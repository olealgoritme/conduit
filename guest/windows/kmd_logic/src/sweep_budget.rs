//! Time budget for a teardown sweep that sends commands to a host which may not
//! answer (StopDevice, device restart).
//!
//! Times are kernel interrupt time in 100 ns units (`KeQueryInterruptTimePrecise`).
//! A budget is a start time plus a total allowance; every host command asks it
//! how long it may wait, and `None` means "the budget is spent: send nothing
//! more, only drop table entries".

/// 100 ns units per millisecond.
pub const UNITS_PER_MS: u64 = 10_000;

/// StopDevice's whole teardown budget: 2 s.
pub const STOP_BUDGET_100NS: u64 = 2 * 1_000 * UNITS_PER_MS;
/// Longest one host command may wait during StopDevice.
pub const STOP_CALL_CAP_MS: u64 = 500;
/// The live (start without a stop, adapter drop) sweep budget: 10 s.
pub const LIVE_BUDGET_100NS: u64 = 10 * 1_000 * UNITS_PER_MS;
/// Longest one host command may wait in the live sweep.
pub const LIVE_CALL_CAP_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepBudget {
    start: u64,
    budget_100ns: u64,
    call_cap_ms: u64,
}

impl SweepBudget {
    pub const fn new(start: u64, budget_100ns: u64, call_cap_ms: u64) -> Self {
        Self {
            start,
            budget_100ns,
            call_cap_ms,
        }
    }

    /// The StopDevice budget starting at `start`.
    pub const fn stop(start: u64) -> Self {
        Self::new(start, STOP_BUDGET_100NS, STOP_CALL_CAP_MS)
    }

    /// The live-sweep budget starting at `start`.
    pub const fn live(start: u64) -> Self {
        Self::new(start, LIVE_BUDGET_100NS, LIVE_CALL_CAP_MS)
    }

    const fn spent(&self, now: u64) -> u64 {
        now.saturating_sub(self.start)
    }

    /// 100 ns units left at `now` (0 once spent).
    pub const fn remaining_100ns(&self, now: u64) -> u64 {
        self.budget_100ns.saturating_sub(self.spent(now))
    }

    pub const fn expired(&self, now: u64) -> bool {
        self.remaining_100ns(now) == 0
    }

    /// How long the next host command may wait: the time left rounded UP to a
    /// millisecond, capped at the per-call cap, at least 1 ms; `None` once the
    /// budget is spent.
    pub const fn call_timeout_ms(&self, now: u64) -> Option<u64> {
        let remaining = self.remaining_100ns(now);
        if remaining == 0 {
            return None;
        }
        let ms = remaining.div_ceil(UNITS_PER_MS);
        let ms = if ms < self.call_cap_ms {
            ms
        } else {
            self.call_cap_ms
        };
        Some(if ms == 0 { 1 } else { ms })
    }
}

/// Whole milliseconds from `start` to `now`, saturated to u32 (a breadcrumb).
pub const fn elapsed_ms(start: u64, now: u64) -> u32 {
    let ms = now.saturating_sub(start) / UNITS_PER_MS;
    if ms > u32::MAX as u64 {
        u32::MAX
    } else {
        ms as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_budget_is_two_seconds_and_calls_are_capped() {
        let b = SweepBudget::stop(1_000);
        assert_eq!(b.call_timeout_ms(1_000), Some(500));
        // 1.9 s spent: 100 ms left.
        assert_eq!(b.call_timeout_ms(1_000 + 19_000_000), Some(100));
        // Exactly spent.
        assert_eq!(b.call_timeout_ms(1_000 + STOP_BUDGET_100NS), None);
        assert!(b.expired(1_000 + STOP_BUDGET_100NS));
        assert!(!b.expired(1_000 + STOP_BUDGET_100NS - 1));
    }

    #[test]
    fn a_call_never_outlives_the_budget() {
        let b = SweepBudget::stop(0);
        let mut now = 0u64;
        let mut worst_end = 0u64;
        // Each call waits its full allowance.
        while let Some(ms) = b.call_timeout_ms(now) {
            let end = now + ms * UNITS_PER_MS;
            worst_end = worst_end.max(end);
            now = end;
        }
        // Rounding up to a whole ms may overshoot by under one ms, no more.
        assert!(worst_end < STOP_BUDGET_100NS + UNITS_PER_MS);
    }

    #[test]
    fn sub_millisecond_remainder_still_gets_one_ms() {
        let b = SweepBudget::stop(0);
        assert_eq!(b.call_timeout_ms(STOP_BUDGET_100NS - 1), Some(1));
    }

    #[test]
    fn live_budget_keeps_the_old_numbers() {
        let b = SweepBudget::live(0);
        assert_eq!(b.call_timeout_ms(0), Some(5_000));
        assert_eq!(b.call_timeout_ms(LIVE_BUDGET_100NS), None);
        assert_eq!(b.call_timeout_ms(LIVE_BUDGET_100NS - 60_000_000), Some(5_000));
        assert_eq!(b.call_timeout_ms(LIVE_BUDGET_100NS - 20_000_000), Some(2_000));
    }

    #[test]
    fn a_clock_before_start_does_not_underflow() {
        let b = SweepBudget::stop(100);
        assert_eq!(b.call_timeout_ms(0), Some(500));
    }

    #[test]
    fn elapsed_ms_saturates() {
        assert_eq!(elapsed_ms(0, 25_000), 2);
        assert_eq!(elapsed_ms(100, 0), 0);
        assert_eq!(elapsed_ms(0, u64::MAX), u32::MAX);
    }
}
