//! Keys and ids of the user views `HELIOS_NVRM_OP_MMAP` makes, in the mapping table
//! the KMD shares with blob views.
//!
//! A view of host BAR memory lives in `AdapterContext::mappings`, which outlives
//! the virtio transport (a view can only be unmapped inside the process that made
//! it, so a `StopDevice` cannot touch it). The ids that name the views are minted
//! by the transport, and a transport that is replaced starts a new generation:
//!
//! * ids must therefore be MONOTONIC across generations. If a new transport
//!   restarted at 1, a surviving device handle's old view would sit on the very key
//!   the new generation's first `MMAP` wants (`insert_unique` would refuse it), and
//!   a stale `MUNMAP` could name a live mapping of the new generation;
//! * a view whose id is below the first id of the current generation belongs to a
//!   transport that no longer exists ("stale"): the host mapping it pointed at is
//!   gone, and its owner unmaps it the next time it calls into the KMD.
//!
//! The rules are pure functions of their arguments so the host tests can pin them.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// NVRM views are keyed `id | KEY_BIT`: the high bit keeps them apart from blob
/// views, which are keyed by a small resource id.
pub const KEY_BIT: u32 = 0x8000_0000;

/// Minted ids stay below this. Above it live the fixed keys other view kinds use
/// in the same table (`READ_LEDGER_MAPPING_ID` = `u32::MAX`, `PRODUCER_MAPPING_ID`
/// = `u32::MAX - 1`), whose low 31 bits are `0x7FFF_FFFF` and `0x7FFF_FFFE`.
pub const ID_LIMIT: u32 = 0x7FFF_FFF0;

/// The key of view `id` in the shared table.
pub const fn key(id: u32) -> u32 {
    id | KEY_BIT
}

/// The id of an NVRM view key, or `None` for any other kind of key (a blob's
/// resource id, or one of the fixed pseudo ids).
pub const fn id_of_key(key: u32) -> Option<u32> {
    let id = key & !KEY_BIT;
    if key & KEY_BIT != 0 && id != 0 && id < ID_LIMIT {
        Some(id)
    } else {
        None
    }
}

/// Whether `key` names an NVRM view minted before `below` (the first id of the
/// current transport generation): a view of a transport that is gone. Never true
/// for a key that is not an NVRM view's.
pub const fn is_stale(key: u32, below: u32) -> bool {
    match id_of_key(key) {
        Some(id) => id < below,
        None => false,
    }
}

/// The counter value after minting `current`, or `None` when `current` is not a
/// mintable id (0, or the id space is used up). A monotonic counter minted with
/// `fetch_update(|n| successor(n))` hands out `1, 2, 3, ...` once each, never wraps
/// into the key bit and never yields 0.
pub const fn successor(current: u32) -> Option<u32> {
    if current == 0 || current >= ID_LIMIT {
        None
    } else {
        Some(current + 1)
    }
}

/// How many distinct owners [`StaleOwners`] names exactly. More than this and it
/// answers "maybe" for everyone until the next rebuild (see
/// [`StaleOwners::rebuild`]).
pub const STALE_OWNER_SLOTS: usize = 16;

/// The owners that may hold a stale NVRM view: the lock-free answer the escape
/// hot path asks before it takes the mapping table's lock.
///
/// A process that never calls back keeps its stale views until its
/// `DestroyDevice`, so "is anything stale anywhere" (one global count) would send
/// EVERY other process through the locked scan of the whole table on every call.
/// This answers per owner instead: an owner with nothing stale reads two atomics
/// and returns.
///
/// The invariant is one-sided: an owner that holds a stale view is ALWAYS
/// reported (`may_have_stale`), never missed; an owner that holds none may be
/// reported for a while (a full set, or a view taken another way), which costs it
/// one locked scan that then repairs the set ([`Self::rebuild`]). Writers
/// (`add`, `remove`, `rebuild`) must be serialised by the caller, in practice by
/// the mapping table's lock; readers take no lock.
pub struct StaleOwners {
    /// Owner tokens; 0 = empty (0 is never a real owner).
    slots: [AtomicUsize; STALE_OWNER_SLOTS],
    /// More owners than slots: everyone is "maybe".
    overflow: AtomicBool,
}

impl StaleOwners {
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicUsize::new(0) }; STALE_OWNER_SLOTS],
            overflow: AtomicBool::new(false),
        }
    }

    /// Whether `owner` may hold a stale view. Lock-free.
    pub fn may_have_stale(&self, owner: usize) -> bool {
        if self.overflow.load(Ordering::Acquire) {
            return true;
        }
        // 0 is the empty marker, so it cannot be looked up; it is never a real
        // owner, and answering "no" would hide a view if it ever were.
        if owner == 0 {
            return self.any();
        }
        self.slots
            .iter()
            .any(|s| s.load(Ordering::Acquire) == owner)
    }

    /// Whether any owner may hold one. Lock-free.
    pub fn any(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
            || self.slots.iter().any(|s| s.load(Ordering::Acquire) != 0)
    }

    /// `owner` now holds a stale view. A full set (or owner 0) becomes
    /// "everyone maybe" until a rebuild.
    pub fn add(&self, owner: usize) {
        if owner == 0 {
            self.overflow.store(true, Ordering::Release);
            return;
        }
        let mut free = None;
        for s in &self.slots {
            let v = s.load(Ordering::Relaxed);
            if v == owner {
                return;
            }
            if v == 0 && free.is_none() {
                free = Some(s);
            }
        }
        match free {
            Some(s) => s.store(owner, Ordering::Release),
            None => self.overflow.store(true, Ordering::Release),
        }
    }

    /// `owner` holds no stale view any more (it was drained, or all of its views
    /// were). Does not clear an overflow: owners that did not fit are unknown.
    pub fn remove(&self, owner: usize) {
        if owner == 0 {
            return;
        }
        for s in &self.slots {
            if s.load(Ordering::Relaxed) == owner {
                s.store(0, Ordering::Release);
            }
        }
    }

    /// Replace the set with exactly `owners` (duplicates fine), as a full scan of
    /// the table found them. Never reports a false "no" while it runs: the set
    /// reads as overflowed until the new content is in place. Ends overflowed only
    /// if `owners` has more distinct members than there are slots.
    pub fn rebuild(&self, owners: impl Iterator<Item = usize>) {
        let mut next = [0usize; STALE_OWNER_SLOTS];
        let mut n = 0usize;
        let mut overflow = false;
        for o in owners {
            if o == 0 || next[..n].contains(&o) {
                overflow |= o == 0;
                continue;
            }
            if n == STALE_OWNER_SLOTS {
                overflow = true;
                continue;
            }
            next[n] = o;
            n += 1;
        }
        self.overflow.store(true, Ordering::Release);
        for (s, &o) in self.slots.iter().zip(next.iter()) {
            s.store(o, Ordering::Release);
        }
        self.overflow.store(overflow, Ordering::Release);
    }
}

impl Default for StaleOwners {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_stay_apart_from_blob_ids() {
        for id in [1u32, 2, 77, ID_LIMIT - 1] {
            assert_eq!(id_of_key(key(id)), Some(id));
        }
        // A blob's resource id has no key bit: never an NVRM view.
        for blob in [1u32, 5, 0x7FFF_FFF0, 0x7FFF_FFFF] {
            assert_eq!(id_of_key(blob), None);
            assert!(!is_stale(blob, u32::MAX));
        }
    }

    #[test]
    fn the_fixed_pseudo_ids_are_not_nvrm_views() {
        // `mapping.rs`: READ_LEDGER_MAPPING_ID and PRODUCER_MAPPING_ID.
        for fixed in [u32::MAX, u32::MAX - 1] {
            assert_eq!(id_of_key(fixed), None);
            assert!(!is_stale(fixed, u32::MAX));
        }
        // Id 0 under the key bit is not a view either (0 is "none").
        assert_eq!(id_of_key(KEY_BIT), None);
        // The first key above the mintable range is not one.
        assert_eq!(id_of_key(key(ID_LIMIT)), None);
    }

    #[test]
    fn stale_means_minted_before_the_current_generation() {
        let base = 10;
        assert!(is_stale(key(1), base));
        assert!(is_stale(key(9), base));
        assert!(!is_stale(key(10), base));
        assert!(!is_stale(key(11), base));
        // A zero base (no generation boundary recorded) marks nothing stale.
        assert!(!is_stale(key(1), 0));
    }

    #[test]
    fn minting_is_monotonic_and_bounded() {
        let mut next = 1u32;
        let mut last = 0u32;
        let mut count = 0u32;
        // Walk the whole space in big steps by jumping near the end.
        for _ in 0..1000 {
            let minted = next;
            next = successor(next).expect("mintable");
            assert!(minted > last);
            assert_eq!(id_of_key(key(minted)), Some(minted));
            last = minted;
            count += 1;
        }
        assert_eq!(count, 1000);
        // The last mintable id is ID_LIMIT - 1; the counter then refuses.
        assert_eq!(successor(ID_LIMIT - 1), Some(ID_LIMIT));
        assert_eq!(successor(ID_LIMIT), None);
        assert_eq!(successor(u32::MAX), None);
        assert_eq!(successor(0), None);
    }

    #[test]
    fn a_new_generation_never_reuses_an_old_key() {
        // Generation 1 mints 1..=3 and goes away; the counter carries on, so
        // generation 2's first id is above every stale one.
        let mut next = 1u32;
        let mut gen1 = [0u32; 3];
        for slot in gen1.iter_mut() {
            *slot = next;
            next = successor(next).unwrap();
        }
        let base = next; // recorded when generation 1 is dropped
        let first_of_gen2 = next;
        assert!(gen1.iter().all(|&id| is_stale(key(id), base)));
        assert!(!is_stale(key(first_of_gen2), base));
        assert!(gen1.iter().all(|&id| key(id) != key(first_of_gen2)));
    }

    // ---- StaleOwners ------------------------------------------------------------

    #[test]
    fn an_owner_with_nothing_stale_is_never_reported_while_another_is() {
        let set = StaleOwners::new();
        assert!(!set.any());
        assert!(!set.may_have_stale(0x10));
        // A process that never calls back holds a stale view.
        set.add(0xAAA0);
        assert!(set.any());
        assert!(set.may_have_stale(0xAAA0));
        // Everyone else keeps the lock-free fast path.
        for other in [0x10usize, 0x20, 0xAAA8, 0xBBB0] {
            assert!(!set.may_have_stale(other));
        }
    }

    #[test]
    fn add_is_idempotent_and_remove_clears_only_that_owner() {
        let set = StaleOwners::new();
        set.add(5);
        set.add(5);
        set.add(6);
        set.remove(5);
        assert!(!set.may_have_stale(5));
        assert!(set.may_have_stale(6));
        set.remove(6);
        assert!(!set.any());
        // Removing an owner that is not there is a no-op.
        set.remove(99);
        set.remove(0);
        assert!(!set.any());
    }

    #[test]
    fn a_full_set_degrades_to_maybe_for_everyone_and_rebuild_repairs_it() {
        let set = StaleOwners::new();
        for o in 1..=STALE_OWNER_SLOTS {
            set.add(o * 8);
        }
        assert!(!set.may_have_stale(0x7770));
        // One more than fits: nobody can be ruled out any more.
        set.add(0x9990);
        assert!(set.may_have_stale(0x7770));
        assert!(set.may_have_stale(0x9990));
        // Removing does not undo the doubt (the unknown owner is unknown).
        set.remove(8);
        assert!(set.may_have_stale(0x7770));
        // A full scan that finds only three owners restores exactness.
        set.rebuild([0x100usize, 0x200, 0x100, 0x300, 0x200].into_iter());
        assert!(!set.may_have_stale(0x7770));
        assert!(!set.may_have_stale(8));
        for o in [0x100usize, 0x200, 0x300] {
            assert!(set.may_have_stale(o));
        }
        // ...and a scan that finds too many keeps the doubt.
        set.rebuild((1..=STALE_OWNER_SLOTS + 1).map(|o| o * 8));
        assert!(set.may_have_stale(0x7770));
        // An empty scan clears everything.
        set.rebuild(core::iter::empty());
        assert!(!set.any());
    }

    #[test]
    fn owner_zero_is_never_hidden() {
        let set = StaleOwners::new();
        assert!(!set.may_have_stale(0));
        set.add(0);
        assert!(set.may_have_stale(0));
        assert!(set.may_have_stale(1));
        set.rebuild([3usize].into_iter());
        assert!(set.may_have_stale(3));
        assert!(!set.may_have_stale(1));
        // A rebuild that meets owner 0 keeps the doubt rather than lose it.
        set.rebuild([0usize, 3].into_iter());
        assert!(set.may_have_stale(1));
    }

    /// A model of `MappingTable`'s use of the set (mark / insert / drain stale /
    /// drain all), run over a scripted mix, checking the one-sided invariant: an
    /// owner that holds a stale view is always reported, and a locked scan that
    /// finds nothing stale clears the set.
    #[test]
    fn the_set_never_misses_an_owner_that_holds_a_stale_view() {
        extern crate std;
        use std::vec::Vec;

        struct Table {
            entries: Vec<(usize, u32)>, // (owner, key)
            below: u32,
            set: StaleOwners,
        }
        impl Table {
            fn rescan(&self) {
                let below = self.below;
                self.set.rebuild(
                    self.entries
                        .iter()
                        .filter(|e| is_stale(e.1, below))
                        .map(|e| e.0),
                );
            }
            fn mark(&mut self, below: u32) {
                self.below = below.max(self.below);
                self.rescan();
            }
            // insert_unique, including a view inserted already stale (the
            // push/insert race with a StopDevice in between).
            fn insert(&mut self, owner: usize, id: u32) {
                self.entries.push((owner, key(id)));
                if is_stale(key(id), self.below) {
                    self.set.add(owner);
                }
            }
            // drain_stale_nvrm_for with a batch limit.
            fn drain_stale(&mut self, owner: usize, batch: usize) -> usize {
                if !self.set.may_have_stale(owner) {
                    return 0;
                }
                let below = self.below;
                let mut n = 0;
                let mut i = 0;
                while i < self.entries.len() {
                    let e = self.entries[i];
                    if is_stale(e.1, below) && e.0 == owner && n < batch {
                        self.entries.swap_remove(i);
                        n += 1;
                    } else {
                        i += 1;
                    }
                }
                self.rescan();
                n
            }
            // drain_for (DestroyDevice): everything of the owner.
            fn drain_all(&mut self, owner: usize) {
                self.entries.retain(|e| e.0 != owner);
                self.set.remove(owner);
            }
            fn check(&self) {
                for &(owner, k) in &self.entries {
                    if is_stale(k, self.below) {
                        assert!(self.set.may_have_stale(owner), "missed owner {owner:#x}");
                    }
                }
            }
        }

        let mut t = Table {
            entries: Vec::new(),
            below: 0,
            set: StaleOwners::new(),
        };
        let mut next_id = 1u32;
        // 40 owners, more than the set holds, so overflow and repair both run.
        let owners: Vec<usize> = (1..=40).map(|o| o * 0x10).collect();
        let mut seed = 0x1234_5678u32;
        let mut rnd = move |m: u32| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) % m
        };
        for _ in 0..4000 {
            let owner = owners[rnd(owners.len() as u32) as usize];
            match rnd(10) {
                0..=4 => {
                    t.insert(owner, next_id);
                    next_id += 1;
                }
                5 => t.mark(next_id), // a transport generation ends
                6..=7 => {
                    let had = t
                        .entries
                        .iter()
                        .any(|e| e.0 == owner && is_stale(e.1, t.below));
                    let n = t.drain_stale(owner, 3);
                    if !had {
                        assert_eq!(n, 0);
                    }
                }
                8 => {
                    // Drain this owner completely, then nothing stale is left in
                    // the whole table if nobody else had any: the set is empty.
                    while t.drain_stale(owner, 3) != 0 {}
                    if !t.entries.iter().any(|e| is_stale(e.1, t.below)) {
                        assert!(!t.set.any());
                    }
                }
                _ => t.drain_all(owner),
            }
            t.check();
        }
    }
}
