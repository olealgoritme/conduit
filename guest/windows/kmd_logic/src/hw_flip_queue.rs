//! WDDM 3.x hardware flip queue for one MPO plane (`kmd_render::ddi::hwfq`).
//!
//! dxgkrnl submits flips with a `PresentId` and a `TargetFlipTime` (QPC units) through
//! `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay3` and learns which ones reached the
//! screen from the flip queue log: a circular array of `{PresentId, PresentTimestamp}` the
//! driver writes, plus the index after the last written entry, reported in the MPO3 vsync
//! (`DXGK_MULTIPLANE_OVERLAY_VSYNC_INFO3.FirstFreeFlipQueueLogEntryIndex`) or by
//! `DxgkDdiUpdateFlipQueueLog`.
//!
//! The "hardware" here is the KMD's vsync tick: at every tick the flips whose target time
//! has passed are taken in order; the newest is shown and every taken flip is logged with
//! the tick's time. A flip submitted with its target time already reached while nothing is
//! queued is shown at once and logged at the next tick.
//!
//! `DxgkDdiSetInterruptTargetPresentId` decides which ticks raise the vsync interrupt:
//! `0` every tick, `u64::MAX` none, otherwise once the last logged PresentId reaches it.

/// Flips the queue holds; `MaxHwQueuedFlips` must stay below this.
pub const CAPACITY: usize = 16;

/// The `MaxHwQueuedFlips` the driver reports when dxgkrnl enables the feature.
pub const MAX_HW_QUEUED_FLIPS: u32 = 8;

/// `DXGK_FEATURE_HWFLIPQUEUE` (`DXGK_FEATURE_ID` 1).
pub const FEATURE_HWFLIPQUEUE: i32 = 1;

/// One submitted flip. `payload` is what the driver needs to show it (`None`: the plane
/// is disabled, nothing to program).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flip<T: Copy> {
    pub present_id: u64,
    pub target_time: u64,
    pub payload: Option<T>,
}

/// What a submission asks the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit<T: Copy> {
    /// Show `payload` now (target reached, nothing queued); it is logged at the next tick.
    ShowNow(Option<T>),
    /// Queued for a later tick.
    Queued,
    /// No room: the caller returns `STATUS_RETRY` with `HwFlipQueueDrainNeeded`.
    Full,
}

/// What a tick asks the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick<T: Copy> {
    /// The newest due flip's payload, to show now (`None` inside: a disabled plane).
    pub show: Option<Option<T>>,
    /// Log entries written by this tick.
    pub logged: u32,
    /// Whether this tick raises the vsync interrupt.
    pub interrupt: bool,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    present_id: u64,
}

/// Queue, log cursor and interrupt target of one plane.
pub struct Queue<T: Copy> {
    flips: [Option<Flip<T>>; CAPACITY],
    head: usize,
    len: usize,
    /// Shown at submission, logged at the next tick (in submission order).
    shown: [Option<Pending>; CAPACITY],
    shown_len: usize,
    /// Log entries written so far (the log index is this modulo the log size).
    written: u64,
    log_entries: u32,
    interrupt_target: u64,
    last_logged: u64,
    /// The interrupt for `interrupt_target` has been raised (it fires once per target).
    target_raised: bool,
}

impl<T: Copy> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Queue<T> {
    pub const fn new() -> Self {
        Self {
            flips: [None; CAPACITY],
            head: 0,
            len: 0,
            shown: [None; CAPACITY],
            shown_len: 0,
            written: 0,
            log_entries: 0,
            interrupt_target: 0,
            last_logged: 0,
            target_raised: false,
        }
    }

    /// `DxgkDdiSetFlipQueueLogBuffer`: a new log of `entries` entries; writing restarts at 0.
    pub fn set_log(&mut self, entries: u32) {
        self.log_entries = entries;
        self.written = 0;
    }

    pub fn log_entries(&self) -> u32 {
        self.log_entries
    }

    /// The index after the last written entry (what the vsync and UpdateFlipQueueLog report).
    pub fn first_free(&self) -> u32 {
        if self.log_entries == 0 {
            0
        } else {
            (self.written % u64::from(self.log_entries)) as u32
        }
    }

    pub fn queued(&self) -> usize {
        self.len
    }

    pub fn last_logged(&self) -> u64 {
        self.last_logged
    }

    /// `DxgkDdiSetInterruptTargetPresentId`.
    pub fn set_interrupt_target(&mut self, target: u64) {
        self.interrupt_target = target;
        self.target_raised = false;
    }

    pub fn interrupt_target(&self) -> u64 {
        self.interrupt_target
    }

    /// A flip from `SetVidPnSourceAddressWithMultiPlaneOverlay3` at time `now`.
    pub fn submit(&mut self, flip: Flip<T>, now: u64) -> Submit<T> {
        if self.len == 0 && flip.target_time <= now && self.shown_len < CAPACITY {
            self.shown[self.shown_len] = Some(Pending { present_id: flip.present_id });
            self.shown_len += 1;
            return Submit::ShowNow(flip.payload);
        }
        if self.len == CAPACITY {
            return Submit::Full;
        }
        self.flips[(self.head + self.len) % CAPACITY] = Some(flip);
        self.len += 1;
        Submit::Queued
    }

    /// The vsync tick at `now`. `log` receives `(index, present_id)` for each entry to
    /// write with the tick's timestamp; the caller writes them into the OS log buffer.
    pub fn tick(&mut self, now: u64, mut log: impl FnMut(u32, u64)) -> Tick<T> {
        let mut logged = 0u32;
        for i in 0..self.shown_len {
            if let Some(p) = self.shown[i].take() {
                self.write(p.present_id, &mut log);
                logged += 1;
            }
        }
        self.shown_len = 0;
        let mut show = None;
        while self.len > 0 {
            let Some(flip) = self.flips[self.head] else {
                break;
            };
            if flip.target_time > now {
                break;
            }
            self.flips[self.head] = None;
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
            self.write(flip.present_id, &mut log);
            logged += 1;
            show = Some(flip.payload);
        }
        let interrupt = self.wants_interrupt();
        Tick { show, logged, interrupt }
    }

    fn write(&mut self, present_id: u64, log: &mut impl FnMut(u32, u64)) {
        if self.log_entries != 0 {
            log(self.first_free(), present_id);
            self.written += 1;
        }
        if present_id > self.last_logged {
            self.last_logged = present_id;
        }
    }

    fn wants_interrupt(&mut self) -> bool {
        match self.interrupt_target {
            0 => true,
            u64::MAX => false,
            target => {
                if self.last_logged >= target && !self.target_raised {
                    self.target_raised = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// `DxgkDdiCancelFlips` for this plane: drops queued flips with PresentId >=
    /// `requested` (they are always the newest ones). Returns `PresentIdCancelled`: the
    /// first cancelled PresentId, or 0 when nothing was cancelled.
    pub fn cancel_from(&mut self, requested: u64) -> u64 {
        let mut first = 0u64;
        while self.len > 0 {
            let tail = (self.head + self.len - 1) % CAPACITY;
            match self.flips[tail] {
                Some(flip) if flip.present_id >= requested => {
                    self.flips[tail] = None;
                    self.len -= 1;
                    first = flip.present_id;
                }
                _ => break,
            }
        }
        first
    }

    /// Drops everything queued (adapter stop, mode change).
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn flip(id: u64, t: u64) -> Flip<u32> {
        Flip { present_id: id, target_time: t, payload: Some(id as u32) }
    }

    fn tick(q: &mut Queue<u32>, now: u64) -> (Tick<u32>, Vec<(u32, u64)>) {
        let mut log = Vec::new();
        let t = q.tick(now, |i, id| log.push((i, id)));
        (t, log)
    }

    #[test]
    fn due_flip_with_empty_queue_shows_at_once_and_logs_next_tick() {
        let mut q = Queue::new();
        q.set_log(4);
        assert_eq!(q.submit(flip(1, 10), 10), Submit::ShowNow(Some(1)));
        let (t, log) = tick(&mut q, 20);
        assert_eq!(t.show, None);
        assert_eq!(log, [(0, 1)]);
        assert_eq!(q.first_free(), 1);
        assert!(t.interrupt);
    }

    #[test]
    fn future_flips_wait_for_their_target_and_newest_due_is_shown() {
        let mut q = Queue::new();
        q.set_log(4);
        assert_eq!(q.submit(flip(1, 100), 0), Submit::Queued);
        assert_eq!(q.submit(flip(2, 100), 0), Submit::Queued);
        assert_eq!(q.submit(flip(3, 300), 0), Submit::Queued);
        let (t, log) = tick(&mut q, 50);
        assert_eq!((t.show, log.len()), (None, 0));
        let (t, log) = tick(&mut q, 150);
        assert_eq!(t.show, Some(Some(2)));
        assert_eq!(log, [(0, 1), (1, 2)]);
        let (t, log) = tick(&mut q, 300);
        assert_eq!(t.show, Some(Some(3)));
        assert_eq!(log, [(2, 3)]);
        assert_eq!(q.queued(), 0);
    }

    #[test]
    fn a_due_flip_behind_a_queued_one_keeps_order() {
        let mut q = Queue::new();
        q.set_log(8);
        assert_eq!(q.submit(flip(1, 100), 0), Submit::Queued);
        assert_eq!(q.submit(flip(2, 0), 0), Submit::Queued);
        let (t, log) = tick(&mut q, 100);
        assert_eq!(t.show, Some(Some(2)));
        assert_eq!(log, [(0, 1), (1, 2)]);
    }

    #[test]
    fn log_wraps() {
        let mut q = Queue::new();
        q.set_log(2);
        for id in 1..=3 {
            q.submit(flip(id, 0), 0);
            tick(&mut q, 1);
        }
        assert_eq!(q.first_free(), 1);
    }

    #[test]
    fn interrupt_target_rules() {
        let mut q = Queue::new();
        q.set_log(8);
        q.set_interrupt_target(u64::MAX);
        q.submit(flip(1, 0), 0);
        assert!(!tick(&mut q, 1).0.interrupt);
        q.set_interrupt_target(3);
        q.submit(flip(2, 0), 0);
        assert!(!tick(&mut q, 2).0.interrupt);
        q.submit(flip(3, 0), 0);
        assert!(tick(&mut q, 3).0.interrupt);
        // Once per target.
        assert!(!tick(&mut q, 4).0.interrupt);
        q.set_interrupt_target(0);
        assert!(tick(&mut q, 5).0.interrupt);
    }

    #[test]
    fn cancel_drops_the_newest_range() {
        let mut q = Queue::new();
        q.set_log(8);
        for id in 1..=4 {
            q.submit(flip(id, 100), 0);
        }
        assert_eq!(q.cancel_from(3), 3);
        assert_eq!(q.queued(), 2);
        assert_eq!(q.cancel_from(9), 0);
        let (t, log) = tick(&mut q, 100);
        assert_eq!(t.show, Some(Some(2)));
        assert_eq!(log, [(0, 1), (1, 2)]);
    }

    #[test]
    fn full_queue_asks_for_a_drain() {
        let mut q = Queue::new();
        for id in 0..CAPACITY as u64 {
            assert_eq!(q.submit(flip(id + 1, 100), 0), Submit::Queued);
        }
        assert_eq!(q.submit(flip(99, 100), 0), Submit::Full);
    }

    #[test]
    fn without_a_log_nothing_is_written_but_progress_continues() {
        let mut q = Queue::new();
        q.submit(flip(1, 0), 5);
        let (_, log) = tick(&mut q, 6);
        assert!(log.is_empty());
        assert_eq!(q.last_logged(), 1);
    }
}
