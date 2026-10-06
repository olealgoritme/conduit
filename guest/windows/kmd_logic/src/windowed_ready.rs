//! The head of the WindowedBlt READY queue (`VirtioGpu::windowed_blt.ready`).
//!
//! `ready` is a FIFO of the tokens of requests the scheduler admitted; the HPD worker
//! dispatches its FRONT, one request per wake, and only the front: dispatch order is
//! admission order. A token is queued by `admit_windowed_blt_prefix` and leaves the
//! queue when the worker dispatches it or when the request is cancelled.
//!
//! The defect this module closes: a request that is admitted but not yet dispatched can
//! also be retired through `terminal_windowed_blt` (the teardown of the snapshot resource
//! it names, `cancel_windowed_blt_for_resource`, which gives an admitted request a
//! TERMINAL so its WDDM fence cannot wait for ever). That path removed the request from
//! `pending` but left its token in `ready`. The worker then found a front token with no
//! request behind it, answered "nothing to dispatch" without popping it, and every later
//! request of every process queued behind the dead token for the rest of the boot: the
//! adapter-global WDDM FIFO head blocked on a blt that is never dispatched, and only the
//! `WddmHeadMs` rebase (which cancels the copy) ever moved it. A process killed with
//! windowed presents still queued (a slow, large-frame app: a backlog is exactly what
//! the one-dispatch-per-wake worker builds) is the way to get there.
//!
//! The rule, a function of the front token and what `pending` knows about it:
//!
//! * no front: nothing to do;
//! * a front token whose request is gone, or is already dispatched: it can never be
//!   dispatched again, so it is STALE and is dropped, and the next front is looked at;
//! * otherwise the front is the one candidate (the caller still applies its own gates:
//!   admission, producer boundary, buffer ownership; none of them pops).

/// What the worker found at the front of the ready queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// The queue is empty.
    Empty,
    /// The front token names no live undispatched request: pop it and look again.
    Stale,
    /// The front token names a pending, undispatched request.
    Candidate,
}

/// Classify the front of the ready queue. `entry` is `None` when `pending` has no request
/// with the front token, else `Some(dispatched)`.
pub fn classify(front: Option<u64>, entry: Option<bool>) -> Head {
    match (front, entry) {
        (None, _) => Head::Empty,
        (Some(_), None) | (Some(_), Some(true)) => Head::Stale,
        (Some(_), Some(false)) => Head::Candidate,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::VecDeque;
    use std::vec::Vec;

    /// A model of `ready` + `pending` with the worker's dispatch step written the way the
    /// fixed `take_ready_windowed_blt` does it.
    struct Model {
        ready: VecDeque<u64>,
        /// `(token, dispatched)`.
        pending: Vec<(u64, bool)>,
        stale_popped: u32,
    }

    impl Model {
        fn new() -> Self {
            Self {
                ready: VecDeque::new(),
                pending: Vec::new(),
                stale_popped: 0,
            }
        }

        fn admit(&mut self, token: u64) {
            self.pending.push((token, false));
            self.ready.push_back(token);
        }

        /// The defective retirement: the request leaves `pending`, its token stays.
        fn terminal_leaving_token(&mut self, token: u64) {
            self.pending.retain(|(t, _)| *t != token);
        }

        /// The fixed retirement.
        fn terminal_fixed(&mut self, token: u64) {
            self.pending.retain(|(t, _)| *t != token);
            self.ready.retain(|t| *t != token);
        }

        /// One worker dispatch, with or without the front-healing loop.
        fn take(&mut self, heal: bool) -> Option<u64> {
            loop {
                let front = self.ready.front().copied();
                let entry = front.and_then(|t| {
                    self.pending
                        .iter()
                        .find(|(token, _)| *token == t)
                        .map(|(_, dispatched)| *dispatched)
                });
                match classify(front, entry) {
                    Head::Empty => return None,
                    Head::Stale if !heal => return None,
                    Head::Stale => {
                        self.ready.pop_front();
                        self.stale_popped += 1;
                    }
                    Head::Candidate => {
                        let token = front.unwrap();
                        for p in self.pending.iter_mut() {
                            if p.0 == token {
                                p.1 = true;
                            }
                        }
                        self.ready.pop_front();
                        return Some(token);
                    }
                }
            }
        }
    }

    #[test]
    fn classification_table() {
        assert_eq!(classify(None, None), Head::Empty);
        assert_eq!(classify(None, Some(false)), Head::Empty);
        assert_eq!(classify(Some(7), None), Head::Stale);
        assert_eq!(classify(Some(7), Some(true)), Head::Stale);
        assert_eq!(classify(Some(7), Some(false)), Head::Candidate);
    }

    #[test]
    fn a_dead_front_token_wedges_the_queue_without_the_heal() {
        let mut m = Model::new();
        m.admit(1);
        m.admit(2);
        // the owner dies: request 1's snapshot resource is torn down
        m.terminal_leaving_token(1);
        // the old behaviour: request 2, another process's, is never dispatched
        for _ in 0..8 {
            assert_eq!(m.take(false), None);
        }
        assert_eq!(m.ready.len(), 2);
    }

    #[test]
    fn the_heal_pops_dead_tokens_and_serves_the_next_request() {
        let mut m = Model::new();
        m.admit(1);
        m.admit(2);
        m.admit(3);
        m.terminal_leaving_token(1);
        m.terminal_leaving_token(2);
        assert_eq!(m.take(true), Some(3));
        assert_eq!(m.stale_popped, 2);
        assert_eq!(m.take(true), None);
        assert!(m.ready.is_empty());
    }

    #[test]
    fn the_fixed_retirement_leaves_no_dead_token_behind() {
        let mut m = Model::new();
        m.admit(1);
        m.admit(2);
        m.terminal_fixed(1);
        assert_eq!(m.ready.len(), 1);
        assert_eq!(m.take(false), Some(2));
        assert_eq!(m.stale_popped, 0);
    }

    #[test]
    fn a_dispatched_request_whose_token_somehow_stayed_is_stale_not_dispatched_twice() {
        let mut m = Model::new();
        m.admit(1);
        m.ready.push_back(1);
        assert_eq!(m.take(true), Some(1));
        assert_eq!(m.take(true), None);
        assert_eq!(m.stale_popped, 1);
    }

    #[test]
    fn a_dead_token_in_the_middle_is_healed_when_it_reaches_the_front() {
        let mut m = Model::new();
        m.admit(1);
        m.admit(2);
        m.admit(3);
        m.terminal_leaving_token(2);
        assert_eq!(m.take(true), Some(1));
        assert_eq!(m.take(true), Some(3));
        assert_eq!(m.stale_popped, 1);
    }
}
