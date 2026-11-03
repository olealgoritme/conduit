//! Placement, page-size and leaf-update decisions for native (NVDA) allocations.
//!
//! The I/O half is `kmd_render/src/virtio/native_memory.rs` and
//! `kmd_render/src/ddi/create_allocation.rs`. Everything here is behind the
//! `NvPerfMem` knob ([`PerfMem`]); with the knob at 0 every function answers
//! what the driver did before it existed.
//!
//! What the 610.88 captures (2026-10-31, D3D11/D3D12/Vulkan/DWM, 259 records)
//! show and these rules follow:
//!
//! * `+0x10` bit 11 of an NVDA record marks an allocation the UMD never maps for
//!   the CPU: no record with the bit was ever `Lock2`ed (0 of 81), and 175 of the
//!   178 records without it were. NVIDIA's KMD keeps those in
//!   video memory; this driver used to report every non-shared native allocation
//!   CpuVisible, which let VidMm keep GPU-only memory in system pages.
//! * NVIDIA's `DxgkDdiMapCpuHostAperture` runs almost only for VidMm's own
//!   page tables (2063 of 2069 calls are one page from the System process): the
//!   UMDs' CPU-visible allocations live in system memory on real hardware too.
//! * GPU-only D3D records larger than 64 KiB got 64-KiB-aligned VAs from VidMm
//!   (one exactly-64-KiB record got 4 KiB), those of 2 MiB and more 2-MiB-aligned
//!   ones; every CUDA allocation (all GPU-only, all at least 2 MiB) was 2-MiB aligned.

extern crate alloc;

use crate::native_memory::{self as nm, MappingPiece};
use alloc::vec::Vec;

/// `+0x10` of an NVDA record (D3D, Vulkan and CUDA alike): bit 11 set = the UMD
/// never maps the allocation for the CPU.
pub const GPU_ONLY_AT: usize = 0x10;
pub const GPU_ONLY_BIT: u32 = 1 << 11;

/// Whether an NVDA record describes memory only the GPU touches.
pub fn is_gpu_only_record(bytes: &[u8]) -> bool {
    nm::get32(bytes, GPU_ONLY_AT).is_some_and(|kind| kind & GPU_ONLY_BIT != 0)
}

/// The `NvPerfMem` knob (REG_DWORD, default [`PERF_MEM_DEFAULT`]; 0 = the old
/// behaviour everywhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PerfMem(pub u32);

/// `NvPerfMem` when the knob is absent: [`PerfMem::GPU_ONLY_VRAM`]. NVIDIA's KMD
/// keeps every record with `+0x10` bit 11 in video memory, and the Vulkan ICD
/// marks its device-local buffers and images that way (67 of the 151 typed
/// records in the vktest capture, every image among them). Reported CpuVisible instead, VidMm backs
/// them with system pages (the `NvPgLeafSys` leaf updates): Steel Nomad's
/// Vulkan timeline ran out of memory for its images ("Could not get memory for
/// an image!") with no KMD-side refusal. The aperture stays in their segment set
/// as VidMm's eviction fallback.
///
/// Plus [`PerfMem::CPU_VISIBLE_APERTURE`]: a CPU-visible native allocation in our memory
/// segment (CpuVisible=0, SupportsCpuHostAperture) makes every D3DKMTLock2 a "complex"
/// lock that VidMm's paging worker serves by suspending the device's contexts and
/// evicting; with NVIDIA's UMD the context it suspends can be the very one waiting on the
/// locking thread (Steel Nomad D3D12: Xid 109 CTX SWITCH TIMEOUT). NVIDIA keeps UMD
/// CPU-visible allocations in system memory; in the aperture they get a simple lock.
pub const PERF_MEM_DEFAULT: u32 = PerfMem::GPU_ONLY_VRAM | PerfMem::CPU_VISIBLE_APERTURE;

impl PerfMem {
    /// GPU-only allocations are not CpuVisible and prefer the memory segment
    /// (the aperture stays in their set as an eviction fallback).
    pub const GPU_ONLY_VRAM: u32 = 0x01;
    /// As [`Self::GPU_ONLY_VRAM`], without the aperture in their segment set.
    pub const GPU_ONLY_VRAM_STRICT: u32 = 0x02;
    /// D3D and CUDA GPU-only allocations get 64-KiB / 2-MiB alignment.
    pub const ALIGN_LARGE: u32 = 0x04;
    /// Video-memory mappings use RM big pages (64 KiB) where VA, offset and length allow.
    pub const BIG_PAGES: u32 = 0x08;
    /// GPU-only allocations of 2 MiB and more get RM huge-page (2 MiB) memory and mappings.
    pub const HUGE_PAGES: u32 = 0x10;
    /// Within one leaf update, every RM map/unmap but the last defers its TLB invalidate.
    pub const TLB_DEFER: u32 = 0x20;
    /// A system-page leaf update that names the pages already mapped there is skipped.
    pub const SKIP_UNCHANGED: u32 = 0x40;
    /// CPU-visible (non-shared, non-GPU-only) native allocations live in the aperture
    /// segment only (simple CPU locks, no suspend/evict round trips).
    pub const CPU_VISIBLE_APERTURE: u32 = 0x80;

    const fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
    pub const fn gpu_only_mode(self) -> GpuOnlyMode {
        if self.has(Self::GPU_ONLY_VRAM_STRICT) {
            GpuOnlyMode::VramOnly
        } else if self.has(Self::GPU_ONLY_VRAM) {
            GpuOnlyMode::VramFirst
        } else {
            GpuOnlyMode::Off
        }
    }
    pub const fn align_large(self) -> bool {
        self.has(Self::ALIGN_LARGE)
    }
    pub const fn big_pages(self) -> bool {
        self.has(Self::BIG_PAGES)
    }
    pub const fn huge_pages(self) -> bool {
        self.has(Self::HUGE_PAGES)
    }
    pub const fn tlb_defer(self) -> bool {
        self.has(Self::TLB_DEFER)
    }
    pub const fn skip_unchanged(self) -> bool {
        self.has(Self::SKIP_UNCHANGED)
    }
    pub const fn cpu_visible_aperture(self) -> bool {
        self.has(Self::CPU_VISIBLE_APERTURE)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuOnlyMode {
    Off,
    VramFirst,
    VramOnly,
}

/// `DXGK_ALLOCATIONINFOFLAGS_WDDM2_0` bits the native path sets.
pub const FLAG_CPU_VISIBLE: u32 = 0x01;
pub const FLAG_CACHED: u32 = 0x04;
pub const FLAG_EXISTING_SYSMEM: u32 = 0x10;

/// What VidMm is told about one native allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Preferred segment (1-based id).
    pub segment: u32,
    /// Supported read/write segment set (bit 0 = segment 1).
    pub supported: u32,
    /// `FlagsWddm2.Value`.
    pub flags: u32,
    /// Placed by the GPU-only rule (counted by the caller).
    pub gpu_only: bool,
}

const fn segment_bit(id: u32) -> u32 {
    if id == 0 || id > 32 {
        0
    } else {
        1 << (id - 1)
    }
}

/// The segment set and flags of a native allocation.
///
/// `memory_segment` is the reported memory segment (None without one),
/// `system` a system-memory backed allocation (pSystemMem, CUDA user memory),
/// `shared` a shareable record. With `mode` Off this is exactly the old rule:
/// system memory in the aperture (CpuVisible | Cached | ExistingSysMem),
/// everything else preferring the memory segment with the aperture in its set,
/// CpuVisible unless shared (dxgkrnl refuses a shareable CpuVisible allocation
/// whose set is not all aperture).
pub fn placement(
    aperture: u32,
    memory_segment: Option<u32>,
    system: bool,
    shared: bool,
    gpu_only: bool,
    mode: GpuOnlyMode,
) -> Placement {
    placement_with(aperture, memory_segment, system, shared, gpu_only, mode, false)
}

/// [`placement`], with `cpu_aperture` ([`PerfMem::CPU_VISIBLE_APERTURE`]): a CPU-visible
/// non-shared allocation that is not placed by the GPU-only rule goes to the aperture only.
pub fn placement_with(
    aperture: u32,
    memory_segment: Option<u32>,
    system: bool,
    shared: bool,
    gpu_only: bool,
    mode: GpuOnlyMode,
    cpu_aperture: bool,
) -> Placement {
    let segment = if system {
        aperture
    } else {
        memory_segment.unwrap_or(aperture)
    };
    let both = segment_bit(segment) | segment_bit(aperture);
    if system {
        return Placement {
            segment,
            supported: both,
            flags: FLAG_CPU_VISIBLE | FLAG_CACHED | FLAG_EXISTING_SYSMEM,
            gpu_only: false,
        };
    }
    if gpu_only && memory_segment.is_some() && mode != GpuOnlyMode::Off {
        let supported = if mode == GpuOnlyMode::VramOnly {
            segment_bit(segment)
        } else {
            both
        };
        return Placement {
            segment,
            supported,
            flags: 0,
            gpu_only: true,
        };
    }
    if cpu_aperture && !shared {
        return Placement {
            segment: aperture,
            supported: segment_bit(aperture),
            flags: FLAG_CPU_VISIBLE,
            gpu_only: false,
        };
    }
    Placement {
        segment,
        supported: both,
        flags: if shared { 0 } else { FLAG_CPU_VISIBLE },
        gpu_only: false,
    }
}

pub const BIG_PAGE: u64 = 0x1_0000;
pub const HUGE_PAGE: u64 = 0x20_0000;

/// VidMm alignment of a GPU-only D3D or CUDA allocation, as NVIDIA's KMD got:
/// 2 MiB from 2 MiB on, 64 KiB above 64 KiB, 4 KiB (None) otherwise.
pub fn gpu_only_alignment(size: u64) -> Option<u32> {
    if size >= HUGE_PAGE {
        Some(HUGE_PAGE as u32)
    } else if size > BIG_PAGE {
        Some(BIG_PAGE as u32)
    } else {
        None
    }
}

/// The large-page alignment of a native allocation record (`None` = 4 KiB).
/// Vulkan records keep the page bits of their writeback (`nvgl_alignment`,
/// unconditional as before); D3D and CUDA GPU-only records get
/// [`gpu_only_alignment`] when `align_large` is on. System memory never does.
pub fn large_page_alignment(bytes: &[u8], size: u64, system: bool, align_large: bool) -> Option<u32> {
    if system {
        return None;
    }
    if nm::is_nvgl_record(bytes) {
        return nm::nvgl_alignment(bytes);
    }
    if align_large && is_gpu_only_record(bytes) {
        return gpu_only_alignment(size);
    }
    None
}

/// Whether a native allocation's RM memory is allocated with 2-MiB pages.
pub fn wants_huge_memory(perf: PerfMem, gpu_only: bool, system: bool, size: u64) -> bool {
    perf.huge_pages() && gpu_only && !system && size >= HUGE_PAGE
}

/// `NVOS32_ATTR_PAGE_SIZE` (24:23) and `NVOS32_ATTR2_PAGE_SIZE_HUGE` (21:20).
pub const ATTR_PAGE_SIZE_MASK: u32 = 3 << 23;
pub const ATTR_PAGE_SIZE_HUGE: u32 = 3 << 23;
pub const ATTR2_PAGE_SIZE_HUGE_MASK: u32 = 3 << 20;
pub const ATTR2_PAGE_SIZE_HUGE_2MB: u32 = 1 << 20;

/// `attr`/`attr2` of video memory with 2-MiB pages, from the big-page pair.
pub const fn huge_attrs(attr: u32, attr2: u32) -> (u32, u32) {
    (
        (attr & !ATTR_PAGE_SIZE_MASK) | ATTR_PAGE_SIZE_HUGE,
        (attr2 & !ATTR2_PAGE_SIZE_HUGE_MASK) | ATTR2_PAGE_SIZE_HUGE_2MB,
    )
}

/// `NVOS46_FLAGS_PAGE_SIZE` (11:8) values.
pub const MAP_PAGE_4K: u32 = 1;
pub const MAP_PAGE_BIG: u32 = 2;
pub const MAP_PAGE_HUGE: u32 = 4;
pub const MAP_PAGE_SIZE_MASK: u32 = 0xf << 8;
/// `NVOS46_FLAGS_DEFER_TLB_INVALIDATION_TRUE` (31:31).
pub const MAP_FLAGS_DEFER_TLB: u32 = 1 << 31;
/// `NVOS47_PARAMETERS.flags` and its `DEFER_TLB_INVALIDATION_TRUE` (0:0).
pub const NVOS47_FLAGS_AT: usize = 16;
pub const UNMAP_FLAGS_DEFER_TLB: u32 = 1;
/// `NV0080_CTRL_CMD_DMA_INVALIDATE_TLB` and its 8-byte params (hVASpace, flags).
pub const CTRL_DMA_INVALIDATE_TLB: u32 = 0x0080_180c;
pub const INVALIDATE_TLB_PARAMS_BYTES: usize = 8;

pub fn invalidate_tlb_params(h_vaspace: u32) -> [u8; INVALIDATE_TLB_PARAMS_BYTES] {
    let mut a = [0u8; INVALIDATE_TLB_PARAMS_BYTES];
    a[..4].copy_from_slice(&h_vaspace.to_le_bytes());
    a
}

/// The largest RM page a video-memory mapping of `piece` can use: VA, memory
/// offset and length all aligned to it. `huge` only for memory allocated with
/// 2-MiB pages.
pub fn map_page_size(piece: MappingPiece, big: bool, huge: bool) -> u32 {
    let aligned = |unit: u64| {
        piece.len != 0
            && piece.va % unit == 0
            && piece.memory_offset % unit == 0
            && piece.len % unit == 0
    };
    if huge && aligned(HUGE_PAGE) {
        MAP_PAGE_HUGE
    } else if (big || huge) && aligned(BIG_PAGE) {
        MAP_PAGE_BIG
    } else {
        MAP_PAGE_4K
    }
}

/// `flags` with its page-size field set to `page`.
pub const fn with_page_size(flags: u32, page: u32) -> u32 {
    (flags & !MAP_PAGE_SIZE_MASK) | ((page & 0xf) << 8)
}

/// Whether the `index`-th of `count` RM map/unmap operations of one leaf update
/// may defer its TLB invalidate: all but the last, so the last one's invalidate
/// covers the batch.
pub const fn defer_tlb(enabled: bool, index: usize, count: usize) -> bool {
    enabled && index + 1 < count
}

/// Pages per OS descriptor of a system-page leaf update: `NvSysChunk`, 0 = the
/// default 128 (RM refused 512- and 335-page descriptors once), at most 4096.
pub const DEFAULT_SYSTEM_CHUNK_PAGES: usize = 128;
pub fn system_chunk_pages(knob: u32) -> usize {
    match knob {
        0 => DEFAULT_SYSTEM_CHUNK_PAGES,
        n => (n as usize).min(4096),
    }
}

/// One OS-descriptor chunk of a system-page leaf update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemChunk {
    pub va: u64,
    pub memory_offset: u64,
    /// Index of its first page in the update's page list.
    pub first: usize,
    pub pages: usize,
}

impl SystemChunk {
    pub const fn bytes(&self) -> u64 {
        self.pages as u64 * nm::PAGE_BYTES
    }
}

/// The chunks `update_leaf_system` maps for `pages` pages at `first_va`.
pub fn system_chunks(first_va: u64, memory_offset: u64, pages: usize, chunk_pages: usize) -> Vec<SystemChunk> {
    let chunk_pages = chunk_pages.max(1);
    let mut out = Vec::new();
    let mut first = 0;
    while first < pages {
        let n = (pages - first).min(chunk_pages);
        let delta = first as u64 * nm::PAGE_BYTES;
        out.push(SystemChunk {
            va: first_va + delta,
            memory_offset: memory_offset + delta,
            first,
            pages: n,
        });
        first += n;
    }
    out
}

/// One live mapping of the allocation in the updated process that overlaps the
/// update: its piece and, for a system-page mapping, the pages it shows.
#[derive(Clone, Copy, Debug)]
pub struct LiveMapping<'a> {
    pub piece: MappingPiece,
    pub pfns: Option<&'a [u64]>,
}

/// Whether a system-page leaf update would map exactly what is mapped already:
/// the allocation's live mappings that overlap the range are the update's
/// chunks one for one (same VA, length and allocation offset) and show the
/// same pages. Anything else (a video-memory mapping, a partial overlap, other
/// pages) needs the unmap/map.
pub fn system_update_unchanged(
    live: &[LiveMapping<'_>],
    first_va: u64,
    memory_offset: u64,
    pfns: &[u64],
    chunk_pages: usize,
) -> bool {
    if pfns.is_empty() {
        return false;
    }
    let len = pfns.len() as u64 * nm::PAGE_BYTES;
    let overlapping: Vec<&LiveMapping<'_>> = live
        .iter()
        .filter(|m| nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
        .collect();
    let chunks = system_chunks(first_va, memory_offset, pfns.len(), chunk_pages);
    if overlapping.len() != chunks.len() {
        return false;
    }
    chunks.iter().all(|chunk| {
        overlapping.iter().any(|m| {
            m.piece.va == chunk.va
                && m.piece.len == chunk.bytes()
                && m.piece.memory_offset == chunk.memory_offset
                && m.pfns.is_some_and(|shown| Some(shown) == pfns.get(chunk.first..chunk.first + chunk.pages))
        })
    })
}

/// Whether a running counter at `n` is published to the diag registry: the
/// first time and every 64th (a registry write per leaf update costs more than
/// many of the updates).
pub const fn publish_due(n: u32) -> bool {
    n == 1 || n % 64 == 0
}

/// 100-ns ticks to whole microseconds, saturating at u32::MAX.
pub fn ticks_to_us(ticks: u64) -> u32 {
    u32::try_from(ticks / 10).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_memory::ALLOCATION_PRIVATE_BYTES;

    fn record(kind: u32) -> Vec<u8> {
        let mut r = alloc::vec![0u8; ALLOCATION_PRIVATE_BYTES];
        r[GPU_ONLY_AT..GPU_ONLY_AT + 4].copy_from_slice(&kind.to_le_bytes());
        r
    }

    #[test]
    fn gpu_only_records_go_to_video_memory_by_default() {
        assert_eq!(PerfMem(PERF_MEM_DEFAULT).gpu_only_mode(), GpuOnlyMode::VramFirst);
        assert!(PerfMem(PERF_MEM_DEFAULT).cpu_visible_aperture());
        // CPU-visible, not shared, not GPU-only: aperture only.
        let p = placement_with(1, Some(2), false, false, false, GpuOnlyMode::VramFirst, true);
        assert_eq!((p.segment, p.supported, p.flags), (1, 0b01, FLAG_CPU_VISIBLE));
        // Shared keeps the old rule; GPU-only keeps VRAM.
        let p = placement_with(1, Some(2), false, true, false, GpuOnlyMode::VramFirst, true);
        assert_eq!((p.segment, p.supported, p.flags), (2, 0b11, 0));
        let p = placement_with(1, Some(2), false, false, true, GpuOnlyMode::VramFirst, true);
        assert_eq!((p.segment, p.flags), (2, 0));
        let p = placement(1, Some(2), false, false, true, PerfMem(PERF_MEM_DEFAULT).gpu_only_mode());
        assert_eq!((p.segment, p.supported, p.flags, p.gpu_only), (2, 0b11, 0, true));
    }

    #[test]
    fn gpu_only_bit() {
        assert!(is_gpu_only_record(&record(0x800)));
        assert!(is_gpu_only_record(&record(0x0220_0800)));
        assert!(!is_gpu_only_record(&record(0x0100_0000)));
        assert!(!is_gpu_only_record(&record(0)));
        assert!(!is_gpu_only_record(&[0u8; 8]));
    }

    #[test]
    fn knob_bits() {
        assert_eq!(PerfMem(0).gpu_only_mode(), GpuOnlyMode::Off);
        assert_eq!(PerfMem(1).gpu_only_mode(), GpuOnlyMode::VramFirst);
        assert_eq!(PerfMem(2).gpu_only_mode(), GpuOnlyMode::VramOnly);
        assert_eq!(PerfMem(3).gpu_only_mode(), GpuOnlyMode::VramOnly);
        let all = PerfMem(0x7f);
        assert!(all.align_large() && all.big_pages() && all.huge_pages());
        assert!(all.tlb_defer() && all.skip_unchanged());
        let none = PerfMem(0);
        assert!(!none.align_large() && !none.big_pages() && !none.huge_pages());
        assert!(!none.tlb_defer() && !none.skip_unchanged());
    }

    #[test]
    fn placement_off_is_the_old_rule() {
        for gpu_only in [false, true] {
            // Device-local, not shared: CpuVisible, memory segment + aperture.
            let p = placement(1, Some(2), false, false, gpu_only, GpuOnlyMode::Off);
            assert_eq!(p, Placement { segment: 2, supported: 0b11, flags: 1, gpu_only: false });
            // Shared: not CpuVisible.
            let p = placement(1, Some(2), false, true, gpu_only, GpuOnlyMode::Off);
            assert_eq!(p.flags, 0);
            assert_eq!(p.supported, 0b11);
            // System memory: aperture, CpuVisible | Cached | ExistingSysMem.
            let p = placement(1, Some(2), true, false, gpu_only, GpuOnlyMode::Off);
            assert_eq!(p, Placement { segment: 1, supported: 0b1, flags: 0x15, gpu_only: false });
            // No memory segment: everything in the aperture.
            let p = placement(1, None, false, false, gpu_only, GpuOnlyMode::Off);
            assert_eq!((p.segment, p.supported, p.flags), (1, 0b1, 1));
        }
    }

    #[test]
    fn placement_gpu_only() {
        let p = placement(1, Some(2), false, false, true, GpuOnlyMode::VramFirst);
        assert_eq!(p, Placement { segment: 2, supported: 0b11, flags: 0, gpu_only: true });
        let p = placement(1, Some(2), false, true, true, GpuOnlyMode::VramOnly);
        assert_eq!(p, Placement { segment: 2, supported: 0b10, flags: 0, gpu_only: true });
        // CPU-visible records keep the old answer.
        let p = placement(1, Some(2), false, false, false, GpuOnlyMode::VramOnly);
        assert_eq!(p, Placement { segment: 2, supported: 0b11, flags: 1, gpu_only: false });
        // System memory never moves, nor does anything without a memory segment.
        let p = placement(1, Some(2), true, false, true, GpuOnlyMode::VramOnly);
        assert_eq!(p.segment, 1);
        assert_eq!(p.flags, 0x15);
        let p = placement(1, None, false, false, true, GpuOnlyMode::VramOnly);
        assert_eq!((p.segment, p.supported, p.flags, p.gpu_only), (1, 0b1, 1, false));
    }

    #[test]
    fn alignment_rules() {
        assert_eq!(gpu_only_alignment(0x1000), None);
        assert_eq!(gpu_only_alignment(0x1_0000), None);
        assert_eq!(gpu_only_alignment(0x1_1000), Some(0x1_0000));
        assert_eq!(gpu_only_alignment(0x1f_f000), Some(0x1_0000));
        assert_eq!(gpu_only_alignment(0x20_0000), Some(0x20_0000));
        assert_eq!(gpu_only_alignment(0x1_0000_0000), Some(0x20_0000));
        let d3d = record(0x800);
        assert_eq!(large_page_alignment(&d3d, 0x40_0000, false, true), Some(0x20_0000));
        assert_eq!(large_page_alignment(&d3d, 0x40_0000, false, false), None);
        assert_eq!(large_page_alignment(&d3d, 0x40_0000, true, true), None);
        assert_eq!(large_page_alignment(&record(0), 0x40_0000, false, true), None);
    }

    #[test]
    fn huge_memory_rule() {
        let on = PerfMem(PerfMem::HUGE_PAGES);
        assert!(wants_huge_memory(on, true, false, 0x20_0000));
        assert!(!wants_huge_memory(on, true, false, 0x1f_f000));
        assert!(!wants_huge_memory(on, false, false, 0x40_0000));
        assert!(!wants_huge_memory(on, true, true, 0x40_0000));
        assert!(!wants_huge_memory(PerfMem(0), true, false, 0x40_0000));
        // SURFACE_ATTR (big pages) -> huge pages, attr2 6 -> 6 | 2MB.
        assert_eq!(huge_attrs(0x1900_0000, 6), (0x1980_0000, 0x0010_0006));
    }

    fn piece(va: u64, memory_offset: u64, len: u64) -> MappingPiece {
        MappingPiece { va, memory_offset, len }
    }

    #[test]
    fn page_size_choice() {
        let p = piece(0x20_0000, 0, 0x20_0000);
        assert_eq!(map_page_size(p, false, false), MAP_PAGE_4K);
        assert_eq!(map_page_size(p, true, false), MAP_PAGE_BIG);
        assert_eq!(map_page_size(p, true, true), MAP_PAGE_HUGE);
        assert_eq!(map_page_size(p, false, true), MAP_PAGE_HUGE);
        // A 64-KiB-aligned piece in huge memory still gets big pages.
        assert_eq!(map_page_size(piece(0x21_0000, 0x1_0000, 0x1f_0000), false, true), MAP_PAGE_BIG);
        // Offset or length off the big page: 4 KiB.
        assert_eq!(map_page_size(piece(0x20_0000, 0x1000, 0x20_0000), true, true), MAP_PAGE_4K);
        assert_eq!(map_page_size(piece(0x20_0000, 0, 0x1_1000), true, true), MAP_PAGE_4K);
        assert_eq!(map_page_size(piece(0x20_1000, 0, 0x1_0000), true, true), MAP_PAGE_4K);
        assert_eq!(map_page_size(piece(0, 0, 0), true, true), MAP_PAGE_4K);
        // The field: 4K flags (1 << 8) become big (2 << 8), other bits kept.
        assert_eq!(with_page_size(1 << 8, MAP_PAGE_BIG), 2 << 8);
        assert_eq!(with_page_size((1 << 8) | (1 << 15), MAP_PAGE_HUGE), (4 << 8) | (1 << 15));
    }

    #[test]
    fn tlb_defer_all_but_last() {
        assert!(!defer_tlb(false, 0, 3));
        assert!(defer_tlb(true, 0, 3));
        assert!(defer_tlb(true, 1, 3));
        assert!(!defer_tlb(true, 2, 3));
        assert!(!defer_tlb(true, 0, 1));
        assert_eq!(invalidate_tlb_params(0x4e57_1003), [0x03, 0x10, 0x57, 0x4e, 0, 0, 0, 0]);
    }

    #[test]
    fn chunking() {
        assert_eq!(system_chunk_pages(0), 128);
        assert_eq!(system_chunk_pages(512), 512);
        assert_eq!(system_chunk_pages(100_000), 4096);
        let c = system_chunks(0x10_0000, 0x2000, 300, 128);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], SystemChunk { va: 0x10_0000, memory_offset: 0x2000, first: 0, pages: 128 });
        assert_eq!(c[1].va, 0x10_0000 + 128 * 4096);
        assert_eq!(c[2], SystemChunk { va: 0x10_0000 + 256 * 4096, memory_offset: 0x2000 + 256 * 4096, first: 256, pages: 44 });
        assert!(system_chunks(0, 0, 0, 128).is_empty());
    }

    #[test]
    fn unchanged_system_update() {
        let pfns: Vec<u64> = (0x1000..0x1000 + 200).collect();
        let (a, b) = pfns.split_at(128);
        let live = [
            LiveMapping { piece: piece(0x40_0000, 0, 128 * 4096), pfns: Some(a) },
            LiveMapping { piece: piece(0x40_0000 + 128 * 4096, 128 * 4096, 72 * 4096), pfns: Some(b) },
            // Elsewhere: ignored.
            LiveMapping { piece: piece(0x80_0000, 0x10_0000, 4096), pfns: None },
        ];
        assert!(system_update_unchanged(&live, 0x40_0000, 0, &pfns, 128));
        // Another page anywhere: changed.
        let mut other = pfns.clone();
        other[150] = 7;
        assert!(!system_update_unchanged(&live, 0x40_0000, 0, &other, 128));
        // Another chunk size, offset or a video-memory mapping: changed.
        assert!(!system_update_unchanged(&live, 0x40_0000, 0, &pfns, 64));
        assert!(!system_update_unchanged(&live, 0x40_0000, 0x1000, &pfns, 128));
        let vid = [LiveMapping { piece: piece(0x40_0000, 0, 200 * 4096), pfns: None }];
        assert!(!system_update_unchanged(&vid, 0x40_0000, 0, &pfns, 256));
        // Nothing mapped yet, or an empty update: changed.
        assert!(!system_update_unchanged(&[], 0x40_0000, 0, &pfns, 128));
        assert!(!system_update_unchanged(&live, 0x40_0000, 0, &[], 128));
        // A part of the range only.
        assert!(!system_update_unchanged(&live[..1], 0x40_0000, 0, &pfns, 128));
    }

    #[test]
    fn publish_cadence() {
        assert!(publish_due(1));
        assert!(!publish_due(2));
        assert!(publish_due(64));
        assert!(publish_due(128));
        assert_eq!(ticks_to_us(25), 2);
        assert_eq!(ticks_to_us(u64::MAX), u32::MAX);
    }
}
