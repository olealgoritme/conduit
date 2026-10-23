//! How far a producer may run ahead of its front-end worker across Presents.
//!
//! The D3D11 threaded front end's queued Present (`UmdPresentAhead`,
//! `guest/windows/docs/d3d11-threaded-frontend.md` 5.8) queues the worker half
//! of a Present (flush, the RM fence signal) as a record and returns without a
//! drain. The producer then waits only when more than `limit` of those Present
//! records are still queued: [`PresentWindow::push`] says which ring position
//! to wait for. Positions are the queue's record end positions (`CmdQueue::
//! push`), which only grow, and `done` is the worker's read cursor.

/// The largest window. A knob above it is clamped.
pub const MAX_FRAMES: usize = 16;

/// The ring positions of the Present records the worker has not run yet, oldest first.
pub struct PresentWindow {
    pos: [u64; MAX_FRAMES + 1],
    head: usize,
    len: usize,
    limit: usize,
}

impl PresentWindow {
    /// A window of `limit` Presents (clamped to 1..=[`MAX_FRAMES`]).
    pub fn new(limit: u32) -> Self {
        Self {
            pos: [0; MAX_FRAMES + 1],
            head: 0,
            len: 0,
            limit: (limit as usize).clamp(1, MAX_FRAMES),
        }
    }

    /// The window in force.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Presents still queued as of the last [`push`](Self::push).
    pub fn queued(&self) -> usize {
        self.len
    }

    /// Note a Present whose record ends at `pos`, with the worker at `done`. Returns the
    /// position the producer must wait for so that at most `limit` Presents stay queued, or
    /// `None` when it need not wait. A `pos` of 0 (the record ran inline or was dropped) or one
    /// at or below `done` is not queued.
    pub fn push(&mut self, pos: u64, done: u64) -> Option<u64> {
        // Drop the Presents the worker has passed.
        while self.len != 0 && self.pos[self.head] <= done {
            self.pop();
        }
        if pos == 0 || pos <= done {
            return None;
        }
        debug_assert!(self.len == 0 || self.pos[(self.head + self.len - 1) % self.pos.len()] < pos);
        let tail = (self.head + self.len) % self.pos.len();
        self.pos[tail] = pos;
        self.len += 1;
        // Waiting for the newest of the surplus covers the older ones (positions grow).
        let mut wait = None;
        while self.len > self.limit {
            wait = Some(self.pop());
        }
        wait
    }

    /// Forget every queued Present (after a full drain).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn pop(&mut self) -> u64 {
        let p = self.pos[self.head];
        self.head = (self.head + 1) % self.pos.len();
        self.len -= 1;
        p
    }
}

#[cfg(test)]
mod tests {
    use super::{PresentWindow, MAX_FRAMES};

    #[test]
    fn one_frame_window_waits_for_the_previous_present() {
        let mut w = PresentWindow::new(1);
        assert_eq!(w.push(100, 0), None);
        // The worker is still in frame 0: frame 1's Present waits for frame 0's.
        assert_eq!(w.push(200, 50), Some(100));
        assert_eq!(w.queued(), 1);
        // The worker caught up: no wait.
        assert_eq!(w.push(300, 250), None);
        assert_eq!(w.queued(), 1);
    }

    #[test]
    fn three_frame_window_lets_three_presents_queue() {
        let mut w = PresentWindow::new(3);
        assert_eq!(w.push(10, 0), None);
        assert_eq!(w.push(20, 0), None);
        assert_eq!(w.push(30, 0), None);
        assert_eq!(w.push(40, 0), Some(10));
        assert_eq!(w.push(50, 0), Some(20));
        // The worker passed 30 and 40: two queued, no wait.
        assert_eq!(w.push(60, 45), None);
        assert_eq!(w.queued(), 2);
    }

    #[test]
    fn records_already_run_or_inline_are_not_queued() {
        let mut w = PresentWindow::new(1);
        assert_eq!(w.push(0, 0), None);
        assert_eq!(w.push(100, 100), None);
        assert_eq!(w.queued(), 0);
        assert_eq!(w.push(200, 100), None);
        assert_eq!(w.queued(), 1);
    }

    #[test]
    fn clear_forgets_and_limit_is_clamped() {
        let mut w = PresentWindow::new(0);
        assert_eq!(w.limit(), 1);
        assert_eq!(w.push(1, 0), None);
        w.clear();
        assert_eq!(w.push(2, 0), None);
        assert_eq!(PresentWindow::new(1000).limit(), MAX_FRAMES);
    }

    #[test]
    fn full_window_wraps_the_ring() {
        let mut w = PresentWindow::new(MAX_FRAMES as u32);
        for i in 1..=(4 * MAX_FRAMES as u64) {
            let wait = w.push(i * 10, 0);
            if i as usize > MAX_FRAMES {
                assert_eq!(wait, Some((i - MAX_FRAMES as u64) * 10));
            } else {
                assert_eq!(wait, None);
            }
            assert!(w.queued() <= MAX_FRAMES);
        }
    }
}
