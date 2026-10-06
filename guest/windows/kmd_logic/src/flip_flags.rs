//! The `FlipCapsX` mask and the decode of the flip flags the KMD observes but does not
//! act on (the S-0a independent-flip probe, `docs/zero-copy-present.md` section "FlipCapsX
//! and flip flag counters (S-0a)").
//!
//! Every bit value below is the position in the C bitfield order of `d3dkmddi.h` from WDK
//! 10.0.26100.0 (the line numbers cited are that file's), which is what MSVC lays out and what
//! bindgen's `.Value` view of the union holds. The comments in the header next to
//! some of those bitfields are stale (see [`SPA_SHARED_PRIMARY_TRANSITION`]); the field ORDER
//! and the `Reserved` widths are the authority.

/// `DXGK_FLIPCAPS.FlipOnVSyncMmIo` (bit 1; `d3dkmddi.h` `DXGK_FLIPCAPS`, 1974): the one bit the
/// KMD reports today and the one dxgkrnl insists on for adapter load.
pub const FLIPCAPS_FLIP_ON_VSYNC_MMIO: u32 = 1 << 1;
/// `DXGK_FLIPCAPS.FlipIndependent` (bit 4, WDDM 1.3+, :1978): "MMIO flip to redirected surfaces
/// bypassing DWM Present".
pub const FLIPCAPS_FLIP_INDEPENDENT: u32 = 1 << 4;
/// `DXGK_FLIPCAPS.DdiPresentForIFlip` (bit 5, WDDM 2.0+, :1980): "call `DxgkDdiPresent` when an
/// independent-flip Present might be issued".
pub const FLIPCAPS_DDI_PRESENT_FOR_IFLIP: u32 = 1 << 5;
/// `DXGK_FLIPCAPS.FlipImmediateOnHSync` (bit 6, WDDM 2.0+, :1981).
pub const FLIPCAPS_FLIP_IMMEDIATE_ON_HSYNC: u32 = 1 << 6;

/// What the KMD reports with `FlipCapsX` = 0: byte-identical to the reported word before the
/// knob became a mask.
pub const FLIPCAPS_DEFAULT: u32 = FLIPCAPS_FLIP_ON_VSYNC_MMIO;

/// The only `FlipCapsX` bits that are accepted as extras: 4, 5 and 6. Everything else (the legacy
/// bits 0..3 the driver sets itself or deliberately does not, and bits 7+, reserved in the
/// header) is dropped and reported through [`FlipCaps::dropped`], except the default word's own
/// bit, which is already set: writing the full word `0x12` instead of the extras `0x10` is
/// accepted silently and reports the same.
pub const FLIPCAPS_X_ACCEPTED: u32 = FLIPCAPS_FLIP_INDEPENDENT
    | FLIPCAPS_DDI_PRESENT_FOR_IFLIP
    | FLIPCAPS_FLIP_IMMEDIATE_ON_HSYNC;

/// The outcome of applying a raw `FlipCapsX` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlipCaps {
    /// The bits of `FlipCapsX` that were kept (`knob & FLIPCAPS_X_ACCEPTED`): the effective mask,
    /// mirrored as `FlipCapsXEff`.
    pub effective: u32,
    /// The bits of `FlipCapsX` that were refused (`knob & !(FLIPCAPS_X_ACCEPTED |
    /// FLIPCAPS_DEFAULT)`), mirrored as
    /// `FlipCapsXMsk`. Nonzero means the knob asked for something this driver will not report.
    pub dropped: u32,
    /// The word handed to dxgkrnl as `DXGK_DRIVERCAPS.FlipCaps`
    /// (`FLIPCAPS_DEFAULT | effective`), mirrored as `FlipCapsRep`.
    pub reported: u32,
}

/// Apply the raw `FlipCapsX` service value: raw `DXGK_FLIPCAPS` bit values OR'd into the default
/// word, restricted to [`FLIPCAPS_X_ACCEPTED`]. `0` (the default) yields exactly
/// [`FLIPCAPS_DEFAULT`].
pub const fn resolve_flip_caps(knob: u32) -> FlipCaps {
    let effective = knob & FLIPCAPS_X_ACCEPTED;
    FlipCaps {
        effective,
        dropped: knob & !(FLIPCAPS_X_ACCEPTED | FLIPCAPS_DEFAULT),
        reported: FLIPCAPS_DEFAULT | effective,
    }
}

/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.SharedPrimaryTransition`: bit 6.
///
/// The header comment says `0x00000020`, but the field is the SEVENTH in the struct
/// (`ModeChange`, `FlipImmediate`, `FlipOnNextVSync`, `FlipStereo`, `FlipStereoTemporaryMono`,
/// `FlipStereoPreferRight`, then this), and `FlipStereoTemporaryMono` and `FlipStereoPreferRight`
/// are both annotated `0x10`: the annotations are stale. The `Reserved :23` width at WDDM 2.1+
/// (9 named bits + 23 = 32) confirms the order, so the real values are 0x40, 0x80, 0x100.
pub const SPA_SHARED_PRIMARY_TRANSITION: u32 = 1 << 6;
/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.IndependentFlipExclusive` (WDDM 2.0+): bit 7.
pub const SPA_INDEPENDENT_FLIP_EXCLUSIVE: u32 = 1 << 7;
/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.MoveFlip` (WDDM 2.1+): bit 8.
pub const SPA_MOVE_FLIP: u32 = 1 << 8;
/// `DXGK_PRESENTFLAGS.RedirectedFlip` (WDDM 2.0+): bit 13, `0x2000`. Here the header comment
/// is right (`Blt` is bit 0 and the thirteenth field is this one).
pub const PRESENT_REDIRECTED_FLIP: u32 = 1 << 13;

/// Which of the ignored `SetVidPnSourceAddress` flags one call carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaFlags {
    pub shared_primary_transition: bool,
    pub independent_flip_exclusive: bool,
    pub move_flip: bool,
}

/// Decode a full `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.Value`.
pub const fn decode_spa_flags(value: u32) -> SpaFlags {
    SpaFlags {
        shared_primary_transition: value & SPA_SHARED_PRIMARY_TRANSITION != 0,
        independent_flip_exclusive: value & SPA_INDEPENDENT_FLIP_EXCLUSIVE != 0,
        move_flip: value & SPA_MOVE_FLIP != 0,
    }
}

/// Whether a full `DXGK_PRESENTFLAGS.Value` carries `RedirectedFlip`.
pub const fn present_is_redirected(value: u32) -> bool {
    value & PRESENT_REDIRECTED_FLIP != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_todays_word_byte_identical() {
        let c = resolve_flip_caps(0);
        assert_eq!(c.reported, 0x2);
        assert_eq!(c.effective, 0);
        assert_eq!(c.dropped, 0);
        assert_eq!(FLIPCAPS_DEFAULT, 0x2);
    }

    #[test]
    fn header_bit_values() {
        assert_eq!(FLIPCAPS_FLIP_ON_VSYNC_MMIO, 0x2);
        assert_eq!(FLIPCAPS_FLIP_INDEPENDENT, 0x10);
        assert_eq!(FLIPCAPS_DDI_PRESENT_FOR_IFLIP, 0x20);
        assert_eq!(FLIPCAPS_FLIP_IMMEDIATE_ON_HSYNC, 0x40);
        assert_eq!(FLIPCAPS_X_ACCEPTED, 0x70);
    }

    #[test]
    fn matrix_values_or_into_the_default() {
        assert_eq!(resolve_flip_caps(0x10).reported, 0x12);
        assert_eq!(resolve_flip_caps(0x30).reported, 0x32);
        assert_eq!(resolve_flip_caps(0x70).reported, 0x72);
        assert_eq!(resolve_flip_caps(0x20).reported, 0x22);
        let c = resolve_flip_caps(0x30);
        assert_eq!(c.effective, 0x30);
        assert_eq!(c.dropped, 0);
    }

    #[test]
    fn a_raw_word_that_already_has_the_default_bit_reports_the_same_word() {
        // 0x12 is FlipOnVSyncMmIo | FlipIndependent as a raw DXGK_FLIPCAPS word: the extra bit
        // is kept, the default's own bit is a silent no-op, the reported word is 0x12.
        let c = resolve_flip_caps(0x12);
        assert_eq!(c.effective, 0x10);
        assert_eq!(c.dropped, 0);
        assert_eq!(c.reported, 0x12);
    }

    #[test]
    fn other_bits_are_dropped_and_reported() {
        // Legacy low bits: FlipOnVSyncWithNoWait, FlipInterval and FlipImmediateMmIo are not
        // accepted as knob input; bit 1 is the default's own and is a silent no-op.
        let c = resolve_flip_caps(0x0F);
        assert_eq!(c.effective, 0);
        assert_eq!(c.dropped, 0x0D);
        assert_eq!(c.reported, 0x2);
        // Reserved bits 7+.
        let c = resolve_flip_caps(0x8000_0010);
        assert_eq!(c.effective, 0x10);
        assert_eq!(c.dropped, 0x8000_0000);
        assert_eq!(c.reported, 0x12);
        let c = resolve_flip_caps(u32::MAX);
        assert_eq!(c.effective, 0x70);
        assert_eq!(c.dropped, !0x72);
        assert_eq!(c.reported, 0x72);
    }

    #[test]
    fn effective_and_dropped_partition_the_input() {
        let mut v = 0u32;
        while v < 0x400 {
            let c = resolve_flip_caps(v);
            assert_eq!(c.effective | c.dropped | (v & FLIPCAPS_DEFAULT), v);
            assert_eq!(c.effective & c.dropped, 0);
            assert_eq!(c.reported & FLIPCAPS_DEFAULT, FLIPCAPS_DEFAULT);
            assert_eq!(c.reported & !FLIPCAPS_DEFAULT, c.effective);
            v += 1;
        }
    }

    #[test]
    fn spa_flag_values_follow_the_bitfield_order_not_the_stale_comments() {
        assert_eq!(SPA_SHARED_PRIMARY_TRANSITION, 0x40);
        assert_eq!(SPA_INDEPENDENT_FLIP_EXCLUSIVE, 0x80);
        assert_eq!(SPA_MOVE_FLIP, 0x100);
        // The first six named flags (ModeChange .. FlipStereoPreferRight) occupy bits 0..=5, so a
        // call carrying only those must not look like a transition.
        let d = decode_spa_flags(0x3F);
        assert_eq!(d, SpaFlags::default());
    }

    #[test]
    fn spa_decode() {
        assert_eq!(decode_spa_flags(0), SpaFlags::default());
        assert_eq!(
            decode_spa_flags(0x40),
            SpaFlags { shared_primary_transition: true, ..SpaFlags::default() }
        );
        assert_eq!(
            decode_spa_flags(0x80 | 0x04),
            SpaFlags { independent_flip_exclusive: true, ..SpaFlags::default() }
        );
        assert_eq!(
            decode_spa_flags(0x100),
            SpaFlags { move_flip: true, ..SpaFlags::default() }
        );
        assert_eq!(
            decode_spa_flags(0x1C0),
            SpaFlags {
                shared_primary_transition: true,
                independent_flip_exclusive: true,
                move_flip: true
            }
        );
        // Reserved bits above MoveFlip are not mistaken for it.
        assert_eq!(decode_spa_flags(0xFFFF_FE3F), SpaFlags::default());
    }

    #[test]
    fn present_redirected() {
        assert_eq!(PRESENT_REDIRECTED_FLIP, 0x2000);
        assert!(!present_is_redirected(0));
        // The ordinary DWM flip: Flip | FlipWithNoWait.
        assert!(!present_is_redirected(0x4 | 0x8));
        // FlipWithMultiPlaneOverlay (0x1000) is the neighbouring bit.
        assert!(!present_is_redirected(0x1000));
        assert!(present_is_redirected(0x2000 | 0x4));
        assert!(!present_is_redirected(0xFFFF_DFFF));
    }
}
