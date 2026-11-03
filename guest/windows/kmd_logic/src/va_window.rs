//! The VA windows of a per-process RM VA space: where they lie, how RM is asked to make them,
//! and which window holds a VA. The pure half; the I/O half is `ensure_space` and the window
//! users in `kmd_render/src/virtio/native_memory.rs`.
//!
//! Every process gets its own `FERMI_VASPACE_A` with two `NV50_MEMORY_VIRTUAL` objects over
//! the VAs VidMm hands out: the low window [64 KiB, 4 GiB) and the high window [8 GiB, 64 GiB).
//! VidMm's mappings there go into the window at their exact address (no virtual object per
//! mapping), and RM's own first-fit mappings (GR context buffers and the like) are pushed into
//! the gap [4 GiB, 8 GiB) that the KMD reserves from VidMm.
//!
//! # Why the windows are LAZY
//!
//! RM (`virtmemConstruct`, `src/kernel/mem_mgr/virtual_mem.c`) treats a virtual object without
//! `NVOS32_ALLOC_FLAGS_LAZY` as a reservation whose page tables must exist up front:
//!
//! * it reserves page-table pool memory for the whole object, sized for the worst case of 4-KiB
//!   PTEs (`vaspaceReserveMempool` -> `gvaspaceReserveMempool`, `gpu_vaspace.c`), and
//! * `gvaspaceAlloc` pins every page-table level of the range for each page size of the lock
//!   mask, which for the default page size is 4 KiB and the big page
//!   (`mmuWalkReserveEntries`, `gvaspaceApplyDefaultAlignment`).
//!
//! For 56 GiB that is ~28k small page tables (112 MiB at 4 KiB each) plus the big-page tables
//! and directories, allocated, scrubbed and written while the call holds RM's GPU lock; the
//! free walks and releases them again. Measured (405.288): 83 ms for the 4 GiB low window,
//! ~1.16 s for the 56 GiB high window, 1.2-1.6 s to free it, per process start and exit.
//!
//! With `LAZY` both steps are skipped: RM only records the VA range in its heap (the
//! reservation that keeps its own mappings out is unchanged), and page tables are made when
//! something is mapped. The map path already reserves pool memory for each mapping
//! (`rmapiMapWithSecInfo`, `src/kernel/rmapi/mapping.c`), and `mmuWalkMap` allocates the levels
//! it needs. RM uses the same flags for its own fixed windows (`mem_utils_gm107.c`, the
//! CE-utils FB alias window). A SPARSE window cannot be lazy: RM drops `LAZY` from a sparse
//! virtual allocation (`stdmemValidateParams`, `standard_mem.c`), so `NvPtSparse` windows stay
//! slow.
//!
//! # Knob
//!
//! `NvVaWin` (REG_DWORD, default 0). Bit 0 ([`PIN`]): make the windows without `LAZY`, with
//! their page tables pinned up front (the behaviour before).
//!
//! # Counters
//!
//! [`COUNTERS`]: per operation ([`Op`]) the count, the total and the longest time in
//! microseconds, and `NvVaWinFlg`, the extra `NVOS32` flags the last window was made with.

extern crate alloc;
use alloc::vec::Vec;

/// The low window: [64 KiB, 4 GiB), VidMm's fences (0x10000) and the UMDs' low VAs included.
pub const LOW_BASE: u64 = 0x1_0000;
pub const LOW_END: u64 = 0x1_0000_0000;
/// The range the KMD reserves from VidMm for RM's own first-fit mappings: no window covers it.
pub const RESERVED_BASE: u64 = 0x1_0000_0000;
pub const RESERVED_END: u64 = 0x2_0000_0000;
/// The high window: [8 GiB, 64 GiB), where VidMm places allocations above the reserved range
/// and CUDA reserves its fixed range. A window over the whole 16 TiB exhausted RM's page-table
/// pool (it was not lazy then); VAs above 64 GiB map through fixed virtual objects.
pub const HIGH_BASE: u64 = 0x2_0000_0000;
pub const HIGH_END: u64 = 0x10_0000_0000;

/// The windows of a space in the order they are made: (first VA, end VA, high?).
pub const LAYOUT: [(u64, u64, bool); 2] = [(LOW_BASE, LOW_END, false), (HIGH_BASE, HIGH_END, true)];

/// `NVOS32_ALLOC_FLAGS_LAZY`: no page-table pool reservation and no pinned page tables.
pub const NVOS32_ALLOC_FLAGS_LAZY: u32 = 0x0000_0400;

/// `NvVaWin` bit 0: pin the windows' page tables up front (no `LAZY`).
pub const PIN: u32 = 1 << 0;

/// The extra `NVOS32` flags of a window under `NvVaWin` = `knob`, given the SPARSE flags the
/// window takes from `NvPtSparse` (`pt_ring::window_flags`). Sparse wins: RM would strip
/// `LAZY` from it anyway, and the sparse state is what that knob asks for.
pub const fn window_flags(knob: u32, sparse: u32) -> u32 {
    if sparse != 0 {
        sparse
    } else if knob != u32::MAX && knob & PIN != 0 {
        0
    } else {
        NVOS32_ALLOC_FLAGS_LAZY
    }
}

/// One window: the space it belongs to, its RM handle and [base, end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub space: u32,
    pub handle: u32,
    pub base: u64,
    pub end: u64,
}

impl Window {
    /// Whether [va, end) lies inside this window (`end` exclusive).
    pub const fn holds(&self, va: u64, end: u64) -> bool {
        va >= self.base && end <= self.end && va <= end
    }

    const fn overlaps(&self, base: u64, end: u64) -> bool {
        base < self.end && self.base < end
    }
}

/// Why a window was not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// Empty range or a zero handle.
    Invalid,
    /// The handle is already a window.
    Duplicate,
    /// It overlaps a window of the same space.
    Overlap,
    /// No memory for the record.
    NoMemory,
}

/// Every live window of every space.
#[derive(Debug, Default)]
pub struct Windows {
    list: Vec<Window>,
}

impl Windows {
    pub const fn new() -> Self {
        Self { list: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// Room for `n` more windows, so a later [`Windows::add`] cannot fail for memory.
    pub fn reserve(&mut self, n: usize) -> Result<(), AddError> {
        self.list.try_reserve(n).map_err(|_| AddError::NoMemory)
    }

    /// Record a window RM made.
    pub fn add(&mut self, w: Window) -> Result<(), AddError> {
        if w.handle == 0 || w.base >= w.end {
            return Err(AddError::Invalid);
        }
        if self.is_window(w.handle) {
            return Err(AddError::Duplicate);
        }
        if self.list.iter().any(|o| o.space == w.space && o.overlaps(w.base, w.end)) {
            return Err(AddError::Overlap);
        }
        self.reserve(1)?;
        self.list.push(w);
        Ok(())
    }

    /// The window of `space` that holds [va, end), if any.
    pub fn find(&self, space: u32, va: u64, end: u64) -> Option<u32> {
        self.list
            .iter()
            .find(|w| w.space == space && w.holds(va, end))
            .map(|w| w.handle)
    }

    /// Whether `handle` is a window (whose mappings do not own it).
    pub fn is_window(&self, handle: u32) -> bool {
        handle != 0 && self.list.iter().any(|w| w.handle == handle)
    }

    /// The windows of `space`, into `out` (cleared first).
    pub fn of(&self, space: u32, out: &mut Vec<u32>) -> Result<(), AddError> {
        out.clear();
        let n = self.list.iter().filter(|w| w.space == space).count();
        out.try_reserve(n).map_err(|_| AddError::NoMemory)?;
        out.extend(self.list.iter().filter(|w| w.space == space).map(|w| w.handle));
        Ok(())
    }

    /// Forget the window `handle` (RM freed it). Whether it was recorded.
    pub fn remove(&mut self, handle: u32) -> bool {
        let before = self.list.len();
        self.list.retain(|w| w.handle != handle);
        self.list.len() != before
    }

    /// Forget every window of `space` (RM freed the space and its dependants). How many.
    pub fn remove_space(&mut self, space: u32) -> usize {
        let before = self.list.len();
        self.list.retain(|w| w.space != space);
        before - self.list.len()
    }
}

/// A timed window or space operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `RM_ALLOC` of a window.
    WinMake = 0,
    /// `RM_FREE` of a window.
    WinFree = 1,
    /// `RM_ALLOC` of a per-process VA space.
    SpaceMake = 2,
    /// `RM_FREE` of a per-process VA space.
    SpaceFree = 3,
}

pub const OPS: usize = 4;

/// Per [`Op`]: the count, the total microseconds and the longest microseconds (at most 14
/// characters each, `diag::record_named_bytes`).
pub const COUNTERS: [[&str; 3]; OPS] = [
    ["NvVaWinMkN", "NvVaWinMkUs", "NvVaWinMkMax"],
    ["NvVaWinFrN", "NvVaWinFrUs", "NvVaWinFrMax"],
    ["NvVaSpMkN", "NvVaSpMkUs", "NvVaSpMkMax"],
    ["NvVaSpFrN", "NvVaSpFrUs", "NvVaSpFrMax"],
];

/// The extra flags of the last window made.
pub const FLAGS_COUNTER: &str = "NvVaWinFlg";

/// Microseconds of an interval in 100-ns ticks, saturating at `u32::MAX`.
pub const fn us(ticks_100ns: u64) -> u32 {
    let us = ticks_100ns / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// One operation's figures, as published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub n: u32,
    pub total_us: u32,
    pub max_us: u32,
}

impl Stat {
    pub const ZERO: Self = Self { n: 0, total_us: 0, max_us: 0 };

    /// Add one operation that took `us` microseconds.
    pub fn note(&mut self, us: u32) {
        self.n = self.n.wrapping_add(1);
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    #[test]
    fn the_layout_leaves_rm_its_range() {
        for (base, end, _) in LAYOUT {
            assert!(base < end);
            assert!(end <= RESERVED_BASE || base >= RESERVED_END, "{base:#x}..{end:#x}");
        }
        assert_eq!(LOW_END, RESERVED_BASE);
        assert_eq!(HIGH_BASE, RESERVED_END);
        assert_eq!(LAYOUT[0].0, 0x1_0000);
        assert_eq!(HIGH_END - HIGH_BASE, 56 << 30);
        assert!(!LAYOUT[0].2 && LAYOUT[1].2);
    }

    #[test]
    fn windows_are_lazy_unless_pinned_or_sparse() {
        assert_eq!(window_flags(0, 0), NVOS32_ALLOC_FLAGS_LAZY);
        assert_eq!(window_flags(PIN, 0), 0);
        assert_eq!(window_flags(PIN | 2, 0), 0);
        assert_eq!(window_flags(2, 0), NVOS32_ALLOC_FLAGS_LAZY);
        // u32::MAX is the default too (`pt_ring::window_flags` treats it as unset).
        assert_eq!(window_flags(u32::MAX, 0), NVOS32_ALLOC_FLAGS_LAZY);
        let sparse = crate::pt_ring::NVOS32_ALLOC_FLAGS_SPARSE;
        assert_eq!(window_flags(0, sparse), sparse);
        assert_eq!(window_flags(PIN, sparse), sparse);
        // Together with the knob that makes the sparse flags.
        assert_eq!(window_flags(0, crate::pt_ring::window_flags(2, true)), sparse);
        assert_eq!(window_flags(0, crate::pt_ring::window_flags(2, false)), NVOS32_ALLOC_FLAGS_LAZY);
        assert_eq!(NVOS32_ALLOC_FLAGS_LAZY & sparse, 0);
    }

    fn two_spaces() -> Windows {
        let mut w = Windows::new();
        for (space, first) in [(10, 100), (20, 200)] {
            for (i, (base, end, _)) in LAYOUT.into_iter().enumerate() {
                w.add(Window { space, handle: first + i as u32, base, end }).unwrap();
            }
        }
        w
    }

    #[test]
    fn a_va_finds_the_window_of_its_space() {
        let w = two_spaces();
        assert_eq!(w.len(), 4);
        assert_eq!(w.find(10, 0x1_0000, 0x2_0000), Some(100));
        assert_eq!(w.find(20, 0x1_0000, 0x2_0000), Some(200));
        assert_eq!(w.find(10, 0xffff_0000, 0x1_0000_0000), Some(100));
        assert_eq!(w.find(10, 0x2_0000_0000, 0x2_0001_0000), Some(101));
        assert_eq!(w.find(20, 0xf_ffff_0000, 0x10_0000_0000), Some(201));
        // Below the low window, in RM's range, straddling, above the high window.
        assert_eq!(w.find(10, 0, 0x1_0000), None);
        assert_eq!(w.find(10, 0x1_0000_0000, 0x1_0001_0000), None);
        assert_eq!(w.find(10, 0xffff_0000, 0x1_0001_0000), None);
        assert_eq!(w.find(10, 0x1_ffff_0000, 0x2_0001_0000), None);
        assert_eq!(w.find(10, 0x10_0000_0000, 0x10_0001_0000), None);
        assert_eq!(w.find(10, 0xf_ffff_0000, 0x10_0001_0000), None);
        // A reversed range is never inside.
        assert_eq!(w.find(10, 0x3_0000_0000, 0x2_0000_0000), None);
        // An unknown space.
        assert_eq!(w.find(30, 0x1_0000, 0x2_0000), None);
    }

    #[test]
    fn handles_and_spaces_are_listed_and_removed() {
        let mut w = two_spaces();
        assert!(w.is_window(101) && w.is_window(200));
        assert!(!w.is_window(0) && !w.is_window(102));
        let mut out = vec![7];
        w.of(10, &mut out).unwrap();
        assert_eq!(out, vec![100, 101]);
        w.of(30, &mut out).unwrap();
        assert!(out.is_empty());
        assert!(w.remove(101));
        assert!(!w.remove(101));
        assert_eq!(w.find(10, 0x2_0000_0000, 0x2_0001_0000), None);
        assert_eq!(w.find(20, 0x2_0000_0000, 0x2_0001_0000), Some(201));
        assert_eq!(w.remove_space(20), 2);
        assert_eq!(w.remove_space(20), 0);
        assert_eq!(w.len(), 1);
        w.clear();
        assert!(w.is_empty());
    }

    #[test]
    fn bad_windows_are_refused() {
        let mut w = two_spaces();
        let ok = Window { space: 30, handle: 300, base: LOW_BASE, end: LOW_END };
        assert_eq!(w.add(Window { handle: 0, ..ok }), Err(AddError::Invalid));
        assert_eq!(w.add(Window { end: LOW_BASE, ..ok }), Err(AddError::Invalid));
        assert_eq!(w.add(Window { handle: 100, ..ok }), Err(AddError::Duplicate));
        assert_eq!(
            w.add(Window { space: 10, handle: 300, base: 0xffff_0000, end: 0x1_0001_0000 }),
            Err(AddError::Overlap)
        );
        // Adjacent in the same space is fine; the same range in another space too.
        w.add(Window { space: 10, handle: 300, base: RESERVED_BASE, end: RESERVED_END }).unwrap();
        assert_eq!(w.add(ok), Err(AddError::Duplicate));
        w.add(Window { handle: 301, ..ok }).unwrap();
        assert_eq!(w.find(30, 0x1_0000, 0x2_0000), Some(301));
    }

    #[test]
    fn stats_add_up_and_saturate() {
        assert_eq!(us(0), 0);
        assert_eq!(us(9), 0);
        assert_eq!(us(11_630_000), 1_163_000);
        assert_eq!(us(u64::MAX), u32::MAX);
        let mut s = Stat::default();
        s.note(83_000);
        s.note(1_163_000);
        s.note(10);
        assert_eq!(s, Stat { n: 3, total_us: 1_246_010, max_us: 1_163_000 });
        s.note(u32::MAX);
        assert_eq!(s.total_us, u32::MAX);
        assert_eq!(s.max_us, u32::MAX);
    }

    #[test]
    fn counter_names_fit() {
        for set in COUNTERS {
            for name in set {
                assert!(name.len() <= 14, "{name}");
            }
        }
        assert!(FLAGS_COUNTER.len() <= 14);
    }
}
