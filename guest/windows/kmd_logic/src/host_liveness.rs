//! Whether the host backend still answers raw RM round trips.
//!
//! `ctrl::raw_roundtrip` carries every RM message to the host. When the backend
//! process is gone (OOM-killed, crashed) nothing answers: each round trip waits its
//! full timeout, the abandoned descriptors stay on the ring until it is full, and
//! then every later call sleeps through the enqueue retry budget as well. A thread
//! that holds a lock across such calls keeps it for minutes.
//!
//! This state machine declares the host unresponsive after [`TRIP_AFTER`]
//! consecutive round trips timed out with no reply from ANY round trip during their
//! wait, and from then on refuses new round trips at once (the caller fails with a
//! timeout), letting one probe through every [`PROBE_INTERVAL_100NS`]. The first
//! reply clears it.
//!
//! Short caller timeouts (a UMD polling with a few milliseconds) and timeouts that
//! overlapped a reply to someone else (a host that is alive but slow on one call)
//! never count. Times are in 100 ns units of one monotonic clock.

/// Consecutive qualifying timeouts that declare the host unresponsive.
pub const TRIP_AFTER: u32 = 2;
/// A timeout only counts when the caller was willing to wait at least this long.
pub const MIN_STRIKE_TIMEOUT_MS: u64 = 1_000;
/// While unresponsive, one round trip per interval is let through as a probe.
pub const PROBE_INTERVAL_100NS: u64 = 20_000_000;
/// The wait a probe is given, at most (its caller's own timeout if shorter).
pub const PROBE_TIMEOUT_MS: u64 = 2_000;

/// What a new round trip may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// The host answers: send with the caller's timeout.
    Go,
    /// The host is unresponsive and this call is the interval's probe: send with at
    /// most [`PROBE_TIMEOUT_MS`].
    Probe,
    /// The host is unresponsive: fail at once without touching the ring.
    Refuse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Liveness {
    strikes: u32,
    last_probe: u64,
    /// When the last reply arrived (0 = none yet).
    last_reply: u64,
}

impl Liveness {
    pub const fn new() -> Self {
        Self {
            strikes: 0,
            last_probe: 0,
            last_reply: 0,
        }
    }

    pub const fn unresponsive(&self) -> bool {
        self.strikes >= TRIP_AFTER
    }

    pub const fn strikes(&self) -> u32 {
        self.strikes
    }

    /// Decide for a round trip starting at `now`.
    pub fn admit(&mut self, now: u64) -> Admit {
        if !self.unresponsive() {
            return Admit::Go;
        }
        if now.saturating_sub(self.last_probe) >= PROBE_INTERVAL_100NS {
            self.last_probe = now;
            Admit::Probe
        } else {
            Admit::Refuse
        }
    }

    /// A reply arrived at `now`. `true` when this ends an unresponsive period.
    pub fn on_reply(&mut self, now: u64) -> bool {
        let was = self.unresponsive();
        self.strikes = 0;
        self.last_reply = self.last_reply.max(now);
        was
    }

    /// A round trip that started at `start` with `timeout_ms` got no reply by `now`
    /// (a timed-out wait, or an enqueue that found the ring full for its whole
    /// budget). `true` when this declares the host unresponsive.
    pub fn on_timeout(&mut self, start: u64, now: u64, timeout_ms: u64) -> bool {
        if timeout_ms < MIN_STRIKE_TIMEOUT_MS || self.last_reply >= start {
            return false;
        }
        let was = self.unresponsive();
        self.strikes = self.strikes.saturating_add(1);
        if !was && self.unresponsive() {
            // The first probe comes one interval after the trip.
            self.last_probe = now;
            return true;
        }
        false
    }
}

/// The wait a round trip is given for an [`Admit`] verdict.
pub const fn wait_ms(admit: Admit, timeout_ms: u64) -> u64 {
    match admit {
        Admit::Go => timeout_ms,
        Admit::Probe => {
            if timeout_ms < PROBE_TIMEOUT_MS {
                timeout_ms
            } else {
                PROBE_TIMEOUT_MS
            }
        }
        Admit::Refuse => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 10_000;

    #[test]
    fn two_silent_timeouts_trip_and_a_reply_clears() {
        let mut l = Liveness::new();
        assert_eq!(l.admit(0), Admit::Go);
        assert!(!l.on_timeout(10, 2_510 * MS, 2_500));
        assert_eq!(l.admit(2_510 * MS), Admit::Go);
        assert!(l.on_timeout(2_510 * MS, 5_010 * MS, 2_500));
        assert!(l.unresponsive());
        // Refused until one interval after the trip, then one probe per interval.
        assert_eq!(l.admit(5_020 * MS), Admit::Refuse);
        assert_eq!(l.admit(5_010 * MS + PROBE_INTERVAL_100NS), Admit::Probe);
        assert_eq!(l.admit(5_010 * MS + PROBE_INTERVAL_100NS + 1), Admit::Refuse);
        // A probe that times out keeps it tripped (and does not re-trip).
        assert!(!l.on_timeout(7_010 * MS, 9_010 * MS, 2_000));
        assert!(l.unresponsive());
        assert!(l.on_reply(9_500 * MS));
        assert!(!l.unresponsive());
        assert_eq!(l.admit(9_600 * MS), Admit::Go);
    }

    #[test]
    fn short_waits_and_timeouts_overlapping_a_reply_never_count() {
        let mut l = Liveness::new();
        for i in 0..10u64 {
            assert!(!l.on_timeout(i * MS, (i + 5) * MS, 5));
        }
        assert_eq!(l.strikes(), 0);
        // Someone got a reply while this call waited: the host is alive, only slow.
        l.on_reply(1_000 * MS);
        assert!(!l.on_timeout(500 * MS, 30_500 * MS, 30_000));
        assert!(!l.on_timeout(900 * MS, 30_900 * MS, 30_000));
        assert_eq!(l.strikes(), 0);
        // A reply between two silent timeouts resets the count.
        assert!(!l.on_timeout(31_000 * MS, 61_000 * MS, 30_000));
        l.on_reply(61_500 * MS);
        assert!(!l.on_timeout(62_000 * MS, 92_000 * MS, 30_000));
        assert!(!l.unresponsive());
    }

    #[test]
    fn probe_waits_are_capped() {
        assert_eq!(wait_ms(Admit::Go, 30_000), 30_000);
        assert_eq!(wait_ms(Admit::Probe, 30_000), PROBE_TIMEOUT_MS);
        assert_eq!(wait_ms(Admit::Probe, 500), 500);
        assert_eq!(wait_ms(Admit::Refuse, 30_000), 0);
    }
}
