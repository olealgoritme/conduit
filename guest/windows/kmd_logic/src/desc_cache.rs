//! The KMD's RM OS-descriptor objects over guest pages, shared and cached.
//!
//! A system-page leaf update (`kmd_render` `virtio/native_memory.rs`) makes an
//! RM `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over VidMm's pages and maps it at the
//! exact VA; a foreign leaf update does the same for dxgkrnl's fence pages. The
//! registration is the costly host step (the backend stitches the pages into a
//! span, RM pins them). An OS descriptor names guest-physical pages and nothing
//! else, and guest RAM is not ballooned (`memballoon none`): a page number
//! always names the same host page. So an object over exactly the same pages is
//! the same object, and one already made is reused:
//!
//! * **shared**: a descriptor mapped somewhere (another process's space, the
//!   same allocation at another VA) is mapped again; it is freed when its last
//!   mapping goes;
//! * **cached** (`NvPaging` 0x2): a descriptor whose last mapping went is kept,
//!   unmapped, up to a bound, and reused when the same pages are mapped again.
//!
//! An idle descriptor is mapped nowhere, so the GPU cannot reach its pages; it
//! only keeps RM's pin on them. It is still dropped as soon as the pages are no
//! longer known to be the allocation's: the allocation is destroyed, or VidMm
//! places it in video memory (its system pages are then VidMm's to free). The
//! oldest idle ones go first when the bound is reached, and all at a transport
//! reset (the host freed every RM object then).
//!
//! Lookup is by an FNV-1a hash of the page list, then the list itself.

extern crate alloc;

use alloc::vec::Vec;

/// The default bound on idle descriptors (`NvDescCacheMax`).
pub const DEFAULT_MAX_IDLE: usize = 256;

/// FNV-1a over the page numbers.
pub fn key(pfns: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in pfns {
        for b in p.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h ^ (pfns.len() as u64)
}

#[derive(Debug)]
struct Entry {
    desc: u32,
    key: u64,
    pfns: Vec<u64>,
    /// Live mappings of the object.
    refs: u32,
    /// The allocation that last mapped it (0: none, a foreign mapping).
    owner: u32,
    /// When it last went idle (a counter): the oldest idle one goes first.
    idle_since: u64,
}

/// What [`DescTable::release`] decided for an object whose mapping went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// Still mapped elsewhere: nothing to free.
    InUse,
    /// Kept idle for reuse: nothing to free now.
    Cached,
    /// Free it (not cached, or not known here).
    Free,
}

/// Every OS-descriptor object the KMD's paging path made, with its pages.
#[derive(Debug, Default)]
pub struct DescTable {
    entries: Vec<Entry>,
    clock: u64,
    idle: usize,
}

impl DescTable {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            clock: 0,
            idle: 0,
        }
    }

    fn find(&self, desc: u32) -> Option<usize> {
        self.entries.iter().position(|e| e.desc == desc)
    }

    /// An object over exactly `pfns` (mapped or idle), taken for one more
    /// mapping: `(desc, was_idle)`.
    pub fn acquire(&mut self, pfns: &[u64], owner: u32) -> Option<(u32, bool)> {
        if pfns.is_empty() {
            return None;
        }
        let k = key(pfns);
        let e = self
            .entries
            .iter_mut()
            .find(|e| e.key == k && e.pfns.as_slice() == pfns)?;
        let was_idle = e.refs == 0;
        e.refs = e.refs.checked_add(1)?;
        if owner != 0 {
            e.owner = owner;
        }
        if was_idle {
            self.idle -= 1;
        }
        Some((e.desc, was_idle))
    }

    /// Record a new object `desc` over `pfns` with its first mapping. `Err` when
    /// there is no memory to record it: the caller frees it at its unmap as it
    /// always did ([`Self::release`] answers `Free` for an unknown one).
    pub fn insert(&mut self, desc: u32, pfns: &[u64], owner: u32) -> Result<(), ()> {
        if desc == 0 || pfns.is_empty() || self.find(desc).is_some() {
            return Err(());
        }
        let mut copy = Vec::new();
        copy.try_reserve_exact(pfns.len()).map_err(|_| ())?;
        copy.extend_from_slice(pfns);
        self.entries.try_reserve(1).map_err(|_| ())?;
        self.entries.push(Entry {
            desc,
            key: key(pfns),
            pfns: copy,
            refs: 1,
            owner,
            idle_since: 0,
        });
        Ok(())
    }

    /// One mapping of `desc` went. With `cache`, an object no longer mapped
    /// anywhere stays (idle) and the oldest idle ones beyond `max_idle` are
    /// appended to `evicted` for the caller to free.
    pub fn release(&mut self, desc: u32, cache: bool, max_idle: usize, evicted: &mut Vec<u32>) -> Released {
        let Some(i) = self.find(desc) else {
            return Released::Free;
        };
        let e = &mut self.entries[i];
        if e.refs > 1 {
            e.refs -= 1;
            return Released::InUse;
        }
        if !cache || max_idle == 0 {
            self.entries.swap_remove(i);
            return Released::Free;
        }
        if e.refs == 1 {
            e.refs = 0;
            self.clock += 1;
            e.idle_since = self.clock;
            self.idle += 1;
        }
        while self.idle > max_idle {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.refs == 0)
                .min_by_key(|(_, e)| e.idle_since)
                .map(|(i, _)| i)
            else {
                break;
            };
            let gone = self.entries.swap_remove(oldest);
            self.idle -= 1;
            if evicted.try_reserve(1).is_ok() {
                evicted.push(gone.desc);
            }
        }
        Released::Cached
    }

    /// Whether [`Self::release`] of `desc` with these settings would answer `Free`
    /// (nothing changes): a batch frees the object in the same round trip as its
    /// unmap, and calls [`Self::forget`] once that worked.
    pub fn would_free(&self, desc: u32, cache: bool, max_idle: usize) -> bool {
        match self.find(desc) {
            None => true,
            Some(i) if self.entries[i].refs > 1 => false,
            Some(_) => !cache || max_idle == 0,
        }
    }

    /// Forget `desc` whatever its count (its RM object is gone or being freed).
    pub fn forget(&mut self, desc: u32) {
        if let Some(i) = self.find(desc) {
            if self.entries[i].refs == 0 {
                self.idle -= 1;
            }
            self.entries.swap_remove(i);
        }
    }

    /// The pages of `desc`, if recorded.
    pub fn pfns(&self, desc: u32) -> Option<&[u64]> {
        self.find(desc).map(|i| self.entries[i].pfns.as_slice())
    }

    /// Take every idle object of allocation `owner` out (its pages are no
    /// longer known to be its own), for the caller to free.
    pub fn drop_idle_of(&mut self, owner: u32, out: &mut Vec<u32>) {
        if owner == 0 || self.idle == 0 {
            return;
        }
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.refs == 0 && e.owner == owner {
                if out.try_reserve(1).is_err() {
                    return;
                }
                out.push(e.desc);
                self.entries.swap_remove(i);
                self.idle -= 1;
            } else {
                i += 1;
            }
        }
    }

    /// Take every idle object out, for the caller to free.
    pub fn drain_idle(&mut self, out: &mut Vec<u32>) {
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].refs == 0 {
                if out.try_reserve(1).is_err() {
                    return;
                }
                out.push(self.entries[i].desc);
                self.entries.swap_remove(i);
                self.idle -= 1;
            } else {
                i += 1;
            }
        }
    }

    /// Forget everything (the transport closed every RM object).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.idle = 0;
    }

    /// `(objects, idle ones)`.
    pub fn counts(&self) -> (usize, usize) {
        (self.entries.len(), self.idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_same_pages_are_the_same_object() {
        let mut t = DescTable::new();
        assert_eq!(t.acquire(&[1, 2, 3], 9), None);
        t.insert(0x100, &[1, 2, 3], 9).unwrap();
        assert_eq!(t.acquire(&[1, 2, 3], 10), Some((0x100, false)), "shared while mapped");
        assert_eq!(t.acquire(&[1, 2], 9), None, "a prefix is other pages");
        assert_eq!(t.acquire(&[3, 2, 1], 9), None, "order matters");
        assert_eq!(t.pfns(0x100), Some(&[1u64, 2, 3][..]));
        let mut gone = vec![];
        assert_eq!(t.release(0x100, false, 8, &mut gone), Released::InUse);
        assert_eq!(t.release(0x100, false, 8, &mut gone), Released::Free);
        assert_eq!(t.counts(), (0, 0));
        assert_eq!(t.release(0x100, true, 8, &mut gone), Released::Free, "unknown");
        assert!(gone.is_empty());
    }

    #[test]
    fn idle_objects_are_reused_then_the_oldest_go() {
        let mut t = DescTable::new();
        let mut gone = vec![];
        for d in 1..=3u32 {
            t.insert(d, &[u64::from(d) * 10], d).unwrap();
            assert_eq!(t.release(d, true, 2, &mut gone), Released::Cached);
        }
        // The bound is 2: the first to go idle went.
        assert_eq!(gone, vec![1]);
        assert_eq!(t.counts(), (2, 2));
        assert_eq!(t.acquire(&[20], 7), Some((2, true)));
        assert_eq!(t.counts(), (2, 1));
        assert_eq!(t.acquire(&[10], 7), None);
        // Mapped again and released again: idle, newest now.
        assert_eq!(t.release(2, true, 2, &mut gone), Released::Cached);
        t.insert(4, &[40], 4).unwrap();
        assert_eq!(t.release(4, true, 2, &mut gone), Released::Cached);
        assert_eq!(gone, vec![1, 3]);
    }

    #[test]
    fn idle_objects_go_with_their_allocation_and_at_a_reset() {
        let mut t = DescTable::new();
        let mut gone = vec![];
        t.insert(1, &[1], 5).unwrap();
        t.insert(2, &[2], 5).unwrap();
        t.insert(3, &[3], 6).unwrap();
        t.release(1, true, 8, &mut gone);
        t.release(3, true, 8, &mut gone);
        // 2 is still mapped: it stays.
        let mut out = vec![];
        t.drop_idle_of(5, &mut out);
        assert_eq!(out, vec![1]);
        assert_eq!(t.counts(), (2, 1));
        t.drop_idle_of(0, &mut out);
        assert_eq!(out, vec![1]);
        let mut all = vec![];
        t.drain_idle(&mut all);
        assert_eq!(all, vec![3]);
        assert_eq!(t.counts(), (1, 0));
        t.forget(2);
        assert_eq!(t.counts(), (0, 0));
        t.insert(9, &[9], 1).unwrap();
        t.release(9, true, 8, &mut gone);
        t.forget(9);
        assert_eq!(t.counts(), (0, 0));
        t.insert(9, &[9], 1).unwrap();
        t.clear();
        assert_eq!(t.counts(), (0, 0));
    }

    #[test]
    fn insert_refuses_what_it_cannot_record() {
        let mut t = DescTable::new();
        assert!(t.insert(0, &[1], 1).is_err());
        assert!(t.insert(1, &[], 1).is_err());
        t.insert(1, &[1], 1).unwrap();
        assert!(t.insert(1, &[2], 1).is_err(), "a handle once");
    }

    #[test]
    fn would_free_is_release_without_the_change() {
        let mut t = DescTable::new();
        let mut gone = vec![];
        assert!(t.would_free(5, true, 8), "unknown");
        t.insert(5, &[5], 1).unwrap();
        assert!(!t.would_free(5, true, 8));
        assert!(t.would_free(5, false, 8));
        assert!(t.would_free(5, true, 0));
        t.acquire(&[5], 1).unwrap();
        assert!(!t.would_free(5, false, 8), "mapped twice");
        assert_eq!(t.counts(), (1, 0));
        assert_eq!(t.release(5, false, 8, &mut gone), Released::InUse);
        assert!(t.would_free(5, false, 8));
    }

    #[test]
    fn caching_off_frees_at_the_last_unmap() {
        let mut t = DescTable::new();
        let mut gone = vec![];
        t.insert(1, &[1], 1).unwrap();
        assert_eq!(t.release(1, true, 0, &mut gone), Released::Free, "bound 0 is off");
        assert_eq!(t.counts(), (0, 0));
    }

    #[test]
    fn the_key_sees_every_page_and_the_length() {
        assert_ne!(key(&[1, 2]), key(&[2, 1]));
        assert_ne!(key(&[0]), key(&[0, 0]));
        assert_eq!(key(&[7, 8, 9]), key(&[7, 8, 9]));
    }
}
