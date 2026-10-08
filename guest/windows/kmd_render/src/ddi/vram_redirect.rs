//! The Present side of `RedirVram` (stages V3-V5 of `docs/vram-redirection.md`): every Blt that
//! has a KMD RM video-memory surface on either side. The allocation service is
//! `virtio/rm_client/vidmem.rs`, the objects in the copy-engine channel `virtio/rm_client/ce_vram.rs`,
//! the pure rules `helios_kmd_logic::rm_vidmem`.
//!
//! The Blt arm asks [`blt`] right after it resolved both allocations and their formats, before any
//! descriptor is built. With the knob off, or no VRAM surface alive, that is one relaxed load and
//! `None`: the Blt continues exactly as before. Otherwise, by the two sides:
//!
//! | source | destination | what runs | the Present |
//! |---|---|---|---|
//! | NVK-on-RM image (foreign) | VRAM surface | the copy-engine route, VRAM to VRAM, GPU-ordered on the producer's semaphore (`ce_present_route::try_route_vram`) | completes as a deferred copy: its DMA fence retires on the CE completion |
//! | VRAM surface | VRAM surface | one CE copy, waited for here (`ce_vram::copy`) | completes now, nothing pending |
//! | CPU-visible standard buffer (GDI's staging, a shadow) | VRAM surface | the UPLOAD: the buffer's authoritative CPU view through the bounce into the surface (`ce_vram::transfer`) | completes now |
//! | VRAM surface | CPU-visible standard buffer | the READBACK for CPU readers (a staging copy, PrintWindow, capture): the surface through the bounce into the buffer's blob and its system pages | completes now |
//!
//! Anything that cannot run (the channel down or busy, a refused route, a shape the copy refuses,
//! differing formats) answers the Present with a counted success and no copy (`RvBltSkip`,
//! `RvBltWhy`): the destination keeps its previous content, as a foreign skip does. A GPU-only
//! surface is never CPU-mapped (it is not `CpuVisible`), so these Blts are the only way bytes reach
//! it or leave it.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use helios_kmd_logic::ce_present::Remap;
use helios_kmd_logic::rm_vidmem::{Dir, Rect};

use crate::adapter::AdapterContext;
use crate::ddi::create_allocation::{PresentAllocInfo, PresentAllocationStorage};
use crate::device::ContextHandleRef;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::virtio::rm_client::{ce_vram, vidmem};
use crate::virtio::venus::{OptimalPresentImageDesc, PresentDestinationDesc};

/// What the Blt arm does with the Present.
pub(crate) enum Outcome {
    /// Queued for the copy engine as a deferred copy (its token is in the private record): complete
    /// it as such (`present_complete`).
    Routed(u64),
    /// Copied (synchronously) or skipped (counted): complete it with nothing pending
    /// (`present_blt_skipped`).
    Done,
}

/// Why a VRAM Blt did nothing (`RvBltWhy`). Never renumbered.
#[derive(Clone, Copy)]
#[repr(u32)]
enum Why {
    RouteRefused = 1,
    NoForeignSource = 2,
    NoDestinationDesc = 3,
    Format = 4,
    Rect = 5,
    ChannelBusy = 6,
    Copy = 7,
    Read = 8,
    Write = 9,
    Memory = 10,
    UnknownSource = 11,
    Disabled = 12,
}

static SEEN: AtomicU32 = AtomicU32::new(0);
static ROUTED: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static READBACK: AtomicU32 = AtomicU32::new(0);
static UPLOAD: AtomicU32 = AtomicU32::new(0);
static GDI_FAIL: AtomicU32 = AtomicU32::new(0);
/// Foreign frames the route refused, copied synchronously instead (`RvBltSync`).
static SYNC: AtomicU32 = AtomicU32::new(0);
/// The route's own reason for the last refusal (`CeRtWhy` at that moment, `RvRtWhy`).
static RT_WHY: AtomicU32 = AtomicU32::new(0);
/// Synchronous copies tried (`RvSyncTry`) and the last failure: step << 24 | `fail_word` low 24
/// bits (`RvSyncWhy`; step 1 foreign_source, 2 the destination's mapping, 3 the copy, 4 the wait).
static SYNC_TRY: AtomicU32 = AtomicU32::new(0);
static SYNC_WHY: AtomicU32 = AtomicU32::new(0);
/// Every Present's marker stash as `DxgkDdiPresent` found it (`RvMkNone`: empty, `RvMkRes`: an
/// RM fence attached at Render, `RvMkStr`: a stream point), and VRAM Blts that reached the route
/// with no boundary (`RvMkVramNo`).
/// Every 64th synchronous copy, the pixel at the centre of its destination rectangle read back
/// after the copy (`RvSyPix`, the last one), how many were sampled (`RvSyPixN`) and how many were
/// not 0 (`RvSyPixNz`): whether the copy wrote the window's content or zeros.
static PIX: AtomicU32 = AtomicU32::new(0);
static PIX_N: AtomicU32 = AtomicU32::new(0);
static PIX_NZ: AtomicU32 = AtomicU32::new(0);
/// The sampled copy's destination and source resource ids (`RvSyDst`, `RvSySrc`) and its
/// destination rectangle's size (`RvSyWH`, width << 16 | height): to compare with what DWM
/// imports for the window (its umd log).
static PIX_DST: AtomicU32 = AtomicU32::new(0);
static PIX_SRC: AtomicU32 = AtomicU32::new(0);
static PIX_WH: AtomicU32 = AtomicU32::new(0);
static MK_NONE: AtomicU32 = AtomicU32::new(0);
static MK_RES: AtomicU32 = AtomicU32::new(0);
static MK_STR: AtomicU32 = AtomicU32::new(0);
static MK_VRAM_NO: AtomicU32 = AtomicU32::new(0);

/// One timed stage: summed microseconds, count, maximum (`<name>Us`, `<name>N`, `<name>Max`).
struct Stage {
    us: AtomicU32,
    n: AtomicU32,
    max: AtomicU32,
}
impl Stage {
    const fn new() -> Self {
        Self {
            us: AtomicU32::new(0),
            n: AtomicU32::new(0),
            max: AtomicU32::new(0),
        }
    }
    fn add_since(&self, t0: u64) {
        let us = (crate::ddi::blt_async::now_100ns().saturating_sub(t0) / 10)
            .min(u64::from(u32::MAX)) as u32;
        self.us.fetch_add(us, Ordering::Relaxed);
        self.n.fetch_add(1, Ordering::Relaxed);
        self.max.fetch_max(us, Ordering::Relaxed);
    }
    fn reset(&self) {
        self.us.store(0, Ordering::Relaxed);
        self.n.store(0, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
    }
    fn publish(&self, us: &[u8], n: &[u8], max: &[u8]) {
        use crate::diag::record_named_bytes as rec;
        rec(us, self.us.load(Ordering::Relaxed));
        rec(n, self.n.load(Ordering::Relaxed));
        rec(max, self.max.load(Ordering::Relaxed));
    }
}
/// Per foreign Present into a VRAM surface: the whole hook (`RvPr*`), the route call (`RvRt*`),
/// and the synchronous copy's steps: foreign_source (`RvSyFs*`), the destination's mapping
/// (`RvSyDs*`), the copy's submit (`RvSySub*`), the wait (`RvSyWt*`).
static T_PR: Stage = Stage::new();
static T_RT: Stage = Stage::new();
static T_FS: Stage = Stage::new();
static T_DS: Stage = Stage::new();
static T_SUB: Stage = Stage::new();
static T_WT: Stage = Stage::new();

/// Which VRAM surfaces the Present path wrote: up to 8 distinct destinations, `RvDst0..7` =
/// resource id << 12 | writes (saturating at 0xfff), `RvDstMore`: writes to further ones. The
/// route's queued copies count when queued, the others when done.
const DST_LEDGER: usize = 8;
static DST_RES: [AtomicU32; DST_LEDGER] = [const { AtomicU32::new(0) }; DST_LEDGER];
static DST_CNT: [AtomicU32; DST_LEDGER] = [const { AtomicU32::new(0) }; DST_LEDGER];
static DST_MORE: AtomicU32 = AtomicU32::new(0);

fn note_dst(resource_id: u32) {
    for i in 0..DST_LEDGER {
        let r = DST_RES[i].load(Ordering::Relaxed);
        if r == resource_id {
            DST_CNT[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
        if r == 0 {
            if DST_RES[i]
                .compare_exchange(0, resource_id, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
                || DST_RES[i].load(Ordering::Relaxed) == resource_id
            {
                DST_CNT[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }
    DST_MORE.fetch_add(1, Ordering::Relaxed);
}

/// The Present's stash (0 none, 1 resolved RM fence, 2 stream point). Any IRQL.
pub(crate) fn note_marker(kind: u32) {
    match kind {
        1 => &MK_RES,
        2 => &MK_STR,
        _ => &MK_NONE,
    }
    .fetch_add(1, Ordering::Relaxed);
}

/// StartDevice (PASSIVE): zero the counters (written only once a VRAM Blt was seen).
pub(crate) fn reset_for_start() {
    for c in [
        &SEEN,
        &ROUTED,
        &SKIP,
        &WHY,
        &READBACK,
        &UPLOAD,
        &GDI_FAIL,
        &SYNC,
        &RT_WHY,
        &SYNC_TRY,
        &SYNC_WHY,
        &MK_NONE,
        &MK_RES,
        &MK_STR,
        &MK_VRAM_NO,
        &PIX,
        &PIX_N,
        &PIX_NZ,
        &PIX_DST,
        &PIX_SRC,
        &PIX_WH,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for t in [&T_PR, &T_RT, &T_FS, &T_DS, &T_SUB, &T_WT] {
        t.reset();
    }
    for i in 0..DST_LEDGER {
        DST_RES[i].store(0, Ordering::Relaxed);
        DST_CNT[i].store(0, Ordering::Relaxed);
    }
    DST_MORE.store(0, Ordering::Relaxed);
}

fn publish() {
    use crate::diag::record_named_bytes as rec;
    rec(b"RvBltSeen", SEEN.load(Ordering::Relaxed));
    rec(b"RvBltRoute", ROUTED.load(Ordering::Relaxed));
    rec(b"RvBltSkip", SKIP.load(Ordering::Relaxed));
    rec(b"RvBltWhy", WHY.load(Ordering::Relaxed));
    rec(b"RvRdBack", READBACK.load(Ordering::Relaxed));
    rec(b"RvUpload", UPLOAD.load(Ordering::Relaxed));
    rec(b"RvGdiFail", GDI_FAIL.load(Ordering::Relaxed));
    rec(b"RvBltSync", SYNC.load(Ordering::Relaxed));
    rec(b"RvRtWhy", RT_WHY.load(Ordering::Relaxed));
    rec(b"RvSyncTry", SYNC_TRY.load(Ordering::Relaxed));
    rec(b"RvSyncWhy", SYNC_WHY.load(Ordering::Relaxed));
    rec(b"RvSyPix", PIX.load(Ordering::Relaxed));
    rec(b"RvSyPixN", PIX_N.load(Ordering::Relaxed));
    rec(b"RvSyPixNz", PIX_NZ.load(Ordering::Relaxed));
    rec(b"RvSyDst", PIX_DST.load(Ordering::Relaxed));
    rec(b"RvSySrc", PIX_SRC.load(Ordering::Relaxed));
    rec(b"RvSyWH", PIX_WH.load(Ordering::Relaxed));
    rec(b"RvMkNone", MK_NONE.load(Ordering::Relaxed));
    rec(b"RvMkRes", MK_RES.load(Ordering::Relaxed));
    rec(b"RvMkStr", MK_STR.load(Ordering::Relaxed));
    rec(b"RvMkVramNo", MK_VRAM_NO.load(Ordering::Relaxed));
    for (i, name) in [b"RvDst0", b"RvDst1", b"RvDst2", b"RvDst3", b"RvDst4", b"RvDst5", b"RvDst6", b"RvDst7"]
        .iter()
        .enumerate()
    {
        let r = DST_RES[i].load(Ordering::Relaxed);
        if r != 0 {
            rec(*name, r << 12 | DST_CNT[i].load(Ordering::Relaxed).min(0xfff));
        }
    }
    rec(b"RvDstMore", DST_MORE.load(Ordering::Relaxed));
    T_PR.publish(b"RvPrUs", b"RvPrN", b"RvPrMax");
    T_RT.publish(b"RvRtUs", b"RvRtN", b"RvRtMax");
    T_FS.publish(b"RvSyFsUs", b"RvSyFsN", b"RvSyFsMax");
    T_DS.publish(b"RvSyDsUs", b"RvSyDsN", b"RvSyDsMax");
    T_SUB.publish(b"RvSySubUs", b"RvSySubN", b"RvSySubMax");
    T_WT.publish(b"RvSyWtUs", b"RvSyWtN", b"RvSyWtMax");
    vidmem::publish_counters();
}

/// Throttled publish: every 64th VRAM Blt, and every skip. With the mirror thread running, a
/// request for its `Nv*` pass (which calls [`publish_if_seen`]); a publish is some fifty registry
/// writes, far too much inline in a Present.
fn maybe_publish(force: bool) {
    if force || SEEN.load(Ordering::Relaxed) % 64 == 1 {
        if crate::ddi::mirror_thread::running() {
            crate::ddi::mirror_thread::request_bits(crate::ddi::mirror_thread::NV);
        } else {
            publish();
        }
    }
}

/// The `Nv*` mirror pass: everything [`publish`] writes, once a VRAM Blt was seen or the VRAM
/// service has objects.
pub(crate) fn publish_if_seen() {
    if SEEN.load(Ordering::Relaxed) != 0 || vidmem::any_live() {
        publish();
    }
}

fn skip(why: Why) -> Option<Outcome> {
    SKIP.fetch_add(1, Ordering::Relaxed);
    WHY.store(why as u32, Ordering::Relaxed);
    maybe_publish(true);
    Some(Outcome::Done)
}

/// The allocation behind `resource_id` is being destroyed (`vidmem::released`): the route forgets it
/// as a destination.
pub(crate) fn destination_gone(resource_id: u32) {
    crate::ddi::ce_present_route::vram_destination_gone(resource_id);
}

/// `SrcRect` and `DstRect` of the Present, as pixel rectangles of equal size inside both surfaces.
fn rects(
    args: &DXGKARG_PRESENT,
    src: &PresentAllocInfo,
    dst: &PresentAllocInfo,
) -> Option<(Rect, Rect)> {
    let conv = |l: i32, t: i32, ri: i32, b: i32, w: u32, h: u32| -> Option<Rect> {
        if l < 0 || t < 0 || ri <= l || b <= t || ri as u32 > w || b as u32 > h {
            return None;
        }
        Some(Rect {
            left: l as u32,
            top: t as u32,
            right: ri as u32,
            bottom: b as u32,
        })
    };
    let (sr, dr) = (&args.SrcRect, &args.DstRect);
    let s = conv(sr.left, sr.top, sr.right, sr.bottom, src.width, src.height)?;
    let d = conv(dr.left, dr.top, dr.right, dr.bottom, dst.width, dst.height)?;
    // No stretch: a stretched Blt is not this path's (it is skipped, counted).
    (s.right - s.left == d.right - d.left && s.bottom - s.top == d.bottom - d.top).then_some((s, d))
}

/// The Blt arm's hook. `None`: neither side is a VRAM surface (or the knob is off): continue as
/// before. PASSIVE (`DxgkDdiPresent`), no lock held.
///
/// # Safety
/// `args` is dxgkrnl's `DXGKARG_PRESENT` for this call, its private data validated by the Blt arm.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(crate) unsafe fn blt(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    context: Option<&ContextHandleRef<'_>>,
    source: &PresentAllocInfo,
    destination: &PresentAllocInfo,
    source_dxgi: u32,
    destination_dxgi: u32,
    boundary: Option<u64>,
) -> Option<Outcome> {
    if !vidmem::any_live() {
        return None;
    }
    let dst_vram = vidmem::lookup(destination.resource_id);
    let src_vram = vidmem::lookup(source.resource_id);
    if dst_vram.is_none() && src_vram.is_none() {
        return None;
    }
    SEEN.fetch_add(1, Ordering::Relaxed);
    if boundary.is_none() {
        MK_VRAM_NO.fetch_add(1, Ordering::Relaxed);
    }
    if vidmem::off(helios_kmd_logic::rm_vidmem::off::PRESENT_HOOK) {
        return skip(Why::Disabled);
    }
    let Some((src_rect, dst_rect)) = rects(args, source, destination) else {
        return skip(Why::Rect);
    };
    match (src_vram, dst_vram) {
        // A KMD standard buffer carries the foreign layout trailer too when it is RM system memory
        // (a GDI staging buffer, `RedirVram`), but it is not an NVK frame: it takes the UPLOAD arm
        // below. 371.1: every staging buffer RM-backed, RvUpload 0, RvBltSkip 9788, and the boot
        // desktop black (CDD's Present Blts of it went to the foreign arm and were refused).
        (_, Some(dst))
            if source.foreign.is_some()
                && src_vram.is_none()
                && source.storage != PresentAllocationStorage::PitchedStandardBuffer =>
        {
            // The redirected Blt of an NVK-on-RM frame into the GPU-only redirection surface.
            let t_pr = crate::ddi::blt_async::now_100ns();
            let out = foreign_arm(
                passive,
                adapter,
                args,
                context,
                source,
                destination,
                dst,
                source_dxgi,
                destination_dxgi,
                boundary,
                src_rect,
                dst_rect,
            );
            T_PR.add_since(t_pr);
            out
        }
        (Some(_), Some(_)) => {
            if source_dxgi != destination_dxgi {
                return skip(Why::Format);
            }
            let (Ok(s), Ok(d)) = (
                ce_vram::ce_surface(passive, adapter, source.resource_id),
                ce_vram::ce_surface(passive, adapter, destination.resource_id),
            ) else {
                return skip(Why::ChannelBusy);
            };
            let Ok(value) =
                ce_vram::copy(&s, src_rect, &d, dst_rect.left, dst_rect.top, Remap::None)
            else {
                return skip(Why::Copy);
            };
            if !ce_vram::wait(passive, value, ce_vram::XFER_MS) {
                return skip(Why::Copy);
            }
            note_dst(destination.resource_id);
            maybe_publish(false);
            Some(Outcome::Done)
        }
        (None, Some(_)) => {
            // GDI's CPU-written content into the GPU-only surface: the UPLOAD.
            if source.storage != PresentAllocationStorage::PitchedStandardBuffer
                || source.pitch == 0
            {
                return skip(Why::UnknownSource);
            }
            if source_dxgi != destination_dxgi {
                return skip(Why::Format);
            }
            let rows = src_rect.bottom - src_rect.top;
            let row = (src_rect.right - src_rect.left) * 4;
            let span = u64::from(source.pitch) * u64::from(rows - 1) + u64::from(row);
            let mut buf: Vec<u8> = Vec::new();
            if buf.try_reserve_exact(span as usize).is_err() {
                return skip(Why::Memory);
            }
            buf.resize(span as usize, 0);
            let offset =
                u64::from(src_rect.top) * u64::from(source.pitch) + u64::from(src_rect.left) * 4;
            if !crate::ddi::build_paging_buffer::read_standard_buffer(
                passive,
                adapter,
                source.resource_id,
                offset,
                &mut buf,
            ) {
                GDI_FAIL.fetch_add(1, Ordering::Relaxed);
                return skip(Why::Read);
            }
            match ce_vram::transfer(
                passive,
                adapter,
                destination.resource_id,
                dst_rect,
                Dir::Upload,
                &mut buf,
                source.pitch as usize,
            ) {
                Ok(()) => {
                    UPLOAD.fetch_add(1, Ordering::Relaxed);
                    note_dst(destination.resource_id);
                    maybe_publish(false);
                    Some(Outcome::Done)
                }
                Err(_) => {
                    GDI_FAIL.fetch_add(1, Ordering::Relaxed);
                    skip(Why::ChannelBusy)
                }
            }
        }
        (Some(_), None) => {
            // A CPU reader of the GPU-only surface: the READBACK into a CPU-visible buffer.
            if destination.storage != PresentAllocationStorage::PitchedStandardBuffer
                || destination.pitch == 0
            {
                return skip(Why::UnknownSource);
            }
            if source_dxgi != destination_dxgi {
                return skip(Why::Format);
            }
            let rows = src_rect.bottom - src_rect.top;
            let row = (src_rect.right - src_rect.left) * 4;
            let bytes = row as usize * rows as usize;
            let mut buf: Vec<u8> = Vec::new();
            if buf.try_reserve_exact(bytes).is_err() {
                return skip(Why::Memory);
            }
            buf.resize(bytes, 0);
            if ce_vram::transfer(
                passive,
                adapter,
                source.resource_id,
                src_rect,
                Dir::Readback,
                &mut buf,
                row as usize,
            )
            .is_err()
            {
                GDI_FAIL.fetch_add(1, Ordering::Relaxed);
                return skip(Why::ChannelBusy);
            }
            let offset = u64::from(dst_rect.top) * u64::from(destination.pitch)
                + u64::from(dst_rect.left) * 4;
            if !crate::ddi::build_paging_buffer::write_standard_buffer(
                passive,
                adapter,
                destination.resource_id,
                offset,
                destination.pitch,
                row,
                rows,
                &buf,
            ) {
                GDI_FAIL.fetch_add(1, Ordering::Relaxed);
                return skip(Why::Write);
            }
            READBACK.fetch_add(1, Ordering::Relaxed);
            maybe_publish(false);
            Some(Outcome::Done)
        }
        (None, None) => None,
    }
}

/// One foreign frame into a VRAM surface on the copy engine, waited for (at most `XFER_MS`).
fn sync_foreign(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    source: u32,
    src_rect: Rect,
    destination: u32,
    dst_rect: Rect,
    dst_fourcc: u32,
) -> bool {
    use helios_kmd_logic::rm_ce_channel as cc_svc;
    SYNC_TRY.fetch_add(1, Ordering::Relaxed);
    let fail = |step: u32, word: u32| {
        SYNC_WHY.store(step << 24 | (word & 0x00ff_ffff), Ordering::Relaxed);
        false
    };
    let now = crate::ddi::blt_async::now_100ns;
    let t = now();
    let fs = ce_vram::foreign_source(passive, adapter, source);
    T_FS.add_since(t);
    let fs = match fs {
        Ok(f) => f,
        Err(f) => return fail(1, cc_svc::fail_word(f)),
    };
    let t = now();
    let d = ce_vram::ce_surface(passive, adapter, destination);
    T_DS.add_since(t);
    let d = match d {
        Ok(d) => d,
        Err(f) => return fail(2, cc_svc::fail_word(f)),
    };
    let t = now();
    let value = ce_vram::foreign_copy(&fs, src_rect, &d, dst_rect.left, dst_rect.top, dst_fourcc);
    T_SUB.add_since(t);
    let value = match value {
        Ok(v) => v,
        Err(f) => return fail(3, cc_svc::fail_word(f)),
    };
    let t = now();
    let done = ce_vram::wait(passive, value, ce_vram::XFER_MS);
    T_WT.add_since(t);
    if !done {
        return fail(4, 0);
    }
    if SYNC_TRY.load(Ordering::Relaxed) % 64 == 1 {
        let x = (dst_rect.left + dst_rect.right) / 2;
        let y = (dst_rect.top + dst_rect.bottom) / 2;
        let mut px = [0u8; 4];
        let one = Rect {
            left: x,
            top: y,
            right: x + 1,
            bottom: y + 1,
        };
        if ce_vram::transfer(
            passive,
            adapter,
            destination,
            one,
            Dir::Readback,
            &mut px,
            4,
        )
        .is_ok()
        {
            let v = u32::from_le_bytes(px);
            PIX_DST.store(destination, Ordering::Relaxed);
            PIX_SRC.store(source, Ordering::Relaxed);
            PIX_WH.store(
                (dst_rect.right - dst_rect.left) << 16 | (dst_rect.bottom - dst_rect.top),
                Ordering::Relaxed,
            );
            PIX.store(v, Ordering::Relaxed);
            PIX_N.fetch_add(1, Ordering::Relaxed);
            if v & 0x00ff_ffff != 0 {
                PIX_NZ.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    true
}

/// The foreign-source arm of [`blt`]: the route, else the synchronous copy.
#[allow(clippy::too_many_arguments)]
unsafe fn foreign_arm(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    context: Option<&ContextHandleRef<'_>>,
    source: &PresentAllocInfo,
    destination: &PresentAllocInfo,
    dst: vidmem::VramObject,
    source_dxgi: u32,
    destination_dxgi: u32,
    boundary: Option<u64>,
    src_rect: Rect,
    dst_rect: Rect,
) -> Option<Outcome> {
    let Some(fsrc) = crate::virtio::venus::foreign_source_if_enabled(
        adapter,
        source.foreign,
        source.venus_alloc_size,
    ) else {
        return skip(Why::NoForeignSource);
    };
    let Some(source_desc) = OptimalPresentImageDesc::new_foreign_dma_buf(
        source.resource_id,
        source.width,
        source.height,
        source_dxgi,
        fsrc,
    ) else {
        return skip(Why::NoForeignSource);
    };
    // The Venus fallback's view of the VRAM surface: the same foreign-import route the
    // source takes (its layout is the KMD's own record).
    let dst_layout = helios_kmd_logic::foreign_resource::Layout {
        width: dst.width,
        height: dst.height,
        stride: dst.pitch,
        offset: 0,
        fourcc: dst.fourcc,
        modifier: helios_kmd_logic::foreign_resource::MOD_LINEAR,
        plane1: None,
    };
    let Some(destination_desc) =
        crate::virtio::venus::foreign_source_if_enabled(adapter, Some(dst_layout), dst.size)
            .and_then(|f| {
                OptimalPresentImageDesc::new_foreign_dma_buf(
                    destination.resource_id,
                    destination.width,
                    destination.height,
                    destination_dxgi,
                    f,
                )
            })
            .map(PresentDestinationDesc::OptimalImage)
    else {
        return skip(Why::NoDestinationDesc);
    };
    // The source's (0, 0) in the surface: `DstRect` minus `SrcRect` (equal sizes, `rects`); a
    // `SrcRect` origin past the `DstRect` one cannot be placed.
    let (Some(place_x), Some(place_y)) = (
        dst_rect.left.checked_sub(src_rect.left),
        dst_rect.top.checked_sub(src_rect.top),
    ) else {
        return skip(Why::Rect);
    };
    let t_rt = crate::ddi::blt_async::now_100ns();
    // SAFETY: the caller's contract.
    let token = unsafe {
        crate::ddi::ce_present_route::try_route_vram(
            passive,
            adapter,
            args,
            context,
            source_desc,
            destination_desc,
            crate::ddi::ce_present_route::DstInfo {
                resource_id: destination.resource_id,
                width: dst.width,
                height: dst.height,
                pitch: dst.pitch,
                dxgi_format: destination_dxgi,
                alloc_size: dst.size,
                x: place_x,
                y: place_y,
            },
            boundary,
        )
    };
    T_RT.add_since(t_rt);
    match token {
        Some(t) => {
            ROUTED.fetch_add(1, Ordering::Relaxed);
            note_dst(destination.resource_id);
            maybe_publish(false);
            Some(Outcome::Routed(t))
        }
        None => {
            // The route refused (no copy-engine Present record for this frame, the
            // channel busy, ...): the destination is GPU-only and has no Venus copy to
            // fall back to, so copy the frame on the copy engine now from the image's
            // memory (imported by resource id, `ce_vram::foreign_source`), waited for
            // here. Without the record there is no acquire of the producer's semaphore:
            // DXGI presents after the frame's work is submitted, which is the ordering a
            // windowed BLT copy has on this path anyway. 372.1: every Present Blt of
            // explorer's composition into its window surface refused (RvBltWhy 1), the
            // file list black.
            RT_WHY.store(crate::ddi::ce_present_route::last_why(), Ordering::Relaxed);
            if sync_foreign(
                passive,
                adapter,
                source.resource_id,
                src_rect,
                destination.resource_id,
                dst_rect,
                dst.fourcc,
            ) {
                SYNC.fetch_add(1, Ordering::Relaxed);
                note_dst(destination.resource_id);
                maybe_publish(false);
                Some(Outcome::Done)
            } else {
                skip(Why::RouteRefused)
            }
        }
    }
}
