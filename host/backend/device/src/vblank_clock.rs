//! The host display's vblank lattice, learned from a display client's
//! presentation reports (`EV_PRESENTED`), and the pacer that turns it into one
//! `HostVblank` event per host vblank for the guest (docs/SCANOUT.md
//! "Host vblank").
//!
//! A presentation report carries the instant the compositor put a frame on
//! the screen (`wp_presentation_feedback.presented`, from the DRM page-flip
//! event: microsecond-exact when the `VSYNC` kind bit is set). Reports only
//! exist while frames flow, and a guest at 200 fps on a 240 Hz output is
//! presented on some vblanks and not others, so the clock is a lattice
//! `anchor + n * period` fitted to the reports (a phase-locked loop), not the
//! reports themselves. Between reports, and for a while after the last one
//! (`HOLD_NS`), it extrapolates: the display keeps scanning out whether or
//! not anything was committed.
//!
//! Pure: the I/O (the client's reports in, the event out) is
//! [`crate::display::DisplayLink`]'s.

use std::time::Duration;

/// `wp_presentation_feedback` kind bit: the presentation was synchronised to
/// a vblank. Without it a report says nothing about the vblank (a tearing,
/// direct-mode commit) and is not used.
pub const KIND_VSYNC: u32 = 0x1;

/// How long the lattice is trusted after the last accepted report. The guest
/// keeps the host's phase on its own timer after the events stop, so this
/// only bounds how long the host keeps sending without fresh evidence.
pub const HOLD_NS: u64 = 10_000_000_000;
/// A gap this long between reports re-anchors the phase on the next report
/// (the period is kept).
const GAP_NS: u64 = 2_000_000_000;
/// Consecutive accepted reports before the lattice is used.
const LOCK_AFTER: u32 = 8;
/// Phase gain per report (reports are exact: a high gain is safe).
const KP: f64 = 0.25;
/// Period gain per report, per lattice period between reports.
const KI: f64 = 0.05;
/// A report this far off the lattice (fraction of a period) is an outlier.
const OUTLIER_FRAC: f64 = 0.25;
/// Outliers in a row that agree with each other re-seed the lattice (an
/// output change, a compositor restart, a mode change).
const STEP_AFTER: u32 = 3;
/// The period may not leave the nominal one by more than this fraction.
const BAND: f64 = 0.02;
/// Shortest and longest periods accepted at all (1000 Hz .. 20 Hz).
const MIN_PERIOD_NS: f64 = 1_000_000.0;
const MAX_PERIOD_NS: f64 = 50_000_000.0;

/// What became of one report (counters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    /// Used.
    Accepted,
    /// No `VSYNC` bit, or no timestamp.
    NotVsync,
    /// At or before the previous report, or the same vblank.
    Duplicate,
    /// Too far off the lattice; ignored.
    Outlier,
    /// Started (or restarted) the lattice from this report.
    Seeded,
}

#[derive(Clone, Debug, Default)]
pub struct VblankClock {
    /// The output's nominal period from the client's refresh, 0 unknown.
    nominal_ns: f64,
    /// The fitted period; 0 until known.
    period_ns: f64,
    /// A lattice point (ns) and its sub-nanosecond part.
    anchor_ns: u64,
    anchor_frac: f64,
    seeded: bool,
    good: u32,
    locked: bool,
    last_ns: u64,
    /// Without a nominal period: the shortest report interval seen while
    /// seeding (the period is at most that).
    min_gap_ns: u64,
    /// The previous outlier (for the step rule) and the run length.
    outlier_ns: u64,
    outliers: u32,
    /// Counters.
    pub accepted: u64,
    pub rejected: u64,
    pub steps: u64,
}

impl VblankClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// The client's refresh in mHz (0 unknown). A change of more than the
    /// band re-seeds: the output or its mode changed.
    pub fn set_refresh_mhz(&mut self, mhz: u32) {
        let nominal = if mhz == 0 { 0.0 } else { 1e12 / mhz as f64 };
        if !(MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&nominal) {
            self.nominal_ns = 0.0;
            return;
        }
        let changed = self.nominal_ns == 0.0 || ((nominal / self.nominal_ns) - 1.0).abs() > BAND;
        self.nominal_ns = nominal;
        if changed && self.seeded {
            self.reset_keep_nominal();
        }
    }

    pub fn nominal_ns(&self) -> f64 {
        self.nominal_ns
    }

    fn reset_keep_nominal(&mut self) {
        let nominal = self.nominal_ns;
        let (a, r, s) = (self.accepted, self.rejected, self.steps);
        *self = Self {
            nominal_ns: nominal,
            accepted: a,
            rejected: r,
            steps: s,
            ..Self::default()
        };
    }

    /// Forget the lattice (client gone, output changed).
    pub fn reset(&mut self) {
        self.reset_keep_nominal();
    }

    fn seed(&mut self, t: u64) {
        self.anchor_ns = t;
        self.anchor_frac = 0.0;
        self.last_ns = t;
        self.seeded = true;
        self.good = 0;
        self.locked = false;
        self.outliers = 0;
        if self.period_ns == 0.0 && self.nominal_ns != 0.0 {
            self.period_ns = self.nominal_ns;
        }
    }

    fn clamp_period(&self, p: f64) -> f64 {
        let p = p.clamp(MIN_PERIOD_NS, MAX_PERIOD_NS);
        if self.nominal_ns != 0.0 {
            p.clamp(
                self.nominal_ns * (1.0 - BAND),
                self.nominal_ns * (1.0 + BAND),
            )
        } else {
            p
        }
    }

    /// One presentation report: the frame reached the screen at `t_ns`
    /// (host `CLOCK_MONOTONIC`), with the `wp_presentation_feedback` kind bits.
    pub fn sample(&mut self, t_ns: u64, kind: u32) -> Sample {
        if kind & KIND_VSYNC == 0 || t_ns == 0 {
            self.rejected += 1;
            return Sample::NotVsync;
        }
        if !self.seeded {
            self.seed(t_ns);
            self.min_gap_ns = 0;
            return Sample::Seeded;
        }
        if t_ns <= self.last_ns {
            self.rejected += 1;
            return Sample::Duplicate;
        }
        let since_last = t_ns - self.last_ns;
        if since_last < (MIN_PERIOD_NS as u64) / 2 {
            self.rejected += 1;
            return Sample::Duplicate;
        }
        if self.period_ns == 0.0 {
            // No nominal period: the shortest interval over a few reports is
            // one period (a client presents at most once per vblank).
            if self.min_gap_ns == 0 || since_last < self.min_gap_ns {
                self.min_gap_ns = since_last;
            }
            self.last_ns = t_ns;
            self.good += 1;
            if self.good >= LOCK_AFTER {
                let p = self.min_gap_ns as f64;
                if (MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&p) {
                    self.period_ns = p;
                    self.anchor_ns = t_ns;
                    self.anchor_frac = 0.0;
                    self.good = 0;
                }
            }
            return Sample::Seeded;
        }
        if since_last > GAP_NS {
            // The phase is re-taken from this exact report; the period stays.
            let locked = self.locked;
            self.seed(t_ns);
            self.good = if locked { LOCK_AFTER } else { 0 };
            self.locked = locked;
            return Sample::Seeded;
        }
        // Signed: the anchor may sit a fraction of a period after the last report, and a bogus
        // early report must not underflow. True anchor = anchor_ns - anchor_frac.
        let elapsed = (t_ns as i64).wrapping_sub(self.anchor_ns as i64) as f64 + self.anchor_frac;
        let n = (elapsed / self.period_ns).round();
        if n < 1.0 {
            self.rejected += 1;
            return Sample::Duplicate;
        }
        let e = elapsed - n * self.period_ns;
        if e.abs() > self.period_ns * OUTLIER_FRAC {
            self.rejected += 1;
            // Outliers that agree with each other (an exact number of
            // periods apart, within the outlier band) are a phase step.
            let agrees = self.outliers > 0 && {
                let d = (t_ns - self.outlier_ns) as f64;
                let m = (d / self.period_ns).round();
                m >= 1.0 && (d - m * self.period_ns).abs() <= self.period_ns * OUTLIER_FRAC / 2.0
            };
            self.outliers = if agrees { self.outliers + 1 } else { 1 };
            self.outlier_ns = t_ns;
            if self.outliers >= STEP_AFTER {
                // A phase step (output re-mode, compositor restart): the period is still
                // good, so the lattice moves to the new phase and stays locked.
                self.steps += 1;
                let locked = self.locked;
                self.seed(t_ns);
                self.good = if locked { LOCK_AFTER } else { 0 };
                self.locked = locked;
                return Sample::Seeded;
            }
            return Sample::Outlier;
        }
        self.outliers = 0;
        // anchor' = predicted lattice point + KP * e, expressed as t - (1-KP)*e.
        let back = (1.0 - KP) * e; // ns before t (may be negative)
        let new_anchor = t_ns as f64 - back;
        let whole = new_anchor.floor();
        self.anchor_ns = whole as u64;
        // The true anchor is `anchor_ns - anchor_frac`.
        self.anchor_frac = whole - new_anchor;
        self.period_ns = self.clamp_period(self.period_ns + KI * e / n);
        self.last_ns = t_ns;
        self.accepted += 1;
        self.good = self.good.saturating_add(1);
        if self.good >= LOCK_AFTER {
            self.locked = true;
        }
        Sample::Accepted
    }

    /// The lattice is fitted and fresh at `now_ns`.
    pub fn is_live(&self, now_ns: u64) -> bool {
        self.locked && self.period_ns != 0.0 && now_ns.saturating_sub(self.last_ns) <= HOLD_NS
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// The fitted period in ns (0 unknown).
    pub fn period_ns(&self) -> f64 {
        self.period_ns
    }

    pub fn last_report_ns(&self) -> u64 {
        self.last_ns
    }

    /// The first vblank strictly after `now_ns`, while live.
    pub fn next_after(&self, now_ns: u64) -> Option<u64> {
        if !self.is_live(now_ns) {
            return None;
        }
        let anchor = self.anchor_ns as f64 - self.anchor_frac;
        let rel = now_ns as f64 - anchor;
        let mut k = (rel / self.period_ns).floor() + 1.0;
        let mut t = anchor + k * self.period_ns;
        if t <= now_ns as f64 {
            k += 1.0;
            t = anchor + k * self.period_ns;
        }
        if !(t.is_finite()) || t < 0.0 {
            return None;
        }
        Some(t.round() as u64)
    }
}

/// The pacer's lead: the event leaves this long before the vblank it stands
/// for (the guest's vsync then lands at the vblank plus the delivery latency
/// minus the lead). From `CONDUIT_VBLANK_LEAD_US`, default 0, at most 4 ms.
pub fn lead_from_env() -> Duration {
    std::env::var("CONDUIT_VBLANK_LEAD_US")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|us| Duration::from_micros(us.min(4_000)))
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P240: f64 = 1e9 / 239.96;

    fn lattice(i: u64, phase: f64, period: f64) -> u64 {
        (phase + i as f64 * period).round() as u64
    }

    #[test]
    fn locks_on_exact_reports_and_predicts_the_next_vblank() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000); // nominal 240.000, display really 239.96
        let base = 5_000_000_000.0;
        for i in 0..200u64 {
            c.sample(lattice(i, base, P240), KIND_VSYNC);
        }
        assert!(c.is_locked());
        assert!(
            (c.period_ns() - P240).abs() < 5.0,
            "period {}",
            c.period_ns()
        );
        let now = lattice(199, base, P240) + 1_000_000;
        let next = c.next_after(now).unwrap();
        let want = lattice(200, base, P240);
        assert!(next.abs_diff(want) < 2_000, "{next} vs {want}");
    }

    #[test]
    fn follows_a_client_presented_on_some_vblanks_only() {
        // A 200 fps guest on a 240 Hz output: shown on 5 of every 6 vblanks.
        let mut c = VblankClock::new();
        c.set_refresh_mhz(239_960);
        let base = 1_000_000_000.0;
        let mut i = 0u64;
        for k in 0..600u64 {
            if k % 6 != 3 {
                c.sample(lattice(k, base, P240), KIND_VSYNC);
                i += 1;
            }
        }
        assert!(i > 400 && c.is_locked());
        let next = c.next_after(lattice(599, base, P240) + 10).unwrap();
        assert!(next.abs_diff(lattice(600, base, P240)) < 1_000);
    }

    #[test]
    fn jitter_and_spikes_do_not_move_the_lattice_far() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000);
        let base = 2_000_000_000.0;
        let mut x: u64 = 12345;
        for k in 0..2000u64 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = ((x >> 33) % 40) as f64 - 20.0; // +-20 us of report jitter
            let spike = if k % 97 == 0 { 1_500_000.0 } else { 0.0 };
            c.sample(
                (base + k as f64 * P240 + j * 1000.0 + spike) as u64,
                KIND_VSYNC,
            );
        }
        // The lattice sits a few microseconds off the true one, so ask 1 ms past a vblank.
        let next = c.next_after(lattice(1999, base, P240) + 1_000_000).unwrap();
        assert!(
            next.abs_diff(lattice(2000, base, P240)) < 30_000,
            "{}",
            next as i64 - lattice(2000, base, P240) as i64
        );
    }

    #[test]
    fn non_vsync_reports_are_ignored_and_never_lock() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000);
        for k in 0..100u64 {
            assert_eq!(c.sample(1_000_000 + k * 3_000_000, 0), Sample::NotVsync);
        }
        assert!(!c.is_locked());
        assert_eq!(c.next_after(10), None);
    }

    #[test]
    fn a_phase_step_reseeds_and_relocks() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000);
        let p = 1e9 / 240.0;
        for k in 0..50u64 {
            c.sample(lattice(k, 1e9, p), KIND_VSYNC);
        }
        assert!(c.is_locked());
        // The output re-modes: the phase jumps by a third of a period.
        let base2 = 1e9 + 50.0 * p + p / 3.0;
        for k in 0..50u64 {
            c.sample(lattice(k, base2, p), KIND_VSYNC);
        }
        assert!(c.steps >= 1);
        assert!(c.is_locked());
        let next = c.next_after(lattice(49, base2, p) + 10).unwrap();
        assert!(next.abs_diff(lattice(50, base2, p)) < 2_000);
    }

    #[test]
    fn goes_stale_after_the_hold_and_reanchors_after_a_gap() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000);
        let p = 1e9 / 240.0;
        for k in 0..20u64 {
            c.sample(lattice(k, 1e9, p), KIND_VSYNC);
        }
        let last = lattice(19, 1e9, p);
        assert!(c.next_after(last + HOLD_NS - 1).is_some());
        assert!(c.next_after(last + HOLD_NS + 1).is_none());
        // 3 s later the display shows a frame at a phase that drifted: the
        // report re-anchors at once, the lock is kept.
        let t = last + 3_000_000_000 + 777_000;
        assert_eq!(c.sample(t, KIND_VSYNC), Sample::Seeded);
        assert!(c.is_locked());
        let next = c.next_after(t + 10).unwrap();
        assert!(next.abs_diff(t + p as u64) < 2_000);
    }

    #[test]
    fn learns_the_period_without_a_refresh_hint() {
        let mut c = VblankClock::new();
        let p = 1e9 / 144.0;
        for k in 0..100u64 {
            if k % 3 != 1 {
                c.sample(lattice(k, 3e9, p), KIND_VSYNC);
            }
        }
        assert!(c.is_locked(), "period {}", c.period_ns());
        assert!((c.period_ns() - p).abs() < 50.0);
    }

    #[test]
    fn a_refresh_change_reseeds() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(240_000);
        for k in 0..20u64 {
            c.sample(lattice(k, 1e9, 1e9 / 240.0), KIND_VSYNC);
        }
        assert!(c.is_locked());
        c.set_refresh_mhz(60_000);
        assert!(!c.is_locked());
        c.set_refresh_mhz(60_010); // within the band: no reseed
        for k in 0..20u64 {
            c.sample(lattice(k, 2e9, 1e9 / 60.0), KIND_VSYNC);
        }
        assert!(c.is_locked());
        assert!((c.period_ns() - 1e9 / 60.0).abs() < 1_000.0);
    }

    #[test]
    fn next_is_strictly_after_now_even_on_a_lattice_point() {
        let mut c = VblankClock::new();
        c.set_refresh_mhz(250_000);
        for k in 0..20u64 {
            c.sample(1_000_000_000 + k * 4_000_000, KIND_VSYNC);
        }
        let on = 1_000_000_000 + 25 * 4_000_000;
        let n = c.next_after(on).unwrap();
        assert!(n > on && n <= on + 4_000_100, "{n}");
    }
}
