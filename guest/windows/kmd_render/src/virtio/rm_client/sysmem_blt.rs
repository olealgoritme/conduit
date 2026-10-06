//! The CPU-copy fallback for a Blt Present whose destination is the level 5 RM system-memory
//! primary (`KmdRmClient` = 5). Design: `docs/kmd-rm-client.md` section 15.17; the decisions
//! (rect clipping, the row-copy plan, the map windows, the byte order, the accounting) are
//! `helios_kmd_logic::rm_blt`, host-tested; this file performs them.
//!
//! WHY. The KMD's own Blt arm treats a STANDARD destination as a registered Present buffer
//! (`begin_present_buffer_write_legacy`) and GPU-copies into the Venus buffer behind it. The RM
//! primary has none (it is RM memory adopted as a foreign record: `venus_image_id` and
//! `venus_memory_id` are 0), so that arm would FAIL the Present with `STATUS_DEVICE_NOT_READY`.
//! This arm answers every such Present with success:
//!
//! * source = a Venus-backed image (an OPTIMAL D3D11/DXVK image, a cross-context GDI texture, a
//!   foreign NVK-on-RM image): GPU-copy it into the Venus client's private LINEAR staging image
//!   (`VenusClient::rm_blt_copy_to_stage`), wait for the copy's wire fence, then copy ONLY the
//!   destination rect(s) row by row from the staging image's guest mapping into the primary's;
//! * source = a CPU-visible allocation (a pitched STANDARD blob): read it through its own blob
//!   mapping, rect by rect;
//! * anything else, or any internal failure: count it (`RmSysBltSkip`, last reason
//!   `RmSysBltWhy`) and return success. A skipped frame is a stale picture, never a failed
//!   Present. Only an unreadable source (no allocation behind the handle) is refused, by the
//!   caller, as before.
//!
//! MAPPINGS. Both views are transient `MmMapIoSpace` windows over `map_blob_prepare`'s guest
//! physical range, each at most a few MiB (one band of rows), made and unmapped per band, with
//! the cache attribute of the host's `map_info` for that blob (`map_cache_to_mm`): for the
//! primary that is the attribute it was created and trial-mapped with (15.5), never another, so
//! no alias is made. `map_blob_prepare` is idempotent: the primary is usually already mapped at
//! the offset dxgkrnl's aperture uses, and the same pages are viewed.
//!
//! LOCKING, IRQL. PASSIVE (`DxgkDdiPresent` is documented PASSIVE_LEVEL). No spinlock is held
//! across the GPU wait or a row copy. The Venus mutex is taken (alone, as the existing legacy
//! Blt arm does) only to submit the GPU copy; the wait and the CPU copy run with no lock. The
//! one thing held across them is `STAGE_BUSY`, an atomic flag that serializes users of the
//! staging image (two Presents on two contexts); a Present that finds it taken sleeps in 1 ms
//! slices for at most [`BUSY_WAIT_100NS`] and then skips.
//!
//! COST. A Present copies its rects, not the frame, but the GPU copy into the staging image is
//! the whole source image (the reusable command the scanout copy records): GPU time, not CPU.
//! At the host-measured write-combined speeds (about 75 MB/s read, 28 MB/s write) a full
//! 5120x1440 frame (29,491,200 bytes) is about 0.4 s to read and 1 s to write; a 400x40 text
//! line is 64,000 bytes.

use super::sysmem_level_on;
use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::{PresentAllocInfo, PresentAllocationStorage};
use crate::irql::PassiveLevel;
use crate::virtio::ctrl::{self, WaitFenceOutcome};
use crate::virtio::gpu::{BlobMapPrep, OwnerFilter};
use crate::virtio::venus::{foreign_source_if_enabled, OptimalPresentImageDesc, RmBltStage};
use crate::virtio::VirtioError;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::foreign_resource::Layout as FrLayout;
use helios_kmd_logic::rm_blt::{self as rb, MapWindow, Order, Plan, Rect, Skip, Surface, Swizzle};
use helios_protocol::{HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY, HELIOS_WDDM_ALLOC_KIND_STANDARD};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::PHYSICAL_ADDRESS;

/// How long the GPU copy into the staging image may take (the existing Blt arm's wait).
const GPU_WAIT_NS: u64 = 5_000_000_000;
/// How long a Present waits for another fallback Present to finish with the staging image.
const BUSY_WAIT_100NS: u64 = 20_000_000;
/// Attempts at one `MmMapIoSpace` (low system PTEs clears by itself).
const MAP_ATTEMPTS: u32 = 3;
// Counters (names at most 14 characters). `RmSysBltCpu` Presents the fallback copied (also
// those that copied nothing: a rect outside the primary), `RmSysBltBytes` cumulative MiB of
// pixels written to the primary, `RmSysBltUs` cumulative microseconds of the whole fallback
// (wait for the GPU copy included) over those Presents, `RmSysBltMaxUs` the slowest one,
// `RmSysBltSkip` Presents answered with success without a (complete) copy, `RmSysBltWhy` the
// reason of the last (`rm_blt::Skip::code`). Written once the fallback has run.
static BLT_CPU: AtomicU32 = AtomicU32::new(0);
static BLT_BYTES: AtomicU64 = AtomicU64::new(0);
static BLT_US: AtomicU64 = AtomicU64::new(0);
static BLT_MAX_US: AtomicU32 = AtomicU32::new(0);
static BLT_SKIP: AtomicU32 = AtomicU32::new(0);
static BLT_WHY: AtomicU32 = AtomicU32::new(0);

/// Whoever holds it owns the staging image from its GPU copy to the end of the CPU read.
static STAGE_BUSY: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the registry. PASSIVE only; nothing is written until the fallback ran.
pub(super) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if BLT_CPU.load(Ordering::Relaxed) == 0 && BLT_SKIP.load(Ordering::Relaxed) == 0 {
        return;
    }
    rec(b"RmSysBltCpu", BLT_CPU.load(Ordering::Relaxed));
    rec(b"RmSysBltBytes", rb::mib(BLT_BYTES.load(Ordering::Relaxed)));
    rec(b"RmSysBltUs", rb::sat32(BLT_US.load(Ordering::Relaxed)));
    rec(b"RmSysBltMaxUs", BLT_MAX_US.load(Ordering::Relaxed));
    rec(b"RmSysBltSkip", BLT_SKIP.load(Ordering::Relaxed));
    rec(b"RmSysBltWhy", BLT_WHY.load(Ordering::Relaxed));
}

/// The RM system-memory primary a Blt writes: its resource and the layout of its record.
#[derive(Clone, Copy)]
pub(crate) struct Primary {
    pub(crate) resource_id: u32,
    layout: FrLayout,
    size: u64,
}

/// `Some` iff `resource_id` is an adopted RM system-memory primary and level 5 is in force: one
/// relaxed load with the knob below 5, and the foreign table's lookup (a short scan under the
/// transport spinlock, no allocation) with it on.
pub(crate) fn primary(adapter: &AdapterContext, resource_id: u32) -> Option<Primary> {
    if !sysmem_level_on() {
        return None;
    }
    match adapter.with_virtio(|v| v.foreign_sysmem_source(resource_id)) {
        Ok(Some((_drm, _gem, layout, size))) => Some(Primary {
            resource_id,
            layout,
            size,
        }),
        _ => None,
    }
}

/// What the Present reads.
#[derive(Clone, Copy)]
pub(crate) enum Source {
    /// A Venus-backed image of `width` x `height` pixels: copied on the GPU into the staging
    /// image first.
    Image {
        desc: OptimalPresentImageDesc,
        width: u32,
        height: u32,
    },
    /// A CPU-visible allocation: read from its blob mapping.
    Cpu {
        resource_id: u32,
        surface: Surface,
        order: Order,
    },
}

/// Which arm the Present's source takes, from what its allocation says. `Err` is a Present the
/// fallback will skip (counted by the caller through [`skipped`]).
pub(crate) fn classify(
    adapter: &AdapterContext,
    source: &PresentAllocInfo,
    snapshot: bool,
) -> Result<Source, Skip> {
    if snapshot {
        return Err(Skip::Snapshot);
    }
    let Some(dxgi) = source.resolved_dxgi_format() else {
        return Err(Skip::SourceFormat);
    };
    if source.kind != HELIOS_WDDM_ALLOC_KIND_DEVICE_MEMORY
        && source.kind != HELIOS_WDDM_ALLOC_KIND_STANDARD
    {
        return Err(Skip::SourceKind);
    }
    // An adopted foreign (NVK-on-RM) resource: an explicit-modifier dma-buf image, exactly as the
    // existing Blt arm imports it.
    if let Some(foreign) =
        foreign_source_if_enabled(adapter, source.foreign, source.venus_alloc_size)
    {
        return OptimalPresentImageDesc::new_foreign_dma_buf(
            source.resource_id,
            source.width,
            source.height,
            dxgi,
            foreign,
        )
        .map(|desc| image(desc, source))
        .ok_or(Skip::SourceFormat);
    }
    match source.storage {
        PresentAllocationStorage::OptimalCrossContextImage => {
            OptimalPresentImageDesc::new_cross_context_dma_buf(
                source.resource_id,
                source.venus_alloc_size,
                source.memory_type_index,
                source.width,
                source.height,
                source.bind_flags,
                dxgi,
            )
            .map(|desc| image(desc, source))
            .ok_or(Skip::SourceFormat)
        }
        PresentAllocationStorage::OptimalOpaqueFdImage => OptimalPresentImageDesc::new_opaque_fd(
            source.resource_id,
            source.venus_alloc_size,
            source.memory_type_index,
            source.width,
            source.height,
            source.bind_flags,
            dxgi,
        )
        .map(|desc| image(desc, source))
        .ok_or(Skip::SourceFormat),
        PresentAllocationStorage::PitchedStandardBuffer => {
            // A pitched STANDARD blob: only a CPU-visible allocation the KMD itself made has a
            // row layout. A UMD image never carries this storage.
            if source.kind != HELIOS_WDDM_ALLOC_KIND_STANDARD {
                return Err(Skip::SourceKind);
            }
            let Some(order) = rb::order_for_dxgi(dxgi) else {
                return Err(Skip::SourceFormat);
            };
            let surface = Surface::new(
                source.width,
                source.height,
                source.pitch,
                source.plane_offset,
                source.venus_alloc_size,
            );
            if !surface.valid() {
                return Err(Skip::SourceKind);
            }
            Ok(Source::Cpu {
                resource_id: source.resource_id,
                surface,
                order,
            })
        }
    }
}

fn image(desc: OptimalPresentImageDesc, source: &PresentAllocInfo) -> Source {
    Source::Image {
        desc,
        width: source.width,
        height: source.height,
    }
}

/// What a fallback Present did.
pub(crate) struct Done {
    /// `None`: the Present was copied (or had nothing to copy). `Some`: skipped, and why.
    pub(crate) skipped: Option<Skip>,
    /// Bytes of pixels written to the primary (also when the copy stopped half way).
    pub(crate) wrote: u64,
    /// The wire fence of the GPU copy, already complete; the caller may merge it into the DMA
    /// private data (harmless) like the existing arm does.
    pub(crate) fence: Option<u64>,
}

/// Count a Present that was skipped before [`present`] (the caller's own refusal reasons).
pub(crate) fn skipped(why: Skip) -> Done {
    note_skip(why);
    Done {
        skipped: Some(why),
        wrote: 0,
        fence: None,
    }
}

fn note_skip(why: Skip) {
    let n = BLT_SKIP.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    BLT_WHY.store(why.code(), Ordering::Relaxed);
    // The first and every 64th skip reach the registry at once; the counters mirror the rest.
    if n == 1 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"RmSysBltWhy", why.code());
        crate::diag::record_named_bytes(b"RmSysBltSkip", n);
    }
}

/// The staging image's owner for the duration of one Present.
struct StageGuard;

impl Drop for StageGuard {
    fn drop(&mut self) {
        STAGE_BUSY.store(0, Ordering::Release);
    }
}

fn take_stage(passive: PassiveLevel) -> Option<StageGuard> {
    let start = now();
    loop {
        if STAGE_BUSY
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(StageGuard);
        }
        if now().wrapping_sub(start) >= BUSY_WAIT_100NS {
            return None;
        }
        ctrl::sleep_ms(passive, 1);
    }
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// Copy the Present's rect(s) into the RM primary. PASSIVE. Never fails: every internal failure
/// is a [`Done::skipped`].
///
/// `subs` yields the Present's `pDstSubRects` (read lazily, at most `SUB_RECT_SCAN_MAX`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn present<I: Iterator<Item = Rect> + Clone>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    primary: &Primary,
    source: Source,
    dst_rect: Rect,
    src_rect: Rect,
    sub_count: u32,
    subs: I,
) -> Done {
    let started = now();
    let mut wrote = 0u64;
    let mut fence = None;
    let result = run(
        passive, adapter, primary, source, dst_rect, src_rect, sub_count, subs, &mut wrote,
        &mut fence,
    );
    BLT_BYTES.fetch_add(wrote, Ordering::Relaxed);
    match result {
        Ok(()) => {
            let us = rb::micros(now().wrapping_sub(started));
            BLT_CPU.fetch_add(1, Ordering::Relaxed);
            BLT_US.fetch_add(us, Ordering::Relaxed);
            BLT_MAX_US.fetch_max(rb::sat32(us), Ordering::Relaxed);
            Done {
                skipped: None,
                wrote,
                fence,
            }
        }
        Err(why) => {
            note_skip(why);
            Done {
                skipped: Some(why),
                wrote,
                fence,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run<I: Iterator<Item = Rect> + Clone>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    primary: &Primary,
    source: Source,
    dst_rect: Rect,
    src_rect: Rect,
    sub_count: u32,
    subs: I,
    wrote: &mut u64,
    fence_out: &mut Option<u64>,
) -> Result<(), Skip> {
    let Some(dst_order) = rb::order_for_fourcc(primary.layout.fourcc) else {
        return Err(Skip::Layout);
    };
    let dst = Surface::new(
        primary.layout.width,
        primary.layout.height,
        primary.layout.stride,
        u64::from(primary.layout.offset),
        primary.size,
    );
    if let Source::Cpu { resource_id, .. } = source {
        if resource_id == primary.resource_id {
            return Err(Skip::SameResource);
        }
    }
    if let Source::Image { desc, .. } = source {
        if desc.resource_id() == primary.resource_id {
            return Err(Skip::SameResource);
        }
    }

    // A Present with nothing to copy (every rect empty or outside the primary) costs no GPU
    // copy: the plan's emptiness depends on the extents only, so a tightly packed stand-in for
    // the staging image decides it.
    if let Source::Image { width, height, .. } = source {
        let row = width.saturating_mul(rb::BPP);
        let probe = Surface::new(
            width,
            height,
            row,
            0,
            u64::from(row).saturating_mul(u64::from(height)),
        );
        if rb::plan(&dst, &probe, dst_rect, src_rect, sub_count, subs.clone())?.is_empty() {
            return Ok(());
        }
    }

    // The staging image is held from the GPU copy to the end of the CPU read (image sources
    // only); the guard releases it on every return.
    let mut _stage_guard = None;
    let (src, src_resource, src_order) = match source {
        Source::Image {
            desc,
            width,
            height,
        } => {
            let Some(guard) = take_stage(passive) else {
                return Err(Skip::Busy);
            };
            _stage_guard = Some(guard);
            let (stage, fence) = gpu_copy(passive, adapter, desc)?;
            *fence_out = Some(fence);
            (
                Surface::new(
                    width,
                    height,
                    stage.row_pitch,
                    u64::from(stage.plane_offset),
                    stage.size,
                ),
                stage.resource_id,
                Order::Bgra,
            )
        }
        Source::Cpu {
            resource_id,
            surface,
            order,
        } => (surface, resource_id, order),
    };

    let plan = rb::plan(&dst, &src, dst_rect, src_rect, sub_count, subs)?;
    if plan.is_empty() {
        return Ok(());
    }
    let swizzle = rb::swizzle(src_order, dst_order);

    let src_prep = ctrl::map_blob_prepare(passive, adapter, OwnerFilter::Any, src_resource)
        .map_err(|_| Skip::MapSrc)?;
    let dst_prep = ctrl::map_blob_prepare(passive, adapter, OwnerFilter::Any, primary.resource_id)
        .map_err(|_| Skip::MapDst)?;
    // SAFETY: PASSIVE; both ranges were RESOURCE_MAP_BLOB'd into the host-visible window by
    // `map_blob_prepare`, and `plan` was proved against both surfaces; the views are made and
    // unmapped inside.
    unsafe {
        copy_plan(
            passive, &plan, &dst, &src, &dst_prep, &src_prep, swizzle, wrote,
        )
    }
}

/// The GPU half: submit the copy of `desc` into the staging image under the Venus mutex (alone,
/// like the legacy Blt arm), then wait for its wire fence with no lock held.
fn gpu_copy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    desc: OptimalPresentImageDesc,
) -> Result<(RmBltStage, u64), Skip> {
    let submitted =
        adapter.with_venus_client(passive, |client| client.rm_blt_copy_to_stage(adapter, desc));
    let (stage, fence) = match submitted {
        Ok(Ok(done)) => done,
        Ok(Err(VirtioError::OutOfMemory)) => return Err(Skip::Stage),
        Ok(Err(_)) => return Err(Skip::GpuCopy),
        Err(_) => return Err(Skip::NoVenus),
    };
    match ctrl::wait_fence(passive, adapter, fence, GPU_WAIT_NS) {
        WaitFenceOutcome::Complete => Ok((stage, fence)),
        WaitFenceOutcome::TimedOut | WaitFenceOutcome::Invalid => Err(Skip::GpuWait),
    }
}

/// A transient kernel mapping of one window of a blob.
struct View {
    va: *mut u8,
    len: u64,
}

impl View {
    /// # Safety
    /// PASSIVE; `prep` describes a blob range mapped into the host-visible window and `win`
    /// lies inside it (`rb::map_window` proved that against `prep.size`).
    unsafe fn map(passive: PassiveLevel, prep: &BlobMapPrep, win: &MapWindow) -> Option<View> {
        let cache = crate::ddi::map_cache_to_mm(prep.map_cache);
        for attempt in 0..MAP_ATTEMPTS {
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = (prep.gpa + win.start) as i64;
            // SAFETY: per the contract; the cache attribute is the host's `MAP_INFO` for this
            // blob, so no view of these pages has another one.
            let va = unsafe { MmMapIoSpace(pa, win.len, cache) } as *mut u8;
            if !va.is_null() {
                return Some(View { va, len: win.len });
            }
            if attempt + 1 < MAP_ATTEMPTS {
                ctrl::sleep_ms(passive, 1);
            }
        }
        None
    }

    fn unmap(self) {
        // SAFETY: the exact mapping `map` made.
        unsafe { MmUnmapIoSpace(self.va as *mut core::ffi::c_void, self.len) };
    }
}

/// Copy every rect of `plan`, band by band: map the source and destination windows of the band,
/// copy its rows, drain this core's write-combining buffers, unmap. `wrote` counts the pixel
/// bytes of every finished band (so a failure half way still accounts what it wrote).
///
/// # Safety
/// As [`View::map`] for both preps; `plan` was made by `rb::plan(dst, src, ..)` for these
/// surfaces.
#[allow(clippy::too_many_arguments)]
unsafe fn copy_plan(
    passive: PassiveLevel,
    plan: &Plan,
    dst: &Surface,
    src: &Surface,
    dst_prep: &BlobMapPrep,
    src_prep: &BlobMapPrep,
    swizzle: Swizzle,
    wrote: &mut u64,
) -> Result<(), Skip> {
    for rc in plan.rects() {
        for (y0, y1) in rb::bands(rc, dst, src) {
            let (Some((d_first, d_end)), Some((s_first, s_end))) =
                (rc.dst_span(dst, y0, y1), rc.src_span(src, y0, y1))
            else {
                return Err(Skip::Plan);
            };
            let Some(dwin) = rb::map_window(d_first, d_end, dst_prep.size) else {
                return Err(Skip::MapDst);
            };
            let Some(swin) = rb::map_window(s_first, s_end, src_prep.size) else {
                return Err(Skip::MapSrc);
            };
            // SAFETY: per the contract; windows proved inside the mappings above.
            let Some(sv) = (unsafe { View::map(passive, src_prep, &swin) }) else {
                return Err(Skip::MapSrc);
            };
            // SAFETY: as above.
            let Some(dv) = (unsafe { View::map(passive, dst_prep, &dwin) }) else {
                sv.unmap();
                return Err(Skip::MapDst);
            };
            let n = rc.row_bytes() as usize;
            for y in y0..y1 {
                let so = (rc.src_row(src, y) - swin.start) as usize;
                let d = (rc.dst_row(dst, y) - dwin.start) as usize;
                // SAFETY: row `y` of the band lies inside both windows (`dst_span` /
                // `src_span` of the band, `map_window` rounds outward); the two views are
                // different blobs' pages.
                unsafe { copy_row(sv.va.add(so), dv.va.add(d), n, swizzle) };
            }
            // SAFETY: SSE2 is baseline on x86_64. The write-combined stores of THIS core are
            // drained before the view goes away and before any re-flip is asked for.
            unsafe { core::arch::x86_64::_mm_sfence() };
            dv.unmap();
            sv.unmap();
            *wrote = wrote.saturating_add(u64::from(y1 - y0) * rc.row_bytes());
        }
    }
    Ok(())
}

/// One row.
///
/// # Safety
/// `n` bytes readable at `src` and writable at `dst`, not overlapping.
unsafe fn copy_row(src: *const u8, dst: *mut u8, n: usize, swizzle: Swizzle) {
    match swizzle {
        // SAFETY: forwarded.
        Swizzle::None => unsafe { crate::virtio::rm_present::copy_row(src, dst, n) },
        Swizzle::SwapRb => {
            // SAFETY: forwarded; the two slices are distinct mappings.
            let (s, d) = unsafe {
                (
                    core::slice::from_raw_parts(src, n),
                    core::slice::from_raw_parts_mut(dst, n),
                )
            };
            rb::swap_rb_row(d, s);
        }
    }
}
