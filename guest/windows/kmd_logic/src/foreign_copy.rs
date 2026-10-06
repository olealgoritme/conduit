//! The KMD copy of a foreign (NVK-on-RM) resource into the adapter's LINEAR
//! scan-out image: the pure half.
//!
//! # Why a foreign resource needs its own import
//!
//! `prepare_optimal_scanout_copy` imports an ordinary UMD resource as a plain
//! OPTIMAL-tiling `VkImage` with the opaque-fd handle type. A foreign resource is
//! RM memory exported as a dma-buf; the host's NVIDIA Vulkan driver imports it
//! only as an image created with `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and an
//! explicit layout, on a dedicated allocation (host spike c6fab91; an OPTIMAL
//! import gives `VK_ERROR_OUT_OF_DEVICE_MEMORY` and would have the wrong layout
//! anyway). Everything the importer needs is in the foreign record
//! ([`crate::foreign_resource::Layout`] plus the host-verified size); this module
//! turns that record into the image parameters and refuses what cannot be
//! imported, as a pure function of its arguments so the rules carry host tests.
//!
//! The Vulkan encoding itself is [`crate::ImagePNext::ExternalMemoryDrmExplicit`]
//! and [`crate::MemoryPNext::ImportResourceDedicated`]; this module only decides
//! what goes into them.
//!
//! # The device extension
//!
//! The image needs `VK_EXT_image_drm_format_modifier` enabled on the KMD's
//! `VkDevice`. The KMD deliberately has no such device by default: enabling it on
//! the ONE production device inflated the memory requirements of ordinary shared
//! OPTIMAL imports in the 38th session. [`modifier_tier_wanted`] therefore turns
//! it on only where a foreign resource can exist at all (the host serves
//! `IMPORT_RM`) and the `ForeignCopy` knob allows it, and the ladder falls back
//! to the unchanged export-only device when the host refuses it.

use crate::foreign_resource::{
    Layout, LayoutError, FOURCC_ABGR8888, FOURCC_ARGB8888, FOURCC_XBGR8888, FOURCC_XRGB8888,
};
use crate::{ImageCreateSpec, ImagePNext, MemoryAllocateSpec, MemoryPNext};

/// `VK_FORMAT_R8G8B8A8_UNORM` (memory order R, G, B, A: `DRM_FORMAT_XBGR8888`).
pub const VK_FORMAT_R8G8B8A8_UNORM: u32 = 37;
/// `VK_FORMAT_B8G8R8A8_UNORM` (memory order B, G, R, A: `DRM_FORMAT_XRGB8888`).
pub const VK_FORMAT_B8G8R8A8_UNORM: u32 = 44;
/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`: the handle type of the
/// image's `VkExternalMemoryImageCreateInfo`.
pub const HANDLE_TYPE_DMA_BUF: u32 = 0x200;
/// `VK_IMAGE_USAGE_TRANSFER_SRC_BIT`: the only use the KMD makes of the source.
/// Anything more would have to be supported by the host driver for the exact
/// modifier, for nothing: the copy reads the image once per frame and never
/// samples or renders to it.
pub const USAGE_TRANSFER_SRC: u32 = 0x1;
/// `VK_IMAGE_LAYOUT_UNDEFINED`: the only initial layout a modifier image may
/// have besides PREINITIALIZED. The reusable copy commands acquire the image
/// GENERAL -> GENERAL from the external queue family, so nothing is discarded.
pub const INITIAL_LAYOUT_UNDEFINED: u32 = 0;
/// One memory plane: every accepted format is single-plane.
pub const PLANE_COUNT: u32 = 1;

/// The device extension the explicit-modifier image requires.
pub const EXT_IMAGE_DRM_FORMAT_MODIFIER: &[u8] = b"VK_EXT_image_drm_format_modifier\0";

/// The `VkFormat` a DRM fourcc is stored as, or `None` for a fourcc the KMD does
/// not import. The X and A variants share a `VkFormat`: the copy moves the bytes,
/// and the alpha byte means whatever the scan-out does with it.
pub const fn vk_format_for_fourcc(fourcc: u32) -> Option<u32> {
    match fourcc {
        FOURCC_XRGB8888 | FOURCC_ARGB8888 => Some(VK_FORMAT_B8G8R8A8_UNORM),
        FOURCC_XBGR8888 | FOURCC_ABGR8888 => Some(VK_FORMAT_R8G8B8A8_UNORM),
        _ => None,
    }
}

/// `FcRefCode` for an image that needs more bytes than the resource has.
pub const CODE_UNDERSIZE: u32 = 9;
/// `FcRefCode` for an image whose `memoryTypeBits` allow no memory type.
pub const CODE_NO_MEMORY_TYPE: u32 = 10;

/// Why a foreign record cannot be imported as an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The record's layout is invalid, or needs more bytes than the resource has.
    Layout(LayoutError),
    /// A fourcc with no `VkFormat` here.
    Format,
    /// The layout's extent is not the extent being copied.
    Extent,
    /// The recorded size is zero.
    Size,
}

impl Refusal {
    /// A small stable code for the registry trace (`FcRefCode`). 1 to 8 are
    /// these; [`CODE_UNDERSIZE`] and [`CODE_NO_MEMORY_TYPE`] are the two the Venus
    /// half adds once the host has reported the image's requirements.
    pub const fn code(self) -> u32 {
        match self {
            Refusal::Layout(LayoutError::Dimensions) => 1,
            Refusal::Layout(LayoutError::Format) => 2,
            Refusal::Layout(LayoutError::Stride) => 3,
            Refusal::Layout(LayoutError::Modifier) => 4,
            Refusal::Layout(LayoutError::TooLarge) => 5,
            Refusal::Format => 6,
            Refusal::Extent => 7,
            Refusal::Size => 8,
        }
    }
}

/// Everything the explicit-modifier image and its memory import are built from.
/// A value of this type has been validated: its pieces are consistent with one
/// another and with the recorded size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForeignImage {
    pub vk_format: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    /// Plane 0 offset from the start of the resource, bytes.
    pub plane_offset: u64,
    /// `rowPitch` of plane 0, bytes.
    pub row_pitch: u64,
    /// The host-verified size of the resource (an upper bound for the image).
    pub record_size: u64,
}

impl ForeignImage {
    /// Build the import parameters from the foreign record.
    ///
    /// `layout` and `record_size` are the record (`VirtioGpu::foreign_record`);
    /// `width` and `height` are the extent being copied, which must be the
    /// layout's own (adoption already refuses an allocation whose geometry
    /// differs, so a mismatch here means the caller mixed two resources).
    pub fn from_record(
        layout: &Layout,
        record_size: u64,
        width: u32,
        height: u32,
    ) -> Result<Self, Refusal> {
        if record_size == 0 {
            return Err(Refusal::Size);
        }
        layout.validate_for(record_size).map_err(Refusal::Layout)?;
        let Some(vk_format) = vk_format_for_fourcc(layout.fourcc) else {
            return Err(Refusal::Format);
        };
        if layout.width != width || layout.height != height {
            return Err(Refusal::Extent);
        }
        Ok(Self {
            vk_format,
            width: layout.width,
            height: layout.height,
            modifier: layout.modifier,
            plane_offset: layout.offset as u64,
            row_pitch: layout.stride as u64,
            record_size,
        })
    }

    /// The `vkCreateImage` parameters: a 2D, one-mip, one-layer image with the
    /// DRM-format-modifier tiling and the explicit single-plane layout, external
    /// memory handle type DMA_BUF, transfer-source usage, UNDEFINED initial
    /// layout.
    pub const fn image_spec(&self) -> ImageCreateSpec {
        ImageCreateSpec {
            pnext: ImagePNext::ExternalMemoryDrmExplicit {
                handle_type: HANDLE_TYPE_DMA_BUF,
                modifier: self.modifier,
                plane_offset: self.plane_offset,
                row_pitch: self.row_pitch,
            },
            flags: 0,
            format: self.vk_format,
            width: self.width,
            height: self.height,
            tiling: crate::IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            usage: USAGE_TRANSFER_SRC,
            initial_layout: INITIAL_LAYOUT_UNDEFINED,
        }
    }

    /// The `vkAllocateMemory` parameters for the import: the virtio resource,
    /// dedicated to `image`, `size` bytes (see [`import_allocation_size`]), of
    /// the chosen memory type.
    pub const fn memory_spec(
        resource_id: u32,
        image: u64,
        size: u64,
        memory_type_index: u32,
    ) -> MemoryAllocateSpec {
        MemoryAllocateSpec {
            pnext: MemoryPNext::ImportResourceDedicated { resource_id, image },
            size,
            memory_type_index,
        }
    }
}

/// The size to allocate for the dedicated import, given what the host reports
/// the image requires and the recorded size of the resource.
///
/// Vulkan wants a dedicated allocation to be exactly the image's requirement, and
/// the host's check is `image size <= resource size` (docs/zero-copy-present.md
/// 10.5), so the requirement itself is what is allocated, provided it fits. An
/// image that needs more than the resource has is refused (it would read past the
/// blob): that is the undersize guard the OPTIMAL path has as
/// `required_size > source_allocation_size`.
pub const fn import_allocation_size(required: u64, record_size: u64) -> Option<u64> {
    if required == 0 || required > record_size {
        None
    } else {
        Some(required)
    }
}

/// Whether the foreign record read at open time still agrees with the record the
/// KMD holds now. The import is built from the open-time copy (it is on the
/// present path and takes no lock); this is the check, made once per import under
/// the table lock, that the copy was not stale or forged.
pub fn record_agrees(opened: &Layout, recorded: &Layout) -> bool {
    opened == recorded
}

/// The geometry an opener reads from the allocation's meta trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenMeta {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub plane_offset: u64,
}

/// The fields of the (already magic- and version-checked) layout trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenTrailer {
    pub modifier: u64,
    pub fourcc: u32,
    pub stride: u32,
    pub plane_offset: u32,
}

/// The foreign layout an opener may carry, from the meta and the trailer of one
/// allocation's private data; `None` unless every part is consistent.
///
/// The trailer's `stride` and `plane_offset` must equal the meta's (that is the
/// trailer's contract), the allocation must be a device-memory one (the only kind
/// that adopts a foreign resource), and the layout must pass the same rules a
/// foreign record does. This is a hint, not proof: the KMD always overwrites the
/// trailer of a foreign adoption, but an ordinary allocation's creator controls
/// those bytes and can forge a consistent one; the import checks the result
/// against the foreign table ([`record_agrees`]) before relying on it.
pub fn layout_from_open(
    device_memory: bool,
    meta: Option<OpenMeta>,
    trailer: Option<OpenTrailer>,
) -> Option<Layout> {
    if !device_memory {
        return None;
    }
    let meta = meta?;
    let trailer = trailer?;
    if trailer.stride != meta.pitch || u64::from(trailer.plane_offset) != meta.plane_offset {
        return None;
    }
    let layout = Layout {
        width: meta.width,
        height: meta.height,
        stride: trailer.stride,
        offset: trailer.plane_offset,
        fourcc: trailer.fourcc,
        modifier: trailer.modifier,
    };
    layout.validate().ok()?;
    Some(layout)
}

/// Whether the device bring-up should try the export trio plus
/// `VK_EXT_image_drm_format_modifier` first.
///
/// All three must hold: the `ForeignCopy` knob (default off; set 1) allows it, the
/// adapter is the display half (the only shape that has a scan-out copy at all),
/// and the host serves `IMPORT_RM` (otherwise no foreign resource can ever exist
/// and the extension would only add the 38th-session risk to a device that
/// has no use for it).
pub const fn modifier_tier_wanted(
    knob_enabled: bool,
    display_half: bool,
    host_serves_rm_import: bool,
) -> bool {
    knob_enabled && display_half && host_serves_rm_import
}

/// The tier the `CreateDevice` ladder starts at: 0 = export trio + modifier,
/// 1 = export trio (the production device before this change), 2 = no
/// extensions. Tier numbers 1 and 2 keep their old meaning (`SdgDevX`).
pub const fn ladder_start_tier(want_scanout_exts: bool, want_modifier: bool) -> u32 {
    if !want_scanout_exts {
        2
    } else if want_modifier {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::foreign_resource::{MOD_LINEAR, MOD_NVIDIA_BLOCK_LINEAR_BASE};
    use crate::{encode_image_create, encode_memory_allocate, Writer, MAX_CMD_BYTES};
    use std::vec::Vec;

    fn layout_1080p(fourcc: u32, modifier: u64) -> Layout {
        Layout {
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            fourcc,
            modifier,
        }
    }

    const SIZE_1080P: u64 = 0x7f_0000;

    /// What RM would allocate for `l`: its lower bound rounded up to 64 KiB.
    fn rm_size(l: &Layout) -> u64 {
        (l.min_bytes() + 0xffff) & !0xffff
    }

    #[test]
    fn fourcc_to_vk_format() {
        assert_eq!(vk_format_for_fourcc(FOURCC_XRGB8888), Some(44));
        assert_eq!(vk_format_for_fourcc(FOURCC_ARGB8888), Some(44));
        assert_eq!(vk_format_for_fourcc(FOURCC_XBGR8888), Some(37));
        assert_eq!(vk_format_for_fourcc(FOURCC_ABGR8888), Some(37));
        // DRM_FORMAT_RGB565, NV12, and zero.
        assert_eq!(vk_format_for_fourcc(0x3631_4752), None);
        assert_eq!(vk_format_for_fourcc(0x3231_564e), None);
        assert_eq!(vk_format_for_fourcc(0), None);
    }

    /// The four fourccs a validated `Layout` can carry all have a format, so the
    /// `Format` refusal in `from_record` is unreachable after `validate_for` and
    /// is a belt-and-braces check, not a second rule.
    #[test]
    fn every_valid_layout_fourcc_has_a_format() {
        for f in [
            FOURCC_XRGB8888,
            FOURCC_ARGB8888,
            FOURCC_XBGR8888,
            FOURCC_ABGR8888,
        ] {
            assert!(vk_format_for_fourcc(f).is_some());
        }
    }

    #[test]
    fn nvk_swapchain_image_builds() {
        // 1080p NVK swapchain image on GB202: modifier ...6015 (h = 5).
        let l = layout_1080p(FOURCC_ARGB8888, MOD_NVIDIA_BLOCK_LINEAR_BASE | 5);
        let size = rm_size(&l);
        let img = ForeignImage::from_record(&l, size, 1920, 1080).unwrap();
        assert_eq!(img.vk_format, VK_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(img.modifier, 0x0300_0000_0060_6015);
        assert_eq!(img.plane_offset, 0);
        assert_eq!(img.row_pitch, 7680);
        assert_eq!(img.record_size, size);
        let spec = img.image_spec();
        assert_eq!(spec.tiling, 1000158000);
        assert_eq!(spec.usage, 1);
        assert_eq!(spec.flags, 0);
        assert_eq!(spec.initial_layout, 0);
        assert_eq!((spec.width, spec.height), (1920, 1080));
        assert!(
            spec.pnext
                == ImagePNext::ExternalMemoryDrmExplicit {
                    handle_type: 0x200,
                    modifier: 0x0300_0000_0060_6015,
                    plane_offset: 0,
                    row_pitch: 7680,
                }
        );
    }

    #[test]
    fn every_accepted_modifier_builds() {
        for h in 0..=5u64 {
            let l = layout_1080p(FOURCC_XRGB8888, MOD_NVIDIA_BLOCK_LINEAR_BASE + h);
            assert!(ForeignImage::from_record(&l, rm_size(&l), 1920, 1080).is_ok());
            // ... and not one byte under its lower bound.
            assert_eq!(
                ForeignImage::from_record(&l, l.min_bytes() - 1, 1920, 1080),
                Err(Refusal::Layout(LayoutError::TooLarge))
            );
        }
        let l = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        assert!(ForeignImage::from_record(&l, SIZE_1080P, 1920, 1080).is_ok());
    }

    #[test]
    fn modifiers_outside_the_family_are_refused() {
        for m in [
            1u64,
            MOD_NVIDIA_BLOCK_LINEAR_BASE - 1,
            MOD_NVIDIA_BLOCK_LINEAR_BASE + 6,
            // Intel X-tiling, and "invalid".
            0x0100_0000_0000_0001,
            0x00ff_ffff_ffff_ffff,
        ] {
            let l = layout_1080p(FOURCC_XRGB8888, m);
            assert_eq!(
                ForeignImage::from_record(&l, SIZE_1080P, 1920, 1080),
                Err(Refusal::Layout(LayoutError::Modifier)),
                "modifier {m:#x}"
            );
        }
    }

    #[test]
    fn layout_must_fit_the_recorded_size() {
        let l = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        // Exactly the lower bound is fine, one byte less is not.
        let min = l.min_bytes();
        assert!(ForeignImage::from_record(&l, min, 1920, 1080).is_ok());
        assert_eq!(
            ForeignImage::from_record(&l, min - 1, 1920, 1080),
            Err(Refusal::Layout(LayoutError::TooLarge))
        );
        assert_eq!(
            ForeignImage::from_record(&l, 0, 1920, 1080),
            Err(Refusal::Size)
        );
    }

    #[test]
    fn plane_offset_counts_toward_the_size() {
        let mut l = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        l.offset = 0x1000;
        let min = l.min_bytes();
        assert_eq!(min, 0x1000 + 7680 * 1080);
        let img = ForeignImage::from_record(&l, min, 1920, 1080).unwrap();
        assert_eq!(img.plane_offset, 0x1000);
        assert_eq!(
            ForeignImage::from_record(&l, min - 1, 1920, 1080),
            Err(Refusal::Layout(LayoutError::TooLarge))
        );
    }

    #[test]
    fn extent_must_be_the_copied_extent() {
        let l = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        assert_eq!(
            ForeignImage::from_record(&l, SIZE_1080P, 1920, 1079),
            Err(Refusal::Extent)
        );
        assert_eq!(
            ForeignImage::from_record(&l, SIZE_1080P, 1919, 1080),
            Err(Refusal::Extent)
        );
    }

    #[test]
    fn bad_stride_and_format_are_refused_by_the_layout_rules() {
        let mut l = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        l.stride = 7679;
        assert_eq!(
            ForeignImage::from_record(&l, SIZE_1080P, 1920, 1080),
            Err(Refusal::Layout(LayoutError::Stride))
        );
        let l = layout_1080p(0x3631_4752, MOD_LINEAR);
        assert_eq!(
            ForeignImage::from_record(&l, SIZE_1080P, 1920, 1080),
            Err(Refusal::Layout(LayoutError::Format))
        );
    }

    #[test]
    fn refusal_codes_are_distinct_and_nonzero() {
        let all = [
            Refusal::Layout(LayoutError::Dimensions),
            Refusal::Layout(LayoutError::Format),
            Refusal::Layout(LayoutError::Stride),
            Refusal::Layout(LayoutError::Modifier),
            Refusal::Layout(LayoutError::TooLarge),
            Refusal::Format,
            Refusal::Extent,
            Refusal::Size,
        ];
        assert_ne!(CODE_UNDERSIZE, CODE_NO_MEMORY_TYPE);
        for (i, a) in all.iter().enumerate() {
            assert_ne!(a.code(), 0);
            assert_ne!(a.code(), CODE_UNDERSIZE);
            assert_ne!(a.code(), CODE_NO_MEMORY_TYPE);
            for b in &all[i + 1..] {
                assert_ne!(a.code(), b.code());
            }
        }
    }

    #[test]
    fn import_size_is_the_requirement_when_it_fits() {
        assert_eq!(
            import_allocation_size(0x7e_9000, SIZE_1080P),
            Some(0x7e_9000)
        );
        assert_eq!(
            import_allocation_size(SIZE_1080P, SIZE_1080P),
            Some(SIZE_1080P)
        );
        // Needs more than the resource has: refused (the undersize guard).
        assert_eq!(import_allocation_size(SIZE_1080P + 1, SIZE_1080P), None);
        assert_eq!(import_allocation_size(0, SIZE_1080P), None);
    }

    #[test]
    fn open_time_copy_must_match_the_record() {
        let a = layout_1080p(FOURCC_XRGB8888, MOD_LINEAR);
        assert!(record_agrees(&a, &a));
        let mut b = a;
        b.modifier = MOD_NVIDIA_BLOCK_LINEAR_BASE;
        assert!(!record_agrees(&a, &b));
        let mut c = a;
        c.stride += 4;
        assert!(!record_agrees(&a, &c));
        let mut d = a;
        d.fourcc = FOURCC_XBGR8888;
        assert!(!record_agrees(&a, &d));
    }

    fn open_fixture() -> (OpenMeta, OpenTrailer) {
        (
            OpenMeta {
                width: 1920,
                height: 1080,
                pitch: 7680,
                plane_offset: 0,
            },
            OpenTrailer {
                modifier: MOD_NVIDIA_BLOCK_LINEAR_BASE | 5,
                fourcc: FOURCC_ARGB8888,
                stride: 7680,
                plane_offset: 0,
            },
        )
    }

    #[test]
    fn open_time_layout_is_the_trailer_with_the_meta_extent() {
        let (m, t) = open_fixture();
        assert_eq!(
            layout_from_open(true, Some(m), Some(t)),
            Some(Layout {
                width: 1920,
                height: 1080,
                stride: 7680,
                offset: 0,
                fourcc: FOURCC_ARGB8888,
                modifier: 0x0300_0000_0060_6015,
            })
        );
    }

    #[test]
    fn open_time_layout_refuses_inconsistent_or_missing_parts() {
        let (m, t) = open_fixture();
        // Not a device-memory allocation, or a part missing.
        assert_eq!(layout_from_open(false, Some(m), Some(t)), None);
        assert_eq!(layout_from_open(true, None, Some(t)), None);
        assert_eq!(layout_from_open(true, Some(m), None), None);
        // The trailer repeats the meta's pitch and offset; a disagreement is
        // not a layout.
        let mut t2 = t;
        t2.stride += 4;
        assert_eq!(layout_from_open(true, Some(m), Some(t2)), None);
        let mut t3 = t;
        t3.plane_offset = 0x1000;
        assert_eq!(layout_from_open(true, Some(m), Some(t3)), None);
        let mut m2 = m;
        m2.plane_offset = 0x1_0000_0000; // does not even fit the trailer's u32
        assert_eq!(layout_from_open(true, Some(m2), Some(t)), None);
        // The foreign layout rules still apply.
        let mut t4 = t;
        t4.modifier = 0x0100_0000_0000_0001;
        assert_eq!(layout_from_open(true, Some(m), Some(t4)), None);
        let mut t5 = t;
        t5.fourcc = 0x3631_4752;
        assert_eq!(layout_from_open(true, Some(m), Some(t5)), None);
        let mut m3 = m;
        m3.width = 0;
        assert_eq!(layout_from_open(true, Some(m3), Some(t)), None);
    }

    #[test]
    fn modifier_tier_needs_all_three() {
        for knob in [false, true] {
            for half in [false, true] {
                for host in [false, true] {
                    assert_eq!(modifier_tier_wanted(knob, half, host), knob && half && host);
                }
            }
        }
    }

    #[test]
    fn ladder_start_tiers() {
        // Without the knob, or without the host feature, the production device
        // is exactly the one from before this change (tier 1 / tier 2).
        assert_eq!(ladder_start_tier(true, false), 1);
        assert_eq!(ladder_start_tier(false, false), 2);
        assert_eq!(ladder_start_tier(true, true), 0);
        // A render-only adapter never gets the modifier tier, whatever else.
        assert_eq!(ladder_start_tier(false, true), 2);
    }

    // ── encoding ───────────────────────────────────────────────────────────

    const DEV: u64 = 0x1111_2222_3333_4444;
    const IMG: u64 = 0x9999_aaaa_bbbb_cccc;
    const MEM: u64 = 0x5555_6666_7777_8888;

    fn u32le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn u64le(v: u64) -> [u8; 8] {
        v.to_le_bytes()
    }

    /// The whole `vkCreateImage` stream, written out field by field from
    /// `vn_encode_VkImageCreateInfo` (host/venus/third_party/build/venus-protocol,
    /// vn_protocol_driver_image.h): header, device, pCreateInfo, sType, then the
    /// pNext chain external-memory -> explicit-modifier (inner struct first),
    /// the create info body, allocator, image handle.
    #[test]
    fn explicit_modifier_image_create_bytes() {
        let l = layout_1080p(FOURCC_ARGB8888, MOD_NVIDIA_BLOCK_LINEAR_BASE | 5);
        let img = ForeignImage::from_record(&l, rm_size(&l), 1920, 1080).unwrap();
        let w = encode_image_create(DEV, IMG, &img.image_spec());

        let mut e: Vec<u8> = Vec::new();
        e.extend(u32le(54)); // vkCreateImage
        e.extend(u32le(1)); // GENERATE_REPLY
        e.extend(u64le(DEV));
        e.extend(u64le(1)); // pCreateInfo
        e.extend(u32le(14)); // VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO
                             // pNext: VkExternalMemoryImageCreateInfo
        e.extend(u64le(1));
        e.extend(u32le(1000072001));
        //   its pNext: VkImageDrmFormatModifierExplicitCreateInfoEXT
        e.extend(u64le(1));
        e.extend(u32le(1000158004));
        e.extend(u64le(0)); // end of chain
        e.extend(u64le(0x0300_0000_0060_6015)); // drmFormatModifier
        e.extend(u32le(1)); // drmFormatModifierPlaneCount
        e.extend(u64le(1)); // pPlaneLayouts array_size
        e.extend(u64le(0)); // offset
        e.extend(u64le(0)); // size
        e.extend(u64le(7680)); // rowPitch
        e.extend(u64le(0)); // arrayPitch
        e.extend(u64le(0)); // depthPitch
        e.extend(u32le(0x200)); // handleTypes (external struct, after the nested one)
                                // VkImageCreateInfo body
        e.extend(u32le(0)); // flags
        e.extend(u32le(1)); // VK_IMAGE_TYPE_2D
        e.extend(u32le(44)); // B8G8R8A8_UNORM
        e.extend(u32le(1920));
        e.extend(u32le(1080));
        e.extend(u32le(1)); // depth
        e.extend(u32le(1)); // mipLevels
        e.extend(u32le(1)); // arrayLayers
        e.extend(u32le(1)); // samples
        e.extend(u32le(1000158000)); // VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT
        e.extend(u32le(1)); // TRANSFER_SRC
        e.extend(u32le(0)); // EXCLUSIVE
        e.extend(u32le(0)); // queueFamilyIndexCount
        e.extend(u64le(0)); // pQueueFamilyIndices
        e.extend(u32le(0)); // UNDEFINED
        e.extend(u64le(0)); // pAllocator
        e.extend(u64le(1)); // pImage
        e.extend(u64le(IMG));
        assert_eq!(w.finished(), Some(e.as_slice()));
        assert!(e.len() <= MAX_CMD_BYTES);
    }

    /// `vkAllocateMemory` with `VkImportMemoryResourceInfoMESA` -> dedicated.
    #[test]
    fn dedicated_resource_import_bytes() {
        let spec = ForeignImage::memory_spec(0x1234, IMG, 0x7e_9000, 2);
        let w = encode_memory_allocate(DEV, MEM, &spec);

        let mut e: Vec<u8> = Vec::new();
        e.extend(u32le(21)); // vkAllocateMemory
        e.extend(u32le(1));
        e.extend(u64le(DEV));
        e.extend(u64le(1)); // pAllocateInfo
        e.extend(u32le(5)); // MEMORY_ALLOCATE_INFO
                            // pNext: VkImportMemoryResourceInfoMESA
        e.extend(u64le(1));
        e.extend(u32le(1000384002));
        //   its pNext: VkMemoryDedicatedAllocateInfo
        e.extend(u64le(1));
        e.extend(u32le(1000127001));
        e.extend(u64le(0)); // end of chain
        e.extend(u64le(IMG)); // image
        e.extend(u64le(0)); // buffer
        e.extend(u32le(0x1234)); // resourceId (after the nested one)
        e.extend(u64le(0x7e_9000)); // allocationSize
        e.extend(u32le(2)); // memoryTypeIndex
        e.extend(u64le(0)); // pAllocator
        e.extend(u64le(1)); // pMemory
        e.extend(u64le(MEM));
        assert_eq!(w.finished(), Some(e.as_slice()));
    }

    /// The new variants must not move any existing encoding: the two neighbours
    /// the foreign import is built from.
    #[test]
    fn existing_variants_still_encode_as_before() {
        let plain_import = encode_memory_allocate(
            DEV,
            MEM,
            &MemoryAllocateSpec {
                pnext: MemoryPNext::ImportResource { resource_id: 7 },
                size: 0x1000,
                memory_type_index: 0,
            },
        );
        let mut e: Vec<u8> = Vec::new();
        e.extend(u32le(21));
        e.extend(u32le(1));
        e.extend(u64le(DEV));
        e.extend(u64le(1));
        e.extend(u32le(5));
        e.extend(u64le(1));
        e.extend(u32le(1000384002));
        e.extend(u64le(0));
        e.extend(u32le(7));
        e.extend(u64le(0x1000));
        e.extend(u32le(0));
        e.extend(u64le(0));
        e.extend(u64le(1));
        e.extend(u64le(MEM));
        assert_eq!(plain_import.finished(), Some(e.as_slice()));
    }

    /// The tier-0 device list is the old export trio plus the one extension; it
    /// must still fit the command buffer.
    #[test]
    fn tier0_extension_list_fits_a_create_device_stream() {
        let exts: [&[u8]; 4] = [
            b"VK_KHR_external_memory\0",
            b"VK_KHR_external_memory_fd\0",
            b"VK_EXT_external_memory_dma_buf\0",
            EXT_IMAGE_DRM_FORMAT_MODIFIER,
        ];
        let mut w = Writer::new();
        w.header(11, 1);
        w.u64(0xdead_beef);
        w.count(true);
        w.i32(3);
        w.u64(0);
        w.u32(0);
        w.u32(1);
        w.count(true);
        w.i32(2);
        w.u64(0);
        w.u32(0);
        w.u32(0);
        w.u32(1);
        w.count(true);
        w.f32(1.0);
        w.u32(0);
        w.count(false);
        w.u32(exts.len() as u32);
        w.u64(exts.len() as u64);
        for e in exts {
            w.u64(e.len() as u64);
            w.bytes_padded(e);
        }
        w.count(false);
        w.count(false);
        w.count(true);
        w.u64(0x1234);
        assert!(w.finished().is_some(), "len {}", w.len());
        assert!(w.len() <= MAX_CMD_BYTES);
    }

    #[test]
    fn modifier_extension_name_is_nul_terminated() {
        assert_eq!(
            EXT_IMAGE_DRM_FORMAT_MODIFIER,
            b"VK_EXT_image_drm_format_modifier\0"
        );
    }
}
