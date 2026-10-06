//! Level 5 (`KmdRmClient` = 5): WHEN the KMD re-flips the shown RM system-memory primary,
//! and how often. Pure decisions, no memory, no transport, no clock; the I/O half is
//! `kmd_render/src/virtio/rm_client/sysmem_flip.rs`. Design: `docs/kmd-rm-client.md`
//! section 15.16.
//!
//! WHY A RE-FLIP AT ALL. The primary is memory that GDI, DWM and the paging copies write
//! through the CPU. The host viewer commits (attach, full damage, `wl_surface_commit`) only
//! when it receives a `ScanoutFlip`; it does not re-sample the buffer by itself and there is
//! no damage message. A flip that names the SAME resource again is the cheap refresh (the
//! PRIME export is cached, the viewer reuses its `wl_buffer`): so every change of the
//! primary has to end in a flip, and this module says which events are such changes
//! ([`judge`]), how fast the flips may follow each other ([`flip_interval_100ns`]), and what
//! covers a change that no event reports ([`Refresher`]).
//!
//! THE EVENTS. The KMD is told about a change of the primary by: `SetVidPnSourceAddress`
//! naming it ([`Edge::Programmed`]); the desktop refresh machinery asking for a flush the
//! arbiter's gate withheld ([`Edge::Refresh`]: UMD present markers, the restore after a user
//! source); `DxgkDdiPresent` with the primary as the destination of a blit
//! ([`Edge::PresentBlt`]); the completion of a two-phase windowed blit into it
//! ([`Edge::WindowedBlt`]); a paging transfer or fill into it ([`Edge::Paging`]). A CPU
//! write through the aperture mapping is told to nobody. That is what [`Refresher`] is for:
//! after the last reported change the primary stays "dirty-unknown" for a short, bounded
//! time, during which it is re-flipped on a decaying schedule ([`TAIL_100NS`]); and an
//! opt-in heartbeat (`KmdRmSysPollMs`) re-flips at a fixed period for a desktop whose
//! writes never come with an event (a GDI-only session). Nothing runs on an idle desktop by
//! default: after the tail the worker has no wake at all.
//!
//! Time is `now` in 100 ns units (interrupt time).

use crate::vsync_deadline;

/// The shortest interval between two flips: 2 ms (500 Hz). Above that rate a flip per
/// refresh buys nothing (no display shows it) and the synchronous flip round trip of the
/// worker is the limit anyway.
pub const MIN_FLIP_INTERVAL_100NS: u64 = 20_000;
/// The longest: 100 ms (10 Hz). A mode slower than that is not a mode the desktop runs at.
pub const MAX_FLIP_INTERVAL_100NS: u64 = 1_000_000;

/// The smallest time between two flips for a mode refreshing at `refresh_mhz` (millihertz,
/// 60 Hz = 60_000): one flip per refresh period, so at most one per vblank, clamped to
/// [[`MIN_FLIP_INTERVAL_100NS`], [`MAX_FLIP_INTERVAL_100NS`]]. An unknown rate (0) or an
/// absurd one (the vsync clock's own fallback) is 60 Hz.
///
/// 5120x1440 at 240 Hz is 41_667 (4.17 ms), 144 Hz 69_444, 60 Hz 166_667.
pub const fn flip_interval_100ns(refresh_mhz: u32) -> u64 {
    let period = vsync_deadline::period_100ns(refresh_mhz);
    if period < MIN_FLIP_INTERVAL_100NS {
        MIN_FLIP_INTERVAL_100NS
    } else if period > MAX_FLIP_INTERVAL_100NS {
        MAX_FLIP_INTERVAL_100NS
    } else {
        period
    }
}

/// What reported that the primary changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// `SetVidPnSourceAddress` made it the shown source (every flip of the DMA / MMIO
    /// present contract ends in one, also for the same allocation again).
    Programmed,
    /// The desktop refresh machinery wanted a host flush (a UMD present marker whose
    /// resource is the primary or none, the restore after a user source ended, a
    /// completed bind) and the arbiter's gate withheld it. Raised from
    /// `foreign_scanout_suppresses`.
    Refresh,
    /// `DxgkDdiPresent` with the primary as the destination of a blit, the blit done.
    PresentBlt,
    /// A two-phase windowed blit into the primary completed (ring copy and mirror).
    WindowedBlt,
    /// A `BuildPagingBuffer` content write into the primary: a page-in (system to the
    /// primary), a fill, a virtual transfer in.
    Paging,
}

/// How many kinds of edge there are (counter arrays).
pub const EDGE_KINDS: usize = 5;

impl Edge {
    pub const fn index(self) -> usize {
        match self {
            Edge::Programmed => 0,
            Edge::Refresh => 1,
            Edge::PresentBlt => 2,
            Edge::WindowedBlt => 3,
            Edge::Paging => 4,
        }
    }
}

/// What an edge asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The shown primary changed: a frame is owed.
    Flip,
    /// The event concerns another allocation: nothing is owed.
    NotShown,
    /// Level 5 is not in force: the Venus path owns every edge.
    Off,
}

/// Whether an event is a change of the shown primary. `shown_resid` is the resource the
/// screen shows (0 = no RM primary is shown), `resid` the resource the event named.
///
/// [`Edge::Programmed`] and [`Edge::Refresh`] name no allocation themselves: the first
/// is the act of showing it, the second is only ever raised by the gate, which fires only
/// while the active scanout resource is the RM primary. The other three name the allocation
/// they touched and must match the one shown: a blit into a back buffer, a paging copy of
/// some other surface (the common case by far) is not a change of the screen.
pub const fn judge(level_on: bool, shown_resid: u32, edge: Edge, resid: u32) -> Verdict {
    if !level_on {
        return Verdict::Off;
    }
    match edge {
        Edge::Programmed | Edge::Refresh => Verdict::Flip,
        Edge::PresentBlt | Edge::WindowedBlt | Edge::Paging => {
            if shown_resid != 0 && resid == shown_resid {
                Verdict::Flip
            } else {
                Verdict::NotShown
            }
        }
    }
}

// ---- the dirty-unknown window ---------------------------------------------------------------

/// Re-flips after the last flip that was owed to a reported change, as delays from the flip
/// before: 50, 100, 200, 400 and 800 ms. Five flips, 1.55 s: they catch CPU writes that
/// trail the event that announced them (GDI drawing after the present that started the
/// frame, a write that raced the flip's own read) and then the schedule ENDS. A real edge
/// restarts it, so a desktop that is being presented to never reaches the tail.
pub const TAIL_100NS: [u64; 5] = [500_000, 1_000_000, 2_000_000, 4_000_000, 8_000_000];

/// The heartbeat's bounds (`KmdRmSysPollMs`): 0 is off; anything else is clamped to
/// 50 ms .. 5 s.
pub const MIN_POLL_MS: u32 = 50;
pub const MAX_POLL_MS: u32 = 5_000;

/// The heartbeat period in 100 ns for the knob's value (0 = off).
pub const fn poll_100ns(ms: u32) -> u64 {
    if ms == 0 {
        return 0;
    }
    let ms = if ms < MIN_POLL_MS {
        MIN_POLL_MS
    } else if ms > MAX_POLL_MS {
        MAX_POLL_MS
    } else {
        ms
    };
    ms as u64 * 10_000
}

/// Which synthetic frame edge [`Refresher::synthetic`] raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synthetic {
    None,
    /// A step of the tail after the last reported change.
    Tail,
    /// The opt-in heartbeat.
    Poll,
}

/// What the refresher counted (cumulative; a reset of its state keeps them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Frame edges raised (all kinds), as the worker saw them.
    pub edges: u32,
    /// Flips that were owed to a reported change.
    pub real_flips: u32,
    /// Flips of the tail.
    pub tail_flips: u32,
    /// Flips of the heartbeat.
    pub poll_flips: u32,
    /// Flips that were owed to nothing reported (the first frame after a registration).
    pub other_flips: u32,
}

impl Stats {
    /// Edges that did not get a flip of their own: they were folded into a frame that was
    /// already owed (the mode's interval has not passed, or the worker was busy).
    pub const fn coalesced(&self) -> u32 {
        self.edges.saturating_sub(self.real_flips)
    }
}

/// The dirty-unknown window of the shown primary: plain data, owned by the HPD worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refresher {
    /// The heartbeat period (0 = off).
    poll: u64,
    /// When a flip was last shown (0 = never).
    last_shown: u64,
    /// When the next tail flip is due (0 = none is scheduled).
    tail_due: u64,
    /// Tail flips done since the last real one.
    tail_stage: u8,
    /// A synthetic edge was raised and its flip has not been shown yet.
    injected: Synthetic,
    /// A reported change is owed a flip.
    owed_real: bool,
    stats: Stats,
}

impl Refresher {
    pub const fn new() -> Self {
        Refresher {
            poll: 0,
            last_shown: 0,
            tail_due: 0,
            tail_stage: 0,
            injected: Synthetic::None,
            owed_real: false,
            stats: Stats {
                edges: 0,
                real_flips: 0,
                tail_flips: 0,
                poll_flips: 0,
                other_flips: 0,
            },
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The heartbeat period from the knob's value in milliseconds (0 = off).
    pub fn set_poll_ms(&mut self, ms: u32) {
        self.poll = poll_100ns(ms);
    }

    /// The worker took `count` reported edges (`count` may be 0 when only the flag was
    /// seen) and `frame_edge` says a frame is owed to them.
    pub fn edges(&mut self, count: u32, frame_edge: bool) {
        self.stats.edges = self.stats.edges.saturating_add(count);
        if frame_edge {
            self.owed_real = true;
        }
    }

    /// Whether this pass owes a frame nobody reported: the next step of the tail, or the
    /// heartbeat. `ready` is "an RM primary is shown and usable". At most one is
    /// outstanding (until its flip is shown), and none while a reported change is owed a
    /// flip anyway.
    pub fn synthetic(&mut self, now: u64, ready: bool) -> Synthetic {
        if !ready || self.owed_real || self.injected != Synthetic::None {
            return Synthetic::None;
        }
        if self.tail_due != 0 && now >= self.tail_due {
            self.tail_due = 0;
            self.injected = Synthetic::Tail;
            return Synthetic::Tail;
        }
        if self.poll != 0
            && self.last_shown != 0
            && now >= self.last_shown.saturating_add(self.poll)
        {
            self.injected = Synthetic::Poll;
            return Synthetic::Poll;
        }
        Synthetic::None
    }

    /// A flip was shown at `now`; `copied` is whether it carried a frame (a `CopyFlip`,
    /// the only flip of a ring of one that owes anything) rather than a resume re-flip.
    pub fn shown(&mut self, now: u64, copied: bool) {
        self.last_shown = now.max(1);
        if !copied {
            return;
        }
        if self.owed_real {
            self.owed_real = false;
            self.stats.real_flips = self.stats.real_flips.saturating_add(1);
            self.restart_tail(now);
        } else {
            match self.injected {
                Synthetic::Tail => {
                    self.stats.tail_flips = self.stats.tail_flips.saturating_add(1);
                    self.tail_stage = self.tail_stage.saturating_add(1);
                    self.tail_due = match TAIL_100NS.get(usize::from(self.tail_stage)) {
                        Some(d) => now.saturating_add(*d),
                        None => 0,
                    };
                }
                Synthetic::Poll => {
                    self.stats.poll_flips = self.stats.poll_flips.saturating_add(1);
                }
                Synthetic::None => {
                    self.stats.other_flips = self.stats.other_flips.saturating_add(1);
                    self.restart_tail(now);
                }
            }
        }
        self.injected = Synthetic::None;
    }

    fn restart_tail(&mut self, now: u64) {
        self.tail_stage = 0;
        self.tail_due = now.saturating_add(TAIL_100NS[0]);
    }

    /// When the worker must wake for this module, strictly after `now`: the next tail step
    /// or heartbeat. `None` means nothing is scheduled and the worker may sleep without a
    /// timeout (the idle desktop). A moment that is already due is NOT returned: it is
    /// consumed by the next [`Self::synthetic`], and a wake for it that the pass could not
    /// serve would spin the worker.
    pub fn next_due(&self, now: u64) -> Option<u64> {
        if self.injected != Synthetic::None || self.owed_real {
            return None;
        }
        let tail = (self.tail_due > now).then_some(self.tail_due);
        let poll = if self.poll != 0 && self.last_shown != 0 {
            let at = self.last_shown.saturating_add(self.poll);
            (at > now).then_some(at)
        } else {
            None
        };
        match (tail, poll) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Forget the window (the presenter was reset, the target changed generation). The
    /// counters stay.
    pub fn reset(&mut self) {
        let stats = self.stats;
        let poll = self.poll;
        *self = Refresher::new();
        self.stats = stats;
        self.poll = poll;
    }
}

impl Default for Refresher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::rm_present::{Act, FlipResult, Presenter};
    use crate::rm_sysmem::flip_inputs;
    use std::vec::Vec;

    const MS: u64 = 10_000;

    // ---- the interval follows the mode ----------------------------------------------------

    #[test]
    fn the_interval_is_one_refresh_period_of_the_mode() {
        assert_eq!(flip_interval_100ns(60_000), 166_667);
        assert_eq!(flip_interval_100ns(144_000), 69_444);
        assert_eq!(flip_interval_100ns(240_000), 41_667, "5120x1440 at 240 Hz");
        assert_eq!(flip_interval_100ns(360_000), 27_778);
        assert_eq!(flip_interval_100ns(59_940), 166_834, "NTSC 59.94");
        assert_eq!(flip_interval_100ns(30_000), 333_333);
    }

    #[test]
    fn the_interval_is_clamped_to_a_sane_range() {
        // Faster than 500 Hz is 500 Hz, slower than 10 Hz is 10 Hz.
        assert_eq!(flip_interval_100ns(500_000), MIN_FLIP_INTERVAL_100NS);
        assert_eq!(flip_interval_100ns(900_000), MIN_FLIP_INTERVAL_100NS);
        assert_eq!(flip_interval_100ns(10_000), MAX_FLIP_INTERVAL_100NS);
        assert_eq!(flip_interval_100ns(1_000), MAX_FLIP_INTERVAL_100NS);
        // An unknown rate, and one the vsync clock itself calls absurd, are 60 Hz.
        assert_eq!(flip_interval_100ns(0), 166_667);
        assert_eq!(flip_interval_100ns(u32::MAX), 166_667);
        for mhz in (1..2_000_000u32).step_by(997) {
            let i = flip_interval_100ns(mhz);
            assert!(
                (MIN_FLIP_INTERVAL_100NS..=MAX_FLIP_INTERVAL_100NS).contains(&i),
                "{mhz}"
            );
        }
    }

    // ---- which events are a change of the shown primary -------------------------------------

    #[test]
    fn every_event_that_changes_the_shown_primary_asks_for_a_flip() {
        let shown = 40;
        for edge in [Edge::Programmed, Edge::Refresh] {
            assert_eq!(judge(true, shown, edge, 0), Verdict::Flip, "{edge:?}");
            assert_eq!(
                judge(true, 0, edge, 0),
                Verdict::Flip,
                "{edge:?} names no allocation"
            );
        }
        for edge in [Edge::PresentBlt, Edge::WindowedBlt, Edge::Paging] {
            assert_eq!(judge(true, shown, edge, shown), Verdict::Flip, "{edge:?}");
            assert_eq!(
                judge(true, shown, edge, 41),
                Verdict::NotShown,
                "{edge:?} other surface"
            );
            assert_eq!(
                judge(true, shown, edge, 0),
                Verdict::NotShown,
                "{edge:?} no identity"
            );
            assert_eq!(
                judge(true, 0, edge, 0),
                Verdict::NotShown,
                "nothing shown: 0 is not a match"
            );
            assert_eq!(
                judge(true, 0, edge, 40),
                Verdict::NotShown,
                "{edge:?} after the target went"
            );
        }
    }

    #[test]
    fn with_level_5_off_no_edge_is_ours() {
        for edge in [
            Edge::Programmed,
            Edge::Refresh,
            Edge::PresentBlt,
            Edge::WindowedBlt,
            Edge::Paging,
        ] {
            assert_eq!(judge(false, 40, edge, 40), Verdict::Off);
        }
    }

    #[test]
    fn the_edge_indices_are_a_dense_permutation() {
        let all = [
            Edge::Programmed,
            Edge::Refresh,
            Edge::PresentBlt,
            Edge::WindowedBlt,
            Edge::Paging,
        ];
        assert_eq!(all.len(), EDGE_KINDS);
        let mut seen = [false; EDGE_KINDS];
        for e in all {
            assert!(!seen[e.index()]);
            seen[e.index()] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    // ---- pacing: the presenter at the mode's rate -------------------------------------------

    /// Drive a registered ring-of-one presenter with `edge_every` between reported edges for
    /// `span`; the worker looks every `look_every`. Returns the instants of the flips.
    fn drive(interval: u64, edge_every: u64, span: u64, look_every: u64) -> Vec<u64> {
        let mut p = Presenter::new(1);
        p.set_min_interval(interval);
        let mut t = 100 * MS;
        let start = t;
        let i0 = flip_inputs(t, true, false, true, true, false);
        assert_eq!(p.decide(i0), Act::Register);
        p.registration(true, t);
        let mut flips = Vec::new();
        let mut next_edge = t;
        while t <= start + span + 4 * interval {
            let edge = if t <= start + span && t >= next_edge {
                next_edge += edge_every;
                true
            } else {
                false
            };
            match p.decide(flip_inputs(t, true, true, true, edge, false)) {
                Act::CopyFlip { slot } => {
                    p.flipped(slot, true, FlipResult::Shown, t);
                    flips.push(t);
                }
                Act::WaitUntil(at) => {
                    assert!(at > t, "a wake in the past would spin the worker");
                }
                Act::Idle => {}
                other => panic!("{other:?}"),
            }
            t += look_every;
        }
        flips
    }

    #[test]
    fn at_240_hz_edges_at_4_khz_flip_at_most_once_per_refresh() {
        let iv = flip_interval_100ns(240_000);
        // An edge every 250 us for 100 ms; the worker looks every 100 us.
        let flips = drive(iv, 2_500, 100 * MS, 1_000);
        assert!(flips.len() >= 20, "{}", flips.len());
        assert!(
            flips.len() <= 100 * MS as usize / iv as usize + 2,
            "{}",
            flips.len()
        );
        for w in flips.windows(2) {
            assert!(w[1] - w[0] >= iv, "{} then {}", w[0], w[1]);
        }
    }

    #[test]
    fn the_last_edge_is_always_shown_by_a_trailing_flip() {
        let iv = flip_interval_100ns(240_000);
        let flips = drive(iv, 3_000, 50 * MS, 500);
        let last = *flips.last().unwrap();
        // The final edge (at or before 50 ms) was not left unflipped: a flip follows it.
        assert!(last >= 100 * MS + 50 * MS - iv, "last flip {last}");
    }

    #[test]
    fn at_60_hz_and_at_240_hz_the_same_load_gets_the_rate_of_its_mode() {
        let n60 = drive(flip_interval_100ns(60_000), 2_500, 200 * MS, 1_000).len();
        let n240 = drive(flip_interval_100ns(240_000), 2_500, 200 * MS, 1_000).len();
        assert!(n60 <= 200 * MS as usize / 166_667 + 2, "{n60}");
        assert!(n240 >= 3 * n60, "{n240} vs {n60}");
    }

    #[test]
    fn a_new_interval_applies_to_the_next_decision_and_survives_a_reset() {
        let mut p = Presenter::new(1);
        p.set_min_interval(flip_interval_100ns(240_000));
        assert_eq!(p.min_interval(), 41_667);
        p.reset();
        assert_eq!(p.min_interval(), 41_667, "the mode did not change");
        p.set_min_interval(0);
        assert_eq!(p.min_interval(), 1, "never a zero interval");
        // The ring levels never set it: 60 Hz as before.
        assert_eq!(
            Presenter::new(2).min_interval(),
            crate::rm_present::MIN_FRAME_INTERVAL_100NS
        );
    }

    // ---- the dirty-unknown window -----------------------------------------------------------

    fn show_real(r: &mut Refresher, t: u64) {
        r.edges(1, true);
        assert_eq!(
            r.synthetic(t, true),
            Synthetic::None,
            "a reported change owes a flip already"
        );
        r.shown(t, true);
    }

    #[test]
    fn after_a_reported_change_the_tail_re_flips_five_times_and_ends() {
        let mut r = Refresher::new();
        let mut t = 100 * MS;
        show_real(&mut r, t);
        let mut at = Vec::new();
        for want in TAIL_100NS {
            let due = r.next_due(t).expect("scheduled");
            assert_eq!(due, t + want);
            assert_eq!(
                r.synthetic(due - 1, true),
                Synthetic::None,
                "not before it is due"
            );
            assert_eq!(r.synthetic(due, true), Synthetic::Tail);
            assert_eq!(
                r.synthetic(due, true),
                Synthetic::None,
                "one outstanding at a time"
            );
            r.shown(due, true);
            t = due;
            at.push(due);
        }
        assert_eq!(r.stats().tail_flips, 5);
        assert_eq!(
            r.next_due(t + 1),
            None,
            "the idle desktop has no wake at all"
        );
        assert_eq!(
            r.synthetic(t + 3_600_000 * MS, true),
            Synthetic::None,
            "an hour later"
        );
        assert_eq!(r.stats().poll_flips, 0);
    }

    #[test]
    fn a_new_reported_change_restarts_the_tail_and_a_busy_desktop_never_reaches_it() {
        let mut r = Refresher::new();
        let mut t = 100 * MS;
        for _ in 0..600 {
            show_real(&mut r, t);
            t += 16 * MS / 10;
            assert_eq!(r.synthetic(t, true), Synthetic::None, "{t}");
        }
        assert_eq!(r.stats().tail_flips, 0);
        assert_eq!(r.stats().real_flips, 600);
    }

    #[test]
    fn nothing_is_injected_without_a_shown_primary() {
        let mut r = Refresher::new();
        show_real(&mut r, 100 * MS);
        assert_eq!(r.synthetic(10_000 * MS, false), Synthetic::None);
        // And the tail that was due is still due when a primary is shown again.
        assert_eq!(r.synthetic(10_000 * MS, true), Synthetic::Tail);
    }

    #[test]
    fn a_wake_is_never_asked_for_a_moment_that_is_already_due() {
        let mut r = Refresher::new();
        show_real(&mut r, 100 * MS);
        let due = r.next_due(100 * MS).unwrap();
        assert_eq!(
            r.next_due(due),
            None,
            "due now: the next synthetic() takes it, not a timer"
        );
        assert_eq!(r.next_due(due + 1), None);
        // A synthetic edge in flight (the source is yielded, say) asks for no wake either.
        assert_eq!(r.synthetic(due, true), Synthetic::Tail);
        assert_eq!(r.next_due(due), None);
        assert_eq!(r.next_due(due - 1), None);
    }

    #[test]
    fn the_heartbeat_is_off_by_default_and_bounded_when_on() {
        assert_eq!(poll_100ns(0), 0);
        assert_eq!(poll_100ns(1), 50 * MS);
        assert_eq!(poll_100ns(250), 250 * MS);
        assert_eq!(poll_100ns(1_000_000), 5_000 * 10_000);
        let mut r = Refresher::new();
        r.set_poll_ms(100);
        let mut t = 100 * MS;
        show_real(&mut r, t);
        // The tail goes first; then the heartbeat keeps a 100 ms period from the last flip.
        let mut polls = 0;
        let end = t + 20_000 * MS;
        while t < end {
            let Some(at) = r.next_due(t) else {
                panic!("a heartbeat always has a next moment")
            };
            t = at;
            match r.synthetic(t, true) {
                Synthetic::Tail => r.shown(t, true),
                Synthetic::Poll => {
                    polls += 1;
                    r.shown(t, true);
                }
                Synthetic::None => panic!("woken for nothing at {t}"),
            }
        }
        assert!(polls >= 150, "{polls}");
        assert_eq!(r.stats().tail_flips, 5);
        assert_eq!(r.stats().poll_flips, polls);
    }

    #[test]
    fn a_heartbeat_that_cannot_flip_raises_one_edge_not_a_stream() {
        let mut r = Refresher::new();
        r.set_poll_ms(50);
        r.shown(100 * MS, true);
        let mut t = 100 * MS;
        let mut raised = 0;
        for _ in 0..1000 {
            t += 10 * MS;
            if r.synthetic(t, true) != Synthetic::None {
                raised += 1;
            }
        }
        // The first is the tail (or the poll); until its flip is shown, nothing more.
        assert_eq!(raised, 1);
    }

    #[test]
    fn the_first_frame_of_a_registration_starts_a_tail_too() {
        let mut r = Refresher::new();
        r.shown(100 * MS, true);
        assert_eq!(r.stats().other_flips, 1);
        assert!(r.next_due(100 * MS).is_some());
    }

    #[test]
    fn a_resume_reflip_changes_neither_the_tail_nor_the_counts() {
        let mut r = Refresher::new();
        show_real(&mut r, 100 * MS);
        let due = r.next_due(100 * MS).unwrap();
        r.shown(100 * MS + 5, false);
        assert_eq!(r.next_due(100 * MS + 5), Some(due));
        assert_eq!(r.stats().real_flips, 1);
    }

    #[test]
    fn coalesced_edges_are_those_without_a_flip_of_their_own() {
        let mut r = Refresher::new();
        // 100 edges seen across a few passes; only 4 flips came of them.
        r.edges(60, true);
        r.shown(1, true);
        r.edges(30, true);
        r.shown(2, true);
        r.edges(9, true);
        r.shown(3, true);
        r.edges(1, true);
        r.shown(4, true);
        let s = r.stats();
        assert_eq!((s.edges, s.real_flips, s.coalesced()), (100, 4, 96));
    }

    #[test]
    fn a_reset_forgets_the_window_and_keeps_the_counters() {
        let mut r = Refresher::new();
        r.set_poll_ms(100);
        show_real(&mut r, 100 * MS);
        r.reset();
        assert_eq!(r.next_due(0), None);
        assert_eq!(r.stats().real_flips, 1);
        // The heartbeat setting is the knob's, not the window's.
        r.shown(200 * MS, true);
        assert!(r.next_due(200 * MS).is_some());
    }

    // ---- the whole chain: edges, presenter, refresher ---------------------------------------

    #[test]
    fn a_burst_then_silence_ends_in_five_tail_flips_and_a_sleeping_worker() {
        let iv = flip_interval_100ns(240_000);
        let mut p = Presenter::new(1);
        p.set_min_interval(iv);
        let mut r = Refresher::new();
        let mut t = 100 * MS;
        assert_eq!(
            p.decide(flip_inputs(t, true, false, true, true, false)),
            Act::Register
        );
        p.registration(true, t);
        let mut flips = 0u32;
        let mut tail_wake: Option<u64> = None;
        // 20 ms of edges every 1 ms, then nothing for 5 s.
        let end = t + 5_000 * MS;
        let burst_end = t + 20 * MS;
        let mut next_edge = t;
        while t < end {
            let reported = t <= burst_end && t >= next_edge;
            if reported {
                next_edge += MS;
            }
            r.edges(u32::from(reported), reported);
            let syn = r.synthetic(t, true);
            let frame_edge = reported || syn != Synthetic::None;
            let act = p.decide(flip_inputs(t, true, true, true, frame_edge, false));
            let mut wake = None;
            match act {
                Act::CopyFlip { slot } => {
                    p.flipped(slot, true, FlipResult::Shown, t);
                    r.shown(t, true);
                    flips += 1;
                }
                Act::WaitUntil(at) => wake = Some(at),
                Act::Idle => {}
                other => panic!("{other:?}"),
            }
            let due = r.next_due(t);
            let next = [wake, due, (t <= burst_end).then_some(next_edge)]
                .into_iter()
                .flatten()
                .min();
            match next {
                Some(n) => {
                    assert!(n > t, "no wake in the past");
                    t = n;
                }
                None => {
                    tail_wake = Some(t);
                    break;
                }
            }
        }
        assert!(
            tail_wake.is_some(),
            "the worker went to sleep with nothing scheduled"
        );
        assert_eq!(r.stats().tail_flips, 5);
        // 20 ms of 240 Hz: about five frames, then the five of the tail.
        assert!((5..=12).contains(&flips), "{flips}");
        assert!(r.stats().coalesced() >= 10);
    }
}
