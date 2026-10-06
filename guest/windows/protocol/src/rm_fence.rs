//! RM fence markers: how a present names its completion boundary as "this RM
//! semaphore value was reached", through a backend fence handle the KMD takes over.
//!
//! Contract and rationale: `guest/windows/docs/rm-fence-marker.md`. The C mirror is
//! `protocol/include/helios_rm_fence.h`, and the NVRM-side bits (`SCANOUT_PRESENT`
//! flag, `QUERY_CAPS` bits, statuses) are mirrored in
//! `guest/rmclient/src/helios_nvrm_escape.h`.
//!
//! A fence is a backend handle returned by a forwarded nvidia-drm
//! `SEMSURF_FENCE_CREATE` (ioctl 0x55, low 16 bits `0x6455`) on a DRM-node handle of
//! the caller. The KMD records it as `DEVICE_TYPE_FENCE` (511). The host sends one
//! `EventReady` for it when the semaphore reaches the value (or, with the fence's
//! error status, when nvidia-drm times it out after at most 5 s).
//!
//! # Carriers
//!
//! | carrier | where | what retires on the fence |
//! |---|---|---|
//! | (a) `HELIOS_NVRM_OP_SCANOUT_PRESENT` + [`HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE`] | NVRM escape, `nvrm_scanout` | the host `ScanoutFlip` is sent when the fence fires |
//! | (b) [`HeliosPresentRefreshCmdFence`], `HeliosPresentRenderCmd` tail ([`HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE`]) | `pfnRenderCb` command before `pfnPresentCb` | the present's DMA fence and its scanout bind / windowed blit |
//! | (b) [`HeliosD3D12SubmitCmdV4`] | `pfnRenderCb` ExecuteCommandLists record | the batch's DMA completion (so the runtime's monitored-fence signals) |
//!
//! # Ownership (all carriers)
//!
//! The KMD takes the handle over when the carrier is ACCEPTED (status OK for (a);
//! `DxgkDdiRender` parsing the record for (b)). From then on the caller must
//! never `Close`, `EVENT_REGISTER` on, `FORWARD` on or reuse it: all of those answer
//! `NOT_OWNED`. The KMD closes it on the host when it fires or on any teardown.
//! A refused carrier whose call status the caller sees ((a) `SCANOUT_PRESENT`, and
//! `HE12` version 4) leaves the handle the caller's, who may CPU-wait and `Close`.
//!
//! The exception is a `HERF` / `HEPR` tail (carrier (b)): its `Render` returns success
//! whatever became of the tail, so the caller cannot learn a refusal. For ANY parsed
//! tail whose handle is a fence of the presenting process the KMD therefore takes
//! the handle and closes it, attached as the present's marker or not (both markers
//! in one record, a partial stream tail, no room, ...). Never `Close` a handle you
//! put in such a tail.

use crate::wddm::{HeliosD3D12SubmitCmd, HeliosPresentRefreshCmd, HeliosPresentRenderCmd};
use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Capability bits: `HeliosNvrmQueryCaps.supported_ops` bits 32..63.
//
// Op numbers are `u32`, so bit `n >= 32` of the 64-bit mask can never name an op;
// those bits are free for capabilities and `QUERY_CAPS` keeps its 88-byte size.
// ---------------------------------------------------------------------------

/// `supported_ops` bit 32: `SCANOUT_PRESENT` accepts
/// [`HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE`]. Set only while the KMD can
/// actually serve it: the event queue is up, the host advertises
/// `NVGPU_CFG_DRM_FENCES` (config `features` bit 11), and the scanout ops exist.
pub const HELIOS_NVRM_CAP_SCANOUT_FENCE: u64 = 1 << 32;
/// `supported_ops` bit 33: the WDDM carriers (b) are honoured. Same preconditions.
/// A UMD must not send a (b) record without it: an older KMD ignores a longer
/// `HERF`, which merely degrades to the legacy wait, but REFUSES `HE12` version 4.
pub const HELIOS_NVRM_CAP_PRESENT_FENCE: u64 = 1 << 33;
/// `supported_ops` bit 34: the flush gate (`HEFL`, [`crate::flush_gate`]) honours its
/// RM fence variant. Set under the same preconditions as bit 33. A UMD must not send a
/// `HEFL` with an RM fence without it: an older KMD does not know the record, gates
/// nothing and does not take the handle.
pub const HELIOS_NVRM_CAP_FLUSH_GATE: u64 = 1 << 34;

// ---------------------------------------------------------------------------
// Statuses (`HeliosNvrmHeader.status`). 13 and 14 are `SCANOUT_BUSY` / `NO_SOURCE`.
// ---------------------------------------------------------------------------

/// Reserved: an already attached handle is the KMD's, so today it answers
/// `NOT_OWNED` like any handle that is not the caller's.
pub const HELIOS_NVRM_ST_FENCE_ATTACHED: i32 = 15;
/// `SCANOUT_PRESENT` with a fence: the source already has
/// [`HELIOS_NVRM_SCANOUT_FENCE_DEPTH`] presents waiting. Retry after the oldest
/// frame's fence fires, or present without a fence (CPU-complete).
pub const HELIOS_NVRM_ST_QUEUE_FULL: i32 = 16;

/// Most `SCANOUT_PRESENT`s that may wait on a fence at once (per source; the KMD
/// has exactly one source). A client rotating N images keeps at most N-1 waiting.
pub const HELIOS_NVRM_SCANOUT_FENCE_DEPTH: usize = 8;

/// `HeliosNvrmScanoutPresent.flags` bit 0: `rm_fence_handle` is a fence to attach.
/// Without the bit `rm_fence_handle` must be 0 (`BAD_RANGE` otherwise) and the
/// flip is sent before the call returns, as before.
pub const HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// (b) WDDM carriers.
// ---------------------------------------------------------------------------

/// [`HeliosRmFenceTail::flags`] bit 0: `rm_fence_handle` is a fence to attach.
pub const HELIOS_RM_FENCE_TAIL_FLAG_FENCE: u32 = 1 << 0;
/// [`HeliosRmFenceTail::flags`] bit 1 (`HE12` only, no handle): this batch has
/// nothing to wait for (the producer already waited on the CPU). The record is
/// accepted and creates no boundary, so the DMA packet retires by the ordinary
/// wire rule.
pub const HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE: u32 = 1 << 1;
/// Every flag bit the KMD knows; any other bit refuses the record.
pub const HELIOS_RM_FENCE_TAIL_FLAGS_ALL: u32 =
    HELIOS_RM_FENCE_TAIL_FLAG_FENCE | HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE;

/// `HeliosPresentPrivateData::reserved` bit 3: the appended
/// [`HeliosRmFenceTail`] of `HeliosPresentRenderCmd` is valid. Honoured only when
/// the command covers the whole tail.
pub const HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE: u32 = 1 << 3;

/// `HeliosD3D12SubmitCmdV4::version`.
pub const HELIOS_D3D12_SUBMIT_VERSION_V4: u32 = 4;

/// The 16 bytes appended to a present marker.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct HeliosRmFenceTail {
    /// A backend handle from a forwarded `SEMSURF_FENCE_CREATE`, recorded by the
    /// KMD as a fence of a device in the SAME PROCESS as the present's creator
    /// (NVK's NVRM device differs from the WDDM device that presents). 0 = none.
    pub rm_fence_handle: u32,
    /// `HELIOS_RM_FENCE_TAIL_FLAG_*`.
    pub flags: u32,
    /// DIAGNOSTIC ONLY: the semaphore value the fence waits for (the value is
    /// already baked into the fence). Never read for a decision.
    pub rm_fence_value: u64,
}

/// `HERF` command with the fence tail: 32 -> 48 bytes. The v1 prefix and its
/// version are unchanged (no version bump: a bumped `HERF` would be IGNORED by an
/// older KMD, losing the scanout-refresh arm, where a longer one merely loses the
/// tail). The tail is read only when `CommandLength >= 48`, and the stream tail
/// (`present_ctx_id`, `present_value`, `present_cookie`) must then be all zero:
/// the two markers are exclusive, a record carrying both has its stream marker
/// honoured and its fence NOT attached (the present follows the stream marker); the
/// fence handle is still the KMD's and is closed (`Render` cannot tell the caller).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRefreshCmdFence {
    pub base: HeliosPresentRefreshCmd,
    pub fence: HeliosRmFenceTail,
}

/// `HEPR` command (`HeliosPresentRenderCmd`, 80 bytes) with the fence tail, 96
/// bytes. The tail is read only when `CommandLength >= 96` AND
/// `present.reserved` has [`HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE`]; the stream tail
/// (`present_ctx_id`, `present_value`, `present_cookie`) must then be zero.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosPresentRenderCmdFence {
    pub base: HeliosPresentRenderCmd,
    pub fence: HeliosRmFenceTail,
}

/// `HE12` version 4: the version 3 record (32 bytes) with the fence tail, 48 bytes.
///
/// Fence variant: `ctx_id`, `value`, `cookie` and `gpu_wire_fence` are 0,
/// `fence.flags == FENCE`, `fence.rm_fence_handle != 0`. CPU-complete variant: all
/// of those 0 and `fence.flags == COMPLETE`. The stream variant is the unchanged
/// version 3 record (`fence` absent or zero). An older KMD refuses a version 4
/// record (`STATUS_INVALID_PARAMETER` from Render), which is why `HE12` is gated on
/// [`HELIOS_NVRM_CAP_PRESENT_FENCE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosD3D12SubmitCmdV4 {
    pub base: HeliosD3D12SubmitCmd,
    pub fence: HeliosRmFenceTail,
}

impl HeliosRmFenceTail {
    /// Shape check shared by every (b) carrier. A fence names a nonzero handle;
    /// unknown flag bits are refused; `COMPLETE` and a handle exclude each other.
    #[inline]
    pub const fn is_fence(&self) -> bool {
        self.flags & !HELIOS_RM_FENCE_TAIL_FLAGS_ALL == 0
            && self.flags & HELIOS_RM_FENCE_TAIL_FLAG_FENCE != 0
            && self.flags & HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE == 0
            && self.rm_fence_handle != 0
    }

    /// The tail says "nothing to wait for" (`HE12` only).
    #[inline]
    pub const fn is_complete(&self) -> bool {
        self.flags == HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE && self.rm_fence_handle == 0
    }
}

impl HeliosD3D12SubmitCmdV4 {
    /// Fence variant: no stream, no wire fence, a fence handle.
    #[inline]
    pub const fn is_fence_record(&self) -> bool {
        self.base.magic == crate::wddm::HELIOS_D3D12_SUBMIT_MAGIC
            && self.base.version == HELIOS_D3D12_SUBMIT_VERSION_V4
            && self.base.ctx_id == 0
            && self.base.value == 0
            && self.base.cookie == 0
            && self.base.gpu_wire_fence == 0
            && self.fence.is_fence()
    }

    /// CPU-complete variant.
    #[inline]
    pub const fn is_complete_record(&self) -> bool {
        self.base.magic == crate::wddm::HELIOS_D3D12_SUBMIT_MAGIC
            && self.base.version == HELIOS_D3D12_SUBMIT_VERSION_V4
            && self.base.ctx_id == 0
            && self.base.value == 0
            && self.base.cookie == 0
            && self.base.gpu_wire_fence == 0
            && self.fence.is_complete()
    }
}

impl HeliosPresentRefreshCmdFence {
    /// The fence variant of a `HERF` command: valid base, no stream tail, a
    /// fence tail.
    #[inline]
    pub fn is_fence_record(&self) -> bool {
        self.base.is_valid()
            && self.base.present_ctx_id == 0
            && self.base.present_value == 0
            && self.base.present_cookie == 0
            && self.fence.is_fence()
    }
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosRmFenceTail>() == 16);
    assert!(size_of::<HeliosPresentRefreshCmdFence>() == 48);
    assert!(offset_of!(HeliosPresentRefreshCmdFence, fence) == 32);
    assert!(size_of::<HeliosPresentRenderCmdFence>() == 96);
    assert!(offset_of!(HeliosPresentRenderCmdFence, fence) == 80);
    assert!(size_of::<HeliosD3D12SubmitCmdV4>() == 48);
    assert!(offset_of!(HeliosD3D12SubmitCmdV4, fence) == 32);
    assert!(HELIOS_NVRM_CAP_SCANOUT_FENCE >> 32 == 1 && HELIOS_NVRM_CAP_PRESENT_FENCE >> 33 == 1);
    assert!(HELIOS_NVRM_CAP_FLUSH_GATE >> 34 == 1);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wddm::{HELIOS_D3D12_SUBMIT_MAGIC, HELIOS_PRESENT_REFRESH_MAGIC};

    fn tail(handle: u32, flags: u32) -> HeliosRmFenceTail {
        HeliosRmFenceTail {
            rm_fence_handle: handle,
            flags,
            rm_fence_value: 0,
        }
    }

    #[test]
    fn a_tail_is_a_fence_only_with_the_flag_and_a_handle() {
        assert!(tail(7, HELIOS_RM_FENCE_TAIL_FLAG_FENCE).is_fence());
        assert!(!tail(0, HELIOS_RM_FENCE_TAIL_FLAG_FENCE).is_fence());
        assert!(!tail(7, 0).is_fence());
        assert!(!tail(7, HELIOS_RM_FENCE_TAIL_FLAGS_ALL).is_fence());
        assert!(!tail(7, HELIOS_RM_FENCE_TAIL_FLAG_FENCE | 4).is_fence());
        assert!(tail(0, HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE).is_complete());
        assert!(!tail(1, HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE).is_complete());
        assert!(!tail(0, HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE | 4).is_complete());
    }

    #[test]
    fn herf_fence_and_stream_markers_are_exclusive() {
        let mut cmd = HeliosPresentRefreshCmdFence::zeroed();
        cmd.base.magic = HELIOS_PRESENT_REFRESH_MAGIC;
        cmd.base.version = crate::wddm::HELIOS_PRESENT_REFRESH_VERSION;
        cmd.fence = tail(9, HELIOS_RM_FENCE_TAIL_FLAG_FENCE);
        assert!(cmd.is_fence_record());
        cmd.base.present_cookie = 1;
        assert!(!cmd.is_fence_record());
    }

    #[test]
    fn he12_v4_variants_are_exclusive_and_never_a_v3_stream_record() {
        let mut cmd = HeliosD3D12SubmitCmdV4::zeroed();
        cmd.base.magic = HELIOS_D3D12_SUBMIT_MAGIC;
        cmd.base.version = HELIOS_D3D12_SUBMIT_VERSION_V4;
        cmd.fence = tail(9, HELIOS_RM_FENCE_TAIL_FLAG_FENCE);
        assert!(cmd.is_fence_record() && !cmd.is_complete_record());
        // The v3 validity gate refuses it by version: an old reader fails closed.
        assert!(!cmd.base.is_valid());
        cmd.fence = tail(0, HELIOS_RM_FENCE_TAIL_FLAG_COMPLETE);
        assert!(cmd.is_complete_record() && !cmd.is_fence_record());
        cmd.base.value = 1;
        assert!(!cmd.is_complete_record());
    }
}
