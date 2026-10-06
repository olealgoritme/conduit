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

/// `QueryCaps.caps_flags`: `IMPORT_RM` is served end to end (KMD gate open and
/// the host has the matching blob type).
pub const HELIOS_FOREIGN_CAP_RM_IMPORT: u32 = 1 << 0;

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
