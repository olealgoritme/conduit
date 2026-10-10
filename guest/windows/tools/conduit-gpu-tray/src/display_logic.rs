//! Following the host viewer's mode requests (docs/SCANOUT.md "Mode list").
//!
//! The KMD publishes the newest `DisplayMode` request as two values of its
//! service key: `HostModeReq` (`width << 16 | height`) and `HostModeSeq`
//! (per-boot nonce << 16 | count; 0 = none this boot, written at StartDevice).
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
    /// One reading (`None`: the value is absent). A request is applied once:
    /// the first reading applies one made this boot before the tray started
    /// (seq != 0), later readings only a changed sequence.
    pub fn step(&mut self, seq: Option<u32>, req: Option<u32>) -> Step {
        let seq = seq.unwrap_or(0);
        if self.last == Some(seq) {
            return Step::Idle;
        }
        self.last = Some(seq);
        if seq == 0 {
            return Step::Idle;
        }
        match req.map(unpack) {
            Some((w, h)) if w >= 64 && h >= 64 => Step::Apply(w, h),
            _ => Step::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R1920: u32 = 1920 << 16 | 1080;
    const R1280: u32 = 1280 << 16 | 960;

    #[test]
    fn a_request_is_applied_once() {
        let mut f = Follower::default();
        assert_eq!(f.step(Some(0x1234_0001), Some(R1920)), Step::Apply(1920, 1080));
        assert_eq!(f.step(Some(0x1234_0001), Some(R1920)), Step::Idle);
        assert_eq!(f.step(Some(0x1234_0002), Some(R1280)), Step::Apply(1280, 960));
        // The same size asked again is a new request (the user switched in
        // Windows meanwhile): applied again.
        assert_eq!(f.step(Some(0x1234_0003), Some(R1280)), Step::Apply(1280, 960));
    }

    #[test]
    fn nothing_this_boot_is_nothing_to_do() {
        let mut f = Follower::default();
        assert_eq!(f.step(Some(0), Some(R1920)), Step::Idle, "reset at StartDevice");
        assert_eq!(f.step(None, None), Step::Idle);
        assert_eq!(f.step(Some(0x0001_0001), Some(R1920)), Step::Apply(1920, 1080));
    }

    #[test]
    fn nonsense_sizes_are_ignored() {
        let mut f = Follower::default();
        assert_eq!(f.step(Some(1), Some(0)), Step::Idle);
        assert_eq!(f.step(Some(2), None), Step::Idle);
        assert_eq!(f.step(Some(3), Some(10 << 16 | 10)), Step::Idle);
    }

    #[test]
    fn unpacks() {
        assert_eq!(unpack(R1920), (1920, 1080));
        assert_eq!(unpack(5120 << 16 | 1440), (5120, 1440));
    }
}
