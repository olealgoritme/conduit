//! Decisions on the `HELIOS_ESCAPE_NVRM` FORWARD hot path that are pure functions
//! of their arguments, so the host can test them.
//!
//! Two of them:
//!
//! * [`spin`] — whether a forward should poll for its reply for a few
//!   microseconds before it blocks, and how that decision adapts to how the host
//!   has been answering. The KMD keeps the packed state in one `AtomicU32` and
//!   applies [`spin::next`] with plain load / store (a lost update is harmless:
//!   the state is a heuristic, never a correctness input).
//! * [`publish_gate`] — when the `Nv*` registry counters are mirrored. The
//!   mirror is a few dozen synchronous registry writes: the escape only asks for
//!   it (loads, and one swap per request), a PASSIVE worker does it, and this
//!   gate keeps the worker to a few mirrors a second.
//!
//! No wdk, no atomics, no clocks: the caller supplies every input.

/// Adaptive pre-wait spin governor.
///
/// A forward whose reply arrives within a few tens of microseconds is cheaper to
/// poll for than to sleep on: blocking means a timer arm, a context switch, and,
/// on an idle vCPU, a halt exit plus the wake-up of the halted vCPU. But a spin is
/// pure waste when the host answers in milliseconds, so the spin is a credit
/// scheme. Each spin that saw the reply in time earns [`HIT_GAIN`] credit (up to
/// [`CREDIT_MAX`]); each that did not loses [`MISS_LOSS`]. With credit the next
/// forward spins; with none it only spins on every [`PROBE_EVERY`]th call, to
/// notice that the host got faster. With `HIT_GAIN = 2` and `MISS_LOSS = 4` the
/// credit rises only while more than two thirds of the spins hit, which is the
/// break-even for a spin that costs its whole budget when it misses.
///
/// State layout (one `u32`): bits 0..8 credit, bits 8..32 a call tick.
pub mod spin {
    /// Credit ceiling. A long run of hits can be paid back by this many misses.
    pub const CREDIT_MAX: u32 = 16;
    /// Credit a spin that saw the reply gains.
    pub const HIT_GAIN: u32 = 2;
    /// Credit a spin that gave up loses.
    pub const MISS_LOSS: u32 = 4;
    /// With no credit, one call in this many spins anyway. A power of two.
    pub const PROBE_EVERY: u32 = 64;
    /// Credit a fresh boot starts with: enough for the first calls to try.
    pub const CREDIT_START: u32 = 4;
    /// Longest budget the knob is honoured for, microseconds. Beyond this a
    /// spin starts to look like a stolen CPU rather than a latency trick.
    pub const BUDGET_US_MAX: u32 = 200;
    /// Budget when the service-key knob is absent, microseconds.
    pub const BUDGET_US_DEFAULT: u32 = 50;

    const CREDIT_MASK: u32 = 0xFF;
    const TICK_SHIFT: u32 = 8;
    const TICK_MASK: u32 = 0x00FF_FFFF;

    /// The state a fresh boot starts from.
    pub const fn initial() -> u32 {
        CREDIT_START
    }

    /// Credit held in `state`.
    pub const fn credit(state: u32) -> u32 {
        state & CREDIT_MASK
    }

    /// Whether the call that sees `state` should spin before blocking.
    pub const fn should_spin(state: u32) -> bool {
        credit(state) != 0 || (state >> TICK_SHIFT) & (PROBE_EVERY - 1) == 0
    }

    /// The state after a call. `spun` says whether it spun, `hit` whether the
    /// reply showed up inside the spin (meaningless when `spun` is false).
    /// The tick advances on every call, so a call that did not spin still
    /// brings the next probe closer.
    pub const fn next(state: u32, spun: bool, hit: bool) -> u32 {
        let tick = ((state >> TICK_SHIFT) + 1) & TICK_MASK;
        let mut credit = credit(state);
        if spun {
            credit = if hit {
                let c = credit + HIT_GAIN;
                if c > CREDIT_MAX {
                    CREDIT_MAX
                } else {
                    c
                }
            } else if credit > MISS_LOSS {
                credit - MISS_LOSS
            } else {
                0
            };
        }
        (tick << TICK_SHIFT) | credit
    }

    /// The spin budget in 100 ns units for a service-key value in microseconds
    /// (`0` disables the spin). Clamped to [`BUDGET_US_MAX`].
    pub const fn budget_100ns(knob_us: u32) -> u64 {
        let us = if knob_us > BUDGET_US_MAX {
            BUDGET_US_MAX
        } else {
            knob_us
        };
        us as u64 * 10
    }
}

/// When the `Nv*` registry counters are mirrored.
///
/// The old rule was "when a session-shaping count moved, or every 256th call",
/// run inside the escape. A pin / unpin pair per buffer made the first half a
/// registry-write storm, and the second half is one call in 256 that stalls for
/// the whole mirror. The new rule keeps both triggers as CANDIDATES, adds a
/// minimum interval, and the candidate test is plain loads: no locked
/// read-modify-write on any shared line.
pub mod publish_gate {
    /// Fewest 100 ns units between two mirrors: 250 ms.
    pub const MIN_INTERVAL_100NS: u64 = 2_500_000;
    /// `calls >> BUCKET_SHIFT` changes every 256 calls.
    pub const BUCKET_SHIFT: u32 = 8;

    /// Which 256-call bucket `calls` is in.
    pub const fn bucket(calls: u32) -> u32 {
        calls >> BUCKET_SHIFT
    }

    /// Whether a mirror is wanted at all: the session shape moved, or the call
    /// count crossed into a new bucket since the last mirror. Cheap, no clock.
    pub const fn candidate(shape: u32, last_shape: u32, calls: u32, last_bucket: u32) -> bool {
        shape != last_shape || bucket(calls) != last_bucket
    }

    /// Whether enough time has passed since the last mirror (`0` = never
    /// mirrored). A clock that went backwards counts as elapsed, so the gate can
    /// never stay shut.
    pub const fn interval_elapsed(now_100ns: u64, last_100ns: u64) -> bool {
        last_100ns == 0
            || now_100ns < last_100ns
            || now_100ns - last_100ns >= MIN_INTERVAL_100NS
    }
}

#[cfg(test)]
mod tests {
    use super::publish_gate as pg;
    use super::spin;

    #[test]
    fn fresh_state_spins() {
        assert!(spin::should_spin(spin::initial()));
        assert_eq!(spin::credit(spin::initial()), spin::CREDIT_START);
    }

    #[test]
    fn hits_earn_credit_up_to_the_ceiling() {
        let mut s = spin::initial();
        for _ in 0..100 {
            s = spin::next(s, true, true);
        }
        assert_eq!(spin::credit(s), spin::CREDIT_MAX);
    }

    #[test]
    fn misses_drain_credit_to_zero_and_no_further() {
        let mut s = spin::next(0, true, true); // credit 2
        s = spin::next(s, true, false);
        assert_eq!(spin::credit(s), 0);
        s = spin::next(s, true, false);
        assert_eq!(spin::credit(s), 0);
    }

    #[test]
    fn a_miss_costs_more_than_a_hit_earns() {
        // 2/3 hits is break-even; 1 hit in 2 must lose credit overall.
        let mut s = spin::next(0, true, true);
        s = spin::next(s, true, true);
        s = spin::next(s, true, true); // credit 6
        let before = spin::credit(s);
        for _ in 0..10 {
            s = spin::next(s, true, true);
            s = spin::next(s, true, false);
        }
        assert!(spin::credit(s) < before);
    }

    #[test]
    fn two_thirds_hits_do_not_lose_credit() {
        let mut s = spin::next(0, true, true); // credit 2
        for _ in 0..50 {
            s = spin::next(s, true, true);
            s = spin::next(s, true, true);
            s = spin::next(s, true, false);
        }
        // Net per cycle: +2 +2 -4 = 0 once away from the ceiling.
        assert!(spin::credit(s) <= spin::CREDIT_MAX);
        assert!(spin::credit(s) >= 2);
    }

    #[test]
    fn without_credit_only_every_probe_every_th_call_spins() {
        let mut s = 0u32; // credit 0, tick 0: the tick-0 call probes
        let mut spun = 0u32;
        for _ in 0..(spin::PROBE_EVERY * 10) {
            if spin::should_spin(s) {
                spun += 1;
                s = spin::next(s, true, false);
            } else {
                s = spin::next(s, false, false);
            }
        }
        assert_eq!(spun, 10);
    }

    #[test]
    fn a_probe_hit_turns_the_spin_back_on() {
        let mut s = 0u32;
        assert!(spin::should_spin(s)); // tick 0
        s = spin::next(s, true, true);
        assert_eq!(spin::credit(s), spin::HIT_GAIN);
        assert!(spin::should_spin(s));
    }

    #[test]
    fn a_non_spinning_call_leaves_credit_alone() {
        let s = spin::next(spin::initial(), false, true);
        assert_eq!(spin::credit(s), spin::CREDIT_START);
    }

    #[test]
    fn tick_wraps_inside_its_field_and_never_touches_credit() {
        let s = (0x00FF_FFFFu32 << 8) | 7;
        let n = spin::next(s, false, false);
        assert_eq!(n >> 8, 0);
        assert_eq!(spin::credit(n), 7);
    }

    #[test]
    fn budget_is_clamped_and_zero_disables() {
        assert_eq!(spin::budget_100ns(0), 0);
        assert_eq!(spin::budget_100ns(50), 500);
        assert_eq!(spin::budget_100ns(10_000), spin::BUDGET_US_MAX as u64 * 10);
        assert_eq!(spin::budget_100ns(u32::MAX), spin::BUDGET_US_MAX as u64 * 10);
    }

    #[test]
    fn no_candidate_while_nothing_moved() {
        assert!(!pg::candidate(5, 5, 255, 0));
        assert!(!pg::candidate(5, 5, 256 * 7 + 3, 7));
    }

    #[test]
    fn shape_change_or_new_bucket_is_a_candidate() {
        assert!(pg::candidate(6, 5, 0, 0));
        assert!(pg::candidate(5, 5, 256, 0));
        // The call counter wrapping is a bucket change like any other.
        assert!(pg::candidate(5, 5, 0, u32::MAX >> pg::BUCKET_SHIFT));
    }

    #[test]
    fn first_mirror_is_never_gated() {
        assert!(pg::interval_elapsed(1, 0));
        assert!(pg::interval_elapsed(0, 0));
    }

    #[test]
    fn interval_gates_a_quick_second_mirror() {
        let t0 = 10_000_000u64;
        assert!(!pg::interval_elapsed(t0 + 1, t0));
        assert!(!pg::interval_elapsed(t0 + pg::MIN_INTERVAL_100NS - 1, t0));
        assert!(pg::interval_elapsed(t0 + pg::MIN_INTERVAL_100NS, t0));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_shut_the_gate() {
        assert!(pg::interval_elapsed(5, 10_000_000));
    }

    #[test]
    fn a_pin_storm_mirrors_at_most_four_times_a_second() {
        // One shape change per call, 1 ms apart, for ten seconds.
        let mut last_shape = 0u32;
        let mut last_pub = 0u64;
        let mut mirrors = 0u32;
        for i in 1..=10_000u32 {
            let now = 10_000_000u64 + u64::from(i) * 10_000; // 1 ms in 100 ns
            if pg::candidate(i, last_shape, i, pg::bucket(i)) && pg::interval_elapsed(now, last_pub)
            {
                mirrors += 1;
                last_shape = i;
                last_pub = now;
            }
        }
        assert!(mirrors <= 41, "{mirrors}");
        assert!(mirrors >= 39, "{mirrors}");
    }
}
