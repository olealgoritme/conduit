//! Flip retirement latency, the tick arithmetic of the vsync chain, and the `FlipAnnounce`
//! decision table (pure logic; no registry, no WDK type). The I/O halves are
//! `kmd_render/src/ddi/flip_lat.rs` (the measurement: statics, the ring of issued flips, the
//! histograms, the mirror) and `kmd_render/src/ddi/flip_announce.rs` (the announce at
//! `SetVidPnSourceAddress`). Design, the model and the hardware checklist:
//! `docs/kmd-rm-client.md` 15.18.15.
//!
//! WHAT THE MODEL IS. A flip dxgkrnl issues through `DxgkDdiSetVidPnSourceAddress` is RETIRED
//! (dxgkrnl lets the compositor reuse the previous buffer and issues the next queued flip) when a
//! `DXGK_INTERRUPT_CRTC_VSYNC` carries the flip's address. This driver synthesises that
//! interrupt from its own heartbeat (`adapter/kobj.rs::service_vsync_tick`), and the address the
//! tick carries is `last_primary_address`, written by whoever publishes (the HPD worker after
//! it programmed the flip; with `FlipAnnounce` the DDI itself). The tick reads the address at
//! its START, so a flip is carried by the first tick that starts AFTER its address was
//! published. [`retire_ticks`] is that rule; everything else here follows from it.

/// Number of buckets of every histogram of this module.
pub const BUCKETS: usize = 8;

/// Upper edges of the latency histograms, microseconds: bucket `i` holds `us < LAT_EDGES_US[i]`,
/// the last bucket (7) holds everything from the final edge on. 4.2 ms is one period at 240 Hz
/// (4.17), 8.4 ms two.
pub const LAT_EDGES_US: [u32; BUCKETS - 1] = [500, 1_000, 2_000, 4_200, 8_400, 17_000, 50_000];

/// Upper edges of the heartbeat-lateness histogram, microseconds (`VsLate0`..`VsLate7`).
pub const LATE_EDGES_US: [u32; BUCKETS - 1] = [100, 250, 500, 1_000, 2_000, 4_200, 8_400];

/// Inter-flip gap edges in HALF periods: bucket `i` holds `gap < GAP_EDGES_HALF[i] / 2` periods
/// (1.5, 2.5, 3.5, 5, 9, 17, 50 periods); the last bucket is 50 periods and more. A flip every
/// vblank at the committed refresh rate lands in bucket 0 (one period, plus the jitter).
pub const GAP_EDGES_HALF: [u32; BUCKETS - 1] = [3, 5, 7, 10, 18, 34, 100];

/// An inter-flip gap longer than this is idleness (the desktop had nothing to show), not a
/// stall: it is counted apart (`IfIdle`) and left out of the histogram. 250 ms.
pub const IDLE_BREAK_100NS: u64 = 2_500_000;

/// A gap over this is one stalled frame at 240 Hz (`IfStall8`). 8 ms.
pub const STALL_100NS: u64 = 80_000;

/// 100 ns units per microsecond.
const UNITS_PER_US: u64 = 10;

/// Upper edges of the ForeignFlip host round trip histogram, microseconds (`FfRttB0..7`): the
/// time the worker saw from submitting (or sending) a `ScanoutFlip` to its answer.
pub const RTT_EDGES_US: [u32; BUCKETS - 1] = [250, 500, 1_000, 2_000, 4_200, 8_400, 17_000];

/// The bucket of a host round trip in microseconds (`FfRttB0..7`).
pub const fn rtt_bucket(us: u64) -> usize {
    bucket_of(us, &RTT_EDGES_US)
}

/// The least time between two host flips the foreign presenter enforces. With the pipelined flip
/// the clock starts at the SUBMIT, so the worker's own latency jitter (a wake that was 0.1 ms
/// late for one flip and on time for the next) shifts the next flip's due time by that jitter
/// and, with the due time exactly one period, parks every flip that arrives a little early
/// behind a timed wait of at least a millisecond: the frame behind it is then overwritten
/// (single owed slot) and one in several is never shown, a beat. A flip is allowed 3/4 of a
/// period after the previous one instead (still at most one per vblank on average: flips arrive
/// once per tick). The synchronous round trip keeps the full period.
pub const fn paced_interval(period_100ns: u64, pipelined: bool) -> u64 {
    if pipelined {
        period_100ns - period_100ns / 4
    } else {
        period_100ns
    }
}

/// The bucket of a latency in microseconds against `edges`.
pub const fn bucket_of(us: u64, edges: &[u32; BUCKETS - 1]) -> usize {
    let mut i = 0;
    while i < BUCKETS - 1 {
        if us < edges[i] as u64 {
            return i;
        }
        i += 1;
    }
    BUCKETS - 1
}

/// The bucket of a retire / host-submit latency in microseconds (`FlipLat0..7`, `FlipHostLat0..7`).
pub const fn lat_bucket(us: u64) -> usize {
    bucket_of(us, &LAT_EDGES_US)
}

/// [`lat_bucket`] of a latency in 100 ns units.
pub const fn lat_bucket_100ns(d: u64) -> usize {
    lat_bucket(d / UNITS_PER_US)
}

/// The bucket of a heartbeat's lateness in 100 ns units (`VsLate0..7`).
pub const fn late_bucket_100ns(d: u64) -> usize {
    bucket_of(d / UNITS_PER_US, &LATE_EDGES_US)
}

/// The bucket of an inter-flip gap, in vblank periods of `period_100ns` (`IfGap0..7`). A zero
/// period (rate unknown) puts every gap in the last bucket rather than dividing by it.
pub const fn gap_bucket(gap_100ns: u64, period_100ns: u64) -> usize {
    if period_100ns == 0 {
        return BUCKETS - 1;
    }
    let mut i = 0;
    while i < BUCKETS - 1 {
        // gap < edge/2 periods  <=>  2 * gap < edge * period (saturating u64: no 128-bit
        // arithmetic in the kernel; a gap that saturates is far past every edge anyway)
        if gap_100ns.saturating_mul(2) < (GAP_EDGES_HALF[i] as u64).saturating_mul(period_100ns) {
            return i;
        }
        i += 1;
    }
    BUCKETS - 1
}

/// What one retiring tick says about the interval since the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// The first retirement (nothing to compare against), or the clock went backwards.
    First,
    /// Longer than [`IDLE_BREAK_100NS`]: nothing was flipping.
    Idle,
    /// An interval between two retirements.
    Interval {
        bucket: usize,
        /// Longer than [`STALL_100NS`].
        stall: bool,
    },
}

/// Classify the gap between the previous retirement (`prev`, 100 ns, 0 = none) and this one.
pub const fn classify_gap(prev: u64, now: u64, period_100ns: u64) -> Gap {
    if prev == 0 || now < prev {
        return Gap::First;
    }
    let gap = now - prev;
    if gap > IDLE_BREAK_100NS {
        return Gap::Idle;
    }
    Gap::Interval {
        bucket: gap_bucket(gap, period_100ns),
        stall: gap > STALL_100NS,
    }
}

/// An UPPER BOUND of the `permille`/1000 quantile (990 = p99) of a histogram over `edges`
/// (microseconds): the upper edge of the bucket holding the quantile's rank, and for the open
/// last bucket (or whenever the edge exceeds it) the observed maximum `max_us`. It
/// never underestimates, and overestimates by less than the width of one bucket; "p99 below
/// 5 ms" is therefore PROVEN by an estimate of 4200 or below (and by a maximum below 5000),
/// while an estimate of 8400 says only "between 4.2 and 8.4 ms". The rank is `ceil(total *
/// permille / 1000)`, at least 1. An empty histogram answers 0.
pub fn percentile_upper_us(
    counts: &[u32; BUCKETS],
    edges: &[u32; BUCKETS - 1],
    permille: u32,
    max_us: u32,
) -> u32 {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total * permille.min(1000) as u64).div_ceil(1000)).max(1);
    let mut cum = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        cum += c as u64;
        if cum >= rank {
            return if i < BUCKETS - 1 {
                edges[i].min(max_us.max(1))
            } else {
                max_us
            };
        }
    }
    max_us
}

/// [`percentile_upper_us`] of a retire latency histogram.
pub fn lat_percentile_us(counts: &[u32; BUCKETS], permille: u32, max_us: u32) -> u32 {
    percentile_upper_us(counts, &LAT_EDGES_US, permille, max_us)
}

/// An upper bound, in microseconds, of the `permille` quantile of the inter-flip gaps (the
/// gap histogram's edges are periods): the bucket's upper edge times the period; the open
/// bucket answers `max_us`.
pub fn gap_percentile_us(
    counts: &[u32; BUCKETS],
    period_100ns: u64,
    permille: u32,
    max_us: u32,
) -> u32 {
    let total: u64 = counts.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total * permille.min(1000) as u64).div_ceil(1000)).max(1);
    let mut cum = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        cum += c as u64;
        if cum >= rank {
            if i == BUCKETS - 1 {
                return max_us;
            }
            let edge_us = (GAP_EDGES_HALF[i] as u64 * period_100ns) / 2 / UNITS_PER_US;
            return (edge_us.min(u32::MAX as u64) as u32).min(max_us.max(1));
        }
    }
    max_us
}

/// Share of the vblanks that retired a flip, in permille (`VbUsed` of `VbTicks`).
pub const fn used_permille(used: u32, ticks: u32) -> u32 {
    if ticks == 0 {
        return 0;
    }
    let p = (used as u64 * 1000) / ticks as u64;
    if p > 1000 {
        1000
    } else {
        p as u32
    }
}

// ---- the ring of issued flips ------------------------------------------------------------------

/// Slots of the ring of issued flips. More than any queue depth dxgkrnl is allowed
/// (`FlipQueueN` is clamped to 16).
pub const RING: usize = 16;

/// The flip a retiring tick matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Its slot.
    pub idx: usize,
    /// From the flip's `SetVidPnSourceAddress` entry to the tick, in 100 ns.
    pub latency_100ns: u64,
    /// Older issued flips still unretired when this one was carried (bit per slot): the tick
    /// named a newer address than theirs. Whether dxgkrnl retires those too is the documented
    /// unknown (`FlipSkip` counts them, the checklist's queue-depth experiment decides).
    pub skipped: u32,
}

/// Find the flip a tick that carried `phys` (and started at `tick_t`) retired: the NEWEST live
/// slot (`live` bit set) with that address that was issued at or before the tick. `head` is
/// the count of slots ever written (the next write goes to `head % RING`), so newest-first is
/// `head - 1` down. A zero address matches nothing.
pub fn retire_match(
    addrs: &[u64; RING],
    times: &[u64; RING],
    live: u32,
    head: u32,
    phys: u64,
    tick_t: u64,
) -> Option<Hit> {
    retire_match_at(addrs, times, live, head, phys, tick_t, tick_t)
}

/// [`retire_match`] with two clocks: `match_t` is the instant AFTER `phys` was read (a flip
/// issued at or before it may be what `phys` names; one issued later cannot be), `lat_t` the
/// tick's start, the end of the latency measured. Reading the tick's start time, then the
/// address, left a window in which an announce that landed between the two (the address already
/// in `phys`, its issue time after the start) was refused and credited one tick late.
pub fn retire_match_at(
    addrs: &[u64; RING],
    times: &[u64; RING],
    live: u32,
    head: u32,
    phys: u64,
    match_t: u64,
    lat_t: u64,
) -> Option<Hit> {
    if phys == 0 {
        return None;
    }
    let mut hit: Option<usize> = None;
    let mut skipped = 0u32;
    for k in 0..RING {
        let idx = (head.wrapping_sub(1 + k as u32) as usize) % RING;
        if live & (1 << idx) == 0 {
            continue;
        }
        match hit {
            None => {
                if addrs[idx] == phys && times[idx] <= match_t {
                    hit = Some(idx);
                }
            }
            Some(_) => skipped |= 1 << idx,
        }
    }
    hit.map(|idx| Hit {
        idx,
        latency_100ns: lat_t.saturating_sub(times[idx]),
        skipped,
    })
}

// ---- the tick arithmetic -----------------------------------------------------------------------

/// How the address of a flip reaches `last_primary_address`, i.e. what decides the first tick
/// that can carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish {
    /// The default: the HPD worker publishes after programming, and it is woken ONLY by the
    /// vsync tick (`service_vsync_tick` signals it when a programming is pending). A flip
    /// issued after tick N is seen by the worker at tick N+1 and published after that tick has
    /// already read its address.
    WorkerTickWake,
    /// The worker is woken at issue (`FfAsyncWin` / `FlipAnnounce` 2: the DDI queues the DPC
    /// that signals it) and publishes `worker_latency` after the flip.
    WorkerEarlyWake,
    /// `FlipAnnounce` 1: the DDI publishes the address itself, at issue.
    Announced,
}

/// One flip of the chain: when dxgkrnl issues it, how the address is published, how long the
/// worker takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    /// The vblank period, 100 ns (`vsync_deadline::period_100ns`).
    pub period_100ns: u64,
    /// The flip is issued this long after a tick STARTED, `0 <= phase < period` (0 = inside the
    /// tick's own notification: what a flip queued behind the previous one's retirement sees).
    pub phase_100ns: u64,
    /// From issue to the worker having published (wake plus programming), when the worker
    /// publishes. Ignored for [`Publish::Announced`].
    pub worker_latency_100ns: u64,
    pub publish: Publish,
}

/// The tick that retires the flip, counted from the tick before the flip was issued
/// (`1` = the very next tick), and the issue-to-retire latency in 100 ns.
///
/// The rule: the tick that starts at `k * period` (k >= 1, counting from the tick the flip
/// follows) carries the address iff it was published BEFORE that instant; the first such `k`.
pub const fn retire_ticks(c: &Chain) -> (u32, u64) {
    let p = c.period_100ns;
    if p == 0 {
        return (0, 0);
    }
    // When the address is published, measured from the tick the flip follows.
    let t_pub = match c.publish {
        Publish::Announced => c.phase_100ns,
        Publish::WorkerEarlyWake => c.phase_100ns + c.worker_latency_100ns,
        // Seen by the worker at the NEXT tick (at `p`), published `worker_latency` after it.
        Publish::WorkerTickWake => p + c.worker_latency_100ns,
    };
    // Smallest k >= 1 with k * p > t_pub (published strictly before that tick reads it).
    let k = t_pub / p + 1;
    let latency = k * p - c.phase_100ns;
    (k as u32, latency)
}

/// Flips per second, in millihertz (1000 = one per second), of a chain that is SATURATED
/// (DWM always has the next flip queued, so dxgkrnl issues it the moment the previous
/// retires: phase 0), at queue depth `depth` (`MaxQueuedFlipOnVSync`), when a flip takes
/// `ticks` ticks to retire. One flip per tick is the most a one-address-per-tick vsync can
/// retire. With depth 1 the rate is `1 / (ticks * period)`. A deeper queue helps only if
/// dxgkrnl retires a flip whose address was overwritten before a tick carried it
/// (`retires_coalesced`, the UNKNOWN of 15.18.15): then `depth` flips overlap; if it does not,
/// the extra flips never retire and the rate is that of depth 1 (and the queue stalls
/// when it fills, which the checklist's experiment watches for).
pub const fn saturated_rate_mhz(
    period_100ns: u64,
    ticks: u32,
    depth: u32,
    retires_coalesced: bool,
) -> u64 {
    if period_100ns == 0 || ticks == 0 {
        return 0;
    }
    // 1e7 units per second * 1000 mHz per Hz.
    const SCALE: u64 = 10_000_000_000;
    let per_chain = SCALE / (ticks as u64 * period_100ns);
    let cap = SCALE / period_100ns;
    if retires_coalesced && depth > 1 {
        let overlapped = per_chain * depth as u64;
        if overlapped > cap {
            cap
        } else {
            overlapped
        }
    } else {
        per_chain
    }
}

/// An upper bound of the issue-to-retire latency (the p100 of the chain, 100 ns) for a flip
/// issued at ANY phase, plus the heartbeat's own lateness `timer_late_100ns` (a late tick
/// starts later and may carry a newer address; it can also be later itself): the worst phase
/// is "just after a tick" (phase 0).
pub const fn latency_bound_100ns(
    period_100ns: u64,
    worker_latency_100ns: u64,
    publish: Publish,
    timer_late_100ns: u64,
) -> u64 {
    let c = Chain {
        period_100ns,
        phase_100ns: 0,
        worker_latency_100ns,
        publish,
    };
    let (_, latency) = retire_ticks(&c);
    latency + timer_late_100ns
}

// ---- the announce decision --------------------------------------------------------------------

/// `FlipAnnounce` in force (the registry value 0, 1, 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceMode {
    /// 0, the default: the worker publishes after its programming, woken by the tick.
    Off,
    /// 1: announce at the DDI the flips of foreign allocations `ForeignFlip` already accepted
    /// (the NVK DWM's swap chain) and wake the worker early.
    Foreign,
    /// 2: announce EVERY flip whose predecessor is fully programmed (the Venus direct and
    /// linear-copy paths included) and wake the worker early.
    All,
}

impl AnnounceMode {
    /// From the registry value: 0 off, 1 foreign classes, anything larger all flips.
    pub const fn from_knob(v: u32) -> Self {
        match v {
            0 => AnnounceMode::Off,
            1 => AnnounceMode::Foreign,
            _ => AnnounceMode::All,
        }
    }
    pub const fn code(self) -> u32 {
        match self {
            AnnounceMode::Off => 0,
            AnnounceMode::Foreign => 1,
            AnnounceMode::All => 2,
        }
    }
    /// Whether the DDI may publish the address at all.
    pub const fn announces(self) -> bool {
        !matches!(self, AnnounceMode::Off)
    }
}

/// The `FlipAnnounce` value when the service key has none: 2, the Venus class announced (the
/// foreign class waits for `FlipAnnForeign`). 2 on 332.1 hardware: 238 fps overlay, 244 flips and
/// 244 used vblanks a second, no artifacts. `FlipAnnounce` 0 in the service key turns it off.
pub const DEFAULT_KNOB: u32 = 2;

/// Whether the DDI asks for the DPC that wakes the worker at issue: an announce mode, or the
/// `FlipEarlyWake` knob alone (early programming without an early retire).
pub const fn wakes_early(mode: AnnounceMode, early_wake_knob: u32) -> bool {
    mode.announces() || early_wake_knob != 0
}

/// Why a flip was not announced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAnnounce {
    /// The knob is 0.
    Off,
    /// The flip names no address (zero): nothing to carry.
    NoAddress,
    /// The handle paired with no allocation of this transport generation (the DDI's existing
    /// kept-picture lane completes it).
    NoResource,
    /// An earlier flip is still being programmed (the worker's pending slot or the programming
    /// gate is not idle): at most one unprogrammed flip is ever announced, so the buffer a
    /// copy or a bind still reads can never be handed back early. The flip retires the
    /// normal way, when the worker publishes it.
    Busy,
    /// Mode 1 only: the allocation was never accepted by `ForeignFlip` (its first flip, or one
    /// the arm refused): the worker decides, as before.
    Unknown,
    /// The foreign arm is failing (the presenter gave up or a flip failed within the retry
    /// pause): the Venus or kept-picture path completes the flip.
    Failing,
    /// Mode 2 and the flip names a foreign or hollow allocation while `FlipAnnForeign` is 0:
    /// the Venus path is announced, the foreign one waits until it was validated.
    ForeignOff,
}

impl NoAnnounce {
    /// The code `FaNoWhy` mirrors.
    pub const fn code(self) -> u32 {
        match self {
            NoAnnounce::Off => 1,
            NoAnnounce::NoAddress => 2,
            NoAnnounce::NoResource => 3,
            NoAnnounce::Busy => 4,
            NoAnnounce::Unknown => 5,
            NoAnnounce::Failing => 6,
            NoAnnounce::ForeignOff => 7,
        }
    }
}

/// What the DDI decides for one flip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Announce {
    Yes,
    No(NoAnnounce),
}

/// The facts the DDI can read with atomics only.
#[derive(Clone, Copy, Debug)]
pub struct AnnounceFacts {
    pub mode: AnnounceMode,
    pub address: u64,
    /// The allocation's resource id, `None` when the handle did not pair.
    pub resource: Option<u32>,
    /// The flip names a foreign or hollow allocation (`flip_completion::Source` not `Venus`),
    /// from the KMD's own lock-free record of the allocation.
    pub foreign_class: bool,
    /// `FlipAnnForeign` is on: mode 2 may announce foreign classes too.
    pub foreign_ok: bool,
    /// The worker is idle: nothing pending and the programming gate lowered, read BEFORE this
    /// flip raises it (the previous flip's bind, copy and completion are all finished).
    pub idle: bool,
    /// `ForeignFlip` accepted this resource at an earlier programming and nothing has
    /// invalidated that since (a file closed, a refusal, a failure, a mode change).
    pub accepted: bool,
    /// `foreign_flip::failing` from atomics alone.
    pub failing: bool,
}

/// The announce decision table, first match wins:
///
/// | mode | address | resource | worker idle | accepted | failing | answer |
/// |---|---|---|---|---|---|---|
/// | 0 | | | | | | `No(Off)` |
/// | | 0 | | | | | `No(NoAddress)` |
/// | | | none or 0 | | | | `No(NoResource)` |
/// | 2 | | | foreign class, `FlipAnnForeign` 0 | | | `No(ForeignOff)` |
/// | | | | no | | | `No(Busy)` |
/// | 1 | | | | no | | `No(Unknown)` |
/// | | | | | yes | yes | `No(Failing)` |
/// | 1 or 2 | nonzero | some | yes | yes (1) / any (2) | no | `Yes` |
pub const fn announce_decide(f: &AnnounceFacts) -> Announce {
    if !f.mode.announces() {
        return Announce::No(NoAnnounce::Off);
    }
    if f.address == 0 {
        return Announce::No(NoAnnounce::NoAddress);
    }
    match f.resource {
        None | Some(0) => return Announce::No(NoAnnounce::NoResource),
        Some(_) => {}
    }
    if matches!(f.mode, AnnounceMode::All) && f.foreign_class && !f.foreign_ok {
        return Announce::No(NoAnnounce::ForeignOff);
    }
    if !f.idle {
        return Announce::No(NoAnnounce::Busy);
    }
    if matches!(f.mode, AnnounceMode::Foreign) && !f.accepted {
        return Announce::No(NoAnnounce::Unknown);
    }
    if f.accepted && f.failing {
        return Announce::No(NoAnnounce::Failing);
    }
    Announce::Yes
}

/// What a publisher on the worker's side does with `address` while `announced` is the address
/// the newest announce is waiting for the worker to confirm (0 = none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerPublish {
    /// Nothing is announced: store the address as always.
    Store,
    /// This IS the announced flip: the address is already published; swallow the store
    /// (idempotent: `FlipPub` counts a flip once) and clear the announcement (`FaWorker`).
    Confirm,
    /// A NEWER flip was announced since: storing this older address would hand the heartbeat
    /// a regressed address and cost the newer flip a tick. Skip it (`FaLate`).
    Late,
}

pub const fn worker_publish(announced: u64, address: u64) -> WorkerPublish {
    if announced == 0 {
        WorkerPublish::Store
    } else if announced == address {
        WorkerPublish::Confirm
    } else {
        WorkerPublish::Late
    }
}

/// The service-key counter names of the I/O halves (`ddi/flip_lat.rs`, `ddi/flip_announce.rs`):
/// at most 14 characters, unique across `kmd_render` and `kmd_logic`. The indexed histograms
/// (`FlipLat0`..`7`, `FlipPrgLat0`..`7`, `FlipHostLat0`..`7`, `IfGap0`..`7`, `VsLate0`..`7`,
/// `FlipPh0`..`3`) are built in `flip_lat.rs` from their prefix and a digit.
pub const COUNTERS: &[&str] = &[
    // retire latency, SetVidPnSourceAddress entry to the CRTC_VSYNC that carried it
    "FlipLat0", "FlipLat1", "FlipLat2", "FlipLat3", "FlipLat4", "FlipLat5", "FlipLat6", "FlipLat7",
    "FlipMaxUs", "FlipMaxT", "FlipMaxSite", "FlipMaxFl", "FlipP50Us", "FlipP99Us", "FlipRetN",
    "FlipSkip", "FlipLive", "FlipLatOn",
    // entry to the worker's publication (the programming: bind accepted, foreign take)
    "FlipPrgLat0", "FlipPrgLat1", "FlipPrgLat2", "FlipPrgLat3", "FlipPrgLat4", "FlipPrgLat5",
    "FlipPrgLat6", "FlipPrgLat7", "FlipPrgMax", "FlipPrgP99",
    // entry to the host flip submitted (ForeignFlip)
    "FlipHostLat0", "FlipHostLat1", "FlipHostLat2", "FlipHostLat3", "FlipHostLat4",
    "FlipHostLat5", "FlipHostLat6", "FlipHostLat7", "FlipHostMax", "FlipHostP99",
    // where in the vblank the DDI ran
    "FlipInDpc", "FlipPh0", "FlipPh1", "FlipPh2", "FlipPh3",
    // inter-flip interval at the retire timestamps
    "IfGap0", "IfGap1", "IfGap2", "IfGap3", "IfGap4", "IfGap5", "IfGap6", "IfGap7",
    "IfGapMax", "IfN", "IfIdle", "IfStall8", "IfP99Us",
    // vblank utilisation
    "VbUsed", "VbTicks", "VbUsedPm",
    // heartbeat lateness
    "VsLate0", "VsLate1", "VsLate2", "VsLate3", "VsLate4", "VsLate5", "VsLate6", "VsLate7",
    "VsLateMaxUs",
    // announce
    "FaKnob", "FaEarly", "FaDdi", "FaWorker", "FaRefuse", "FaLate", "FaTick", "FaNo", "FaNoWhy",
    "FaNoBusy", "FaNoUnk", "FaNoFail", "FaNoOther", "FaNoFgn",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const P240: u64 = 41_667;
    const P60: u64 = 166_667;

    #[test]
    fn latency_buckets_at_every_edge() {
        assert_eq!(lat_bucket(0), 0);
        assert_eq!(lat_bucket(499), 0);
        assert_eq!(lat_bucket(500), 1);
        assert_eq!(lat_bucket(999), 1);
        assert_eq!(lat_bucket(1_000), 2);
        assert_eq!(lat_bucket(1_999), 2);
        assert_eq!(lat_bucket(2_000), 3);
        assert_eq!(lat_bucket(4_199), 3);
        assert_eq!(lat_bucket(4_200), 4);
        assert_eq!(lat_bucket(8_399), 4);
        assert_eq!(lat_bucket(8_400), 5);
        assert_eq!(lat_bucket(16_999), 5);
        assert_eq!(lat_bucket(17_000), 6);
        assert_eq!(lat_bucket(49_999), 6);
        assert_eq!(lat_bucket(50_000), 7);
        assert_eq!(lat_bucket(u64::MAX), 7);
        // 100 ns units: 4.17 ms is the 240 Hz period and sits under the 4.2 ms edge.
        assert_eq!(lat_bucket_100ns(P240), 3);
        assert_eq!(lat_bucket_100ns(2 * P240), 4);
    }

    #[test]
    fn rtt_buckets_and_pacing() {
        assert_eq!(rtt_bucket(0), 0);
        assert_eq!(rtt_bucket(249), 0);
        assert_eq!(rtt_bucket(250), 1);
        assert_eq!(rtt_bucket(999), 2);
        assert_eq!(rtt_bucket(1_000), 3);
        assert_eq!(rtt_bucket(4_199), 4);
        assert_eq!(rtt_bucket(4_200), 5);
        assert_eq!(rtt_bucket(8_400), 6);
        assert_eq!(rtt_bucket(17_000), 7);
        // the 240 Hz period: pipelined 3/4 (3.1 ms), synchronous the whole period
        assert_eq!(paced_interval(P240, false), P240);
        assert_eq!(paced_interval(P240, true), P240 - P240 / 4);
        assert!(paced_interval(P240, true) > P240 / 2);
        assert_eq!(paced_interval(0, true), 0);
    }

    #[test]
    fn a_pacing_slack_keeps_every_flip_of_a_tick_aligned_chain() {
        // Flips arrive once per period; the worker submits each `lat` after its arrival and the
        // presenter lets the next one go `interval` after the previous SUBMIT, or when it
        // arrives, whichever is later (a timed wait of at least 1 ms for a flip that is early).
        // Count the arrivals that are overwritten (a newer one arrived before it flew).
        fn dropped(interval: u64, lats: &[u64]) -> u32 {
            let mut last_submit = 0u64;
            let mut dropped = 0u32;
            let mut t_free = 0u64; // the worker's next flip time for the owed frame
            let mut owed: Option<u64> = None;
            for n in 0..400u64 {
                let arrive = n * P240;
                // before this arrival, fly the owed frame if it is due
                if let Some(a) = owed {
                    let due = (last_submit + interval).max(a);
                    if due < arrive {
                        last_submit = due;
                        owed = None;
                        t_free = due;
                    }
                }
                if owed.is_some() {
                    dropped += 1; // overwritten by the newer one
                }
                let lat = lats[(n as usize) % lats.len()];
                owed = Some(arrive + lat);
                let _ = t_free;
            }
            dropped
        }
        // worker latency alternating 0.1 ms and 2 ms: the full-period pacing drops frames,
        // the 3/4 period does not
        let lats = [1_000u64, 20_000, 1_000, 1_000, 20_000, 3_000];
        let full = dropped(paced_interval(P240, false), &lats);
        let slack = dropped(paced_interval(P240, true), &lats);
        assert!(full > slack, "full {full} slack {slack}");
        assert_eq!(slack, 0);
    }

    #[test]
    fn lateness_buckets() {
        assert_eq!(late_bucket_100ns(0), 0);
        assert_eq!(late_bucket_100ns(999), 0); // 99.9 us
        assert_eq!(late_bucket_100ns(1_000), 1);
        assert_eq!(late_bucket_100ns(4_999), 2); // 499.9 us
        assert_eq!(late_bucket_100ns(5_000), 3); // 500 us
        assert_eq!(late_bucket_100ns(10_000), 4); // 1 ms
        assert_eq!(late_bucket_100ns(84_000), 7);
    }

    #[test]
    fn gap_buckets_are_in_periods() {
        // exactly one period, and the usual timer jitter around it: bucket 0
        assert_eq!(gap_bucket(P240, P240), 0);
        assert_eq!(gap_bucket(P240 - 5_000, P240), 0);
        assert_eq!(gap_bucket(P240 + 10_000, P240), 0);
        // two periods
        assert_eq!(gap_bucket(2 * P240, P240), 1);
        assert_eq!(gap_bucket(3 * P240, P240), 2);
        assert_eq!(gap_bucket(4 * P240, P240), 3);
        assert_eq!(gap_bucket(5 * P240, P240), 4);
        assert_eq!(gap_bucket(8 * P240, P240), 4);
        assert_eq!(gap_bucket(9 * P240, P240), 5);
        assert_eq!(gap_bucket(16 * P240, P240), 5);
        assert_eq!(gap_bucket(17 * P240, P240), 6);
        assert_eq!(gap_bucket(49 * P240, P240), 6);
        assert_eq!(gap_bucket(50 * P240, P240), 7);
        // the edges are exact at 1.5 periods
        assert_eq!(gap_bucket(P240 * 3 / 2 - 1, P240), 0);
        assert_eq!(gap_bucket(P240 * 3 / 2 + 1, P240), 1);
        // the same gap in time is a different bucket at another rate
        assert_eq!(gap_bucket(2 * P240, P60), 0);
        assert_eq!(gap_bucket(100, 0), 7);
    }

    #[test]
    fn gap_classification() {
        assert_eq!(classify_gap(0, 1_000, P240), Gap::First);
        assert_eq!(classify_gap(2_000, 1_000, P240), Gap::First);
        assert_eq!(
            classify_gap(1_000_000, 1_000_000 + P240, P240),
            Gap::Interval { bucket: 0, stall: false }
        );
        // 9 ms is a stall at 240 Hz (over 8 ms), 8 ms exactly is not
        assert_eq!(
            classify_gap(1_000_000, 1_000_000 + 90_000, P240),
            Gap::Interval { bucket: 1, stall: true }
        );
        assert_eq!(
            classify_gap(1_000_000, 1_000_000 + STALL_100NS, P240),
            Gap::Interval { bucket: 1, stall: false }
        );
        // idleness is not a stall
        assert_eq!(
            classify_gap(1_000_000, 1_000_000 + IDLE_BREAK_100NS + 1, P240),
            Gap::Idle
        );
        assert!(matches!(
            classify_gap(1_000_000, 1_000_000 + IDLE_BREAK_100NS, P240),
            Gap::Interval { stall: true, .. }
        ));
    }

    #[test]
    fn percentile_is_an_upper_bound_with_documented_approximation() {
        // empty
        assert_eq!(lat_percentile_us(&[0; 8], 990, 0), 0);
        // 100 flips, all in the 2..4.2 ms bucket (bucket 3): the estimate is the edge, clamped
        // to the observed maximum when that is lower.
        let mut c = [0u32; 8];
        c[3] = 100;
        assert_eq!(lat_percentile_us(&c, 990, 4_300), 4_200);
        assert_eq!(lat_percentile_us(&c, 990, 4_100), 4_100);
        assert_eq!(lat_percentile_us(&c, 500, 4_100), 4_100);
        // 98 fast, 2 slow: p99 (rank 99) is in the slow bucket, p50 in the fast one
        let mut c = [0u32; 8];
        c[1] = 98;
        c[4] = 2;
        assert_eq!(lat_percentile_us(&c, 500, 8_000), 1_000);
        assert_eq!(lat_percentile_us(&c, 990, 8_000), 8_000);
        assert_eq!(lat_percentile_us(&c, 980, 8_000), 1_000);
        // 99 fast and 1 slow: p99 is rank 99, still fast; p100 is the slow one
        let mut c = [0u32; 8];
        c[1] = 99;
        c[4] = 1;
        assert_eq!(lat_percentile_us(&c, 990, 8_000), 1_000);
        assert_eq!(lat_percentile_us(&c, 1000, 8_000), 8_000);
        // the open bucket answers the maximum
        let mut c = [0u32; 8];
        c[7] = 5;
        assert_eq!(lat_percentile_us(&c, 990, 123_456), 123_456);
        // never below 1: a maximum of 0 with samples (sub-microsecond) does not answer 0
        let mut c = [0u32; 8];
        c[0] = 10;
        assert_eq!(lat_percentile_us(&c, 990, 0), 1);
    }

    #[test]
    fn percentile_never_underestimates_a_sampled_population() {
        // A small deterministic population: latencies i * 37 us for i in 1..=1000.
        let mut c = [0u32; 8];
        let mut all = [0u32; 1000];
        let mut max = 0u32;
        for i in 1..=1000u32 {
            let us = i * 37;
            all[(i - 1) as usize] = us;
            max = max.max(us);
            c[lat_bucket(us as u64)] += 1;
        }
        all.sort();
        for pm in [500u32, 900, 990, 999] {
            let rank = ((1000u64 * pm as u64).div_ceil(1000)).max(1) as usize;
            let exact = all[rank - 1];
            let est = lat_percentile_us(&c, pm, max);
            assert!(est >= exact, "p{pm}: estimate {est} below exact {exact}");
            // and by less than the bucket width above it
            let b = lat_bucket(exact as u64);
            let upper = if b < 7 { LAT_EDGES_US[b] } else { max };
            assert!(est <= upper.max(exact));
        }
    }

    #[test]
    fn gap_percentile_scales_with_the_period() {
        let mut c = [0u32; 8];
        c[0] = 99;
        c[1] = 1;
        // p99 in bucket 0: 1.5 periods (6.25 ms at 240 Hz), clamped to the max when lower
        assert_eq!(gap_percentile_us(&c, P240, 990, 20_000), 6_250);
        assert_eq!(gap_percentile_us(&c, P240, 990, 4_300), 4_300);
        assert_eq!(gap_percentile_us(&c, P240, 1000, 9_000), 9_000);
        assert_eq!(gap_percentile_us(&[0; 8], P240, 990, 0), 0);
    }

    #[test]
    fn utilisation() {
        assert_eq!(used_permille(0, 0), 0);
        assert_eq!(used_permille(240, 240), 1000);
        assert_eq!(used_permille(120, 240), 500);
        assert_eq!(used_permille(300, 240), 1000);
    }

    fn ring_with(entries: &[(u64, u64)]) -> ([u64; RING], [u64; RING], u32, u32) {
        let mut a = [0u64; RING];
        let mut t = [0u64; RING];
        let mut live = 0u32;
        for (i, &(addr, time)) in entries.iter().enumerate() {
            a[i % RING] = addr;
            t[i % RING] = time;
            live |= 1 << (i % RING);
        }
        (a, t, live, entries.len() as u32)
    }

    #[test]
    fn retire_match_finds_the_newest_flip_with_the_address() {
        let (a, t, live, head) = ring_with(&[(0x100, 1_000), (0x200, 2_000)]);
        let h = retire_match(&a, &t, live, head, 0x200, 6_000).unwrap();
        assert_eq!(h.idx, 1);
        assert_eq!(h.latency_100ns, 4_000);
        assert_eq!(h.skipped, 1 << 0, "the older flip was passed over");
        let h = retire_match(&a, &t, live, head, 0x100, 6_000).unwrap();
        assert_eq!(h.idx, 0);
        assert_eq!(h.skipped, 0, "nothing older than the oldest");
        // an address nobody issued (the heartbeat re-reporting the displayed one)
        assert_eq!(retire_match(&a, &t, live, head, 0x300, 6_000), None);
        assert_eq!(retire_match(&a, &t, live, head, 0, 6_000), None);
        // a flip issued AFTER the tick started cannot be what that tick carried
        assert_eq!(retire_match(&a, &t, live, head, 0x200, 1_999), None);
        // a retired slot (live bit clear) does not match again
        assert_eq!(retire_match(&a, &t, live & !(1 << 1), head, 0x200, 6_000), None);
    }

    #[test]
    fn an_announce_between_the_tick_start_and_the_address_read_is_carried_by_that_tick() {
        // flip issued at 5_200, the tick started at 5_000 and read the address at 5_300
        let (a, t, live, head) = ring_with(&[(0x100, 1_000), (0x200, 5_200)]);
        // the old single-clock rule refuses it (issued after the tick's start): one tick late
        assert_eq!(retire_match(&a, &t, live, head, 0x200, 5_000), None);
        let h = retire_match_at(&a, &t, live, head, 0x200, 5_300, 5_000).unwrap();
        assert_eq!(h.idx, 1);
        // latency never underflows when the flip is newer than the tick's start
        assert_eq!(h.latency_100ns, 0);
        // a flip issued AFTER the address read cannot have been in it
        assert_eq!(retire_match_at(&a, &t, live, head, 0x200, 5_100, 5_000), None);
    }

    #[test]
    fn retire_match_prefers_the_newest_of_two_flips_to_one_address() {
        let (a, t, live, head) = ring_with(&[(0x100, 1_000), (0x200, 2_000), (0x100, 3_000)]);
        let h = retire_match(&a, &t, live, head, 0x100, 9_000).unwrap();
        assert_eq!(h.idx, 2);
        assert_eq!(h.latency_100ns, 6_000);
        assert_eq!(h.skipped, (1 << 0) | (1 << 1));
    }

    #[test]
    fn retire_match_wraps_the_ring() {
        let mut a = [0u64; RING];
        let mut t = [0u64; RING];
        let mut live = 0u32;
        for n in 0..(RING as u32 + 3) {
            let i = (n as usize) % RING;
            a[i] = 0x1000 + n as u64;
            t[i] = 100 * (n as u64 + 1);
            live |= 1 << i;
        }
        let head = RING as u32 + 3;
        // the newest is n = RING + 2, in slot 2
        let h = retire_match(&a, &t, live, head, 0x1000 + RING as u64 + 2, 1_000_000).unwrap();
        assert_eq!(h.idx, 2);
        // an overwritten (lost) flip's address is gone
        assert_eq!(retire_match(&a, &t, live, head, 0x1000, 1_000_000), None);
        // everything else is older than the hit
        assert_eq!(h.skipped.count_ones() as usize, RING - 1);
    }

    // ---- the chain arithmetic ----

    fn chain(publish: Publish, phase: u64, worker: u64) -> Chain {
        Chain {
            period_100ns: P240,
            phase_100ns: phase,
            worker_latency_100ns: worker,
            publish,
        }
    }

    #[test]
    fn default_chain_is_two_ticks_per_flip_and_120_per_second() {
        // THE CEILING of the default configuration (worker woken only by the tick): a flip
        // issued inside tick N (phase 0) is seen by the worker at tick N+1 and published after
        // that tick read the address, so it retires at N+2. Whatever the worker's latency
        // below one period.
        for worker in [0, 1_000, 10_000, 40_000] {
            let (k, lat) = retire_ticks(&chain(Publish::WorkerTickWake, 0, worker));
            assert_eq!(k, 2, "worker latency {worker}");
            assert_eq!(lat, 2 * P240);
        }
        let hz = saturated_rate_mhz(P240, 2, 1, false);
        assert!((119_900..=120_100).contains(&hz), "{hz}");
    }

    #[test]
    fn early_wake_is_one_tick_when_the_worker_beats_the_period() {
        let (k, lat) = retire_ticks(&chain(Publish::WorkerEarlyWake, 0, 5_000));
        assert_eq!((k, lat), (1, P240));
        let hz = saturated_rate_mhz(P240, 1, 1, false);
        assert!((239_900..=240_100).contains(&hz), "{hz}");
    }

    #[test]
    fn slow_worker_costs_whole_ticks() {
        // worker latency 5 ms (more than one 4.17 ms period): the tick wake is two periods
        // behind, the early wake one more than its latency's whole periods
        let (k, _) = retire_ticks(&chain(Publish::WorkerEarlyWake, 0, 50_000));
        assert_eq!(k, 2);
        assert_eq!(saturated_rate_mhz(P240, k, 1, false) / 1000, 119);
        let (k, _) = retire_ticks(&chain(Publish::WorkerTickWake, 0, 50_000));
        assert_eq!(k, 3);
        // the "80 flips per second" case: 3 ticks per flip at 240 Hz
        let hz = saturated_rate_mhz(P240, k, 1, false);
        assert!((79_900..=80_100).contains(&hz), "{hz}");
        // 9 ms of worker latency, early wake: still 3 ticks
        let (k, _) = retire_ticks(&chain(Publish::WorkerEarlyWake, 0, 90_000));
        assert_eq!(k, 3);
    }

    #[test]
    fn announce_is_one_tick_whatever_the_worker_does() {
        for worker in [0, 40_000, 400_000] {
            let (k, lat) = retire_ticks(&chain(Publish::Announced, 0, worker));
            assert_eq!((k, lat), (1, P240));
        }
    }

    #[test]
    fn asynchronous_arrival_phases() {
        // A flip issued mid-period (DWM presented with nothing queued):
        let phi = P240 / 2;
        // tick wake: retires at tick N+2: latency 2 periods minus the phase
        assert_eq!(
            retire_ticks(&chain(Publish::WorkerTickWake, phi, 1_000)),
            (2, 2 * P240 - phi)
        );
        // early wake and announce: the very next tick
        assert_eq!(
            retire_ticks(&chain(Publish::WorkerEarlyWake, phi, 1_000)),
            (1, P240 - phi)
        );
        assert_eq!(retire_ticks(&chain(Publish::Announced, phi, 0)), (1, P240 - phi));
        // an early-wake worker that finishes after the next tick has read the address
        // (phase 3/4 period + latency 1/2 period) costs a second tick
        let late = P240 * 3 / 4;
        let (k, _) = retire_ticks(&chain(Publish::WorkerEarlyWake, late, P240 / 2));
        assert_eq!(k, 2);
        // announce has no such race: published at the instant of the DDI
        assert_eq!(retire_ticks(&chain(Publish::Announced, late, P240 / 2)).0, 1);
    }

    #[test]
    fn queue_depth_helps_only_if_dxgkrnl_retires_coalesced_flips() {
        // two ticks per flip, depth 2, coalesced flips retire: one flip per tick
        let hz = saturated_rate_mhz(P240, 2, 2, true);
        assert!((239_900..=240_100).contains(&hz), "{hz}");
        // three ticks per flip needs depth 3
        assert!(saturated_rate_mhz(P240, 3, 2, true) < saturated_rate_mhz(P240, 3, 3, true));
        // never more than one per tick
        assert_eq!(saturated_rate_mhz(P240, 1, 8, true), saturated_rate_mhz(P240, 1, 1, false));
        // if they do NOT retire, depth does not raise the rate of the chain
        assert_eq!(
            saturated_rate_mhz(P240, 2, 4, false),
            saturated_rate_mhz(P240, 2, 1, false)
        );
        assert_eq!(saturated_rate_mhz(0, 2, 1, false), 0);
        assert_eq!(saturated_rate_mhz(P240, 0, 1, false), 0);
    }

    #[test]
    fn worst_case_latency_bounds_for_the_acceptance_numbers() {
        // 240 Hz: the target is a worst case under 8 ms and p99 under 5 ms.
        let late = 5_000; // 0.5 ms of heartbeat lateness
        let ann = latency_bound_100ns(P240, 0, Publish::Announced, late);
        assert_eq!(ann, P240 + late);
        assert!(ann < 50_000, "announce: one period plus the timer jitter is below 5 ms");
        assert!(ann < 80_000);
        // the default chain's worst case is two periods: 8.3 ms, over the bar
        let def = latency_bound_100ns(P240, 5_000, Publish::WorkerTickWake, late);
        assert_eq!(def, 2 * P240 + late);
        assert!(def > 80_000);
        // early wake: one period only while the worker stays inside it
        assert_eq!(
            latency_bound_100ns(P240, 5_000, Publish::WorkerEarlyWake, late),
            P240 + late
        );
        assert!(latency_bound_100ns(P240, 50_000, Publish::WorkerEarlyWake, late) > 80_000);
    }

    // ---- the announce decision ----

    fn facts() -> AnnounceFacts {
        AnnounceFacts {
            mode: AnnounceMode::Foreign,
            address: 0x1_0000,
            resource: Some(7),
            foreign_class: true,
            foreign_ok: true,
            idle: true,
            accepted: true,
            failing: false,
        }
    }

    #[test]
    fn the_default_announces_the_venus_class_only() {
        assert_eq!(AnnounceMode::from_knob(DEFAULT_KNOB), AnnounceMode::All);
        let venus = AnnounceFacts {
            mode: AnnounceMode::from_knob(DEFAULT_KNOB),
            foreign_class: false,
            foreign_ok: false,
            accepted: false,
            ..facts()
        };
        assert_eq!(announce_decide(&venus), Announce::Yes);
        let foreign = AnnounceFacts { foreign_class: true, accepted: true, ..venus };
        assert_eq!(announce_decide(&foreign), Announce::No(NoAnnounce::ForeignOff));
    }

    #[test]
    fn knob_values() {
        assert_eq!(AnnounceMode::from_knob(0), AnnounceMode::Off);
        assert_eq!(AnnounceMode::from_knob(1), AnnounceMode::Foreign);
        assert_eq!(AnnounceMode::from_knob(2), AnnounceMode::All);
        assert_eq!(AnnounceMode::from_knob(3), AnnounceMode::All);
        assert_eq!(AnnounceMode::from_knob(u32::MAX), AnnounceMode::All);
        assert!(!AnnounceMode::Off.announces());
        assert!(AnnounceMode::Foreign.announces());
        assert!(AnnounceMode::All.announces());
        assert_eq!(AnnounceMode::Off.code(), 0);
        assert_eq!(AnnounceMode::Foreign.code(), 1);
        assert_eq!(AnnounceMode::All.code(), 2);
        // early wake: any announce mode, or the knob alone
        assert!(!wakes_early(AnnounceMode::Off, 0));
        assert!(wakes_early(AnnounceMode::Off, 1));
        assert!(wakes_early(AnnounceMode::Foreign, 0));
        assert!(wakes_early(AnnounceMode::All, 0));
    }

    #[test]
    fn the_announce_table_row_by_row() {
        assert_eq!(announce_decide(&facts()), Announce::Yes);
        let f = AnnounceFacts { mode: AnnounceMode::Off, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Off));
        let f = AnnounceFacts { address: 0, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::NoAddress));
        for resource in [None, Some(0)] {
            let f = AnnounceFacts { resource, ..facts() };
            assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::NoResource));
        }
        let f = AnnounceFacts { idle: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Busy));
        // mode 2 announces the Venus class always and the foreign class only with FlipAnnForeign
        let f = AnnounceFacts {
            mode: AnnounceMode::All,
            foreign_class: true,
            foreign_ok: false,
            ..facts()
        };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::ForeignOff));
        let f = AnnounceFacts { foreign_class: false, foreign_ok: false, ..f };
        assert_eq!(announce_decide(&f), Announce::Yes);
        // mode 1 is the explicit foreign mode: the extra knob does not gate it
        let f = AnnounceFacts { foreign_ok: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::Yes);
        // mode 1 needs a foreign allocation the arm accepted; mode 2 does not
        let f = AnnounceFacts { accepted: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Unknown));
        let f = AnnounceFacts { mode: AnnounceMode::All, accepted: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::Yes);
        // a failing foreign arm declines for a known foreign allocation, in either mode
        let f = AnnounceFacts { failing: true, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Failing));
        let f = AnnounceFacts { mode: AnnounceMode::All, failing: true, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Failing));
        // but a Venus flip (not known to the foreign arm) is unaffected by that arm failing
        let f = AnnounceFacts {
            mode: AnnounceMode::All,
            accepted: false,
            failing: true,
            ..facts()
        };
        assert_eq!(announce_decide(&f), Announce::Yes);
        // first match wins: the knob beats everything, then the address, the resource, idleness
        let f = AnnounceFacts {
            mode: AnnounceMode::Off,
            address: 0,
            resource: None,
            foreign_class: true,
            foreign_ok: false,
            idle: false,
            accepted: false,
            failing: true,
        };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Off));
        let f = AnnounceFacts { address: 0, resource: None, idle: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::NoAddress));
        let f = AnnounceFacts { resource: None, idle: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::NoResource));
        let f = AnnounceFacts { idle: false, accepted: false, ..facts() };
        assert_eq!(announce_decide(&f), Announce::No(NoAnnounce::Busy));
    }

    #[test]
    fn at_most_one_unprogrammed_flip_is_ever_announced() {
        // The hazard bound: while the worker still holds an earlier flip (pending slot or
        // gate), nothing is announced, whatever the mode. Simulate a 240 Hz run where the
        // worker takes 0.3 of a period (fast) or 1.4 periods (slow) per flip, and flips are
        // issued one per period when the previous one retired.
        fn run(worker_periods_x10: u64, mode: AnnounceMode) -> (u32, u32) {
            let period = 100u64;
            let worker = period * worker_periods_x10 / 10;
            let mut busy_until = 0u64; // the worker finishes the previous flip's programming
            let mut announced = 0;
            let mut declined = 0;
            let mut t = 0u64;
            for _ in 0..1000 {
                let f = AnnounceFacts { mode, idle: busy_until <= t, ..facts() };
                match announce_decide(&f) {
                    Announce::Yes => announced += 1,
                    Announce::No(_) => declined += 1,
                }
                // the flip is programmed from now (the worker is the same either way)
                busy_until = t + worker;
                // the next flip is issued one period later at best (depth 1)
                t += period;
            }
            (announced, declined)
        }
        // a fast worker: every flip after the first is announced
        let (yes, no) = run(3, AnnounceMode::All);
        assert_eq!((yes, no), (1000, 0));
        // a slow worker (1.4 periods per flip): the next flip arrives while the worker is
        // still busy, is declined, retires the normal way; the following one finds it idle
        let (yes, no) = run(14, AnnounceMode::All);
        assert!(no > 0 && yes > 0, "{yes} {no}");
        assert_eq!(yes + no, 1000);
        // off: never
        assert_eq!(run(3, AnnounceMode::Off), (0, 1000));
    }

    #[test]
    fn the_worker_never_regresses_an_announced_address() {
        // nothing announced: store as always (the default, byte for byte)
        assert_eq!(worker_publish(0, 0x100), WorkerPublish::Store);
        // the worker reached the very flip that was announced: already published
        assert_eq!(worker_publish(0x100, 0x100), WorkerPublish::Confirm);
        // an older flip's programming finishing after a newer announce
        assert_eq!(worker_publish(0x200, 0x100), WorkerPublish::Late);
    }

    #[test]
    fn no_announce_codes_are_distinct_and_nonzero() {
        let all = [
            NoAnnounce::Off,
            NoAnnounce::NoAddress,
            NoAnnounce::NoResource,
            NoAnnounce::Busy,
            NoAnnounce::Unknown,
            NoAnnounce::Failing,
            NoAnnounce::ForeignOff,
        ];
        let mut codes: std::vec::Vec<u32> = all.iter().map(|w| w.code()).collect();
        assert!(codes.iter().all(|&c| c != 0));
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: std::vec::Vec<&str> = COUNTERS.iter().copied().collect();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(!n.is_empty());
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        // not a name of any other counter list of this crate
        for other in crate::foreign_flip::COUNTERS
            .iter()
            .chain(crate::flip_completion::COUNTERS.iter())
        {
            assert!(!COUNTERS.contains(other), "{other} collides");
        }
    }

    /// The sibling `kmd_render/src`, or `None` when this copy of the crate has none. With
    /// `HELIOS_REQUIRE_NAME_SCAN=1` an absent sibling FAILS the test instead of skipping it, so a
    /// pre-push run that forgot to copy `kmd_render` next to `kmd_logic` cannot pass silently.
    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    const INDEXED: [&str; 6] = ["FlipLat", "FlipPrgLat", "FlipHostLat", "IfGap", "VsLate", "FlipPh"];
    const KNOBS: [&str; 4] = ["FlipAnnounce", "FlipEarlyWake", "FlipLat", "FlipAnnForeign"];

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        // Needs `kmd_render` as a sibling of this crate (the pre-push scripts copy both).
        let Some(render) = render_src() else {
            return;
        };
        // The byte-string literals of the two I/O files.
        let mut literals: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        for file in ["ddi/flip_lat.rs", "ddi/flip_announce.rs"] {
            let text = std::fs::read_to_string(render.join(file)).unwrap();
            let mut rest = text.as_str();
            while let Some(i) = rest.find("b\"") {
                let tail = &rest[i + 2..];
                let end = tail.find('"').unwrap();
                let name = &tail[..end];
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric())
                    && !literals.iter().any(|w| w == name)
                {
                    literals.push(name.into());
                }
                rest = &tail[end + 1..];
            }
        }
        for n in COUNTERS {
            let is_indexed = INDEXED.iter().any(|p| {
                n.starts_with(p)
                    && n.len() == p.len() + 1
                    && n.ends_with(|c: char| c.is_ascii_digit())
            });
            if is_indexed {
                let prefix = &n[..n.len() - 1];
                assert!(
                    literals.iter().any(|l| l == prefix),
                    "the prefix of {n} is not spelled in the I/O files"
                );
            } else {
                assert!(
                    literals.iter().any(|l| l == n),
                    "{n} is listed but not written by the I/O files"
                );
            }
        }
        for l in &literals {
            let listed = COUNTERS.contains(&l.as_str());
            let prefix = INDEXED.contains(&l.as_str());
            let knob = KNOBS.contains(&l.as_str());
            assert!(
                listed || prefix || knob,
                "{l} is written by the I/O files but not listed"
            );
        }
    }

    #[test]
    fn no_other_file_writes_these_names() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.file_name().unwrap().to_string_lossy().into_owned();
                    // the two I/O halves, and `diag.rs` which spells the KNOB names
                    if name == "flip_lat.rs" || name == "flip_announce.rs" || name == "diag.rs" {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in COUNTERS.iter().chain(INDEXED.iter()).chain(KNOBS.iter()) {
                        let lit = std::format!("b\"{n}\"");
                        assert!(
                            !text.contains(&lit),
                            "{} spells the counter name {n}",
                            p.display()
                        );
                    }
                    for prefix in ["b\"Fa", "b\"IfGap", "b\"Vb", "b\"FlipLat", "b\"FlipPrg", "b\"FlipHost", "b\"FlipMax", "b\"FlipP5", "b\"FlipP9", "b\"FlipRet", "b\"FlipSkip", "b\"FlipLive", "b\"FlipInDpc", "b\"FlipPh", "b\"VsLate"] {
                        assert!(
                            !text.contains(prefix),
                            "{} spells a counter name starting {prefix}",
                            p.display()
                        );
                    }
                }
            }
        }
        assert!(checked > 20);
    }
}
