//! The HPD worker's wake accounting, its wait, the cadence of its periodic dump and the vsync
//! heartbeat's watchdog and power rule: the pure half of the "T5 anomalies" work
//! (`docs/kmd-rm-client.md` 15.18.14). The I/O half is `kmd_render/src/ddi/stall_diag.rs` (the
//! atomics and the counters), `ddi/hpd.rs` (the loop), `ddi/scanout_trace.rs` (the dump),
//! `adapter/kobj.rs` (the timer) and `ddi/lifecycle.rs` (`DxgkDdiSetPowerState`).
//!
//! WHAT THE T5 RUN SHOWED (KMD 325.1, `ForeignFlip` 1, `FfAsyncWin` 0, DWM on NVK, an NVK window
//! presenting at ~8000 fps through WDDM and a window moving at ~36 moves/s): `HpdLoopN` rose about
//! 9000 per second while `FfFrames` rose 150 per second, and `VsTickN` stayed at 269.
//!
//! * **The loop rate is the present rate, not a timer.** Every timed wake of the worker is at
//!   least 1 ms ([`wait_plan`] proves it for every input), so a timer cannot make 9000 loops a
//!   second; only an event can: each windowed-Blt `SubmitCommand` signals the worker once (about
//!   8000 a second from the spin window), and each present marker that asks for a desktop refresh
//!   signals it again. What WAS a defect is the cost of a wake: the `Vp*` dump (several hundred
//!   registry writes) ran every 128th WAKE, so at this wake rate it ran about 70 times a second
//!   and kept the worker out of its flip pacing for a large part of every second ([`dump_due`]
//!   makes the cadence a function of time as well).
//! * **The wait is bounded by construction** ([`wait_plan`]): a zero or positive timeout would be
//!   an immediate return or an ABSOLUTE time to `KeWaitForSingleObject` (a busy loop); the plan
//!   never produces one.
//! * **The vsync heartbeat** can stop for two reasons the counters could not tell apart: the
//!   power callback quiesced it, or the timer chain died with `vsync_armed` still set
//!   ([`revive_due`], [`power_vsync`]).
//!
//! Everything here is a function of its arguments: no clock, no memory, no registry.

use crate::stall_diag::age_ms;

// ---- who signalled the worker --------------------------------------------------------------

/// The cause of a wake signal (`AdapterContext::signal_hpd_for`). The bit of each is
/// `1 << index`, and the index is also the slot of its `HpdSg*` counter. Append, never renumber:
/// the bits are read by hand from the service key (`HpdWkSrc`).
pub mod cause {
    /// A windowed-Blt `SubmitCommand` (`note_and_maybe_signal`): the worker owes the retire pass.
    pub const BLT: u32 = 0;
    /// A desktop refresh was requested (`request_scanout_refresh_for`: a present marker, a bind
    /// edge, a completion DPC, a source's end).
    pub const REFRESH: u32 = 1;
    /// A frame or resume edge of the resident source (`rm_present::note_frame_edge`,
    /// `note_resume_edge`).
    pub const EDGE: u32 = 2;
    /// The used-ring drain found a fence the KMD owes a flip or a close for.
    pub const FENCE: u32 = 3;
    /// A foreign scanout source was set (`foreign_scanout_set`): the worker arms its lapse.
    pub const FS_SET: u32 = 4;
    /// The host released a scanout buffer (`scanout_release`).
    pub const RELEASE: u32 = 5;
    /// The `ForeignFlip` arm dropped or changed its shown allocation.
    pub const FLIP: u32 = 6;
    /// Every other `signal_hpd()` caller (config change, DPC wakes, ctrl completions, ...).
    pub const OTHER: u32 = 7;
    /// How many causes there are (the length of the counter array).
    pub const COUNT: usize = 8;

    /// Names for the doc and the tests, in index order.
    pub const NAMES: [&str; COUNT] = [
        "blt", "refresh", "edge", "fence", "fs_set", "release", "flip", "other",
    ];
}

/// The bit of a cause in the `HpdWkSrc` mask.
pub const fn cause_bit(cause: u32) -> u32 {
    if cause < 32 {
        1 << cause
    } else {
        0
    }
}

// ---- the worker's wait ---------------------------------------------------------------------

/// The shortest timed wait the worker ever asks for, in 100 ns (0.5 ms): the floor of
/// [`wait_plan`]. Every producer of a timed wake already clamps to 1 ms or more; this is the
/// guarantee that no future one can make the worker spin through a zero, positive (absolute) or
/// microscopic timeout.
pub const MIN_WAIT_100NS: i64 = 5_000;

/// What kind of wait the loop is about to make (the `HpdTm*` counters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitClass {
    /// One async control command owns descriptors: the lost-interrupt poll.
    CtrlPoll,
    /// A refresh enqueue failed and must be retried.
    Retry,
    /// A foreign lapse, a paced frame or a counter mirror is due.
    Due,
    /// Nothing is due: wait for an event forever.
    Infinite,
}

/// The inputs of the worker's wait, all relative (negative) 100 ns timeouts where a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitInputs {
    /// An async flush owns control descriptors.
    pub ctrl_inflight: bool,
    /// A refresh enqueue is owed and none is in flight.
    pub retry_pending: bool,
    /// `foreign_scanout_wait_100ns`: the earlier of the lapse and the presenter's paced frame.
    pub foreign: Option<i64>,
    /// The counter mirror's recheck, when one is wanted.
    pub mirror: Option<i64>,
    /// The vsync heartbeat's watchdog tick ([`VSYNC_WATCH_100NS`], relative), while the heartbeat
    /// is meant to run: without it a heartbeat that died while the worker sleeps (nothing else
    /// wakes it for an MMIO flip) would strand the pending flip for ever.
    pub watch: Option<i64>,
}

/// The worker's watchdog tick while the vsync heartbeat is meant to run: 250 ms (4 wakes a
/// second at idle), the same silence [`REVIVE_MIN_100NS`] counts as a dead heartbeat.
pub const VSYNC_WATCH_100NS: i64 = -(REVIVE_MIN_100NS as i64);

/// The ctrl poll and the refresh retry timeouts (relative, 100 ns), as `ddi/hpd.rs` has them.
pub const CTRL_INFLIGHT_POLL_100NS: i64 = -40_000;
pub const REFRESH_RETRY_100NS: i64 = -160_000;

/// The worker's wait: `(timeout, class)`. `None` is an infinite wait. The order and the values
/// are those the loop had inline (control poll, else retry, else the earlier of the two optional
/// due times), plus the floor: a timeout that is zero, positive, or shorter than
/// [`MIN_WAIT_100NS`] becomes exactly the floor (a positive or zero value would be an immediate
/// return or an absolute deadline: a spin).
pub const fn wait_plan(i: WaitInputs) -> (Option<i64>, WaitClass) {
    if i.ctrl_inflight {
        return (Some(floor(CTRL_INFLIGHT_POLL_100NS)), WaitClass::CtrlPoll);
    }
    if i.retry_pending {
        return (Some(floor(REFRESH_RETRY_100NS)), WaitClass::Retry);
    }
    // Both are relative (negative) 100 ns units: the earlier is the one closer to zero, i.e. the
    // larger.
    let due = earlier(earlier(i.foreign, i.mirror), i.watch);
    match due {
        Some(d) => (Some(floor(d)), WaitClass::Due),
        None => (None, WaitClass::Infinite),
    }
}

/// The earlier of two optional relative timeouts (the one closer to zero, i.e. the larger).
const fn earlier(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a > b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The floor: at most `-MIN_WAIT_100NS`.
const fn floor(t: i64) -> i64 {
    if t > -MIN_WAIT_100NS {
        -MIN_WAIT_100NS
    } else {
        t
    }
}

/// The published `HpdWait`: the timeout in microseconds (0 = infinite).
pub const fn wait_us(t: Option<i64>) -> u32 {
    match t {
        Some(t) => {
            let us = t.unsigned_abs() / 10;
            if us > u32::MAX as u64 {
                u32::MAX
            } else {
                us as u32
            }
        }
        None => 0,
    }
}

// ---- the periodic dump ---------------------------------------------------------------------

/// Worker loops between two `Vp*` dumps, at least (the cadence before this work).
pub const DUMP_EVERY_LOOPS: u32 = 128;
/// Milliseconds between two dumps, at least. A dump is several hundred registry writes (the `Vp`
/// ring, the histograms, the scalars, the read ledger): at the T5 run's 9000 loops a second the
/// loop-count cadence alone ran it about 70 times a second.
pub const DUMP_MIN_INTERVAL_MS: u32 = 1_000;

/// Whether this wake runs the periodic dump. `loops_since` is the number of wakes since the last
/// dump (or since the reset), `age` the milliseconds since it, `first` that no dump has run yet
/// this generation. Both conditions must hold: at a low wake rate the 128 loops are the limit
/// (the cadence is what it was), at a high one the interval is.
pub const fn dump_due(first: bool, loops_since: u32, now_ms: u32, last_ms: u32) -> bool {
    first || (loops_since >= DUMP_EVERY_LOOPS && age_ms(now_ms, last_ms) >= DUMP_MIN_INTERVAL_MS)
}

// ---- the vsync heartbeat -------------------------------------------------------------------

/// A heartbeat that has not ticked for this many periods (and at least [`REVIVE_MIN_100NS`])
/// while armed is dead.
pub const REVIVE_PERIODS: u64 = 16;
/// The least silence that counts as a dead heartbeat, 100 ns (250 ms): well above any scheduling
/// delay of a DISPATCH timer, so a live chain never meets it.
pub const REVIVE_MIN_100NS: u64 = 2_500_000;

/// How long the heartbeat may stay silent before it is revived, for a tick period of `period`.
pub const fn revive_after_100ns(period: u64) -> u64 {
    let by_period = period.saturating_mul(REVIVE_PERIODS);
    if by_period > REVIVE_MIN_100NS {
        by_period
    } else {
        REVIVE_MIN_100NS
    }
}

/// Whether the heartbeat must be re-armed: the chain is supposed to run (`armed` and the display
/// half is up) and nothing has ticked, armed or revived it since `reference` (the newest of the
/// last tick, the last arm and the last revive; 0 = unknown, never revived on unknown).
/// A revive re-bases `reference`, so a chain that cannot be revived is tried once per
/// [`revive_after_100ns`], not once per call.
pub const fn revive_due(armed: bool, display_half: bool, now: u64, reference: u64, period: u64) -> bool {
    if !armed || !display_half || reference == 0 {
        return false;
    }
    now.saturating_sub(reference) > revive_after_100ns(period)
}

/// What the watchdog does about the heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsyncWatch {
    /// Nothing: healthy, or not meant to run.
    Ok,
    /// Armed and silent for too long: set the one-shot again ([`revive_due`]).
    Revive,
    /// Disarmed although the adapter is in D0, the display half is up and dxgkrnl has the vsync
    /// delivery gate open (a quiesce whose D0 never came): arm it (PASSIVE callers only).
    Resume,
}

/// The watchdog's decision. `gate_open` is `vsync_enabled != 0` (set by StartDevice and
/// `ControlInterrupt`, cleared by StopDevice), `adapter_d0` the adapter's last power state.
pub const fn vsync_watch(
    armed: bool,
    display_half: bool,
    adapter_d0: bool,
    gate_open: bool,
    now: u64,
    reference: u64,
    period: u64,
) -> VsyncWatch {
    if !display_half {
        return VsyncWatch::Ok;
    }
    if armed {
        if revive_due(true, true, now, reference, period) {
            VsyncWatch::Revive
        } else {
            VsyncWatch::Ok
        }
    } else if adapter_d0 && gate_open {
        VsyncWatch::Resume
    } else {
        VsyncWatch::Ok
    }
}

/// `DISPLAY_ADAPTER_HW_ID`: the `DeviceUid` `DxgkDdiSetPowerState` carries for the adapter itself.
/// Any other uid is a child device (the monitor on the video present target).
pub const DISPLAY_ADAPTER_HW_ID: u32 = 0xFFFF_FFFF;

/// What a power transition does to the vsync heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerVsync {
    /// Re-arm (D0 with the display half up).
    Resume,
    /// Cancel and drain (the ADAPTER left D0).
    Quiesce,
    /// Nothing.
    Leave,
}

/// The heartbeat's reaction to `DxgkDdiSetPowerState(device_uid, state)`. It used to quiesce on
/// ANY non-D0 state of ANY uid. The heartbeat is the adapter's: it stops when the adapter powers
/// down (a tick calls back into dxgkrnl, which is not allowed then). A CHILD going to D3 (the
/// monitor blanked) must not stop it: dxgkrnl keeps queueing flips to the source, and a flip is
/// retired only by a vsync, so a heartbeat stopped by the monitor's power state strands the
/// desktop's flips until something re-arms it. D0 (any uid) keeps re-arming, as before; arming an
/// armed heartbeat is a no-op.
pub const fn power_vsync(device_uid: u32, d0: bool, display_half: bool) -> PowerVsync {
    if d0 {
        if display_half {
            PowerVsync::Resume
        } else {
            PowerVsync::Leave
        }
    } else if device_uid == DISPLAY_ADAPTER_HW_ID {
        PowerVsync::Quiesce
    } else {
        PowerVsync::Leave
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const MS: u64 = 10_000;

    fn inputs(c: bool, r: bool, f: Option<i64>, m: Option<i64>) -> WaitInputs {
        WaitInputs {
            ctrl_inflight: c,
            retry_pending: r,
            foreign: f,
            mirror: m,
            watch: None,
        }
    }

    // ---- the wait ----------------------------------------------------------------------

    #[test]
    fn the_wait_is_what_the_loop_had_inline() {
        // No due time: infinite.
        assert_eq!(
            wait_plan(inputs(false, false, None, None)),
            (None, WaitClass::Infinite)
        );
        // The control poll beats everything, then the retry.
        assert_eq!(
            wait_plan(inputs(true, true, Some(-20_000), Some(-30_000))),
            (Some(-40_000), WaitClass::CtrlPoll)
        );
        assert_eq!(
            wait_plan(inputs(false, true, Some(-20_000), None)),
            (Some(-160_000), WaitClass::Retry)
        );
        // The earlier of the two optional due times is the one closer to zero.
        assert_eq!(
            wait_plan(inputs(false, false, Some(-20_000), Some(-2_500_000))),
            (Some(-20_000), WaitClass::Due)
        );
        assert_eq!(
            wait_plan(inputs(false, false, Some(-9_000_000), Some(-2_500_000))),
            (Some(-2_500_000), WaitClass::Due)
        );
        assert_eq!(
            wait_plan(inputs(false, false, None, Some(-2_500_000))),
            (Some(-2_500_000), WaitClass::Due)
        );
        assert_eq!(
            wait_plan(inputs(false, false, Some(-36_000_000_000), None)),
            (Some(-36_000_000_000), WaitClass::Due)
        );
    }

    #[test]
    fn no_input_can_make_the_worker_spin_on_a_timeout() {
        // A zero or positive timeout is an immediate return (or an absolute deadline in the
        // past) for KeWaitForSingleObject: the loop would run flat out. Sweep the bad values.
        let bad = [
            0i64,
            1,
            100,
            i64::MAX,
            -1,
            -10,
            -999,
            -4_999,
            -5_000,
            -5_001,
            -10_000,
        ];
        for &f in &bad {
            for &m in &bad {
                for c in [false, true] {
                    for r in [false, true] {
                        for fo in [None, Some(f)] {
                            for mi in [None, Some(m)] {
                                let (t, _) = wait_plan(inputs(c, r, fo, mi));
                                if let Some(t) = t {
                                    assert!(t <= -MIN_WAIT_100NS, "{t} from {c} {r} {fo:?} {mi:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_timer_alone_cannot_give_9000_loops_a_second() {
        // T5: HpdLoopN rose ~9000 per second. The shortest wait is 0.5 ms (2000 a second), and
        // every real producer asks for 1 ms or more (the foreign wait clamps there): a timer
        // alone cannot be that rate. Only event signals can.
        let per_second_at_floor = 10_000_000 / MIN_WAIT_100NS as u64;
        assert!(per_second_at_floor < 9_000);
        assert_eq!(per_second_at_floor, 2_000);
    }

    #[test]
    fn wait_us_publishes_microseconds() {
        assert_eq!(wait_us(None), 0);
        assert_eq!(wait_us(Some(-40_000)), 4_000);
        assert_eq!(wait_us(Some(-5_000)), 500);
        assert_eq!(wait_us(Some(-36_000_000_000)), 3_600_000_000);
        assert_eq!(wait_us(Some(i64::MIN)), u32::MAX);
    }

    // ---- causes ------------------------------------------------------------------------

    #[test]
    fn the_causes_are_distinct_bits_and_named() {
        let all = [
            cause::BLT,
            cause::REFRESH,
            cause::EDGE,
            cause::FENCE,
            cause::FS_SET,
            cause::RELEASE,
            cause::FLIP,
            cause::OTHER,
        ];
        assert_eq!(all.len(), cause::COUNT);
        assert_eq!(cause::NAMES.len(), cause::COUNT);
        let mut mask = 0u32;
        for (i, c) in all.iter().enumerate() {
            assert_eq!(*c as usize, i, "the index is the slot");
            assert_eq!(mask & cause_bit(*c), 0);
            mask |= cause_bit(*c);
        }
        assert_eq!(mask, 0xFF);
        assert_eq!(cause_bit(32), 0);
        assert_eq!(cause_bit(200), 0);
    }

    // ---- the dump ----------------------------------------------------------------------

    /// Simulate `seconds` of wakes at `rate` per second; the dumps that ran.
    fn dumps(rate: u32, seconds: u32) -> u32 {
        let mut n = 0u32;
        let mut since = 0u32;
        let mut last_ms = 0u32;
        let mut first = true;
        let mut count = 0;
        let total = rate as u64 * seconds as u64;
        for i in 0..total {
            let now_ms = (i * 1000 / rate as u64) as u32;
            n += 1;
            since += 1;
            if dump_due(first, since, now_ms, last_ms) {
                first = false;
                since = 0;
                last_ms = now_ms;
                count += 1;
            }
        }
        let _ = n;
        count
    }

    #[test]
    fn the_dump_is_once_a_second_at_9000_wakes_a_second() {
        // The T5 rate: the old cadence (every 128th wake) would have run it 70 times a second.
        let old = 9_000 / DUMP_EVERY_LOOPS;
        assert!(old >= 70);
        let d = dumps(9_000, 10);
        assert!(d <= 11 && d >= 9, "{d} dumps in 10 s");
    }

    #[test]
    fn the_dump_cadence_at_a_low_rate_is_what_it_was() {
        // 10 wakes a second: 128 loops is 12.8 s, so the loop count is the limit, as before.
        let d = dumps(10, 60);
        // first + one per 128 loops (600 loops: 4 more)
        assert_eq!(d, 1 + 600 / 128);
        // 155 wakes a second (the flush rate the old cadence was sized for): about once a
        // second, as it was (0.83 s then, 1 s now).
        let d = dumps(155, 10);
        assert!((9..=12).contains(&d), "{d}");
    }

    #[test]
    fn the_dump_survives_a_wrapping_clock() {
        assert!(dump_due(false, 128, 500, u32::MAX - 600));
        assert!(!dump_due(false, 128, 500, u32::MAX - 400));
        // A stamp ahead of now is age 0, never a 49-day age.
        assert!(!dump_due(false, 1_000, 10, 500));
        assert!(dump_due(true, 0, 0, 0));
    }

    // ---- the heartbeat -----------------------------------------------------------------

    #[test]
    fn a_live_chain_is_never_revived() {
        let p = 41_667; // 240 Hz
        // Ticking every period: the silence is one period.
        assert!(!revive_due(true, true, 1_000_000 + p, 1_000_000, p));
        // 100 ms of silence at 240 Hz is 24 periods but under the 250 ms floor.
        assert!(!revive_due(true, true, 1_000_000 + 100 * MS, 1_000_000, p));
        // 60 Hz: 16 periods are 267 ms.
        let p60 = 166_667;
        assert!(!revive_due(true, true, 1_000_000 + 260 * MS, 1_000_000, p60));
        assert!(revive_due(true, true, 1_000_000 + 270 * MS, 1_000_000, p60));
    }

    #[test]
    fn a_dead_chain_is_revived_once_per_silence() {
        let p = 41_667;
        let t0 = 5_000_000u64;
        assert!(revive_due(true, true, t0 + 251 * MS, t0, p));
        // The revive re-bases the reference: not again at once.
        assert!(!revive_due(true, true, t0 + 251 * MS + 1, t0 + 251 * MS, p));
        assert!(revive_due(true, true, t0 + 503 * MS, t0 + 251 * MS, p));
    }

    #[test]
    fn nothing_is_revived_that_is_not_meant_to_run() {
        let p = 166_667;
        let t0 = 5_000_000u64;
        let late = t0 + 10_000 * MS;
        assert!(!revive_due(false, true, late, t0, p), "disarmed (D3, stopped)");
        assert!(!revive_due(true, false, late, t0, p), "no display half");
        assert!(!revive_due(true, true, late, 0, p), "no reference yet");
        // A clock read before the reference (a stale stamp) is no silence.
        assert!(!revive_due(true, true, t0 - 1, t0, p));
    }

    #[test]
    fn the_revive_threshold_follows_the_period() {
        assert_eq!(revive_after_100ns(41_667), REVIVE_MIN_100NS);
        assert_eq!(revive_after_100ns(166_667), 166_667 * 16);
        assert_eq!(revive_after_100ns(10_000_000), 160_000_000); // 1 Hz: 16 s
        assert_eq!(revive_after_100ns(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_watchdog_decides_per_state() {
        let p = 166_667;
        let t0 = 5_000_000u64;
        let late = t0 + 10_000 * MS;
        // Armed, silent: revive. Armed, ticking: ok.
        assert_eq!(vsync_watch(true, true, true, true, late, t0, p), VsyncWatch::Revive);
        assert_eq!(vsync_watch(true, true, true, true, t0 + 1, t0, p), VsyncWatch::Ok);
        // Disarmed in D0 with the gate open: resume. A quiesce (adapter not D0), a stop (gate
        // closed) or no display half: leave it.
        assert_eq!(vsync_watch(false, true, true, true, late, 0, p), VsyncWatch::Resume);
        assert_eq!(vsync_watch(false, true, false, true, late, 0, p), VsyncWatch::Ok);
        assert_eq!(vsync_watch(false, true, true, false, late, 0, p), VsyncWatch::Ok);
        assert_eq!(vsync_watch(false, false, true, true, late, 0, p), VsyncWatch::Ok);
        assert_eq!(vsync_watch(true, false, true, true, late, t0, p), VsyncWatch::Ok);
    }

    #[test]
    fn the_worker_wakes_for_the_watchdog_and_for_nothing_faster() {
        let mut i = inputs(false, false, None, None);
        i.watch = Some(VSYNC_WATCH_100NS);
        assert_eq!(wait_plan(i), (Some(-2_500_000), WaitClass::Due));
        // A nearer due time wins; a farther one does not.
        i.foreign = Some(-20_000);
        assert_eq!(wait_plan(i), (Some(-20_000), WaitClass::Due));
        i.foreign = Some(-9_000_000);
        assert_eq!(wait_plan(i), (Some(-2_500_000), WaitClass::Due));
    }

    #[test]
    fn power_states_that_are_not_the_adapters_do_not_stop_the_heartbeat() {
        // The adapter leaving D0 quiesces, as it always did.
        assert_eq!(
            power_vsync(DISPLAY_ADAPTER_HW_ID, false, true),
            PowerVsync::Quiesce
        );
        // A child (the monitor) leaving D0 does not.
        assert_eq!(power_vsync(0, false, true), PowerVsync::Leave);
        assert_eq!(power_vsync(1, false, true), PowerVsync::Leave);
        // D0 of any uid re-arms with the display half up (as before: arming is idempotent).
        assert_eq!(
            power_vsync(DISPLAY_ADAPTER_HW_ID, true, true),
            PowerVsync::Resume
        );
        assert_eq!(power_vsync(0, true, true), PowerVsync::Resume);
        // Without the display half D0 does nothing, as before.
        assert_eq!(power_vsync(DISPLAY_ADAPTER_HW_ID, true, false), PowerVsync::Leave);
        // The render-only adapter leaving D0 still quiesces (a no-op there), as before.
        assert_eq!(
            power_vsync(DISPLAY_ADAPTER_HW_ID, false, false),
            PowerVsync::Quiesce
        );
    }
}
