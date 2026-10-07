//! Display submit staging (`SubmitPool`, `docs/zero-copy-present.md` 24.13): the stage counters
//! and the knob mirror. The rules (stage clock, kick timer, pool-or-fresh plan, the counter list)
//! are `helios_kmd_logic::submit_stage` (host-tested); the submit itself is
//! `virtio/ctrl.rs` `stage_display_submit` / `display_enqueue`.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::submit_stage::COUNTERS`;
//! atomics, written to the registry by [`publish_counters`] from `publish_nvrm_counters`, the
//! mirror thread's throttled cadence; PASSIVE writes only): the per-stage totals in microseconds
//! `SubReap` `SubTake` `SubAlloc` `SubCopy` `SubLock` `SubDrain` `SubEnq` `SubKick`, the whole
//! submit `SubTotal` / `SubTotMax`, the measured submits `SubN`, fresh allocations `SubAllocN`,
//! pool takes `SubPoolHit`, suppressed notifies `SubNoKick`, pre-enqueue drains that found work
//! `SubDrainHit`, the clock's own cost `SubClock`, and per path `SubNScan` `SubNPres` `SubNBlt`
//! `SubNWin`. `SubPoolOn` is the knob in force, written at every StartDevice.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::submit_stage::{self as ss, Clock, Path, Stage, STAGES};

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

/// Interrupt time in 100 ns units (`KeQueryInterruptTimePrecise`); legal at any IRQL, no lock.
pub(crate) fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// Start one submit's clock. Two back-to-back stamps: the second starts the clock, their
/// difference is the clock's own cost (`SubClock`), so a hardware run can tell whether the
/// instrumentation itself is part of what it measures.
pub(crate) fn start() -> Clock {
    let a = now_100ns();
    let b = now_100ns();
    CLOCK_TICKS.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
    Clock::start(b)
}

/// Charge the time since the previous stamp to `stage`.
pub(crate) fn lap(clock: &mut Clock, stage: Stage) {
    clock.lap(stage, now_100ns());
}

/// Fresh allocations and pool takes of one staging. Atomics only.
pub(crate) fn note_staging(fresh: u32, pooled: u32) {
    if fresh != 0 {
        ALLOC_N.fetch_add(fresh, Ordering::Relaxed);
    }
    if pooled != 0 {
        POOL_HIT.fetch_add(pooled, Ordering::Relaxed);
    }
}

/// The device suppressed the notify of a display enqueue. Atomics only (any IRQL).
pub(crate) fn note_no_kick() {
    NO_KICK.fetch_add(1, Ordering::Relaxed);
}

/// The pre-enqueue drain found completions to consume. Atomics only (any IRQL).
pub(crate) fn note_drain_hit() {
    DRAIN_HIT.fetch_add(1, Ordering::Relaxed);
}

/// Fold one finished submit (accepted or not) into the totals. Atomics only.
pub(crate) fn finish(path: Path, clock: &Clock) {
    for (cell, ticks) in STAGE_TICKS.iter().zip(clock.stages().iter()) {
        if *ticks != 0 {
            cell.fetch_add(*ticks, Ordering::Relaxed);
        }
    }
    let total = clock.total();
    TOTAL_TICKS.fetch_add(total, Ordering::Relaxed);
    TOTAL_MAX_US.fetch_max(ss::ticks_to_us(total), Ordering::Relaxed);
    N.fetch_add(1, Ordering::Relaxed);
    PATH_N[path.index()].fetch_add(1, Ordering::Relaxed);
}

/// The knob in force, written at every StartDevice (`AdapterKnobs::read_at_start`). PASSIVE.
pub(crate) fn record_knob(on: bool) {
    crate::diag::record_named_bytes(b"SubPoolOn", on as u32);
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
    write_block();
}

/// Mirror the counters to the service key once a display submit was measured. PASSIVE only.
pub(crate) fn publish_counters() {
    if N.load(Ordering::Relaxed) == 0 {
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
}
