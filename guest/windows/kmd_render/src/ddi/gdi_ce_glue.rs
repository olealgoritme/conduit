//! The one seam between GDI acceleration (`ddi/gdi_accel.rs`, `ddi/gdi_exec.rs`) and the RM
//! video-memory surfaces of the redirection lane (`virtio/rm_client/ce_vram.rs`, `RedirVram`).
//! Until that module is on this branch every surface answers "not VRAM", so with `GdiAccel` = 1
//! nothing is planned on the copy engine.

/// Is `resource_id` an RM-VRAM-backed surface with a copy-engine mapping (`ce_vram`'s cache)?
/// Spinlock-only, any IRQL up to DISPATCH.
pub(crate) fn is_vram(_resource_id: u32) -> bool {
    false
}
