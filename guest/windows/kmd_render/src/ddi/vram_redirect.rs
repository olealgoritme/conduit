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
}

static SEEN: AtomicU32 = AtomicU32::new(0);
static ROUTED: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static READBACK: AtomicU32 = AtomicU32::new(0);
static UPLOAD: AtomicU32 = AtomicU32::new(0);
static GDI_FAIL: AtomicU32 = AtomicU32::new(0);

/// StartDevice (PASSIVE): zero the counters (written only once a VRAM Blt was seen).
pub(crate) fn reset_for_start() {
    for c in [&SEEN, &ROUTED, &SKIP, &WHY, &READBACK, &UPLOAD, &GDI_FAIL] {
        c.store(0, Ordering::Relaxed);
    }
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
    vidmem::publish_counters();
}

/// Throttled publish: every 64th VRAM Blt, and every skip.
fn maybe_publish(force: bool) {
    if force || SEEN.load(Ordering::Relaxed) % 64 == 1 {
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
fn rects(args: &DXGKARG_PRESENT, src: &PresentAllocInfo, dst: &PresentAllocInfo) -> Option<(Rect, Rect)> {
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
    let Some((src_rect, dst_rect)) = rects(args, source, destination) else {
        return skip(Why::Rect);
    };
    match (src_vram, dst_vram) {
        (_, Some(dst)) if source.foreign.is_some() && src_vram.is_none() => {
            // The redirected Blt of an NVK-on-RM frame into the GPU-only redirection surface.
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
            let Some(destination_desc) = crate::virtio::venus::foreign_source_if_enabled(
                adapter,
                Some(dst_layout),
                dst.size,
            )
            .and_then(|f| {
                OptimalPresentImageDesc::new_foreign_dma_buf(
                    destination.resource_id,
                    destination.width,
                    destination.height,
                    destination_dxgi,
                    f,
                )
            })
            .map(PresentDestinationDesc::OptimalImage) else {
                return skip(Why::NoDestinationDesc);
            };
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
                    },
                    boundary,
                )
            };
            match token {
                Some(t) => {
                    ROUTED.fetch_add(1, Ordering::Relaxed);
                    maybe_publish(false);
                    Some(Outcome::Routed(t))
                }
                None => skip(Why::RouteRefused),
            }
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
            let Ok(value) = ce_vram::copy(&s, src_rect, &d, dst_rect.left, dst_rect.top, Remap::None)
            else {
                return skip(Why::Copy);
            };
            if !ce_vram::wait(passive, value, ce_vram::XFER_MS) {
                return skip(Why::Copy);
            }
            maybe_publish(false);
            Some(Outcome::Done)
        }
        (None, Some(_)) => {
            // GDI's CPU-written content into the GPU-only surface: the UPLOAD.
            if source.storage != PresentAllocationStorage::PitchedStandardBuffer || source.pitch == 0 {
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
            let offset = u64::from(src_rect.top) * u64::from(source.pitch) + u64::from(src_rect.left) * 4;
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
