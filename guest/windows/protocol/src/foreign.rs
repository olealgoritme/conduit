//! `HELIOS_ESCAPE_FOREIGN_RESOURCE` — admit a resource the KMD did not create
//! to the KMD's resource tables, so it can be presented without a CPU copy.
//!
//! The case it exists for: an NVK-on-RM client (`HELIOS_ESCAPE_NVRM`) renders a
//! swapchain image into RM memory. The host exports that memory (nvidia-drm GEM
//! object, then dma-buf) and imports it into the Venus renderer as a blob
//! resource, and every later step of presentation is the one Venus images already
//! take: attach to a context, adopt into a WDDM allocation, bind to scanout.
//! Those steps key off the KMD's resource and blob tables, so the one thing that
//! was missing is a way to put a resource into them that the KMD did not create
//! through `ALLOC_BLOB`. This is that way.
//!
//! Design note: `guest/windows/docs/zero-copy-present.md`. Read it before
//! changing anything here; the rules below are the short form.
//!
//! # Rules
//!
//! * **The KMD mints the resource id.** User mode never supplies one and never
//!   learns one from anywhere but this reply. A caller-supplied or host-minted
//!   id would be a number the KMD cannot tell from another process's resource.
//! * **The caller proves ownership of the source.** `rm_handle` must be a
//!   backend handle the calling device opened through `HELIOS_ESCAPE_NVRM`
//!   and it must be a DRM file (`device_type >= 512`); `ctx_id` must be a Venus
//!   context the calling device created. `gem_handle` is checked by the host
//!   against that DRM file's GEM table.
//! * **The size is a claim the host verifies.** The host refuses an import
//!   whose `size` exceeds the exported object, so the size the KMD records is a
//!   bound it can rely on.
//! * **No CPU view.** A foreign resource cannot be mapped (`MAP_BLOB` refuses
//!   it): there is no byte of it the guest CPU can touch.
//! * **Lifetime.** The resource belongs to the importing device until a WDDM
//!   allocation adopts it (`adopt_resource_id`, existing path), or until
//!   `RELEASE_BLOB`, `DestroyDevice` or `StopDevice`. Closing the RM handle does
//!   NOT release it: the host import holds its own reference to the memory.
//!
//! # Capability
//!
//! `OP_QUERY_CAPS` answers on every KMD that knows the verb. `CAP_RM_IMPORT` is
//! set only when the whole path is usable, which needs a host that serves the
//! import. `IMPORT_RM` on a KMD or host without it returns [`HELIOS_FOREIGN_ST_UNSUPPORTED`]
//! and changes nothing. An older KMD does not know the verb and answers
//! `STATUS_NOT_IMPLEMENTED`: that is the probe.
//!
//! # Layout
//!
//! Same rules as `nvrm.rs`: `repr(C)`, padding-free, 8-byte aligned, no
//! pointers, identical for 32-bit and 64-bit callers. Every request starts with
//! [`HeliosForeignHeader`] (40 bytes, like `HeliosNvrmHeader`). Result
//! reporting is also the same three layers: the escape's NTSTATUS for a
//! malformed request, `HeliosForeignHeader.status` for the KMD's verdict on a
//! well-formed one, and nothing of the host's reply is interpreted beyond
//! success or failure.

use crate::HeliosEscapeHeader;
use bytemuck::{Pod, Zeroable};

/// The escape verb. `0x0013` is the producer, `0x0014` is retired, `0x0015` is
/// snapshot status, `0x0016` is NVRM, `0x0017` is the submit batch.
pub const HELIOS_ESCAPE_FOREIGN_RESOURCE: u32 = 0x0018;
/// Version of this ABI, carried in every request. Additive changes (ops, status
/// codes, flag bits) do not bump it; `QUERY_CAPS` is how a client finds them.
pub const HELIOS_FOREIGN_ABI_VERSION: u32 = 1;

/// Report capabilities and limits. See [`HeliosForeignQueryCaps`].
pub const HELIOS_FOREIGN_OP_QUERY_CAPS: u32 = 1;
/// Import an RM-exported GEM object as a Venus resource.
/// See [`HeliosForeignImportRm`].
pub const HELIOS_FOREIGN_OP_IMPORT_RM: u32 = 2;
/// Make a GEM handle, in the caller's own host DRM file, for an RM-export
/// resource the caller created or opened (a second process's way to the memory
/// another NVK process rendered into). See [`HeliosForeignRmResourceImport`].
pub const HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT: u32 = 3;

/// `QueryCaps.caps_flags`: `IMPORT_RM` is served end to end (KMD gate open and
/// the host has the matching blob type).
pub const HELIOS_FOREIGN_CAP_RM_IMPORT: u32 = 1 << 0;
/// `QueryCaps.caps_flags`: this KMD lets OTHER processes open an adopted foreign
/// allocation (`D3DKMTOpenResource` / `OpenResourceFromNtHandle` of a shared
/// allocation whose `blob_mem` is `HELIOS_BLOB_MEM_RM_EXPORT`). It sets
/// [`crate::HELIOS_WDDM_OPEN_FLAG_FOREIGN`] in the open identity, rewrites the
/// [`crate::HeliosWddmAllocLayout`] trailer at every open from its own record,
/// and keeps the host resource alive until the last of the adopting allocation's
/// destroy and every open's close. Independent of [`HELIOS_FOREIGN_CAP_RM_IMPORT`]
/// (that one needs the host; this one is KMD-only). A producer that is about to
/// share a foreign allocation should require it: an older KMD would open it
/// without the flag. See `guest/windows/docs/shared-foreign-surfaces.md`.
pub const HELIOS_FOREIGN_CAP_SHARED_OPEN: u32 = 1 << 1;
/// `QueryCaps.caps_flags`: `RM_RESOURCE_IMPORT` is served end to end: this KMD
/// knows the op and the host serves `RmResourceImport` (config features bit 14,
/// with bits 13 and 10). Needs the host, unlike
/// [`HELIOS_FOREIGN_CAP_SHARED_OPEN`]. Without it the op answers
/// [`HELIOS_FOREIGN_ST_UNSUPPORTED`] and touches nothing.
pub const HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT: u32 = 1 << 2;
/// `QueryCaps.caps_flags`: the layout record takes every format of
/// [`share_format`] (not only the four 32-bit RGB ones), the
/// [`HELIOS_FOREIGN_IMPORT_FLAG_PLANE1`] tail for two-plane formats, and the KMD
/// writes the version-2 trailer ([`crate::HELIOS_WDDM_LAYOUT_VERSION_PLANES`],
/// plane 1 at [`crate::HELIOS_WDDM_LAYOUT_PLANE1_OFFSET`]) for a two-plane
/// record. KMD-only (the host moves one object and never looks at its format).
/// Without it a client mints ids for the 32-bit RGB formats only.
/// `guest/windows/docs/shared-formats.md`.
pub const HELIOS_FOREIGN_CAP_LAYOUT_FORMATS: u32 = 1 << 3;

pub const HELIOS_FOREIGN_ST_OK: i32 = 0;
/// The op is valid in this ABI but not served: `CAP_RM_IMPORT` is not set.
pub const HELIOS_FOREIGN_ST_UNSUPPORTED: i32 = 1;
/// `rm_handle` is not a backend handle this device opened, or is not a DRM
/// file. One code, so a process learns nothing about another's handles.
pub const HELIOS_FOREIGN_ST_NOT_OWNED: i32 = 2;
/// `ctx_id` is not a Venus context this device created.
pub const HELIOS_FOREIGN_ST_BAD_CONTEXT: i32 = 3;
/// A field is out of range: a zero id, nonzero `flags`, a `size` that is zero,
/// not a page multiple or over the per-resource limit.
pub const HELIOS_FOREIGN_ST_BAD_RANGE: i32 = 4;
/// A quota is exhausted: the table, this device's count, or this device's bytes.
pub const HELIOS_FOREIGN_ST_NO_RESOURCES: i32 = 5;
/// The host or the transport refused the import (no such GEM object, `size`
/// over the object, device gone). `out_host_errno` has the host's errno when it
/// gave one.
pub const HELIOS_FOREIGN_ST_DEVICE_ERROR: i32 = 6;

/// `RESOURCE_CREATE_BLOB.blob_mem` the KMD sends for `IMPORT_RM`. Vendor
/// range (bit 31 set); the standard values are 1 to 3. A host that does not
/// know it refuses the command, which the KMD reports as `DEVICE_ERROR`.
///
/// `blob_id` is `(rm_handle << 32) | gem_handle`
/// (`helios_kmd_logic::foreign_resource::foreign_blob_id`), `size` is the
/// claimed size, `blob_flags` is 0, and the resource is attached to `ctx_id`
/// by the ordinary `CTX_ATTACH_RESOURCE` that follows.
pub const HELIOS_BLOB_MEM_RM_EXPORT: u32 = 0x8000_0001;

/// First 40 bytes of every `HELIOS_ESCAPE_FOREIGN_RESOURCE` buffer; the same
/// shape as `HeliosNvrmHeader`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignHeader {
    /// `cmd_type = HELIOS_ESCAPE_FOREIGN_RESOURCE`, `size` = total buffer bytes.
    pub hdr: HeliosEscapeHeader,
    /// in: [`HELIOS_FOREIGN_ABI_VERSION`].
    pub abi_version: u32,
    /// in: one of `HELIOS_FOREIGN_OP_*`.
    pub op: u32,
    /// out: one of `HELIOS_FOREIGN_ST_*`. Written on every success return.
    pub status: i32,
    /// in: zero.
    pub reserved: u32,
    /// out: the device generation, the same value as `HeliosNvrmHeader.epoch`.
    /// A resource of an earlier epoch is gone.
    pub epoch: u64,
}

pub const HELIOS_FOREIGN_HEADER_BYTES: usize = 40;

/// `QUERY_CAPS`. 96 bytes, no trailing data. Touches no device state beyond
/// reading the tables.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignQueryCaps {
    pub head: HeliosForeignHeader,
    /// out: bit `n` set <=> `HELIOS_FOREIGN_OP_*` value `n` is implemented by
    /// this KMD. (Whether the op is also served is `caps_flags`.)
    pub supported_ops: u64,
    /// out: `HELIOS_FOREIGN_CAP_*`.
    pub caps_flags: u32,
    /// out: most resources one device may hold.
    pub max_per_owner: u32,
    /// out: most resources across every process.
    pub max_total: u32,
    /// out: zero.
    pub reserved0: u32,
    /// out: largest single resource, in bytes.
    pub max_bytes_per_resource: u64,
    /// out: most bytes one device may hold.
    pub max_bytes_per_owner: u64,
    /// out: foreign resources live now, across every process.
    pub live_total: u32,
    /// out: of those, the ones the calling device created and no allocation
    /// has adopted (what its quota counts).
    pub live_owner: u32,
    /// out: imports completed since the transport started.
    pub imported: u32,
    /// out: requests refused since the transport started, for any reason.
    pub refused: u32,
}

pub const HELIOS_FOREIGN_QUERY_CAPS_BYTES: usize = 96;

/// `IMPORT_RM.flags` bit: a [`HeliosForeignLayout`] follows the 72-byte request
/// ([`HeliosForeignImportRmLayout`], 104 bytes). **The layout is not optional**:
/// a request without this bit (the 72-byte form) is refused `BAD_RANGE`, because
/// a foreign resource whose layout the KMD does not know cannot be scanned out
/// or imported without guessing, and the guess (from the size) is wrong for
/// heights that are a whole number of blocks. The 72-byte struct itself is
/// unchanged so a client can still build it as the prefix of the 104-byte one.
pub const HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT: u32 = 1 << 0;

/// `IMPORT_RM.flags` bit (with [`HELIOS_FOREIGN_CAP_LAYOUT_FORMATS`]): plane 1
/// of a two-plane format follows the layout ([`HeliosForeignImportRmPlanes`],
/// 120 bytes). Only together with [`HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT`]; set iff
/// the fourcc has two planes ([`ShareFormat::planes`]).
pub const HELIOS_FOREIGN_IMPORT_FLAG_PLANE1: u32 = 1 << 1;

/// `IMPORT_RM`. 72 bytes, followed by the 32-byte [`HeliosForeignLayout`] when
/// [`HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT`] is set (always, in a served gate).
///
/// `rm_handle` is the backend handle (from an `Open` of a DRM node) in whose
/// file `gem_handle` was created, typically by `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`
/// on RM memory exported with `OS_UNIX_EXPORT_OBJECT_TO_FD`. `size` is the byte
/// size of the exported object (its RM allocation size), a page multiple.
///
/// On success `out_resource_id` names a Venus resource attached to `ctx_id`. It
/// is usable with `ATTACH_RESOURCE` (other contexts), as `adopt_resource_id` in
/// `D3DKMTCreateAllocation` private data, in present snapshot descriptors, and
/// with `RELEASE_BLOB`; it is refused by `MAP_BLOB`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignImportRm {
    pub head: HeliosForeignHeader,
    /// in: a Venus context the calling device created (`CTX_CREATE`).
    pub ctx_id: u32,
    /// in: backend handle of the DRM file, owned by the calling device.
    pub rm_handle: u32,
    /// in: GEM handle in that file.
    pub gem_handle: u32,
    /// in: [`HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT`]; every other bit zero (a later
    /// revision may add sync hints).
    pub flags: u32,
    /// in: bytes of the exported object, a page multiple, at most
    /// `max_bytes_per_resource`.
    pub size: u64,
    /// out: the resource id the KMD minted. Zero unless `status == ST_OK`.
    pub out_resource_id: u32,
    /// out: the host's errno when `status == ST_DEVICE_ERROR` and the host
    /// said, otherwise zero.
    pub out_host_errno: u32,
}

pub const HELIOS_FOREIGN_IMPORT_RM_BYTES: usize = 72;

/// What is inside the exported object. 32 bytes. Plane 0 only. The KMD validates
/// it against `size` once (`helios_kmd_logic::foreign_resource::Layout`) and
/// records it with the resource; the allocation that adopts the resource must
/// repeat it (`HeliosWddmAllocMeta` geometry plus `HeliosWddmAllocLayout`).
///
/// Accepted: `fourcc` one of `DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888`; `modifier`
/// `DRM_FORMAT_MOD_LINEAR` (0) or `0x0300000000606010 | h`, `h` in `0..=5` (what
/// NVK builds for B8G8R8A8 / R8G8B8A8); `width`/`height` in `1..=16384`;
/// `stride` a multiple of 4, at least `width * 4`, at most 1 MiB; and
/// `offset + stride * rows <= size` where `rows` is `height` (LINEAR) or `height`
/// rounded up to `8 << h` (block-linear). The last is a lower bound on the image,
/// never an equality: RM rounds allocations up to 64 KiB.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignLayout {
    /// in: pixels, 1..=16384.
    pub width: u32,
    /// in: rows, 1..=16384.
    pub height: u32,
    /// in: plane 0 pitch in bytes (`rowPitch`).
    pub stride: u32,
    /// in: plane 0 offset in bytes from the start of the object.
    pub offset: u32,
    /// in: `DRM_FORMAT_*`.
    pub fourcc: u32,
    /// in: zero.
    pub reserved: u32,
    /// in: `DRM_FORMAT_MOD_*`.
    pub modifier: u64,
}

pub const HELIOS_FOREIGN_LAYOUT_BYTES: usize = 32;

/// `IMPORT_RM` with its layout: the request a served gate requires. 104 bytes.
/// Only the 72-byte `base` is written back (`out_resource_id`,
/// `out_host_errno`, the header's `status` and `epoch`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignImportRmLayout {
    pub base: HeliosForeignImportRm,
    pub layout: HeliosForeignLayout,
}

pub const HELIOS_FOREIGN_IMPORT_RM_LAYOUT_BYTES: usize = 104;

/// One more plane of a foreign resource: plane 1 of NV12 / P010 / P016 (the
/// interleaved chroma, `width / 2` x `height / 2` texel pairs). 16 bytes. Its
/// own modifier because NVK picks the block height per plane extent; it must be
/// LINEAR iff plane 0's is, and from the same block-linear family otherwise.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosForeignPlane {
    /// `DRM_FORMAT_MOD_*` of this plane.
    pub modifier: u64,
    /// Row pitch in bytes.
    pub stride: u32,
    /// Byte offset from the start of the object; past plane 0.
    pub offset: u32,
}

pub const HELIOS_FOREIGN_PLANE_BYTES: usize = 16;

/// `IMPORT_RM` with layout and plane 1 ([`HELIOS_FOREIGN_IMPORT_FLAG_PLANE1`]).
/// 120 bytes. Only the 72-byte `base.base` is written back.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignImportRmPlanes {
    pub base: HeliosForeignImportRmLayout,
    pub plane1: HeliosForeignPlane,
}

pub const HELIOS_FOREIGN_IMPORT_RM_PLANES_BYTES: usize = 120;

// ---------------------------------------------------------------------------
// The formats a foreign resource may hold (HELIOS_FOREIGN_CAP_LAYOUT_FORMATS)
// ---------------------------------------------------------------------------
//
// What the Windows desktop and browsers share between processes, by DRM fourcc
// (the layout record's format code); guest/windows/docs/shared-formats.md.

pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
pub const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
pub const DRM_FORMAT_R8: u32 = 0x2020_3852;
pub const DRM_FORMAT_GR88: u32 = 0x3838_5247;
pub const DRM_FORMAT_R16: u32 = 0x2036_3152;
pub const DRM_FORMAT_GR1616: u32 = 0x3233_5247;
pub const DRM_FORMAT_RGB565: u32 = 0x3631_4752;
pub const DRM_FORMAT_ARGB1555: u32 = 0x3531_5241;
pub const DRM_FORMAT_ARGB4444: u32 = 0x3231_5241;
pub const DRM_FORMAT_ABGR2101010: u32 = 0x3033_4241;
pub const DRM_FORMAT_ABGR16161616F: u32 = 0x4834_4241;
pub const DRM_FORMAT_ABGR16161616: u32 = 0x3834_4241;
pub const DRM_FORMAT_YUYV: u32 = 0x5659_5559;
pub const DRM_FORMAT_NV12: u32 = 0x3231_564E;
pub const DRM_FORMAT_P010: u32 = 0x3031_3050;
pub const DRM_FORMAT_P016: u32 = 0x3631_3050;

/// How a [`share_format`] lays out its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareFormat {
    /// 1, or 2 for the 4:2:0 formats (plane 1: interleaved chroma at half
    /// width and half height).
    pub planes: u32,
    /// Bytes of one texel of plane 0 (YUYV: of one two-pixel group).
    pub bpp0: u32,
    /// Bytes of one texel of plane 1 (a chroma pair), 0 for one plane.
    pub bpp1: u32,
    /// Plane 0 texels per row = `width.div_ceil(hdiv0)` (2 for YUYV, else 1).
    pub hdiv0: u32,
    /// `width` must be even (4:2:2 and 4:2:0).
    pub even_width: bool,
    /// `height` must be even (4:2:0).
    pub even_height: bool,
}

impl ShareFormat {
    const fn one(bpp: u32) -> Self {
        Self { planes: 1, bpp0: bpp, bpp1: 0, hdiv0: 1, even_width: false, even_height: false }
    }

    const fn yuv420(bpp0: u32) -> Self {
        Self { planes: 2, bpp0, bpp1: bpp0 * 2, hdiv0: 1, even_width: true, even_height: true }
    }

    /// Bytes of one row of plane `plane` of a `width`-pixel image, unpadded.
    pub const fn row_bytes(&self, plane: u32, width: u32) -> u64 {
        if plane == 0 {
            (width.div_ceil(self.hdiv0) as u64) * self.bpp0 as u64
        } else {
            (width.div_ceil(2) as u64) * self.bpp1 as u64
        }
    }

    /// Rows of plane `plane` of a `height`-row image.
    pub const fn rows(&self, plane: u32, height: u32) -> u32 {
        if plane == 0 {
            height
        } else {
            height.div_ceil(2)
        }
    }

    /// The alignment a plane's stride must have: its texel size, at most 4.
    pub const fn stride_align(&self, plane: u32) -> u32 {
        let bpp = if plane == 0 { self.bpp0 } else { self.bpp1 };
        if bpp > 4 {
            4
        } else {
            bpp
        }
    }

    /// One of the four 32-bit RGB formats every KMD with `IMPORT_RM` takes.
    pub const fn is_rgb32(fourcc: u32) -> bool {
        matches!(
            fourcc,
            DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888
        )
    }
}

/// The layout facts of `fourcc`, or `None` for a format a foreign resource may
/// not hold. The four 32-bit RGB formats are always in; the rest need
/// [`HELIOS_FOREIGN_CAP_LAYOUT_FORMATS`].
pub const fn share_format(fourcc: u32) -> Option<ShareFormat> {
    Some(match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => {
            ShareFormat::one(4)
        }
        DRM_FORMAT_R8 => ShareFormat::one(1),
        DRM_FORMAT_GR88 | DRM_FORMAT_R16 | DRM_FORMAT_RGB565 | DRM_FORMAT_ARGB1555
        | DRM_FORMAT_ARGB4444 => ShareFormat::one(2),
        DRM_FORMAT_GR1616 | DRM_FORMAT_ABGR2101010 => ShareFormat::one(4),
        DRM_FORMAT_ABGR16161616F | DRM_FORMAT_ABGR16161616 => ShareFormat::one(8),
        DRM_FORMAT_YUYV => ShareFormat {
            planes: 1,
            bpp0: 4,
            bpp1: 0,
            hdiv0: 2,
            even_width: true,
            even_height: false,
        },
        DRM_FORMAT_NV12 => ShareFormat::yuv420(1),
        DRM_FORMAT_P010 | DRM_FORMAT_P016 => ShareFormat::yuv420(2),
        _ => return None,
    })
}

/// `RM_RESOURCE_IMPORT`. 80 bytes, no trailing data.
///
/// The caller is a process that OPENED an adopted foreign allocation (or the
/// device that imported the resource, before adoption) and holds `resource_id`
/// from the open identity. `rm_handle` is a DRM node (`device_type >= 512`) its
/// own device opened through `HELIOS_ESCAPE_NVRM`. On success the host has made
/// GEM object `out_gem_handle` in that file from the resource's dma-buf; NVK then
/// runs `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY` on it, `OS_UNIX_IMPORT_OBJECT_FROM_FD`
/// (`NV0000` `0x3d06`) into its RM client, and `DRM_IOCTL_GEM_CLOSE`, all through
/// `FORWARD`. The same resource on the same file answers the same handle, so one
/// close undoes any number of imports; the KMD records nothing (the host closes
/// the file's GEM handles when the file closes, which `DestroyDevice` does).
///
/// The KMD refuses, answering `NOT_OWNED` for all of them (one code, so a
/// process learns nothing about another's handles or resources): `rm_handle`
/// not this device's DRM node; no such foreign resource; its adopting allocation
/// destroyed; the caller neither its importer nor a process holding an open of
/// it. A zero `rm_handle` or `resource_id`, or nonzero `flags`, is `BAD_RANGE`.
/// Host errnos map as for `IMPORT_RM`: `EBADF`/`ENOENT` `NOT_OWNED`,
/// `EINVAL`/`ERANGE` `BAD_RANGE`, `EOPNOTSUPP` and `EPROTO` (a backend that
/// predates the message) `UNSUPPORTED`, `ENOMEM` `NO_RESOURCES`, anything else
/// `DEVICE_ERROR`; `out_host_errno` has the host's errno when it gave one.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosForeignRmResourceImport {
    pub head: HeliosForeignHeader,
    /// in: backend handle of a DRM file the calling device opened.
    pub rm_handle: u32,
    /// in: the resource id (from the open identity, or the `IMPORT_RM` reply).
    pub resource_id: u32,
    /// in: zero. Reserved for the host's request `flags`.
    pub flags: u32,
    /// out: GEM handle in `rm_handle`'s file. Zero unless `status == ST_OK`.
    pub out_gem_handle: u32,
    /// out: the object's size in bytes (the dma-buf's).
    pub out_size: u64,
    /// out: the modifier the resource was created with; zero unless
    /// [`HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER`] is set in `out_flags`.
    pub out_modifier: u64,
    /// out: `HELIOS_FOREIGN_RM_RESOURCE_IMPORT_*`.
    pub out_flags: u32,
    /// out: the host's errno when it refused and said so, otherwise zero.
    pub out_host_errno: u32,
}

pub const HELIOS_FOREIGN_RM_RESOURCE_IMPORT_BYTES: usize = 80;

/// `RM_RESOURCE_IMPORT.out_flags` bit 0: `out_modifier` is known.
pub const HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER: u32 = 1 << 0;

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosForeignHeader>() == HELIOS_FOREIGN_HEADER_BYTES);
    assert!(offset_of!(HeliosForeignHeader, abi_version) == 16);
    assert!(offset_of!(HeliosForeignHeader, op) == 20);
    assert!(offset_of!(HeliosForeignHeader, status) == 24);
    assert!(offset_of!(HeliosForeignHeader, reserved) == 28);
    assert!(offset_of!(HeliosForeignHeader, epoch) == 32);

    assert!(size_of::<HeliosForeignQueryCaps>() == HELIOS_FOREIGN_QUERY_CAPS_BYTES);
    assert!(offset_of!(HeliosForeignQueryCaps, supported_ops) == 40);
    assert!(offset_of!(HeliosForeignQueryCaps, caps_flags) == 48);
    assert!(offset_of!(HeliosForeignQueryCaps, max_per_owner) == 52);
    assert!(offset_of!(HeliosForeignQueryCaps, max_total) == 56);
    assert!(offset_of!(HeliosForeignQueryCaps, reserved0) == 60);
    assert!(offset_of!(HeliosForeignQueryCaps, max_bytes_per_resource) == 64);
    assert!(offset_of!(HeliosForeignQueryCaps, max_bytes_per_owner) == 72);
    assert!(offset_of!(HeliosForeignQueryCaps, live_total) == 80);
    assert!(offset_of!(HeliosForeignQueryCaps, live_owner) == 84);
    assert!(offset_of!(HeliosForeignQueryCaps, imported) == 88);
    assert!(offset_of!(HeliosForeignQueryCaps, refused) == 92);

    assert!(size_of::<HeliosForeignImportRm>() == HELIOS_FOREIGN_IMPORT_RM_BYTES);
    assert!(offset_of!(HeliosForeignImportRm, ctx_id) == 40);
    assert!(offset_of!(HeliosForeignImportRm, rm_handle) == 44);
    assert!(offset_of!(HeliosForeignImportRm, gem_handle) == 48);
    assert!(offset_of!(HeliosForeignImportRm, flags) == 52);
    assert!(offset_of!(HeliosForeignImportRm, size) == 56);
    assert!(offset_of!(HeliosForeignImportRm, out_resource_id) == 64);
    assert!(offset_of!(HeliosForeignImportRm, out_host_errno) == 68);

    assert!(size_of::<HeliosForeignLayout>() == HELIOS_FOREIGN_LAYOUT_BYTES);
    assert!(offset_of!(HeliosForeignLayout, width) == 0);
    assert!(offset_of!(HeliosForeignLayout, height) == 4);
    assert!(offset_of!(HeliosForeignLayout, stride) == 8);
    assert!(offset_of!(HeliosForeignLayout, offset) == 12);
    assert!(offset_of!(HeliosForeignLayout, fourcc) == 16);
    assert!(offset_of!(HeliosForeignLayout, reserved) == 20);
    assert!(offset_of!(HeliosForeignLayout, modifier) == 24);

    assert!(size_of::<HeliosForeignImportRmLayout>() == HELIOS_FOREIGN_IMPORT_RM_LAYOUT_BYTES);
    assert!(offset_of!(HeliosForeignImportRmLayout, base) == 0);
    assert!(offset_of!(HeliosForeignImportRmLayout, layout) == HELIOS_FOREIGN_IMPORT_RM_BYTES);

    assert!(size_of::<HeliosForeignPlane>() == HELIOS_FOREIGN_PLANE_BYTES);
    assert!(offset_of!(HeliosForeignPlane, modifier) == 0);
    assert!(offset_of!(HeliosForeignPlane, stride) == 8);
    assert!(offset_of!(HeliosForeignPlane, offset) == 12);
    assert!(size_of::<HeliosForeignImportRmPlanes>() == HELIOS_FOREIGN_IMPORT_RM_PLANES_BYTES);
    assert!(offset_of!(HeliosForeignImportRmPlanes, base) == 0);
    assert!(
        offset_of!(HeliosForeignImportRmPlanes, plane1) == HELIOS_FOREIGN_IMPORT_RM_LAYOUT_BYTES
    );

    assert!(size_of::<HeliosForeignRmResourceImport>() == HELIOS_FOREIGN_RM_RESOURCE_IMPORT_BYTES);
    assert!(offset_of!(HeliosForeignRmResourceImport, rm_handle) == 40);
    assert!(offset_of!(HeliosForeignRmResourceImport, resource_id) == 44);
    assert!(offset_of!(HeliosForeignRmResourceImport, flags) == 48);
    assert!(offset_of!(HeliosForeignRmResourceImport, out_gem_handle) == 52);
    assert!(offset_of!(HeliosForeignRmResourceImport, out_size) == 56);
    assert!(offset_of!(HeliosForeignRmResourceImport, out_modifier) == 64);
    assert!(offset_of!(HeliosForeignRmResourceImport, out_flags) == 72);
    assert!(offset_of!(HeliosForeignRmResourceImport, out_host_errno) == 76);

    // The ops are distinct and the cap bits do not overlap.
    assert!(HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT != HELIOS_FOREIGN_OP_IMPORT_RM);
    assert!(HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT != HELIOS_FOREIGN_OP_QUERY_CAPS);
    assert!(
        HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT
            & (HELIOS_FOREIGN_CAP_RM_IMPORT | HELIOS_FOREIGN_CAP_SHARED_OPEN)
            == 0
    );
    // `RmResourceImport` (host MsgType 31) is sent by the KMD after its own checks
    // (`helios_kmd_logic::rm_resource_import`); it must never become forwardable,
    // or any process could name any resource.
    assert!(crate::HELIOS_NVRM_FORWARD_MSG_TYPES & (1u32 << 31) == 0);

    // Distinct from every other verb in the protocol crate.
    assert!(HELIOS_ESCAPE_FOREIGN_RESOURCE != crate::HELIOS_ESCAPE_NVRM);
    assert!(HELIOS_ESCAPE_FOREIGN_RESOURCE != crate::HELIOS_ESCAPE_SUBMIT_VENUS_BATCH);
    assert!(HELIOS_ESCAPE_FOREIGN_RESOURCE != crate::HELIOS_ESCAPE_PRODUCER);
    assert!(HELIOS_ESCAPE_FOREIGN_RESOURCE != crate::HELIOS_ESCAPE_SNAPSHOT_STATUS);
};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// The value of `#define name ...` in the C mirror, numeric only.
    fn c_define(name: &str) -> u64 {
        let header = include_str!("../include/helios_foreign.h");
        for line in header.lines() {
            let Some(rest) = line.strip_prefix("#define ") else {
                continue;
            };
            let Some(value) = rest.strip_prefix(name) else {
                continue;
            };
            // `name` must be the whole identifier.
            if !value.starts_with(' ') {
                continue;
            }
            // Drop a trailing comment, then the value is one literal or `(1u << n)`.
            let value = value.split("/*").next().unwrap_or("").trim();
            if let Some(bits) = value.strip_prefix("(1u << ") {
                return 1u64 << bits.trim_end_matches(')').parse::<u32>().unwrap();
            }
            let value = value.trim_end_matches('u');
            return match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
                None => value.parse().unwrap(),
            };
        }
        panic!("{name} is not defined in helios_foreign.h");
    }

    #[test]
    fn the_shared_format_table_and_its_c_mirror() {
        for (name, v) in [
            ("HELIOS_FOREIGN_CAP_LAYOUT_FORMATS", HELIOS_FOREIGN_CAP_LAYOUT_FORMATS),
            ("HELIOS_FOREIGN_IMPORT_FLAG_PLANE1", HELIOS_FOREIGN_IMPORT_FLAG_PLANE1),
            ("HELIOS_DRM_FORMAT_R8", DRM_FORMAT_R8),
            ("HELIOS_DRM_FORMAT_GR88", DRM_FORMAT_GR88),
            ("HELIOS_DRM_FORMAT_R16", DRM_FORMAT_R16),
            ("HELIOS_DRM_FORMAT_GR1616", DRM_FORMAT_GR1616),
            ("HELIOS_DRM_FORMAT_RGB565", DRM_FORMAT_RGB565),
            ("HELIOS_DRM_FORMAT_ARGB1555", DRM_FORMAT_ARGB1555),
            ("HELIOS_DRM_FORMAT_ARGB4444", DRM_FORMAT_ARGB4444),
            ("HELIOS_DRM_FORMAT_ABGR2101010", DRM_FORMAT_ABGR2101010),
            ("HELIOS_DRM_FORMAT_ABGR16161616F", DRM_FORMAT_ABGR16161616F),
            ("HELIOS_DRM_FORMAT_ABGR16161616", DRM_FORMAT_ABGR16161616),
            ("HELIOS_DRM_FORMAT_YUYV", DRM_FORMAT_YUYV),
            ("HELIOS_DRM_FORMAT_NV12", DRM_FORMAT_NV12),
            ("HELIOS_DRM_FORMAT_P010", DRM_FORMAT_P010),
            ("HELIOS_DRM_FORMAT_P016", DRM_FORMAT_P016),
            ("HELIOS_WDDM_LAYOUT_VERSION_PLANES", crate::HELIOS_WDDM_LAYOUT_VERSION_PLANES),
            ("HELIOS_WDDM_LAYOUT_PLANE1_OFFSET", crate::HELIOS_WDDM_LAYOUT_PLANE1_OFFSET as u32),
            (
                "HELIOS_WDDM_PRIVATE_WITH_PLANES_BYTES",
                crate::HELIOS_WDDM_PRIVATE_WITH_PLANES_BYTES as u32,
            ),
        ] {
            assert_eq!(c_define(name), v as u64, "{name}");
        }
        // The fourccs spell what drm_fourcc.h spells.
        let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
        assert_eq!(DRM_FORMAT_R8, cc(b"R8  "));
        assert_eq!(DRM_FORMAT_NV12, cc(b"NV12"));
        assert_eq!(DRM_FORMAT_P010, cc(b"P010"));
        assert_eq!(DRM_FORMAT_ABGR16161616F, cc(b"AB4H"));
        assert_eq!(DRM_FORMAT_ABGR2101010, cc(b"AB30"));
        assert_eq!(DRM_FORMAT_YUYV, cc(b"YUYV"));

        let nv12 = share_format(DRM_FORMAT_NV12).unwrap();
        assert_eq!((nv12.planes, nv12.row_bytes(0, 1920), nv12.row_bytes(1, 1920)), (2, 1920, 1920));
        assert_eq!((nv12.rows(0, 1080), nv12.rows(1, 1080)), (1080, 540));
        let p010 = share_format(DRM_FORMAT_P010).unwrap();
        assert_eq!((p010.row_bytes(0, 1920), p010.row_bytes(1, 1920)), (3840, 3840));
        let yuyv = share_format(DRM_FORMAT_YUYV).unwrap();
        assert_eq!(yuyv.row_bytes(0, 1920), 3840);
        let f16 = share_format(DRM_FORMAT_ABGR16161616F).unwrap();
        assert_eq!((f16.row_bytes(0, 100), f16.stride_align(0)), (800, 4));
        assert_eq!(share_format(DRM_FORMAT_R8).unwrap().stride_align(0), 1);
        assert!(share_format(0).is_none());
        assert!(ShareFormat::is_rgb32(DRM_FORMAT_ARGB8888) && !ShareFormat::is_rgb32(DRM_FORMAT_R8));
    }

    #[test]
    fn the_c_mirror_agrees_with_the_constants() {
        assert_eq!(
            c_define("HELIOS_ESCAPE_MAGIC"),
            crate::HELIOS_ESCAPE_MAGIC as u64
        );
        assert_eq!(
            c_define("HELIOS_ESCAPE_VERSION"),
            crate::HELIOS_ESCAPE_VERSION as u64
        );
        assert_eq!(
            c_define("HELIOS_ESCAPE_FOREIGN_RESOURCE"),
            HELIOS_ESCAPE_FOREIGN_RESOURCE as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ABI_VERSION"),
            HELIOS_FOREIGN_ABI_VERSION as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_OP_QUERY_CAPS"),
            HELIOS_FOREIGN_OP_QUERY_CAPS as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_OP_IMPORT_RM"),
            HELIOS_FOREIGN_OP_IMPORT_RM as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_CAP_RM_IMPORT"),
            HELIOS_FOREIGN_CAP_RM_IMPORT as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_CAP_SHARED_OPEN"),
            HELIOS_FOREIGN_CAP_SHARED_OPEN as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT"),
            HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT"),
            HELIOS_FOREIGN_CAP_RM_RESOURCE_IMPORT as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER"),
            HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT"),
            HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_OK"),
            HELIOS_FOREIGN_ST_OK as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_UNSUPPORTED"),
            HELIOS_FOREIGN_ST_UNSUPPORTED as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_NOT_OWNED"),
            HELIOS_FOREIGN_ST_NOT_OWNED as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_BAD_CONTEXT"),
            HELIOS_FOREIGN_ST_BAD_CONTEXT as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_BAD_RANGE"),
            HELIOS_FOREIGN_ST_BAD_RANGE as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_NO_RESOURCES"),
            HELIOS_FOREIGN_ST_NO_RESOURCES as u64
        );
        assert_eq!(
            c_define("HELIOS_FOREIGN_ST_DEVICE_ERROR"),
            HELIOS_FOREIGN_ST_DEVICE_ERROR as u64
        );
        assert_eq!(
            c_define("HELIOS_BLOB_MEM_RM_EXPORT"),
            HELIOS_BLOB_MEM_RM_EXPORT as u64
        );
    }

    #[test]
    fn layout_extension_is_a_prefix_compatible_tail() {
        // The 104-byte request starts with the unchanged 72-byte one.
        let mut ext = HeliosForeignImportRmLayout::zeroed();
        ext.base.flags = HELIOS_FOREIGN_IMPORT_FLAG_LAYOUT;
        ext.base.size = 0x7f_0000;
        ext.layout = HeliosForeignLayout {
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            fourcc: 0x3432_5258,
            reserved: 0,
            modifier: 0x0300_0000_0060_6015,
        };
        let bytes = bytemuck::bytes_of(&ext);
        assert_eq!(bytes.len(), 104);
        let base: HeliosForeignImportRm =
            bytemuck::pod_read_unaligned(&bytes[..HELIOS_FOREIGN_IMPORT_RM_BYTES]);
        assert_eq!(base.flags, 1);
        assert_eq!(base.size, 0x7f_0000);
        let tail: HeliosForeignLayout =
            bytemuck::pod_read_unaligned(&bytes[HELIOS_FOREIGN_IMPORT_RM_BYTES..]);
        assert_eq!(tail.modifier, 0x0300_0000_0060_6015);
        assert_eq!(tail.stride, 7680);
    }

    #[test]
    fn rm_resource_import_round_trips_through_bytes() {
        let mut r = HeliosForeignRmResourceImport::zeroed();
        r.head.hdr = HeliosEscapeHeader::new(
            HELIOS_ESCAPE_FOREIGN_RESOURCE,
            HELIOS_FOREIGN_RM_RESOURCE_IMPORT_BYTES as u32,
        );
        r.head.abi_version = HELIOS_FOREIGN_ABI_VERSION;
        r.head.op = HELIOS_FOREIGN_OP_RM_RESOURCE_IMPORT;
        r.rm_handle = 6;
        r.resource_id = 100;
        r.out_gem_handle = 9;
        r.out_size = 0x80_0000;
        r.out_modifier = 0x0300_0000_0060_6015;
        r.out_flags = HELIOS_FOREIGN_RM_RESOURCE_IMPORT_MODIFIER;
        r.out_host_errno = 71;
        let b = bytemuck::bytes_of(&r);
        assert_eq!(b.len(), 80);
        assert_eq!(&b[16..24], &[1, 0, 0, 0, 3, 0, 0, 0]); // abi_version, op
        assert_eq!(&b[40..48], &[6, 0, 0, 0, 100, 0, 0, 0]);
        let back: HeliosForeignRmResourceImport = bytemuck::pod_read_unaligned(b);
        assert_eq!(back.out_size, 0x80_0000);
        assert_eq!(back.out_modifier, 0x0300_0000_0060_6015);
        assert_eq!(back.out_host_errno, 71);
    }

    #[test]
    fn blob_mem_is_outside_the_standard_virtio_range() {
        assert!(HELIOS_BLOB_MEM_RM_EXPORT & 0x8000_0000 != 0);
        assert!(HELIOS_BLOB_MEM_RM_EXPORT > crate::VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST);
    }

    #[test]
    fn header_round_trips_through_bytes() {
        let mut h = HeliosForeignHeader::zeroed();
        h.hdr = HeliosEscapeHeader::new(
            HELIOS_ESCAPE_FOREIGN_RESOURCE,
            HELIOS_FOREIGN_IMPORT_RM_BYTES as u32,
        );
        h.abi_version = HELIOS_FOREIGN_ABI_VERSION;
        h.op = HELIOS_FOREIGN_OP_IMPORT_RM;
        h.status = -1;
        h.epoch = 0x0102_0304_0506_0708;
        let b = bytemuck::bytes_of(&h);
        assert_eq!(b.len(), HELIOS_FOREIGN_HEADER_BYTES);
        let back: HeliosForeignHeader = bytemuck::pod_read_unaligned(b);
        assert_eq!(back.hdr.cmd_type, 0x0018);
        assert_eq!(back.op, 2);
        assert_eq!(back.status, -1);
        assert_eq!(back.epoch, 0x0102_0304_0506_0708);
    }
}
