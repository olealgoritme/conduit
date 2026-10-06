//! The arithmetic behind the vsync heartbeat's diagnostic values, so a rate is
//! computed one way, by one tested function, instead of by eye from registry
//! mirrors read at unrelated times.
//!
//! The KMD publishes (service key, REG_DWORD, all on the same interrupt-time
//! clock; see `docs/foreign-scanout.md`, "Reading vsync rates"):
//!
//! * a count of heartbeat ticks and, beside it, the time of the tick that
//!   produced it in milliseconds (`ScVs`/`ScVsT`, `VpVsN`/`VpVsT`,
//!   `VsCnt`/`VsCntT`), and the time of the dump itself (`VpDmpT`);
//! * the smallest gap between two ticks (`VsMinGap`, 100 ns units) and the number
//!   of ticks closer than half a period to their predecessor (`VsFast`).
//!
//! Counts and millisecond times are `u32` and wrap (a 240 Hz count after 207 days,
//! a millisecond time after 49.7 days), so every difference here is a wrapping
//! subtraction: one wrap between two samples is handled, more than one is not
//! detectable and is the reader's to avoid by sampling often.

/// 100 ns units per millisecond.
pub const UNITS_PER_MS: u64 = 10_000;

/// Interrupt time in 100 ns units as the published millisecond value. Truncates
/// to `u32` (wraps every 2^32 ms), which is what a wrapping difference wants.
pub const fn ms_from_100ns(time_100ns: u64) -> u32 {
    (time_100ns / UNITS_PER_MS) as u32
}

/// Gap between the previous tick and this one, in 100 ns units. `None` for the
/// first tick after an arm (`previous == 0`, which arm and disarm store), so a
/// tick is never measured against the time before a quiesce. A clock that
/// reads earlier than `previous` yields 0, never a wrapped huge value.
pub const fn tick_gap(previous: u64, now: u64) -> Option<u64> {
    if previous == 0 {
        None
    } else {
        Some(now.saturating_sub(previous))
    }
}

/// True if a tick came closer than half a period to its predecessor: the
/// signature of a burst or of a second timer source. A single late tick followed
/// by the on-phase one also qualifies (the fixed-phase scheme resumes at the
/// original deadline), so a healthy heartbeat can show a few; a burst shows
/// `VsFast` close to the tick count.
pub const fn is_fast(gap_100ns: u64, period_100ns: u64) -> bool {
    gap_100ns < period_100ns / 2
}

/// The published `VsMinGap`: the smallest gap saturated to 32 bits (about 429 s,
/// far above any period). `u64::MAX` (nothing measured) and any gap too large
/// for 32 bits both publish as `u32::MAX`.
pub const fn publish_gap(min_gap_100ns: u64) -> u32 {
    if min_gap_100ns > u32::MAX as u64 {
        u32::MAX
    } else {
        min_gap_100ns as u32
    }
}

/// `later - earlier` of two wrapping `u32` samples (counts or milliseconds).
pub const fn delta32(earlier: u32, later: u32) -> u32 {
    later.wrapping_sub(earlier)
}

/// Rate in millihertz between two samples of the same counter, each with the time
/// it was produced at (milliseconds): `count_a`/`ms_a` the earlier, `count_b`/
/// `ms_b` the later. `None` when no time passed between them (the rate is not
/// defined; this also catches two reads of the same dump). A counter that did
/// not move over real time is `Some(0)`, not `None`.
///
/// 240 Hz is 240_000, 60 Hz is 60_000. The result is `u64` because a counter
/// that went backwards (a reboot between the samples) wraps to a huge delta and
/// must not be clamped silently: see [`plausible`].
pub const fn rate_mhz(count_a: u32, count_b: u32, ms_a: u32, ms_b: u32) -> Option<u64> {
    let dt = delta32(ms_a, ms_b) as u64;
    if dt == 0 {
        return None;
    }
    // delta count <= 2^32, times 1e6 <= 2^52: no overflow.
    Some(delta32(count_a, count_b) as u64 * 1_000_000 / dt)
}

/// How long before a dump the counter last advanced, in milliseconds: dump time
/// minus the time of the last tick. About one period on a running heartbeat;
/// seconds mean it has stalled. Both on the interrupt-time clock.
pub const fn age_ms(dump_ms: u32, last_tick_ms: u32) -> u32 {
    delta32(last_tick_ms, dump_ms)
}

/// True if a measured rate is within `tolerance_permille` (parts per thousand) of
/// the nominal one, both in millihertz. A reboot between two samples, a counter
/// that wrapped twice, or a count and a time taken from different dumps all fail
/// this by orders of magnitude rather than by a few percent.
pub const fn plausible(measured_mhz: u64, nominal_mhz: u32, tolerance_permille: u32) -> bool {
    let nominal = nominal_mhz as u64;
    let slack = nominal * tolerance_permille as u64 / 1_000;
    measured_mhz >= nominal.saturating_sub(slack) && measured_mhz <= nominal + slack
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vsync_deadline::period_100ns;

    #[test]
    fn milliseconds_truncate_and_wrap() {
        assert_eq!(ms_from_100ns(0), 0);
        assert_eq!(ms_from_100ns(9_999), 0);
        assert_eq!(ms_from_100ns(10_000), 1);
        assert_eq!(ms_from_100ns(1_234_567_890), 123_456);
        // 2^32 ms is 49.7 days; one more millisecond wraps to 0, like the registry value.
        assert_eq!(ms_from_100ns((1u64 << 32) * UNITS_PER_MS), 0);
        assert_eq!(ms_from_100ns(((1u64 << 32) + 5) * UNITS_PER_MS), 5);
    }

    #[test]
    fn first_tick_after_arm_has_no_gap() {
        assert_eq!(tick_gap(0, 123_456_789), None);
        assert_eq!(tick_gap(1_000, 1_000 + 41_667), Some(41_667));
        // A clock that reads earlier never produces a huge gap.
        assert_eq!(tick_gap(5_000, 4_000), Some(0));
    }

    #[test]
    fn fast_means_closer_than_half_a_period() {
        let p = period_100ns(240_000); // 41_667
        assert!(!is_fast(p, p));
        assert!(!is_fast(p / 2, p)); // 20_833 is not < 20_833
        assert!(is_fast(p / 2 - 1, p));
        assert!(is_fast(0, p));
        // A late tick (gap > period) is never fast; the on-phase tick after it can be.
        assert!(!is_fast(p * 3, p));
        assert!(is_fast(p / 4, p));
        // 60 Hz, default period.
        assert!(!is_fast(166_667, 166_667));
        assert!(is_fast(80_000, 166_667));
    }

    #[test]
    fn min_gap_publishes_saturated_with_a_none_marker() {
        assert_eq!(publish_gap(u64::MAX), u32::MAX);
        assert_eq!(publish_gap(0), 0);
        assert_eq!(publish_gap(41_667), 41_667);
        assert_eq!(publish_gap(u32::MAX as u64), u32::MAX);
        assert_eq!(publish_gap(u32::MAX as u64 + 1), u32::MAX);
    }

    #[test]
    fn deltas_wrap() {
        assert_eq!(delta32(10, 25), 15);
        assert_eq!(delta32(u32::MAX - 1, 3), 5);
        assert_eq!(delta32(7, 7), 0);
    }

    #[test]
    fn a_240_hz_heartbeat_reads_240_hz() {
        // 2400 ticks in 10 s.
        assert_eq!(rate_mhz(1_000, 3_400, 50_000, 60_000), Some(240_000));
        // 60 Hz over 2.5 s.
        assert_eq!(rate_mhz(0, 150, 1_000, 3_500), Some(60_000));
    }

    #[test]
    fn the_bogus_4000_per_second_is_a_wrong_pairing_not_a_rate() {
        // Counts from two dumps 30 s apart, divided by a much shorter time
        // difference, give the figure the tester computed. Paired with its own
        // times the same counts read 240 Hz.
        let (n_a, n_b) = (10_000u32, 17_200u32); // 7200 ticks
        assert_eq!(rate_mhz(n_a, n_b, 100_000, 130_000), Some(240_000));
        let bogus = rate_mhz(n_a, n_b, 100_000, 101_800).unwrap();
        assert_eq!(bogus, 4_000_000);
        assert!(!plausible(bogus, 240_000, 50));
    }

    #[test]
    fn counts_and_times_may_wrap_independently() {
        // Count wraps, time does not.
        assert_eq!(rate_mhz(u32::MAX - 239, 240, 1_000, 3_000), Some(240_000));
        // Time wraps (49.7 days), count does not: 2.5 s across the wrap.
        assert_eq!(rate_mhz(0, 600, u32::MAX - 1_000, 1_499), Some(240_000));
        // Both wrap.
        assert_eq!(
            rate_mhz(u32::MAX - 119, 120, u32::MAX - 499, 500),
            Some(240_000)
        );
    }

    #[test]
    fn no_elapsed_time_is_no_rate() {
        assert_eq!(rate_mhz(5, 9, 1_000, 1_000), None);
        // A stalled counter over real time is a rate of zero, not an absent one.
        assert_eq!(rate_mhz(77, 77, 1_000, 6_000), Some(0));
    }

    #[test]
    fn a_counter_that_went_backwards_is_loudly_implausible() {
        // Reboot between the samples: the count restarted.
        let r = rate_mhz(500_000, 100, 1_000, 11_000).unwrap();
        assert!(r > 100_000_000_000);
        assert!(!plausible(r, 240_000, 1_000));
    }

    #[test]
    fn extreme_inputs_do_not_overflow() {
        assert_eq!(
            rate_mhz(0, u32::MAX, 0, 1),
            Some(u32::MAX as u64 * 1_000_000)
        );
        assert_eq!(rate_mhz(0, 0, 0, u32::MAX), Some(0));
    }

    #[test]
    fn age_is_the_distance_from_the_last_tick() {
        assert_eq!(age_ms(10_004, 10_000), 4);
        assert_eq!(age_ms(2, u32::MAX - 3), 6); // across the ms wrap
        assert_eq!(age_ms(7, 7), 0);
    }

    #[test]
    fn plausible_is_a_symmetric_band() {
        assert!(plausible(240_000, 240_000, 0));
        assert!(plausible(238_000, 240_000, 10)); // -0.83%, band is 2400
        assert!(plausible(242_400, 240_000, 10));
        assert!(!plausible(242_401, 240_000, 10));
        assert!(!plausible(237_599, 240_000, 10));
        assert!(!plausible(0, 240_000, 10));
        assert!(plausible(0, 0, 0));
    }

    #[test]
    fn the_period_rounding_stays_inside_one_permille() {
        // The heartbeat's own period (rounded to 100 ns) must read back as the
        // nominal rate within 0.1% at every rate the ladder offers.
        for mhz in [
            30_000u32, 59_940, 60_000, 75_000, 120_000, 144_000, 165_000, 240_000, 360_000,
        ] {
            let period = period_100ns(mhz);
            let measured = 10_000_000_000u64 / period;
            assert!(plausible(measured, mhz, 1), "{mhz}: {measured}");
        }
    }
}
