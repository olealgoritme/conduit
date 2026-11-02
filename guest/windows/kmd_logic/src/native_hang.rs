//! Fault and hang detection for native NVIDIA channels (the RM GPFIFO channels
//! behind the NVIDIA UMDs' hardware queues), and the decisions of their recovery.
//!
//! A native queue's work completes when the GPU writes the queue's progress fence.
//! A channel that RM put in error (an MMU fault, Xid 31; a PBDMA error, Xid 32; ...)
//! or that stopped making progress never writes it again, and everything waiting on
//! that fence (dxgkrnl's HW queue waits, process teardown, the scheduler flush at
//! adapter stop) waits forever. The KMD's progress poll (`ddi::hws`) feeds every
//! registered queue through a [`Watch`]:
//!
//! * a set error notifier (RM writes `NvNotification` index 0 of the channel's
//!   `hObjectError` memory with a nonzero status on every robust-channel error) is a
//!   fault at once, whether or not work is outstanding;
//! * outstanding work whose progress fence and GP_GET both stayed put for the hang
//!   timeout (`NvHangMs`, [`hang_timeout_100ns`]) is a hang.
//!
//! Either is reported once ([`Action::Report`]: the KMD raises
//! `DXGK_INTERRUPT_GPU_ENGINE_TIMEOUT` for the node, and dxgkrnl answers with
//! `DxgkDdiResetHwEngine`, whose recovery marks the queue dead). A report that no
//! reset follows within [`RESET_GRACE_100NS`] force-completes the queue anyway
//! ([`Action::ForceComplete`]), so no wait on it can outlive timeout + grace; it
//! stays flagged, so a reset that comes later still recovers its context.
//!
//! Times are in 100 ns units of one monotonic clock. Argument-only and host-tested;
//! the I/O lives in `kmd_render` (`ddi/hws.rs`, `virtio/native_channel.rs`).

/// Byte offsets of the error notifier's words in the channel's control memory
/// (`NvNotification` index 0 at offset 0: `info32` at +8, `info16 | status << 16`
/// at +12).
pub const ERROR_INFO32_AT: u64 = 8;
pub const ERROR_WORD_AT: u64 = 12;

/// What RM wrote into a channel's error notifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelError {
    /// The robust-channel error (the Xid number: 31 MMU fault, 32 PBDMA error, ...).
    pub xid: u32,
    /// `info16`: the RM engine type the error was raised on.
    pub engine: u16,
    /// RM writes 0xffff; any nonzero value marks the channel in error.
    pub status: u16,
}

/// The error notifier's two words, read from the channel's control memory.
pub const fn channel_error(info32: u32, word: u32) -> Option<ChannelError> {
    let status = (word >> 16) as u16;
    if status == 0 {
        None
    } else {
        Some(ChannelError {
            xid: info32,
            engine: word as u16,
            status,
        })
    }
}

/// `NvHangMs` when the service key has no value.
pub const DEFAULT_HANG_MS: u32 = 10_000;
/// Shorter values are raised to this: the progress fence is written once per DMA
/// buffer, so a busy queue can legitimately sit on one value for a while.
pub const MIN_HANG_MS: u32 = 2_000;
pub const MAX_HANG_MS: u32 = 600_000;

/// The hang timeout for an `NvHangMs` value: `None` (0) turns hang detection off,
/// leaving fault detection on; anything else is clamped to
/// [`MIN_HANG_MS`]..=[`MAX_HANG_MS`].
pub const fn hang_timeout_100ns(knob_ms: u32) -> Option<u64> {
    if knob_ms == 0 {
        return None;
    }
    let ms = if knob_ms < MIN_HANG_MS {
        MIN_HANG_MS
    } else if knob_ms > MAX_HANG_MS {
        MAX_HANG_MS
    } else {
        knob_ms
    };
    Some(ms as u64 * 10_000)
}

/// How long a report waits for `DxgkDdiResetHwEngine` before the queue is
/// force-completed without it.
pub const RESET_GRACE_100NS: u64 = 30_000_000;

/// Why a queue was reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Fault(ChannelError),
    Hang,
}

impl Cause {
    /// One word for the diagnostics: the Xid of a fault, 0x10000 for a hang.
    pub const fn code(self) -> u32 {
        match self {
            Cause::Fault(e) => e.xid & 0xffff,
            Cause::Hang => 0x1_0000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Healthy,
    Reported { at: u64, cause: Cause },
    /// The grace ran out: completed by the KMD, still awaiting a reset that
    /// invalidates its context.
    Forced(Cause),
    Dead,
}

/// One poll's look at a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub now: u64,
    /// Work was submitted that the progress fence does not show complete.
    pub outstanding: bool,
    pub progress: u64,
    pub gp_get: u32,
    pub error: Option<ChannelError>,
}

/// What the KMD does after a [`Watch::tick`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// First detection: report the node to dxgkrnl (GPU engine timeout).
    Report(Cause),
    /// The report went unanswered for the grace period: complete the queue's fence
    /// and stop submitting to it, without dxgkrnl's reset.
    ForceComplete(Cause),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    phase: Phase,
    progress: u64,
    gp_get: u32,
    /// When progress and GP_GET were last seen to change with work outstanding.
    since: Option<u64>,
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

impl Watch {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Healthy,
            progress: 0,
            gp_get: 0,
            since: None,
        }
    }

    pub fn tick(&mut self, s: Sample, hang_timeout: Option<u64>) -> Action {
        match self.phase {
            Phase::Dead | Phase::Forced(_) => Action::None,
            Phase::Reported { at, cause } => {
                // A hang that was only a long wait, gone before any reset: healthy
                // again. A fault never heals.
                if cause == Cause::Hang
                    && s.error.is_none()
                    && (s.progress != self.progress || !s.outstanding)
                {
                    self.phase = Phase::Healthy;
                    self.follow(s);
                    return Action::None;
                }
                if s.now.saturating_sub(at) >= RESET_GRACE_100NS {
                    self.phase = Phase::Forced(cause);
                    Action::ForceComplete(cause)
                } else {
                    Action::None
                }
            }
            Phase::Healthy => {
                if let Some(error) = s.error {
                    return self.report(s.now, Cause::Fault(error));
                }
                if !s.outstanding || self.since.is_none() || self.moved(s) {
                    self.follow(s);
                    return Action::None;
                }
                match (hang_timeout, self.since) {
                    (Some(timeout), Some(since)) if s.now.saturating_sub(since) >= timeout => {
                        self.report(s.now, Cause::Hang)
                    }
                    _ => Action::None,
                }
            }
        }
    }

    fn moved(&self, s: Sample) -> bool {
        s.progress != self.progress || s.gp_get != self.gp_get
    }

    fn follow(&mut self, s: Sample) {
        self.progress = s.progress;
        self.gp_get = s.gp_get;
        self.since = s.outstanding.then_some(s.now);
    }

    fn report(&mut self, now: u64, cause: Cause) -> Action {
        self.phase = Phase::Reported { at: now, cause };
        Action::Report(cause)
    }

    /// The reported cause still awaiting recovery (a reset), if any.
    pub const fn flagged(&self) -> Option<Cause> {
        match self.phase {
            Phase::Reported { cause, .. } | Phase::Forced(cause) => Some(cause),
            _ => None,
        }
    }

    /// Reported and inside the grace: the poll must keep running for the
    /// fallback even with nothing outstanding.
    pub const fn in_grace(&self) -> bool {
        matches!(self.phase, Phase::Reported { .. })
    }

    /// Recovered (reset, adapter TDR or the grace fallback): never reported again.
    pub fn mark_dead(&mut self) {
        self.phase = Phase::Dead;
    }

    pub const fn is_dead(&self) -> bool {
        matches!(self.phase, Phase::Dead)
    }
}

/// The value a dead queue's progress fence is completed to: everything submitted
/// (never below what the storage already holds).
pub const fn completion_value(submitted: u64, current: u64) -> u64 {
    if submitted > current {
        submitted
    } else {
        current
    }
}

/// `NV906F_CTRL_CMD_RESET_CHANNEL` (allow-listed for unprivileged clients).
pub const CTRL_RESET_CHANNEL: u32 = 0x906f_0102;
/// `NV906F_CTRL_CMD_RESET_CHANNEL_PARAMS`: engineID, subdeviceInstance,
/// resetReason, bIsRcPending (+ padding) = 16 bytes.
pub const RESET_CHANNEL_BYTES: usize = 16;
/// `NV2080_ENGINE_TYPE_NULL`: reset the channel only, no engine (what RM's own
/// watchdog uses to put one channel in error).
pub const RESET_ENGINE_NULL: u32 = 0;
/// `NV906F_CTRL_CMD_RESET_CHANNEL_REASON_DEFAULT`.
pub const RESET_REASON_DEFAULT: u32 = 0;

pub fn reset_channel_params() -> [u8; RESET_CHANNEL_BYTES] {
    let mut a = [0u8; RESET_CHANNEL_BYTES];
    a[0..4].copy_from_slice(&RESET_ENGINE_NULL.to_le_bytes());
    a[4..8].copy_from_slice(&0u32.to_le_bytes());
    a[8..12].copy_from_slice(&RESET_REASON_DEFAULT.to_le_bytes());
    a
}

/// `NvRcReset` when the service key has no value: bit 0 = send
/// `RESET_CHANNEL` to every channel of a recovered context.
pub const DEFAULT_RC_RESET: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 10_000;
    const XID31: ChannelError = ChannelError {
        xid: 31,
        engine: 1,
        status: 0xffff,
    };

    fn sample(now_ms: u64, outstanding: bool, progress: u64, gp_get: u32) -> Sample {
        Sample {
            now: now_ms * MS,
            outstanding,
            progress,
            gp_get,
            error: None,
        }
    }

    #[test]
    fn error_notifier_words() {
        assert_eq!(channel_error(31, 0), None);
        assert_eq!(channel_error(0, 0x0000_0001), None);
        assert_eq!(channel_error(31, 0xffff_0001), Some(XID31));
        let e = channel_error(32, 0x0001_0005).unwrap();
        assert_eq!((e.xid, e.engine, e.status), (32, 5, 1));
    }

    #[test]
    fn knob_clamps_and_zero_disables() {
        assert_eq!(hang_timeout_100ns(0), None);
        assert_eq!(hang_timeout_100ns(1), Some(MIN_HANG_MS as u64 * MS));
        assert_eq!(hang_timeout_100ns(DEFAULT_HANG_MS), Some(100_000_000));
        assert_eq!(hang_timeout_100ns(u32::MAX), Some(MAX_HANG_MS as u64 * MS));
    }

    #[test]
    fn a_fault_is_reported_at_once_then_forced_after_the_grace() {
        let mut w = Watch::new();
        assert_eq!(w.tick(sample(0, true, 5, 3), Some(10_000 * MS)), Action::None);
        let mut s = sample(1, true, 5, 3);
        s.error = Some(XID31);
        assert_eq!(w.tick(s, Some(10_000 * MS)), Action::Report(Cause::Fault(XID31)));
        assert_eq!(w.flagged(), Some(Cause::Fault(XID31)));
        // Only once, even as progress moves.
        let mut s = sample(2, true, 6, 4);
        s.error = Some(XID31);
        assert_eq!(w.tick(s, None), Action::None);
        assert!(w.in_grace());
        s.now = 1 * MS + RESET_GRACE_100NS;
        assert_eq!(w.tick(s, None), Action::ForceComplete(Cause::Fault(XID31)));
        // Completed, but a later reset must still find it to invalidate its context.
        assert!(!w.is_dead());
        assert!(!w.in_grace());
        assert_eq!(w.flagged(), Some(Cause::Fault(XID31)));
        assert_eq!(w.tick(s, None), Action::None);
        w.mark_dead();
        assert!(w.is_dead());
        assert_eq!(w.flagged(), None);
    }

    #[test]
    fn a_fault_with_nothing_outstanding_is_still_a_fault() {
        let mut w = Watch::new();
        let mut s = sample(0, false, 9, 9);
        s.error = Some(XID31);
        assert_eq!(w.tick(s, None), Action::Report(Cause::Fault(XID31)));
    }

    #[test]
    fn a_hang_needs_both_fence_and_gp_get_still_for_the_timeout() {
        let t = Some(10_000 * MS);
        let mut w = Watch::new();
        assert_eq!(w.tick(sample(0, true, 5, 3), t), Action::None);
        // GP_GET moving restarts the clock.
        assert_eq!(w.tick(sample(9_000, true, 5, 4), t), Action::None);
        assert_eq!(w.tick(sample(18_000, true, 5, 4), t), Action::None);
        assert_eq!(w.tick(sample(19_000, true, 5, 4), t), Action::Report(Cause::Hang));
        // Reported once.
        assert_eq!(w.tick(sample(19_001, true, 5, 4), t), Action::None);
    }

    #[test]
    fn idle_queues_and_disabled_detection_never_hang() {
        let mut w = Watch::new();
        for i in 0..100 {
            assert_eq!(w.tick(sample(i * 1_000, false, 5, 3), Some(10_000 * MS)), Action::None);
        }
        let mut w = Watch::new();
        for i in 0..100 {
            assert_eq!(w.tick(sample(i * 1_000, true, 5, 3), None), Action::None);
        }
        // Work that keeps finishing is never a hang, however long the run.
        let mut w = Watch::new();
        for i in 0..100u64 {
            assert_eq!(w.tick(sample(i * 1_000, true, i, 3), Some(10_000 * MS)), Action::None);
        }
    }

    #[test]
    fn a_reported_hang_that_resumes_is_healthy_again() {
        let t = Some(10_000 * MS);
        let mut w = Watch::new();
        w.tick(sample(0, true, 5, 3), t);
        assert_eq!(w.tick(sample(10_000, true, 5, 3), t), Action::Report(Cause::Hang));
        assert_eq!(w.tick(sample(10_500, true, 6, 3), t), Action::None);
        assert_eq!(w.flagged(), None);
        assert!(!w.is_dead());
        // The clock starts over from the resume.
        assert_eq!(w.tick(sample(19_000, true, 6, 3), t), Action::None);
        assert_eq!(w.tick(sample(20_500, true, 6, 3), t), Action::Report(Cause::Hang));
    }

    #[test]
    fn an_unanswered_hang_is_forced_after_the_grace() {
        let t = Some(10_000 * MS);
        let mut w = Watch::new();
        w.tick(sample(0, true, 5, 3), t);
        assert_eq!(w.tick(sample(10_000, true, 5, 3), t), Action::Report(Cause::Hang));
        assert_eq!(w.tick(sample(12_999, true, 5, 3), t), Action::None);
        assert_eq!(w.tick(sample(13_000, true, 5, 3), t), Action::ForceComplete(Cause::Hang));
    }

    #[test]
    fn recovery_marks_dead_and_completion_never_goes_back() {
        let mut w = Watch::new();
        let mut s = sample(0, true, 5, 3);
        s.error = Some(XID31);
        w.tick(s, None);
        w.mark_dead();
        assert!(w.is_dead());
        assert_eq!(w.tick(s, None), Action::None);
        assert_eq!(completion_value(9, 5), 9);
        assert_eq!(completion_value(5, 9), 9);
        assert_eq!(Cause::Hang.code(), 0x1_0000);
        assert_eq!(Cause::Fault(XID31).code(), 31);
    }

    #[test]
    fn reset_channel_block_is_channel_only() {
        let p = reset_channel_params();
        assert_eq!(p.len(), 16);
        assert_eq!(&p[0..4], &[0, 0, 0, 0]);
        assert_eq!(&p[8..12], &[0, 0, 0, 0]);
        assert!(p.iter().all(|b| *b == 0));
    }
}
