//! The producer's memory in the KMD's copy-engine client (milestone M3c-1): the pure half of the
//! dup + map cache. The I/O half is `kmd_render/src/virtio/rm_client/ce_dup.rs`. Design:
//! `docs/rm-copy-engine-present.md` 2.3, 11.2 and 14 (what is built).
//!
//! A copy-engine copy of a real Present reads two objects of the PRODUCER's RM client (NVK's,
//! named by the `'HEF3'` record, section 10): the timeline semaphore the push acquires on, and the
//! presented image. The KMD's channel client gets its own handle of each with
//! `NV_ESC_RM_DUP_OBJECT` (`hClientSrc` = the record's client, the parent the KMD client's device;
//! [`crate::rm_ce_channel::nvos55`]) and maps it into the channel's VA space with an
//! `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA`:
//!
//! * the semaphore: system memory, `PAGE_SIZE_4KB | CACHE_SNOOP_ENABLE` (the tool's
//!   `SYSMEM_MAP_FLAGS`), the page that holds `offset`;
//! * the source: the whole object, with the PTE kind the modifier names (`k` = 0x06 for every
//!   block-linear family NVK emits; `ce_present::source_plan`), first as nvk-rm 0005 maps an image
//!   in video memory (big pages, `PAGE_KIND_OVERRIDE`), then, if RM refuses that, as system memory
//!   with the same kind ([`map_tries`]). A pitch-linear source has no override.
//!
//! The mappings are cached per `(client, memory, what, kind, length)` in a bounded table
//! ([`Cache`]: [`SOURCE_SLOTS`] images, a swap chain rotates two or three, and
//! [`SEMAPHORE_SLOTS`] timelines), least recently used first out. Each slot owns a fixed pair of
//! handles ([`handles`]) and a fixed 64 MiB VA window ([`slot_va`]), so nothing is allocated to
//! name them and a slot's leftovers can never collide with another's. Teardown gives the slots
//! back youngest first ([`Cache::take_youngest`]), the reverse of their making.
//!
//! Nothing here does I/O, reads a clock or takes a lock.

use crate::rm_ce_channel as cc;

/// Images cached at most (a swap chain rotates two or three).
pub const SOURCE_SLOTS: usize = 4;
/// Timeline semaphores cached at most (one per presenting queue).
pub const SEMAPHORE_SLOTS: usize = 2;
pub const SLOTS: usize = SOURCE_SLOTS + SEMAPHORE_SLOTS;

/// The largest object a slot maps: its VA window.
pub const MAX_MAP_BYTES: u64 = cc::VA_WINDOW;
/// The largest semaphore mapping (the semaphore memory of an `NV_SEMAPHORE_SURFACE` is 4 KiB).
pub const MAX_SEMAPHORE_MAP: u64 = 64 * 1024;

/// What a slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Semaphore,
    Source,
}

/// One cached mapping's identity: the producer's object and how it is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub client: u32,
    pub memory: u32,
    pub what: What,
    /// The PTE kind of the mapping (`None`: the memory's own).
    pub kind: Option<u32>,
    /// Bytes mapped from offset 0.
    pub len: u64,
}

impl Key {
    const fn same_object(&self, other: &Key) -> bool {
        self.client == other.client
            && self.memory == other.memory
            && matches!(
                (self.what, other.what),
                (What::Semaphore, What::Semaphore) | (What::Source, What::Source)
            )
    }
}

/// One live slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: Key,
    pub slot: u8,
    /// The GPU VA RM answered (the slot's window, or RM's choice when it refused the fixed one).
    pub va: u64,
    /// The `NVOS46` flags the mapping was made with ([`map_word`] publishes them).
    pub map_flags: u32,
    /// Order of making (teardown runs youngest first).
    made: u64,
    /// Last use (eviction takes the least recent).
    used: u64,
}

/// The slots of one kind: sources first, then semaphores.
pub const fn slot_range(what: What) -> (usize, usize) {
    match what {
        What::Source => (0, SOURCE_SLOTS),
        What::Semaphore => (SOURCE_SLOTS, SLOTS),
    }
}

/// The RM handles of slot `slot`: the dup, and its virtual allocation.
pub const fn handles(slot: u8) -> (u32, u32) {
    let h = cc::H_DUP_BASE + 2 * slot as u32;
    (h, h + 1)
}

/// The fixed GPU VA window of slot `slot`.
pub const fn slot_va(slot: u8) -> u64 {
    cc::VA_DUP_BASE + slot as u64 * cc::VA_WINDOW
}

const _: () = assert!(SLOTS <= 16);
const _: () = assert!(cc::VA_SCRATCH + cc::VA_WINDOW <= cc::VA_DUP_BASE);

/// What [`Cache::plan`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Mapped already: the copy uses `Entry::va`.
    Hit(Entry),
    /// Dup and map into `slot`; `evict` must be unmapped and freed first (the slot's handles are
    /// reused).
    Make { slot: u8, evict: Option<Entry> },
}

/// The bounded table of dup'd and mapped producer objects.
#[derive(Clone, Copy, Debug)]
pub struct Cache {
    slots: [Option<Entry>; SLOTS],
    tick: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub const fn new() -> Self {
        Self { slots: [None; SLOTS], tick: 0 }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The slot for `key`: a hit (marked used), the slot that held the same object with another
    /// kind or length (re-made), a free slot of its kind, or the least recently used one.
    pub fn plan(&mut self, key: &Key) -> Plan {
        let (lo, hi) = slot_range(key.what);
        let now = self.next_tick();
        let mut same = None;
        let mut free = None;
        let mut oldest: Option<(usize, u64)> = None;
        for i in lo..hi {
            match &mut self.slots[i] {
                Some(e) if e.key == *key => {
                    e.used = now;
                    return Plan::Hit(*e);
                }
                Some(e) => {
                    if e.key.same_object(key) && same.is_none() {
                        same = Some(i);
                    }
                    if oldest.map_or(true, |(_, u)| e.used < u) {
                        oldest = Some((i, e.used));
                    }
                }
                None => {
                    if free.is_none() {
                        free = Some(i);
                    }
                }
            }
        }
        let i = same.or(free).or(oldest.map(|(i, _)| i)).unwrap_or(lo);
        Plan::Make { slot: i as u8, evict: self.slots[i] }
    }

    /// `slot` now holds `key`, mapped at `va` with `map_flags`.
    pub fn insert(&mut self, slot: u8, key: Key, va: u64, map_flags: u32) {
        let now = self.next_tick();
        if let Some(s) = self.slots.get_mut(slot as usize) {
            *s = Some(Entry { key, slot, va, map_flags, made: now, used: now });
        }
    }

    /// Forget `slot` (its objects were given back, or are lost with the client).
    pub fn remove(&mut self, slot: u8) -> Option<Entry> {
        self.slots.get_mut(slot as usize).and_then(Option::take)
    }

    /// The most recently made live slot, taken out (teardown order: the reverse of making).
    pub fn take_youngest(&mut self) -> Option<Entry> {
        let i = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|e| (i, e.made)))
            .max_by_key(|&(_, made)| made)?
            .0;
        self.slots[i].take()
    }

    /// Live slots: `(sources, semaphores)`.
    pub fn live(&self) -> (u32, u32) {
        let mut n = (0, 0);
        for e in self.slots.iter().flatten() {
            match e.key.what {
                What::Source => n.0 += 1,
                What::Semaphore => n.1 += 1,
            }
        }
        n
    }

    /// The live slot holding `(client, memory)` as `what`, if any (not marked used).
    pub fn find(&self, client: u32, memory: u32, what: What) -> Option<Entry> {
        self.slots
            .iter()
            .flatten()
            .find(|e| e.key.client == client && e.key.memory == memory && e.key.what == what)
            .copied()
    }

    /// Whether a live slot holds `(client, memory)` (any kind, length or role).
    pub fn is_cached(&self, client: u32, memory: u32) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|e| e.key.client == client && e.key.memory == memory)
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Forget everything (the transport is gone: the sweep closed the client).
    pub fn clear(&mut self) {
        self.slots = [None; SLOTS];
    }
}

/// One way to map a slot: `NVOS46` flags and the PTE kind (with
/// [`cc::MAP_FLAGS_KIND_OVERRIDE`] set in `flags` when `kind` is `Some`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapTry {
    pub flags: u32,
    pub kind: Option<u32>,
}

/// The ways to map `what` with `kind`, in order; the second runs only when RM refused the first
/// (an RM status, not a transport failure).
pub const fn map_tries(what: What, kind: Option<u32>) -> (MapTry, Option<MapTry>) {
    let sysmem = cc::MAP_FLAGS_SYSMEM;
    match (what, kind) {
        (What::Semaphore, _) => (MapTry { flags: sysmem, kind: None }, None),
        (What::Source, Some(k)) => (
            MapTry { flags: cc::MAP_FLAGS_PAGE_SIZE_BIG | cc::MAP_FLAGS_KIND_OVERRIDE, kind: Some(k) },
            Some(MapTry { flags: sysmem | cc::MAP_FLAGS_KIND_OVERRIDE, kind: Some(k) }),
        ),
        (What::Source, None) => (
            MapTry { flags: cc::MAP_FLAGS_PAGE_SIZE_BIG, kind: None },
            Some(MapTry { flags: sysmem, kind: None }),
        ),
    }
}

/// `CeMapFlags`: `kind << 24 | flags & 0xff_ffff` of a mapping (kind 0 without an override).
pub const fn map_word(t: MapTry) -> u32 {
    let kind = match t.kind {
        Some(k) => k & 0xff,
        None => 0,
    };
    (kind << 24) | (t.flags & 0x00ff_ffff)
}

/// Bytes of the semaphore mapping that holds the 64-bit value at `offset`: whole pages from 0,
/// at most [`MAX_SEMAPHORE_MAP`]. `None` when it would be larger (refused, never mapped).
pub const fn semaphore_map_len(offset: u64) -> Option<u64> {
    let Some(end) = offset.checked_add(8) else {
        return None;
    };
    let len = crate::round_up_page(end);
    if len > MAX_SEMAPHORE_MAP {
        None
    } else {
        Some(len)
    }
}

/// Bytes of a source mapping: the whole object, page-rounded, at most [`MAX_MAP_BYTES`].
pub const fn source_map_len(size: u64) -> Option<u64> {
    if size == 0 || size > MAX_MAP_BYTES {
        return None;
    }
    let len = crate::round_up_page(size);
    if len > MAX_MAP_BYTES {
        None
    } else {
        Some(len)
    }
}

/// `CeDupLive`: `semaphores << 8 | sources`.
pub const fn live_word(live: (u32, u32)) -> u32 {
    (live.1 << 8) | live.0
}

/// The counters the dup + map cache writes, all in `kmd_render/src/virtio/rm_client/ce_dup.rs`.
/// At most 14 characters, prefix `Ce`, unique across `kmd_render` and `kmd_logic`.
pub const COUNTERS: &[&str] = &[
    // `NV_ESC_RM_DUP_OBJECT`s sent, confirmed, refused; the last refusal's status (`fail_word`:
    // RM's `NV_STATUS` as it is, else `0x8000_0000 | kind << 16 | code`). The failing call itself
    // is also `CeRmCall` (escape 0x34, the source object's low 24 bits) / `CeRmStat`.
    "CeDupN",
    "CeDupOk",
    "CeDupFail",
    "CeDupStat",
    // GPU mappings of a dup made, refused (after the fallback), the last refusal's status; the
    // flags of the last source mapping (`map_word`).
    "CeMapOk",
    "CeMapFail",
    "CeMapStat",
    "CeMapFlags",
    // Live slots (`live_word`), slots given back (teardown or eviction), evictions.
    "CeDupLive",
    "CeDupFree",
    "CeDupEvict",
];

/// The files that write [`COUNTERS`] (relative to `kmd_render/src`).
pub const WRITERS: [&str; 1] = ["virtio/rm_client/ce_dup.rs"];

/// Helpers of the counter-name scans (`ce_dup`, `ce_shadow`).
#[cfg(test)]
pub(crate) mod scan {
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    pub fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    /// Every `b"..."` literal of alphanumerics in `text`, once each.
    pub fn literals(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    /// `writers` spell every name of `counters`, every `prefix` literal they spell is listed, and
    /// no other file of `kmd_render/src` spells one of `counters`.
    pub fn exact_list(counters: &[&str], writers: &[&str], prefix: &str) {
        let Some(render) = render_src() else {
            return;
        };
        let mut written = Vec::new();
        for f in writers {
            written.extend(literals(&std::fs::read_to_string(render.join(f)).unwrap()));
        }
        for n in counters {
            assert!(written.iter().any(|l| l == n), "{n} is listed but not written by {writers:?}");
        }
        for l in written.iter().filter(|l| l.starts_with(prefix)) {
            assert!(counters.contains(&l.as_str()), "{l} is written by {writers:?} but not listed");
        }
        let mut stack = std::vec![render];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if writers.iter().any(|w| s.ends_with(w)) {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in counters {
                        assert!(!text.contains(&std::format!("b\"{n}\"")), "{s} spells {n}");
                    }
                }
            }
        }
        assert!(checked > 20);
    }

    /// `names` fit the registry mirror (at most 14 characters, prefix `Ce`, alphanumerics), are
    /// unique, and are in none of the other `Ce` lists.
    pub fn names_fit(names: &[&str], others: &[&[&str]]) {
        let mut v: Vec<&str> = names.to_vec();
        for n in &v {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Ce"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        v.sort();
        let before = v.len();
        v.dedup();
        assert_eq!(v.len(), before, "duplicate counter name");
        for n in names {
            for o in others {
                assert!(!o.contains(n), "{n} is in another list");
            }
            assert_ne!(*n, crate::ce_present::KNOB);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn key(memory: u32, what: What) -> Key {
        Key { client: 0xc1d0_9d92, memory, what, kind: None, len: 4096 }
    }

    #[test]
    fn slots_have_their_own_handles_and_windows() {
        for s in 0..SLOTS as u8 {
            let (h, v) = handles(s);
            assert_eq!(v, h + 1);
            assert!(h >= cc::H_DUP_BASE && v < cc::H_DUP_BASE + 32);
            assert_eq!(slot_va(s) % cc::VA_WINDOW, 0);
            assert!(slot_va(s) + MAX_MAP_BYTES <= crate::ce_present::MAX_VA);
            assert!(slot_va(s) >= cc::VA_SCRATCH + cc::VA_WINDOW);
        }
        assert_ne!(handles(0).0, cc::H_SCRATCH);
        assert_eq!(slot_range(What::Source), (0, 4));
        assert_eq!(slot_range(What::Semaphore), (4, 6));
    }

    #[test]
    fn a_hit_a_free_slot_then_the_least_recently_used() {
        let mut c = Cache::new();
        let a = key(0x5c00_0079, What::Source);
        let Plan::Make { slot, evict: None } = c.plan(&a) else { panic!() };
        assert_eq!(slot, 0);
        c.insert(slot, a, slot_va(slot), 0);
        assert!(matches!(c.plan(&a), Plan::Hit(e) if e.slot == 0 && e.va == slot_va(0)));
        assert!(c.is_cached(0xc1d0_9d92, 0x5c00_0079));
        assert!(!c.is_cached(0xc1d0_9d92, 0x5c00_007a));
        assert!(!c.is_cached(0xc1d0_0001, 0x5c00_0079));
        assert_eq!(c.find(0xc1d0_9d92, 0x5c00_0079, What::Source).map(|e| e.slot), Some(0));
        assert_eq!(c.find(0xc1d0_9d92, 0x5c00_0079, What::Semaphore), None);
        for (i, m) in [0x5c00_007a, 0x5c00_007b, 0x5c00_007c].into_iter().enumerate() {
            let k = key(m, What::Source);
            let Plan::Make { slot, evict: None } = c.plan(&k) else { panic!() };
            assert_eq!(slot as usize, i + 1);
            c.insert(slot, k, slot_va(slot), 0);
        }
        // `a` was used last of all before the others were made... then used again: the oldest
        // use is now slot 1's.
        assert!(matches!(c.plan(&a), Plan::Hit(_)));
        let e = key(0x5c00_00ff, What::Source);
        let Plan::Make { slot, evict: Some(old) } = c.plan(&e) else { panic!() };
        assert_eq!((slot, old.key.memory), (1, 0x5c00_007a));
        assert_eq!(c.live(), (4, 0));
        // A semaphore never takes a source's slot.
        let s = key(0x5c00_00ee, What::Semaphore);
        let Plan::Make { slot, evict: None } = c.plan(&s) else { panic!() };
        assert_eq!(slot, 4);
    }

    #[test]
    fn the_same_object_with_another_kind_or_length_is_remade_in_its_slot() {
        let mut c = Cache::new();
        let a = key(0x5c00_0079, What::Source);
        c.insert(2, a, slot_va(2), 0);
        let b = Key { kind: Some(6), ..a };
        assert_eq!(c.plan(&b), Plan::Make { slot: 2, evict: c.slots[2] });
        let l = Key { len: 8192, ..a };
        assert!(matches!(c.plan(&l), Plan::Make { slot: 2, evict: Some(_) }));
        // Another client's object of the same number is another object.
        let other = Key { client: 0xc1d0_0001, ..a };
        assert!(matches!(c.plan(&other), Plan::Make { slot: 0, evict: None }));
    }

    #[test]
    fn teardown_runs_youngest_first() {
        let mut c = Cache::new();
        c.insert(4, key(1, What::Semaphore), slot_va(4), 0);
        c.insert(0, key(2, What::Source), slot_va(0), 0);
        c.insert(1, key(3, What::Source), slot_va(1), 0);
        let order: std::vec::Vec<u8> = core::iter::from_fn(|| c.take_youngest().map(|e| e.slot)).collect();
        assert_eq!(order, [1, 0, 4]);
        assert!(c.is_empty());
        c.insert(5, key(1, What::Semaphore), slot_va(5), 0);
        assert_eq!(c.remove(5).map(|e| e.slot), Some(5));
        assert_eq!(c.remove(5), None);
        c.insert(5, key(1, What::Semaphore), slot_va(5), 0);
        c.clear();
        assert_eq!(c.live(), (0, 0));
    }

    #[test]
    fn the_map_tries() {
        let (first, second) = map_tries(What::Source, Some(6));
        assert_eq!(first, MapTry { flags: 0x0008_0200, kind: Some(6) });
        assert_eq!(second, Some(MapTry { flags: 0x0008_0110, kind: Some(6) }));
        assert_eq!(map_word(first), 0x0608_0200);
        let (first, second) = map_tries(What::Source, None);
        assert_eq!((first.flags, first.kind), (0x200, None));
        assert_eq!(second.map(|t| t.flags), Some(cc::MAP_FLAGS_SYSMEM));
        let (sem, none) = map_tries(What::Semaphore, Some(6));
        assert_eq!((sem, none), (MapTry { flags: 0x110, kind: None }, None));
    }

    #[test]
    fn mapping_lengths() {
        assert_eq!(semaphore_map_len(0), Some(4096));
        assert_eq!(semaphore_map_len(4088), Some(4096));
        assert_eq!(semaphore_map_len(4089), Some(8192));
        assert_eq!(semaphore_map_len(MAX_SEMAPHORE_MAP), None);
        assert_eq!(semaphore_map_len(u64::MAX), None);
        // The records of section 10: Heaven's and the 5120x1440 one.
        assert_eq!(source_map_len(6_553_600), Some(6_553_600));
        assert_eq!(source_map_len(31_457_280), Some(31_457_280));
        assert_eq!(source_map_len(5), Some(4096));
        assert_eq!(source_map_len(0), None);
        assert_eq!(source_map_len(MAX_MAP_BYTES + 1), None);
        assert_eq!(live_word((3, 1)), 0x103);
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        scan::names_fit(
            COUNTERS,
            &[
                crate::ce_present::COUNTERS,
                crate::rm_ce_channel::COUNTERS,
                crate::ce_record::COUNTERS,
                crate::blt_async::COUNTERS,
                crate::guest_blob::COUNTERS,
                &crate::onscanout::COUNTERS,
            ],
        );
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        scan::exact_list(COUNTERS, &WRITERS, "CeDup");
        scan::exact_list(COUNTERS, &WRITERS, "CeMap");
    }
}
