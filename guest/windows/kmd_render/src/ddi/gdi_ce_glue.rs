//! The one seam between GDI acceleration (`ddi/gdi_accel.rs`, `ddi/gdi_exec.rs`) and the modules
//! of the redirection lane it executes on (boundary agreed with the V2-V5 work, recorded in
//! `docs/vram-redirection.md` 8 and 10): the RM video-memory surfaces and their copy-engine
//! mappings (`virtio/rm_client/{vidmem,ce_vram}.rs`, `RedirVram`), the generic submission on the
//! KMD's copy-engine channel (`ce_channel::submit_build`), and the CPU view of a KMD standard
//! buffer (`build_paging_buffer::{read,write}_standard_buffer`). Every function here is a thin
//! call, so the executor depends on this file's names only.

use helios_kmd_logic::ce_present::{self as cp, Gen};
use helios_kmd_logic::gdi_accel::{CeView, Rect};
use helios_kmd_logic::rm_vidmem as rv;

use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::virtio::rm_client::{ce_channel, ce_route, ce_vram, vidmem};

/// The copy-engine channel's state for GDI acceleration: 0 up, 1 cold (a bring-up may be asked
/// for), 2 disabled for the generation, 3 broken (waiting for its teardown), 4 another phase
/// (coming up, cooling down). Spinlock only.
pub(crate) fn channel_state() -> u32 {
    let v = ce_route::chan_view();
    if v.up {
        0
    } else if v.may_bring_up {
        1
    } else if v.disabled {
        2
    } else if v.broken {
        3
    } else {
        4
    }
}

/// Bring the channel up (`ce_route::bring_up`: the route's own bring-up, bounded by the channel's
/// 6 s budget, never waiting for its I/O). HPD worker only. Until now only a routed Present asked
/// for it, so a desktop with no windowed NVK Present left the channel cold and every GDI
/// operation on a VRAM surface failed (358.1). Whether it is up afterwards.
pub(crate) fn bring_up(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    ce_route::bring_up(passive, adapter)
}

/// A foreign NVK image as a copy-engine source (`ce_vram::foreign_source`): the producer's
/// objects with an acquire when the route saw a record for it, else an import by resource id.
/// Takes the channel's I/O itself: call it before `with_standard`. PASSIVE.
pub(crate) struct ForeignSrc(ce_vram::ForeignSource);

pub(crate) fn foreign_source(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) -> Option<ForeignSrc> {
    retry_busy(passive, || ce_vram::foreign_source(passive, adapter, resource_id)).map(ForeignSrc)
}

/// The `DRM_FORMAT_*` of a GDI surface's D3DDDIFORMAT (21 A8R8G8B8, 22 X8R8G8B8, 32 A8B8G8R8,
/// 33 X8B8G8R8; anything else is the GDI default ARGB8888).
pub(crate) fn fourcc_of(d3dddi_format: u32) -> u32 {
    use helios_kmd_logic::foreign_resource as fr;
    match d3dddi_format {
        22 => fr::FOURCC_XRGB8888,
        32 => fr::FOURCC_ABGR8888,
        33 => fr::FOURCC_XBGR8888,
        _ => fr::FOURCC_ARGB8888,
    }
}

/// Copies `(src_rect, dst_rect)` pairs from a foreign image into `dst` and waits for the last one;
/// `0` when every copy landed, else the failing step (3 submit, 4 wait).
fn foreign_copies(passive: PassiveLevel, src: &ForeignSrc, dst: &ce_vram::CeSurface, pairs: &[(Rect, Rect)], fourcc: u32) -> u32 {
    let mut last = None;
    for (s, d) in pairs {
        let (Some(sr), true) = (vrect(*s), d.left >= 0 && d.top >= 0) else {
            continue;
        };
        match ce_vram::foreign_copy(&src.0, sr, dst, d.left as u32, d.top as u32, fourcc) {
            Ok(v) => last = Some(v),
            Err(_) => {
                if let Some(v) = last {
                    let _ = ce_vram::wait(passive, v, 100);
                }
                return 3;
            }
        }
    }
    match last {
        Some(v) if !ce_vram::wait(passive, v, 100) => 4,
        _ => 0,
    }
}

/// A foreign image into a VRAM surface. `0` done, else the failing step (2 destination mapping,
/// 3 submit, 4 wait).
pub(crate) fn foreign_to_vram(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    src: &ForeignSrc,
    dst_resource_id: u32,
    pairs: &[(Rect, Rect)],
    fourcc: u32,
) -> u32 {
    let Some(dst) = retry_busy(passive, || ce_vram::ce_surface(passive, adapter, dst_resource_id)) else {
        return 2;
    };
    foreign_copies(passive, src, &dst, pairs, fourcc)
}

/// A foreign image into a staging buffer's copy-engine view (`ce_sysmem::with_standard`). `0`
/// done, 5 the view refused, else as [`foreign_to_vram`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn foreign_to_standard(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    src: &ForeignSrc,
    dst_resource_id: u32,
    pitch: u32,
    width: u32,
    height: u32,
    pairs: &[(Rect, Rect)],
    fourcc: u32,
) -> u32 {
    let r = crate::ddi::ce_sysmem::with_standard(passive, adapter, dst_resource_id, pitch, width, height, |dst| {
        foreign_copies(passive, src, dst, pairs, fourcc)
    });
    crate::ddi::ce_sysmem::publish_counters();
    r.unwrap_or(5)
}

/// How often a call that found the channel's I/O held by another thread tries again (1 ms apart).
const BUSY_TRIES: u32 = 10;

/// Is `resource_id` an RM-VRAM-backed surface (`vidmem::lookup`)? Spinlock-only, any IRQL up to
/// DISPATCH.
pub(crate) fn is_vram(resource_id: u32) -> bool {
    vidmem::lookup(resource_id).is_some()
}

fn retry_busy<T>(passive: PassiveLevel, mut f: impl FnMut() -> Result<T, helios_kmd_logic::rm_client::Fail>) -> Option<T> {
    for i in 0..BUSY_TRIES {
        match f() {
            Ok(v) => return Some(v),
            Err(e) if ce_route::is_busy(&e) && i + 1 < BUSY_TRIES => crate::virtio::ctrl::sleep_ms(passive, 1),
            Err(_) => return None,
        }
    }
    None
}

/// `resource_id` as a copy-engine surface, mapped on demand (`ce_vram::ce_surface`). PASSIVE, no
/// lock held, the channel's I/O not held by the caller.
pub(crate) fn ce_surface(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) -> Option<CeView> {
    let s = retry_busy(passive, || ce_vram::ce_surface(passive, adapter, resource_id))?;
    Some(CeView { va: s.va, pitch: s.pitch, width: s.width, height: s.height })
}

/// One push on the channel (`ce_channel::submit_build`): `build` writes its methods and must end
/// with the release it is handed. The completion value, or `None` when the channel refused.
pub(crate) fn submit(
    build: impl FnOnce(&mut cp::Push<'_>, Gen, cp::Release) -> Result<(), cp::PushError>,
) -> Option<u64> {
    ce_channel::submit_build(build).ok()
}

/// The push slot's size in dwords (`rm_ce_channel::SLOT_DWORDS`).
pub(crate) const SLOT_DWORDS: usize = helios_kmd_logic::rm_ce_channel::SLOT_DWORDS;

/// Wait until the channel's completion reaches `value` (`ce_vram::wait`).
pub(crate) fn wait(passive: PassiveLevel, value: u64, max_ms: u64) -> bool {
    ce_vram::wait(passive, value, max_ms)
}

fn vrect(r: Rect) -> Option<rv::Rect> {
    if r.left < 0 || r.top < 0 || r.is_empty() {
        return None;
    }
    Some(rv::Rect { left: r.left as u32, top: r.top as u32, right: r.right as u32, bottom: r.bottom as u32 })
}

/// `rect` of VRAM surface `resource_id` into `out`, rows `row_pitch` apart (`ce_vram::transfer`,
/// readback through the bounce buffer). PASSIVE.
pub(crate) fn vram_read(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    rect: Rect,
    out: &mut [u8],
    row_pitch: usize,
) -> bool {
    let Some(r) = vrect(rect) else { return false };
    retry_busy(passive, || ce_vram::transfer(passive, adapter, resource_id, r, rv::Dir::Readback, out, row_pitch)).is_some()
}

/// `data` (rows `row_pitch` apart) into `rect` of VRAM surface `resource_id` (`ce_vram::transfer`,
/// upload). PASSIVE.
pub(crate) fn vram_write(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    rect: Rect,
    data: &mut [u8],
    row_pitch: usize,
) -> bool {
    let Some(r) = vrect(rect) else { return false };
    retry_busy(passive, || ce_vram::transfer(passive, adapter, resource_id, r, rv::Dir::Upload, data, row_pitch)).is_some()
}

/// Run `f` with KMD standard buffer `resource_id` (`pitch` bytes per row, `width` x `height`) as a
/// copy-engine surface over its system pages (`ce_sysmem::with_standard`: takes the content
/// transaction, then the channel's I/O). `f` must wait for everything it submits and must not
/// resolve a VRAM surface (do that before). `Err`: the refusal as a [`SysRefusal`]. PASSIVE, no
/// lock held.
pub(crate) fn with_standard<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    pitch: u32,
    width: u32,
    height: u32,
    f: impl FnOnce(&CeView) -> R,
) -> Result<R, SysRefusal> {
    let r = crate::ddi::ce_sysmem::with_standard(passive, adapter, resource_id, pitch, width, height, |s| {
        f(&CeView { va: s.va, pitch: s.pitch, width: s.width, height: s.height })
    });
    // `ce_sysmem` mirrors its `RvSys*` counters only after a success; a refusal is mirrored here
    // too, so a session where everything is refused still shows why.
    crate::ddi::ce_sysmem::publish_counters();
    r.map_err(refusal)
}

/// Two staging buffers' copy-engine views in one content transaction
/// (`ce_sysmem::with_standard_pair`); `a`, `b` are `(resource_id, pitch, width, height)`. Same rules
/// as [`with_standard`]. PASSIVE, no lock held.
pub(crate) fn with_standard_pair<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    a: (u32, u32, u32, u32),
    b: (u32, u32, u32, u32),
    f: impl FnOnce(&CeView, &CeView) -> R,
) -> Result<R, SysRefusal> {
    let view = |s: &ce_vram::CeSurface| CeView { va: s.va, pitch: s.pitch, width: s.width, height: s.height };
    let r = crate::ddi::ce_sysmem::with_standard_pair(passive, adapter, a, b, |x, y| f(&view(x), &view(y)));
    crate::ddi::ce_sysmem::publish_counters();
    r.map_err(refusal)
}

fn refusal(f: helios_kmd_logic::rm_client::Fail) -> SysRefusal {
    SysRefusal {
        class: if f == crate::ddi::ce_sysmem::NOT_SYSTEM {
            SysClass::NotSystem
        } else if f == crate::ddi::ce_sysmem::UNCOVERED {
            SysClass::Uncovered
        } else if ce_route::is_busy(&f) || f == ce_route::NO_CHANNEL {
            SysClass::Busy
        } else if f == crate::ddi::ce_sysmem::UNSURE {
            SysClass::Unsure
        } else {
            SysClass::Other
        },
        word: helios_kmd_logic::rm_ce_channel::fail_word(f),
    }
}

/// Why `with_standard` refused.
#[derive(Clone, Copy)]
pub(crate) struct SysRefusal {
    pub class: SysClass,
    /// `rm_ce_channel::fail_word` of the refusal.
    pub word: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SysClass {
    /// No leases (the buffer is in the Venus window, segment 2), a stale system copy, a guest blob.
    NotSystem,
    /// Partial leases, more than one window (64 MiB), no slot.
    Uncovered,
    /// The channel's I/O busy past 250 ms, or no channel.
    Busy,
    /// An RM timeout (the pages stay pinned).
    Unsure,
    Other,
}

/// `out.len()` bytes at `offset` of KMD standard buffer `resource_id`'s authoritative CPU view
/// (`read_standard_buffer`). PASSIVE.
pub(crate) fn std_read(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    offset: u64,
    out: &mut [u8],
) -> bool {
    crate::ddi::build_paging_buffer::read_standard_buffer(passive, adapter, resource_id, offset, out)
}

/// `rows` packed rows of `row_bytes` at `offset`, stride `pitch`, into KMD standard buffer
/// `resource_id` (`write_standard_buffer`: the blob, then the system pages VidMm holds). PASSIVE.
#[allow(clippy::too_many_arguments)]
pub(crate) fn std_write(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    offset: u64,
    pitch: u32,
    row_bytes: u32,
    rows: u32,
    data: &[u8],
) -> bool {
    crate::ddi::build_paging_buffer::write_standard_buffer(
        passive, adapter, resource_id, offset, pitch, row_bytes, rows, data,
    )
}
