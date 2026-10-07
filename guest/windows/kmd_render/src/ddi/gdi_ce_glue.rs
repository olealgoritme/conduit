//! The one seam between GDI acceleration (`ddi/gdi_accel.rs`, `ddi/gdi_exec.rs`) and the modules
//! of the redirection lane it executes on (boundary agreed with the V2-V5 work, recorded in
//! `docs/vram-redirection.md` 8 and 9): the RM video-memory surfaces and their copy-engine
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

/// How often a call that found the channel's I/O held by another thread tries again (1 ms apart).
const BUSY_TRIES: u32 = 4;

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
