//! Immediate (tearing) flips: no host-send pacing and no wait for the vsync tick (`ImmFlip`,
//! default on). The I/O half is `kmd_render/src/ddi/immediate_flip.rs`.
//!
//! WHAT AN IMMEDIATE FLIP IS HERE. dxgkrnl issues a present with sync interval 0 (and
//! `DXGI_PRESENT_ALLOW_TEARING` under independent flip) as a DMA-BUFFER flip (`pDmaBuffer` set,
//! `FlipInterval` 0; the census in `query_adapter_info.rs`), and an MMIO flip that must not wait
//! for a retrace with `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.FlipImmediate` (bit 1). On real hardware
//! such a flip changes the scan-out address at once and is complete when the change is made: the
//! application is never held behind a vblank. Two things in this driver held it there anyway:
//!
//! 1. the `ForeignFlip` presenter sends at most one host flip per 3/4 refresh period
//!    (`flip_retire::paced_interval`): a newer frame waits for the next slot (up to 3 ms at
//!    240 Hz) and the ones in between are overwritten;
//! 2. a flip retires toward dxgkrnl only when a `CRTC_VSYNC` carries its address, and the only
//!    producer was the vsync tick (up to a period later). With `MaxQueuedFlipOnVSync` 1 dxgkrnl
//!    does not issue the next flip before that, so an application presenting immediately was
//!    capped at one flip per tick and blocked in `Present`.
//!
//! With the knob on, an immediate flip's programming is sent to the host as soon as the worker
//! takes it ([`min_interval`] 0), and once its address is published the worker reports a
//! `CRTC_VSYNC` carrying it at once ([`completes_now`]), which retires it. A flip with a sync
//! interval (DWM's composition, an application with V-sync on) is unchanged: paced, retired by
//! the tick. Whether the HOST then shows the frame at once (tearing) or at its next vblank is the
//! viewer's choice (`docs/SCANOUT.md`, tearing / direct mode); the guest never waits for either.

/// The service-key knob (REG_DWORD, at most 14 characters). Read at every StartDevice.
pub const KNOB: &str = "ImmFlip";
/// Bit 0: immediate flips are not held by the `ForeignFlip` presenter's minimum interval.
pub const NO_PACE: u32 = 1 << 0;
/// Bit 1: an immediate flip is retired by a `CRTC_VSYNC` reported as soon as its address is
/// published, not by the next tick.
pub const COMPLETE_NOW: u32 = 1 << 1;
/// The knob when absent: both.
pub const DEFAULT_KNOB: u32 = NO_PACE | COMPLETE_NOW;
/// The bits this version knows; others are ignored.
pub const KNOWN: u32 = NO_PACE | COMPLETE_NOW;

/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.FlipImmediate` (bit 1 of `Value`; WDK `d3dkmddi.h`).
pub const SVSA_FLIP_IMMEDIATE: u32 = 1 << 1;

/// `D3DDDI_FLIPINTERVAL_IMMEDIATE` (`DXGKARG_PRESENT.FlipInterval`).
pub const INTERVAL_IMMEDIATE: u32 = 0;
/// `D3DDDI_FLIPINTERVAL_IMMEDIATE_ALLOW_TEARING` (WDDM 2.1+, `d3dukmdt.h`).
pub const INTERVAL_IMMEDIATE_ALLOW_TEARING: u32 = 5;

/// Whether a Present's `FlipInterval` asks for a flip that does not wait for a retrace.
pub const fn interval_is_immediate(flip_interval: u32) -> bool {
    flip_interval == INTERVAL_IMMEDIATE || flip_interval == INTERVAL_IMMEDIATE_ALLOW_TEARING
}

/// The flags word a flip records on its allocation (`set_vidpn_primary_address`): the MMIO
/// flip's own `SetVidPnSourceAddress` flags, or for a DMA-buffer flip [`SVSA_FLIP_IMMEDIATE`]
/// when its Present's interval was immediate.
pub const fn dma_flip_flags(immediate: bool) -> u32 {
    if immediate {
        SVSA_FLIP_IMMEDIATE
    } else {
        0
    }
}

/// Counter names (registry mirrors, at most 14 characters).
pub const COUNTERS: [&str; 6] = [
    "ImmKnob", "ImmDma", "ImmMmio", "ImmProg", "ImmVs", "ImmVsNo",
];

/// The knob in force, unknown bits dropped.
pub const fn mode(knob: u32) -> u32 {
    knob & KNOWN
}

/// Whether an MMIO flip (`SetVidPnSourceAddress`) with this flags word is immediate.
pub const fn mmio_is_immediate(flags: u32) -> bool {
    flags & SVSA_FLIP_IMMEDIATE != 0
}

/// The least time between two host flips for the next owed frame: the presenter's paced interval,
/// or 0 for an immediate flip with [`NO_PACE`].
pub const fn min_interval(paced_100ns: u64, immediate: bool, mode: u32) -> u64 {
    if immediate && mode & NO_PACE != 0 {
        0
    } else {
        paced_100ns
    }
}

/// Why an immediate flip's completion vsync was not reported at publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotNow {
    /// The flip is not immediate, or [`COMPLETE_NOW`] is off: the tick retires it.
    NotAsked,
    /// The vsync chain is not running or dxgkrnl closed the delivery gate: nothing would take it.
    NotLive,
    /// `FlipDoneHost` holds addresses until the host confirms them: its rules decide.
    HostHolds,
    /// Nothing was published (the programming failed and kept nothing).
    NoAddress,
}

/// Whether the worker reports a `CRTC_VSYNC` for the address it just published.
pub const fn completes_now(
    mode: u32,
    immediate: bool,
    vsync_live: bool,
    host_holds: bool,
    published: u64,
) -> Result<(), NotNow> {
    if !immediate || mode & COMPLETE_NOW == 0 {
        return Err(NotNow::NotAsked);
    }
    if !vsync_live {
        return Err(NotNow::NotLive);
    }
    if host_holds {
        return Err(NotNow::HostHolds);
    }
    if published == 0 {
        return Err(NotNow::NoAddress);
    }
    Ok(())
}

/// Whether the worker's programming of an allocation is immediate: the flags its flip recorded
/// on it (per allocation, so a coalesced or re-armed slot keeps the right answer), under the
/// knob.
pub const fn programming_is_immediate(mode: u32, recorded_flags: u32) -> bool {
    mode != 0 && mmio_is_immediate(recorded_flags)
}

/// Whether the completion vsync is worth sending: the programming published a new address.
/// (A programming that was deferred, superseded or refused left the previous address, and a
/// vsync carrying it would retire nothing new.)
pub const fn published_new(before: u64, after: u64) -> bool {
    after != 0 && after != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_both_and_zero_is_the_old_behaviour() {
        assert_eq!(mode(DEFAULT_KNOB), NO_PACE | COMPLETE_NOW);
        assert_eq!(min_interval(31_250, true, mode(0)), 31_250);
        assert_eq!(
            completes_now(mode(0), true, true, false, 0x1000),
            Err(NotNow::NotAsked)
        );
        assert_eq!(mode(0xFFFF_FFFF), KNOWN);
    }

    #[test]
    fn only_immediate_flips_skip_the_pacing() {
        let m = mode(DEFAULT_KNOB);
        assert_eq!(min_interval(31_250, true, m), 0);
        assert_eq!(
            min_interval(31_250, false, m),
            31_250,
            "DWM's interval-1 flips stay paced"
        );
        assert_eq!(
            min_interval(31_250, true, COMPLETE_NOW),
            31_250,
            "bit 0 off"
        );
    }

    #[test]
    fn completion_now_needs_an_immediate_flip_a_live_chain_and_an_address() {
        let m = mode(DEFAULT_KNOB);
        assert_eq!(completes_now(m, true, true, false, 0x1000), Ok(()));
        assert_eq!(
            completes_now(m, false, true, false, 0x1000),
            Err(NotNow::NotAsked)
        );
        assert_eq!(
            completes_now(NO_PACE, true, true, false, 0x1000),
            Err(NotNow::NotAsked)
        );
        assert_eq!(
            completes_now(m, true, false, false, 0x1000),
            Err(NotNow::NotLive)
        );
        assert_eq!(
            completes_now(m, true, true, true, 0x1000),
            Err(NotNow::HostHolds)
        );
        assert_eq!(
            completes_now(m, true, true, false, 0),
            Err(NotNow::NoAddress)
        );
    }

    #[test]
    fn the_mmio_flag_is_bit_one() {
        assert!(mmio_is_immediate(0x2));
        assert!(!mmio_is_immediate(0x1), "ModeChange");
        assert!(!mmio_is_immediate(0x4), "FlipOnNextVSync");
        assert!(mmio_is_immediate(0x6));
    }

    #[test]
    fn intervals_zero_and_allow_tearing_are_immediate() {
        assert!(interval_is_immediate(0));
        assert!(interval_is_immediate(5));
        for i in 1..=4 {
            assert!(!interval_is_immediate(i), "interval {i} waits for retraces");
        }
        assert!(mmio_is_immediate(dma_flip_flags(true)));
        assert_eq!(dma_flip_flags(false), 0);
    }

    #[test]
    fn the_programming_follows_the_allocations_record_and_the_knob() {
        assert!(programming_is_immediate(DEFAULT_KNOB, SVSA_FLIP_IMMEDIATE));
        assert!(!programming_is_immediate(DEFAULT_KNOB, 0));
        assert!(!programming_is_immediate(0, SVSA_FLIP_IMMEDIATE));
    }

    #[test]
    fn a_completion_vsync_needs_a_new_address() {
        assert!(published_new(0x1000, 0x2000));
        assert!(!published_new(0x1000, 0x1000), "deferred or refused");
        assert!(!published_new(0x1000, 0), "nothing published");
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        assert!(KNOB.len() <= 14);
        for (i, a) in COUNTERS.iter().enumerate() {
            assert!(a.len() <= 14, "{a}");
            for b in &COUNTERS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
