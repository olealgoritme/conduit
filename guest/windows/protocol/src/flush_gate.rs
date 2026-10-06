//! `HEFL`: the flush gate, a WDDM record that makes the DMA fence of a D3D11 `pfnFlush`
//! mean "the GPU work of this flush is done" (`guest/windows/docs/flush-gate.md`).
//!
//! A D3D11 device submits its GPU work through the ICD's own escapes (Venus ring) or
//! through the RM channel (NVK), not in WDDM DMA buffers, so the fence dxgkrnl orders a
//! keyed-mutex release against retires with nothing behind it. The UMD closes that for a
//! device that holds a keyed-mutex resource by submitting ONE tiny render packet from
//! `pfnFlush` (`pfnRenderCb`, no allocations, no patches) whose command is this record.
//! The KMD turns it into a completion boundary of that packet's WDDM fence.
//!
//! The record never fails `DxgkDdiRender` for anything about its boundary (like `HERF`
//! and `HEPR`, unlike `HE12`): a boundary the KMD cannot honour degrades to the legacy
//! wire-prefix rule and is counted, and a fence handle in the tail is always the KMD's
//! afterwards. Capability bits: [`crate::HELIOS_SCANOUT_CAP_FLUSH_GATE`] (Venus, the
//! `MAP_READ_LEDGER` PROBE reply) and [`crate::HELIOS_NVRM_CAP_FLUSH_GATE`] (NVK, the
//! NVRM `QUERY_CAPS` reply). An older KMD does not know the magic: it records the bytes
//! into the DMA buffer, returns success, gates nothing and does NOT take the fence
//! handle, so a UMD must never send the record without the capability.

use bytemuck::{Pod, Zeroable};

use crate::rm_fence::HeliosRmFenceTail;

/// `'HEFL'`: magic of [`HeliosFlushGateCmd`]. Distinct from `HEPR`, `HERF` and `HE12`
/// (`DxgkDdiRender` tries each decode on every command of enough length).
pub const HELIOS_FLUSH_GATE_MAGIC: u32 = 0x4C46_4548;
/// Current record version. Any other version is not this record (the Render copies it
/// and gates nothing).
pub const HELIOS_FLUSH_GATE_VERSION: u32 = 1;

/// `flags` bit 0: `ctx_id`, `value` and `cookie` name a point of the process's
/// registered Venus producer stream (the same tuple as a present marker). `ctx_id` and
/// `cookie` must be nonzero; `value` may be 0 ("already complete": the stream must be
/// live, nothing is waited for).
pub const HELIOS_FLUSH_GATE_FLAG_STREAM: u32 = 1 << 0;
/// `flags` bit 1: `fence` is an RM fence (`rm-fence-marker.md`) of this process, taken
/// over by the KMD. The handle is never the caller's afterwards, attached or not.
pub const HELIOS_FLUSH_GATE_FLAG_RM_FENCE: u32 = 1 << 1;
/// Every flag this version defines. Any other bit makes the record "no boundary".
pub const HELIOS_FLUSH_GATE_FLAGS_ALL: u32 =
    HELIOS_FLUSH_GATE_FLAG_STREAM | HELIOS_FLUSH_GATE_FLAG_RM_FENCE;

/// The flush-gate command, 48 bytes, the whole `CommandLength` of one `pfnRenderCb`
/// with `NumAllocations = NumPatchLocations = 0`.
///
/// `flags == 0` is the WIRE rung: no boundary of its own, the packet retires by the
/// ordinary rule (every transport entry enqueued before `SubmitCommand`, GPU
/// completion included). That is only a proof for work that already reached the
/// transport, so a producer that sends it waits for its own submission thread first.
/// `STREAM` and `RM_FENCE` are exclusive: with both, a complete stream point is
/// honoured and the fence is taken and closed unattached.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosFlushGateCmd {
    pub magic: u32,
    pub version: u32,
    /// `HELIOS_FLUSH_GATE_FLAG_*`.
    pub flags: u32,
    /// Venus context id of the producer stream (STREAM).
    pub ctx_id: u32,
    /// Stream point (STREAM). The KMD does not order it against earlier gates and does
    /// not bound it (a point ahead of the submitted tag is the normal state, exactly as
    /// for a present marker): the stream's retirement decides.
    pub value: u32,
    /// Reserved, zero.
    pub reserved: u32,
    /// The stream's registration cookie (STREAM).
    pub cookie: u64,
    /// The RM fence tail (RM_FENCE): `flags == FENCE`, a nonzero handle.
    pub fence: HeliosRmFenceTail,
}

impl HeliosFlushGateCmd {
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.magic == HELIOS_FLUSH_GATE_MAGIC && self.version == HELIOS_FLUSH_GATE_VERSION
    }
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosFlushGateCmd>() == 48);
    assert!(offset_of!(HeliosFlushGateCmd, flags) == 8);
    assert!(offset_of!(HeliosFlushGateCmd, ctx_id) == 12);
    assert!(offset_of!(HeliosFlushGateCmd, value) == 16);
    assert!(offset_of!(HeliosFlushGateCmd, cookie) == 24);
    assert!(offset_of!(HeliosFlushGateCmd, fence) == 32);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rm_fence::{HELIOS_NVRM_CAP_FLUSH_GATE, HELIOS_RM_FENCE_TAIL_FLAG_FENCE};
    use crate::wddm::{
        HELIOS_D3D12_SUBMIT_MAGIC, HELIOS_PRESENT_REFRESH_MAGIC, HELIOS_PRESENT_RENDER_MAGIC,
    };

    #[test]
    fn magic_reads_hefl_in_memory_and_is_distinct() {
        assert_eq!(&HELIOS_FLUSH_GATE_MAGIC.to_le_bytes(), b"HEFL");
        for other in [
            HELIOS_D3D12_SUBMIT_MAGIC,
            HELIOS_PRESENT_REFRESH_MAGIC,
            HELIOS_PRESENT_RENDER_MAGIC,
        ] {
            assert_ne!(HELIOS_FLUSH_GATE_MAGIC, other);
        }
    }

    #[test]
    fn zeroed_record_is_not_valid_and_a_built_one_is() {
        let mut cmd = HeliosFlushGateCmd::zeroed();
        assert!(!cmd.is_valid());
        cmd.magic = HELIOS_FLUSH_GATE_MAGIC;
        assert!(!cmd.is_valid());
        cmd.version = HELIOS_FLUSH_GATE_VERSION;
        assert!(cmd.is_valid());
        cmd.version = 2;
        assert!(!cmd.is_valid());
    }

    #[test]
    fn layout_matches_the_c_header() {
        let mut cmd = HeliosFlushGateCmd::zeroed();
        cmd.magic = HELIOS_FLUSH_GATE_MAGIC;
        cmd.version = HELIOS_FLUSH_GATE_VERSION;
        cmd.flags = HELIOS_FLUSH_GATE_FLAG_RM_FENCE;
        cmd.fence = HeliosRmFenceTail {
            rm_fence_handle: 0x77,
            flags: HELIOS_RM_FENCE_TAIL_FLAG_FENCE,
            rm_fence_value: 9,
        };
        let bytes = bytemuck::bytes_of(&cmd);
        assert_eq!(bytes.len(), 48);
        assert_eq!(&bytes[0..4], b"HEFL");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 0x77);
        assert_eq!(u64::from_le_bytes(bytes[40..48].try_into().unwrap()), 9);
    }

    #[test]
    fn flag_bits_are_distinct_and_the_nvrm_capability_is_a_cap_bit() {
        assert_eq!(
            HELIOS_FLUSH_GATE_FLAG_STREAM & HELIOS_FLUSH_GATE_FLAG_RM_FENCE,
            0
        );
        assert_eq!(HELIOS_FLUSH_GATE_FLAGS_ALL, 3);
        // Bits 32.. of `supported_ops` are capabilities, never op numbers.
        assert!(HELIOS_NVRM_CAP_FLUSH_GATE >= 1 << 32);
        assert_eq!(HELIOS_NVRM_CAP_FLUSH_GATE.count_ones(), 1);
    }
}
