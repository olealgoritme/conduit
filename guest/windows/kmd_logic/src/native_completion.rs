//! Event-driven completion of native HW queues (NVIDIA personality).
//!
//! The KMD learns that native GPU work finished by reading each queue's progress
//! fence (`ddi::hws`). Reading it from a timer alone costs up to one period per
//! completion (1 ms, 10 ms after a quiet second). This module is the pure half of
//! the interrupt-driven path that wakes the read instead:
//!
//! * The KMD's RM client allocates `NV01_EVENT_OS_EVENT`s on its subdevice for
//!   the engines' non-stall interrupts, each delivered on a control file of its
//!   own (`NV_ESC_ALLOC_OS_EVENT`). RM posts every non-stall interrupt of the
//!   engine to the file (`engineNonStallIntrNotify` -> `nv_post_event`), the
//!   host's event pump sees the file become readable and sends `EventReady` for
//!   it on the event queue, and the KMD's event DPC scans the progress fences
//!   at once. The events carry no data (`NV01_EVENT_WITHOUT_EVENT_DATA`): RM
//!   then queues nothing per interrupt, and the host's `poll` clears the pending
//!   flag, so the file never needs draining.
//! * What raises an interrupt: the D3D UMDs' copy work ends with
//!   `LAUNCH_DMA.INTERRUPT_TYPE = NON_BLOCKING` (a CE non-stall interrupt), the
//!   graphics work with `SET_REPORT_SEMAPHORE_D.OPERATION = TRAP` after the
//!   progress-fence release (610.88 D3D11 capture). Which RM notifier the GR
//!   trap reaches is not known, so the GR engine is watched both as a non-stall
//!   event and as a subdevice notification; each source has its own file and
//!   its own counter, and the run tells which one fires.
//! * The timer poll stays as the fallback. Per queue, [`Cover`] remembers who
//!   saw its progress last: while every queue with work outstanding is seen by
//!   events, the poll runs at the long period ([`PollPeriods::covered_100ns`]);
//!   any queue the poll had to find keeps it at the short one.
//! * [`LatHist`] / [`Inflight`]: submit-to-signal latency, by who saw the
//!   completion (at submit, by the poll, by an event).

/// `NV01_EVENT_OS_EVENT` (`cl0079.h`).
pub const NV01_EVENT_OS_EVENT: u32 = 0x79;
/// `NV01_EVENT_WITHOUT_EVENT_DATA` (`nvos.h`).
pub const EVENT_WITHOUT_DATA: u32 = 0x1000_0000;
/// `NV01_EVENT_NONSTALL_INTR` (`nvos.h`).
pub const EVENT_NONSTALL: u32 = 0x0800_0000;
/// `NV2080_NOTIFIERS_GRAPHICS` (`NV2080_NOTIFIERS_GR0`).
pub const NOTIFY_GR0: u32 = 12;
/// `NV2080_NOTIFIERS_CE0`; `CE1..CE9` follow it.
pub const NOTIFY_CE0: u32 = 23;
/// `NV2080_NOTIFIERS_FIFO_EVENT_MTHD`: the host's `NON_STALL_INTERRUPT` method.
pub const NOTIFY_FIFO_EVENT_MTHD: u32 = 35;
/// `NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION`.
pub const CTRL_EVENT_SET_NOTIFICATION: u32 = 0x2080_0301;
/// `NV2080_CTRL_EVENT_SET_NOTIFICATION_ACTION_REPEAT`.
pub const ACTION_REPEAT: u32 = 2;
/// `NV2080_CTRL_EVENT_SET_NOTIFICATION_ACTION_DISABLE`.
pub const ACTION_DISABLE: u32 = 0;
/// `sizeof(NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS)`.
pub const SET_NOTIFICATION_BYTES: usize = 20;
/// `NV_ESC_ALLOC_OS_EVENT` / `NV_ESC_FREE_OS_EVENT` (`nv-ioctl-numbers.h`).
pub const ESC_ALLOC_OS_EVENT: u32 = 0xCE;
pub const ESC_FREE_OS_EVENT: u32 = 0xCF;
/// `sizeof(nv_ioctl_alloc_os_event_t)`, and where RM writes its status in it.
pub const OS_EVENT_BYTES: usize = 16;
pub const OS_EVENT_STATUS_AT: usize = 12;
/// `sizeof(NV0005_ALLOC_PARAMETERS)`.
pub const EVENT_PARAMS_BYTES: usize = 24;

/// The RM handles of the KMD's completion events: `H_EVENT_BASE + 0x20 * source
/// + index`, apart from every other KMD handle range (`0x4b4d_0001..`,
/// `0x4b4d_1000..0x4b4d_1100`, `0x4b4d_2000..`, `0x4b4d_3000..`, `0x4e43_1000..`).
pub const H_EVENT_BASE: u32 = 0x4b4d_e000;

/// One interrupt source, watched on a control file of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// GR0 non-stall interrupts.
    Gr = 0,
    /// CE0..CE9 non-stall interrupts (the graphics copy engine is one of them).
    Ce = 1,
    /// The host's `NON_STALL_INTERRUPT` method.
    Host = 2,
    /// GR0 as a subdevice notification (`SET_NOTIFICATION` REPEAT), not a
    /// non-stall event: the route a stall-side GR notify (the UMDs' `TRAP`)
    /// would take if RM posts one.
    GrNotify = 3,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::Gr, Source::Ce, Source::Host, Source::GrNotify];

    /// Its bit in the `NvCompEv` knob and in the setup status.
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The `NV2080_NOTIFIERS_*` indices this source watches.
    pub const fn notifiers(self) -> &'static [u32] {
        match self {
            Source::Gr | Source::GrNotify => &[NOTIFY_GR0],
            Source::Ce => &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32],
            Source::Host => &[NOTIFY_FIFO_EVENT_MTHD],
        }
    }

    /// Whether its events are non-stall events (else subdevice notifications,
    /// which need `SET_NOTIFICATION` to be delivered).
    pub const fn nonstall(self) -> bool {
        !matches!(self, Source::GrNotify)
    }

    /// The `notifyIndex` of the event allocation for notifier `index`.
    pub const fn notify_index(self, index: u32) -> u32 {
        let flags = if self.nonstall() {
            EVENT_NONSTALL | EVENT_WITHOUT_DATA
        } else {
            EVENT_WITHOUT_DATA
        };
        index | flags
    }

    /// The RM handle of this source's `i`th event.
    pub const fn event_handle(self, i: usize) -> u32 {
        H_EVENT_BASE + 0x20 * self as u32 + i as u32
    }
}

/// `NvCompEv` unset: every source. 0 turns the event path off (the timer poll
/// alone, at its old 1 ms / 10 ms periods).
pub const KNOB_DEFAULT: u32 = 0xF;

/// The sources a knob value turns on.
pub fn sources(knob: u32) -> impl Iterator<Item = Source> {
    Source::ALL.into_iter().filter(move |s| knob & s.bit() != 0)
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// `nv_ioctl_alloc_os_event_t { hClient, hDevice, fd, Status }` (alloc and free).
/// `file` is the backend handle of the control file the escape is issued on and
/// that receives the events; the host turns it into its descriptor.
pub fn os_event_params(h_client: u32, h_device: u32, file: u32) -> [u8; OS_EVENT_BYTES] {
    let mut b = [0u8; OS_EVENT_BYTES];
    put32(&mut b, 0, h_client);
    put32(&mut b, 4, h_device);
    put32(&mut b, 8, file);
    b
}

/// `NV0005_ALLOC_PARAMETERS { hParentClient, hSrcResource, hClass, notifyIndex,
/// data }` for an OS event on `h_src` delivered to `file` (the host translates
/// `data`'s low word, a backend handle, into its descriptor).
pub fn event_params(
    h_client: u32,
    h_src: u32,
    notify_index: u32,
    file: u32,
) -> [u8; EVENT_PARAMS_BYTES] {
    let mut b = [0u8; EVENT_PARAMS_BYTES];
    put32(&mut b, 0, h_client);
    put32(&mut b, 4, h_src);
    put32(&mut b, 8, NV01_EVENT_OS_EVENT);
    put32(&mut b, 12, notify_index);
    put32(&mut b, 16, file);
    b
}

/// `NV2080_CTRL_EVENT_SET_NOTIFICATION_PARAMS { event, action, bNotifyState,
/// info32, info16 }`.
pub fn set_notification_params(event: u32, action: u32) -> [u8; SET_NOTIFICATION_BYTES] {
    let mut b = [0u8; SET_NOTIFICATION_BYTES];
    put32(&mut b, 0, event);
    put32(&mut b, 4, action);
    b
}

// ── the fallback poll's period ──────────────────────────────────────────────────────────────

/// The fallback poll's two periods, in 100 ns units (positive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollPeriods {
    /// While any queue's progress is found by the poll (or events are off).
    pub fast_100ns: i64,
    /// While every queue with outstanding work is seen by events.
    pub covered_100ns: i64,
}

/// `NvHwsPollUs` unset: the old 1 ms.
pub const POLL_US_DEFAULT: u32 = 1_000;
/// The high-resolution timer's floor on Windows (0.5 ms clock).
pub const POLL_US_MIN: u32 = 500;
pub const POLL_US_MAX: u32 = 10_000;
/// `NvHwsPollEvUs` unset: 4 ms, a safety net for a lost event.
pub const POLL_EV_US_DEFAULT: u32 = 4_000;
pub const POLL_EV_US_MAX: u32 = 20_000;

impl PollPeriods {
    /// From the knobs (0 = default), clamped; the covered period is never
    /// shorter than the fast one.
    pub const fn from_knobs(poll_us: u32, poll_ev_us: u32) -> Self {
        let fast = clamp(
            if poll_us == 0 { POLL_US_DEFAULT } else { poll_us },
            POLL_US_MIN,
            POLL_US_MAX,
        );
        let covered = clamp(
            if poll_ev_us == 0 { POLL_EV_US_DEFAULT } else { poll_ev_us },
            fast,
            POLL_EV_US_MAX,
        );
        Self {
            fast_100ns: fast as i64 * 10,
            covered_100ns: covered as i64 * 10,
        }
    }

    /// The period for a decision: `covered` = events are on and every queue
    /// with outstanding work is event-driven ([`Cover::event_driven`]).
    pub const fn period(&self, covered: bool) -> i64 {
        if covered {
            self.covered_100ns
        } else {
            self.fast_100ns
        }
    }
}

const fn clamp(v: u32, lo: u32, hi: u32) -> u32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

// ── who saw a queue's progress ──────────────────────────────────────────────────────────────

/// Who observed a progress-fence move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detect {
    /// `SubmitCommandToHwQueue`'s own look at the fence.
    Submit = 0,
    /// The timer poll.
    Poll = 1,
    /// An interrupt event's scan.
    Event = 2,
}

/// Consecutive event-found moves after which a queue counts as seen by events.
/// Events are per engine, not per queue: one that another queue's (or another
/// host client's) interrupt raised can find this queue's progress by chance, so
/// one is not enough. A single poll-found move undoes it: then a completion
/// waited for the poll, so at most one completion waits for the long period
/// before the poll is fast again.
pub const COVER_GAIN_AFTER: u8 = 2;

/// Whether a queue's completions arrive as events. A new queue is not covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cover {
    event_streak: u8,
}

impl Cover {
    pub const fn new() -> Self {
        Self { event_streak: 0 }
    }

    /// One observed move.
    pub fn note(&mut self, by: Detect) {
        match by {
            Detect::Event => self.event_streak = self.event_streak.saturating_add(1),
            Detect::Poll => self.event_streak = 0,
            Detect::Submit => {}
        }
    }

    pub const fn event_driven(&self) -> bool {
        self.event_streak >= COVER_GAIN_AFTER
    }
}

impl Default for Cover {
    fn default() -> Self {
        Self::new()
    }
}

// ── submit-to-signal latency ────────────────────────────────────────────────────────────────

/// Upper bounds (exclusive, microseconds) of the first seven buckets; the eighth
/// holds the rest.
pub const LAT_BOUNDS_US: [u64; 7] = [100, 250, 500, 1_000, 2_000, 4_000, 8_000];
pub const LAT_BUCKETS: usize = LAT_BOUNDS_US.len() + 1;

/// A latency histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatHist {
    pub buckets: [u32; LAT_BUCKETS],
    pub count: u32,
    pub sum_us: u64,
    pub max_us: u32,
}

impl LatHist {
    pub const fn new() -> Self {
        Self {
            buckets: [0; LAT_BUCKETS],
            count: 0,
            sum_us: 0,
            max_us: 0,
        }
    }

    /// The bucket of `us`.
    pub fn bucket(us: u64) -> usize {
        LAT_BOUNDS_US
            .iter()
            .position(|&bound| us < bound)
            .unwrap_or(LAT_BOUNDS_US.len())
    }

    /// Record one latency of `d_100ns`.
    pub fn add(&mut self, d_100ns: u64) {
        let us = d_100ns / 10;
        let b = Self::bucket(us);
        self.buckets[b] = self.buckets[b].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(u32::try_from(us).unwrap_or(u32::MAX));
    }

    /// The mean in microseconds (0 with no samples).
    pub fn mean_us(&self) -> u32 {
        if self.count == 0 {
            0
        } else {
            u32::try_from(self.sum_us / u64::from(self.count)).unwrap_or(u32::MAX)
        }
    }
}

impl Default for LatHist {
    fn default() -> Self {
        Self::new()
    }
}

/// The submissions of one queue whose completion was not seen yet: fence id and
/// submit time, oldest first. Fence ids rise per queue, so the oldest retire
/// first. Bounded: a submission that finds it full is not timed (counted by the
/// caller), which samples rather than skews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inflight<const N: usize> {
    ids: [u64; N],
    at: [u64; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Inflight<N> {
    pub const fn new() -> Self {
        Self {
            ids: [0; N],
            at: [0; N],
            head: 0,
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Note a submission of `id` at `now`. `false` = not timed (full, or an id
    /// not above the newest one held).
    pub fn push(&mut self, id: u64, now: u64) -> bool {
        if self.len == N {
            return false;
        }
        if self.len != 0 {
            let newest = (self.head + self.len - 1) % N;
            if id <= self.ids[newest] {
                return false;
            }
        }
        let at = (self.head + self.len) % N;
        self.ids[at] = id;
        self.at[at] = now;
        self.len += 1;
        true
    }

    /// The fence reads `value` at `now`: every held id <= `value` retired; `f`
    /// gets each one's latency (100 ns). Returns how many retired.
    pub fn retire(&mut self, value: u64, now: u64, mut f: impl FnMut(u64)) -> usize {
        let mut n = 0;
        while self.len != 0 && self.ids[self.head] <= value {
            f(now.saturating_sub(self.at[self.head]));
            self.head = (self.head + 1) % N;
            self.len -= 1;
            n += 1;
        }
        n
    }

    /// Forget everything (the queue was recovered or force-completed).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

impl<const N: usize> Default for Inflight<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    #[test]
    fn notify_indices_and_flags() {
        assert_eq!(Source::Gr.notify_index(NOTIFY_GR0), 12 | 0x0800_0000 | 0x1000_0000);
        assert_eq!(Source::GrNotify.notify_index(NOTIFY_GR0), 12 | 0x1000_0000);
        assert_eq!(Source::Host.notifiers(), &[35]);
        assert_eq!(Source::Ce.notifiers().len(), 10);
        assert_eq!(Source::Ce.notifiers()[0], NOTIFY_CE0);
        assert_eq!(Source::Ce.notifiers()[9], 32);
        assert!(Source::Ce.nonstall() && !Source::GrNotify.nonstall());
    }

    #[test]
    fn knob_selects_sources() {
        assert_eq!(sources(0).count(), 0);
        assert_eq!(sources(KNOB_DEFAULT).collect::<Vec<_>>(), Source::ALL.to_vec());
        assert_eq!(sources(0b0101).collect::<Vec<_>>(), [Source::Gr, Source::Host]);
        assert_eq!(sources(1 << 8).count(), 0);
    }

    #[test]
    fn event_handles_are_distinct_and_apart() {
        let mut all = Vec::new();
        for s in Source::ALL {
            for i in 0..s.notifiers().len() {
                all.push(s.event_handle(i));
            }
        }
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
        for h in all {
            assert!((0x4b4d_e000..0x4b4d_e080).contains(&h));
            assert_ne!(h, crate::rm_client::H_DEVICE);
            assert_ne!(h, crate::rm_client::H_SUBDEVICE);
        }
    }

    #[test]
    fn params_layouts() {
        let o = os_event_params(0xc1d0_0001, 0x4b4d_0002, 7);
        assert_eq!(&o[0..4], &0xc1d0_0001u32.to_le_bytes());
        assert_eq!(&o[4..8], &0x4b4d_0002u32.to_le_bytes());
        assert_eq!(&o[8..12], &7u32.to_le_bytes());
        assert_eq!(&o[12..16], &[0; 4]);
        let e = event_params(0xc1d0_0001, 0x4b4d_0002, 0x1800_000c, 9);
        assert_eq!(&e[8..12], &0x79u32.to_le_bytes());
        assert_eq!(&e[12..16], &0x1800_000cu32.to_le_bytes());
        // `data` is a 64-bit field; the host reads the low word as a handle.
        assert_eq!(&e[16..24], &9u64.to_le_bytes());
        let s = set_notification_params(NOTIFY_GR0, ACTION_REPEAT);
        assert_eq!(s.len(), 20);
        assert_eq!(&s[0..8], &[12, 0, 0, 0, 2, 0, 0, 0]);
    }

    #[test]
    fn poll_periods_clamp() {
        let d = PollPeriods::from_knobs(0, 0);
        assert_eq!(d.fast_100ns, 10_000);
        assert_eq!(d.covered_100ns, 40_000);
        assert_eq!(d.period(false), 10_000);
        assert_eq!(d.period(true), 40_000);
        let low = PollPeriods::from_knobs(1, 1);
        assert_eq!(low.fast_100ns, 5_000);
        assert_eq!(low.covered_100ns, 5_000);
        let high = PollPeriods::from_knobs(99_999, 99_999);
        assert_eq!(high.fast_100ns, 100_000);
        assert_eq!(high.covered_100ns, 200_000);
        // The covered period is never below the fast one.
        let odd = PollPeriods::from_knobs(3_000, 1_000);
        assert_eq!(odd.covered_100ns, 30_000);
    }

    #[test]
    fn cover_needs_two_events_and_is_lost_to_one_poll() {
        let mut c = Cover::new();
        assert!(!c.event_driven());
        c.note(Detect::Submit);
        c.note(Detect::Event);
        // One event may be another engine's, by chance.
        assert!(!c.event_driven());
        c.note(Detect::Submit);
        c.note(Detect::Event);
        assert!(c.event_driven());
        for _ in 0..1000 {
            c.note(Detect::Event);
        }
        assert!(c.event_driven());
        // A completion the poll had to find: fast again at once.
        c.note(Detect::Poll);
        assert!(!c.event_driven());
        c.note(Detect::Event);
        c.note(Detect::Poll);
        c.note(Detect::Event);
        assert!(!c.event_driven());
        c.note(Detect::Event);
        assert!(c.event_driven());
    }

    #[test]
    fn histogram_buckets() {
        assert_eq!(LatHist::bucket(0), 0);
        assert_eq!(LatHist::bucket(99), 0);
        assert_eq!(LatHist::bucket(100), 1);
        assert_eq!(LatHist::bucket(999), 3);
        assert_eq!(LatHist::bucket(1_000), 4);
        assert_eq!(LatHist::bucket(7_999), 6);
        assert_eq!(LatHist::bucket(8_000), 7);
        assert_eq!(LatHist::bucket(u64::MAX), 7);
        let mut h = LatHist::new();
        assert_eq!(h.mean_us(), 0);
        h.add(500); // 50 us
        h.add(15_000); // 1.5 ms
        assert_eq!(h.buckets[0], 1);
        assert_eq!(h.buckets[4], 1);
        assert_eq!(h.count, 2);
        assert_eq!(h.mean_us(), 775);
        assert_eq!(h.max_us, 1_500);
    }

    #[test]
    fn inflight_retires_in_order() {
        let mut q: Inflight<4> = Inflight::new();
        assert!(q.push(10, 100));
        assert!(q.push(11, 200));
        assert!(!q.push(11, 250)); // not above the newest
        assert!(q.push(12, 300));
        let mut seen = Vec::new();
        assert_eq!(q.retire(9, 1_000, |d| seen.push(d)), 0);
        assert_eq!(q.retire(11, 1_000, |d| seen.push(d)), 2);
        assert_eq!(seen, [900, 800]);
        assert_eq!(q.len(), 1);
        // Wraps around the ring.
        assert!(q.push(13, 400));
        assert!(q.push(14, 500));
        assert!(q.push(15, 600));
        assert!(!q.push(16, 700)); // full
        seen.clear();
        assert_eq!(q.retire(u64::MAX, 1_000, |d| seen.push(d)), 4);
        assert_eq!(seen, [700, 600, 500, 400]);
        assert!(q.is_empty());
        // A time before the submit never underflows.
        assert!(q.push(20, 5_000));
        seen.clear();
        q.retire(20, 1_000, |d| seen.push(d));
        assert_eq!(seen, [0]);
        q.push(21, 1);
        q.clear();
        assert!(q.is_empty());
    }
}
