//! Fence tail v3: the producer's semaphore and the copy source of a windowed Present, so the
//! KMD can copy the frame on its own RM copy-engine channel (`guest/windows/docs/rm-copy-engine-present.md`
//! sections 2.3 (b), 10 and 11).
//!
//! The 16-byte [`HeliosRmFenceTail`] names the producer's completion as an opaque backend fence
//! handle (`rm-fence-marker.md`). That is enough to retire a Present, not to let the GPU wait for
//! the producer: a semaphore ACQUIRE needs the semaphore's memory and offset, and a copy needs the
//! source image's memory and layout. This record carries both. It is OPTIONAL and travels behind
//! the existing bytes, so nothing that exists changes:
//!
//! ```text
//! HERF, CommandLength = 168, little-endian
//!    0..32   HeliosPresentRefreshCmd   ('HERF', version 1, stream tail zero)
//!   32..48   HeliosRmFenceTail         (flags = FENCE, the handle; value DIAGNOSTIC as ever)
//!   48..72   HeliosOnScanoutTag slot   (ALL ZERO: an on-scanout frame is never copied)
//!   72..168  HeliosRmFenceTailV3       ('HEF3', this module)
//!
//! HEPR, CommandLength = 192
//!    0..80   HeliosPresentRenderCmd    (present.reserved has FLAG_RM_FENCE)
//!   80..96   HeliosRmFenceTail
//!   96..192  HeliosRmFenceTailV3
//! ```
//!
//! # Compatibility
//!
//! * An older KMD reads a `HERF` of 168 bytes exactly as one of 48: its fence tail is read from
//!   32, the zero slot at 48 parses as "no on-scanout tag" (`kmd_logic::onscanout::parse` treats a
//!   zero magic as absent), the rest is ignored. A `HEPR` of 192 bytes is read as one of 96.
//! * An older producer sends no record: the KMD routes the Present as today (the Venus copy).
//! * Nothing is version-bumped: the record has its own magic, version and byte count. A later
//!   revision may append fields (a larger `bytes`); a reader takes the first
//!   [`HELIOS_RM_FENCE_TAIL_V3_BYTES`] and ignores the rest. A different `version` is NOT this
//!   record and is refused (counted), never reinterpreted.
//!
//! # Semantics
//!
//! * The record is read only together with a FENCE tail (`HeliosRmFenceTail::is_fence`). The fence
//!   handle stays the retirement signal and keeps every rule of `rm-fence-marker.md`
//!   (ownership, refusal rule, close on fire). The record is a HINT for the copy-engine route: a
//!   KMD that refuses it (malformed, foreign handles, an unsupported layout) still honours the
//!   fence and copies with Venus.
//! * `semaphore.value` is the timeline value the fence was created for, and unlike
//!   `HeliosRmFenceTail::rm_fence_value` it IS read: the copy engine acquires on it. The KMD
//!   requires it to equal `rm_fence_value` ([`HeliosRmFenceTailV3::matches_fence`]); a mismatch
//!   refuses the record, not the fence.
//! * Handles are RM handles of the PRODUCER's own RM client (NVK's `VkDevice` client), not backend
//!   handles. The KMD accepts them only when `h_client` is a client recorded for the presenting
//!   process (the `NvDupHarden` rule, `shared-foreign-surfaces.md`), then duplicates the objects
//!   into its own client (`NV_ESC_RM_DUP_OBJECT`).
//! * The source must be uncompressed and one plane. A block-linear source is described by its
//!   DRM format modifier (the GB20x families of [`crate::gb20x_family`]); the page kind the
//!   modifier names (`k`, 0x06 for every family NVK uses) is the kind the KMD must map the
//!   source with.

use bytemuck::{Pod, Zeroable};

use crate::foreign::{gb20x_family, share_format};
use crate::rm_fence::HeliosRmFenceTail;

/// `'HEF3'`: magic of [`HeliosRmFenceTailV3`] (bytes `48 45 46 33` in memory).
pub const HELIOS_RM_FENCE_TAIL_V3_MAGIC: u32 = 0x3346_4548;
/// The record's version. v1 was the stream marker, v2 the 16-byte fence tail.
pub const HELIOS_RM_FENCE_TAIL_V3_VERSION: u16 = 3;
/// Bytes of the version-3 record. `bytes` may be larger (a later revision); never smaller.
pub const HELIOS_RM_FENCE_TAIL_V3_BYTES: usize = 96;
/// Offset of the record in a `HERF` command: after the 32-byte `HERF`, the 16-byte fence tail and
/// the 24-byte on-scanout slot (zero).
pub const HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET: usize = 72;
/// Offset of the record in a `HEPR` command: after the 80-byte `HEPR` and the fence tail.
pub const HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET: usize = 96;

/// `flags` bit 0: [`HeliosRmFenceTailV3::semaphore`] is filled.
pub const HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE: u16 = 1 << 0;
/// `flags` bit 1: [`HeliosRmFenceTailV3::source`] is filled.
pub const HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE: u16 = 1 << 1;
/// Every `flags` bit version 3 knows. Version 3 REQUIRES both: a record without either is
/// refused ([`TailV3Error::Incomplete`]).
pub const HELIOS_RM_FENCE_TAIL_V3_FLAGS_ALL: u16 =
    HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE | HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE;

/// `source.flags` bit 0: the source memory is compressed (GB20x compressible kinds). Known so the
/// KMD can count it; the copy-engine route refuses such a source.
pub const HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED: u32 = 1 << 0;
/// Every `source.flags` bit the KMD knows; any other bit refuses the record.
pub const HELIOS_RM_COPY_SOURCE_FLAGS_ALL: u32 = HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED;

/// `DRM_FORMAT_MOD_LINEAR`.
pub const HELIOS_DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// Largest block height `h` (log2 GOBs) a block-linear modifier may carry.
pub const HELIOS_RM_COPY_MAX_BLOCK_HEIGHT_LOG2: u32 = 5;
/// Rows of one GOB.
pub const HELIOS_RM_COPY_GOB_ROWS: u64 = 8;
/// Largest width and height of a copy source (the foreign layout's bound).
pub const HELIOS_RM_COPY_MAX_DIM: u32 = 16_384;
/// Largest source pitch in bytes (the foreign layout's bound).
pub const HELIOS_RM_COPY_MAX_PITCH: u32 = 1 << 20;

/// Where the producer's 64-bit timeline semaphore lives. 24 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosRmSemaphoreLoc {
    /// The producer's RM client (`NV01_ROOT_CLIENT` handle of NVK's device).
    pub h_client: u32,
    /// The memory object the semaphore surface lives in (`hSemaphoreMem` of its
    /// `NV_SEMAPHORE_SURFACE`), a handle of `h_client`.
    pub h_memory: u32,
    /// Byte offset of the 64-bit value in that memory: the surface entry's index times the entry
    /// size (`FB_GET_SEMAPHORE_SURFACE_LAYOUT`). 8-aligned.
    pub offset: u64,
    /// The value the frame's work releases. The copy acquires `>= value`. Nonzero.
    pub value: u64,
}

/// The image the copy reads: the presented swapchain image. 56 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosRmCopySource {
    /// The producer's RM client. Usually the same as the semaphore's.
    pub h_client: u32,
    /// The image's RM memory object (a handle of `h_client`).
    pub h_memory: u32,
    /// Byte offset of plane 0 in that memory.
    pub offset: u64,
    /// Bytes of the memory object (its RM allocation size, a lower bound the copy must stay in).
    pub size: u64,
    /// `DRM_FORMAT_MOD_*`: LINEAR, or `gb20x_family(bytes per pixel) | h`, `h <= 5`.
    pub modifier: u64,
    /// Row pitch in bytes (the foreign layout's `stride`). For a block-linear image a multiple of
    /// 64 (a GOB is 64 bytes wide).
    pub pitch: u32,
    /// Pixels, `1..=16384`.
    pub width: u32,
    /// Rows, `1..=16384`.
    pub height: u32,
    /// `DRM_FORMAT_*`, one plane.
    pub fourcc: u32,
    /// `HELIOS_RM_COPY_SOURCE_FLAG_*`.
    pub flags: u32,
    /// Zero.
    pub reserved: u32,
}

/// The record. 96 bytes, 8-aligned, padding-free; see the module docs for where it sits.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosRmFenceTailV3 {
    /// [`HELIOS_RM_FENCE_TAIL_V3_MAGIC`].
    pub magic: u32,
    /// [`HELIOS_RM_FENCE_TAIL_V3_VERSION`].
    pub version: u16,
    /// `HELIOS_RM_FENCE_TAIL_V3_FLAG_*`; version 3 needs both.
    pub flags: u16,
    /// Bytes of the record as written, at least [`HELIOS_RM_FENCE_TAIL_V3_BYTES`].
    pub bytes: u32,
    /// Zero.
    pub reserved: u32,
    pub semaphore: HeliosRmSemaphoreLoc,
    pub source: HeliosRmCopySource,
}

/// Why a record was refused. Every refusal leaves the fence tail in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailV3Error {
    /// A nonzero magic that is not `'HEF3'`.
    BadMagic,
    /// The command ends inside the record, or `bytes` is under 96 or past the command's end.
    Short,
    /// `version != 3`.
    Version,
    /// An unknown `flags` or `source.flags` bit.
    Flags,
    /// Version 3 without both the semaphore and the source.
    Incomplete,
    /// A reserved field is nonzero.
    Reserved,
    /// A zero client or memory handle.
    Handle,
    /// The semaphore offset is not 8-aligned or its 8 bytes overflow.
    SemaphoreOffset,
    /// The semaphore value is zero.
    Value,
    /// Width or height outside `1..=16384`.
    Dimensions,
    /// A fourcc that is not a one-plane shared format.
    Format,
    /// A pitch under the row, off its alignment, over 1 MiB, or (block-linear) not a whole GOB.
    Pitch,
    /// Not LINEAR and not the GB20x block-linear family for the format's element size.
    Modifier,
    /// `offset + pitch * rows` overflows or exceeds `size` (rows rounded up to the block).
    Size,
}

/// What a command held at the record's offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailV3 {
    /// No record: the command ends before it, or its magic is zero.
    Absent,
    /// A record that passed every check of [`HeliosRmFenceTailV3::validate`].
    Record(HeliosRmFenceTailV3),
    /// A malformed record: counted, never used.
    Reject(TailV3Error),
}

/// `h` of a block-linear `modifier` for elements `element_bytes` wide: `Some(Some(h))` for the
/// family [`gb20x_family`] names with `h <= 5`, `Some(None)` for LINEAR, `None` for anything else.
/// The same family rule as `kmd_logic::foreign_resource` (`plane_modifier_ok`).
pub const fn copy_source_block_log2(modifier: u64, element_bytes: u32) -> Option<Option<u32>> {
    if modifier == HELIOS_DRM_FORMAT_MOD_LINEAR {
        return Some(None);
    }
    let family = gb20x_family(element_bytes);
    if modifier >= family && modifier <= family + HELIOS_RM_COPY_MAX_BLOCK_HEIGHT_LOG2 as u64 {
        Some(Some((modifier - family) as u32))
    } else {
        None
    }
}

impl HeliosRmCopySource {
    /// The rows the image spans: `height` (LINEAR) or `height` rounded up to the block
    /// (`8 << h` rows). Meaningful for a validated source.
    pub const fn rows(&self) -> u64 {
        let Some(f) = share_format(self.fourcc) else {
            return self.height as u64;
        };
        match copy_source_block_log2(self.modifier, f.bpp0) {
            Some(Some(h)) => {
                let block = HELIOS_RM_COPY_GOB_ROWS << h;
                (self.height as u64).div_ceil(block) * block
            }
            _ => self.height as u64,
        }
    }

    /// `offset + pitch * rows`, or `None` on overflow.
    pub const fn end(&self) -> Option<u64> {
        match (self.pitch as u64).checked_mul(self.rows()) {
            Some(bytes) => self.offset.checked_add(bytes),
            None => None,
        }
    }

    fn validate(&self) -> Result<(), TailV3Error> {
        if self.h_client == 0 || self.h_memory == 0 {
            return Err(TailV3Error::Handle);
        }
        if self.flags & !HELIOS_RM_COPY_SOURCE_FLAGS_ALL != 0 {
            return Err(TailV3Error::Flags);
        }
        if self.reserved != 0 {
            return Err(TailV3Error::Reserved);
        }
        if self.width == 0
            || self.height == 0
            || self.width > HELIOS_RM_COPY_MAX_DIM
            || self.height > HELIOS_RM_COPY_MAX_DIM
        {
            return Err(TailV3Error::Dimensions);
        }
        let f = match share_format(self.fourcc) {
            Some(f) if f.planes == 1 => f,
            _ => return Err(TailV3Error::Format),
        };
        if (f.even_width && self.width % 2 != 0) || (f.even_height && self.height % 2 != 0) {
            return Err(TailV3Error::Dimensions);
        }
        let Some(block) = copy_source_block_log2(self.modifier, f.bpp0) else {
            return Err(TailV3Error::Modifier);
        };
        if (self.pitch as u64) < f.row_bytes(0, self.width)
            || self.pitch > HELIOS_RM_COPY_MAX_PITCH
            || self.pitch % f.stride_align(0) != 0
            || (block.is_some() && self.pitch % 64 != 0)
        {
            return Err(TailV3Error::Pitch);
        }
        match self.end() {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(TailV3Error::Size),
        }
    }
}

impl HeliosRmFenceTailV3 {
    /// A filled version-3 record (`bytes` = 96).
    pub const fn new(semaphore: HeliosRmSemaphoreLoc, source: HeliosRmCopySource) -> Self {
        Self {
            magic: HELIOS_RM_FENCE_TAIL_V3_MAGIC,
            version: HELIOS_RM_FENCE_TAIL_V3_VERSION,
            flags: HELIOS_RM_FENCE_TAIL_V3_FLAGS_ALL,
            bytes: HELIOS_RM_FENCE_TAIL_V3_BYTES as u32,
            reserved: 0,
            semaphore,
            source,
        }
    }

    /// Every check that needs nothing but the record. `available` is the number of command bytes
    /// from the record's offset to the end of the command.
    pub fn validate(&self, available: usize) -> Result<(), TailV3Error> {
        if self.magic != HELIOS_RM_FENCE_TAIL_V3_MAGIC {
            return Err(TailV3Error::BadMagic);
        }
        if self.version != HELIOS_RM_FENCE_TAIL_V3_VERSION {
            return Err(TailV3Error::Version);
        }
        if (self.bytes as usize) < HELIOS_RM_FENCE_TAIL_V3_BYTES || self.bytes as usize > available {
            return Err(TailV3Error::Short);
        }
        if self.flags & !HELIOS_RM_FENCE_TAIL_V3_FLAGS_ALL != 0 {
            return Err(TailV3Error::Flags);
        }
        if self.flags != HELIOS_RM_FENCE_TAIL_V3_FLAGS_ALL {
            return Err(TailV3Error::Incomplete);
        }
        if self.reserved != 0 {
            return Err(TailV3Error::Reserved);
        }
        let s = &self.semaphore;
        if s.h_client == 0 || s.h_memory == 0 {
            return Err(TailV3Error::Handle);
        }
        if s.offset % 8 != 0 || s.offset.checked_add(8).is_none() {
            return Err(TailV3Error::SemaphoreOffset);
        }
        if s.value == 0 {
            return Err(TailV3Error::Value);
        }
        self.source.validate()
    }

    /// The semaphore value is the one the fence tail's fence was created for.
    pub const fn matches_fence(&self, fence: &HeliosRmFenceTail) -> bool {
        fence.is_fence() && fence.rm_fence_value == self.semaphore.value
    }

    /// Parse the bytes of a command from the record's offset to its end (any length).
    /// Little-endian. Empty, shorter than the magic or a zero magic: [`TailV3::Absent`] (every
    /// command that predates the record, and the zero padding of a longer one). A nonzero magic is
    /// a claim: [`TailV3::Record`] when valid, else [`TailV3::Reject`] naming the first fault.
    pub fn parse(tail: &[u8]) -> TailV3 {
        if tail.len() < 4 {
            return TailV3::Absent;
        }
        let magic = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
        if magic == 0 {
            return TailV3::Absent;
        }
        if magic != HELIOS_RM_FENCE_TAIL_V3_MAGIC {
            return TailV3::Reject(TailV3Error::BadMagic);
        }
        if tail.len() < HELIOS_RM_FENCE_TAIL_V3_BYTES {
            return TailV3::Reject(TailV3Error::Short);
        }
        let record: HeliosRmFenceTailV3 =
            bytemuck::pod_read_unaligned(&tail[..HELIOS_RM_FENCE_TAIL_V3_BYTES]);
        match record.validate(tail.len()) {
            Ok(()) => TailV3::Record(record),
            Err(e) => TailV3::Reject(e),
        }
    }
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosRmSemaphoreLoc>() == 24);
    assert!(offset_of!(HeliosRmSemaphoreLoc, h_memory) == 4);
    assert!(offset_of!(HeliosRmSemaphoreLoc, offset) == 8);
    assert!(offset_of!(HeliosRmSemaphoreLoc, value) == 16);
    assert!(size_of::<HeliosRmCopySource>() == 56);
    assert!(offset_of!(HeliosRmCopySource, h_memory) == 4);
    assert!(offset_of!(HeliosRmCopySource, offset) == 8);
    assert!(offset_of!(HeliosRmCopySource, size) == 16);
    assert!(offset_of!(HeliosRmCopySource, modifier) == 24);
    assert!(offset_of!(HeliosRmCopySource, pitch) == 32);
    assert!(offset_of!(HeliosRmCopySource, width) == 36);
    assert!(offset_of!(HeliosRmCopySource, height) == 40);
    assert!(offset_of!(HeliosRmCopySource, fourcc) == 44);
    assert!(offset_of!(HeliosRmCopySource, flags) == 48);
    assert!(offset_of!(HeliosRmCopySource, reserved) == 52);
    assert!(size_of::<HeliosRmFenceTailV3>() == HELIOS_RM_FENCE_TAIL_V3_BYTES);
    assert!(offset_of!(HeliosRmFenceTailV3, version) == 4);
    assert!(offset_of!(HeliosRmFenceTailV3, flags) == 6);
    assert!(offset_of!(HeliosRmFenceTailV3, bytes) == 8);
    assert!(offset_of!(HeliosRmFenceTailV3, reserved) == 12);
    assert!(offset_of!(HeliosRmFenceTailV3, semaphore) == 16);
    assert!(offset_of!(HeliosRmFenceTailV3, source) == 40);
    // The HERF offset is the end of the on-scanout layout; the HEPR offset the end of its fence form.
    assert!(HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET == crate::onscanout::HELIOS_ONSCANOUT_HERF_BYTES);
    assert!(
        HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET
            == size_of::<crate::rm_fence::HeliosPresentRenderCmdFence>()
    );
};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::foreign::{DRM_FORMAT_ABGR8888, DRM_FORMAT_NV12, DRM_FORMAT_R8, MOD_NVIDIA_BL_GB20X,
        MOD_NVIDIA_BL_GB20X_8BPP};
    use crate::rm_fence::HELIOS_RM_FENCE_TAIL_FLAG_FENCE;
    use std::vec::Vec;

    /// The windowed source of a measured Heaven run: 1600x900 AB24, block-linear `h = 4`, stride
    /// 6400, in a 6553600-byte object.
    fn heaven() -> HeliosRmFenceTailV3 {
        HeliosRmFenceTailV3::new(
            HeliosRmSemaphoreLoc {
                h_client: 0xc1d0_0001,
                h_memory: 0xcaf0_0010,
                offset: 64,
                value: 1234,
            },
            HeliosRmCopySource {
                h_client: 0xc1d0_0001,
                h_memory: 0xcaf0_0020,
                offset: 0,
                size: 6_553_600,
                modifier: 0x0300_0000_0060_6014,
                pitch: 6400,
                width: 1600,
                height: 900,
                fourcc: DRM_FORMAT_ABGR8888,
                flags: 0,
                reserved: 0,
            },
        )
    }

    fn bytes(r: &HeliosRmFenceTailV3) -> Vec<u8> {
        bytemuck::bytes_of(r).to_vec()
    }

    fn reject(r: HeliosRmFenceTailV3) -> TailV3Error {
        match HeliosRmFenceTailV3::parse(&bytes(&r)) {
            TailV3::Reject(e) => e,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn magic_reads_hef3_and_is_distinct() {
        assert_eq!(&HELIOS_RM_FENCE_TAIL_V3_MAGIC.to_le_bytes(), b"HEF3");
        for other in [
            crate::wddm::HELIOS_D3D12_SUBMIT_MAGIC,
            crate::wddm::HELIOS_PRESENT_REFRESH_MAGIC,
            crate::wddm::HELIOS_PRESENT_RENDER_MAGIC,
            crate::HELIOS_FLUSH_GATE_MAGIC,
            crate::HELIOS_ONSCANOUT_MAGIC,
        ] {
            assert_ne!(HELIOS_RM_FENCE_TAIL_V3_MAGIC, other);
        }
    }

    #[test]
    fn the_heaven_source_is_a_valid_record_with_the_documented_bytes() {
        let r = heaven();
        let b = bytes(&r);
        assert_eq!(b.len(), 96);
        assert_eq!(&b[0..4], b"HEF3");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 3);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 3);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 96);
        assert_eq!(u64::from_le_bytes(b[24..32].try_into().unwrap()), 64);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1234);
        assert_eq!(u64::from_le_bytes(b[64..72].try_into().unwrap()), 0x0300_0000_0060_6014);
        assert_eq!(HeliosRmFenceTailV3::parse(&b), TailV3::Record(r));
        // 900 rows round up to 1024 (blocks of 16 GOBs = 128 rows): 6400 * 1024 = 6553600, the
        // object's size exactly (the copy itself reads only 900 rows, 5760000 bytes).
        assert_eq!(r.source.rows(), 1024);
        assert_eq!(r.source.end(), Some(6_553_600));
        assert_eq!(copy_source_block_log2(r.source.modifier, 4), Some(Some(4)));
    }

    #[test]
    fn absent_when_empty_short_of_a_magic_or_zero() {
        assert_eq!(HeliosRmFenceTailV3::parse(&[]), TailV3::Absent);
        assert_eq!(HeliosRmFenceTailV3::parse(&[0x48, 0x45, 0x46]), TailV3::Absent);
        assert_eq!(HeliosRmFenceTailV3::parse(&[0u8; 96]), TailV3::Absent);
        assert_eq!(HeliosRmFenceTailV3::parse(&[0u8; 4]), TailV3::Absent);
    }

    #[test]
    fn a_short_or_lying_record_is_refused() {
        let b = bytes(&heaven());
        for cut in [4usize, 16, 40, 95] {
            assert_eq!(
                HeliosRmFenceTailV3::parse(&b[..cut]),
                TailV3::Reject(TailV3Error::Short),
                "cut at {cut}"
            );
        }
        let mut r = heaven();
        r.bytes = 95;
        assert_eq!(reject(r), TailV3Error::Short);
        // `bytes` past the end of the command.
        r.bytes = 97;
        assert_eq!(reject(r), TailV3Error::Short);
        // A later, longer revision is read as version 3 when the command covers it.
        r.bytes = 104;
        let mut longer = bytes(&r);
        longer.extend_from_slice(&[0xee; 8]);
        assert!(matches!(HeliosRmFenceTailV3::parse(&longer), TailV3::Record(_)));
    }

    #[test]
    fn magic_version_flags_and_reserved_are_exact() {
        let mut r = heaven();
        r.magic = 0x4353_4F48; // HOSC
        assert_eq!(reject(r), TailV3Error::BadMagic);
        for v in [0u16, 1, 2, 4, 0xffff] {
            let mut r = heaven();
            r.version = v;
            assert_eq!(reject(r), TailV3Error::Version, "version {v}");
        }
        let mut r = heaven();
        r.flags |= 1 << 2;
        assert_eq!(reject(r), TailV3Error::Flags);
        for f in [0, HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE, HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE] {
            let mut r = heaven();
            r.flags = f;
            assert_eq!(reject(r), TailV3Error::Incomplete, "flags {f}");
        }
        let mut r = heaven();
        r.reserved = 1;
        assert_eq!(reject(r), TailV3Error::Reserved);
        let mut r = heaven();
        r.source.reserved = 1;
        assert_eq!(reject(r), TailV3Error::Reserved);
        let mut r = heaven();
        r.source.flags = 1 << 1;
        assert_eq!(reject(r), TailV3Error::Flags);
        // The known source flag passes the parser (the route refuses it later).
        let mut r = heaven();
        r.source.flags = HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED;
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
    }

    #[test]
    fn handles_semaphore_offset_and_value() {
        for which in 0..4 {
            let mut r = heaven();
            match which {
                0 => r.semaphore.h_client = 0,
                1 => r.semaphore.h_memory = 0,
                2 => r.source.h_client = 0,
                _ => r.source.h_memory = 0,
            }
            assert_eq!(reject(r), TailV3Error::Handle, "handle {which}");
        }
        let mut r = heaven();
        r.semaphore.offset = 4;
        assert_eq!(reject(r), TailV3Error::SemaphoreOffset);
        r.semaphore.offset = u64::MAX - 7;
        assert_eq!(reject(r), TailV3Error::SemaphoreOffset);
        r.semaphore.offset = u64::MAX - 15; // 8-aligned, its 8 bytes end at u64::MAX - 7
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
        let mut r = heaven();
        r.semaphore.value = 0;
        assert_eq!(reject(r), TailV3Error::Value);
    }

    #[test]
    fn dimensions_format_and_pitch() {
        for (w, h) in [(0u32, 900u32), (1600, 0), (16_385, 900), (1600, 16_385)] {
            let mut r = heaven();
            r.source.width = w;
            r.source.height = h;
            assert_eq!(reject(r), TailV3Error::Dimensions, "{w}x{h}");
        }
        let mut r = heaven();
        r.source.fourcc = 0x1234_5678;
        assert_eq!(reject(r), TailV3Error::Format);
        // Two planes are not a copy source.
        r.source.fourcc = DRM_FORMAT_NV12;
        r.source.modifier = 0;
        assert_eq!(reject(r), TailV3Error::Format);
        let mut r = heaven();
        r.source.pitch = 6396; // under the row
        assert_eq!(reject(r), TailV3Error::Pitch);
        r.source.pitch = 6402; // off the 4-byte alignment
        assert_eq!(reject(r), TailV3Error::Pitch);
        r.source.pitch = 6404; // aligned, but not a whole GOB for block-linear
        assert_eq!(reject(r), TailV3Error::Pitch);
        r.source.modifier = 0; // LINEAR takes it
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
        r.source.pitch = HELIOS_RM_COPY_MAX_PITCH + 4;
        assert_eq!(reject(r), TailV3Error::Pitch);
    }

    #[test]
    fn modifier_families() {
        let mut r = heaven();
        for h in 0..=5u64 {
            r.source.modifier = MOD_NVIDIA_BL_GB20X | h;
            r.source.size = u64::MAX / 2;
            assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)), "h {h}");
        }
        r.source.modifier = MOD_NVIDIA_BL_GB20X | 6;
        assert_eq!(reject(r), TailV3Error::Modifier);
        // Another family's modifier, whatever its h, is refused for a 32 bpp format.
        r.source.modifier = MOD_NVIDIA_BL_GB20X_8BPP;
        assert_eq!(reject(r), TailV3Error::Modifier);
        // ... and the 8 bpp family is the one an R8 source must use.
        let mut r8 = heaven();
        r8.source.fourcc = DRM_FORMAT_R8;
        r8.source.width = 1600;
        r8.source.pitch = 1600;
        r8.source.modifier = MOD_NVIDIA_BL_GB20X_8BPP | 2;
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r8)), TailV3::Record(_)));
        r8.source.modifier = MOD_NVIDIA_BL_GB20X | 2;
        assert_eq!(reject(r8), TailV3Error::Modifier);
        // Vendor or kind changed (a compressed or other-kind modifier) is not a family member.
        let mut r = heaven();
        r.source.modifier = 0x0300_0000_0060_6014 | (1 << 23);
        assert_eq!(reject(r), TailV3Error::Modifier);
        r.source.modifier = 0x0300_0000_0060_7014;
        assert_eq!(reject(r), TailV3Error::Modifier);
    }

    #[test]
    fn size_bounds_and_overflow() {
        // Block-linear: the image needs whole blocks, 6400 * 1024 bytes.
        let mut r = heaven();
        r.source.size = 6_553_599;
        assert_eq!(reject(r), TailV3Error::Size);
        // LINEAR needs only height rows: 5760000 bytes.
        r.source.modifier = 0;
        r.source.size = 5_760_000;
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
        r.source.size = 5_759_999;
        assert_eq!(reject(r), TailV3Error::Size);
        // The offset counts.
        let mut r = heaven();
        r.source.offset = 4096;
        assert_eq!(reject(r), TailV3Error::Size);
        r.source.size += 4096;
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
        // offset + bytes overflows u64.
        let mut r = heaven();
        r.source.offset = u64::MAX - 100;
        r.source.size = u64::MAX;
        assert_eq!(reject(r), TailV3Error::Size);
        assert_eq!(r.source.end(), None);
        // The largest pitch times the largest block-rounded height cannot overflow by itself.
        let mut r = heaven();
        r.source.pitch = HELIOS_RM_COPY_MAX_PITCH;
        r.source.height = HELIOS_RM_COPY_MAX_DIM;
        r.source.modifier = MOD_NVIDIA_BL_GB20X | 5;
        r.source.size = u64::MAX;
        assert_eq!(r.source.end(), Some((1u64 << 20) * 16_384));
        assert!(matches!(HeliosRmFenceTailV3::parse(&bytes(&r)), TailV3::Record(_)));
    }

    #[test]
    fn the_semaphore_value_must_match_the_fence() {
        let r = heaven();
        let mut fence = HeliosRmFenceTail {
            rm_fence_handle: 7,
            flags: HELIOS_RM_FENCE_TAIL_FLAG_FENCE,
            rm_fence_value: 1234,
        };
        assert!(r.matches_fence(&fence));
        fence.rm_fence_value = 1235;
        assert!(!r.matches_fence(&fence));
        fence.rm_fence_value = 1234;
        fence.rm_fence_handle = 0;
        assert!(!r.matches_fence(&fence));
    }

    #[test]
    fn placement_composes_with_the_existing_carriers() {
        use crate::onscanout::HeliosPresentRefreshCmdOnScanout;
        use crate::rm_fence::HeliosPresentRenderCmdFence;
        // HERF 168: the fence tail at 32, a zero on-scanout slot at 48, the record at 72.
        let mut herf = HeliosPresentRefreshCmdOnScanout::zeroed();
        herf.base.base.magic = crate::wddm::HELIOS_PRESENT_REFRESH_MAGIC;
        herf.base.base.version = crate::wddm::HELIOS_PRESENT_REFRESH_VERSION;
        herf.base.fence = HeliosRmFenceTail {
            rm_fence_handle: 9,
            flags: HELIOS_RM_FENCE_TAIL_FLAG_FENCE,
            rm_fence_value: 1234,
        };
        let mut cmd = bytemuck::bytes_of(&herf).to_vec();
        cmd.extend_from_slice(bytemuck::bytes_of(&heaven()));
        assert_eq!(cmd.len(), 168);
        assert!(cmd[48..72].iter().all(|b| *b == 0));
        assert!(matches!(
            HeliosRmFenceTailV3::parse(&cmd[HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET..]),
            TailV3::Record(_)
        ));
        // HEPR 192: the record right after the 96-byte fence form.
        let hepr = HeliosPresentRenderCmdFence::zeroed();
        let mut cmd = bytemuck::bytes_of(&hepr).to_vec();
        cmd.extend_from_slice(bytemuck::bytes_of(&heaven()));
        assert_eq!(cmd.len(), 192);
        assert!(matches!(
            HeliosRmFenceTailV3::parse(&cmd[HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET..]),
            TailV3::Record(_)
        ));
    }

    // ── the C mirror (include/helios_rm_fence.h) ────────────────────────────────────────────

    const HEADER: &str = include_str!("../include/helios_rm_fence.h");

    fn c_define(name: &str) -> u64 {
        for line in HEADER.lines() {
            let Some(rest) = line.strip_prefix("#define ") else {
                continue;
            };
            let Some(value) = rest.strip_prefix(name) else {
                continue;
            };
            if !value.starts_with(' ') {
                continue;
            }
            let value = value.split("/*").next().unwrap_or("").trim();
            if let Some(bits) = value.strip_prefix("(1u << ") {
                return 1u64 << bits.trim_end_matches(')').parse::<u32>().unwrap();
            }
            let value = value.trim_end_matches('u').trim_end_matches("ull");
            return match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
                None => value.parse().unwrap(),
            };
        }
        panic!("{name} is not defined in helios_rm_fence.h");
    }

    /// `sizeof` / `offsetof` right-hand sides the header asserts for `lhs`, C or C++ spelling.
    fn c_asserted(lhs: &str, cxx: bool) -> Vec<u64> {
        let (m, lhs) = if cxx {
            ("static_assert(", lhs.replace("struct ", ""))
        } else {
            ("_Static_assert(", std::string::String::from(lhs))
        };
        let needle = std::format!("{m}{lhs} == ");
        HEADER
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix(needle.as_str()))
            .map(|rest| rest.split(',').next().unwrap().trim().parse().unwrap())
            .collect()
    }

    fn pinned(lhs: &str, rust: usize) {
        for cxx in [false, true] {
            assert_eq!(c_asserted(lhs, cxx), [rust as u64], "{lhs} (cxx {cxx})");
        }
    }

    #[test]
    fn the_c_mirror_agrees_with_the_rust_record() {
        use core::mem::{offset_of, size_of};
        for (name, v) in [
            ("HELIOS_RM_FENCE_TAIL_V3_MAGIC", HELIOS_RM_FENCE_TAIL_V3_MAGIC as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_VERSION", HELIOS_RM_FENCE_TAIL_V3_VERSION as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_BYTES", HELIOS_RM_FENCE_TAIL_V3_BYTES as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET", HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET", HELIOS_RM_FENCE_TAIL_V3_HEPR_OFFSET as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE", HELIOS_RM_FENCE_TAIL_V3_FLAG_SEMAPHORE as u64),
            ("HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE", HELIOS_RM_FENCE_TAIL_V3_FLAG_SOURCE as u64),
            ("HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED", HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED as u64),
            ("HELIOS_RM_FENCE_TAIL_FLAG_FENCE", HELIOS_RM_FENCE_TAIL_FLAG_FENCE as u64),
            (
                "HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE",
                crate::rm_fence::HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE as u64,
            ),
            (
                "HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE",
                crate::rm_fence::HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE as u64,
            ),
            ("HELIOS_D3D12_SUBMIT_VERSION_V4", crate::rm_fence::HELIOS_D3D12_SUBMIT_VERSION_V4 as u64),
        ] {
            assert_eq!(c_define(name), v, "{name}");
        }
        pinned("sizeof(struct HeliosRmSemaphoreLoc)", size_of::<HeliosRmSemaphoreLoc>());
        pinned("offsetof(struct HeliosRmSemaphoreLoc, offset)", offset_of!(HeliosRmSemaphoreLoc, offset));
        pinned("offsetof(struct HeliosRmSemaphoreLoc, value)", offset_of!(HeliosRmSemaphoreLoc, value));
        pinned("sizeof(struct HeliosRmCopySource)", size_of::<HeliosRmCopySource>());
        pinned("offsetof(struct HeliosRmCopySource, offset)", offset_of!(HeliosRmCopySource, offset));
        pinned("offsetof(struct HeliosRmCopySource, size)", offset_of!(HeliosRmCopySource, size));
        pinned("offsetof(struct HeliosRmCopySource, modifier)", offset_of!(HeliosRmCopySource, modifier));
        pinned("offsetof(struct HeliosRmCopySource, pitch)", offset_of!(HeliosRmCopySource, pitch));
        pinned("offsetof(struct HeliosRmCopySource, fourcc)", offset_of!(HeliosRmCopySource, fourcc));
        pinned("offsetof(struct HeliosRmCopySource, reserved)", offset_of!(HeliosRmCopySource, reserved));
        pinned("sizeof(struct HeliosRmFenceTailV3)", size_of::<HeliosRmFenceTailV3>());
        pinned("offsetof(struct HeliosRmFenceTailV3, bytes)", offset_of!(HeliosRmFenceTailV3, bytes));
        pinned(
            "offsetof(struct HeliosRmFenceTailV3, semaphore)",
            offset_of!(HeliosRmFenceTailV3, semaphore),
        );
        pinned("offsetof(struct HeliosRmFenceTailV3, source)", offset_of!(HeliosRmFenceTailV3, source));
    }
}
