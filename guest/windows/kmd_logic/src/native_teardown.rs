//! Bookkeeping for the NVIDIA personality's per-process teardown.
//!
//! dxgkrnl destroys a process's HW queues, contexts and allocations, then the
//! process, whether it exits or crashes, and it never calls a Destroy DDI a second
//! time: a teardown step the KMD fails is not retried by the OS. The KMD's native
//! objects all live in its one shared RM client, so what a failed step leaves
//! behind stays in RM until the transport ends unless the KMD retries it itself.
//!
//! * [`Slot`]: a process's native association (its RM VA space and the shared
//!   client). Initialization is exclusive; everyone else reads a copy, so two
//!   threads of one process tearing down queues and contexts at the same time
//!   never refuse each other.
//! * [`Ledger`]: RM objects a failed destroy could not free, by process and
//!   transport generation, freed again at `DestroyProcess` (children were filed
//!   before their parents and come back in that order).
//! * [`retire_owner`]: state a dead process could not release is filed under
//!   [`DEAD_PROCESS`], because process keys are `ProcessContext` addresses and a
//!   new process can get the same one.

/// The key of what a dead process left behind. Process keys are heap addresses
/// (never 1); 0 is the device-wide owner.
pub const DEAD_PROCESS: usize = 1;

/// `owner` after `process` exited: its own entries move to [`DEAD_PROCESS`].
pub fn retire_owner(owner: usize, process: usize) -> usize {
    if owner == process && process != 0 {
        DEAD_PROCESS
    } else {
        owner
    }
}

/// A key that may own a fresh VA space (a live process or the device-wide 0).
pub fn is_live_key(key: usize) -> bool {
    key != DEAD_PROCESS
}

/// A process's native association.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Slot<T> {
    #[default]
    Empty,
    /// One thread is bringing it up (host calls in flight).
    Initializing,
    Ready(T),
}

/// [`Slot::clear`] found an initialization in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Initializing;

/// What [`Slot::begin`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin<T> {
    /// Already up and current: use it.
    Ready(T),
    /// The caller initializes now and must call [`Slot::finish`].
    Initialize,
    /// Another thread is initializing: wait and ask again.
    Wait,
}

/// What [`Slot::peek`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peek<T> {
    Ready(T),
    /// Another thread is initializing: wait and look again.
    Wait,
    Empty,
}

impl<T: Copy> Slot<T> {
    /// Start using the slot; `current` says whether a ready value still belongs to
    /// the live transport (a stale one is dropped and initialized again).
    pub fn begin(&mut self, current: impl Fn(&T) -> bool) -> Begin<T> {
        match *self {
            Slot::Ready(value) if current(&value) => Begin::Ready(value),
            Slot::Initializing => Begin::Wait,
            Slot::Empty | Slot::Ready(_) => {
                *self = Slot::Initializing;
                Begin::Initialize
            }
        }
    }

    /// End the initialization [`Slot::begin`] handed out: `Some` = ready.
    pub fn finish(&mut self, value: Option<T>) {
        *self = match value {
            Some(value) => Slot::Ready(value),
            None => Slot::Empty,
        };
    }

    /// A copy for a create or destroy that needs the client but not exclusivity.
    pub fn peek(&self) -> Peek<T> {
        match *self {
            Slot::Ready(value) => Peek::Ready(value),
            Slot::Initializing => Peek::Wait,
            Slot::Empty => Peek::Empty,
        }
    }

    /// `DestroyProcess`: empty the slot and hand back what was ready. `Err` while
    /// an initialization is in flight (the slot is left as it is).
    pub fn clear(&mut self) -> Result<Option<T>, Initializing> {
        match *self {
            Slot::Initializing => Err(Initializing),
            Slot::Ready(value) => {
                *self = Slot::Empty;
                Ok(Some(value))
            }
            Slot::Empty => Ok(None),
        }
    }
}

/// One RM object a failed teardown left allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leftover {
    pub process: usize,
    pub epoch: u64,
    pub parent: u32,
    pub object: u32,
}

/// RM objects to free again at `DestroyProcess`, in the order they were filed.
pub struct Ledger<const N: usize> {
    entries: [Option<Leftover>; N],
    /// Filing sequence of each entry (older first when taken).
    order: [u64; N],
    next: u64,
    /// Objects that found no room (left to the transport's sweep).
    pub dropped: u32,
}

impl<const N: usize> Ledger<N> {
    pub const fn new() -> Self {
        Self {
            entries: [None; N],
            order: [0; N],
            next: 0,
            dropped: 0,
        }
    }

    /// File `leftover`; `false` (counted in `dropped`) when the ledger is full or
    /// the object is 0.
    pub fn push(&mut self, leftover: Leftover) -> bool {
        if leftover.object == 0 {
            return false;
        }
        match self.entries.iter().position(Option::is_none) {
            Some(i) => {
                self.entries[i] = Some(leftover);
                self.order[i] = self.next;
                self.next += 1;
                true
            }
            None => {
                self.dropped = self.dropped.saturating_add(1);
                false
            }
        }
    }

    /// The oldest entry of `process`, removed.
    pub fn take(&mut self, process: usize) -> Option<Leftover> {
        let i = (0..N)
            .filter(|&i| self.entries[i].is_some_and(|l| l.process == process))
            .min_by_key(|&i| self.order[i])?;
        self.entries[i].take()
    }

    /// Forget every entry of another transport generation than `epoch` (the
    /// transport's sweep freed them with the client).
    pub fn keep_epoch(&mut self, epoch: u64) {
        for entry in self.entries.iter_mut() {
            if entry.is_some_and(|l| l.epoch != epoch) {
                *entry = None;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<const N: usize> Default for Ledger<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_process_entries_move_to_the_dead_key_only() {
        assert_eq!(retire_owner(0x1000, 0x1000), DEAD_PROCESS);
        assert_eq!(retire_owner(0x2000, 0x1000), 0x2000);
        // The device-wide owner is never retired.
        assert_eq!(retire_owner(0, 0), 0);
        assert!(!is_live_key(DEAD_PROCESS));
        assert!(is_live_key(0) && is_live_key(0x1000));
    }

    #[test]
    fn one_thread_initializes_and_the_others_wait_for_it() {
        let mut slot: Slot<u64> = Slot::Empty;
        assert_eq!(slot.begin(|_| true), Begin::Initialize);
        assert_eq!(slot.begin(|_| true), Begin::Wait);
        assert_eq!(slot.peek(), Peek::Wait);
        assert_eq!(slot.clear(), Err(Initializing));
        slot.finish(Some(7));
        assert_eq!(slot.begin(|_| true), Begin::Ready(7));
        assert_eq!(slot.peek(), Peek::Ready(7));
    }

    #[test]
    fn concurrent_destroys_read_the_same_copy() {
        // Two queues of one process torn down at once: neither takes the slot.
        let slot = Slot::Ready(3u64);
        assert_eq!(slot.peek(), Peek::Ready(3));
        assert_eq!(slot.peek(), Peek::Ready(3));
        assert_eq!(slot, Slot::Ready(3));
    }

    #[test]
    fn a_stale_value_is_initialized_again_and_a_failure_empties() {
        let mut slot = Slot::Ready(1u64);
        assert_eq!(slot.begin(|v| *v == 2), Begin::Initialize);
        slot.finish(None);
        assert_eq!(slot.peek(), Peek::Empty);
    }

    #[test]
    fn destroy_process_hands_back_the_ready_value_once() {
        let mut slot = Slot::Ready(5u64);
        assert_eq!(slot.clear(), Ok(Some(5)));
        assert_eq!(slot.clear(), Ok(None));
    }

    fn l(process: usize, epoch: u64, parent: u32, object: u32) -> Leftover {
        Leftover {
            process,
            epoch,
            parent,
            object,
        }
    }

    #[test]
    fn leftovers_come_back_per_process_in_filing_order() {
        let mut ledger: Ledger<4> = Ledger::new();
        assert!(ledger.push(l(1 << 12, 9, 0x10, 0x15))); // channel under its TSG
        assert!(ledger.push(l(2 << 12, 9, 0x20, 0x21)));
        assert!(ledger.push(l(1 << 12, 9, 0x2, 0x13))); // its control memory
        assert!(ledger.push(l(1 << 12, 9, 0x1, 0x10))); // the TSG last
        assert!(!ledger.push(l(1 << 12, 9, 0x1, 0x11)));
        assert_eq!(ledger.dropped, 1);
        assert_eq!(ledger.take(1 << 12).map(|l| l.object), Some(0x15));
        // A freed slot is reused, but the order stays the filing order.
        assert!(ledger.push(l(1 << 12, 9, 0x1, 0x30)));
        assert_eq!(ledger.take(1 << 12).map(|l| l.object), Some(0x13));
        assert_eq!(ledger.take(1 << 12).map(|l| l.object), Some(0x10));
        assert_eq!(ledger.take(1 << 12).map(|l| l.object), Some(0x30));
        assert_eq!(ledger.take(1 << 12), None);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn another_generation_s_leftovers_are_forgotten_and_zero_is_not_filed() {
        let mut ledger: Ledger<4> = Ledger::new();
        assert!(!ledger.push(l(0x1000, 1, 1, 0)));
        assert_eq!(ledger.dropped, 0);
        ledger.push(l(0x1000, 1, 1, 2));
        ledger.push(l(0x1000, 2, 1, 3));
        ledger.keep_epoch(2);
        assert_eq!(ledger.take(0x1000).map(|l| l.object), Some(3));
        assert!(ledger.is_empty());
    }
}
