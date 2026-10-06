//! Importing a foreign (NVK-on-RM) resource into the KMD's own Venus device, for
//! the copy into the adapter's LINEAR scan-out image.
//!
//! A foreign resource is RM memory the host exported as a dma-buf (`blob_mem
//! 0x80000001`, `HELIOS_ESCAPE_FOREIGN_RESOURCE IMPORT_RM`). The host's NVIDIA
//! Vulkan driver imports it only as an explicit-modifier image on a dedicated
//! allocation; the plain OPTIMAL opaque-fd import that every ordinary UMD
//! resource uses fails (`VK_ERROR_OUT_OF_DEVICE_MEMORY`) and would have the wrong
//! layout anyway. See `docs/zero-copy-present.md`, "The KMD copy of a foreign
//! resource", for the whole picture; the rules (format mapping, modifier set,
//! size checks, ladder choice) are pure and live in
//! `helios_kmd_logic::foreign_copy` with host tests. This file is the Venus half:
//! the two commands that carry the new chains, and the import sequence.
//!
//! Two callers share it: [`VenusClient::prepare_optimal_scanout_copy`] (the
//! SetVidPnSourceAddress primary copy) and
//! [`VenusClient::import_optimal_present_image`] (the windowed/GDI Blt). Both pick
//! this path only when the source has a foreign record; every other resource
//! keeps the code it always had, byte for byte.
//!
//! PASSIVE_LEVEL, under the Venus mutex, like every other function in the module.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::foreign_copy as fc;
use helios_kmd_logic::foreign_resource as fr;

use super::ring::*;
use super::*;

// ── counters (registry mirror: `publish_counters`, from `publish_nvrm_counters`) ──
//
// Every refused or failed path is counted. `FcImp` counts only complete imports;
// the difference to `FcScan + FcBlt` is zero by construction, and anything in
// `FcRefuse`/`FcHostErr`/`FcNoExt`/`FcStale` is a foreign source that was turned
// away, with `FcRefCode` naming the last reason.

/// Foreign sources imported completely (image created, memory imported, bound).
pub(crate) static FC_IMPORTS: AtomicU32 = AtomicU32::new(0);
/// ... of which for the SetVidPnSourceAddress scan-out copy (`FcScan`).
pub(crate) static FC_SCANOUT: AtomicU32 = AtomicU32::new(0);
/// ... of which for the windowed/GDI Blt (`FcBlt`).
pub(crate) static FC_BLT: AtomicU32 = AtomicU32::new(0);
/// Foreign sources refused before or after the host was asked, for a reason the
/// KMD itself decided: bad record, extent mismatch, undersized resource, no
/// usable memory type (`FcRefuse`; the last reason is `FcRefCode`).
pub(crate) static FC_REFUSED: AtomicU32 = AtomicU32::new(0);
/// The last refusal's code: `fc::Refusal::code` (1 to 8) or `fc::CODE_*`.
pub(crate) static FC_REFUSE_CODE: AtomicU32 = AtomicU32::new(0);
/// The host refused a Vulkan step of an import (image create, memory import,
/// bind, requirements), or a transport error cut it short (`FcHostErr`).
pub(crate) static FC_HOST_ERRORS: AtomicU32 = AtomicU32::new(0);
/// A foreign source arrived but the KMD's device was created without
/// `VK_EXT_image_drm_format_modifier` (the host does not expose it, the host does
/// not serve IMPORT_RM, or the knob was off at device creation): the foreign copy
/// path is unavailable, never a bring-up failure (`FcNoExt`).
pub(crate) static FC_NO_EXT: AtomicU32 = AtomicU32::new(0);
/// The layout the opener read from the allocation disagreed with the KMD's
/// record, or there was no record (`FcStale`).
pub(crate) static FC_STALE: AtomicU32 = AtomicU32::new(0);
/// A foreign source seen while the `ForeignCopy` knob is 0: it takes the
/// ordinary OPTIMAL import, as before this feature (`FcOff`).
pub(crate) static FC_KNOB_OFF: AtomicU32 = AtomicU32::new(0);
/// A foreign source whose record is a shared format this 32 bpp, one-plane copy cannot carry
/// (`R8`, `YUYV`, `NV12`, fp16, ... or any record with a plane 1: `docs/shared-formats.md`),
/// refused instead of read as BGRA (`FcNotRgb32`). Included in `FcRefuse`; `FcRefCode` is 6.
pub(crate) static FC_NOT_RGB32: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the service key. PASSIVE_LEVEL only; called with the
/// rest of the NVRM counters (`publish_nvrm_counters`).
pub(crate) fn publish_counters() {
    crate::diag::record_named_bytes(b"FcImp", FC_IMPORTS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcScan", FC_SCANOUT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcBlt", FC_BLT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcRefuse", FC_REFUSED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcRefCode", FC_REFUSE_CODE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcHostErr", FC_HOST_ERRORS.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcNoExt", FC_NO_EXT.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcStale", FC_STALE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcOff", FC_KNOB_OFF.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FcNotRgb32", FC_NOT_RGB32.load(Ordering::Relaxed));
}

fn refuse(code: u32) -> VirtioError {
    FC_REFUSED.fetch_add(1, Ordering::Relaxed);
    FC_REFUSE_CODE.store(code, Ordering::Relaxed);
    VirtioError::DeviceError
}

/// What a caller read about a foreign source: the layout and the host-verified
/// size of the resource. It comes from the allocation (the creator's context or
/// the open-time private data, neither of which takes a lock on the present
/// path) and is checked against the KMD's own record once, when the import is
/// built ([`VenusClient::foreign_preflight`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ForeignSource {
    pub layout: fr::Layout,
    pub record_size: u64,
}

impl ForeignSource {
    /// The Present pixel format the image's bytes are, from the record's fourcc
    /// (never from the allocation's DXGI format: the fourcc is what NVK created
    /// the image with). `None` for a fourcc the import does not handle.
    pub(super) fn pixel_format(&self) -> Option<PresentPixelFormat> {
        match fc::vk_format_for_fourcc(self.layout.fourcc)? {
            fc::VK_FORMAT_B8G8R8A8_UNORM => Some(PresentPixelFormat::Bgra8Unorm),
            fc::VK_FORMAT_R8G8B8A8_UNORM => Some(PresentPixelFormat::Rgba8Unorm),
            _ => None,
        }
    }
}

/// The source to import as a foreign resource, or `None` for the ordinary path.
///
/// `layout` is `Some` only for an allocation that adopted a foreign resource. The
/// `ForeignCopy` knob (default 0 = OFF, `adapter::Knobs::foreign_copy`; set 1 to use the
/// foreign copy) is the switch: at 0, the default, the foreign source is treated exactly
/// as it was before this feature existed (the plain OPTIMAL import, which the host
/// refuses for these resources), and counted as `FcOff`.
pub(crate) fn foreign_source_if_enabled(
    adapter: &AdapterContext,
    layout: Option<fr::Layout>,
    record_size: u64,
) -> Option<ForeignSource> {
    let layout = layout?;
    if !adapter.knobs().foreign_copy {
        FC_KNOB_OFF.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(ForeignSource {
        layout,
        record_size,
    })
}

impl VenusClient {
    /// Everything that can be decided without touching the host: the device has
    /// the extension, the opener's copy of the record is the KMD's record, and
    /// the record describes an importable image of exactly this extent.
    ///
    /// Run BEFORE the resource is attached to the Venus context, so a refusal
    /// leaves nothing to undo.
    pub(super) fn foreign_preflight(
        &self,
        adapter: &AdapterContext,
        resource_id: u32,
        source: &ForeignSource,
        width: u32,
        height: u32,
    ) -> Result<fc::ForeignImage, VirtioError> {
        if !self.modifier_import_device {
            FC_NO_EXT.fetch_add(1, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"FcImpSt", 0xE1);
            return Err(VirtioError::DeviceError);
        }
        // The authoritative record. A missing one means the resource is dead or
        // was never foreign (a forged trailer on an ordinary allocation); a
        // different one means the copy the caller holds is stale. Either way
        // nothing is imported.
        let recorded = adapter
            .with_virtio(|v| v.foreign_record(resource_id))
            .map_err(|_| VirtioError::DeviceError)?;
        match recorded {
            Some((layout, size))
                if fc::record_agrees(&source.layout, &layout) && size == source.record_size => {}
            _ => {
                FC_STALE.fetch_add(1, Ordering::Relaxed);
                crate::diag::record_named_bytes(b"FcImpSt", 0xE2);
                return Err(VirtioError::DeviceError);
            }
        }
        fc::ForeignImage::from_record(&source.layout, source.record_size, width, height).map_err(
            |reason| {
                crate::diag::record_named_bytes(b"FcImpSt", 0xE3);
                if reason == fc::Refusal::Format {
                    FC_NOT_RGB32.fetch_add(1, Ordering::Relaxed);
                }
                refuse(reason.code())
            },
        )
    }

    /// Create the explicit-modifier source image.
    fn create_foreign_modifier_image(
        &mut self,
        adapter: &AdapterContext,
        image: &fc::ForeignImage,
    ) -> Result<VkImageId, VirtioError> {
        let image_id = self.new_image_id();
        let w = encode_image_create(self.device_id.into(), image_id.into(), &image.image_spec());
        let mut r = self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_CREATE_IMAGE)
                .mismatch(0x0140)
                .refuse_result(0x0141)
                .result_marks(b"FcImgVr"),
        )?;
        if r.read_u64()? == 0 || r.read_u64()? == 0 {
            diag(0x0142);
            return Err(VirtioError::DeviceError);
        }
        Ok(image_id)
    }

    /// Import the virtio resource as `VkDeviceMemory`, dedicated to `image_id`.
    fn allocate_foreign_import_memory(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        image_id: VkImageId,
        size: u64,
        memory_type_index: u32,
    ) -> Result<VkDeviceMemoryId, VirtioError> {
        let memory_id = self.new_memory_id();
        let w = encode_memory_allocate(
            self.device_id.into(),
            memory_id.into(),
            &fc::ForeignImage::memory_spec(resource_id, image_id.into(), size, memory_type_index),
        );
        self.ring_command_expect(
            adapter,
            w.as_slice()?,
            ReplyCheck::new(CMD_ALLOCATE_MEMORY)
                .mismatch(0x0143)
                .refuse_result(0x0144)
                .result_marks(b"FcMemVr"),
        )?;
        Ok(memory_id)
    }

    /// Import an attached foreign resource as an explicit-modifier source image
    /// bound to its own dedicated, imported memory.
    ///
    /// `resource_id` is already attached to this Venus context (the caller's
    /// `attach_resource_checked`); on any failure this function leaves nothing
    /// behind, including the attachment, exactly like the OPTIMAL import it stands
    /// in for. The memory type is the KMD device's own choice (the first
    /// DEVICE_LOCAL type the image allows), not the creator's: a foreign resource
    /// has no creator-side `vkAllocateMemory`. The allocation is the image's own
    /// requirement, which must fit the recorded size.
    pub(super) fn import_foreign_source(
        &mut self,
        adapter: &AdapterContext,
        resource_id: u32,
        image: &fc::ForeignImage,
    ) -> Result<(VkImageId, VkDeviceMemoryId), VirtioError> {
        crate::diag::record_named_bytes(b"FcImpSt", 1);
        let image_id = match self.create_foreign_modifier_image(adapter, image) {
            Ok(id) => id,
            Err(e) => {
                FC_HOST_ERRORS.fetch_add(1, Ordering::Relaxed);
                let _ =
                    ctrl::ctx_detach_resource(self.passive(), adapter, self.ctx_id(), resource_id);
                return Err(e);
            }
        };

        crate::diag::record_named_bytes(b"FcImpSt", 2);
        let (required_size, memory_type_bits) = match self
            .image_memory_requirements(adapter, image_id)
        {
            Ok(req) => req,
            Err(e) => {
                FC_HOST_ERRORS.fetch_add(1, Ordering::Relaxed);
                let _ = self.cleanup_imported_source_alias(adapter, resource_id, image_id, None);
                return Err(e);
            }
        };
        crate::diag::record_named_bytes(b"FcReq", required_size as u32);
        crate::diag::record_named_bytes(b"FcBit", memory_type_bits);
        let Some(allocation_size) = fc::import_allocation_size(required_size, image.record_size)
        else {
            // The image needs more bytes than the resource has: importing it
            // would let the copy read past the blob.
            crate::diag::record_named_bytes(b"FcImpSt", 0xE4);
            let _ = self.cleanup_imported_source_alias(adapter, resource_id, image_id, None);
            return Err(refuse(fc::CODE_UNDERSIZE));
        };
        let Some(choice) = self.choose_device_local_memory_type(memory_type_bits) else {
            crate::diag::record_named_bytes(b"FcImpSt", 0xE5);
            let _ = self.cleanup_imported_source_alias(adapter, resource_id, image_id, None);
            return Err(refuse(fc::CODE_NO_MEMORY_TYPE));
        };
        let memory_type_index = Self::accept_memory_type(choice);
        crate::diag::record_named_bytes(b"FcMt", memory_type_index);

        crate::diag::record_named_bytes(b"FcImpSt", 3);
        let memory_id = match self.allocate_foreign_import_memory(
            adapter,
            resource_id,
            image_id,
            allocation_size,
            memory_type_index,
        ) {
            Ok(id) => id,
            Err(e) => {
                FC_HOST_ERRORS.fetch_add(1, Ordering::Relaxed);
                let _ = self.cleanup_imported_source_alias(adapter, resource_id, image_id, None);
                return Err(e);
            }
        };

        crate::diag::record_named_bytes(b"FcImpSt", 4);
        if let Err(e) = self.bind_image_memory(adapter, image_id, memory_id) {
            FC_HOST_ERRORS.fetch_add(1, Ordering::Relaxed);
            let _ =
                self.cleanup_imported_source_alias(adapter, resource_id, image_id, Some(memory_id));
            return Err(e);
        }
        FC_IMPORTS.fetch_add(1, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"FcImpSt", 0x10);
        Ok((image_id, memory_id))
    }
}
