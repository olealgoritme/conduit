//! WDDM GpuMmu geometry for the default Venus model and the native NVDA model.
//!
//! With `NvUmd` off the original decorative 40-bit geometry is unchanged.  The
//! native 610.88 path advertises the hardware-shaped 44-bit, four-level walk
//! (12 + 9 + 9 + 9 + 5), 4-KiB tables and a 32-entry root.  Its leaf updates are
//! translated into real fixed RM mappings by `virtio::native_memory`.
//!
//! Optional capabilities stay off in both models: large pages, dual PTEs,
//! sparse zero pages, explicit invalidation and every other unimplemented cap.

use core::mem::size_of;

use crate::dxgk::_DXGK_PAGETABLEUPDATEMODE::DXGK_PAGETABLEUPDATE_GPU_PHYSICAL;
use crate::dxgk::*;

/// Total GPU virtual-address width in the default model. 40 covers VidMm's
/// current 0x8001064300 page-table init range and keeps the default root small.
pub const VIRTUAL_ADDRESS_BIT_COUNT: u32 = 40;
/// Native 610.88 geometry: 44 bits = 12 + 9 + 9 + 9 + 5.
pub const NATIVE_VIRTUAL_ADDRESS_BIT_COUNT: u32 = 44;
pub const NATIVE_ROOT_PAGE_TABLE_INDEX_BIT_COUNT: u32 = 5;
/// Bits consumed by the in-page byte offset (4-KiB pages).
pub const PAGE_SHIFT_BITS: u32 = 12;
/// Bits that index a leaf page table (512 entries ⇒ 9 bits).
pub const LEAF_PAGE_TABLE_INDEX_BIT_COUNT: u32 = 9;
/// Number of translation levels. Four levels keep every table at or below 4 KiB,
/// allowing all page-table allocations to use system memory segment id 0.
pub const PAGE_TABLE_LEVEL_COUNT: u32 = 4;
/// Intermediate non-root levels are also one 4-KiB page each.
pub const INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT: u32 = 9;
/// Root index bits are "the rest" above page offset + three 9-bit levels.
pub const ROOT_PAGE_TABLE_INDEX_BIT_COUNT: u32 = VIRTUAL_ADDRESS_BIT_COUNT
    - PAGE_SHIFT_BITS
    - LEAF_PAGE_TABLE_INDEX_BIT_COUNT
    - 2 * INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT;
/// Decorative hardware PTE size, in bytes. 512 entries × 8 = one 4-KiB page.
pub const PTE_SIZE_BYTES: u32 = 8;
/// Size VidMm allocates per leaf page table (one 4-KiB page).
pub const LEAF_PAGE_TABLE_SIZE_BYTES: u32 =
    (1u32 << LEAF_PAGE_TABLE_INDEX_BIT_COUNT) * PTE_SIZE_BYTES;
/// Every table level is one CPU page in the system-memory contract.
pub const PAGE_TABLE_SIZE_BYTES: u32 = 4096;
/// Page-table alignment (one page).
pub const PAGE_TABLE_ALIGNMENT_BYTES: u32 = 4096;
/// Segment id of the **aperture** segment VidMm carves *paging buffers* (the DMA
/// buffers that carry paging commands) from. This MUST be an aperture segment:
/// the live KD decode of `dxgmms2!VidMm...InitDmaPools` (INITDMAPOOLS_HANDOVER.md)
/// proved the paging DMA-pool validates the FIRST reported segment (`segdesc[0]`)
/// and requires its per-attribute object's "can host a paging buffer" flag
/// (`[attr+0x68] & 1`) to be set — which a CPU-visible *memory* segment (the BAR)
/// never has, but a linear **aperture** segment does (it is the classic WDDM
/// paging-buffer backing: system-memory MDLs mapped via MAP_APERTURE_SEGMENT). So
/// the aperture is reported FIRST (segment id 1) and is `PagingBufferSegmentId`.
/// Matches the proven viogpu3d virtio-gpu WDDM segment (WDDM_FAKE_VIDMM §A2.2).
pub const APERTURE_SEGMENT_ID: u32 = 1;
/// Segment id of the CPU-visible **memory** segment: the head partition of the
/// host-visible venus BAR window, used by render allocations. Segment ids are
/// positional and the aperture is always index 0, so this is index 1.
///
/// It is reported with the CPU-HOST-APERTURE shape — the only memory-segment
/// CPU-access shape this dxgkrnl accepts; the classic
/// CpuVisible/CpuTranslatedAddress form fails AddAdapter with Code 43 (proven
/// 2026-07-05). CPU access (Lock / win32k GDI raster) is REAL:
/// `DxgkDdiMapCpuHostAperture` maps each allocation's venus blob at the
/// dxgkrnl-chosen aperture offset inside the window, so CPU raster, the RenderGdi
/// executor and the host GPU all address ONE memory — the two-memory-split fix.
///
/// Page-table allocations deliberately use system memory segment id 0 instead;
/// see [`SYSTEM_MEMORY_SEGMENT_ID`]. A sibling `BAR_SEGMENT_ID = 3` used to sit
/// here for the three-segment topologies; those shapes were all documented as
/// AddAdapter-rejected and have been deleted, so id 3 no longer exists.
pub const MEMORY_SEGMENT_ID: u32 = 2;
/// WDDM reserves page-table segment id 0 for system memory. This avoids the
/// `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress` segment-attribute path that
/// bugchecks when a fake page-table device segment is dropped or lacks bit 13.
pub const SYSTEM_MEMORY_SEGMENT_ID: u32 = 0;

// Compile-time guards: the declared geometry must be internally consistent, or
// VidMm will exercise the page-table DDIs against a contradiction.
const _: () = assert!(
    PAGE_SHIFT_BITS
        + LEAF_PAGE_TABLE_INDEX_BIT_COUNT
        + 2 * INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT
        + ROOT_PAGE_TABLE_INDEX_BIT_COUNT
        == VIRTUAL_ADDRESS_BIT_COUNT
);
const _: () = assert!(PAGE_TABLE_LEVEL_COUNT == 4);
const _: () = assert!(LEAF_PAGE_TABLE_SIZE_BYTES == 4096);
const _: () = assert!(ROOT_PAGE_TABLE_INDEX_BIT_COUNT <= 9);
const _: () = assert!(
    PAGE_SHIFT_BITS
        + LEAF_PAGE_TABLE_INDEX_BIT_COUNT
        + 2 * INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT
        + NATIVE_ROOT_PAGE_TABLE_INDEX_BIT_COUNT
        == NATIVE_VIRTUAL_ADDRESS_BIT_COUNT
);
const _: () = assert!(PAGE_TABLE_SIZE_BYTES == 4096);
/// The coupling `root_page_table_size_bytes` relies on: a root table addressing
/// every index the declared bit count allows must fit in ONE page. This is the
/// invariant that used to be implied by the `<= 9` assert above and by nothing
/// else; stating it directly means a future bit-count change fails the build
/// here rather than silently making the returned size wrong.
const _: () =
    assert!((1u32 << ROOT_PAGE_TABLE_INDEX_BIT_COUNT) * PTE_SIZE_BYTES <= PAGE_TABLE_SIZE_BYTES);

/// Fill a `DXGK_GPUMMUCAPS` (`DXGKQAITYPE_GPUMMUCAPS = 13`) with the decorative
/// geometry above. All optional behaviour bits stay off (we back none of them):
/// `CacheCoherentMemorySupported`, `LargePageSupported`, `DualPteSupported`, etc.
/// are 0 until their DDI path is real (the checklist rule: unknown stays
/// unadvertised). Returns `STATUS_SUCCESS` / `STATUS_BUFFER_TOO_SMALL`.
pub unsafe fn fill_gpummu_caps(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_GPUMMUCAPS>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: pOutputData points to a writable DXGK_GPUMMUCAPS of sufficient size.
    let caps = unsafe { &mut *(args.pOutputData as *mut DXGK_GPUMMUCAPS) };
    unsafe { core::ptr::write_bytes(caps as *mut _ as *mut u8, 0, size_of::<DXGK_GPUMMUCAPS>()) };

    // No optional capability bits in the default personality. NVIDIA's personality
    // advertises ZeroInPteSupported (`zero_pte_supported`).
    caps.__bindgen_anon_1.Value = (if zero_pte_supported() { GPUMMU_ZERO_IN_PTE } else { 0 })
        | (if explicit_invalidation() { GPUMMU_EXPLICIT_INVALIDATION } else { 0 });
    // GPU_PHYSICAL: page-table updates come through BuildPagingBuffer
    // UPDATE_PAGE_TABLE and are retired by the null engine. CPU_VIRTUAL was proven
    // wrong by KD: it crashes in VidMm's CPU-visible-address resolver before our
    // paging DDI can participate.
    caps.PageTableUpdateMode = DXGK_PAGETABLEUPDATE_GPU_PHYSICAL;
    caps.VirtualAddressBitCount = if crate::virtio::nvda::enabled() {
        NATIVE_VIRTUAL_ADDRESS_BIT_COUNT
    } else {
        VIRTUAL_ADDRESS_BIT_COUNT
    };
    // We don't advertise 64-KiB pages; report the 4-KiB leaf size for consistency.
    caps.LeafPageTableSizeFor64KPagesInBytes = LEAF_PAGE_TABLE_SIZE_BYTES;
    caps.PageTableLevelCount = PAGE_TABLE_LEVEL_COUNT;
    // LegacyBehaviors stays zeroed (no SourcePageTableVaInTransfer).
    STATUS_SUCCESS
}

/// `DXGK_GPUMMUCAPS.Flags` bit 2, `ZeroInPteSupported`.
const GPUMMU_ZERO_IN_PTE: u32 = 0x4;

/// `NvZeroPte` (default 1, NVIDIA personality only): advertise
/// `ZeroInPteSupported`, as NVIDIA's KMD does (its GPUMMUCAPS are 0x2dd in the L3
/// capture). The Vulkan ICD maps the VA past a large (>= 64 MiB) allocation with
/// `Protection.Zero` and `hAllocation` NULL; without the capability dxgkrnl refuses
/// that MapGpuVirtualAddress ("Invalid hAllocation", ETW event 494) and the ICD
/// answers VK_ERROR_OUT_OF_DEVICE_MEMORY for every allocation from 64 MiB on.
/// A Zero PTE reaches the KMD as an unmapped page (`native_update_leaf`, the
/// foreign-leaf path): it faults on access unless `NvPtSparse` makes that window
/// read zero. `NvZeroPteOn` mirrors the value.
fn zero_pte_supported() -> bool {
    if !crate::virtio::nvda::enabled() {
        return false;
    }
    let on = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvZeroPte"), 1) != 0;
    crate::diag::record_named_bytes(b"NvZeroPteOn", u32::from(on));
    on
}

/// `DXGK_GPUMMUCAPS.Flags` bit 3, `ExplicitPageTableInvalidation`.
const GPUMMU_EXPLICIT_INVALIDATION: u32 = 0x8;

/// `NvPtExplicit` (default 0, NVIDIA personality only): advertise
/// `ExplicitPageTableInvalidation`, as NVIDIA's KMD does (GPUMMUCAPS 0x2dd). VidMm then
/// invalidates every entry of a page table or directory through UpdatePageTable
/// (Repeat, NotifyEviction) before it evicts or frees the table, instead of dropping
/// tables that still hold valid entries. Leaf updates are this driver's only view of a
/// process's VA space, so without the capability a mapping can outlive its page table
/// in the RM mirror. `NvPtExplicitOn` mirrors the value.
fn explicit_invalidation() -> bool {
    if !crate::virtio::nvda::enabled() {
        return false;
    }
    let on = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPtExplicit"), 0) != 0;
    crate::diag::record_named_bytes(b"NvPtExplicitOn", u32::from(on));
    on
}

/// Fill one `DXGK_PAGE_TABLE_LEVEL_DESC` (`DXGKQAITYPE_PAGETABLELEVELDESC = 14`).
/// VidMm queries this once per level (`DXGK_QUERYPAGETABLELEVELDESCIN::LevelIndex`,
/// 0 = leaf, 1 = root for the two-level scheme.
///
/// The `page_table_segment_id` parameter is gone: it had exactly ONE possible
/// value at its single call site, and taking it as an argument hid the invariant
/// that page tables live in SYSTEM MEMORY (segment 0). WDDM reserves id 0 for
/// that, which is what avoids the
/// `VIDMM_PAGE_TABLE_BASE::GetCpuVisibleAddress` segment-attribute path that
/// bugchecks when a fake page-table device segment is dropped or lacks bit 13.
pub unsafe fn fill_page_table_level_desc(args: &DXGKARG_QUERYADAPTERINFO) -> NTSTATUS {
    if (args.OutputDataSize as usize) < size_of::<DXGK_PAGE_TABLE_LEVEL_DESC>() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // Validate the requested level against the declared level count.
    if (args.InputDataSize as usize) < size_of::<DXGK_QUERYPAGETABLELEVELDESCIN>()
        || args.pInputData.is_null()
    {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: bounds-checked; the input is a DXGK_QUERYPAGETABLELEVELDESCIN.
    let level_in = unsafe { &*(args.pInputData as *const DXGK_QUERYPAGETABLELEVELDESCIN) };
    if level_in.LevelIndex as u32 >= PAGE_TABLE_LEVEL_COUNT {
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: pOutputData points to a writable DXGK_PAGE_TABLE_LEVEL_DESC.
    let desc = unsafe { &mut *(args.pOutputData as *mut DXGK_PAGE_TABLE_LEVEL_DESC) };
    unsafe {
        core::ptr::write_bytes(
            desc as *mut _ as *mut u8,
            0,
            size_of::<DXGK_PAGE_TABLE_LEVEL_DESC>(),
        );
    }
    desc.PageTableIndexBitCount = match level_in.LevelIndex as u32 {
        0 => LEAF_PAGE_TABLE_INDEX_BIT_COUNT,
        1 | 2 => INTERMEDIATE_PAGE_TABLE_INDEX_BIT_COUNT,
        _ if crate::virtio::nvda::enabled() => NATIVE_ROOT_PAGE_TABLE_INDEX_BIT_COUNT,
        _ => ROOT_PAGE_TABLE_INDEX_BIT_COUNT,
    };
    desc.PageTableSegmentId = SYSTEM_MEMORY_SEGMENT_ID;
    desc.PagingProcessPageTableSegmentId = SYSTEM_MEMORY_SEGMENT_ID;
    desc.PageTableSizeInBytes = PAGE_TABLE_SIZE_BYTES;
    desc.PageTableAlignmentInBytes = PAGE_TABLE_ALIGNMENT_BYTES;
    STATUS_SUCCESS
}

/// Root page tables larger than one page were requested — value is the count.
///
/// The declared geometry makes this unreachable: `ROOT_PAGE_TABLE_INDEX_BIT_COUNT`
/// is 1, so a root table addresses at most 2 entries, and the `<= 9` assert
/// bounds it at 512 (= one page / `PTE_SIZE_BYTES`) even if the bit count
/// changes. Counted anyway, because a geometry contradiction should be LOUD
/// rather than absorbed by a function that ignored its own argument.
pub static ROOT_PT_OVERSIZE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Byte size of a root page table that must address `num_pte` entries.
///
/// The doc has always promised a `num_pte`-derived, page-rounded size; the body
/// ignored `num_pte` entirely and returned the constant. It was CORRECT only
/// because of the `ROOT_PAGE_TABLE_INDEX_BIT_COUNT <= 9` assert above — i.e. the
/// promise was kept by an invariant declared 80 lines away rather than by the
/// arithmetic.
///
/// Now it is derived. For every legal request today the result is still exactly
/// 4096, so `GET_ROOT_PT_SIZE_LAST` records the same `(NumberOfPte, bytes)`
/// pairs.
pub fn root_page_table_size_bytes(num_pte: u32) -> SIZE_T {
    if num_pte == 0 {
        return 0;
    }
    let bytes = u64::from(num_pte).saturating_mul(u64::from(PTE_SIZE_BYTES));
    let page = u64::from(PAGE_TABLE_SIZE_BYTES);
    if bytes > page {
        ROOT_PT_OVERSIZE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    // Round up to a whole page; never zero for a non-zero request.
    (bytes.div_ceil(page).max(1).saturating_mul(page)) as SIZE_T
}
