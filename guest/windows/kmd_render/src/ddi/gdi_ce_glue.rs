//! The one seam between GDI acceleration (`ddi/gdi_accel.rs`, `ddi/gdi_exec.rs`) and the modules
//! of the redirection lane it executes on: the RM video-memory surfaces and their copy-engine
//! mappings (`virtio/rm_client/ce_vram.rs`, `RedirVram`), the generic submission on the KMD's
//! copy-engine channel (`ce_channel::submit_build`), and the CPU view of a KMD standard buffer
//! (`build_paging_buffer::{read,write}_standard_buffer`). Every function here is a thin call, so
//! the executor depends on this file's names only.
//!
//! On a branch without those modules every function answers "not available": no surface is
//! VRAM, no submission is made, no CPU view is read. The executor then drops every command
//! (`GdiDrop`, `GdiWhy` 9/12) and still retires every fence.

use helios_kmd_logic::ce_present::{self as cp, Gen};
use helios_kmd_logic::gdi_accel::{CeView, Rect};

use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;

/// Is `resource_id` an RM-VRAM-backed surface (`vidmem::lookup`)? Spinlock-only, any IRQL up to
/// DISPATCH.
pub(crate) fn is_vram(_resource_id: u32) -> bool {
    false
}

/// `resource_id` as a copy-engine surface, mapped on demand (`ce_vram::ce_surface`). PASSIVE, no
/// lock held, the channel's I/O not held by the caller.
pub(crate) fn ce_surface(_passive: PassiveLevel, _adapter: &AdapterContext, _resource_id: u32) -> Option<CeView> {
    None
}

/// One push on the channel (`ce_channel::submit_build`): `build` writes its methods and must end
/// with the release it is handed. The completion value, or `None` when the channel refused.
pub(crate) fn submit(
    _build: impl FnOnce(&mut cp::Push<'_>, Gen, cp::Release) -> Result<(), cp::PushError>,
) -> Option<u64> {
    None
}

/// The push slot's size in dwords (`rm_ce_channel::SLOT_DWORDS`).
pub(crate) const SLOT_DWORDS: usize = helios_kmd_logic::rm_ce_channel::SLOT_DWORDS;

/// Wait until the channel's completion reaches `value` (`ce_vram::wait`).
pub(crate) fn wait(_passive: PassiveLevel, _value: u64, _max_ms: u64) -> bool {
    false
}

/// `rect` of VRAM surface `resource_id` into `out`, rows `row_pitch` apart (`ce_vram::transfer`,
/// readback through the bounce buffer). PASSIVE.
pub(crate) fn vram_read(
    _passive: PassiveLevel,
    _adapter: &AdapterContext,
    _resource_id: u32,
    _rect: Rect,
    _out: &mut [u8],
    _row_pitch: usize,
) -> bool {
    false
}

/// `data` (rows `row_pitch` apart) into `rect` of VRAM surface `resource_id` (`ce_vram::transfer`,
/// upload). PASSIVE.
pub(crate) fn vram_write(
    _passive: PassiveLevel,
    _adapter: &AdapterContext,
    _resource_id: u32,
    _rect: Rect,
    _data: &mut [u8],
    _row_pitch: usize,
) -> bool {
    false
}

/// `out.len()` bytes at `offset` of KMD standard buffer `resource_id`'s authoritative CPU view
/// (`read_standard_buffer`). PASSIVE.
pub(crate) fn std_read(
    _passive: PassiveLevel,
    _adapter: &AdapterContext,
    _resource_id: u32,
    _offset: u64,
    _out: &mut [u8],
) -> bool {
    false
}

/// `rows` packed rows of `row_bytes` at `offset`, stride `pitch`, into KMD standard buffer
/// `resource_id` (`write_standard_buffer`: the blob, then the system pages VidMm holds). PASSIVE.
#[allow(clippy::too_many_arguments)]
pub(crate) fn std_write(
    _passive: PassiveLevel,
    _adapter: &AdapterContext,
    _resource_id: u32,
    _offset: u64,
    _pitch: u32,
    _row_bytes: u32,
    _rows: u32,
    _data: &[u8],
) -> bool {
    false
}
