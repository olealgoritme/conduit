//! Wall-clock bound for a wait made of 1 ms sleep slices.
//!
//! `KeDelayExecutionThread` with a small relative timeout rounds UP to the timer
//! granularity (about 15.6 ms by default), so a loop that sleeps "1 ms" and counts one
//! millisecond per iteration spends up to ~16x its nominal budget in real time. The
//! Venus ring wait (`VenusRing::ring_wait_until`) is such a loop with a 30 000 slice
//! budget: 30 s on paper, up to ~468 s (7.8 minutes) on a default-resolution timer, and
//! it runs under the Venus mutex (and the scanout mutex when it is reached from a
//! Present or a blob teardown), which the HPD worker takes with an infinite wait. A
//! host that stops consuming the ring therefore froze the desktop's flip programming
//! for minutes while the budget "30 s" looked fine in every dump.
//!
//! The rule: a wait is over when EITHER its nominal slice count OR the real time since
//! it began reaches the budget. The real clock only ever tightens the wait; a wait that
//! was healthy (milliseconds) is unchanged.

/// 100 ns units per millisecond.
pub const UNITS_PER_MS: u64 = 10_000;

/// Which bound ended the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Neither bound is reached: keep waiting.
    Within,
    /// The nominal slice count reached the budget first or together with the clock.
    NominalSpent,
    /// The real time reached the budget while the slice count had not: the sleeps took
    /// longer than their nominal length (timer quantum, a descheduled thread).
    RealSpent,
}

/// Real milliseconds between two readings of the same 100 ns clock (0 if it went back).
pub fn elapsed_ms(start_100ns: u64, now_100ns: u64) -> u64 {
    now_100ns.saturating_sub(start_100ns) / UNITS_PER_MS
}

/// Judge a wait of `total_ms` that has slept `nominal_ms` slice-milliseconds and has been
/// running for `real_ms` of real time. A zero `total_ms` is spent at once.
pub fn verdict(nominal_ms: u64, real_ms: u64, total_ms: u64) -> Verdict {
    if nominal_ms >= total_ms {
        Verdict::NominalSpent
    } else if real_ms >= total_ms {
        Verdict::RealSpent
    } else {
        Verdict::Within
    }
}

/// The milliseconds to report for a wait that ended: the larger of the two readings.
pub fn reported_ms(nominal_ms: u64, real_ms: u64) -> u64 {
    nominal_ms.max(real_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_wait_is_unchanged() {
        for n in 0..50 {
            assert_eq!(verdict(n, n, 30_000), Verdict::Within);
        }
    }

    #[test]
    fn the_nominal_count_still_ends_a_wait_whose_sleeps_are_exact() {
        assert_eq!(verdict(30_000, 30_000, 30_000), Verdict::NominalSpent);
        assert_eq!(verdict(30_000, 29_000, 30_000), Verdict::NominalSpent);
    }

    #[test]
    fn rounded_up_sleeps_end_the_wait_at_the_real_budget_not_sixteen_times_later() {
        // Sleeps of ~15.6 ms: after 1 923 slices the real clock reads 30 s, the slice
        // count only 1 923 of 30 000.
        let slices = 1_923u64;
        assert_eq!(verdict(slices - 1, 29_900, 30_000), Verdict::Within);
        assert_eq!(verdict(slices, 30_000, 30_000), Verdict::RealSpent);
        // the old rule would have run on to slice 30 000: about 468 s
        assert!(30_000 * 156 / 10 / 1_000 >= 468);
    }

    #[test]
    fn elapsed_is_in_whole_milliseconds_and_never_negative() {
        assert_eq!(elapsed_ms(0, 10_000), 1);
        assert_eq!(elapsed_ms(5, 9_999 + 5), 0);
        assert_eq!(elapsed_ms(100, 50), 0);
        assert_eq!(reported_ms(1_923, 30_000), 30_000);
        assert_eq!(reported_ms(30_000, 1_000), 30_000);
    }

    #[test]
    fn a_zero_budget_is_spent() {
        assert_eq!(verdict(0, 0, 0), Verdict::NominalSpent);
    }
}
