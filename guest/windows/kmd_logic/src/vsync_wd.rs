//! The independent heartbeat watchdog timer (docs/zero-copy-present.md, "Heartbeat stops after
//! (re)start"): the decisions of a low-rate DISPATCH timer that is NOT part of the vsync chain
//! and NOT the HPD worker, so it cannot be silenced by either.
//!
//! The older watchdog (`hpd_wake::vsync_watch`) runs on a worker pass or an escape, and the
//! worker waits with no timeout when nothing is due: when the heartbeat is dead and nothing
//! wakes the worker, it never runs. The same worker is the only writer of the registry mirror
//! (`VsTickN` ...), so a quiet worker also leaves the mirror frozen at its last pass, and a
//! frozen mirror looks exactly like a dead heartbeat. This timer fixes both: it re-arms a
//! heartbeat that is demonstrably silent, and it asks the worker to refresh the mirror every
//! [`PUBLISH_EVERY_TICKS`] ticks of its own.

use crate::hpd_wake::revive_after_100ns;

/// The watchdog timer's period: 250 ms, relative (negative) 100 ns units. Also the least silence
/// that counts as a dead heartbeat (`hpd_wake::REVIVE_MIN_100NS`).
pub const WD_PERIOD_100NS: i64 = -2_500_000;

/// The watchdog asks the worker to refresh the registry mirror every this many of its own ticks
/// (8 x 250 ms = 2 s).
pub const PUBLISH_EVERY_TICKS: u32 = 8;

/// `VsWdTimer` (default 1): 0 = the watchdog timer is never armed, anything else = on.
pub const fn clamp_wd_timer(v: u32) -> u32 {
    if v != 0 {
        1
    } else {
        0
    }
}

/// How long the heartbeat has been silent, 100 ns: from the newest of the last tick and the
/// watchdog's reference (the last arm, tick or revive). `None` when neither is known (the
/// heartbeat was never armed this generation, or was disarmed).
pub const fn silent_for_100ns(now: u64, last_tick: u64, reference: u64) -> Option<u64> {
    let newest = if last_tick > reference {
        last_tick
    } else {
        reference
    };
    if newest == 0 {
        None
    } else {
        Some(now.saturating_sub(newest))
    }
}

/// Whether a tick callback has been entered and not left: the wrapping difference of the
/// entered and returned counts is small and positive. A returned count AHEAD of the entered one
/// (a reset raced a callback) is not "in flight".
pub const fn cb_in_flight(entered: u32, returned: u32) -> bool {
    let d = entered.wrapping_sub(returned);
    d != 0 && d < 0x8000_0000
}

/// Whether a synchronized call has begun and not returned (same arithmetic as [`cb_in_flight`]).
pub const fn sync_in_flight(begun: u32, returned: u32) -> bool {
    cb_in_flight(begun, returned)
}

/// What one watchdog tick found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdAction {
    /// Not meant to run (disarmed, display half off, adapter not in D0, not yet armed) or
    /// healthy.
    Idle,
    /// Armed, in D0, silent past [`revive_after_100ns`], and no callback is inside the tick:
    /// the one-shot is lost; set it again.
    Fix,
    /// Silent past the limit, but a tick callback is entered and has not returned: a blocked
    /// callback. Re-arming cannot help (an Ex timer does not call back while it runs); count it
    /// and keep the evidence.
    Hung,
}

/// The inputs of [`decide`].
#[derive(Clone, Copy, Debug)]
pub struct WdInput {
    pub armed: bool,
    pub display_half: bool,
    pub adapter_d0: bool,
    pub now: u64,
    pub last_tick: u64,
    pub reference: u64,
    pub period: u64,
    pub cb_entered: u32,
    pub cb_returned: u32,
}

/// The watchdog's decision. Acts only on a heartbeat that is meant to run (`armed`, display half
/// up, adapter in D0) and has been silent for more than `max(250 ms, 16 periods)`.
pub const fn decide(i: WdInput) -> WdAction {
    if !i.armed || !i.display_half || !i.adapter_d0 {
        return WdAction::Idle;
    }
    let Some(silent) = silent_for_100ns(i.now, i.last_tick, i.reference) else {
        return WdAction::Idle;
    };
    if silent <= revive_after_100ns(i.period) {
        return WdAction::Idle;
    }
    if cb_in_flight(i.cb_entered, i.cb_returned) {
        WdAction::Hung
    } else {
        WdAction::Fix
    }
}

/// Whether this watchdog tick (`tick_n`, counted from 1) also asks the worker to refresh the
/// registry mirror. A fix or a hang always does.
pub const fn publish_due(tick_n: u32, acted: bool) -> bool {
    acted || tick_n % PUBLISH_EVERY_TICKS == 0
}

/// The silence in milliseconds for the `VsWdAgeMs` value (0 when unknown), saturated to 32 bits.
pub const fn age_ms(silent: Option<u64>) -> u32 {
    match silent {
        None => 0,
        Some(s) => {
            let ms = s / crate::vsync_rate::UNITS_PER_MS;
            if ms > u32::MAX as u64 {
                u32::MAX
            } else {
                ms as u32
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 10_000;
    const P240: u64 = 41_667;
    const P60: u64 = 166_667;

    fn base() -> WdInput {
        WdInput {
            armed: true,
            display_half: true,
            adapter_d0: true,
            now: 10_000 * MS,
            last_tick: 10_000 * MS - 4 * MS,
            reference: 10_000 * MS - 4 * MS,
            period: P240,
            cb_entered: 100,
            cb_returned: 100,
        }
    }

    #[test]
    fn healthy_chain_is_left_alone() {
        assert_eq!(decide(base()), WdAction::Idle);
    }

    #[test]
    fn silence_below_the_limit_is_left_alone() {
        let mut i = base();
        i.last_tick = i.now - 250 * MS;
        i.reference = i.last_tick;
        assert_eq!(decide(i), WdAction::Idle);
        i.last_tick = i.now - 250 * MS - 1;
        i.reference = i.last_tick;
        assert_eq!(decide(i), WdAction::Fix);
    }

    #[test]
    fn the_limit_is_sixteen_periods_for_slow_rates() {
        // 1 Hz: 16 s, not 250 ms.
        let mut i = base();
        i.now = 100_000 * MS;
        i.period = 10_000_000;
        i.last_tick = i.now - 5_000 * MS;
        i.reference = i.last_tick;
        assert_eq!(decide(i), WdAction::Idle);
        i.last_tick = i.now - 16_001 * MS;
        i.reference = i.last_tick;
        assert_eq!(decide(i), WdAction::Fix);
        // 60 Hz: 16 periods is 266 ms, above the 250 ms floor.
        let mut j = base();
        j.period = P60;
        j.last_tick = j.now - 260 * MS;
        j.reference = j.last_tick;
        assert_eq!(decide(j), WdAction::Idle);
        j.last_tick = j.now - 270 * MS;
        j.reference = j.last_tick;
        assert_eq!(decide(j), WdAction::Fix);
    }

    #[test]
    fn only_a_heartbeat_that_is_meant_to_run_is_acted_on() {
        let mut i = base();
        i.last_tick = 1;
        i.reference = 1;
        assert_eq!(decide(i), WdAction::Fix);
        let mut a = i;
        a.armed = false;
        assert_eq!(decide(a), WdAction::Idle);
        let mut b = i;
        b.display_half = false;
        assert_eq!(decide(b), WdAction::Idle);
        let mut c = i;
        c.adapter_d0 = false;
        assert_eq!(decide(c), WdAction::Idle);
    }

    #[test]
    fn never_armed_or_disarmed_is_idle() {
        let mut i = base();
        i.last_tick = 0;
        i.reference = 0;
        assert_eq!(decide(i), WdAction::Idle);
        assert_eq!(silent_for_100ns(5, 0, 0), None);
    }

    #[test]
    fn the_newest_of_tick_and_reference_counts() {
        // A revive re-bases the reference: the chain just revived is not silent.
        let mut i = base();
        i.last_tick = 1;
        i.reference = i.now - MS;
        assert_eq!(decide(i), WdAction::Idle);
        // A tick newer than the reference counts as well.
        i.reference = 1;
        i.last_tick = i.now - MS;
        assert_eq!(decide(i), WdAction::Idle);
        assert_eq!(silent_for_100ns(100, 40, 70), Some(30));
        assert_eq!(silent_for_100ns(10, 40, 70), Some(0));
    }

    #[test]
    fn a_blocked_callback_is_counted_not_fixed() {
        let mut i = base();
        i.last_tick = 1;
        i.reference = 1;
        i.cb_entered = 101;
        assert_eq!(decide(i), WdAction::Hung);
        i.cb_returned = 101;
        assert_eq!(decide(i), WdAction::Fix);
    }

    #[test]
    fn in_flight_arithmetic_wraps() {
        assert!(!cb_in_flight(5, 5));
        assert!(cb_in_flight(6, 5));
        assert!(cb_in_flight(0, u32::MAX));
        assert!(!cb_in_flight(5, 6));
        assert!(sync_in_flight(1, 0));
        assert!(!sync_in_flight(0, 0));
    }

    #[test]
    fn publish_cadence() {
        assert!(!publish_due(1, false));
        assert!(!publish_due(7, false));
        assert!(publish_due(8, false));
        assert!(publish_due(16, false));
        assert!(publish_due(3, true));
    }

    #[test]
    fn age_publishes_saturated_milliseconds() {
        assert_eq!(age_ms(None), 0);
        assert_eq!(age_ms(Some(2_500_000)), 250);
        assert_eq!(age_ms(Some(u64::MAX)), u32::MAX);
    }

    #[test]
    fn knob_default_is_on_and_period_is_negative_relative() {
        assert_eq!(clamp_wd_timer(0), 0);
        assert_eq!(clamp_wd_timer(1), 1);
        assert_eq!(clamp_wd_timer(9), 1);
        assert!(WD_PERIOD_100NS < 0);
        assert_eq!(WD_PERIOD_100NS, crate::hpd_wake::VSYNC_WATCH_100NS);
    }
}
