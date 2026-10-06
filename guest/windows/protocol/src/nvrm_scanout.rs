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
//! The ops are advertised in `HeliosNvrmQueryCaps.supported_ops` (bits 9, 10, 11).
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
/// The bits these ops occupy in `QueryCaps.supported_ops`.
pub const HELIOS_NVRM_SCANOUT_OPS: u64 = (1 << HELIOS_NVRM_OP_SCANOUT_SET)
    | (1 << HELIOS_NVRM_OP_SCANOUT_PRESENT)
    | (1 << HELIOS_NVRM_OP_SCANOUT_RELEASE);

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
    /// in: zero.
    pub flags: u32,
    /// in: zero.
    pub reserved: u32,
    /// out: the `seq` the KMD put in the `ScanoutFlip`.
    pub out_seq: u64,
}
pub const HELIOS_NVRM_SCANOUT_PRESENT_BYTES: usize = 64;

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
    assert!(offset_of!(HeliosNvrmScanoutPresent, reserved) == 52);
    assert!(offset_of!(HeliosNvrmScanoutPresent, out_seq) == 56);

    assert!(size_of::<HeliosNvrmScanoutRelease>() == HELIOS_NVRM_SCANOUT_RELEASE_BYTES);
    assert!(offset_of!(HeliosNvrmScanoutRelease, handle) == 40);
    assert!(offset_of!(HeliosNvrmScanoutRelease, flags) == 44);

    assert!(HELIOS_NVRM_SCANOUT_OPS == 0xE00);
};
