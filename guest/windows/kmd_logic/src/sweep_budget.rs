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
/// Longest one RM `Close` may wait during StopDevice. A device-handle `Close`
/// frees VRAM and legitimately takes longer than a table query; 500 ms was short
/// for it, and a `Close` that is not confirmed leaves the host holding the pinned
/// pages. It is still inside [`STOP_BUDGET_100NS`], which stays the hard bound.
pub const STOP_CLOSE_CAP_MS: u64 = 2_000;
/// The live (start without a stop, adapter drop) sweep budget: 10 s.
pub const LIVE_BUDGET_100NS: u64 = 10 * 1_000 * UNITS_PER_MS;
/// Longest one host command may wait in the live sweep.
pub const LIVE_CALL_CAP_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepBudget {
    start: u64,
    budget_100ns: u64,
    call_cap_ms: u64,
    close_cap_ms: u64,
}

impl SweepBudget {
    pub const fn new(start: u64, budget_100ns: u64, call_cap_ms: u64) -> Self {
        Self {
            start,
            budget_100ns,
            call_cap_ms,
            close_cap_ms: call_cap_ms,
        }
    }

    /// The same budget with a different per-call cap for `Close`.
    pub const fn with_close_cap(self, close_cap_ms: u64) -> Self {
        Self {
            close_cap_ms,
            ..self
        }
    }

    /// The same budget with `spent_100ns` of unrelated waiting (a registry hive
    /// flush, a worker join) given back, so that time does not eat the allowance
    /// the host commands need. The deadline moves, the allowance does not.
    pub const fn credit(self, spent_100ns: u64) -> Self {
        Self {
            start: self.start.saturating_add(spent_100ns),
            ..self
        }
    }

    /// The StopDevice budget starting at `start`.
    pub const fn stop(start: u64) -> Self {
        Self::new(start, STOP_BUDGET_100NS, STOP_CALL_CAP_MS).with_close_cap(STOP_CLOSE_CAP_MS)
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
        self.timeout_with_cap(now, self.call_cap_ms)
    }

    /// [`Self::call_timeout_ms`] for an RM `Close`, whose cap is higher (see
    /// [`STOP_CLOSE_CAP_MS`]); never more than what is left of the budget.
    pub const fn close_timeout_ms(&self, now: u64) -> Option<u64> {
        self.timeout_with_cap(now, self.close_cap_ms)
    }

    const fn timeout_with_cap(&self, now: u64, cap_ms: u64) -> Option<u64> {
        let remaining = self.remaining_100ns(now);
        if remaining == 0 {
            return None;
        }
        let ms = remaining.div_ceil(UNITS_PER_MS);
        let ms = if ms < cap_ms { ms } else { cap_ms };
        Some(if ms == 0 { 1 } else { ms })
    }
}

/// What may be done with the pages pinned for the host once the sweep ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinFate {
    /// The host confirmed it let go of everything that could alias them: unlock.
    Unlock,
    /// Some mapping/handle was never confirmed closed (budget spent, timeout,
    /// device error): the GPU may still write these pages. Unlocking them would
    /// let the guest reuse that RAM underneath the GPU, so they stay locked —
    /// leaked on purpose, counted.
    Leak,
}

/// What to do with ONE pin, given the sweep's [`PinFate`] and whether the host
/// may alias that pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAction {
    /// `MmUnlockPages` (and release everything the pin owns).
    Unlock,
    /// Leave the pages locked on purpose. The pin's reference on its owning
    /// process stays too (see `NvrmPin::leak`): the pin is never dropped.
    Leak,
}

impl PinFate {
    /// A pin no `FORWARD` claimed was never described to the host, so nothing
    /// can alias it and it is unlocked whatever happened to the sweep. A claimed
    /// pin is unlocked only when the host confirmed closing everything that could
    /// alias it.
    pub const fn action(self, host_may_alias: bool) -> PinAction {
        match self {
            PinFate::Leak if host_may_alias => PinAction::Leak,
            PinFate::Unlock | PinFate::Leak => PinAction::Unlock,
        }
    }
}

/// Outcome of the host-side closes of one sweep.
///
/// A handle or mapping that was dropped from the tables WITHOUT a confirmed
/// host close (nothing sent because the budget was spent or an earlier command
/// failed; or sent and failed) counts against `all_closed`. The pins decide on
/// that, not on whether the sweep merely kept "sending" until the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloseTally {
    confirmed: u32,
    unsent: u32,
    failed: u32,
}

impl CloseTally {
    pub const fn new() -> Self {
        Self {
            confirmed: 0,
            unsent: 0,
            failed: 0,
        }
    }

    /// The host acknowledged this Close/Munmap.
    pub fn confirmed(&mut self) {
        self.confirmed = self.confirmed.saturating_add(1);
    }

    /// The table entry was dropped without sending (budget spent, or sending
    /// already stopped).
    pub fn unsent(&mut self) {
        self.unsent = self.unsent.saturating_add(1);
    }

    /// Sent, and the round trip failed or timed out.
    pub fn failed(&mut self) {
        self.failed = self.failed.saturating_add(1);
    }

    pub const fn confirmed_count(&self) -> u32 {
        self.confirmed
    }

    /// Every taken entry was confirmed closed by the host.
    pub const fn all_closed(&self) -> bool {
        self.unsent == 0 && self.failed == 0
    }

    pub const fn pin_fate(&self) -> PinFate {
        if self.all_closed() {
            PinFate::Unlock
        } else {
            PinFate::Leak
        }
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
    fn a_close_may_wait_longer_than_other_commands_but_never_past_the_budget() {
        let b = SweepBudget::stop(0);
        assert_eq!(b.call_timeout_ms(0), Some(500));
        assert_eq!(b.close_timeout_ms(0), Some(2_000));
        // 1.5 s spent: 500 ms left, so a Close gets exactly what is left.
        assert_eq!(b.close_timeout_ms(15_000_000), Some(500));
        assert_eq!(b.close_timeout_ms(STOP_BUDGET_100NS), None);
        assert_eq!(b.close_timeout_ms(STOP_BUDGET_100NS - 1), Some(1));
    }

    #[test]
    fn the_live_budget_close_cap_is_the_call_cap() {
        let b = SweepBudget::live(0);
        assert_eq!(b.close_timeout_ms(0), b.call_timeout_ms(0));
        assert_eq!(b.close_timeout_ms(LIVE_BUDGET_100NS), None);
    }

    #[test]
    fn a_close_never_outlives_the_budget() {
        let b = SweepBudget::stop(0);
        let mut now = 0u64;
        let mut worst_end = 0u64;
        while let Some(ms) = b.close_timeout_ms(now) {
            let end = now + ms * UNITS_PER_MS;
            worst_end = worst_end.max(end);
            now = end;
        }
        assert!(worst_end < STOP_BUDGET_100NS + UNITS_PER_MS);
    }

    #[test]
    fn credited_time_does_not_eat_the_budget() {
        let b = SweepBudget::stop(0);
        // A 700 ms hive flush ran: without credit the allowance is short by that.
        let flush = 700 * UNITS_PER_MS;
        assert_eq!(b.remaining_100ns(flush), STOP_BUDGET_100NS - flush);
        let c = b.credit(flush);
        assert_eq!(c.remaining_100ns(flush), STOP_BUDGET_100NS);
        assert_eq!(c.call_timeout_ms(flush), Some(500));
        assert_eq!(c.close_timeout_ms(flush), Some(2_000));
        // Still bounded: the same allowance counts down from the new deadline.
        assert!(c.expired(flush + STOP_BUDGET_100NS));
        // Crediting never underflows or wraps.
        assert_eq!(b.credit(u64::MAX).remaining_100ns(0), STOP_BUDGET_100NS);
    }

    #[test]
    fn pins_unlock_only_when_every_close_was_confirmed() {
        let mut t = CloseTally::new();
        // Nothing taken at all: nothing the host could hold.
        assert!(t.all_closed());
        assert_eq!(t.pin_fate(), PinFate::Unlock);
        t.confirmed();
        t.confirmed();
        assert!(t.all_closed());
        assert_eq!(t.pin_fate(), PinFate::Unlock);
        assert_eq!(t.confirmed_count(), 2);
    }

    #[test]
    fn a_budget_spent_sweep_leaks_the_pins() {
        // Sending stopped early: later handles were dropped from the tables
        // with nothing sent. The host still holds them.
        let mut t = CloseTally::new();
        t.confirmed();
        t.unsent();
        assert!(!t.all_closed());
        assert_eq!(t.pin_fate(), PinFate::Leak);
    }

    #[test]
    fn a_failed_close_leaks_the_pins() {
        let mut t = CloseTally::new();
        t.failed();
        t.confirmed();
        assert_eq!(t.pin_fate(), PinFate::Leak);
    }

    #[test]
    fn pin_action_follows_fate_and_alias() {
        // Confirmed closed: everything unlocks, claimed or not.
        assert_eq!(PinFate::Unlock.action(true), PinAction::Unlock);
        assert_eq!(PinFate::Unlock.action(false), PinAction::Unlock);
        // Not confirmed: only a pin the host may alias stays locked.
        assert_eq!(PinFate::Leak.action(true), PinAction::Leak);
        assert_eq!(PinFate::Leak.action(false), PinAction::Unlock);
    }

    #[test]
    fn a_failed_transport_sweep_is_a_leak_fate() {
        // `teardown_nvrm_state` (nothing was sent, nothing confirmed) decides
        // with `PinFate::Leak`: the same answer an unconfirmed sweep gives.
        let mut t = CloseTally::new();
        t.unsent();
        assert_eq!(t.pin_fate(), PinFate::Leak);
        assert_eq!(t.pin_fate().action(true), PinAction::Leak);
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
