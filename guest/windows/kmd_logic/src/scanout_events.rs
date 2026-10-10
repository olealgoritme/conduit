//! The D4a retirement-event table and its broadcast latch (`kmd_render`
//! `adapter/read_ledger.rs`).
//!
//! Every UMD device registers one persistent auto-reset event; the KMD signals
//! all of them when host reads of the read ledger retire. The KMD holds the
//! table under a leaf spinlock; the rules live here so they run on the host:
//!
//! * REGISTER: an (owner, event) pair already parked is an idempotent success;
//!   an owner holding [`EVENTS_PER_OWNER`] entries is refused, so a device that
//!   retries its registration (the UMD does, on present and flush) can never
//!   crowd other devices out of the table; a full table is refused.
//! * Retirement does not signal. It marks the [`BroadcastLatch`], and whoever
//!   leaves the transport lock (`AdapterContext::with_virtio`) takes the latch
//!   and signals every entry once: one broadcast per used-ring drain pass,
//!   never one per retired read, and never under `virtio_lock`.
//! * A transport reset (Stop/StartDevice) signals every entry and KEEPS it.
//!   A registration belongs to a device, not to a transport generation: the
//!   device keeps its ledger mapping across the reset, and only its
//!   DestroyDevice (owner reclaim) or the adapter's removal drops the entry.

use core::sync::atomic::{AtomicBool, Ordering};

/// Retirement-event registrations per adapter (`AqLive` is the occupancy).
pub const EVENT_TABLE_LEN: usize = 64;

/// Registrations one owner (a D3D device) may hold. The UMD registers one
/// event per device; the second entry leaves room for a device that replaces
/// its event without waiting for the old one's UNREGISTER.
pub const EVENTS_PER_OWNER: usize = 2;

/// One parked registration. Both fields are opaque `usize`: the owner is the
/// registering device handle, the event the referenced `KEVENT` pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventEntry {
    pub owner: usize,
    pub event: usize,
}

/// Outcome of [`EventTable::register`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    /// Parked; the table owns the caller's object reference.
    Registered,
    /// Already parked; the caller drops the reference it took for this call.
    AlreadyRegistered,
    /// The owner already holds [`EVENTS_PER_OWNER`] other entries.
    OwnerFull,
    /// No free entry.
    TableFull,
}

impl Register {
    /// Whether the table took the caller's reference.
    pub const fn took_reference(self) -> bool {
        matches!(self, Register::Registered)
    }
}

/// The table itself: fixed storage, no allocation, values only.
#[derive(Clone, Copy, Debug)]
pub struct EventTable<const N: usize> {
    entries: [Option<EventEntry>; N],
}

impl<const N: usize> Default for EventTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> EventTable<N> {
    pub const fn new() -> Self {
        Self { entries: [None; N] }
    }

    /// Park `(owner, event)` under the rules in the module doc.
    pub fn register(&mut self, owner: usize, event: usize) -> Register {
        let mut free = None;
        let mut held = 0usize;
        for (i, slot) in self.entries.iter().enumerate() {
            match slot {
                Some(e) if e.owner == owner && e.event == event => {
                    return Register::AlreadyRegistered;
                }
                Some(e) if e.owner == owner => held += 1,
                None if free.is_none() => free = Some(i),
                _ => {}
            }
        }
        if held >= EVENTS_PER_OWNER {
            return Register::OwnerFull;
        }
        match free {
            Some(i) => {
                self.entries[i] = Some(EventEntry { owner, event });
                Register::Registered
            }
            None => Register::TableFull,
        }
    }

    /// Remove `(owner, event)`. Returns whether it was parked (the caller then
    /// drops the reference the table held).
    pub fn unregister(&mut self, owner: usize, event: usize) -> bool {
        for slot in self.entries.iter_mut() {
            if matches!(slot, Some(e) if e.owner == owner && e.event == event) {
                *slot = None;
                return true;
            }
        }
        false
    }

    /// Move up to `out.len()` entries (all, or only `owner`'s) into `out`,
    /// removing them from the table. Returns how many were moved; a caller
    /// draining everything repeats until it moves fewer than `out.len()`.
    pub fn take(&mut self, owner: Option<usize>, out: &mut [Option<EventEntry>]) -> usize {
        let mut n = 0;
        for slot in self.entries.iter_mut() {
            if n >= out.len() {
                break;
            }
            let matches = match (slot.as_ref(), owner) {
                (Some(_), None) => true,
                (Some(e), Some(o)) => e.owner == o,
                (None, _) => false,
            };
            if matches {
                out[n] = slot.take();
                n += 1;
            }
        }
        n
    }

    /// Every parked entry, for a broadcast or a reset's signal pass. Neither
    /// removes anything.
    pub fn live(&self) -> impl Iterator<Item = EventEntry> + '_ {
        self.entries.iter().flatten().copied()
    }

    pub fn len(&self) -> usize {
        self.live().count()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    pub fn held_by(&self, owner: usize) -> usize {
        self.live().filter(|e| e.owner == owner).count()
    }
}

/// "A read retired since the last broadcast." Set by every retirement (any
/// IRQL, any lock held), taken by the one place that signals.
#[derive(Debug, Default)]
pub struct BroadcastLatch {
    pending: AtomicBool,
}

impl BroadcastLatch {
    pub const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
        }
    }

    /// A retirement happened: the next [`Self::take`] answers true.
    pub fn note_retired(&self) {
        // Release: a taker that sees the flag sees the ledger stores before it
        // (the consumers re-read the ledger with Acquire loads anyway).
        self.pending.store(true, Ordering::Release);
    }

    /// Whether a broadcast is owed; clears the latch. One relaxed load when
    /// nothing retired, so the transport-lock exit path stays a compare.
    pub fn take(&self) -> bool {
        if !self.pending.load(Ordering::Relaxed) {
            return false;
        }
        self.pending.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Table = EventTable<EVENT_TABLE_LEN>;

    #[test]
    fn register_is_idempotent_per_pair() {
        let mut t = Table::new();
        assert_eq!(t.register(1, 10), Register::Registered);
        assert_eq!(t.register(1, 10), Register::AlreadyRegistered);
        assert_eq!(t.len(), 1);
        assert!(!Register::AlreadyRegistered.took_reference());
    }

    #[test]
    fn owner_is_capped() {
        let mut t = Table::new();
        for e in 0..EVENTS_PER_OWNER {
            assert_eq!(t.register(7, 100 + e), Register::Registered);
        }
        assert_eq!(t.register(7, 999), Register::OwnerFull);
        // The cap is per owner: another device still registers.
        assert_eq!(t.register(8, 999), Register::Registered);
        // A re-register of a parked pair is not a new entry.
        assert_eq!(t.register(7, 100), Register::AlreadyRegistered);
        assert_eq!(t.held_by(7), EVENTS_PER_OWNER);
    }

    #[test]
    fn retrying_owner_cannot_fill_the_table() {
        // A device whose REGISTER keeps being retried with fresh events (one
        // per attempt, the old one closed without UNREGISTER) holds at most
        // EVENTS_PER_OWNER entries.
        let mut t = Table::new();
        for attempt in 0..1000 {
            let _ = t.register(3, 5000 + attempt);
        }
        assert_eq!(t.len(), EVENTS_PER_OWNER);
    }

    #[test]
    fn full_table_refuses_then_accepts_after_unregister() {
        let mut t = EventTable::<4>::new();
        for o in 0..4 {
            assert_eq!(t.register(o, o + 100), Register::Registered);
        }
        assert_eq!(t.register(9, 900), Register::TableFull);
        assert!(t.unregister(2, 102));
        assert!(!t.unregister(2, 102));
        // The refused device's retry now lands.
        assert_eq!(t.register(9, 900), Register::Registered);
    }

    #[test]
    fn owner_reclaim_takes_only_that_owner() {
        let mut t = Table::new();
        t.register(1, 11);
        t.register(2, 21);
        t.register(1, 12);
        let mut out = [None; 16];
        assert_eq!(t.take(Some(1), &mut out), 2);
        assert_eq!(t.len(), 1);
        assert_eq!(t.live().next(), Some(EventEntry { owner: 2, event: 21 }));
    }

    #[test]
    fn drain_all_in_chunks() {
        let mut t = Table::new();
        for o in 0..40 {
            t.register(o, o);
        }
        let mut total = 0;
        loop {
            let mut out = [None; 16];
            let n = t.take(None, &mut out);
            total += n;
            if n < out.len() {
                break;
            }
        }
        assert_eq!(total, 40);
        assert!(t.is_empty());
    }

    #[test]
    fn reset_signal_pass_keeps_registrations() {
        // A transport reset signals every entry (`live`) and removes none: a
        // device that survives the reset keeps its event without having to
        // notice the reset, and its re-register stays idempotent.
        let mut t = Table::new();
        t.register(1, 11);
        t.register(2, 21);
        let signalled: [Option<EventEntry>; 2] = {
            let mut it = t.live();
            [it.next(), it.next()]
        };
        assert!(signalled.iter().all(Option::is_some));
        assert_eq!(t.len(), 2);
        assert_eq!(t.register(1, 11), Register::AlreadyRegistered);
    }

    #[test]
    fn latch_coalesces_retirements_into_one_broadcast() {
        let l = BroadcastLatch::new();
        assert!(!l.take());
        for _ in 0..50 {
            l.note_retired();
        }
        assert!(l.take());
        assert!(!l.take());
        l.note_retired();
        assert!(l.take());
    }
}
