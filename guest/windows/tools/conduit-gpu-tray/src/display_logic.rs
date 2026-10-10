//! Following the host viewer's mode requests (docs/SCANOUT.md "Mode list").
//!
//! The KMD publishes the newest `DisplayMode` request as one QWORD of its
//! service key, `HostMode`: the sequence (per-boot nonce << 16 | count) in the
//! high half, `width << 16 | height` in the low half; 0 = none this boot
//! (written at StartDevice). One value, so a size and a sequence are never
//! read from two different requests.
//! The tray polls them and applies a new request with
//! `ChangeDisplaySettingsEx` in the user's session; this is the decision part.

/// `width << 16 | height` -> (width, height).
pub fn unpack(p: u32) -> (u32, u32) {
    (p >> 16, p & 0xffff)
}

/// What to do with a reading of the two values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing new.
    Idle,
    /// Switch the Conduit display to this size.
    Apply(u32, u32),
}

/// The follower's state: the sequence last acted on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Follower {
    last: Option<u32>,
}

impl Follower {
    /// One reading of `HostMode` (`None`: it could not be read, which says
    /// nothing and changes nothing). A request is applied once: the first
    /// reading applies one made this boot before the tray started (sequence
    /// != 0), later readings only a changed sequence.
    pub fn step(&mut self, value: Option<u64>) -> Step {
        let Some(v) = value else {
            return Step::Idle;
        };
        let seq = (v >> 32) as u32;
        if self.last == Some(seq) {
            return Step::Idle;
        }
        self.last = Some(seq);
        if seq == 0 {
            return Step::Idle;
        }
        match unpack(v as u32) {
            (w, h) if w >= 64 && h >= 64 => Step::Apply(w, h),
            _ => Step::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R1920: u32 = 1920 << 16 | 1080;
    const R1280: u32 = 1280 << 16 | 960;

    fn v(seq: u32, req: u32) -> Option<u64> {
        Some((seq as u64) << 32 | req as u64)
    }

    #[test]
    fn a_request_is_applied_once() {
        let mut f = Follower::default();
        assert_eq!(f.step(v(0x1234_0001, R1920)), Step::Apply(1920, 1080));
        assert_eq!(f.step(v(0x1234_0001, R1920)), Step::Idle);
        assert_eq!(f.step(v(0x1234_0002, R1280)), Step::Apply(1280, 960));
        // The same size asked again is a new request (the user switched in
        // Windows meanwhile): applied again.
        assert_eq!(f.step(v(0x1234_0003, R1280)), Step::Apply(1280, 960));
    }

    #[test]
    fn nothing_this_boot_is_nothing_to_do() {
        let mut f = Follower::default();
        assert_eq!(f.step(v(0, R1920)), Step::Idle, "reset at StartDevice");
        assert_eq!(f.step(Some(0)), Step::Idle);
        assert_eq!(f.step(v(0x0001_0001, R1920)), Step::Apply(1920, 1080));
    }

    #[test]
    fn a_failed_read_does_not_replay_the_last_request() {
        let mut f = Follower::default();
        assert_eq!(f.step(v(0x0002_0001, R1920)), Step::Apply(1920, 1080));
        assert_eq!(f.step(None), Step::Idle);
        assert_eq!(f.step(v(0x0002_0001, R1920)), Step::Idle);
    }

    #[test]
    fn nonsense_sizes_are_ignored() {
        let mut f = Follower::default();
        assert_eq!(f.step(v(1, 0)), Step::Idle);
        assert_eq!(f.step(v(3, 10 << 16 | 10)), Step::Idle);
    }

    #[test]
    fn unpacks() {
        assert_eq!(unpack(R1920), (1920, 1080));
        assert_eq!(unpack(5120 << 16 | 1440), (5120, 1440));
    }
}
