//! `DxgkDdiBuildPagingBuffer` and the GpuMmu root-page-table DDIs.
//!
//! Helios declares a **decorative** GpuMmu (WDDM_FAKE_VIDMM_RESEARCH.md §A3.7):
//! the host GPU owns the real MMU and venus addresses resources by opaque id, so
//! the guest page-table *content* is never read by hardware. What VidMm still
//! requires is that every page-table DDI exist, succeed, and return values
//! consistent with the declared `ddi::gpummu` geometry. One part of that content
//! is nevertheless authoritative to the CPU paging executor: leaf PTEs for the
//! paging-process virtual addresses used by `VIRTUAL_TRANSFER`. So:
//!
//!   - For the aperture / page-table segments `BuildPagingBuffer` stays a
//!     **null engine**: it consumes the operation and returns success **without
//!     writing DMA / advancing `pDmaBuffer`**. The accompanying `SubmitCommand`
//!     retires the fence, so VidMm believes the operation ran.
//!   - Leaf `UPDATE_PAGE_TABLE` calls retain the exact system-memory
//!     `FirstPteVirtualAddress` → `DXGK_PTE::PageAddress` mappings supplied by
//!     VidMm for Helios blob allocations. `VIRTUAL_TRANSFER` uses those mappings
//!     to copy allocation content between the blob and the locked system pages.
//!     This is the software implementation of the paging GPU's VA walk; it does
//!     not classify resources or infer an identity.
//!   - `GetRootPageTableSize` returns a byte size consistent with the declared
//!     PTE size, so VidMm carves a correctly-sized root page table.
//!   - `SetRootPageTable` records-and-ignores (the root address is decorative).
//!
//! **BAR SEGMENT CONTENT OPS ARE REAL** (two-memory-split fix,
//! HANDOFF_GDI_EXECUTOR_2026_07_05.md ★FINAL). A BAR-segment allocation's
//! content IS its venus blob (the CPU host aperture exposes the blob bytes —
//! `cpu_host_aperture.rs`), so:
//!
//!   - Content TRANSFERs (system MDL ↔ segment) and VIRTUAL_TRANSFERs
//!     (paging-process GPU VA ↔ segment) execute synchronously here as
//!     CPU copies between the MDL and a transient kernel map of the blob,
//!     BEFORE the paging fence retires — VidMm's content model stays truthful
//!     across eviction/re-commit. FILL / VIRTUAL_FILL pattern-fill the blob.
//!   - A segment→segment move needs NO copy (content is intrinsic to the host
//!     memory object; the decorative SegmentAddress values are ignored).
//!   - A leaf UPDATE_PAGE_TABLE naming the BAR segment is harvested (atomic store
//!     only) as a placement diagnostic.
//!
//! IRQL: the docs state `DxgkDdiBuildPagingBuffer` runs at PASSIVE_LEVEL; the
//! BAR content work (host round-trips, Mm mapping, registry counters) is
//! additionally gated on a runtime `KeGetCurrentIrql() == PASSIVE_LEVEL` check
//! and counted loudly (`PgEi`) if that ever fails — never a silent skip, never
//! a DISPATCH-illegal call. `SetRootPageTable` can run at DISPATCH_LEVEL and
//! keeps its atomics-only tracing.

use alloc::vec::Vec;

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use wdk_sys::ntddk::{
    KeGetCurrentIrql, MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace,
};
use wdk_sys::{_MEMORY_CACHING_TYPE, PHYSICAL_ADDRESS, PMDL};

use crate::adapter::{AdapterContext, SystemBackingGuard, MAX_SYSTEM_BACKING_RANGES};
use crate::ddi::create_allocation::SystemBackingPolicy;
use crate::ddi::create_allocation::{paging_alloc_info, set_bar_placement};
use crate::dxgk::*;
use crate::virtio::rm_client::sysmem_flip::primary_changed;
use helios_kmd_logic::device_lost as dl;
use helios_kmd_logic::paging::{self as pg, Clamp};
use helios_kmd_logic::rm_refresh::Edge;

/// DISPATCH-safe paging tracers (ntoseye reads these by symbol — no IRQL
/// violation, unlike the `diag::record` ring).
pub static PAGING_LAST_OP: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
pub static PAGING_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
/// Bitmask of every `DXGK_BUILDPAGINGBUFFER_OPERATION` value seen (bit = 1<<op).
/// Lets the bring-up session see *which* paging ops VidMm drives once GpuMmu is
/// declared (e.g. UPDATE_PAGE_TABLE=11 → bit 11) without flooding the ring.
pub static PAGING_OP_SEEN_MASK: AtomicU32 = AtomicU32::new(0);
/// `DxgkDdiSetRootPageTable` call count + the last context/NumEntries seen.
pub static SET_ROOT_PT_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SET_ROOT_PT_LAST: AtomicU64 = AtomicU64::new(0);
/// `DxgkDdiGetRootPageTableSize` call count + last (NumberOfPte<<32 | bytes).
pub static GET_ROOT_PT_SIZE_COUNT: AtomicU32 = AtomicU32::new(0);
pub static GET_ROOT_PT_SIZE_LAST: AtomicU64 = AtomicU64::new(0);

/// Mirror the DISPATCH-safe GpuMmu page-table tracers into the PASSIVE diag ring.
/// Call ONLY from a PASSIVE DDI (e.g. `DxgkDdiDestroyDevice`) — `diag::record`
/// is `RtlWriteRegistryValue` (PASSIVE-only). This lets the registry ring (read
/// over SSH, no ntoseye) show how far into the GpuMmu page-table setup VidMm got
/// before a post-CreateContext Code-43 teardown:
///   0x0F01_MMMM = PAGING_OP_SEEN_MASK (bit11=UPDATE_PAGE_TABLE, bit5=MAP_APERTURE…)
///   0x0F02_NNNN = BuildPagingBuffer call count
///   0x0F03_NNNN = SetRootPageTable call count
///   0x0F04_NNNN = GetRootPageTableSize call count
///   0x0F05_OOOO = last paging Operation
/// Mirror the GpuMmu page-table-DDI tracers into the PASSIVE breadcrumb ring.
///
/// Takes the PASSIVE proof token: every `diag::record` here is a synchronous
/// `RtlWriteRegistryValue`, and the obligation used to be a doc comment reading
/// "Call ONLY from a PASSIVE DDI".
pub fn diag_dump_gpummu_atomics(_passive: PassiveLevel) {
    let mask = PAGING_OP_SEEN_MASK.load(Ordering::Relaxed) & 0xFFFF;
    crate::diag::record(0x0F01_0000 | mask);
    crate::diag::record(0x0F02_0000 | (PAGING_CALL_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F03_0000 | (SET_ROOT_PT_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F04_0000 | (GET_ROOT_PT_SIZE_COUNT.load(Ordering::Relaxed) & 0xFFFF));
    crate::diag::record(0x0F05_0000 | (PAGING_LAST_OP.load(Ordering::Relaxed) & 0xFFFF));
}

// ── BAR-segment paging engine ───────────────────────────────────────────────
//
// Content ops for BAR-segment allocations are PLACEMENT-INDEPENDENT: an
// allocation's content is its venus BLOB (the CPU host aperture exposes blob
// bytes wherever dxgkrnl asked them mapped — `cpu_host_aperture.rs`), so a
// paging TRANSFER/FILL reads/writes the blob through a transient kernel map of
// its CURRENT window mapping, and a segment→segment "move" needs no copy at
// all. The decorative SegmentAddress values are ignored.

/// `PASSIVE_LEVEL` (KIRQL 0) — the only IRQL at which the BAR content ops
/// (host round-trips, Mm mapping calls) may run.
use crate::ddi::PASSIVE_LEVEL_IRQL;
use crate::irql::PassiveLevel;

/// `DISPATCH_LEVEL` (KIRQL 2): the highest IRQL at which a spinlock may be taken
/// with `KeAcquireSpinLockRaiseToDpc`.
const DISPATCH_LEVEL_IRQL: u8 = 2;

/// What one content-op executor did, as a value the dispatch must consume.
///
/// The executors used to return `()`: every failure inside them — an
/// unresolvable handle, an MDL map failure, a blob map failure, an out-of-blob
/// range — was discarded and `DxgkDdiBuildPagingBuffer` answered
/// STATUS_SUCCESS. VidMm then retired the paging fence believing the content
/// had moved, so a page-in left stale bytes in the BAR blob and an eviction
/// lost the only copy of the allocation. Making the contribution part of the
/// return type means a new executor cannot silently skip it.
enum PagingOpOutcome {
    /// The content operation ran to completion.
    Executed,
    /// The operation does not belong to this driver's content engine (another
    /// segment, or a device-local allocation whose bytes are host-owned).
    /// Reported as success, exactly as before — nothing was supposed to happen.
    NotOurs,
    /// The operation was ours and did not happen. Must reach the DDI status.
    Failed(NTSTATUS),
}

/// The status a content operation that did not happen is answered with.
///
/// It is `STATUS_SUCCESS`, for every arm, and it must stay that way: VidMm accepts
/// only `STATUS_SUCCESS` and `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` from this
/// DDI (the latter only means "retry with a bigger DMA buffer", which a driver
/// that emits no DMA cannot honestly ask for). Anything else is "Driver returned
/// an invalid error code from BuildPagingBuffer" and bugchecks 0x10E
/// (VIDEO_MEMORY_MANAGEMENT_INTERNAL, parameter 1 = 0xB). That was measured with
/// STATUS_INSUFFICIENT_RESOURCES (0xC000009A) on a VIRTUAL_TRANSFER eviction after
/// an in-place driver update; 0ab908b had already hit the same bugcheck for the
/// classic TRANSFER with no transport and only guarded that one arm.
///
/// So a failed content op moves nothing, is COUNTED (`PgSkipV`, plus the
/// per-reason `Pg*` counter at the failing site) and answers success. Content is
/// intrinsic to the venus blob, which a skipped LOCAL_TO_SYSTEM leaves intact; a
/// skipped SYSTEM_TO_LOCAL leaves the blob at its last content, so only writes
/// made while the allocation was system-resident are lost. That is the price of
/// never taking the machine down, and `PgSkipV` makes it visible.
///
/// The blob is only left intact UNTIL THE NEXT PAGE-IN: VidMm believes a skipped
/// eviction copied the content to system memory, so it will later page those
/// (garbage) pages back over the good blob. A skipped eviction therefore records
/// its allocation as "system copy invalid" (`SystemBackingTable`), and a page-in
/// of such an allocation is itself skipped (`PgInvSk`) until the successful
/// eviction chunks since the mark cover the whole allocation (`PgInvClr`; one
/// whole-allocation eviction or several chunks, see
/// `helios_kmd_logic::paging::InvalidSet::evict_chunk_done`), the content is
/// discarded, or the allocation is destroyed.
/// Transient causes (a mapping returning NULL, an allocation failing) are retried
/// first: [`helios_kmd_logic::paging::retry_after_failure`].
const fn paging_failure() -> NTSTATUS {
    STATUS_SUCCESS
}

/// The rule above is a compile-time fact, checked by the host-tested predicate.
const _: () = assert!(helios_kmd_logic::paging::is_legal_status(paging_failure()));

// Counters (registry-visible after any BAR-segment op; atomics are the source
// of truth and stay readable by symbol even if the registry write is skipped).
static BAR_XFER_IN: AtomicU32 = AtomicU32::new(0); // system MDL → blob copies
static BAR_XFER_OUT: AtomicU32 = AtomicU32::new(0); // blob → system MDL copies
static BAR_XFER_MOVE: AtomicU32 = AtomicU32::new(0); // segment→segment (no-op)
static BAR_FILLS: AtomicU32 = AtomicU32::new(0);
static BAR_DISCARDS: AtomicU32 = AtomicU32::new(0);
static BAR_PT_HARVESTS: AtomicU32 = AtomicU32::new(0); // placements seen in leaf PTEs
static BAR_LAST_RESID: AtomicU32 = AtomicU32::new(0);
/// Host cache mode used for the most recent transient blob mapping.
///
/// This is the `VIRTIO_GPU_MAP_CACHE_*` nibble returned by
/// `RESOURCE_MAP_BLOB`; keeping it visible is important because mapping the
/// same host-visible pages with a different Windows cache type creates an
/// invalid cache-attribute alias.
static BAR_LAST_MAP_CACHE: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_XFER_FLAGS: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_XFER_OFF: AtomicU32 = AtomicU32::new(0);
static BAR_LAST_MDL_OFF: AtomicU32 = AtomicU32::new(0);
// Loud failure counters — any nonzero value after boot is a design gap to chase.
static BAR_ERR_IRQL: AtomicU32 = AtomicU32::new(0); // content op arrived > PASSIVE
static BAR_ERR_MAP: AtomicU32 = AtomicU32::new(0); // blob map / kernel map failed
static BAR_ERR_TX_GONE: AtomicU32 = AtomicU32::new(0); // paging transfer with no transport (restart/loss)
static BAR_ERR_BOUNDS: AtomicU32 = AtomicU32::new(0); // op range outside the blob
static BAR_ERR_DISCONTIG: AtomicU32 = AtomicU32::new(0); // leaf PTEs not contiguous
static BAR_ERR_VIRTUAL: AtomicU32 = AtomicU32::new(0); // unresolved paging-process VA
static BAR_ERR_MDL: AtomicU32 = AtomicU32::new(0); // system-MDL kernel map failed
static BAR_ERR_SHADOW_FULL: AtomicU32 = AtomicU32::new(0); // PTE shadow capacity exhausted
/// Content ops that did NOT move their data and were answered STATUS_SUCCESS
/// anyway (`PgSkipV`). Any nonzero value is content VidMm believes moved and
/// did not; the per-reason counter beside it says why.
static BAR_SKIPPED: AtomicU32 = AtomicU32::new(0);
/// Transfers/fills whose range ran past the recorded allocation size and were
/// cut to it (`PgClamp`): the bytes past the allocation are padding.
static BAR_CLAMPED: AtomicU32 = AtomicU32::new(0);
/// A classic TRANSFER (`PgEh`) / FILL (`PgFh`) named an `hAllocation` that does
/// not resolve to a live Helios allocation. Both were bare `return`s: the op did
/// not run, nothing was counted, and the DDI still answered STATUS_SUCCESS, so
/// VidMm retired the paging fence believing content had moved.
static BAR_ERR_XFER_HANDLE: AtomicU32 = AtomicU32::new(0);
static BAR_ERR_FILL_HANDLE: AtomicU32 = AtomicU32::new(0);
/// `VIRTUAL_FILL`s (`PgFv`) that arrived while the allocation was system-
/// resident — evidence only, no behaviour change.
///
/// The VIRTUAL_FILL arm fills the blob at `AllocationOffsetInBytes` and never
/// resolves `DestinationVirtualAddress` through the PTE shadow, the way
/// `bar_virtual_transfer` does for the same class of address. While the
/// allocation is paged out to system memory, the bytes VidMm means are the
/// system pages, not the blob. Whether that is reachable at all is an open
/// question this counter answers before anything is built for it: a nonzero
/// value is the trigger for a VA-resolving implementation (k-paging-14).
static BAR_VIRTUAL_FILL_SYSTEM: AtomicU32 = AtomicU32::new(0);
static BAR_VIRTUAL_PTES: AtomicU32 = AtomicU32::new(0); // system PTEs retained
static BAR_LAST_VIRTUAL_SRC: AtomicU64 = AtomicU64::new(0);
static BAR_LAST_VIRTUAL_DST: AtomicU64 = AtomicU64::new(0);
/// Paging content op named a device-local/opaque allocation. Such resources
/// have no CPU byte mapping; attempting RESOURCE_MAP_BLOB is a contract error.
static BAR_DEVICE_OP_SKIPS: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_CAPTURES: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_MIRRORS: AtomicU32 = AtomicU32::new(0);
static BAR_SYSTEM_BACKING_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Transient failures retried at PASSIVE (`PgRetry`): an MDL or blob mapping that
/// returned NULL, a reservation that failed. One per extra attempt.
static BAR_RETRIES: AtomicU32 = AtomicU32::new(0);
/// Allocations marked "system copy invalid" because their LOCAL_TO_SYSTEM
/// eviction was skipped (`PgInv`). Each is content VidMm believes it saved and
/// did not; the blob stays authoritative and the matching page-in is skipped.
static BAR_INVALID_MARKED: AtomicU32 = AtomicU32::new(0);
/// The invalid-copy set was full and went to overflow (`PgInvOvf`): until the
/// next transport generation EVERY page-in is skipped. Must stay 0.
static BAR_INVALID_OVERFLOW: AtomicU32 = AtomicU32::new(0);
/// "System copy invalid" marks cleared by successful evictions that, together,
/// covered the whole allocation (`PgInvClr`). `PgInv - PgInvClr` that keeps
/// growing is allocations that stay marked (their page-ins keep being skipped).
static BAR_INVALID_CLEARED: AtomicU32 = AtomicU32::new(0);
/// SYSTEM_TO_LOCAL page-ins skipped because the allocation's system copy is
/// invalid (`PgInvSk`).
static BAR_INVALID_SKIPS: AtomicU32 = AtomicU32::new(0);
/// VIRTUAL_TRANSFERs carrying nonzero `Flags` (`PgV64`): the 64-KiB-page forms.
/// The driver never declares 64-KiB pages (`gpummu::fill_gpummu_caps` leaves the
/// caps bits off and reports only the 4-KiB leaf size), so VidMm should never set
/// them; the PTE shadow models 4-KiB leaves only, so such a transfer is skipped.
/// Must stay 0 — a nonzero value means the driver's page-size contract changed.
static BAR_VIRTUAL_FLAGS: AtomicU32 = AtomicU32::new(0);

/// The BAR paging counter block, mirrored into the registry through the shared
/// throttled emitter (R317). Named values and encodings are unchanged; only the
/// cadence is — this ran at the tail of EVERY content op, i.e. 26 synchronous
/// registry writes per paging operation, per allocation, under eviction
/// pressure. Failure counters still surface on the op that produced them, via
/// `CounterBlock`'s flush-on-failure-change rule.
static PAGING_FLUSH_TICKS: AtomicU32 = AtomicU32::new(0);
static PAGING_FLUSH_FAILURES: AtomicU32 = AtomicU32::new(0);

static PAGING_COUNTERS: crate::diag::CounterBlock = crate::diag::CounterBlock {
    entries: &[
        e(b"PgTi", &BAR_XFER_IN),
        e(b"PgTo", &BAR_XFER_OUT),
        e(b"PgTm", &BAR_XFER_MOVE),
        e(b"PgFn", &BAR_FILLS),
        e(b"PgDn", &BAR_DISCARDS),
        e(b"PgUn", &BAR_PT_HARVESTS),
        e(b"PgMr", &BAR_LAST_RESID),
        e(b"PgMc", &BAR_LAST_MAP_CACHE),
        e(b"PgSf", &BAR_LAST_XFER_FLAGS),
        e(b"PgTs", &BAR_LAST_XFER_OFF),
        e(b"PgTd", &BAR_LAST_MDL_OFF),
        f(b"PgEi", &BAR_ERR_IRQL),
        f(b"PgEm", &BAR_ERR_MAP),
        f(b"PgTxG", &BAR_ERR_TX_GONE),
        f(b"PgEb", &BAR_ERR_BOUNDS),
        f(b"PgEc", &BAR_ERR_DISCONTIG),
        f(b"PgEv", &BAR_ERR_VIRTUAL),
        f(b"PgEx", &BAR_ERR_MDL),
        f(b"PgEf", &BAR_ERR_SHADOW_FULL),
        // A VALUE entry, not a failure: every skip site already bumps its own failure
        // counter (PgEm/PgEb/PgEv/PgSe/...) which forces the flush, and this one used
        // to force the whole ~35-value registry write on EVERY skipped op — in a
        // storm, inside BuildPagingBuffer.
        e(b"PgSkipV", &BAR_SKIPPED),
        e(b"PgRetry", &BAR_RETRIES),
        f(b"PgInv", &BAR_INVALID_MARKED),
        f(b"PgInvOvf", &BAR_INVALID_OVERFLOW),
        e(b"PgInvSk", &BAR_INVALID_SKIPS),
        e(b"PgInvClr", &BAR_INVALID_CLEARED),
        f(b"PgV64", &BAR_VIRTUAL_FLAGS),
        e(
            b"PgStale",
            &crate::ddi::create_allocation::STALE_ALLOC_REFUSED,
        ),
        e(b"PgClamp", &BAR_CLAMPED),
        e(b"PgVp", &BAR_VIRTUAL_PTES),
        e64(b"PgVs", &BAR_LAST_VIRTUAL_SRC),
        e64(b"PgVd", &BAR_LAST_VIRTUAL_DST),
        e(b"PgDi", &BAR_DEVICE_OP_SKIPS),
        e(b"PgSc", &BAR_SYSTEM_BACKING_CAPTURES),
        e(b"PgSm", &BAR_SYSTEM_BACKING_MIRRORS),
        f(b"PgSe", &BAR_SYSTEM_BACKING_ERRORS),
        f(b"PgEh", &BAR_ERR_XFER_HANDLE),
        f(b"PgFh", &BAR_ERR_FILL_HANDLE),
        e(b"PgFv", &BAR_VIRTUAL_FILL_SYSTEM),
    ],
    ticks: &PAGING_FLUSH_TICKS,
    failures: &PAGING_FLUSH_FAILURES,
    policy: crate::diag::FlushPolicy::EveryNth(64),
};

/// Value entry.
const fn e(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: false,
    }
}
/// Failure entry — its change forces an immediate flush.
const fn f(name: &'static [u8], value: &'static AtomicU32) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U32(value),
        failure: true,
    }
}
/// Value entry reported as the low 32 bits of a u64, as before.
const fn e64(name: &'static [u8], value: &'static AtomicU64) -> crate::diag::CounterEntry {
    crate::diag::CounterEntry {
        name,
        value: crate::diag::CounterRef::U64Low(value),
        failure: false,
    }
}

/// Mirror the paging counter block into the registry.
///
/// Takes the PASSIVE proof token by value (a ZST, so it costs nothing): this is
/// ~26 synchronous `RtlWriteRegistryValue` calls, and `k-paging-05` got in
/// precisely because a call to this sat on the DISPATCH-safe UPDATE_PAGE_TABLE
/// branch, twelve lines above a content path that installs a runtime IRQL gate
/// because the documented PASSIVE contract is not trusted. The token makes that
/// call site a COMPILE error rather than a review miss.
fn dump_bar_counters(_passive: PassiveLevel) {
    PAGING_COUNTERS.flush();
}

// ── Paging-process leaf-PTE shadow ──────────────────────────────────────────

/// Maximum concurrently-live system-memory PTEs retained per adapter.
///
/// VidMm maps a bounded paging-process scratch range around each virtual content
/// operation and unmaps it immediately afterward. 65,536 4-KiB pages covers
/// 256 MiB of simultaneous transfers. Exhaustion is counted (`PgEf`, `PgSkipV`)
/// and answered STATUS_SUCCESS — VidMm bugchecks on any other status — so the
/// ranges that were not retained are skipped when a transfer cannot resolve them.
const MAX_PAGING_SYSTEM_PTES: usize = 65_536;

#[derive(Clone, Copy)]
struct PagingSystemPte {
    /// Paging-process GPU virtual page number.
    gpu_page: u64,
    /// System-memory physical page number from `DXGK_PTE::PageAddress`.
    physical_page: u64,
}

/// Insert one entry into a `gpu_page`-sorted table, preserving the order.
///
/// Returns `false` when the table is full — the caller must then FAIL the paging
/// operation rather than retire an incomplete mapping.
///
/// Allocation-free: `FixedVec::push` cannot grow, and the tail shift is a
/// `copy_within` inside the already-reserved buffer. Never `Vec::insert` per
/// element into a growing vector — that is O(k*n) and can allocate under the
/// spinlock.
fn insert_sorted(
    entries: &mut crate::sync::FixedVec<PagingSystemPte>,
    entry: PagingSystemPte,
) -> bool {
    if entries.is_full() {
        return false;
    }
    let at = {
        let slice = entries.as_slice();
        // Same arithmetic as `helios_kmd_logic::sorted_splice_range`'s lower
        // bound, which is host-tested against a retain-then-sort oracle.
        slice.partition_point(|e| e.gpu_page < entry.gpu_page)
    };
    // Grow by one at the end (cannot allocate — capacity was reserved once at
    // construction), then rotate the new slot into place.
    if !entries.push(entry) {
        return false;
    }
    let slice = entries.as_mut_slice();
    slice[at..].rotate_right(1);
    true
}

/// Exact system-memory leaf mappings supplied by VidMm in
/// `DXGK_OPERATION_UPDATE_PAGE_TABLE`.
///
/// The table retains no resource classification. `update_leaf` first removes
/// every old entry in the Windows-supplied VA range, then retains an entry only
/// when the update names a live Helios blob allocation and its exact PTE is
/// valid, non-zero, and in segment 0 (system memory).
pub(crate) struct PagingPteShadow {
    /// SORTED BY `gpu_page`, by construction. `resolve`'s `binary_search_by_key`
    /// depends on that, and it used to be kept true by one `sort_unstable_by_key`
    /// call at the end of `update_leaf` — so a future edit that dropped or moved
    /// the sort broke eviction content silently. The only mutator is
    /// [`Self::update_leaf`], which splices an ascending block into the exact
    /// range it just cleared.
    entries: crate::sync::SpinLock<crate::sync::FixedVec<PagingSystemPte>>,
}

impl PagingPteShadow {
    /// Reserve once at adapter construction (PASSIVE_LEVEL); no update allocates
    /// while the spinlock is held.
    pub(crate) fn new() -> Self {
        Self {
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(
                MAX_PAGING_SYSTEM_PTES,
            )),
        }
    }

    /// Apply one authoritative leaf-page-table update.
    ///
    /// Returns `false` if retaining all supplied system PTEs would exceed the
    /// fixed non-paged table. The caller must fail the paging operation instead
    /// of retiring an incomplete mapping.
    unsafe fn update_leaf(
        &self,
        update: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE,
        track_system_pages: bool,
    ) -> bool {
        if update.PageTableLevel != 0
            || update.pPageTableEntries.is_null()
            || update.NumPageTableEntries == 0
        {
            return true;
        }

        let first_page = update.FirstPteVirtualAddress >> 12;
        let page_count = update.NumPageTableEntries as u64;
        let end_page = first_page.saturating_add(page_count);
        let mut ok = true;
        {
            let mut entries = self.entries.lock();

            // The new update replaces this exact Windows-supplied VA range even
            // if it maps a non-Helios allocation, a device segment, or invalid
            // PTEs.
            entries.retain(|entry| entry.gpu_page < first_page || entry.gpu_page >= end_page);

            if track_system_pages {
                let repeat = update.Flags.Repeat() != 0;
                for i in 0..update.NumPageTableEntries as usize {
                    // Repeat means pPageTableEntries names one value replicated
                    // across the whole update; otherwise it names Num entries.
                    let pte_index = if repeat { 0 } else { i };
                    // SAFETY: index follows the DXGK_UPDATEPAGETABLEFLAGS contract.
                    let pte = unsafe {
                        core::ptr::read_unaligned(update.pPageTableEntries.add(pte_index))
                    };
                    let bits = unsafe { pte.__bindgen_anon_1.__bindgen_anon_1 };
                    let valid = bits.Valid() != 0;
                    let zero = bits.Zero() != 0;
                    let segment = bits.Segment() as u32;
                    if !valid || zero || segment != 0 {
                        continue;
                    }
                    // SPLICE, not append-then-sort. `retain` above removed
                    // exactly `[first_page, end_page)`, every entry below lands
                    // in that range in ascending order, and the table was sorted
                    // before the update — so inserting at the partition point
                    // reproduces the sorted result WITHOUT the O(n log n)
                    // `sort_unstable_by_key` this used to run over up to 65,536
                    // entries with the spinlock held, at DISPATCH_LEVEL.
                    let entry = PagingSystemPte {
                        gpu_page: first_page + i as u64,
                        physical_page: unsafe { pte.__bindgen_anon_2.PageAddress },
                    };
                    if !insert_sorted(&mut entries, entry) {
                        ok = false;
                        break;
                    }
                }
            }
            BAR_VIRTUAL_PTES.store(entries.len() as u32, Ordering::Relaxed);
        }

        if !ok {
            BAR_ERR_SHADOW_FULL.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Resolve a paging-process GPU-VA byte range to the exact ordered physical
    /// pages currently supplied by VidMm.
    fn resolve(&self, passive: PassiveLevel, virtual_address: u64, size: u64) -> Option<Vec<u64>> {
        if size == 0 {
            return Some(Vec::new());
        }
        let first_page = virtual_address >> 12;
        let last_byte = (virtual_address & 0xFFF).checked_add(size - 1)?;
        let page_count = (last_byte >> 12).checked_add(1)?;
        let page_count_usize = usize::try_from(page_count).ok()?;

        let mut pages = Vec::new();
        // A failed reservation is a transient pool shortage, not an unresolvable
        // range: retried at PASSIVE before the transfer is given up.
        if !reserve_retry(passive, &mut pages, page_count_usize) {
            return None;
        }

        let mut resolved = true;
        {
            let entries = self.entries.lock();
            let slice = entries.as_slice();
            for i in 0..page_count {
                let gpu_page = first_page + i;
                match slice.binary_search_by_key(&gpu_page, |entry| entry.gpu_page) {
                    Ok(index) => pages.push(slice[index].physical_page),
                    Err(_) => {
                        resolved = false;
                        break;
                    }
                }
            }
        }
        resolved.then_some(pages)
    }
}

/// `try_reserve_exact` that retries a transient shortage at PASSIVE, the same
/// bounded policy the mappings get ([`pg::retry_after_failure`]). The sleep is
/// legal because every caller runs inside the PASSIVE-gated content path and holds
/// only the sleeping content mutex.
fn reserve_retry<T>(passive: PassiveLevel, v: &mut Vec<T>, additional: usize) -> bool {
    let mut failed = 0u32;
    loop {
        if v.try_reserve_exact(additional).is_ok() {
            return true;
        }
        failed += 1;
        if backoff(passive, failed, false).is_none() {
            return false;
        }
    }
}

/// Sleep before the next attempt after `failed` consecutive failures, or `None`
/// when the attempts are spent. PASSIVE (the token), no spinlock held.
fn backoff(passive: PassiveLevel, failed: u32, timed_out: bool) -> Option<()> {
    let ms = pg::retry_after_failure(failed, timed_out)?;
    BAR_RETRIES.fetch_add(1, Ordering::Relaxed);
    crate::virtio::ctrl::sleep_ms(passive, ms);
    Some(())
}

/// A LOCAL_TO_SYSTEM eviction of the live, reachable allocation `resource_id` did
/// not (fully) happen although the DDI will answer STATUS_SUCCESS: VidMm now
/// believes the system pages hold the content, and they do not. Remember it, so
/// the matching page-in is skipped instead of overwriting the good blob with them.
///
/// Needs no content guard (own spinlock): the content mutex failing is itself a
/// reason to call it.
fn note_skipped_eviction(adapter: &AdapterContext, resource_id: u32) {
    match adapter
        .system_backings
        .mark_system_copy_invalid(resource_id)
    {
        pg::Mark::Newly => {
            BAR_INVALID_MARKED.fetch_add(1, Ordering::Relaxed);
        }
        pg::Mark::Overflow => {
            BAR_INVALID_MARKED.fetch_add(1, Ordering::Relaxed);
            BAR_INVALID_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        }
        pg::Mark::Already | pg::Mark::Ignored => {}
    }
}

/// The outcome of a classic eviction that was skipped for a live allocation.
fn eviction_skipped(adapter: &AdapterContext, resource_id: u32) -> PagingOpOutcome {
    note_skipped_eviction(adapter, resource_id);
    PagingOpOutcome::Failed(paging_failure())
}

/// A LOCAL_TO_SYSTEM eviction chunk just succeeded; `moved` is the count ACTUALLY
/// copied (not the requested size). Once the chunks that succeeded since the mark
/// cover the whole allocation, the system copy is real again and the mark goes.
fn note_eviction_done(
    content_guard: &SystemBackingGuard<'_>,
    alloc_size: u64,
    offset: u64,
    moved: u64,
    resource_id: u32,
) {
    if content_guard.evict_chunk_done(resource_id, alloc_size, offset, moved)
        == pg::Chunk::Revalidated
    {
        BAR_INVALID_CLEARED.fetch_add(1, Ordering::Relaxed);
    }
}

/// `NormalPagePriority | MdlMappingNoExecute` for the system-MDL kernel map.
const MDL_MAP_PRIORITY: u32 = 16 | 0x4000_0000;
/// `MDL_MAPPED_TO_SYSTEM_VA | MDL_SOURCE_IS_NONPAGED_POOL` — MappedSystemVa valid.
const MDL_HAS_SYSTEM_VA: i16 = 0x0001 | 0x0004;
/// `KernelMode` (`KPROCESSOR_MODE`).
const KERNEL_MODE: i8 = 0;

/// A paging op's system-memory MDL mapping, carrying the length the raw pointer
/// does not.
///
/// The blob side of every copy has always been bounds-checked; the MDL side was
/// not. `mdl_off` and `TransferSize` are VidMm-supplied and were applied raw,
/// with a comment ("validated post-boot via PgTs/PgTd") standing in for the
/// check — on the eviction arm that is a kernel-memory WRITE past the mapped
/// buffer. `_MDL.ByteCount` is exactly the length of the described buffer and is
/// one field read away, so the bound is now carried by construction
/// (k-paging-03).
#[derive(Clone, Copy)]
struct MdlWindow {
    va: core::ptr::NonNull<u8>,
    len: u64,
    byte_offset: u32,
}

impl MdlWindow {
    /// Pointer to `bytes` mapped bytes at `offset`, or `None` (counted by the
    /// caller as PgEb) if any part of that range leaves the mapping.
    fn slice_at(&self, offset: u64, bytes: u64) -> Option<*mut u8> {
        let offset = helios_kmd_logic::window_range(self.len, offset, bytes)?;
        // SAFETY: offset + bytes <= len was just proven, so the result stays
        // inside the buffer `va` describes.
        Some(unsafe { self.va.as_ptr().add(offset as usize) })
    }

    /// Resolve DXGK's PFN-array `MdlOffset` against a VA that begins at the
    /// MDL's described byte, not at the first page boundary.
    fn slice_at_page(&self, mdl_page: u32, bytes: u64) -> Option<*mut u8> {
        let offset = if mdl_page == 0 {
            0
        } else {
            u64::from(mdl_page)
                .checked_mul(4096)?
                .checked_sub(u64::from(self.byte_offset))?
        };
        self.slice_at(offset, bytes)
    }
}

/// Kernel VA + length of a paging op's system-memory MDL (VidMm passes locked
/// MDLs). Reuses an existing system mapping if the MDL has one; otherwise maps
/// KernelMode/cached (released when VidMm frees the MDL, the
/// `MmGetSystemAddressForMdlSafe` pattern). Returns `None` (counted) on failure.
///
/// `ByteOffset` is deliberately NOT added to the returned pointer: both branches
/// already point at the start of the described buffer, whose length is exactly
/// `ByteCount`, so adding it would itself introduce an off-by-`ByteOffset`
/// overrun.
///
/// # Safety
/// `mdl` must be a valid, locked MDL for the duration of the paging op.
unsafe fn mdl_system_va(passive: PassiveLevel, mdl: PMDL) -> Option<MdlWindow> {
    if mdl.is_null() {
        return None;
    }
    // SAFETY: valid MDL per the fn contract.
    unsafe {
        let len = u64::from((*mdl).ByteCount);
        let byte_offset = (*mdl).ByteOffset;
        if (*mdl).MdlFlags & MDL_HAS_SYSTEM_VA != 0 {
            return core::ptr::NonNull::new((*mdl).MappedSystemVa as *mut u8).map(|va| MdlWindow {
                va,
                len,
                byte_offset,
            });
        }
        // A NULL return is low system PTEs, which clears by itself: a few attempts
        // at PASSIVE before the op is given up on (and answered success + counted).
        let mut failed = 0u32;
        loop {
            let va = MmMapLockedPagesSpecifyCache(
                mdl,
                KERNEL_MODE,
                _MEMORY_CACHING_TYPE::MmCached,
                core::ptr::null_mut(),
                0, // BugCheckOnFailure = FALSE → NULL return for KernelMode failure
                MDL_MAP_PRIORITY,
            );
            if let Some(va) = core::ptr::NonNull::new(va as *mut u8) {
                return Some(MdlWindow {
                    va,
                    len,
                    byte_offset,
                });
            }
            BAR_ERR_MDL.fetch_add(1, Ordering::Relaxed);
            failed += 1;
            backoff(passive, failed, false)?;
        }
    }
}

/// Run `f` over a transient kernel mapping of the allocation's blob bytes
/// (mapped into the window first if it is not currently mapped — content is
/// identical at any window offset). PASSIVE_LEVEL. Returns `false` (counted)
/// if the blob could not be resolved/mapped.
unsafe fn with_blob_bytes(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    f: impl FnOnce(*mut u8, u64),
) -> bool {
    BAR_LAST_RESID.store(resource_id, Ordering::Relaxed);
    // Both halves are retried together at PASSIVE (the mapping of the blob into
    // the window, then the kernel map of that range): either can fail for a
    // reason that clears in milliseconds. A host TIMEOUT gets one retry only —
    // each attempt can wait for seconds, and that stall happens under the content
    // mutex. The map is idempotent, so repeating it is safe.
    let mut failed = 0u32;
    let (prep, va) = loop {
        let timed_out = match crate::virtio::ctrl::map_blob_prepare(
            passive,
            adapter,
            crate::virtio::gpu::OwnerFilter::Any,
            resource_id,
        ) {
            Ok(prep) => {
                BAR_LAST_MAP_CACHE.store(prep.map_cache, Ordering::Relaxed);
                let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
                pa.QuadPart = prep.gpa as i64;
                // SAFETY: PASSIVE_LEVEL; the range was RESOURCE_MAP_BLOB'd into the
                // host-visible window, so the pages are backed. The cache attribute
                // MUST match the host's MAP_INFO response: choosing MmCached
                // unconditionally creates a conflicting WB alias when virglrenderer
                // reports WC/UC. Such an alias is architecturally invalid and can
                // expose stale cache lines after the host GPU writes the blob.
                // Unmapped below.
                let cache = super::blob_map::map_cache_to_mm(prep.map_cache);
                let va = unsafe { MmMapIoSpace(pa, prep.size, cache) } as *mut u8;
                if !va.is_null() {
                    break (prep, va);
                }
                false
            }
            Err(error) => matches!(error, crate::virtio::VirtioError::Timeout),
        };
        BAR_ERR_MAP.fetch_add(1, Ordering::Relaxed);
        failed += 1;
        if backoff(passive, failed, timed_out).is_none() {
            return false;
        }
    };
    f(va, prep.size);
    // SAFETY: `va` maps `prep.size` bytes, mapped just above.
    unsafe { MmUnmapIoSpace(va as *mut c_void, prep.size) };
    true
}

/// Visit contiguous runs of the exact ordinary-RAM PFNs named by one live
/// virtual paging transfer.
///
/// `MmMapIoSpace` is deliberately limited to this operation-owned scope. The
/// Windows contract permits it for locked-down pages; VidMm supplied and owns
/// that lock while BuildPagingBuffer handles this transfer. Numerical PFNs are
/// never retained. A range needed by a later Present acquires its own
/// `MmProbeAndLockPages` lease from the transient VA before it is unmapped.
unsafe fn for_each_paging_system_run(
    passive: PassiveLevel,
    pages: &[u64],
    system_virtual_address: u64,
    size: u64,
    mut visit: impl FnMut(*mut u8, u64, u64) -> bool,
) -> bool {
    if size == 0 {
        return true;
    }
    let byte_offset = system_virtual_address & 0xFFF;
    let Some(last_byte) = byte_offset.checked_add(size - 1) else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let Some(page_count) = usize::try_from((last_byte >> 12) + 1).ok() else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    // `pages` was resolved for the requested size; a transfer cut to a shorter
    // mapped blob needs only the first `page_count` of them.
    if pages.len() < page_count {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let pages = &pages[..page_count];

    let mut page_index = 0usize;
    let mut page_offset = byte_offset;
    let mut copied = 0u64;
    while copied < size {
        let Some(first_physical) = helios_kmd_logic::Pfn(pages[page_index]).physical_address()
        else {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let mut run_pages = 1usize;
        while page_index + run_pages < pages.len()
            && pages[page_index + run_pages - 1].checked_add(1)
                == Some(pages[page_index + run_pages])
        {
            run_pages += 1;
        }
        let Some(map_size) = (run_pages as u64).checked_mul(4096) else {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        // SAFETY: VidMm keeps every supplied system PTE locked for this paging
        // operation. MmCached matches ordinary system RAM's established cache
        // attribute, and the view is released before BuildPagingBuffer returns.
        let mut failed = 0u32;
        let mapping = loop {
            // Rebuilt per attempt: the address is passed by value.
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = first_physical as i64;
            let mapping =
                unsafe { MmMapIoSpace(pa, map_size, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
            if !mapping.is_null() {
                break mapping;
            }
            BAR_ERR_MAP.fetch_add(1, Ordering::Relaxed);
            failed += 1;
            // PASSIVE (the token); nothing is mapped or held across the sleep.
            if backoff(passive, failed, false).is_none() {
                return false;
            }
        };
        let available = map_size - page_offset;
        let chunk = (size - copied).min(available);
        let start = unsafe { mapping.add(page_offset as usize) };
        let accepted = visit(start, copied, chunk);
        // SAFETY: exact mapping and size returned above.
        unsafe { MmUnmapIoSpace(mapping.cast(), map_size) };
        if !accepted {
            return false;
        }
        copied += chunk;
        page_index += run_pages;
        page_offset = 0;
    }
    copied == size
}

/// Take an independent memory-manager lease on a paging transfer's system end.
unsafe fn remember_system_backing(
    passive: PassiveLevel,
    content_guard: &SystemBackingGuard<'_>,
    resource_id: u32,
    blob_offset: u64,
    size: u64,
    system_start: *mut u8,
) -> bool {
    let Some(range) =
        (unsafe { content_guard.acquire_range(passive, blob_offset, size, system_start) })
    else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let mut replacements = Vec::new();
    if replacements.try_reserve_exact(1).is_err() {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    replacements.push(range);
    if content_guard.replace_range(passive, resource_id, blob_offset, size, replacements) {
        BAR_SYSTEM_BACKING_CAPTURES.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Mirror a completed Present destination blob into the exact system-memory
/// backing Windows previously supplied for that allocation.
///
/// `None` means the allocation is no longer system-backed (for example Windows
/// paged it back into the BAR before this Present). `Some(false)` is a real
/// mapping/copy failure and must not be silently treated as a successful frame.
pub(crate) unsafe fn mirror_present_system_backing(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Option<bool> {
    let Some(content_guard) = adapter.system_backings.serialize(passive) else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return Some(false);
    };
    let backing = content_guard.snapshot(resource_id)?;
    let mut copied = false;
    let mapped = unsafe {
        with_blob_bytes(passive, adapter, resource_id, |blob, len| {
            // SAFETY: every range validates itself against `len`; all leases
            // remain alive through `backing`, and the content guard excludes
            // paging replacement/removal for the complete multi-range copy.
            copied = backing.copy_from_blob(blob, len);
        })
    };
    let ok = mapped && copied;
    if ok {
        BAR_SYSTEM_BACKING_MIRRORS.fetch_add(1, Ordering::Relaxed);
    } else {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    Some(ok)
}

/// WDDM 2.x `VIRTUAL_TRANSFER` for a Helios blob allocation.
///
/// The allocation handle, direction, VAs, size, and the leaf PTEs resolving the
/// system-memory side are all supplied by VidMm. The allocation offset applies
/// only to the blob and is deliberately not added to either virtual address, as
/// required by `DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL`.
///
/// Returns `false` for an op that was skipped (the caller answers STATUS_SUCCESS
/// and counts it). A skipped LOCAL_TO_SYSTEM eviction of a live allocation marks
/// its system copy invalid, so the page-in VidMm will issue for it is skipped
/// rather than copying garbage over the blob (see [`note_skipped_eviction`]).
unsafe fn bar_virtual_transfer(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    content_guard: &SystemBackingGuard<'_>,
    transfer: &DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL,
) -> bool {
    // Set by the inner function once it knows this is an eviction of a live,
    // reachable allocation: from there on a `false` return is a lost snapshot.
    let mut evicting: Option<u32> = None;
    let ok = unsafe {
        bar_virtual_transfer_inner(passive, adapter, content_guard, transfer, &mut evicting)
    };
    if !ok {
        if let Some(resource_id) = evicting {
            note_skipped_eviction(adapter, resource_id);
        }
    }
    ok
}

unsafe fn bar_virtual_transfer_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    content_guard: &SystemBackingGuard<'_>,
    transfer: &DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL,
    evicting: &mut Option<u32>,
) -> bool {
    let Some(alloc) = (unsafe { paging_alloc_info(adapter, transfer.hAllocation) }) else {
        // Unknown handle, or an allocation of an older transport generation: there
        // is no blob this driver can name, so nothing to preserve or to skip.
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if !alloc.bar_eligible {
        // Device-local optimal resources cannot be interpreted as linear bytes.
        // This preserves the existing host-owned-content behavior; the software
        // content engine applies only to allocations KMD made blob-linear.
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    let offset = transfer.AllocationOffsetInBytes;
    // The transfer is bounded by what this driver recorded of the allocation, and
    // an overrun is CUT, not refused: VidMm sizes the VA window of a virtual
    // transfer itself (measured 0x1E10000 against a recorded 0x1C20000), and the
    // only status that could refuse it is one VidMm bugchecks on. The part past
    // the allocation is padding and moves nothing.
    let size = match pg::clamp_range(alloc.size, offset, transfer.TransferSizeInBytes) {
        Clamp::Full(n) => n,
        Clamp::Clamped(n) => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            n
        }
        Clamp::Nothing => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    };
    if size == 0 {
        return true;
    }
    BAR_LAST_XFER_OFF.store(offset as u32, Ordering::Relaxed);
    let virtual_flags = unsafe { transfer.Flags.__bindgen_anon_1.Flags };
    BAR_LAST_XFER_FLAGS.store(virtual_flags, Ordering::Relaxed);
    BAR_LAST_VIRTUAL_SRC.store(transfer.SourceVirtualAddress, Ordering::Relaxed);
    BAR_LAST_VIRTUAL_DST.store(transfer.DestinationVirtualAddress, Ordering::Relaxed);

    use crate::dxgk::_DXGK_MEMORY_TRANSFER_DIRECTION as Direction;
    let (system_va, blob_to_system) = match transfer.TransferDirection {
        Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_SYSTEM => {
            (transfer.DestinationVirtualAddress, true)
        }
        Direction::DXGK_MEMORY_TRANSFER_SYSTEM_TO_LOCAL => (transfer.SourceVirtualAddress, false),
        Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_LOCAL => {
            // The allocation's bytes are intrinsic to its venus blob; a VidMm
            // placement move does not move or alias that host memory object.
            BAR_XFER_MOVE.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        _ => {
            BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    // From here on this is a content op on a live allocation of this generation.
    if blob_to_system {
        *evicting = Some(alloc.resource_id);
    } else if pg::page_in_decision(content_guard.page_in_blocked(alloc.resource_id))
        == pg::PageIn::SkipBlobAuthoritative
    {
        // The matching eviction was skipped: the system pages are not this
        // allocation's content, the blob is. Copying them in would destroy it.
        BAR_INVALID_SKIPS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // The retained PTE shadow models 4-KiB leaves. Interpreting a 64-KiB page
    // table through it would address the wrong physical pages, so skip the
    // operation instead of silently corrupting either copy. This is a capability
    // boundary, not a best-effort fallback: add 64-KiB shadow support before
    // accepting either documented flag. The driver never declares 64-KiB pages
    // (`PgV64` must stay 0; see `BAR_VIRTUAL_FLAGS`).
    if virtual_flags != 0 {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        BAR_VIRTUAL_FLAGS.fetch_add(1, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"Pg64Ref", virtual_flags);
        return false;
    }

    let Some(system_pages) = adapter.paging_pte_shadow.resolve(passive, system_va, size) else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    let mut replacements = Vec::new();
    let replacement_capacity = system_pages.len().min(MAX_SYSTEM_BACKING_RANGES);
    let retain_system_backing =
        alloc.system_backing_policy == SystemBackingPolicy::PresentLinearBuffer;
    if blob_to_system
        && retain_system_backing
        && !reserve_retry(passive, &mut replacements, replacement_capacity)
    {
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // The bytes actually moved: `size` cut again to the MAPPED blob (a blob
    // shorter than the recorded allocation moves its prefix, not nothing).
    let mut moved = 0u64;
    // Set when the lease bookkeeping for a run could not be kept. NEVER a reason
    // to stop copying: the data copy and the lease record are independent, and an
    // eviction that stops after some runs leaves the system image half garbage
    // while VidMm is told it is whole.
    let mut lease_failed = false;
    let mut copied = false;
    let mapped = unsafe {
        with_blob_bytes(passive, adapter, alloc.resource_id, |blob, blob_size| {
            let n = match pg::clamp_to_mapped(offset, size, blob_size) {
                Clamp::Full(n) => n,
                Clamp::Clamped(n) => {
                    BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                    n
                }
                Clamp::Nothing => {
                    BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            // SAFETY: the blob range `[offset, offset + n)` lies inside the mapping
            // (`clamp_to_mapped`); the PTE shadow resolves exactly the other side
            // of this live VidMm paging operation.
            copied = for_each_paging_system_run(
                passive,
                &system_pages,
                system_va,
                n,
                |system_start, range_offset, range_size| {
                    let Some(blob_range_offset) = offset.checked_add(range_offset) else {
                        return false;
                    };
                    let Ok(blob_range_offset_usize) = usize::try_from(blob_range_offset) else {
                        return false;
                    };
                    let Ok(range_size_usize) = usize::try_from(range_size) else {
                        return false;
                    };
                    let blob_start = blob.add(blob_range_offset_usize);
                    if blob_to_system {
                        core::ptr::copy_nonoverlapping(blob_start, system_start, range_size_usize);
                        if retain_system_backing && !lease_failed {
                            // Acquire the independent lock while this run's
                            // operation-owned system mapping is still live. A
                            // refusal (pinned-byte ceiling, MDL lock failure, the
                            // range table or the record table full) only costs
                            // later Present mirroring into this surface's CPU view:
                            // the run is copied, and so are all that follow.
                            if replacements.len() >= MAX_SYSTEM_BACKING_RANGES
                                || replacements.len() >= replacements.capacity()
                            {
                                lease_failed = true;
                                BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                            } else {
                                match content_guard.acquire_range(
                                    passive,
                                    blob_range_offset,
                                    range_size,
                                    system_start,
                                ) {
                                    Some(range) => replacements.push(range),
                                    None => {
                                        lease_failed = true;
                                        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    } else {
                        core::ptr::copy_nonoverlapping(system_start, blob_start, range_size_usize);
                    }
                    true
                },
            );
            moved = n;
        })
    };
    if !(mapped && copied) {
        // Skipped (counted at the failing site). A page-in that stopped partway
        // leaves the blob partly updated from valid system pages; an eviction is
        // marked by the caller.
        return false;
    }
    if blob_to_system {
        if retain_system_backing {
            let recorded = !lease_failed
                && content_guard.replace_range(
                    passive,
                    alloc.resource_id,
                    offset,
                    moved,
                    core::mem::take(&mut replacements),
                );
            if !recorded {
                // The snapshot is complete; only the record that lets Present keep
                // mirroring into the system copy is not. Drop what was gathered
                // (unlocking those runs) and any older record of this range, which
                // no longer describes the system image, and say so (`PgSe`).
                replacements.clear();
                BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                if !content_guard.remove_range(passive, alloc.resource_id, offset, moved) {
                    BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                BAR_SYSTEM_BACKING_CAPTURES.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The system copy now holds the content: whole-allocation evictions clear
        // an earlier "invalid" mark.
        note_eviction_done(content_guard, alloc.size, offset, moved, alloc.resource_id);
        BAR_XFER_OUT.fetch_add(1, Ordering::Relaxed);
    } else {
        // The page-in copied; the blob is authoritative again. A record that
        // cannot be dropped only leaves a stale lease behind (counted).
        if retain_system_backing
            && !content_guard.remove_range(passive, alloc.resource_id, offset, moved)
        {
            BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
        BAR_XFER_IN.fetch_add(1, Ordering::Relaxed);
        // Level 5: the page-in wrote the primary through the CPU (see `bar_transfer`).
        primary_changed(adapter, Edge::Paging, alloc.resource_id);
    }
    true
}

/// Classic TRANSFER touching the BAR segment: content copy between the
/// allocation's blob and its system-memory backing (synchronous — done before
/// the paging fence retires, so VidMm's content model stays truthful across
/// eviction/re-commit). PASSIVE_LEVEL.
unsafe fn bar_transfer(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    content_guard: &SystemBackingGuard<'_>,
    bar_id: u32,
    t: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1,
) -> PagingOpOutcome {
    let src_seg = t.Source.SegmentId;
    let dst_seg = t.Destination.SegmentId;
    if src_seg != bar_id && dst_seg != bar_id {
        return PagingOpOutcome::NotOurs; // aperture/system transfer — null engine
    }
    // No transport (StopDevice, a live `pnputil /restart-device`, host loss): there
    // is no blob to read or write and no host copy of the content to keep
    // truthful, because VidMm is tearing this adapter down. Failing the paging
    // operation anyway hands VidMm a status it cannot tolerate (0x10E
    // VIDEO_MEMORY_MANAGEMENT_INTERNAL, 0xC000009A, measured on a restart under
    // DWM load), so count it and report that nothing needed doing. With the
    // transport UP a failed copy is skipped and counted instead.
    if adapter.with_virtio(|_| ()).is_err() {
        BAR_ERR_TX_GONE.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::NotOurs;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(adapter, t.hAllocation) }) else {
        // The transfer names the BAR segment but no live Helios allocation of this
        // transport generation: there is nothing this driver can copy, and the
        // caller must not read that as "content moved".
        BAR_ERR_XFER_HANDLE.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::Failed(paging_failure());
    };
    if !alloc.bar_eligible {
        // A device-local OPTIMAL image cannot be interpreted as linear bytes.
        // VidMm placement bookkeeping is decorative for this host-owned memory;
        // never issue RESOURCE_MAP_BLOB for it.
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::NotOurs;
    }
    let flags = unsafe { t.Flags.__bindgen_anon_1.Value };
    BAR_LAST_XFER_FLAGS.store(flags, Ordering::Relaxed);
    BAR_LAST_XFER_OFF.store(t.TransferOffset, Ordering::Relaxed);
    BAR_LAST_MDL_OFF.store(t.MdlOffset, Ordering::Relaxed);
    // Cut to the recorded allocation, as in `bar_virtual_transfer`; nothing known
    // to move is a counted no-op, not an error.
    let bytes = match pg::clamp_range(alloc.size, t.TransferOffset as u64, t.TransferSize as u64) {
        Clamp::Full(n) => n,
        Clamp::Clamped(n) => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            n
        }
        Clamp::Nothing => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            return PagingOpOutcome::NotOurs;
        }
    };
    // Microsoft defines these two offsets independently: TransferOffset is a
    // byte offset applied only to the segment location, while MdlOffset names
    // the first system-memory page inside the MDL. Never leak TransferOffset's
    // low bits into the MDL side.
    let mdl_page = t.MdlOffset;
    let blob_off = t.TransferOffset as u64;

    match (src_seg, dst_seg) {
        // Page-in: system backing → blob (evicted or initial content).
        (0, s) if s == bar_id => {
            // A skipped eviction left the SYSTEM pages holding garbage while VidMm
            // believes they hold the content. The blob is the only good copy.
            if pg::page_in_decision(content_guard.page_in_blocked(alloc.resource_id))
                == pg::PageIn::SkipBlobAuthoritative
            {
                BAR_INVALID_SKIPS.fetch_add(1, Ordering::Relaxed);
                return PagingOpOutcome::Failed(paging_failure());
            }
            // SAFETY: `t.Source` is the transfer's source descriptor; its
            // SegmentId selects the union arm, and it is 0 on this arm.
            let TransferEnd::SystemMdl(mdl) = (unsafe { TransferEnd::source(&t.Source) }) else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return PagingOpOutcome::Failed(paging_failure());
            };
            let Some(window) = (unsafe { mdl_system_va(passive, mdl) }) else {
                // PgEx counted inside mdl_system_va, after its retries. The
                // page-in did not happen: the blob keeps its last content.
                return PagingOpOutcome::Failed(paging_failure());
            };
            // The MDL side is range-checked exactly like the blob side.
            let Some(src) = window.slice_at_page(mdl_page, bytes) else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return PagingOpOutcome::Failed(paging_failure());
            };
            let mut copied = false;
            let ok = unsafe {
                with_blob_bytes(passive, adapter, alloc.resource_id, |dst, len| {
                    // The mapped blob may be shorter than the recorded allocation:
                    // move what is there.
                    let n = match pg::clamp_to_mapped(blob_off, bytes, len) {
                        Clamp::Full(n) => n,
                        Clamp::Clamped(n) => {
                            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                            n
                        }
                        Clamp::Nothing => {
                            BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    };
                    // SAFETY: dst covers `len` blob bytes and src covers `bytes`
                    // mapped MDL bytes, both checked above, and n <= both.
                    core::ptr::copy_nonoverlapping(src, dst.add(blob_off as usize), n as usize);
                    copied = true;
                })
            };
            if !(ok && copied) {
                // PgEm (blob map, after retries) or PgEb already counted.
                return PagingOpOutcome::Failed(paging_failure());
            }
            // The inverse transfer makes the BAR blob authoritative again. The
            // copy has already happened, so a record that cannot be dropped is
            // counted (it leaves a stale lease behind), not reported as a skipped
            // page-in.
            if alloc.system_backing_policy == SystemBackingPolicy::PresentLinearBuffer
                && !content_guard.remove_range(passive, alloc.resource_id, blob_off, bytes)
            {
                BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            BAR_XFER_IN.fetch_add(1, Ordering::Relaxed);
            // Level 5: this wrote the primary the screen may be showing through the CPU, with
            // no present call: a frame is owed (atomics only; nothing when it is another surface).
            primary_changed(adapter, Edge::Paging, alloc.resource_id);
            PagingOpOutcome::Executed
        }
        // Eviction: blob → system backing. From here every way of not completing
        // the copy goes through `eviction_skipped`: VidMm is told the system pages
        // are current, so the allocation must be remembered as invalid.
        (s, 0) if s == bar_id => {
            // SAFETY: as the page-in arm — SegmentId 0 selects pMdl.
            let TransferEnd::SystemMdl(mdl) = (unsafe { TransferEnd::destination(&t.Destination) })
            else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return eviction_skipped(adapter, alloc.resource_id);
            };
            let Some(window) = (unsafe { mdl_system_va(passive, mdl.cast()) }) else {
                // PgEx counted inside mdl_system_va, after its retries.
                return eviction_skipped(adapter, alloc.resource_id);
            };
            // THE WRITE SIDE: an unchecked `mdl_off + bytes` here is a kernel
            // memory write past the mapped buffer.
            let Some(dst_start) = window.slice_at_page(mdl_page, bytes) else {
                BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                return eviction_skipped(adapter, alloc.resource_id);
            };
            let mut copied = false;
            // The bytes actually moved: `bytes` cut again to the MAPPED blob. A
            // blob shorter than the recorded allocation moves its prefix, and the
            // lease and the "invalid" bookkeeping below must describe that prefix,
            // not the request (a lease running past the blob makes every later
            // Present mirror and fill of this allocation fail).
            let mut moved = 0u64;
            let ok = unsafe {
                with_blob_bytes(passive, adapter, alloc.resource_id, |src, len| {
                    let n = match pg::clamp_to_mapped(blob_off, bytes, len) {
                        Clamp::Full(n) => n,
                        Clamp::Clamped(n) => {
                            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                            n
                        }
                        Clamp::Nothing => {
                            BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    };
                    // SAFETY: src covers `len` blob bytes and dst_start covers
                    // `bytes` mapped MDL bytes, both checked above, and n <= both.
                    core::ptr::copy_nonoverlapping(
                        src.add(blob_off as usize),
                        dst_start,
                        n as usize,
                    );
                    moved = n;
                    copied = true;
                })
            };
            if !(ok && copied) {
                // PgEm / PgEb already counted.
                return eviction_skipped(adapter, alloc.resource_id);
            }
            // The copy is complete. Whether Present can keep mirroring into it is
            // a separate matter: without an independent lease it cannot, but the
            // snapshot is whole, so this is counted (`PgSe`) and still success.
            if alloc.system_backing_policy == SystemBackingPolicy::PresentLinearBuffer
                && !unsafe {
                    remember_system_backing(
                        passive,
                        content_guard,
                        alloc.resource_id,
                        blob_off,
                        moved,
                        dst_start,
                    )
                }
            {
                // Any older record of this range no longer describes the system
                // image. Best effort; both failures are already in PgSe.
                if !content_guard.remove_range(passive, alloc.resource_id, blob_off, moved) {
                    BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
            }
            note_eviction_done(
                content_guard,
                alloc.size,
                blob_off,
                moved,
                alloc.resource_id,
            );
            BAR_XFER_OUT.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Executed
        }
        // Move within the segment: content is intrinsic to the blob; the CPU
        // view follows the aperture maps. Nothing to copy.
        (s, d) if s == bar_id && d == bar_id => {
            BAR_XFER_MOVE.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Executed
        }
        // BAR ↔ aperture/paging-RAM combinations are not part of the declared
        // allocation segment sets; loud counter, no silent data motion.
        _ => {
            BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
            PagingOpOutcome::Failed(paging_failure())
        }
    }
}

/// Classic FILL of a BAR-segment allocation: CPU-fill the blob. PASSIVE_LEVEL.
unsafe fn bar_fill(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    content_guard: &SystemBackingGuard<'_>,
    bar_id: u32,
    f: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2,
) -> PagingOpOutcome {
    if f.Destination.SegmentId != bar_id {
        return PagingOpOutcome::NotOurs;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(adapter, f.hAllocation) }) else {
        // Same class as PgEh on the transfer side: a BAR-segment fill naming no
        // live allocation is a refusal, not a no-op.
        BAR_ERR_FILL_HANDLE.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::Failed(paging_failure());
    };
    if !alloc.bar_eligible {
        BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
        return PagingOpOutcome::NotOurs;
    }
    let fill_len = match pg::clamp_range(alloc.size, 0, f.FillSize as u64) {
        Clamp::Full(n) => n,
        Clamp::Clamped(n) => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            n
        }
        Clamp::Nothing => {
            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
            return PagingOpOutcome::NotOurs;
        }
    };
    let pattern = f.FillPattern;
    let system_backing = content_guard.snapshot(alloc.resource_id);
    let mut filled = false;
    let ok = unsafe {
        with_blob_bytes(passive, adapter, alloc.resource_id, |dst, len| {
            // A blob shorter than the recorded allocation is filled up to its mapped
            // length (the second bound, after the recorded-size cut above).
            let n = match pg::clamp_to_mapped(0, fill_len, len) {
                Clamp::Full(n) => n,
                Clamp::Clamped(n) => {
                    BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                    n
                }
                Clamp::Nothing => {
                    BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            fill_pattern(dst, n as usize, pattern);
            filled = system_backing
                .as_ref()
                .is_none_or(|backing| backing.copy_blob_range(dst, len, 0, n));
        })
    };
    if !(ok && filled) {
        // PgEm (blob map) or PgEb (nothing mapped) already counted.
        return PagingOpOutcome::Failed(paging_failure());
    }
    BAR_FILLS.fetch_add(1, Ordering::Relaxed);
    // Level 5: the fill wrote the primary (see `bar_transfer`).
    primary_changed(adapter, Edge::Paging, alloc.resource_id);
    PagingOpOutcome::Executed
}

/// Write `pattern` (u32, repeated) over `len` bytes at `dst`.
fn fill_pattern(dst: *mut u8, len: usize, pattern: u32) {
    let words = len / 4;
    for i in 0..words {
        // SAFETY: caller bounds-checked `len` bytes at `dst`.
        unsafe { core::ptr::write_unaligned((dst as *mut u32).add(i), pattern) };
    }
    let bytes = pattern.to_le_bytes();
    for i in (words * 4)..len {
        // SAFETY: as above; tail bytes of a non-multiple-of-4 fill.
        unsafe { *dst.add(i) = bytes[i % 4] };
    }
}

/// Diagnostic harvest of a LEAF `UPDATE_PAGE_TABLE`: record the BAR-segment
/// physical placement VidMm assigned (pure atomic store — DISPATCH-safe, no
/// side effects; content ops do not depend on it in the aperture model).
unsafe fn bar_harvest_page_table(
    adapter: &AdapterContext,
    bar_id: u32,
    bar_size: u64,
    u: &DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE,
) {
    if u.PageTableLevel != 0 || u.pPageTableEntries.is_null() || u.NumPageTableEntries == 0 {
        return;
    }
    let Some(alloc) = (unsafe { paging_alloc_info(adapter, u.hAllocation) }) else {
        return;
    };
    if !alloc.bar_eligible {
        return;
    }
    // SAFETY: pPageTableEntries holds NumPageTableEntries DXGK_PTEs for the call.
    let pte0 = unsafe { core::ptr::read_unaligned(u.pPageTableEntries) };
    let valid = unsafe { pte0.__bindgen_anon_1.__bindgen_anon_1 }.Valid() != 0;
    let seg = unsafe { pte0.__bindgen_anon_1.__bindgen_anon_1 }.Segment() as u32;
    if !valid || seg != bar_id {
        return;
    }
    let page0 = unsafe { pte0.__bindgen_anon_2.PageAddress };
    // Contiguity check: a memory-segment allocation should be one contiguous
    // range; discontiguous PTEs are counted (they would matter if partial
    // aperture maps ever need placement-relative offsets).
    let n = u.NumPageTableEntries as u64;
    if n >= 2 && u.Flags.Repeat() == 0 {
        let last = unsafe { core::ptr::read_unaligned(u.pPageTableEntries.add((n - 1) as usize)) };
        let last_valid = unsafe { last.__bindgen_anon_1.__bindgen_anon_1 }.Valid() != 0;
        if last_valid && unsafe { last.__bindgen_anon_2.PageAddress } != page0 + (n - 1) {
            BAR_ERR_DISCONTIG.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    // Same guard as the transfer path: an unchecked `page0 << 12` wraps.
    let Some(page0_address) = helios_kmd_logic::Pfn(page0).physical_address() else {
        BAR_ERR_VIRTUAL.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if let Some(base) = page0_address
        .checked_sub(u.AllocationOffsetInBytes)
        .filter(|b| *b < bar_size)
    {
        if alloc.bar_placed != base {
            // SAFETY: h is the live paging-op allocation handle.
            unsafe { set_bar_placement(u.hAllocation, base) };
            BAR_PT_HARVESTS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One `DXGKARG_BUILDPAGINGBUFFER` operation, with its union arm already resolved.
///
/// # What this replaces
///
/// SIX sites repeated the same `// SAFETY: union arm selected by Operation.`
/// sentence and reached into `args.__bindgen_anon_1` directly, and the operation
/// set was maintained TWICE — once in an `is_content_op` `matches!` list and once
/// in the dispatch arms — with nothing making the two agree. Added to the
/// dispatch only, an operation never ran; added to `is_content_op` only, it fell
/// through the wildcard and reported success having done nothing.
///
/// Now every union read happens inside [`PagingOperation::parse`], the IRQL gate
/// keys off the parsed value, and the dispatch is an exhaustive match over a safe
/// enum — so adding an operation forces a decision in exactly one place.
///
/// The residual trust is stated once instead of six times: `parse` believes
/// `Operation` describes the live arm. That is the DDI contract, and there is no
/// discriminant to check it against.
enum PagingOperation<'a> {
    UpdatePageTable(&'a DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE),
    Transfer(&'a _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1),
    Fill(&'a _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2),
    DiscardContent(&'a _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3),
    VirtualFill(&'a DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL),
    VirtualTransfer(&'a DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL),
    /// Any operation this driver does not service — the null engine.
    Other,
}

impl<'a> PagingOperation<'a> {
    /// Resolve the union arm named by `Operation`.
    ///
    /// # Safety
    /// `args.Operation` must describe the union arm dxgkrnl initialised. That is
    /// the `DxgkDdiBuildPagingBuffer` contract; this is the ONE place in the
    /// driver that relies on it.
    unsafe fn parse(args: &'a DXGKARG_BUILDPAGINGBUFFER) -> Self {
        use crate::dxgk::_DXGK_BUILDPAGINGBUFFER_OPERATION as PagingOp;
        // SAFETY: per the fn contract — each arm is read only when `Operation`
        // names it.
        unsafe {
            match args.Operation {
                PagingOp::DXGK_OPERATION_UPDATE_PAGE_TABLE => {
                    Self::UpdatePageTable(args.__bindgen_anon_1.UpdatePageTable.as_ref())
                }
                PagingOp::DXGK_OPERATION_TRANSFER => {
                    Self::Transfer(args.__bindgen_anon_1.Transfer.as_ref())
                }
                PagingOp::DXGK_OPERATION_FILL => Self::Fill(args.__bindgen_anon_1.Fill.as_ref()),
                PagingOp::DXGK_OPERATION_DISCARD_CONTENT => {
                    Self::DiscardContent(args.__bindgen_anon_1.DiscardContent.as_ref())
                }
                PagingOp::DXGK_OPERATION_VIRTUAL_FILL => {
                    Self::VirtualFill(args.__bindgen_anon_1.FillVirtual.as_ref())
                }
                PagingOp::DXGK_OPERATION_VIRTUAL_TRANSFER => {
                    Self::VirtualTransfer(args.__bindgen_anon_1.TransferVirtual.as_ref())
                }
                _ => Self::Other,
            }
        }
    }

    /// Whether this operation needs PASSIVE_LEVEL: host round-trips and `Mm`
    /// mapping calls.
    ///
    /// THE single source for that set. It used to be a `matches!` list beside the
    /// dispatch, which is the drift hazard this enum removes.
    const fn is_content_op(&self) -> bool {
        matches!(
            self,
            Self::Transfer(_)
                | Self::Fill(_)
                | Self::DiscardContent(_)
                | Self::VirtualFill(_)
                | Self::VirtualTransfer(_)
        )
    }
}

/// Which end of a classic TRANSFER a `DXGK_TRANSFERVIRTUAL`-style descriptor
/// names.
///
/// The SECOND, undocumented discriminant in this DDI: `SegmentId == 0` selects
/// `pMdl` out of a `__BindgenUnionField`, and that rule lived only in an inline
/// comment at two sites. Resolving it in one place means a future arm cannot copy
/// the neighbouring line and read the wrong union field — which would compile,
/// run, and swap a segment id for an MDL pointer, making `bar_transfer` copy in
/// the wrong direction.
enum TransferEnd {
    /// A real memory segment. The id itself is read directly from the
    /// descriptor by `bar_transfer`'s `(src_seg, dst_seg)` match — this variant
    /// exists to say "NOT the MDL arm", which is the discriminant that was
    /// undocumented.
    Segment,
    /// Segment 0: system memory, described by an MDL.
    SystemMdl(PMDL),
}

impl TransferEnd {
    /// The transfer's SOURCE end.
    ///
    /// Source and Destination are structurally identical but are SEPARATE
    /// bindgen types, so there are two constructors rather than one generic —
    /// which also means neither can be applied to the wrong end by accident.
    ///
    /// # Safety
    /// `end` is the live `Source` descriptor of a `DXGK_OPERATION_TRANSFER`,
    /// whose `SegmentId` selects its union arm.
    unsafe fn source(
        end: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1,
    ) -> Self {
        if end.SegmentId == 0 {
            // SAFETY: the pMdl arm is the live one exactly when SegmentId is 0.
            // The cast maps the dxgk-bindings MDL to the layout-identical
            // wdk_sys MDL.
            Self::SystemMdl(unsafe { *end.__bindgen_anon_1.pMdl.as_ref() }.cast())
        } else {
            Self::Segment
        }
    }

    /// The transfer's DESTINATION end. See [`Self::source`].
    ///
    /// # Safety
    /// As [`Self::source`], for the `Destination` descriptor.
    unsafe fn destination(
        end: &_DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2,
    ) -> Self {
        if end.SegmentId == 0 {
            // SAFETY: as `source`.
            Self::SystemMdl(unsafe { *end.__bindgen_anon_1.pMdl.as_ref() }.cast())
        } else {
            Self::Segment
        }
    }
}

/// The content mutex could not be taken, so `operation` is skipped whole. If it
/// was an eviction of a live allocation of this generation, remember that: the
/// page-in that follows must not copy the (unwritten) system pages over the blob.
/// Reads no content state — only the allocation handle — so it needs no guard.
fn note_unserialized_eviction(
    adapter: &AdapterContext,
    bar_id: u32,
    operation: &PagingOperation<'_>,
) {
    use crate::dxgk::_DXGK_MEMORY_TRANSFER_DIRECTION as Direction;
    let h_allocation = match operation {
        PagingOperation::Transfer(t)
            if t.Destination.SegmentId == 0 && t.Source.SegmentId == bar_id =>
        {
            t.hAllocation
        }
        PagingOperation::VirtualTransfer(tv)
            if matches!(
                tv.TransferDirection,
                Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_SYSTEM
            ) =>
        {
            tv.hAllocation
        }
        _ => return,
    };
    // SAFETY: an in-flight paging op's hAllocation, as everywhere in this file.
    if let Some(alloc) = unsafe { paging_alloc_info(adapter, h_allocation) } {
        if alloc.bar_eligible {
            note_skipped_eviction(adapter, alloc.resource_id);
        }
    }
}

/// `DxgkDdiBuildPagingBuffer` — translate a memory-management operation into GPU
/// DMA. Null engine for the aperture / page-table segments; REAL content engine
/// for BAR-segment allocations. See the module doc.
pub unsafe extern "C" fn dxgkddi_build_paging_buffer(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
) -> NTSTATUS {
    // The last-operation record, the Evict counts and the longest call
    // (`ddi::device_lost`, `PgLast*` / `PgEv*` / `PgLongUs`): atomics and two clock reads, no
    // change to what the inner function decides or answers.
    let started = crate::adapter::foreign_scanout::now_100ns();
    let mut note = PagingNote::new();
    // SAFETY: the DDI contract, forwarded unchanged.
    let status = unsafe { build_paging_buffer_inner(h_adapter, build_paging_buffer, &mut note) };
    crate::ddi::device_lost::paging_done(
        note.op,
        note.handle,
        note.size,
        note.kind,
        note.result,
        started,
    );
    status
}

/// What the call did, for `ddi::device_lost::paging_done`.
struct PagingNote {
    op: u32,
    /// The allocation handle's low 32 bits (never dereferenced here), 0 for none.
    handle: u32,
    /// The bytes the operation names, low 32 bits.
    size: u32,
    kind: dl::PagingKind,
    /// A `dl::paging_result` code.
    result: u32,
}

impl PagingNote {
    const fn new() -> Self {
        Self {
            op: 0xFFFF_FFFF,
            handle: 0,
            size: 0,
            kind: dl::PagingKind::Other,
            result: dl::paging_result::NOT_OURS,
        }
    }

    /// Fill `handle`, `size` and `kind` from the parsed operation. `bar` is the BAR segment id.
    fn describe(&mut self, operation: &PagingOperation<'_>, bar: u32) {
        use crate::dxgk::_DXGK_MEMORY_TRANSFER_DIRECTION as Direction;
        match operation {
            PagingOperation::Transfer(t) => {
                self.handle = t.hAllocation as usize as u32;
                self.size = t.TransferSize as u32;
                self.kind = dl::transfer_kind(t.Source.SegmentId, t.Destination.SegmentId, bar);
            }
            PagingOperation::Fill(f) => {
                self.handle = f.hAllocation as usize as u32;
                self.size = f.FillSize as u32;
            }
            PagingOperation::DiscardContent(d) => {
                self.handle = d.hAllocation as usize as u32;
            }
            PagingOperation::VirtualFill(fv) => {
                self.handle = fv.hAllocation as usize as u32;
                self.size = fv.FillSizeInBytes as u32;
            }
            PagingOperation::VirtualTransfer(tv) => {
                self.handle = tv.hAllocation as usize as u32;
                self.size = tv.TransferSizeInBytes as u32;
                self.kind = match tv.TransferDirection {
                    Direction::DXGK_MEMORY_TRANSFER_LOCAL_TO_SYSTEM => dl::PagingKind::Evict,
                    Direction::DXGK_MEMORY_TRANSFER_SYSTEM_TO_LOCAL => dl::PagingKind::PageIn,
                    _ => dl::PagingKind::Other,
                };
            }
            PagingOperation::UpdatePageTable(u) => {
                self.handle = u.hAllocation as usize as u32;
            }
            PagingOperation::Other => {}
        }
    }
}

/// The body of [`dxgkddi_build_paging_buffer`]; every return records its outcome in `note`.
unsafe fn build_paging_buffer_inner(
    h_adapter: *mut c_void,
    build_paging_buffer: *mut DXGKARG_BUILDPAGINGBUFFER,
    note: &mut PagingNote,
) -> NTSTATUS {
    if h_adapter.is_null() || build_paging_buffer.is_null() {
        note.result = dl::paging_result::BAD_ARGS;
        return STATUS_INVALID_PARAMETER;
    }

    // SAFETY: valid per the DDI contract. We do NOT advance pDmaBuffer — no
    // hardware command is ever emitted (BAR-segment work runs synchronously
    // on the CPU right here, before the paging fence retires).
    let args = unsafe { &*build_paging_buffer };
    let op = args.Operation as u32;
    note.op = op;
    PAGING_LAST_OP.store(op, Ordering::Relaxed);
    PAGING_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    if op < 32 {
        PAGING_OP_SEEN_MASK.fetch_or(1u32 << op, Ordering::Relaxed);
    }

    // SAFETY: dxgkrnl hands back our AdapterContext as the miniport context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let Some(bar) = adapter.bar_segment() else {
        note.result = dl::paging_result::NO_BAR;
        return STATUS_SUCCESS; // BAR segment inactive → pure null engine
    };

    // ONE union read for the whole DDI.
    // SAFETY: `Operation` describes the arm dxgkrnl initialised — the DDI
    // contract, now relied on in exactly one place instead of six.
    let operation = unsafe { PagingOperation::parse(args) };
    note.describe(&operation, bar.seg_id);

    // Placement harvest is DISPATCH-safe (atomic store only) — no IRQL gate.
    if let PagingOperation::UpdatePageTable(update) = operation {
        let track_system_pages = unsafe { paging_alloc_info(adapter, update.hAllocation) }
            .is_some_and(|alloc| alloc.bar_eligible);
        // Preserve the exact leaf mapping before retiring the page-table update.
        // Every update clears its Windows-supplied VA range first, including
        // updates for unrelated allocations and explicit unmaps.
        if !unsafe {
            adapter
                .paging_pte_shadow
                .update_leaf(update, track_system_pages)
        } {
            // NO REGISTRY FLUSH HERE. This branch is the one the comment above
            // declares DISPATCH-safe, and `dump_bar_counters` is 24 synchronous
            // `RtlWriteRegistryValue` calls — PASSIVE_LEVEL only. It stood
            // directly against the driver's hardest invariant, twelve lines above
            // a content path that installs a runtime IRQL gate precisely because
            // the documented PASSIVE contract is not trusted (k-paging-05).
            // Nothing is lost: `update_leaf` already stored `BAR_ERR_SHADOW_FULL`
            // (PgEf) into its atomic, and the next PASSIVE content op mirrors the
            // whole block, so only the latency of that one value changes.
            //
            // The status is STATUS_SUCCESS all the same: VidMm bugchecks on
            // STATUS_INSUFFICIENT_RESOURCES from this DDI (see `paging_failure`).
            // The mapping was not retained, so a later virtual transfer through
            // this range resolves nothing and is skipped and counted
            // (PgEv + PgSkipV) rather than copied through a wrong page.
            BAR_SKIPPED.fetch_add(1, Ordering::Relaxed);
            note.result = dl::paging_result::SKIPPED;
            return paging_failure();
        }
        unsafe { bar_harvest_page_table(adapter, bar.seg_id, bar.size, update) };
        note.result = dl::paging_result::PTE;
        return STATUS_SUCCESS;
    }

    // The content-op set is a method on the parsed value, so it cannot drift
    // from the dispatch below.
    if !operation.is_content_op() {
        note.result = dl::paging_result::NOT_OURS;
        return STATUS_SUCCESS;
    }
    // Content ops need PASSIVE (host round-trips, Mm mapping calls). The DDI
    // is documented PASSIVE; if that ever fails in practice this counter
    // fires and the op degrades to the old null engine — loud, not silent.
    //
    // IRQL-DEGRADE POLICY (decided here, where the status is chosen): this arm
    // keeps STATUS_SUCCESS. It is the one place that cannot honestly fail,
    // because the gate runs before the union is parsed — it covers content ops
    // for allocations that are NOT ours (another segment, a device-local image)
    // just as much as ours, and failing those would refuse work this driver was
    // never asked to do. PgEi is the loud signal instead: it has never moved,
    // and a same-boot nonzero value is a design-gap escalation, not something
    // to absorb.
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    let irql = unsafe { KeGetCurrentIrql() };
    if irql != PASSIVE_LEVEL_IRQL {
        BAR_ERR_IRQL.fetch_add(1, Ordering::Relaxed);
        // The skipped op may be the eviction of one of OUR live allocations: VidMm
        // will believe the system pages hold its content, and they do not, so the
        // matching page-in must not copy them over the blob. Remembering that
        // reads only the allocation handle (atomics, the generation check) and
        // takes the invalid set's own spinlock (raise-to-DPC, legal up to
        // DISPATCH_LEVEL) — no guard, no mutex, no Mm call — so it is safe at any
        // IRQL this DDI could be called at. Above DISPATCH the spinlock is not
        // legal and nothing can be recorded; that is not a state this DDI is
        // documented to run in, and PgEi already makes it loud.
        if irql <= DISPATCH_LEVEL_IRQL {
            note_unserialized_eviction(adapter, bar.seg_id, &operation);
        }
        note.result = dl::paging_result::BAD_IRQL;
        return STATUS_SUCCESS;
    }
    // SAFETY: the strongest mint in the driver — `DxgkDdiBuildPagingBuffer` is
    // documented "IRQL: PASSIVE_LEVEL", and the gate immediately above CHECKED
    // it rather than trusting the annotation (PgEi has never moved). Every
    // content arm below is downstream of that check, so `IrqlBad` and PgEi can
    // never disagree for this DDI.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };

    // BuildPagingBuffer executes Helios's software content engine immediately,
    // while Present and teardown can run on other PASSIVE callbacks. Keep the
    // copy plus its backing-table transition atomic with respect to those
    // paths. This is a sleeping mutex: multi-megabyte copies never run under a
    // spinlock or at raised IRQL.
    // `PgMtxMaxUs` / `PgMtxFail`: how long VidMm's paging thread waited for the content mutex
    // (a teardown or a Present mirror holding it across a host round trip shows here).
    let wait_started = crate::adapter::foreign_scanout::now_100ns();
    let guard = adapter.system_backings.serialize(passive);
    crate::ddi::device_lost::paging_mutex_wait(
        (crate::adapter::foreign_scanout::now_100ns().saturating_sub(wait_started) / 10)
            .min(u32::MAX as u64) as u32,
        guard.is_some(),
    );
    let Some(content_guard) = guard else {
        note.result = dl::paging_result::NO_GUARD;
        BAR_SYSTEM_BACKING_ERRORS.fetch_add(1, Ordering::Relaxed);
        BAR_SKIPPED.fetch_add(1, Ordering::Relaxed);
        // Both directions are skipped. A skipped EVICTION still has to be
        // remembered (the invalid mark has its own lock and needs no guard).
        note_unserialized_eviction(adapter, bar.seg_id, &operation);
        return paging_failure();
    };

    // Every content arm yields a `PagingOpOutcome`, so the match itself is the
    // driver's answer: `Failed` is the only variant that reaches VidMm as a
    // status, and every arm that produces one routes through `paging_failure()`.
    let outcome = match operation {
        PagingOperation::Transfer(t) => unsafe {
            bar_transfer(passive, adapter, &content_guard, bar.seg_id, t)
        },
        PagingOperation::Fill(f) => unsafe {
            bar_fill(passive, adapter, &content_guard, bar.seg_id, f)
        },
        PagingOperation::DiscardContent(d) => {
            let discarded = unsafe { paging_alloc_info(adapter, d.hAllocation) };
            if let Some(alloc) = discarded {
                // The content is gone, so no system copy of it can be "invalid":
                // drop the backing ranges AND the mark.
                content_guard.remove_all(alloc.resource_id);
            }
            if d.SegmentId == bar.seg_id && discarded.is_some() {
                // Content lives in the blob; nothing to release here (aperture
                // unmaps handle CPU visibility). Counted for the op census.
                BAR_DISCARDS.fetch_add(1, Ordering::Relaxed);
            }
            // Discard cannot fail: there is nothing to move.
            PagingOpOutcome::Executed
        }
        PagingOperation::VirtualFill(fv) => {
            match unsafe { paging_alloc_info(adapter, fv.hAllocation) } {
                None => {
                    BAR_ERR_FILL_HANDLE.fetch_add(1, Ordering::Relaxed);
                    PagingOpOutcome::Failed(paging_failure())
                }
                Some(alloc) if !alloc.bar_eligible => {
                    BAR_DEVICE_OP_SKIPS.fetch_add(1, Ordering::Relaxed);
                    PagingOpOutcome::NotOurs
                }
                Some(alloc) => {
                    let off = fv.AllocationOffsetInBytes;
                    let fill_len = match pg::clamp_range(alloc.size, off, fv.FillSizeInBytes) {
                        Clamp::Full(n) => n,
                        Clamp::Clamped(n) => {
                            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                            n
                        }
                        Clamp::Nothing => {
                            BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                            0
                        }
                    };
                    let pattern = fv.FillPattern;
                    let system_backing = content_guard.snapshot(alloc.resource_id);
                    if system_backing.is_some() {
                        BAR_VIRTUAL_FILL_SYSTEM.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut filled = false;
                    let ok = unsafe {
                        with_blob_bytes(passive, adapter, alloc.resource_id, |dst, len| {
                            // Cut to the MAPPED blob (a blob shorter than the
                            // recorded allocation fills its prefix, not nothing).
                            let n = match pg::clamp_to_mapped(off, fill_len, len) {
                                Clamp::Full(n) => n,
                                Clamp::Clamped(n) => {
                                    BAR_CLAMPED.fetch_add(1, Ordering::Relaxed);
                                    n
                                }
                                Clamp::Nothing => {
                                    BAR_ERR_BOUNDS.fetch_add(1, Ordering::Relaxed);
                                    return;
                                }
                            };
                            // SAFETY: bounds-checked against the blob mapping.
                            fill_pattern(dst.add(off as usize), n as usize, pattern);
                            // When this allocation is system-resident, keep the
                            // intersecting owned backing ranges authoritative as
                            // part of the same serialized content transaction.
                            filled = system_backing
                                .as_ref()
                                .is_none_or(|backing| backing.copy_blob_range(dst, len, off, n));
                        })
                    };
                    if ok && filled {
                        BAR_FILLS.fetch_add(1, Ordering::Relaxed);
                        PagingOpOutcome::Executed
                    } else {
                        // PgEm / PgEb already counted.
                        PagingOpOutcome::Failed(paging_failure())
                    }
                }
            }
        }
        PagingOperation::VirtualTransfer(tv) => {
            if unsafe { bar_virtual_transfer(passive, adapter, &content_guard, tv) } {
                PagingOpOutcome::Executed
            } else {
                // Was STATUS_UNSUCCESSFUL — the crate's last use of a status two
                // sibling DDIs carry comments about dxgkrnl logging as "Driver
                // returned an invalid NTSTATUS" 197x with adapter resets.
                PagingOpOutcome::Failed(paging_failure())
            }
        }
        // Exhaustive: `is_content_op` already returned for these, so reaching
        // them here is impossible. Named rather than wildcarded so a new variant
        // is a compile error in BOTH places at once.
        PagingOperation::UpdatePageTable(_) | PagingOperation::Other => PagingOpOutcome::NotOurs,
    };
    // Counted BEFORE the mirror below, so the registry carries it on this op.
    if matches!(outcome, PagingOpOutcome::Failed(_)) {
        BAR_SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    note.result = match outcome {
        PagingOpOutcome::Executed => dl::paging_result::EXECUTED,
        PagingOpOutcome::NotOurs => dl::paging_result::NOT_OURS,
        PagingOpOutcome::Failed(_) => dl::paging_result::SKIPPED,
    };
    // Registry diagnostics can block independently; the backing transaction is
    // complete, so do not unnecessarily serialize another Present behind it.
    drop(content_guard);
    dump_bar_counters(passive);
    match outcome {
        // `reason` is `paging_failure()` at every producer: STATUS_SUCCESS.
        PagingOpOutcome::Failed(reason) => reason,
        PagingOpOutcome::Executed | PagingOpOutcome::NotOurs => STATUS_SUCCESS,
    }
}

// ── GpuMmu root page-table DDIs. ─────────────────────────────────────────────

/// `DxgkDdiSetRootPageTable` — bind a context's root page table. Decorative:
/// the root address names a (never-walked) guest page table, so we record the
/// call for the debugger and ignore it.
pub unsafe extern "C" fn dxgkddi_set_root_page_table(
    _h_adapter: IN_CONST_HANDLE,
    set_root_page_table: IN_CONST_PDXGKARG_SETROOTPAGETABLE,
) {
    SET_ROOT_PT_COUNT.fetch_add(1, Ordering::Relaxed);
    if !set_root_page_table.is_null() {
        // SAFETY: non-null checked; read-only access to the args struct.
        let args = unsafe { &*set_root_page_table };
        // Pack NumEntries (low 32) so the debugger can confirm VidMm drives the
        // declared geometry. (Address is decorative; the host never walks it.)
        SET_ROOT_PT_LAST.store(args.NumEntries as u64, Ordering::Relaxed);
    }
}

/// `DxgkDdiGetRootPageTableSize` — report the byte size of a root page table that
/// must address `NumberOfPte` entries. Must be consistent with the declared PTE
/// size in `ddi::gpummu`, or VidMm carves a mis-sized root page table.
pub unsafe extern "C" fn dxgkddi_get_root_page_table_size(
    _h_adapter: IN_CONST_HANDLE,
    get_root_page_table_size: INOUT_PDXGKARG_GETROOTPAGETABLESIZE,
) -> SIZE_T {
    GET_ROOT_PT_SIZE_COUNT.fetch_add(1, Ordering::Relaxed);
    if get_root_page_table_size.is_null() {
        return 0;
    }
    // SAFETY: non-null checked; NumberOfPte is the input count.
    let args = unsafe { &*get_root_page_table_size };
    let num_pte = args.NumberOfPte;
    let bytes = super::gpummu::root_page_table_size_bytes(num_pte);
    GET_ROOT_PT_SIZE_LAST.store(
        ((num_pte as u64) << 32) | (bytes as u64 & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );
    bytes
}
