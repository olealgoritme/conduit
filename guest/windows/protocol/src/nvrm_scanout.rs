//! The foreign scanout source: three more `HELIOS_ESCAPE_NVRM` ops that let a
//! process own scanout 0 and present host GEM objects to it through the KMD.
//!
//! Why. `FORWARD` of a `ScanoutFlip` (host `MsgType` 20) shows the app's image,
//! but the desktop keeps flipping the same scanout through Venus
//! (`SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`), so the two alternate, and WDDM knows
//! nothing of the app's frames. With a source registered the KMD does three things:
//!
//! * it SUPPRESSES the desktop's flushes of scanout 0 (the only thing that makes the
//!   host show a Venus resource) while the source is live, completing everything of
//!   the desktop's present path as usual: only the host flush is withheld;
//! * it sends the `ScanoutFlip` itself, from [`HeliosNvrmScanoutPresent`]: the
//!   layout was validated once at `SET`, the sequence number is minted by the KMD
//!   (strictly increasing, never reused), and only the registering device can
//!   present;
//! * it gives scanout 0 back, with one fresh desktop flush, when the source is
//!   released, the owner closes the DRM file or exits, the device resets, or the
//!   owner stops presenting for `lapse_ms` (a fallback so a hung process cannot
//!   freeze the desktop).
//!
//! # Usage
//!
//! ```text
//! SET       { handle = DRM-node handle (Open device_type >= 512), layout, lapse_ms }
//! PRESENT   { handle, gem }            per frame; pushes the lapse out; out_seq
//! PRESENT   ...
//! RELEASE   { handle }                 (or just close the file / exit)
//! ```
//!
//! `SET` again from the same device changes the layout or file in place. A
//! `PRESENT` after the lapse answers [`HELIOS_NVRM_ST_NO_SOURCE`]: `SET` again.
//! While another device's source is live `SET` answers
//! [`HELIOS_NVRM_ST_SCANOUT_BUSY`], and a `FORWARD` of a `ScanoutFlip` from a
//! device that does not hold it is refused `FORBIDDEN`.
//!
//! The ops are advertised in `HeliosNvrmQueryCaps.supported_ops` (bits 9, 10, 11). A
//! fourth, `SCANOUT_STATUS` (bit 12, with capability bit 34), exists only where the host's
//! buffer releases are on: the precise answer to "may I write this image again?".
//! Calls from one device should be serialised by the client, as `FORWARD` is: the
//! KMD mints `seq` in call order but the host takes frames in arrival order.
//!
//! Layout rules and the state machine: `helios_kmd_logic::foreign_scanout`.

use crate::nvrm::HeliosNvrmHeader;
use bytemuck::{Pod, Zeroable};

/// Own scanout 0. See [`HeliosNvrmScanoutSet`].
pub const HELIOS_NVRM_OP_SCANOUT_SET: u32 = 9;
/// Show one GEM object on scanout 0. See [`HeliosNvrmScanoutPresent`].
pub const HELIOS_NVRM_OP_SCANOUT_PRESENT: u32 = 10;
/// Give scanout 0 back. See [`HeliosNvrmScanoutRelease`].
pub const HELIOS_NVRM_OP_SCANOUT_RELEASE: u32 = 11;
/// Read which presented images the host is done with. See [`HeliosNvrmScanoutStatus`].
/// Offered only with [`HELIOS_NVRM_CAP_SCANOUT_RELEASE`].
pub const HELIOS_NVRM_OP_SCANOUT_STATUS: u32 = 12;
/// The bits these ops occupy in `QueryCaps.supported_ops`.
pub const HELIOS_NVRM_SCANOUT_OPS: u64 = (1 << HELIOS_NVRM_OP_SCANOUT_SET)
    | (1 << HELIOS_NVRM_OP_SCANOUT_PRESENT)
    | (1 << HELIOS_NVRM_OP_SCANOUT_RELEASE);
/// The op bit of `SCANOUT_STATUS`, ORed in only on a device with buffer releases.
pub const HELIOS_NVRM_SCANOUT_STATUS_OPS: u64 = 1 << HELIOS_NVRM_OP_SCANOUT_STATUS;
/// `supported_ops` bit 34 (a capability, like bits 32 and 33 in `rm_fence`): the KMD acked
/// the host's `NVGPU_F_SCANOUT_RELEASE`, so `SCANOUT_STATUS` and the event kind
/// `HELIOS_NVRM_EVENT_SCANOUT_RELEASED` work and the KMD's own ring presenter waits for
/// releases. Gate on this bit, never on the op bit alone.
pub const HELIOS_NVRM_CAP_SCANOUT_RELEASE: u64 = 1 << 34;

/// `SET`: another device holds scanout 0 and is still presenting.
pub const HELIOS_NVRM_ST_SCANOUT_BUSY: i32 = 13;
/// `PRESENT`: the caller has no live source on that handle (never set, released,
/// lapsed, its file closed, or the device reset). Also the answer for somebody
/// else's source, so a process learns nothing of another's.
pub const HELIOS_NVRM_ST_NO_SOURCE: i32 = 14;

/// `SET`. 88 bytes. A bad field is `BAD_RANGE`; `handle` not a DRM-node handle of
/// the caller is `NOT_OWNED` (or `FORBIDDEN` when it is the caller's but not a DRM
/// node, as for a forwarded `ScanoutFlip`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmScanoutSet {
    pub head: HeliosNvrmHeader,
    /// in: backend handle of a DRM-node file the caller opened.
    pub handle: u32,
    /// in: zero.
    pub flags: u32,
    /// in: 64..=16384.
    pub width: u32,
    /// in: 64..=16384.
    pub height: u32,
    /// in: plane 0 pitch in bytes, at least `width * 4`, at most 1 MiB.
    pub stride: u32,
    /// in: plane 0 offset in bytes.
    pub offset: u32,
    /// in: `DRM_FORMAT_{XRGB,ARGB,XBGR,ABGR}8888`.
    pub fourcc: u32,
    /// in: ms without a `PRESENT` after which the source lapses; 0 = 2000. Clamped
    /// to 100..=30000. out: the value in effect.
    pub lapse_ms: u32,
    /// in: `DRM_FORMAT_MOD_*` (NVIDIA block-linear allowed); passed to the host.
    pub modifier: u64,
    /// out: identifies this source (nonzero; an in-place `SET` keeps it).
    pub out_generation: u32,
    /// in: zero.
    pub reserved: u32,
}
pub const HELIOS_NVRM_SCANOUT_SET_BYTES: usize = 88;

/// `PRESENT`. 64 bytes. The host's own answer (a GEM handle it does not know, a
/// failed export) is `DEVICE_ERROR`; the frame is then not shown.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmScanoutPresent {
    pub head: HeliosNvrmHeader,
    /// in: the handle given to `SET`.
    pub handle: u32,
    /// in: the GEM handle, in that DRM file, of the image to show.
    pub gem: u32,
    /// in: `HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE` or zero (`rm_fence`).
    pub flags: u32,
    /// in: with the `RM_FENCE` flag, a fence handle of the caller's (from a
    /// forwarded `SEMSURF_FENCE_CREATE`) that the KMD takes over: the flip is sent
    /// when it fires and `out_seq` is returned at once. Zero without the flag.
    /// See `rm_fence` and `docs/rm-fence-marker.md`.
    pub rm_fence_handle: u32,
    /// out: the `seq` the KMD put in the `ScanoutFlip`.
    pub out_seq: u64,
}
pub const HELIOS_NVRM_SCANOUT_PRESENT_BYTES: usize = 64;

/// `STATUS`. 64 bytes. Which of the caller's presented images the host is done with.
///
/// Only on a device that acked the host's `NVGPU_F_SCANOUT_RELEASE` (and has its
/// display half on): [`HELIOS_NVRM_CAP_SCANOUT_RELEASE`] is set in
/// `QUERY_CAPS.supported_ops` and so are op bit 12 and event kind
/// `HELIOS_NVRM_EVENT_SCANOUT_RELEASED`. Elsewhere the op answers `UNSUPPORTED` and a
/// client keeps the old rule of `rm-fence-marker.md` (with N >= 3 images, do not write
/// the image of present P before present P+1 has returned and its fence fired).
///
/// `handle` must be a DRM-node handle of the caller (`NOT_OWNED` / `FORBIDDEN` as for
/// `SET`); no live source is needed, so a source that lapsed can still be drained.
/// Answers `OK`; a nonzero `flags` is `BAD_RANGE`.
///
/// # What the numbers mean
///
/// Every `PRESENT` returns the `seq` of its flip (`out_seq`), and a flip moves
/// *queued* (fenced, waiting; or being sent) -> *on the host* -> *done*. A flip is done
/// when ANY of these holds:
///
/// * the host released the buffer: it was replaced by a flip of another buffer (or the
///   scanout was disabled) and every display client it was sent is finished reading it
///   (or the host overruled a slow client after 500 ms);
/// * the same buffer was flipped again (the later `seq` now carries the buffer: use IT);
/// * the flip never reached the host: skipped for a newer ready frame, dropped when the
///   source ended, or refused by the host;
/// * its release is overdue by 2 s after replacement (a buffer the guest closed gets no
///   event; this keeps a stuck entry from holding the numbers back).
///
/// `out_released_seq` is the highest `S` such that EVERY flip of this handle with
/// `seq <= S` is done, `0` if none. `out_last_seq` is the highest `seq` the KMD still
/// remembers for the handle (the newest present). Client rule: an image whose latest
/// present returned `seq == P` may be written again once `out_released_seq >= P`, which
/// is never true while it is the image on screen (the buffer on the scanout is not
/// released). A slow buffer delays every image presented after it (`released_seq` is
/// contiguous): sound, and bounded by the host's 500 ms. A handle the KMD has no flips of
/// answers the highest seq the KMD ever minted in BOTH fields (nothing of yours is
/// outstanding).
///
/// # Waiting without polling
///
/// `EVENT_REGISTER` an event with kind `HELIOS_NVRM_EVENT_SCANOUT_RELEASED` (handle 0)
/// once. It is signalled whenever `out_released_seq` may have advanced (a release
/// matched one of this process's flips; a queued flip was found skipped or dropped).
/// Lose-no-wakeup order: reset the event (manual-reset) or rely on the auto-reset
/// state, call `STATUS`, and only if `out_released_seq < P` wait on the event (with a
/// timeout; the transport's loss also signals it, `epoch` in the reply header tells), then
/// call `STATUS` again. A spurious wake costs one more `STATUS`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmScanoutStatus {
    pub head: HeliosNvrmHeader,
    /// in: the handle given to `SET` (a DRM-node handle of the caller).
    pub handle: u32,
    /// in: zero.
    pub flags: u32,
    /// out: every flip of `handle` with `seq <=` this is done; 0 = none.
    pub out_released_seq: u64,
    /// out: the newest `seq` the KMD remembers for `handle`.
    pub out_last_seq: u64,
}
pub const HELIOS_NVRM_SCANOUT_STATUS_BYTES: usize = 64;

/// `RELEASE`. 48 bytes. Idempotent: nothing to release is `OK`. Somebody else's
/// source is `NOT_OWNED`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct HeliosNvrmScanoutRelease {
    pub head: HeliosNvrmHeader,
    /// in: the handle given to `SET`, or 0 for "whatever I hold".
    pub handle: u32,
    /// in: zero.
    pub flags: u32,
}
pub const HELIOS_NVRM_SCANOUT_RELEASE_BYTES: usize = 48;

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<HeliosNvrmScanoutSet>() == HELIOS_NVRM_SCANOUT_SET_BYTES);
    assert!(offset_of!(HeliosNvrmScanoutSet, handle) == 40);
    assert!(offset_of!(HeliosNvrmScanoutSet, flags) == 44);
    assert!(offset_of!(HeliosNvrmScanoutSet, width) == 48);
    assert!(offset_of!(HeliosNvrmScanoutSet, height) == 52);
    assert!(offset_of!(HeliosNvrmScanoutSet, stride) == 56);
    assert!(offset_of!(HeliosNvrmScanoutSet, offset) == 60);
    assert!(offset_of!(HeliosNvrmScanoutSet, fourcc) == 64);
    assert!(offset_of!(HeliosNvrmScanoutSet, lapse_ms) == 68);
    assert!(offset_of!(HeliosNvrmScanoutSet, modifier) == 72);
    assert!(offset_of!(HeliosNvrmScanoutSet, out_generation) == 80);
    assert!(offset_of!(HeliosNvrmScanoutSet, reserved) == 84);

    assert!(size_of::<HeliosNvrmScanoutPresent>() == HELIOS_NVRM_SCANOUT_PRESENT_BYTES);
    assert!(offset_of!(HeliosNvrmScanoutPresent, handle) == 40);
    assert!(offset_of!(HeliosNvrmScanoutPresent, gem) == 44);
    assert!(offset_of!(HeliosNvrmScanoutPresent, flags) == 48);
    assert!(offset_of!(HeliosNvrmScanoutPresent, rm_fence_handle) == 52);
    assert!(offset_of!(HeliosNvrmScanoutPresent, out_seq) == 56);

    assert!(size_of::<HeliosNvrmScanoutStatus>() == HELIOS_NVRM_SCANOUT_STATUS_BYTES);
    assert!(offset_of!(HeliosNvrmScanoutStatus, handle) == 40);
    assert!(offset_of!(HeliosNvrmScanoutStatus, flags) == 44);
    assert!(offset_of!(HeliosNvrmScanoutStatus, out_released_seq) == 48);
    assert!(offset_of!(HeliosNvrmScanoutStatus, out_last_seq) == 56);
    assert!(HELIOS_NVRM_SCANOUT_STATUS_OPS == 0x1000);
    // The capability bit is above every op number and apart from the fence caps.
    assert!(HELIOS_NVRM_CAP_SCANOUT_RELEASE >> 34 == 1);

    assert!(size_of::<HeliosNvrmScanoutRelease>() == HELIOS_NVRM_SCANOUT_RELEASE_BYTES);
    assert!(offset_of!(HeliosNvrmScanoutRelease, handle) == 40);
    assert!(offset_of!(HeliosNvrmScanoutRelease, flags) == 44);

    assert!(HELIOS_NVRM_SCANOUT_OPS == 0xE00);
};
