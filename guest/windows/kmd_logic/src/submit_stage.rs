//! Display submit staging (`SubmitPool`, `docs/zero-copy-present.md` 24.14): the pure half. The
//! I/O half is `kmd_render/src/virtio/submit_stage.rs` (atomics, knob mirror, registry
//! publication) and `kmd_render/src/virtio/ctrl.rs` (`stage_display_submit` and the four
//! display submitters that call it).
//!
//! What is here:
//!
//! * [`Clock`]: one display submit's stage clock. The caller stamps interrupt time (100 ns) at
//!   each stage boundary with [`Clock::lap`]; the notify is timed inside the enqueue and carved
//!   out of it with [`Clock::carve`]. Pure arithmetic, saturating, so a clock that goes
//!   backwards (it does not, but a test can) reads 0 rather than wrapping into hours.
//! * [`KickTimer`]: the arm / record / take protocol between a display submitter and the
//!   transport's `publish_then_notify`, which runs inside the enqueue under the same lock hold.
//! * [`Source`] / [`plan`]: where a staged buffer comes from (the transport's bounded DMA pool or
//!   a fresh contiguous allocation), the whole decision the knob makes.
//! * [`COUNTERS`] and [`KNOB`]: the names, checked against the I/O file by the tests below.
//!
//! The buffer lifetime rule is NOT re-implemented here: the pool is the transport's existing
//! `dma_pool` (`virtio/gpu/mod.rs`), which only ever receives a buffer after the host consumed
//! it (the used-ring completion popped by `drain_used`, then the PASSIVE reap), and which dies
//! with the transport generation at StopDevice. 24.14 says why a second pool would be worse.

/// The stages of one display submit, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The PASSIVE reap of completed entries (`reap_parked`): the frees or the recycling of
    /// the buffers of earlier submits.
    Reap,
    /// The lock hold that takes the two staged buffers from the pool (pool path only).
    Take,
    /// Fresh contiguous allocations (`DmaBuffer::new`), both buffers together.
    Alloc,
    /// The copy of the Venus stream into its staged buffer.
    Copy,
    /// From the call into `with_virtio` to the first instruction under the lock: the wait.
    Lock,
    /// `drain_used` before the enqueue.
    Drain,
    /// The enqueue (descriptor build, avail-ring publish, in-flight record) without the notify.
    Enq,
    /// The doorbell (`transport.notify`), when the device asked for one.
    Kick,
    /// Work of the submit before its staging: the foreign flip's mint, release-book entry and
    /// request build (`foreign_scanout::present_submit`). 0 on the four display submitters.
    Prep,
}

/// Number of [`Stage`]s.
pub const STAGES: usize = 9;

impl Stage {
    /// Every stage, in order. Index `i` is `ALL[i].index()`.
    pub const ALL: [Stage; STAGES] = [
        Stage::Reap,
        Stage::Take,
        Stage::Alloc,
        Stage::Copy,
        Stage::Lock,
        Stage::Drain,
        Stage::Enq,
        Stage::Kick,
        Stage::Prep,
    ];

    /// Slot of this stage in [`Clock::stages`].
    pub const fn index(self) -> usize {
        match self {
            Stage::Reap => 0,
            Stage::Take => 1,
            Stage::Alloc => 2,
            Stage::Copy => 3,
            Stage::Lock => 4,
            Stage::Drain => 5,
            Stage::Enq => 6,
            Stage::Kick => 7,
            Stage::Prep => 8,
        }
    }

    /// The counter that carries this stage's total microseconds.
    pub const fn counter(self) -> &'static str {
        COUNTERS[self.index()]
    }
}

/// One submit's stage clock, in 100 ns ticks. Lives on the submitter's stack (88 bytes).
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    start: u64,
    last: u64,
    acc: [u64; STAGES],
}

impl Clock {
    /// Start at `now` (100 ns).
    pub const fn start(now: u64) -> Self {
        Self {
            start: now,
            last: now,
            acc: [0; STAGES],
        }
    }

    /// Charge everything since the previous stamp to `stage`, and stamp `now`.
    pub fn lap(&mut self, stage: Stage, now: u64) {
        let d = now.saturating_sub(self.last);
        let slot = &mut self.acc[stage.index()];
        *slot = slot.saturating_add(d);
        if now > self.last {
            self.last = now;
        }
    }

    /// Move up to `ticks` from `from` to `to` (the notify is timed inside the enqueue, whose
    /// lap already holds it). Never more than `from` holds, so the total is unchanged.
    pub fn carve(&mut self, from: Stage, to: Stage, ticks: u64) {
        let have = self.acc[from.index()];
        let moved = ticks.min(have);
        self.acc[from.index()] = have - moved;
        let slot = &mut self.acc[to.index()];
        *slot = slot.saturating_add(moved);
    }

    /// Ticks charged to each stage, indexed by [`Stage::index`].
    pub const fn stages(&self) -> &[u64; STAGES] {
        &self.acc
    }

    /// Ticks from the start to the last stamp (the sum of the stages when every interval was
    /// lapped, which is how the I/O half uses it).
    pub const fn total(&self) -> u64 {
        self.last.saturating_sub(self.start)
    }
}

/// 100 ns ticks to whole microseconds (the counters' unit), saturating into `u32`.
pub const fn ticks_to_us(ticks: u64) -> u32 {
    let us = ticks / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// The notify timing handed from `publish_then_notify` to the display submitter that armed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kick {
    /// 100 ns spent in `transport.notify` (0 when the device suppressed the notify). `u32`
    /// (about 7 minutes) keeps the transport field small; [`kick_ticks`] saturates into it.
    pub ticks: u32,
    /// Whether the doorbell was rung.
    pub notified: bool,
}

/// 100 ns ticks into a [`Kick::ticks`], saturating.
pub const fn kick_ticks(ticks: u64) -> u32 {
    if ticks > u32::MAX as u64 {
        u32::MAX
    } else {
        ticks as u32
    }
}

/// Arm / record / take. A display submitter arms it inside its `with_virtio` hold, the enqueue's
/// `publish_then_notify` records into it only when armed (one test on every other path), and the
/// submitter takes it in the same hold. A take always disarms, so an enqueue that failed before
/// the publish (nothing recorded) cannot leave it armed for an unrelated later enqueue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KickTimer {
    armed: bool,
    got: Option<Kick>,
}

impl KickTimer {
    /// Disarmed and empty.
    pub const fn new() -> Self {
        Self {
            armed: false,
            got: None,
        }
    }

    /// Arm for the next publish; forgets anything an earlier armed publish left.
    pub fn arm(&mut self) {
        self.armed = true;
        self.got = None;
    }

    /// Whether a publish should time its notify.
    pub const fn armed(&self) -> bool {
        self.armed
    }

    /// Record one publish's notify. Ignored unless armed; the first record wins (one enqueue
    /// publishes one entry).
    pub fn record(&mut self, kick: Kick) {
        if self.armed && self.got.is_none() {
            self.got = Some(kick);
        }
    }

    /// What was recorded, and disarm.
    pub fn take(&mut self) -> Option<Kick> {
        self.armed = false;
        self.got.take()
    }
}

/// Where one staged buffer comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Taken from the transport's DMA pool (a buffer a completed submit gave back).
    Pool,
    /// A fresh contiguous allocation at PASSIVE.
    Fresh,
}

/// The source of the meta and the stream buffer: the pool when the knob is on and the pool had
/// one, a fresh allocation otherwise (knob off = the old allocate-per-submit path, exactly).
/// A miss never waits or retries: a full or empty pool is not an error.
pub const fn plan(pool_on: bool, meta_hit: bool, stream_hit: bool) -> (Source, Source) {
    (pick(pool_on, meta_hit), pick(pool_on, stream_hit))
}

const fn pick(pool_on: bool, hit: bool) -> Source {
    if pool_on && hit {
        Source::Pool
    } else {
        Source::Fresh
    }
}

/// Fresh allocations a [`plan`] costs (0, 1 or 2): the `SubAllocN` increment.
pub const fn fresh_count(p: (Source, Source)) -> u32 {
    let a = matches!(p.0, Source::Fresh) as u32;
    let b = matches!(p.1, Source::Fresh) as u32;
    a + b
}

/// The knob (REG_DWORD in the service key, default 1 = take the staged buffers from the pool;
/// 0 = allocate both per submit, the behaviour before 24.14). Read at every StartDevice.
pub const KNOB: &str = "SubmitPool";

/// The timing knob (REG_DWORD, default 1 = stamp every stage; 0 = counts only, no interrupt-time
/// read anywhere on the submit paths): the A/B that tells the clock's own cost apart from what it
/// measures, in particular inside the `virtio_lock` hold. Read at every StartDevice.
pub const KNOB_TIMING: &str = "SubStageClk";

/// Which submitter ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// `submit_venus_async_scanout` (the scan-out copy of a flip).
    Scanout,
    /// `submit_venus_async_present` (the synchronous-arm Present Blt).
    Present,
    /// `submit_venus_async_blt` (`BltAsync` direct).
    Blt,
    /// `submit_venus_async_windowed_blt` (deferred windowed Blt).
    Windowed,
    /// `foreign_scanout::present_submit` -> `ctrl::raw_submit_async`: the pipelined foreign
    /// flip (`ScanoutFlip`). NOT in the aggregate `Sub*` totals (they stay the four display
    /// submitters', comparable with earlier runs); its own table is `SubF*`.
    Flip,
}

impl Path {
    /// The four display submitters, in `SubNScan` .. `SubNWin` order (the aggregate).
    pub const ALL: [Path; 4] = [Path::Scanout, Path::Present, Path::Blt, Path::Windowed];

    /// Index: the aggregate per-path counter for the four display submitters, 4 for the flip.
    pub const fn index(self) -> usize {
        match self {
            Path::Scanout => 0,
            Path::Present => 1,
            Path::Blt => 2,
            Path::Windowed => 3,
            Path::Flip => 4,
        }
    }

    /// Whether this path's stages go into the aggregate `Sub*` totals.
    pub const fn aggregate(self) -> bool {
        !matches!(self, Path::Flip)
    }

    /// This path's own table in [`TABLE_NAMES`], if it has one.
    pub const fn table(self) -> Option<usize> {
        match self {
            Path::Windowed => Some(0),
            Path::Flip => Some(1),
            _ => None,
        }
    }

    /// The per-path call counter.
    pub const fn counter(self) -> &'static str {
        match self {
            Path::Flip => TABLE_NAMES[1][TABLE_N],
            _ => COUNTERS[PATH_COUNTER_BASE + self.index()],
        }
    }
}

/// Index of `SubNScan` in [`COUNTERS`].
pub const PATH_COUNTER_BASE: usize = 17;

/// Every aggregate counter the I/O half writes, at most 14 characters, all `Sub*`. The first
/// [`STAGES`] are the per-stage totals in microseconds, in [`Stage`] order; then the totals, the
/// counts, and (from [`PATH_COUNTER_BASE`]) the per-path call counts in [`Path::ALL`] order.
/// The four display submitters only (see [`Path::Flip`]).
pub const COUNTERS: [&str; 21] = [
    // Stage totals, microseconds since the start of the transport generation.
    "SubReap",
    "SubTake",
    "SubAlloc",
    "SubCopy",
    "SubLock",
    "SubDrain",
    "SubEnq",
    "SubKick",
    "SubPrep",
    // Whole submit (first stamp to last), total and longest, microseconds.
    "SubTotal",
    "SubTotMax",
    // Measured submits: every call of a display submitter, accepted or not.
    "SubN",
    // Fresh `DmaBuffer::new` calls / staged buffers taken from the pool.
    "SubAllocN",
    "SubPoolHit",
    // Enqueues the device asked not to be notified of / pre-enqueue drains that found work.
    "SubNoKick",
    "SubDrainHit",
    // The stage clock's own cost: two back-to-back stamps per submit, microseconds in all.
    "SubClock",
    // Per-path calls.
    "SubNScan",
    "SubNPres",
    "SubNBlt",
    "SubNWin",
];

/// The stages a per-path table carries, in table-slot order (`Copy` is left out: it is the same
/// sub-microsecond memcpy on every path, and the aggregate `SubCopy` has it).
pub const TABLE_STAGES: [Stage; 8] = [
    Stage::Prep,
    Stage::Reap,
    Stage::Take,
    Stage::Alloc,
    Stage::Lock,
    Stage::Drain,
    Stage::Enq,
    Stage::Kick,
];

/// Slot of the whole-submit total (us) in a table row.
pub const TABLE_TOT: usize = 8;
/// Slot of the longest submit (us).
pub const TABLE_TOT_MAX: usize = 9;
/// Slot of the call count.
pub const TABLE_N: usize = 10;
/// Names per table row.
pub const TABLE_WIDTH: usize = 11;
/// Number of per-path tables.
pub const TABLES: usize = 2;

/// The per-path tables: row 0 the windowed Blt (`SubW*`), row 1 the foreign flip (`SubF*`).
/// Slots `0..8` are [`TABLE_STAGES`] totals in microseconds, then [`TABLE_TOT`],
/// [`TABLE_TOT_MAX`], [`TABLE_N`]. The I/O half writes them from this table (no literals).
pub const TABLE_NAMES: [[&str; TABLE_WIDTH]; TABLES] = [
    [
        "SubWPrep",
        "SubWReap",
        "SubWTake",
        "SubWAlloc",
        "SubWLock",
        "SubWDrain",
        "SubWEnq",
        "SubWKick",
        "SubWTot",
        "SubWTotMax",
        "SubWN",
    ],
    [
        "SubFPrep",
        "SubFReap",
        "SubFTake",
        "SubFAlloc",
        "SubFLock",
        "SubFDrain",
        "SubFEnq",
        "SubFKick",
        "SubFTot",
        "SubFTotMax",
        "SubFN",
    ],
];

/// The StartDevice mirrors of the knobs in force (written with the knob read, not with the
/// throttled counters, so they are not in [`COUNTERS`]): `SubmitPool`, `SubStageClk`.
pub const KNOB_MIRRORS: [&str; 2] = ["SubPoolOn", "SubClkOn"];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn laps_charge_the_interval_to_the_named_stage() {
        let mut c = Clock::start(1_000);
        c.lap(Stage::Reap, 1_050);
        c.lap(Stage::Take, 1_060);
        c.lap(Stage::Copy, 1_061);
        c.lap(Stage::Lock, 1_100);
        c.lap(Stage::Drain, 1_102);
        c.lap(Stage::Enq, 1_300);
        let s = c.stages();
        assert_eq!(s[Stage::Reap.index()], 50);
        assert_eq!(s[Stage::Take.index()], 10);
        assert_eq!(s[Stage::Alloc.index()], 0);
        assert_eq!(s[Stage::Copy.index()], 1);
        assert_eq!(s[Stage::Lock.index()], 39);
        assert_eq!(s[Stage::Drain.index()], 2);
        assert_eq!(s[Stage::Enq.index()], 198);
        assert_eq!(c.total(), 300);
        assert_eq!(s.iter().sum::<u64>(), c.total());
    }

    #[test]
    fn a_stage_lapped_twice_accumulates() {
        let mut c = Clock::start(0);
        c.lap(Stage::Alloc, 100);
        c.lap(Stage::Copy, 110);
        c.lap(Stage::Alloc, 300);
        assert_eq!(c.stages()[Stage::Alloc.index()], 290);
        assert_eq!(c.total(), 300);
    }

    #[test]
    fn a_clock_that_goes_backwards_reads_zero_not_hours() {
        let mut c = Clock::start(1_000);
        c.lap(Stage::Reap, 900);
        assert_eq!(c.stages()[Stage::Reap.index()], 0);
        c.lap(Stage::Copy, 1_010);
        assert_eq!(c.stages()[Stage::Copy.index()], 10);
        assert_eq!(c.total(), 10);
    }

    #[test]
    fn carving_the_kick_out_of_the_enqueue_keeps_the_total() {
        let mut c = Clock::start(0);
        c.lap(Stage::Enq, 500);
        c.carve(Stage::Enq, Stage::Kick, 420);
        assert_eq!(c.stages()[Stage::Enq.index()], 80);
        assert_eq!(c.stages()[Stage::Kick.index()], 420);
        // A kick longer than the enqueue lap (clock granularity) moves only what is there.
        let mut d = Clock::start(0);
        d.lap(Stage::Enq, 5);
        d.carve(Stage::Enq, Stage::Kick, 9);
        assert_eq!(d.stages()[Stage::Enq.index()], 0);
        assert_eq!(d.stages()[Stage::Kick.index()], 5);
        assert_eq!(d.stages().iter().sum::<u64>(), d.total());
    }

    #[test]
    fn ticks_become_microseconds_and_saturate() {
        assert_eq!(ticks_to_us(0), 0);
        assert_eq!(ticks_to_us(9), 0);
        assert_eq!(ticks_to_us(1_145), 114);
        assert_eq!(ticks_to_us(u64::MAX), u32::MAX);
    }

    #[test]
    fn kick_ticks_saturate() {
        assert_eq!(kick_ticks(12), 12);
        assert_eq!(kick_ticks(u64::MAX), u32::MAX);
    }

    #[test]
    fn the_kick_timer_records_only_when_armed_and_a_take_disarms() {
        let mut t = KickTimer::new();
        t.record(Kick {
            ticks: 5,
            notified: true,
        });
        assert_eq!(t.take(), None, "not armed: nothing recorded");
        t.arm();
        assert!(t.armed());
        t.record(Kick {
            ticks: 7,
            notified: true,
        });
        t.record(Kick {
            ticks: 99,
            notified: false,
        });
        assert_eq!(
            t.take(),
            Some(Kick {
                ticks: 7,
                notified: true
            }),
            "the first record wins"
        );
        assert!(!t.armed());
        // An armed enqueue that failed before its publish leaves nothing, and the take disarms
        // so a later unrelated publish is not charged to anyone.
        t.arm();
        assert_eq!(t.take(), None);
        t.record(Kick {
            ticks: 3,
            notified: true,
        });
        assert_eq!(t.take(), None);
        // A re-arm forgets a record nobody took.
        t.arm();
        t.record(Kick {
            ticks: 1,
            notified: false,
        });
        t.arm();
        assert_eq!(t.take(), None);
    }

    #[test]
    fn the_plan_is_the_pool_only_with_the_knob_on_and_a_hit() {
        use Source::*;
        assert_eq!(plan(true, true, true), (Pool, Pool));
        assert_eq!(plan(true, true, false), (Pool, Fresh));
        assert_eq!(plan(true, false, true), (Fresh, Pool));
        assert_eq!(plan(true, false, false), (Fresh, Fresh));
        for m in [false, true] {
            for s in [false, true] {
                assert_eq!(plan(false, m, s), (Fresh, Fresh), "knob 0 is the old path");
            }
        }
        assert_eq!(fresh_count(plan(true, true, true)), 0);
        assert_eq!(fresh_count(plan(true, false, true)), 1);
        assert_eq!(fresh_count(plan(false, true, true)), 2);
    }

    #[test]
    fn stage_and_path_indices_are_dense_and_match_the_counter_list() {
        for (i, s) in Stage::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
        assert_eq!(Stage::Reap.counter(), "SubReap");
        assert_eq!(Stage::Kick.counter(), "SubKick");
        for (i, p) in Path::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
        assert_eq!(Path::Scanout.counter(), "SubNScan");
        assert_eq!(Path::Windowed.counter(), "SubNWin");
        assert_eq!(Path::Flip.counter(), "SubFN");
        assert_eq!(PATH_COUNTER_BASE + Path::ALL.len(), COUNTERS.len());
        assert_eq!(Stage::Prep.counter(), "SubPrep");
        assert_eq!(COUNTERS[STAGES], "SubTotal");
        // The flip is never in the aggregate; the windowed Blt is in both.
        assert!(!Path::Flip.aggregate());
        assert!(Path::ALL.iter().all(|p| p.aggregate()));
        assert_eq!(Path::Windowed.table(), Some(0));
        assert_eq!(Path::Flip.table(), Some(1));
        assert_eq!(Path::Blt.table(), None);
    }

    #[test]
    fn table_rows_follow_the_table_stages_and_name_their_path() {
        for (row, prefix) in [(0usize, "SubW"), (1, "SubF")] {
            for (i, stage) in TABLE_STAGES.iter().enumerate() {
                // `SubWAlloc` is the windowed row's `SubAlloc`: the aggregate name with the
                // path letter after `Sub`.
                let aggregate = stage.counter();
                let want = std::format!("{prefix}{}", &aggregate[3..]);
                assert_eq!(TABLE_NAMES[row][i], want.as_str());
            }
            assert_eq!(TABLE_NAMES[row][TABLE_TOT], std::format!("{prefix}Tot").as_str());
            assert_eq!(TABLE_NAMES[row][TABLE_TOT_MAX], std::format!("{prefix}TotMax").as_str());
            assert_eq!(TABLE_NAMES[row][TABLE_N], std::format!("{prefix}N").as_str());
        }
        assert_eq!(TABLE_STAGES.len(), TABLE_TOT);
        assert!(!TABLE_STAGES.contains(&Stage::Copy));
    }

    #[test]
    fn counter_names_fit_and_are_unique_and_differ_from_the_knobs() {
        let mut names: Vec<&str> = all_names();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Sub"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate counter name");
        // The knobs share the service key: a counter of the same name would overwrite one.
        for knob in [KNOB, KNOB_TIMING, "SubSpaceWake"] {
            assert!(knob.len() <= 14);
            assert!(!names.contains(&knob), "{knob} is also a counter");
        }
        let lists: [&[&str]; 3] = [
            crate::guest_blob::COUNTERS,
            &crate::foreign_flip::COUNTERS[..],
            &crate::stall_diag::COUNTERS[..],
        ];
        for list in lists {
            for other in list {
                assert!(!names.contains(other), "{other} collides");
            }
        }
    }

    /// Every name this module owns: aggregate, tables, knob mirrors.
    fn all_names() -> Vec<&'static str> {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for row in TABLE_NAMES.iter() {
            names.extend_from_slice(row);
        }
        names.extend_from_slice(&KNOB_MIRRORS);
        names
    }

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

    /// Every `b"..."` literal of alphanumerics in `text`.
    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    /// The I/O file writes exactly the listed names (plus the knob mirror), every one of them,
    /// and no other `Sub*` name.
    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("virtio/submit_stage.rs")).unwrap();
        let written = literals(&text);
        for n in COUNTERS.iter().chain(KNOB_MIRRORS.iter()) {
            assert!(
                written.iter().any(|l| l == n),
                "{n} is listed but not written by virtio/submit_stage.rs"
            );
        }
        for l in written.iter().filter(|l| l.starts_with("Sub")) {
            assert!(
                COUNTERS.contains(&l.as_str()) || KNOB_MIRRORS.contains(&l.as_str()),
                "{l} is written by virtio/submit_stage.rs but not listed"
            );
        }
        // The per-path tables are written from `TABLE_NAMES` (no literals): the file must use it.
        assert!(text.contains("TABLE_NAMES"), "the per-path tables are not written");
    }

    /// No other `kmd_render` file spells one of these names (or a longer literal that the
    /// 14-byte registry name clamps onto one), and the knob is declared in `diag.rs`.
    #[test]
    fn no_other_file_writes_these_names_and_the_knob_is_in_diag() {
        let Some(render) = render_src() else {
            return;
        };
        let mine = all_names();
        let mut stack = std::vec![render.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let s = p.to_string_lossy().into_owned();
                    if s.ends_with("virtio/submit_stage.rs") {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for lit in literals(&text) {
                        for m in &mine {
                            assert!(lit != *m, "{s} spells {m}");
                            if lit.len() > 14 {
                                assert!(&lit[..14] != *m, "{lit} in {s} clamps onto {m}");
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 20);
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(diag.contains("KnobName::new(b\"SubmitPool\")"));
        assert!(diag.contains("KnobName::new(b\"SubStageClk\")"));
    }
}
