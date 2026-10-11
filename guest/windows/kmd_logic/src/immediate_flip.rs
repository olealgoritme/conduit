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
/// Bit 2: report `DXGK_FLIPCAPS.FlipImmediateMmIo`. Without it dxgkrnl turns a promoted
/// application's tearing presents into `FlipOnNextVSync` MMIO flips (measured on 405.49: every
/// `ALLOW_TEARING` interval-0 flip of an independent-flip swap chain carried `FlipOnNextVSync`, none
/// `FlipImmediate`, so the application was held to the refresh). With it the flips arrive with
/// `FlipImmediate` and take bits 0 and 1. The MMIO contract treats such a flip as done when
/// `SetVidPnSourceAddress` returns; this driver programs it a moment later from the worker, so the
/// application may reuse the previous buffer while the host still shows it: the tear
/// `ALLOW_TEARING` asks for. Flips with a sync interval are untouched.
pub const ADVERTISE_MMIO: u32 = 1 << 2;
/// Bit 3: a flip of a PROMOTED swap chain (`SetVidPnSourceAddress` with
/// `IndependentFlipExclusive`) counts as immediate even when dxgkrnl asked for `FlipOnNextVSync`.
/// dxgkrnl issues a promoted application's interval-0 presents that way when they do not carry
/// `ALLOW_TEARING` (CS2 on 405.50: `Hardware: Independent Flip`, sync interval 0, every flip
/// `FlipOnNextVSync`), and retires them only on this driver's synthetic vsync heartbeat, so it
/// coalesced the application's 207 presents per second into about 130 flips at irregular one- and
/// two-vblank spacing. The vblank is ours to define: a promoted flip is shown when the host gets
/// it, so it is retired then and dxgkrnl issues the next one at once. DWM's own composition is
/// never `IndependentFlipExclusive` and keeps the heartbeat. The cost: a promoted application
/// with V-sync ON is no longer held to the refresh by the flip queue (it runs at its render
/// rate; the host shows the newest frame).
pub const IFLIP_NOW: u32 = 1 << 3;
/// The knob when absent: all four.
pub const DEFAULT_KNOB: u32 = NO_PACE | COMPLETE_NOW | ADVERTISE_MMIO | IFLIP_NOW;
/// The bits this version knows; others are ignored.
pub const KNOWN: u32 = NO_PACE | COMPLETE_NOW | ADVERTISE_MMIO | IFLIP_NOW;
/// `DXGK_SETVIDPNSOURCEADDRESS_FLAGS.IndependentFlipExclusive` (bit 7; `flip_flags`).
pub const SVSA_INDEPENDENT_FLIP_EXCLUSIVE: u32 = crate::flip_flags::SPA_INDEPENDENT_FLIP_EXCLUSIVE;
/// An immediate frame still waits this fraction of the paced interval after the previous host
/// flip (the paced interval is 3/4 of a period: a quarter period, about 1 ms at 240 Hz). A host
/// flip per application frame at thousands per second flooded the viewer's Wayland connection
/// (405.50, about 7000 flips per second from an uncapped full-screen application: the viewer
/// lost its display); several flips per refresh are all a viewer can use.
pub const IMMEDIATE_INTERVAL_DIVISOR: u64 = 3;
/// `DXGK_FLIPCAPS.FlipImmediateMmIo` (bit 3).
pub const FLIPCAPS_FLIP_IMMEDIATE_MMIO: u32 = 1 << 3;

/// The reported `DXGK_FLIPCAPS` word given the one built from everything else and the raw knob.
pub const fn flip_caps(reported: u32, knob: u32) -> u32 {
    if knob & ADVERTISE_MMIO != 0 {
        reported | FLIPCAPS_FLIP_IMMEDIATE_MMIO
    } else {
        reported
    }
}

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
        paced_100ns / IMMEDIATE_INTERVAL_DIVISOR
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
    mode != 0
        && (mmio_is_immediate(recorded_flags)
            || (mode & IFLIP_NOW != 0 && recorded_flags & SVSA_INDEPENDENT_FLIP_EXCLUSIVE != 0))
}

/// The address a completion vsync carries after an immediate programming, or `None`: the address
/// published now (`published`) unless the previous completion or tick already carried it
/// (`last_done`). The newest published address, not the programmed flip's own: at very high frame
/// rates dxgkrnl issues the next flip while the worker programs this one, the newest one is
/// published by then, and requiring this flip's address left every such flip to the tick (405.52,
/// an uncapped 488 fps application: 218 completions for 3036 immediate programmings, 240 flips
/// per second). A vsync carrying the newest address retires whatever dxgkrnl queued up to it.
pub const fn completion_address(published: u64, last_done: u64) -> Option<u64> {
    if published != 0 && published != last_done {
        Some(published)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_all_three_and_zero_is_the_old_behaviour() {
        assert_eq!(
            mode(DEFAULT_KNOB),
            NO_PACE | COMPLETE_NOW | ADVERTISE_MMIO | IFLIP_NOW
        );
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
        assert_eq!(
            min_interval(31_250, true, m),
            31_250 / IMMEDIATE_INTERVAL_DIVISOR
        );
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
    fn bit_two_reports_flip_immediate_mmio_and_nothing_else() {
        let base =
            crate::flip_flags::FLIPCAPS_DEFAULT | crate::flip_flags::FLIPCAPS_FLIP_INDEPENDENT;
        assert_eq!(flip_caps(base, DEFAULT_KNOB), base | (1 << 3));
        assert_eq!(flip_caps(base, NO_PACE | COMPLETE_NOW), base);
        assert_eq!(flip_caps(base, 0), base);
        // Bit 3 is the WDK's FlipImmediateMmIo, not one of the FlipCapsX bits.
        assert_eq!(
            FLIPCAPS_FLIP_IMMEDIATE_MMIO & crate::flip_flags::FLIPCAPS_X_ACCEPTED,
            0
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
    fn a_promoted_flip_is_immediate_with_bit_three_whatever_dxgkrnl_asked() {
        let ife_vsync = SVSA_INDEPENDENT_FLIP_EXCLUSIVE | (1 << 2); // IFE | FlipOnNextVSync
        assert!(programming_is_immediate(DEFAULT_KNOB, ife_vsync));
        assert!(!programming_is_immediate(
            NO_PACE | COMPLETE_NOW | ADVERTISE_MMIO,
            ife_vsync
        ));
        assert!(
            !programming_is_immediate(DEFAULT_KNOB, 1 << 2),
            "DWM: FlipOnNextVSync alone"
        );
        assert!(!programming_is_immediate(DEFAULT_KNOB, 0));
    }

    #[test]
    fn immediate_host_sends_are_capped_at_a_fraction_of_the_period() {
        let paced = 31_250; // 3/4 of a 240 Hz period
        let m = mode(DEFAULT_KNOB);
        assert_eq!(min_interval(paced, true, m), 10_416, "about 1 ms");
        assert!(
            min_interval(paced, true, m) > 0,
            "never every application frame"
        );
    }

    #[test]
    fn a_completion_vsync_carries_the_newest_published_address_once() {
        assert_eq!(completion_address(0x2000, 0x1000), Some(0x2000));
        assert_eq!(
            completion_address(0x3000, 0x1000),
            Some(0x3000),
            "a newer flip overtook"
        );
        assert_eq!(completion_address(0x2000, 0x2000), None, "already carried");
        assert_eq!(completion_address(0, 0x1000), None);
    }

    // ---- replay of an uncapped application's present cadence ---------------------------------
    //
    // The worker's immediate path, end to end, as `kmd_render` performs it: dxgkrnl hands the
    // worker one flip at a time (`pending_vidpn_allocation`, one slot), the programming owes the
    // `ForeignFlip` presenter one frame (`OWED`, one slot: a newer programming replaces a frame
    // still owed), the presenter sends it to the host no sooner than `min_interval` after the
    // previous host flip, and the completion vsync carries the published address at once
    // (`completion_address`). The 240 Hz tick delivers the newest published address on its own
    // grid and gates nothing on this path.

    extern crate std;
    use crate::rm_present::{Act, FlipResult, Presenter};
    use std::vec::Vec;

    const MS: u64 = 10_000;
    const HZ240_MHZ: u32 = 240_000;

    #[derive(Debug, Default)]
    struct Replay {
        /// (frame, host send time) in send order.
        sent: Vec<(usize, u64)>,
        /// (frame, the newer frame that replaced it) for every frame never sent.
        superseded: Vec<(usize, usize)>,
        /// Completion vsyncs reported by the worker (not the tick).
        completions: usize,
    }

    fn paced(pipelined: bool) -> u64 {
        crate::flip_retire::paced_interval(
            crate::rm_refresh::flip_interval_100ns(HZ240_MHZ),
            pipelined,
        )
    }

    /// Replays `arrivals` (100 ns, ascending: when each flip reaches the worker) through the
    /// single owed slot and the presenter; `rtt` is the host round trip the worker spends per
    /// flip (the synchronous arm).
    fn replay(arrivals: &[u64], pipelined: bool, rtt: u64) -> Replay {
        let period = crate::vsync_deadline::period_100ns(HZ240_MHZ);
        let interval = min_interval(paced(pipelined), true, mode(DEFAULT_KNOB));
        let mut p = Presenter::new(1);
        p.set_min_interval(interval);
        // Registered before the first frame (the registration is its own act).
        assert_eq!(
            p.decide(crate::rm_sysmem::flip_inputs(
                1, true, true, true, false, false
            )),
            Act::Register
        );
        p.registration(true, 1);
        let addr = |f: usize| 0x1_0000_0000u64 + (f as u64 % 3) * 0x0200_0000;
        let mut out = Replay::default();
        let mut owed: Option<usize> = None;
        let mut wake: Option<u64> = None;
        let mut busy_until = 0u64;
        let mut last_done = 0u64;
        let mut next_tick = period;
        let mut published = 0u64;
        let mut i = 0;
        loop {
            // The next event: an arrival, a timed wake of the worker, or a tick.
            let t_arr = arrivals.get(i).map(|&a| a.max(busy_until));
            let t_wake = wake.map(|w| w.max(busy_until));
            let t = match (t_arr, t_wake) {
                (None, None) => break,
                (Some(a), None) => a,
                (None, Some(w)) => w,
                (Some(a), Some(w)) => a.min(w),
            };
            while next_tick <= t {
                // The tick's CRTC_VSYNC carries the newest published address.
                last_done = published;
                next_tick += period;
            }
            let mut edge = false;
            if t_arr == Some(t) {
                // Programming: published at once, retired by the completion vsync.
                let f = i;
                i += 1;
                published = addr(f);
                if let Some(old) = owed.replace(f) {
                    out.superseded.push((old, f));
                }
                if let Some(a) = completion_address(published, last_done) {
                    last_done = a;
                    out.completions += 1;
                }
                edge = true;
            }
            wake = None;
            let act = p.decide(crate::rm_sysmem::flip_inputs(
                t, true, true, true, edge, false,
            ));
            match act {
                Act::CopyFlip { slot } => {
                    let f = owed.take().expect("a frame is owed");
                    out.sent.push((f, t));
                    p.flipped(slot, true, FlipResult::Shown, t);
                    busy_until = t + rtt;
                }
                Act::WaitUntil(at) => wake = Some(at),
                Act::Idle => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    /// The CS2-like cadence `guest/windows/tools/motiontest.cpp` phase 2 presents: a frame
    /// every 2.37 ms, and once per 15.625 ms one 4.33 ms longer (fast, fast, fast, slow...).
    fn irregular(seconds: u64) -> Vec<u64> {
        let (fast, slow, beat) = (23_700u64, 43_300u64, 156_250u64);
        let mut v = Vec::new();
        let (mut t, mut tick) = (MS, MS);
        while t < seconds * 1000 * MS {
            v.push(t);
            t += fast;
            if t - tick >= beat {
                tick += beat;
                t += slow;
            }
        }
        v
    }

    fn assert_every_frame_sent_in_order(r: &Replay, n: usize) {
        assert!(
            r.superseded.is_empty(),
            "merged: {:?}",
            &r.superseded[..r.superseded.len().min(8)]
        );
        assert_eq!(r.sent.len(), n);
        for (k, (f, _)) in r.sent.iter().enumerate() {
            assert_eq!(*f, k, "out of order");
        }
        assert_eq!(r.completions, n, "every flip completes at programming");
    }

    #[test]
    fn an_irregular_cadence_reaches_the_host_frame_for_frame() {
        let arrivals = irregular(2);
        let fps = arrivals.len() as u64 / 2;
        assert!((280..=330).contains(&fps), "{fps} fps");
        for pipelined in [false, true] {
            for rtt in [0, 3_000, 10_000] {
                let r = replay(&arrivals, pipelined, rtt);
                assert_every_frame_sent_in_order(&r, arrivals.len());
                // Promptly: each one when it arrives (the gaps are wider than the cap and the
                // round trip), never at a tick.
                for (f, at) in &r.sent {
                    assert!(
                        at - arrivals[*f] <= rtt,
                        "frame {f} held {} us",
                        (at - arrivals[*f]) / 10
                    );
                }
                // The host's spacing is the application's, not a multiple of the refresh.
                let gaps: Vec<u64> = r.sent.windows(2).map(|w| w[1].1 - w[0].1).collect();
                let near = |g: u64, c: u64| g.abs_diff(c) < 2_000;
                let locked = gaps
                    .iter()
                    .filter(|&&g| near(g, 83_333) || near(g, 41_667))
                    .count();
                assert!(
                    locked * 10 < gaps.len(),
                    "{locked} of {} gaps on the refresh grid",
                    gaps.len()
                );
            }
        }
    }

    #[test]
    fn an_even_six_point_seven_ms_cadence_is_149_fps_end_to_end() {
        // What motiontest's first copy presented for most of phase 2 (its 15.625 ms clock
        // started at the program start: one long frame after every short one). All 149 per
        // second reach the host; a 240 Hz viewer then shows them at 4.17 / 8.33 ms.
        let arrivals: Vec<u64> = (0..298u64).map(|k| MS + k * 67_000).collect();
        let r = replay(&arrivals, true, 3_000);
        assert_every_frame_sent_in_order(&r, arrivals.len());
    }

    #[test]
    fn even_cadences_up_to_the_cap_are_sent_frame_for_frame() {
        for fps in [240u64, 300, 500, 700] {
            let step = 10_000_000 / fps;
            let arrivals: Vec<u64> = (0..fps).map(|k| MS + k * step).collect();
            let r = replay(&arrivals, true, 0);
            assert_every_frame_sent_in_order(&r, arrivals.len());
        }
    }

    #[test]
    fn only_a_frame_overtaken_within_the_host_send_cap_is_dropped() {
        // Bursts of three flips 0.3 ms apart every 4 ms: the first of each goes at once, the
        // second waits for the cap and is overtaken by the third before then, the third goes
        // at the cap. Never more than the frames overtaken while one was held.
        let cap = min_interval(paced(true), true, mode(DEFAULT_KNOB));
        let mut arrivals = Vec::new();
        for b in 0..100u64 {
            for k in 0..3u64 {
                arrivals.push(MS + b * 40_000 + k * 3_000);
            }
        }
        let r = replay(&arrivals, true, 0);
        assert_eq!(r.sent.len() + r.superseded.len(), arrivals.len());
        for &(old, newer) in &r.superseded {
            assert_eq!(newer, old + 1, "replaced by the very next flip");
            let prev_send = r
                .sent
                .iter()
                .rev()
                .find(|(f, _)| *f < old)
                .map(|s| s.1)
                .unwrap();
            assert!(
                arrivals[newer] < prev_send + cap,
                "{old} was due before {newer} came"
            );
        }
        assert_eq!(r.superseded.len(), 100, "one per burst");
        for w in r.sent.windows(2) {
            assert!(w[0].0 < w[1].0);
            assert!(w[1].1 - w[0].1 >= cap);
        }
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
