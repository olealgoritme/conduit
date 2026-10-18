//! The GPU-ordered window copy (`NvkBltGpuCopy`): an NVK producer writes its windowed blt-model
//! frame into the window's redirection surface itself, on its own queue, and tells the KMD so.
//!
//! The ordinary windowed Blt (`RedirVram`, `guest/windows/docs/vram-redirection.md`) has the KMD
//! copy the back buffer into the window's VRAM redirection surface on its copy engine, behind the
//! frame's RM fence, and retire the Present only after its worker saw that copy done. The producer
//! can do the copy itself, right after the frame on the same queue, so it needs no ordering of its
//! own and costs no KMD round trip. Two pieces:
//!
//! * [`HELIOS_ESCAPE_BLT_TARGET`] ([`HeliosBltTargetQuery`]): "where does the KMD copy this back
//!   buffer?" The KMD answers from the record of the last Blt it did from that source for the
//!   calling process: the redirection surface's resource id, its import facts (the same ones a
//!   `DxgkDdiOpenAllocation` of it hands DWM), the rectangles and a `ticket` naming that record.
//!   The answer also authorizes this process to import that surface by resource id (it presented
//!   into it, so it may write it).
//! * The `HOWW` tag ([`HeliosWindowWrittenTag`]) at the tail of the frame's `HERF` command, beside
//!   its RM fence: "this Present's destination is already written by my queue, ordered before the
//!   fence in this command". The KMD checks the tag against its own record and the Present
//!   (source, destination, rectangles, ticket, an attached fence) and completes the Blt with no
//!   copy and no read-ledger claim. The Present's DMA buffer still retires on the fence (the RM
//!   gate point the fence tail attached), so whatever orders a reader of the window surface after
//!   the Present orders it after the producer's copy. A tag the KMD cannot verify is counted and
//!   the Present is the ordinary Blt.
//!
//! ```text
//! HERF + fence slot + tag, CommandLength = 72, all little-endian
//!   0..32   HeliosPresentRefreshCmd   ('HERF', version 1, zero stream tail)
//!  32..48   HeliosRmFenceTail         (the frame's RM fence: required)
//!  48..72   HeliosWindowWrittenTag    ('HOWW', this module; the HOSC slot)
//! ```
//!
//! An older KMD reads the fence, does not know `HOWW` (counted as a malformed `HOSC`) and does the
//! ordinary Blt: the window gets the same pixels twice.

use bytemuck::{Pod, Zeroable};

use crate::escape::{HeliosEscapeHeader, HELIOS_ESCAPE_MAGIC, HELIOS_ESCAPE_VERSION};
use crate::rm_fence::HeliosPresentRefreshCmdFence;

/// The escape verb. `0x0019` is ECL_DONE.
pub const HELIOS_ESCAPE_BLT_TARGET: u32 = 0x001A;

/// `'HOWW'`: magic of [`HeliosWindowWrittenTag`] (bytes `48 4F 57 57` in memory).
pub const HELIOS_WINDOW_WRITTEN_MAGIC: u32 = 0x5757_4F48;
/// Current tag version; any other version is rejected (counted), never reinterpreted.
pub const HELIOS_WINDOW_WRITTEN_VERSION: u16 = 1;
/// Byte offset of the tag in a `HERF` command: the on-scanout tag's slot (the two are exclusive).
pub const HELIOS_WINDOW_WRITTEN_HERF_OFFSET: usize = 48;
/// `CommandLength` of a `HERF` command that carries the tag.
pub const HELIOS_WINDOW_WRITTEN_HERF_BYTES: usize = 72;

/// [`HeliosBltTargetQuery::out_state`]: no record for this source and process (no Blt seen yet,
/// or its destination is not a surface a producer can write).
pub const HELIOS_BLT_TARGET_NONE: u32 = 0;
/// [`HeliosBltTargetQuery::out_state`]: the `out_*` fields describe the destination.
pub const HELIOS_BLT_TARGET_FOUND: u32 = 1;

/// The tag, 24 bytes. The claim is "my queue wrote the frame of resource `source_resource_id`
/// into resource `destination_resource_id` with the rectangles of record `ticket`, before the RM
/// fence this command carries".
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosWindowWrittenTag {
    /// [`HELIOS_WINDOW_WRITTEN_MAGIC`].
    pub magic: u32,
    /// [`HELIOS_WINDOW_WRITTEN_VERSION`].
    pub version: u16,
    /// Reserved, zero.
    pub flags: u16,
    /// The Blt source's resource id (the back buffer's `IMPORT_RM` id). Nonzero.
    pub source_resource_id: u32,
    /// The redirection surface's resource id ([`HeliosBltTargetQuery::out_destination_resource_id`]).
    /// Nonzero.
    pub destination_resource_id: u32,
    /// [`HeliosBltTargetQuery::out_ticket`] of the answer the copy followed. Nonzero.
    pub ticket: u32,
    /// Reserved, zero.
    pub reserved: u32,
}

/// The tag inside a complete `HERF` command (the fence slot carries the frame's RM fence).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRefreshCmdWindowWritten {
    pub base: HeliosPresentRefreshCmdFence,
    pub tag: HeliosWindowWrittenTag,
}

/// The `HELIOS_ESCAPE_BLT_TARGET` buffer, 104 bytes. In: the header and `source_resource_id`.
/// Out: everything else (the request's `out_*` fields are ignored). The escape needs a device
/// (`hDevice`): the record is looked up for its `hKmdProcess`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosBltTargetQuery {
    pub hdr: HeliosEscapeHeader,
    /// In: the back buffer's resource id.
    pub source_resource_id: u32,
    /// Out: [`HELIOS_BLT_TARGET_NONE`] or [`HELIOS_BLT_TARGET_FOUND`].
    pub out_state: u32,
    /// Out: the redirection surface's resource id (what an NVK import by id names).
    pub out_destination_resource_id: u32,
    /// Out: names this answer; the tag repeats it. Nonzero when found.
    pub out_ticket: u32,
    /// Out: the surface's extent, row pitch (bytes) and DXGI format.
    pub out_width: u32,
    pub out_height: u32,
    pub out_pitch: u32,
    pub out_dxgi_format: u32,
    /// Out: the surface's D3D11 DDI bind flags, as its allocation records them.
    pub out_bind_flags: u32,
    /// Out: the memory type its open identity records.
    pub out_memory_type_index: u32,
    /// Out: the allocation size an import names.
    pub out_alloc_size: u64,
    /// Out: the foreign layout (`HeliosWddmAllocLayout`): DRM modifier, plane offset, fourcc.
    pub out_modifier: u64,
    pub out_offset: u32,
    pub out_fourcc: u32,
    /// Out: the rectangle of the source the Blt reads (left, top) and where it lands in the
    /// surface (left, top), and its size. The copy is exactly this.
    pub out_src_left: u32,
    pub out_src_top: u32,
    pub out_dst_left: u32,
    pub out_dst_top: u32,
    pub out_copy_width: u32,
    pub out_copy_height: u32,
}

impl HeliosBltTargetQuery {
    /// A request for `source_resource_id`.
    pub fn new(source_resource_id: u32) -> Self {
        let mut req = Self::zeroed();
        req.hdr = HeliosEscapeHeader::new(
            HELIOS_ESCAPE_BLT_TARGET,
            core::mem::size_of::<Self>() as u32,
        );
        req.source_resource_id = source_resource_id;
        req
    }

    /// The source a request the KMD read names, or `None` when it is malformed (magic, version,
    /// verb, a declared size smaller than the struct, a zero source).
    pub fn valid_source(&self) -> Option<u32> {
        (self.hdr.magic == HELIOS_ESCAPE_MAGIC
            && self.hdr.version == HELIOS_ESCAPE_VERSION
            && self.hdr.cmd_type == HELIOS_ESCAPE_BLT_TARGET
            && self.hdr.size as usize >= core::mem::size_of::<Self>()
            && self.source_resource_id != 0)
            .then_some(self.source_resource_id)
    }

    /// The answer is usable: found, and every field a copy needs is nonzero.
    pub fn found(&self) -> bool {
        self.out_state == HELIOS_BLT_TARGET_FOUND
            && self.out_destination_resource_id != 0
            && self.out_ticket != 0
            && self.out_width != 0
            && self.out_height != 0
            && self.out_copy_width != 0
            && self.out_copy_height != 0
            && self.out_dst_left.saturating_add(self.out_copy_width) <= self.out_width
            && self.out_dst_top.saturating_add(self.out_copy_height) <= self.out_height
    }
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosWindowWrittenTag>() == 24);
    assert!(offset_of!(HeliosWindowWrittenTag, version) == 4);
    assert!(offset_of!(HeliosWindowWrittenTag, flags) == 6);
    assert!(offset_of!(HeliosWindowWrittenTag, source_resource_id) == 8);
    assert!(offset_of!(HeliosWindowWrittenTag, destination_resource_id) == 12);
    assert!(offset_of!(HeliosWindowWrittenTag, ticket) == 16);
    assert!(size_of::<HeliosPresentRefreshCmdWindowWritten>() == HELIOS_WINDOW_WRITTEN_HERF_BYTES);
    assert!(
        offset_of!(HeliosPresentRefreshCmdWindowWritten, tag) == HELIOS_WINDOW_WRITTEN_HERF_OFFSET
    );
    // The on-scanout tag's slot, so a command carries one or the other, never both.
    assert!(HELIOS_WINDOW_WRITTEN_HERF_OFFSET == crate::onscanout::HELIOS_ONSCANOUT_HERF_OFFSET);
    assert!(HELIOS_WINDOW_WRITTEN_HERF_BYTES == crate::onscanout::HELIOS_ONSCANOUT_HERF_BYTES);
    assert!(size_of::<HeliosBltTargetQuery>() == 104);
    assert!(offset_of!(HeliosBltTargetQuery, source_resource_id) == 16);
    assert!(offset_of!(HeliosBltTargetQuery, out_alloc_size) == 56);
    assert!(offset_of!(HeliosBltTargetQuery, out_modifier) == 64);
    assert!(offset_of!(HeliosBltTargetQuery, out_src_left) == 80);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wddm::{HELIOS_PRESENT_REFRESH_MAGIC, HELIOS_PRESENT_REFRESH_VERSION};
    use crate::HELIOS_RM_FENCE_TAIL_FLAG_FENCE;

    #[test]
    fn magic_reads_howw_and_differs_from_the_other_tails() {
        assert_eq!(&HELIOS_WINDOW_WRITTEN_MAGIC.to_le_bytes(), b"HOWW");
        assert_ne!(HELIOS_WINDOW_WRITTEN_MAGIC, crate::HELIOS_ONSCANOUT_MAGIC);
        assert_ne!(
            HELIOS_WINDOW_WRITTEN_MAGIC,
            crate::HELIOS_RM_FENCE_TAIL_V3_MAGIC
        );
        assert_ne!(HELIOS_WINDOW_WRITTEN_MAGIC, HELIOS_PRESENT_REFRESH_MAGIC);
    }

    #[test]
    fn the_verb_is_new() {
        for other in [
            crate::HELIOS_ESCAPE_ECL_DONE,
            crate::HELIOS_ESCAPE_FOREIGN_RESOURCE,
            crate::HELIOS_ESCAPE_SUBMIT_VENUS_BATCH,
            crate::HELIOS_ESCAPE_NVRM,
            crate::HELIOS_ESCAPE_SNAPSHOT_STATUS,
            crate::HELIOS_ESCAPE_PRODUCER,
        ] {
            assert_ne!(HELIOS_ESCAPE_BLT_TARGET, other);
        }
    }

    #[test]
    fn the_herf_carrier_has_the_fence_and_the_tag_at_48() {
        let mut cmd = HeliosPresentRefreshCmdWindowWritten::zeroed();
        cmd.base.base.magic = HELIOS_PRESENT_REFRESH_MAGIC;
        cmd.base.base.version = HELIOS_PRESENT_REFRESH_VERSION;
        cmd.base.fence.flags = HELIOS_RM_FENCE_TAIL_FLAG_FENCE;
        cmd.base.fence.rm_fence_handle = 7;
        cmd.tag = HeliosWindowWrittenTag {
            magic: HELIOS_WINDOW_WRITTEN_MAGIC,
            version: HELIOS_WINDOW_WRITTEN_VERSION,
            flags: 0,
            source_resource_id: 0x11,
            destination_resource_id: 0x22,
            ticket: 0x33,
            reserved: 0,
        };
        let b = bytemuck::bytes_of(&cmd);
        assert_eq!(b.len(), 72);
        assert_eq!(&b[0..4], b"HERF");
        assert_eq!(u32::from_le_bytes(b[32..36].try_into().unwrap()), 7);
        assert_eq!(&b[48..52], b"HOWW");
        assert_eq!(u16::from_le_bytes(b[52..54].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(b[56..60].try_into().unwrap()), 0x11);
        assert_eq!(u32::from_le_bytes(b[60..64].try_into().unwrap()), 0x22);
        assert_eq!(u32::from_le_bytes(b[64..68].try_into().unwrap()), 0x33);
    }

    #[test]
    fn a_query_round_trips_and_malformed_ones_are_refused() {
        let req = HeliosBltTargetQuery::new(42);
        assert_eq!(req.valid_source(), Some(42));
        assert_eq!(req.hdr.size, 104);
        assert!(!req.found());
        assert_eq!(HeliosBltTargetQuery::new(0).valid_source(), None);
        let mut bad = req;
        bad.hdr.size = 103;
        assert_eq!(bad.valid_source(), None);
        let mut bad = req;
        bad.hdr.cmd_type = crate::HELIOS_ESCAPE_ECL_DONE;
        assert_eq!(bad.valid_source(), None);
        let mut bad = req;
        bad.hdr.magic ^= 1;
        assert_eq!(bad.valid_source(), None);
    }

    #[test]
    fn found_needs_a_copy_inside_the_surface() {
        let mut a = HeliosBltTargetQuery::new(1);
        a.out_state = HELIOS_BLT_TARGET_FOUND;
        a.out_destination_resource_id = 9;
        a.out_ticket = 3;
        a.out_width = 1616;
        a.out_height = 939;
        a.out_dst_left = 8;
        a.out_dst_top = 31;
        a.out_copy_width = 1600;
        a.out_copy_height = 900;
        assert!(a.found());
        let mut b = a;
        b.out_copy_width = 1609;
        assert!(!b.found());
        let mut b = a;
        b.out_ticket = 0;
        assert!(!b.found());
        let mut b = a;
        b.out_state = HELIOS_BLT_TARGET_NONE;
        assert!(!b.found());
    }
}
