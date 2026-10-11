//! Scaled direct flip (`ScaledFlip`, default 0): the pure half of the single-plane multi-plane
//! overlay surface that lets dxgkrnl flip a full-screen swap chain smaller than the mode
//! straight to scan-out (`docs/independent-flip.md` section 14).
//!
//! Windows scales a full-screen swap chain whose size differs from the mode in one of two ways:
//! DWM composes it (a stretch into DWM's own chain every frame), or the display hardware does,
//! through the multi-plane overlay (MPO) surface: plane 0 carries the application's buffer with
//! a source rectangle (the buffer) and a destination rectangle (the output), or a post-composition
//! ("panel fitter") transform scales the composed result. `IDXGIOutput6::
//! CheckHardwareCompositionSupport` reports the second way as
//! `DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN`. dxgkrnl offers it only to an adapter that
//! reports `DXGK_DRIVERCAPS.SupportMultiPlaneOverlay` (otherwise it "will not call multiplane
//! overlay functions") and answers the MPO DDIs.
//!
//! The KMD has no overlay hardware. What it has is a host that shows any frame no larger than the
//! mode scaled to the output with its aspect ratio kept (black bars; `ForeignFlip` hands the host
//! the buffer at its own size). That is exactly one plane that can be stretched as a whole. So
//! the surface is: one plane (`MaxOverlayPlanes` = 1), the whole buffer as the source, and the
//! destination either the whole output or the aspect-correct rectangle centred in it. [`check`]
//! is that rule for `DxgkDdiCheckMultiPlaneOverlaySupport3`; every other configuration is refused,
//! and dxgkrnl then composes as it does today.
//!
//! What the screen shows for an accepted configuration is always the aspect-correct fit (the
//! host's rule). A destination that asks for a full stretch is accepted too and shown with black
//! bars: no game is worse off than composed-and-stretched in a way it could notice, and a
//! stretch-only acceptance would refuse the common case.
//!
//! The second half is completion. A flip that arrives through
//! `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3` is retired by
//! `DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2`, which names the last completed
//! `PresentId` per plane, not by the scanned-out address `DXGK_INTERRUPT_CRTC_VSYNC` carries. The
//! rest of the KMD completes flips by publishing the flip's address and reporting it at the next
//! vsync. [`PresentBook`] maps one to the other: the DDI records `(address, PresentId)`, and the
//! vsync looks up the newest `PresentId` recorded for the address it reports.
//!
//! Everything is a function of its arguments, apart from [`PresentBook`]'s atomics.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::ScanoutFormat;

/// Service-key knob (REG_DWORD). Read once, in `DriverEntry` (the MPO DDIs are registered there)
/// and used for the caps, so a change needs a reboot.
pub const KNOB: &str = "ScaledFlip";
/// `ScaledFlip` when the value is absent: off.
pub const KNOB_DEFAULT: u32 = 0;

/// What the knob asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0, absent or any value but 1: no MPO caps, no MPO DDIs. The driver as before.
    Off,
    /// 1: one plane with whole-buffer stretch is advertised, the MPO3 DDIs are registered.
    On,
}

impl Mode {
    /// Only 1 turns it on: a typo never changes how every flip of the desktop is delivered.
    pub const fn from_knob(v: u32) -> Self {
        if v == 1 {
            Self::On
        } else {
            Self::Off
        }
    }

    pub const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// The planes the adapter reports (`DXGK_DRIVERCAPS.MaxOverlayPlanes`, "including the primary
/// surface"): the primary alone.
pub const MAX_PLANES: u32 = 1;
/// The largest stretch per axis the KMD accepts (the destination at most this many times the
/// source). The host scales any ratio; the bound keeps a degenerate source (a 64x64 buffer on a
/// 5120x1440 output) composed. The UMD reports it as `MaxStretchFactor`.
pub const MAX_STRETCH: u32 = 8;
/// No shrink: a buffer larger than the mode is refused before any rectangle is looked at (the
/// flip arms refuse it too). The UMD reports 1.0 as `MaxShrinkFactor`.
pub const MAX_SHRINK: u32 = 1;
/// How far, in pixels per edge, a destination may be from the aspect-correct rectangle and still
/// be taken for it (DWM and the runtime round the centred rectangle their own way).
pub const ASPECT_SLACK: i32 = 2;

/// `D3DDDI_ROTATION_IDENTITY`. 0 (unset) is taken as identity too.
pub const ROTATION_IDENTITY: u32 = 1;
/// `D3DDDI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`: the only colour space the host shows unchanged.
pub const COLOR_SPACE_SRGB: u32 = 0;
/// `DXGK_MULTIPLANE_OVERLAY_FLAGS`: `VerticalFlip | HorizontalFlip`. `StaticCheck` (bit 2, WDDM
/// 3.0) is never set for a 2.1 adapter and is refused as an unknown bit.
pub const PLANE_FLAGS_MIRROR: u32 = 0x3;
/// `DXGK_MULTIPLANE_OVERLAY_BLEND.AlphaBlend`.
pub const BLEND_ALPHA: u32 = 0x1;

/// A `RECT`: left/top inclusive, right/bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// `(0, 0, w, h)`.
    pub const fn sized(w: u32, h: u32) -> Self {
        Self::new(0, 0, w as i32, h as i32)
    }

    pub const fn width(self) -> i64 {
        self.right as i64 - self.left as i64
    }

    pub const fn height(self) -> i64 {
        self.bottom as i64 - self.top as i64
    }

    const fn is_empty(self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    /// Whether `inner` lies inside `self`.
    const fn contains(self, inner: Rect) -> bool {
        inner.left >= self.left
            && inner.top >= self.top
            && inner.right <= self.right
            && inner.bottom <= self.bottom
    }

    /// Every edge within `slack` pixels of `other`'s.
    const fn near(self, other: Rect, slack: i32) -> bool {
        (self.left as i64 - other.left as i64).abs() <= slack as i64
            && (self.top as i64 - other.top as i64).abs() <= slack as i64
            && (self.right as i64 - other.right as i64).abs() <= slack as i64
            && (self.bottom as i64 - other.bottom as i64).abs() <= slack as i64
    }
}

/// The rectangle a `src_w` x `src_h` picture fills when it is scaled to a `mode_w` x `mode_h`
/// output with its aspect ratio kept and centred: what the host shows.
pub const fn aspect_fit(src_w: u32, src_h: u32, mode_w: u32, mode_h: u32) -> Rect {
    if src_w == 0 || src_h == 0 || mode_w == 0 || mode_h == 0 {
        return Rect::sized(mode_w, mode_h);
    }
    let (sw, sh, mw, mh) = (src_w as u64, src_h as u64, mode_w as u64, mode_h as u64);
    if sw * mh >= sh * mw {
        // Wider than the output (or equal): full width, bars above and below.
        let h = (sh * mw + sw / 2) / sw;
        let top = (mh - h) / 2;
        Rect::new(0, top as i32, mode_w as i32, (top + h) as i32)
    } else {
        // Narrower: full height, bars left and right.
        let w = (sw * mh + sh / 2) / sh;
        let left = (mw - w) / 2;
        Rect::new(left as i32, 0, (left + w) as i32, mode_h as i32)
    }
}

/// One plane of a `DxgkDdiCheckMultiPlaneOverlaySupport3` query or a
/// `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3` flip (`DXGK_MULTIPLANE_OVERLAY_ATTRIBUTES3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub layer: u32,
    pub src: Rect,
    pub dst: Rect,
    pub clip: Rect,
    /// `DXGK_MULTIPLANE_OVERLAY_FLAGS.Value`.
    pub flags: u32,
    /// `D3DDDI_ROTATION`.
    pub rotation: u32,
    /// `DXGK_MULTIPLANE_OVERLAY_BLEND.Value`.
    pub blend: u32,
    /// `D3DDDI_COLOR_SPACE_TYPE`.
    pub color_space: u32,
}

/// `DXGK_MULTIPLANE_OVERLAY_POST_COMPOSITION`: the transform applied after the planes were
/// composed (the virtual mode `src` shown at `dst` of the physical mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Post {
    pub src: Rect,
    pub dst: Rect,
    /// `DXGK_MULTIPLANE_OVERLAY_POST_COMPOSITION_FLAGS.Value` (vertical / horizontal flip).
    pub flags: u32,
    pub rotation: u32,
}

/// What the KMD knows about a plane's allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub width: u32,
    pub height: u32,
    pub dxgi_format: u32,
    /// An adopted NVK-on-RM allocation: `ForeignFlip` is the only arm that shows a buffer smaller
    /// than the mode.
    pub foreign: bool,
}

/// Why a configuration is refused. The codes are the `SflWhy` counter's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// More than one plane, or none and no post-composition transform.
    PlaneCount = 1,
    /// A plane other than layer 0.
    Layer = 2,
    /// The allocation is not one of this adapter's, or has no extent.
    NoSource = 3,
    /// Not one of the three 32-bit scan-out formats (YUV, 10-bit and fp16 stay composed).
    Format = 4,
    /// The source rectangle is not the whole buffer.
    SrcRect = 5,
    /// A rotation.
    Rotation = 6,
    /// A mirror flag, or an unknown plane flag.
    Mirror = 7,
    /// Alpha blending (the host shows the plane opaque).
    Blend = 8,
    /// A colour space other than sRGB.
    ColorSpace = 9,
    /// A clip rectangle that cuts into the destination.
    Clip = 10,
    /// The destination is neither the whole output nor the aspect-correct rectangle.
    DstRect = 11,
    /// A post-composition transform this adapter cannot do.
    PostRect = 12,
    /// The buffer is larger than the mode.
    TooLarge = 13,
    /// A buffer smaller than the mode that `ForeignFlip` would not show (not an NVK buffer).
    NotForeign = 14,
    /// More than [`MAX_STRETCH`] per axis.
    Stretch = 15,
}

/// An accepted configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    /// The buffer is not the mode's size: the host scales it.
    pub scaled: bool,
}

/// Whether `dst` is a destination the host can show `src_w` x `src_h` at, on a `mode` output:
/// the whole output, or the aspect-correct rectangle.
const fn dst_ok(dst: Rect, src_w: u32, src_h: u32, mode: (u32, u32)) -> bool {
    let full = Rect::sized(mode.0, mode.1);
    if dst.left == full.left
        && dst.top == full.top
        && dst.right == full.right
        && dst.bottom == full.bottom
    {
        return true;
    }
    full.contains(dst) && dst.near(aspect_fit(src_w, src_h, mode.0, mode.1), ASPECT_SLACK)
}

const fn rotation_ok(r: u32) -> bool {
    r == 0 || r == ROTATION_IDENTITY
}

/// The rule of `DxgkDdiCheckMultiPlaneOverlaySupport3` for one VidPn source showing a `mode`
/// output. `planes` pairs each plane with its allocation's facts (`None`: unresolvable).
pub fn check(
    mode: (u32, u32),
    planes: &[(Plane, Option<Source>)],
    post: Option<&Post>,
) -> Result<Fit, Why> {
    if let Some(p) = post {
        if p.flags != 0 || !rotation_ok(p.rotation) {
            return Err(Why::PostRect);
        }
        // The virtual mode sits at the origin and is no larger than the physical one.
        if p.src.left != 0
            || p.src.top != 0
            || p.src.is_empty()
            || p.src.width() > mode.0 as i64
            || p.src.height() > mode.1 as i64
        {
            return Err(Why::PostRect);
        }
        if !dst_ok(p.dst, p.src.width() as u32, p.src.height() as u32, mode) {
            return Err(Why::PostRect);
        }
    }
    let (plane, source) = match (planes, post) {
        // A post-composition query on its own (the panel fitter for a virtual mode): checked above.
        ([], Some(p)) => {
            return Ok(Fit {
                scaled: p.src != Rect::sized(mode.0, mode.1),
            });
        }
        ([one], _) => one,
        _ => return Err(Why::PlaneCount),
    };
    if plane.layer != 0 {
        return Err(Why::Layer);
    }
    let Some(s) = source else {
        return Err(Why::NoSource);
    };
    if s.width == 0 || s.height == 0 {
        return Err(Why::NoSource);
    }
    if ScanoutFormat::from_dxgi(s.dxgi_format).is_none() {
        return Err(Why::Format);
    }
    if !rotation_ok(plane.rotation) {
        return Err(Why::Rotation);
    }
    if plane.flags != 0 {
        return Err(Why::Mirror);
    }
    if plane.blend & BLEND_ALPHA != 0 {
        return Err(Why::Blend);
    }
    if plane.color_space != COLOR_SPACE_SRGB {
        return Err(Why::ColorSpace);
    }
    if s.width > mode.0 || s.height > mode.1 {
        return Err(Why::TooLarge);
    }
    if plane.src != Rect::sized(s.width, s.height) {
        return Err(Why::SrcRect);
    }
    if !plane.clip.contains(plane.dst) {
        return Err(Why::Clip);
    }
    // Where the buffer ends up on the output.
    let shown = match post {
        // The plane fills the virtual mode, which the transform then places.
        Some(p) => {
            if plane.dst != p.src {
                return Err(Why::PostRect);
            }
            p.dst
        }
        None => plane.dst,
    };
    if !dst_ok(shown, s.width, s.height, mode) {
        return Err(Why::DstRect);
    }
    if shown.width() > s.width as i64 * MAX_STRETCH as i64
        || shown.height() > s.height as i64 * MAX_STRETCH as i64
    {
        return Err(Why::Stretch);
    }
    let scaled = s.width != mode.0 || s.height != mode.1;
    if scaled && !s.foreign {
        return Err(Why::NotForeign);
    }
    Ok(Fit { scaled })
}

/// `DXGK_PLANE_SPECIFIC_INPUT_FLAGS` bits.
pub mod plane_input {
    pub const ENABLED: u32 = 0x1;
    pub const FLIP_IMMEDIATE: u32 = 0x2;
    pub const FLIP_ON_NEXT_VSYNC: u32 = 0x4;
    pub const SHARED_PRIMARY_TRANSITION: u32 = 0x8;
    pub const INDEPENDENT_FLIP_EXCLUSIVE: u32 = 0x10;
}

/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS` bits the rest of the KMD reads from a flip.
pub mod legacy_flags {
    pub const FLIP_IMMEDIATE: u32 = 0x2;
    pub const FLIP_ON_NEXT_VSYNC: u32 = 0x4;
    pub const SHARED_PRIMARY_TRANSITION: u32 = 0x20;
    pub const INDEPENDENT_FLIP_EXCLUSIVE: u32 = 0x40;
}

/// The `DXGK_SETVIDPNSOURCEADDRESS_FLAGS` word an MPO3 plane flip stands for, so the flip takes the
/// legacy flip's path unchanged (immediacy, the independent-flip census, the hand-over marks).
pub const fn legacy_flags_of(plane_input_flags: u32) -> u32 {
    let mut f = 0;
    if plane_input_flags & plane_input::FLIP_IMMEDIATE != 0 {
        f |= legacy_flags::FLIP_IMMEDIATE;
    }
    if plane_input_flags & plane_input::FLIP_ON_NEXT_VSYNC != 0 {
        f |= legacy_flags::FLIP_ON_NEXT_VSYNC;
    }
    if plane_input_flags & plane_input::SHARED_PRIMARY_TRANSITION != 0 {
        f |= legacy_flags::SHARED_PRIMARY_TRANSITION;
    }
    if plane_input_flags & plane_input::INDEPENDENT_FLIP_EXCLUSIVE != 0 {
        f |= legacy_flags::INDEPENDENT_FLIP_EXCLUSIVE;
    }
    f
}

/// A recorded flip that completes at the next vsync whatever address it reports (a disabled
/// plane: nothing new is shown, the `PresentId` still has to retire).
pub const ANY_ADDRESS: u64 = u64::MAX;
/// Flips the book remembers. dxgkrnl queues at most `MaxQueuedFlipOnVSync` (at most 16) ahead
/// of the vsync, and the vsync reports the newest published one, so a handful is plenty; an entry
/// that falls out was overtaken by a newer flip whose report retires it too (`PresentId`s are
/// reported as "the last completed").
pub const BOOK_SLOTS: usize = 16;

/// What one vsync reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsyncReport {
    /// `DXGK_INTERRUPT_CRTC_VSYNC` with the address, as without the knob.
    Legacy,
    /// `DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY2`, layer 0, this `PresentId`.
    Mpo { present_id: u64 },
    /// Both: dxgkrnl went back to the legacy flip DDI while MPO flips were still unreported.
    Both { present_id: u64 },
}

/// `(address, PresentId)` of the newest MPO3 flips, written by the flip DDI and read by the vsync.
///
/// One writer at a time: dxgkrnl issues a VidPn source's flips one after the other (the DDI runs
/// under its interrupt synchronization). Readers are lock-free: a seqlock over the slots, with a
/// bounded number of retries, so a reader at DIRQL never spins on a writer it interrupted on the
/// same processor (it reports the previous `PresentId` again instead).
pub struct PresentBook {
    seq: AtomicU32,
    next: AtomicU32,
    addr: [AtomicU64; BOOK_SLOTS],
    id: [AtomicU64; BOOK_SLOTS],
    /// The newest `PresentId` recorded.
    newest: AtomicU64,
    /// The newest `PresentId` reported (never goes back).
    reported: AtomicU64,
    /// The newest flip came through the MPO3 DDI (cleared by a legacy `SetVidPnSourceAddress`).
    mpo_active: AtomicBool,
}

const READ_TRIES: u32 = 4;

impl PresentBook {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            next: AtomicU32::new(0),
            addr: [const { AtomicU64::new(0) }; BOOK_SLOTS],
            id: [const { AtomicU64::new(0) }; BOOK_SLOTS],
            newest: AtomicU64::new(0),
            reported: AtomicU64::new(0),
            mpo_active: AtomicBool::new(false),
        }
    }

    /// Forget everything (a new device start: `PresentId`s start over with the adapter).
    pub fn reset(&self) {
        self.seq.fetch_add(1, Ordering::AcqRel);
        for i in 0..BOOK_SLOTS {
            self.addr[i].store(0, Ordering::Relaxed);
            self.id[i].store(0, Ordering::Relaxed);
        }
        self.next.store(0, Ordering::Relaxed);
        self.newest.store(0, Ordering::Relaxed);
        self.reported.store(0, Ordering::Relaxed);
        self.mpo_active.store(false, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::AcqRel);
    }

    /// An MPO3 flip of plane 0 to `address` (or [`ANY_ADDRESS`]) with `present_id`.
    pub fn record(&self, address: u64, present_id: u64) {
        self.seq.fetch_add(1, Ordering::AcqRel); // odd: writing
        let slot = self.next.load(Ordering::Relaxed) as usize % BOOK_SLOTS;
        self.addr[slot].store(address, Ordering::Relaxed);
        self.id[slot].store(present_id, Ordering::Relaxed);
        self.next
            .store((slot as u32 + 1) % BOOK_SLOTS as u32, Ordering::Relaxed);
        self.newest.fetch_max(present_id, Ordering::Relaxed);
        self.mpo_active.store(true, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::AcqRel); // even: done
    }

    /// A flip through the legacy `DxgkDdiSetVidPnSourceAddress`: dxgkrnl left MPO mode.
    pub fn legacy_flip(&self) {
        self.mpo_active.store(false, Ordering::Release);
    }

    pub fn mpo_active(&self) -> bool {
        self.mpo_active.load(Ordering::Acquire)
    }

    /// The newest `PresentId` recorded for `address` (or for [`ANY_ADDRESS`]); `None` when there
    /// is none or the slots kept changing under the reader.
    fn lookup(&self, address: u64) -> Option<u64> {
        for _ in 0..READ_TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let mut best: Option<u64> = None;
            for i in 0..BOOK_SLOTS {
                let id = self.id[i].load(Ordering::Relaxed);
                if id == 0 {
                    continue;
                }
                let a = self.addr[i].load(Ordering::Relaxed);
                if a == address || a == ANY_ADDRESS {
                    best = Some(best.map_or(id, |b: u64| b.max(id)));
                }
            }
            if self.seq.load(Ordering::Acquire) == s1 {
                return best;
            }
        }
        None
    }

    /// What the vsync that reports `address` delivers. Updates the reported `PresentId`.
    pub fn on_vsync(&self, address: u64) -> VsyncReport {
        if self.mpo_active() {
            let found = self.lookup(address);
            let prev = match found {
                Some(id) => self.reported.fetch_max(id, Ordering::AcqRel),
                None => self.reported.load(Ordering::Acquire),
            };
            let id = found.map_or(prev, |f| f.max(prev));
            return if id == 0 {
                // No MPO flip was ever retired: nothing to name yet.
                VsyncReport::Legacy
            } else {
                VsyncReport::Mpo { present_id: id }
            };
        }
        let newest = self.newest.load(Ordering::Acquire);
        let prev = self.reported.fetch_max(newest, Ordering::AcqRel);
        if newest > prev {
            VsyncReport::Both { present_id: newest }
        } else {
            VsyncReport::Legacy
        }
    }
}

impl Default for PresentBook {
    fn default() -> Self {
        Self::new()
    }
}

/// The counters (`ddi/scaled_flip.rs` writes exactly these, at most 13 characters).
pub const COUNTERS: [&str; 14] = [
    "SflKnob",
    "SflChk",
    "SflChkOk",
    "SflWhy",
    "SflFlip",
    "SflScaled",
    "SflOff",
    "SflBad",
    "SflVsMpo",
    "SflVsBoth",
    "SflPresent",
    "SflPrDma",
    "SflLayer",
    "SflVsErr",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const MODE: (u32, u32) = (5120, 1440);

    fn plane(src: Rect, dst: Rect) -> Plane {
        Plane {
            layer: 0,
            src,
            dst,
            clip: Rect::sized(MODE.0, MODE.1),
            flags: 0,
            rotation: ROTATION_IDENTITY,
            blend: 0,
            color_space: COLOR_SPACE_SRGB,
        }
    }

    fn nvk(w: u32, h: u32) -> Option<Source> {
        Some(Source {
            width: w,
            height: h,
            dxgi_format: 87,
            foreign: true,
        })
    }

    #[test]
    fn the_aspect_fit_of_a_16_9_buffer_on_a_32_9_output_is_centred() {
        assert_eq!(
            aspect_fit(1600, 900, 5120, 1440),
            Rect::new(1280, 0, 3840, 1440)
        );
        assert_eq!(
            aspect_fit(2560, 1440, 5120, 1440),
            Rect::new(1280, 0, 3840, 1440)
        );
        // Wider than the output: bars above and below.
        assert_eq!(
            aspect_fit(1920, 400, 1920, 1080),
            Rect::new(0, 340, 1920, 740)
        );
        // Equal aspect: the whole output.
        assert_eq!(aspect_fit(1280, 720, 1920, 1080), Rect::sized(1920, 1080));
    }

    #[test]
    fn the_desktop_plane_at_the_mode_is_supported_unscaled() {
        let p = plane(Rect::sized(5120, 1440), Rect::sized(5120, 1440));
        assert_eq!(
            check(MODE, &[(p, nvk(5120, 1440))], None),
            Ok(Fit { scaled: false })
        );
        // A Venus allocation at the mode is fine too (every arm shows the mode's extent).
        let venus = Some(Source {
            foreign: false,
            ..nvk(5120, 1440).unwrap()
        });
        assert_eq!(check(MODE, &[(p, venus)], None), Ok(Fit { scaled: false }));
    }

    #[test]
    fn a_smaller_buffer_stretched_or_aspect_fitted_is_supported_scaled() {
        let src = Rect::sized(1600, 900);
        for dst in [
            Rect::sized(5120, 1440),
            Rect::new(1280, 0, 3840, 1440),
            Rect::new(1281, 0, 3839, 1440),
        ] {
            assert_eq!(
                check(MODE, &[(plane(src, dst), nvk(1600, 900))], None),
                Ok(Fit { scaled: true }),
                "{dst:?}"
            );
        }
    }

    #[test]
    fn a_centred_unscaled_or_partial_destination_stays_composed() {
        let src = Rect::sized(1600, 900);
        // 1:1 in the middle of the output.
        let centred = Rect::new(1760, 270, 3360, 1170);
        assert_eq!(
            check(MODE, &[(plane(src, centred), nvk(1600, 900))], None),
            Err(Why::DstRect)
        );
        // A window-sized overlay somewhere on the desktop.
        let window = Rect::new(100, 100, 1700, 1000);
        assert_eq!(
            check(MODE, &[(plane(src, window), nvk(1600, 900))], None),
            Err(Why::DstRect)
        );
    }

    #[test]
    fn only_the_whole_buffer_may_be_the_source() {
        let p = plane(Rect::new(0, 0, 1600, 800), Rect::sized(5120, 1440));
        assert_eq!(check(MODE, &[(p, nvk(1600, 900))], None), Err(Why::SrcRect));
    }

    #[test]
    fn yuv_wide_and_unknown_formats_are_refused() {
        let p = plane(Rect::sized(1600, 900), Rect::sized(5120, 1440));
        for fmt in [
            103u32, /* NV12 */
            24,     /* R10G10B10A2 */
            10,     /* R16G16B16A16_FLOAT */
            0,
        ] {
            let s = Some(Source {
                dxgi_format: fmt,
                ..nvk(1600, 900).unwrap()
            });
            assert_eq!(check(MODE, &[(p, s)], None), Err(Why::Format), "{fmt}");
        }
    }

    #[test]
    fn transforms_the_host_cannot_do_are_refused() {
        let base = plane(Rect::sized(1600, 900), Rect::sized(5120, 1440));
        let s = nvk(1600, 900);
        assert_eq!(
            check(
                MODE,
                &[(
                    Plane {
                        rotation: 2,
                        ..base
                    },
                    s
                )],
                None
            ),
            Err(Why::Rotation)
        );
        assert_eq!(
            check(MODE, &[(Plane { flags: 1, ..base }, s)], None),
            Err(Why::Mirror)
        );
        assert_eq!(
            check(MODE, &[(Plane { flags: 4, ..base }, s)], None),
            Err(Why::Mirror)
        );
        assert_eq!(
            check(MODE, &[(Plane { blend: 1, ..base }, s)], None),
            Err(Why::Blend)
        );
        assert_eq!(
            check(
                MODE,
                &[(
                    Plane {
                        color_space: 12,
                        ..base
                    },
                    s
                )],
                None
            ),
            Err(Why::ColorSpace)
        );
        let clipped = Plane {
            clip: Rect::new(0, 0, 2560, 1440),
            ..base
        };
        assert_eq!(check(MODE, &[(clipped, s)], None), Err(Why::Clip));
        // Rotation 0 (unset) is identity.
        assert!(check(
            MODE,
            &[(
                Plane {
                    rotation: 0,
                    ..base
                },
                s
            )],
            None
        )
        .is_ok());
    }

    #[test]
    fn more_than_one_plane_or_another_layer_is_refused() {
        let p = plane(Rect::sized(1600, 900), Rect::sized(5120, 1440));
        let s = nvk(1600, 900);
        assert_eq!(
            check(MODE, &[(p, s), (Plane { layer: 1, ..p }, s)], None),
            Err(Why::PlaneCount)
        );
        assert_eq!(
            check(MODE, &[(Plane { layer: 1, ..p }, s)], None),
            Err(Why::Layer)
        );
        assert_eq!(check(MODE, &[], None), Err(Why::PlaneCount));
        assert_eq!(check(MODE, &[(p, None)], None), Err(Why::NoSource));
    }

    #[test]
    fn larger_than_the_mode_never() {
        let p = plane(Rect::sized(5200, 1440), Rect::sized(5120, 1440));
        assert_eq!(
            check(MODE, &[(p, nvk(5200, 1440))], None),
            Err(Why::TooLarge)
        );
    }

    #[test]
    fn a_smaller_venus_buffer_is_refused_because_no_arm_would_show_it() {
        let p = plane(Rect::sized(1600, 900), Rect::sized(5120, 1440));
        let venus = Some(Source {
            foreign: false,
            ..nvk(1600, 900).unwrap()
        });
        assert_eq!(check(MODE, &[(p, venus)], None), Err(Why::NotForeign));
    }

    #[test]
    fn a_tiny_buffer_is_beyond_the_stretch_bound() {
        let p = plane(Rect::sized(320, 90), Rect::sized(5120, 1440));
        assert_eq!(check(MODE, &[(p, nvk(320, 90))], None), Err(Why::Stretch));
        let p = plane(Rect::sized(640, 180), Rect::sized(5120, 1440));
        assert!(check(MODE, &[(p, nvk(640, 180))], None).is_ok());
    }

    #[test]
    fn the_panel_fitter_places_the_plane_through_the_post_transform() {
        let virt = Rect::sized(1600, 900);
        let post = Post {
            src: virt,
            dst: Rect::new(1280, 0, 3840, 1440),
            flags: 0,
            rotation: 1,
        };
        let p = plane(virt, virt);
        assert_eq!(
            check(MODE, &[(p, nvk(1600, 900))], Some(&post)),
            Ok(Fit { scaled: true })
        );
        // The query of the transform alone.
        assert_eq!(check(MODE, &[], Some(&post)), Ok(Fit { scaled: true }));
        // A plane that does not fill the virtual mode.
        let half = plane(virt, Rect::sized(800, 900));
        assert_eq!(
            check(MODE, &[(half, nvk(1600, 900))], Some(&post)),
            Err(Why::PostRect)
        );
        // A transform that mirrors or is not at the origin.
        let mirrored = Post { flags: 1, ..post };
        assert_eq!(check(MODE, &[], Some(&mirrored)), Err(Why::PostRect));
        let shifted = Post {
            src: Rect::new(10, 0, 1610, 900),
            ..post
        };
        assert_eq!(check(MODE, &[], Some(&shifted)), Err(Why::PostRect));
        let bigger = Post {
            src: Rect::sized(6000, 1440),
            ..post
        };
        assert_eq!(check(MODE, &[], Some(&bigger)), Err(Why::PostRect));
    }

    #[test]
    fn the_plane_flags_map_onto_the_legacy_flip_flags() {
        use plane_input::*;
        assert_eq!(legacy_flags_of(ENABLED), 0);
        assert_eq!(
            legacy_flags_of(ENABLED | FLIP_IMMEDIATE),
            legacy_flags::FLIP_IMMEDIATE
        );
        assert_eq!(
            legacy_flags_of(FLIP_ON_NEXT_VSYNC),
            legacy_flags::FLIP_ON_NEXT_VSYNC
        );
        assert_eq!(
            legacy_flags_of(SHARED_PRIMARY_TRANSITION | INDEPENDENT_FLIP_EXCLUSIVE),
            legacy_flags::SHARED_PRIMARY_TRANSITION | legacy_flags::INDEPENDENT_FLIP_EXCLUSIVE
        );
        // Bit 5 (FlipImmediateNoTearing, WDDM 2.6) does not exist at 2.1 and maps to nothing.
        assert_eq!(legacy_flags_of(0x20), 0);
    }

    #[test]
    fn only_one_turns_the_knob_on() {
        assert_eq!(Mode::from_knob(KNOB_DEFAULT), Mode::Off);
        assert_eq!(Mode::from_knob(1), Mode::On);
        for v in [0u32, 2, 3, 0xFFFF_FFFF] {
            assert_eq!(Mode::from_knob(v), Mode::Off, "{v}");
        }
    }

    #[test]
    fn the_book_reports_the_newest_present_id_of_the_shown_address() {
        let b = PresentBook::new();
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Legacy, "no MPO flip yet");
        b.record(0x1000, 10);
        b.record(0x2000, 11);
        // The vsync still shows the first buffer.
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 10 });
        // The second is shown.
        assert_eq!(b.on_vsync(0x2000), VsyncReport::Mpo { present_id: 11 });
        // A swap chain of two: the first address again, a newer id.
        b.record(0x1000, 12);
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 12 });
    }

    #[test]
    fn the_reported_present_id_never_goes_back() {
        let b = PresentBook::new();
        b.record(0x1000, 10);
        b.record(0x2000, 11);
        assert_eq!(b.on_vsync(0x2000), VsyncReport::Mpo { present_id: 11 });
        // A late vsync carrying the older address re-reports 11, not 10.
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 11 });
        // An address the book does not know repeats the last report.
        assert_eq!(b.on_vsync(0x9999), VsyncReport::Mpo { present_id: 11 });
    }

    #[test]
    fn a_disabled_plane_retires_at_the_next_vsync() {
        let b = PresentBook::new();
        b.record(0x1000, 5);
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 5 });
        b.record(ANY_ADDRESS, 6);
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 6 });
    }

    #[test]
    fn leaving_mpo_mode_flushes_the_unreported_ids_once() {
        let b = PresentBook::new();
        b.record(0x1000, 20);
        b.record(0x2000, 21);
        b.legacy_flip();
        assert!(!b.mpo_active());
        assert_eq!(b.on_vsync(0x3000), VsyncReport::Both { present_id: 21 });
        assert_eq!(b.on_vsync(0x3000), VsyncReport::Legacy);
        // Back in MPO mode.
        b.record(0x1000, 22);
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Mpo { present_id: 22 });
    }

    #[test]
    fn more_flips_than_slots_keep_the_newest() {
        let b = PresentBook::new();
        for i in 0..(BOOK_SLOTS as u64 * 3) {
            b.record(0x1000 + (i % 3) * 0x1000, 100 + i);
        }
        let last = 100 + BOOK_SLOTS as u64 * 3 - 1;
        let last_addr = 0x1000 + ((BOOK_SLOTS as u64 * 3 - 1) % 3) * 0x1000;
        assert_eq!(b.on_vsync(last_addr), VsyncReport::Mpo { present_id: last });
    }

    #[test]
    fn reset_forgets_everything() {
        let b = PresentBook::new();
        b.record(0x1000, 7);
        b.reset();
        assert!(!b.mpo_active());
        assert_eq!(b.on_vsync(0x1000), VsyncReport::Legacy);
    }

    #[test]
    fn concurrent_readers_never_see_an_id_that_was_not_recorded_for_the_address() {
        use std::sync::Arc;
        let b = Arc::new(PresentBook::new());
        let w = {
            let b = b.clone();
            std::thread::spawn(move || {
                // Even ids at 0x1000, odd ids at 0x2000.
                for id in 1..20_000u64 {
                    b.record(if id % 2 == 0 { 0x1000 } else { 0x2000 }, id);
                }
            })
        };
        let mut seen: Vec<u64> = Vec::new();
        for _ in 0..20_000 {
            if let Some(id) = b.lookup(0x1000) {
                assert_eq!(id % 2, 0, "id {id} is not one of 0x1000's");
                seen.push(id);
            }
        }
        w.join().unwrap();
        assert!(b.lookup(0x1000).is_some());
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut v: Vec<&str> = COUNTERS.to_vec();
        for n in &v {
            assert!(n.len() <= 13 && n.starts_with("Sfl"), "{n}");
        }
        v.sort();
        v.dedup();
        assert_eq!(v.len(), COUNTERS.len());
        assert!(KNOB.len() <= 14);
    }

    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
            render.display()
        );
        None
    }

    #[test]
    fn the_driver_writes_exactly_these_counters_in_its_one_file() {
        let Some(root) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(root.join("ddi/scaled_flip.rs")).unwrap();
        let mut written: Vec<std::string::String> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("b\"Sfl") {
            let tail = &rest[i + 2..];
            let end = tail.find('"').unwrap();
            let name = &tail[..end];
            if !written.iter().any(|w| w == name) {
                written.push(name.into());
            }
            rest = &tail[end..];
        }
        written.sort();
        let mut want: Vec<&str> = COUNTERS.to_vec();
        want.sort();
        assert_eq!(written, want);
    }
}
