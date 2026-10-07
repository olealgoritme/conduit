//! Display submit staging (`SubmitPool`, `docs/zero-copy-present.md` 24.14): the stage counters
//! and the knob mirrors. The rules (stage clock, kick timer, pool-or-fresh plan, the counter
//! lists) are `helios_kmd_logic::submit_stage` (host-tested); the submits themselves are
//! `virtio/ctrl.rs` `stage_display_submit` / `display_enqueue` (the four display submitters)
//! and `raw_submit_async` (the pipelined foreign flip).
//!
//! Counters (at most 14 characters; atomics, written to the registry by [`publish_counters`]
//! from `publish_nvrm_counters`, the mirror thread's throttled cadence; PASSIVE writes only):
//!
//! * the aggregate of the four display submitters (`helios_kmd_logic::submit_stage::COUNTERS`):
//!   the per-stage totals in microseconds `SubReap` `SubTake` `SubAlloc` `SubCopy` `SubLock`
//!   `SubDrain` `SubEnq` `SubKick` `SubPrep`, the whole submit `SubTotal` / `SubTotMax`, the
//!   measured submits `SubN`, fresh allocations `SubAllocN`, pool takes `SubPoolHit`, suppressed
//!   notifies `SubNoKick`, pre-enqueue drains that found work `SubDrainHit`, the clock's own
//!   cost `SubClock`, and per path `SubNScan` `SubNPres` `SubNBlt` `SubNWin`;
//! * the per-path tables (`TABLE_NAMES`): `SubW*` for the windowed Blt, `SubF*` for the foreign
//!   flip (which is NOT in the aggregate), each with the stage totals, `Tot`, `TotMax` and `N`.
//!
//! `SubPoolOn` / `SubClkOn` are the knobs in force, written at every StartDevice. With
//! `SubStageClk` 0 no interrupt time is read on any submit path (the counts still run).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::submit_stage::{
    self as ss, Clock, Path, Stage, STAGES, TABLES, TABLE_N, TABLE_NAMES, TABLE_STAGES, TABLE_TOT,
    TABLE_TOT_MAX,
};

static STAGE_TICKS: [AtomicU64; STAGES] = [const { AtomicU64::new(0) }; STAGES];
static TOTAL_TICKS: AtomicU64 = AtomicU64::new(0);
static TOTAL_MAX_US: AtomicU32 = AtomicU32::new(0);
static N: AtomicU32 = AtomicU32::new(0);
static ALLOC_N: AtomicU32 = AtomicU32::new(0);
static POOL_HIT: AtomicU32 = AtomicU32::new(0);
static NO_KICK: AtomicU32 = AtomicU32::new(0);
static DRAIN_HIT: AtomicU32 = AtomicU32::new(0);
static CLOCK_TICKS: AtomicU64 = AtomicU64::new(0);
static PATH_N: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// One per-path table: stage totals (100 ns, [`TABLE_STAGES`] order), total, longest (us), count.
struct Table {
    stages: [AtomicU64; TABLE_STAGES.len()],
    total: AtomicU64,
    max_us: AtomicU32,
    n: AtomicU32,
}

static TABLE: [Table; TABLES] = [const {
    Table {
        stages: [const { AtomicU64::new(0) }; TABLE_STAGES.len()],
        total: AtomicU64::new(0),
        max_us: AtomicU32::new(0),
        n: AtomicU32::new(0),
    }
}; TABLES];

/// `SubStageClk` in force (default on). Off: no interrupt-time read anywhere on the submit paths.
static TIMING: AtomicBool = AtomicBool::new(true);

/// Interrupt time in 100 ns units (`KeQueryInterruptTimePrecise`); legal at any IRQL, no lock.
pub(crate) fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// Whether the stage clock reads time (`SubStageClk`). One relaxed load.
pub(crate) fn timing() -> bool {
    TIMING.load(Ordering::Relaxed)
}

/// Start one submit's clock. Two back-to-back stamps: the second starts the clock, their
/// difference is the clock's own cost (`SubClock`), so a hardware run can tell whether the
/// instrumentation itself is part of what it measures. With the timing off, a zero clock.
pub(crate) fn start() -> Clock {
    if !timing() {
        return Clock::start(0);
    }
    let a = now_100ns();
    let b = now_100ns();
    CLOCK_TICKS.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
    Clock::start(b)
}

/// Run one submit under the stage clock and fold the clock into the counters for `path`,
/// whatever the outcome (docs 24.14). Atomics only beyond what `f` does.
pub(crate) fn measured<R>(path: Path, f: impl FnOnce(&mut Clock) -> R) -> R {
    let mut clock = start();
    let result = f(&mut clock);
    finish(path, &clock);
    result
}

/// Charge the time since the previous stamp to `stage` (nothing with the timing off).
pub(crate) fn lap(clock: &mut Clock, stage: Stage) {
    if timing() {
        clock.lap(stage, now_100ns());
    }
}

/// Fresh allocations and pool takes of one display staging. Atomics only.
pub(crate) fn note_staging(fresh: u32, pooled: u32) {
    if fresh != 0 {
        ALLOC_N.fetch_add(fresh, Ordering::Relaxed);
    }
    if pooled != 0 {
        POOL_HIT.fetch_add(pooled, Ordering::Relaxed);
    }
}

/// The device suppressed the notify of a display enqueue. Atomics only (any IRQL).
pub(crate) fn note_no_kick(path: Path) {
    if path.aggregate() {
        NO_KICK.fetch_add(1, Ordering::Relaxed);
    }
}

/// The pre-enqueue drain of a display submit found completions to consume. Atomics only.
pub(crate) fn note_drain_hit(path: Path) {
    if path.aggregate() {
        DRAIN_HIT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Fold one finished submit (accepted or not) into the aggregate (display submitters) and into
/// its path's table (windowed Blt, flip). Atomics only.
pub(crate) fn finish(path: Path, clock: &Clock) {
    let total = clock.total();
    if path.aggregate() {
        for (cell, ticks) in STAGE_TICKS.iter().zip(clock.stages().iter()) {
            if *ticks != 0 {
                cell.fetch_add(*ticks, Ordering::Relaxed);
            }
        }
        TOTAL_TICKS.fetch_add(total, Ordering::Relaxed);
        TOTAL_MAX_US.fetch_max(ss::ticks_to_us(total), Ordering::Relaxed);
        N.fetch_add(1, Ordering::Relaxed);
        PATH_N[path.index()].fetch_add(1, Ordering::Relaxed);
    }
    if let Some(row) = path.table() {
        let t = &TABLE[row];
        for (cell, stage) in t.stages.iter().zip(TABLE_STAGES.iter()) {
            let ticks = clock.stages()[stage.index()];
            if ticks != 0 {
                cell.fetch_add(ticks, Ordering::Relaxed);
            }
        }
        t.total.fetch_add(total, Ordering::Relaxed);
        t.max_us.fetch_max(ss::ticks_to_us(total), Ordering::Relaxed);
        t.n.fetch_add(1, Ordering::Relaxed);
    }
}

/// The knobs in force, written at every StartDevice (`AdapterKnobs::read_at_start`), and the
/// timing switch armed for this generation. PASSIVE.
pub(crate) fn record_knobs(pool: bool, timing: bool) {
    TIMING.store(timing, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"SubPoolOn", pool as u32);
    crate::diag::record_named_bytes(b"SubClkOn", timing as u32);
}

/// A new transport generation: the statics are zeroed and the zeroed block is written once, so
/// a value an earlier generation left in the service key is never read as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for cell in STAGE_TICKS.iter().chain([&TOTAL_TICKS, &CLOCK_TICKS]) {
        cell.store(0, Ordering::Relaxed);
    }
    for cell in PATH_N
        .iter()
        .chain([&TOTAL_MAX_US, &N, &ALLOC_N, &POOL_HIT, &NO_KICK, &DRAIN_HIT])
    {
        cell.store(0, Ordering::Relaxed);
    }
    for t in TABLE.iter() {
        for cell in t.stages.iter().chain([&t.total]) {
            cell.store(0, Ordering::Relaxed);
        }
        t.max_us.store(0, Ordering::Relaxed);
        t.n.store(0, Ordering::Relaxed);
    }
    write_block();
}

/// Mirror the counters to the service key once a submit was measured. PASSIVE only.
pub(crate) fn publish_counters() {
    let any = N.load(Ordering::Relaxed) | TABLE.iter().fold(0, |a, t| a | t.n.load(Ordering::Relaxed));
    if any == 0 {
        return;
    }
    write_block();
}

fn write_block() {
    use crate::diag::record_named_bytes as rec;
    let us = |cell: &AtomicU64| ss::ticks_to_us(cell.load(Ordering::Relaxed));
    let stage = |s: Stage| us(&STAGE_TICKS[s.index()]);
    rec(b"SubReap", stage(Stage::Reap));
    rec(b"SubTake", stage(Stage::Take));
    rec(b"SubAlloc", stage(Stage::Alloc));
    rec(b"SubCopy", stage(Stage::Copy));
    rec(b"SubLock", stage(Stage::Lock));
    rec(b"SubDrain", stage(Stage::Drain));
    rec(b"SubEnq", stage(Stage::Enq));
    rec(b"SubKick", stage(Stage::Kick));
    rec(b"SubPrep", stage(Stage::Prep));
    rec(b"SubTotal", us(&TOTAL_TICKS));
    rec(b"SubTotMax", TOTAL_MAX_US.load(Ordering::Relaxed));
    rec(b"SubN", N.load(Ordering::Relaxed));
    rec(b"SubAllocN", ALLOC_N.load(Ordering::Relaxed));
    rec(b"SubPoolHit", POOL_HIT.load(Ordering::Relaxed));
    rec(b"SubNoKick", NO_KICK.load(Ordering::Relaxed));
    rec(b"SubDrainHit", DRAIN_HIT.load(Ordering::Relaxed));
    rec(b"SubClock", us(&CLOCK_TICKS));
    let path = |p: Path| PATH_N[p.index()].load(Ordering::Relaxed);
    rec(b"SubNScan", path(Path::Scanout));
    rec(b"SubNPres", path(Path::Present));
    rec(b"SubNBlt", path(Path::Blt));
    rec(b"SubNWin", path(Path::Windowed));
    // The per-path tables, from `TABLE_NAMES` (host-tested names, no literals here).
    for (t, names) in TABLE.iter().zip(TABLE_NAMES.iter()) {
        for (cell, name) in t.stages.iter().zip(names.iter()) {
            rec(name.as_bytes(), us(cell));
        }
        rec(names[TABLE_TOT].as_bytes(), us(&t.total));
        rec(names[TABLE_TOT_MAX].as_bytes(), t.max_us.load(Ordering::Relaxed));
        rec(names[TABLE_N].as_bytes(), t.n.load(Ordering::Relaxed));
    }
}
