//! `HOSC`: the "already on scanout" present tag (`guest/windows/docs/zero-copy-present.md`,
//! section "Already-on-scanout present tag").
//!
//! A producer that already put the frame on scanout through the user foreign-scanout source
//! (`HELIOS_NVRM_OP_SCANOUT_SET` / `SCANOUT_PRESENT`) still has to call `pfnPresentCb` for the
//! D3D11 frame-latency bookkeeping. dxgkrnl then asks the KMD for a full-frame `Blt` into the
//! window's redirection surface, which nothing will ever look at (the live source withholds the
//! desktop's host flush). The tag says so: the KMD completes that Present exactly as a copied one
//! (fence, patch references, stream boundary), with no copy and no host call, but only when it can
//! verify the claim against its own foreign-scanout state. A tag it cannot verify is counted and
//! the Present is the ordinary Blt.
//!
//! WHERE IT TRAVELS. Not in `pfnPresentCb`'s `pPrivateDriverData`: dxgkrnl does not forward that to
//! `DxgkDdiPresent` (measured, `PBIdOk` "no payload" across three driver generations; the D4b
//! snapshot and the stream marker had to move to the Render command for the same reason). The
//! tag is the TAIL of the `HERF` command ([`HeliosPresentRefreshCmd`]) the UMD already submits with
//! `pfnRenderCb` immediately before `pfnPresentCb`, on the same `hContext`: `DxgkDdiRender` parses
//! and stashes it, the Present that follows on that context takes it (same pairing and orphan
//! bound as the stream marker).
//!
//! ```text
//! HERF + fence slot + tag, CommandLength = 72, all little-endian
//!   0..32   HeliosPresentRefreshCmd   ('HERF', version 1, the stream tail as ever)
//!  32..48   HeliosRmFenceTail         (all zero unless an RM fence is attached; see rm_fence.rs)
//!  48..72   HeliosOnScanoutTag        ('HOSC', this module)
//! ```
//!
//! An older KMD copies the 72 bytes into the DMA buffer, reads the 32-byte `HERF` (and a zero
//! fence slot as "no fence"), ignores the rest and does the ordinary Blt: sending the tag is
//! always safe.

use bytemuck::{Pod, Zeroable};

use crate::rm_fence::HeliosPresentRefreshCmdFence;

/// `'HOSC'`: magic of [`HeliosOnScanoutTag`] (bytes `48 4F 53 43` in memory).
pub const HELIOS_ONSCANOUT_MAGIC: u32 = 0x4353_4F48;
/// Current tag version. Any other version is not this record: the tag is rejected (counted),
/// never reinterpreted.
pub const HELIOS_ONSCANOUT_VERSION: u16 = 1;
/// Byte offset of the tag in a `HERF` command: after the 32-byte `HERF` and the 16-byte fence
/// slot, so the fence variant and the tag compose.
pub const HELIOS_ONSCANOUT_HERF_OFFSET: usize = 48;
/// `CommandLength` of a `HERF` command that carries the tag.
pub const HELIOS_ONSCANOUT_HERF_BYTES: usize = 72;
/// How many frames behind the newest `SCANOUT_PRESENT` of the source a tag may be before the KMD
/// calls it stale and does the Blt. About a second at 240 fps; far more than the frame latency of
/// any swap chain.
pub const HELIOS_ONSCANOUT_MAX_LAG: u64 = 256;

/// The tag, 24 bytes, 8-aligned. See the module docs for where it sits.
///
/// The claim is "the frame I am presenting is the one I put on scanout with `SCANOUT_PRESENT`
/// number `sequence` of source `generation`, and I am the process that owns that source".
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosOnScanoutTag {
    /// [`HELIOS_ONSCANOUT_MAGIC`].
    pub magic: u32,
    /// [`HELIOS_ONSCANOUT_VERSION`].
    pub version: u16,
    /// Reserved, zero. A nonzero value is rejected so a later flag cannot be misread by this KMD.
    pub flags: u16,
    /// The `out_seq` that `HELIOS_NVRM_OP_SCANOUT_PRESENT` returned for this frame. Nonzero.
    pub sequence: u64,
    /// The `out_generation` that `HELIOS_NVRM_OP_SCANOUT_SET` returned for the live source.
    /// Nonzero.
    pub generation: u32,
    /// Optional consistency check: the Helios resource id of the Blt's source allocation
    /// (`IMPORT_RM` `out_resource_id` of the adopted image). 0 = not stated; nonzero must equal the
    /// Present's source or the tag is rejected.
    pub resource_id: u32,
}

/// The tag inside a complete `HERF` command (fence slot present, zero when unused).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRefreshCmdOnScanout {
    pub base: HeliosPresentRefreshCmdFence,
    pub tag: HeliosOnScanoutTag,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosOnScanoutTag>() == 24);
    assert!(offset_of!(HeliosOnScanoutTag, version) == 4);
    assert!(offset_of!(HeliosOnScanoutTag, flags) == 6);
    assert!(offset_of!(HeliosOnScanoutTag, sequence) == 8);
    assert!(offset_of!(HeliosOnScanoutTag, generation) == 16);
    assert!(offset_of!(HeliosOnScanoutTag, resource_id) == 20);
    assert!(size_of::<HeliosPresentRefreshCmdOnScanout>() == HELIOS_ONSCANOUT_HERF_BYTES);
    assert!(offset_of!(HeliosPresentRefreshCmdOnScanout, tag) == HELIOS_ONSCANOUT_HERF_OFFSET);
    assert!(
        HELIOS_ONSCANOUT_HERF_OFFSET + size_of::<HeliosOnScanoutTag>()
            == HELIOS_ONSCANOUT_HERF_BYTES
    );
};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::wddm::{
        HELIOS_D3D12_SUBMIT_MAGIC, HELIOS_PRESENT_REFRESH_MAGIC, HELIOS_PRESENT_REFRESH_VERSION,
        HELIOS_PRESENT_RENDER_MAGIC,
    };
    use crate::{HELIOS_FLUSH_GATE_MAGIC, HELIOS_RM_FENCE_TAIL_FLAG_FENCE};
    use std::format;

    #[test]
    fn magic_reads_hosc_in_memory_and_is_distinct() {
        assert_eq!(&HELIOS_ONSCANOUT_MAGIC.to_le_bytes(), b"HOSC");
        for other in [
            HELIOS_D3D12_SUBMIT_MAGIC,
            HELIOS_PRESENT_REFRESH_MAGIC,
            HELIOS_PRESENT_RENDER_MAGIC,
            HELIOS_FLUSH_GATE_MAGIC,
        ] {
            assert_ne!(HELIOS_ONSCANOUT_MAGIC, other);
        }
    }

    #[test]
    fn a_built_tag_has_the_documented_bytes() {
        let tag = HeliosOnScanoutTag {
            magic: HELIOS_ONSCANOUT_MAGIC,
            version: HELIOS_ONSCANOUT_VERSION,
            flags: 0,
            sequence: 0x0102_0304_0506_0708,
            generation: 0x1112_1314,
            resource_id: 0x2122_2324,
        };
        let b = bytemuck::bytes_of(&tag);
        assert_eq!(b.len(), 24);
        assert_eq!(&b[0..4], b"HOSC");
        assert_eq!(u16::from_le_bytes(b[4..6].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(b[6..8].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(b[8..16].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            u32::from_le_bytes(b[16..20].try_into().unwrap()),
            0x1112_1314
        );
        assert_eq!(
            u32::from_le_bytes(b[20..24].try_into().unwrap()),
            0x2122_2324
        );
    }

    #[test]
    fn the_herf_carrier_is_72_bytes_with_the_tag_at_48_and_a_zero_fence_slot() {
        let mut cmd = HeliosPresentRefreshCmdOnScanout::zeroed();
        cmd.base.base.magic = HELIOS_PRESENT_REFRESH_MAGIC;
        cmd.base.base.version = HELIOS_PRESENT_REFRESH_VERSION;
        cmd.tag.magic = HELIOS_ONSCANOUT_MAGIC;
        cmd.tag.version = HELIOS_ONSCANOUT_VERSION;
        cmd.tag.sequence = 9;
        cmd.tag.generation = 3;
        let b = bytemuck::bytes_of(&cmd);
        assert_eq!(b.len(), HELIOS_ONSCANOUT_HERF_BYTES);
        assert_eq!(&b[0..4], b"HERF");
        // The fence slot is zero: an older KMD reads "no fence".
        assert!(b[32..48].iter().all(|x| *x == 0));
        assert_eq!(
            &b[HELIOS_ONSCANOUT_HERF_OFFSET..HELIOS_ONSCANOUT_HERF_OFFSET + 4],
            b"HOSC"
        );
        // An RM fence composes with it: the tag does not move.
        cmd.base.fence.flags = HELIOS_RM_FENCE_TAIL_FLAG_FENCE;
        cmd.base.fence.rm_fence_handle = 7;
        let b = bytemuck::bytes_of(&cmd);
        assert_eq!(
            &b[HELIOS_ONSCANOUT_HERF_OFFSET..HELIOS_ONSCANOUT_HERF_OFFSET + 4],
            b"HOSC"
        );
    }

    #[test]
    fn the_c_header_agrees() {
        let header = include_str!("../include/helios_onscanout.h");
        for (name, value) in [
            (
                "HELIOS_ONSCANOUT_MAGIC",
                format!("0x{:08X}u", HELIOS_ONSCANOUT_MAGIC),
            ),
            (
                "HELIOS_ONSCANOUT_VERSION",
                format!("{}u", HELIOS_ONSCANOUT_VERSION),
            ),
            (
                "HELIOS_ONSCANOUT_HERF_OFFSET",
                format!("{}u", HELIOS_ONSCANOUT_HERF_OFFSET),
            ),
            (
                "HELIOS_ONSCANOUT_HERF_BYTES",
                format!("{}u", HELIOS_ONSCANOUT_HERF_BYTES),
            ),
            (
                "HELIOS_ONSCANOUT_MAX_LAG",
                format!("{}u", HELIOS_ONSCANOUT_MAX_LAG),
            ),
        ] {
            let line = header
                .lines()
                .find(|l| l.starts_with(&format!("#define {name} ")))
                .unwrap_or_else(|| panic!("{name} missing from the header"));
            assert!(
                line.contains(&value),
                "{name}: header says `{line}`, Rust says {value}"
            );
        }
    }
}
