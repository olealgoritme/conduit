//! Asynchronous composed present (`BltAsync`) and the dropped CPU mirror (`BltNoMirror`): the I/O
//! half. The decisions are `helios_kmd_logic::blt_async` (host-tested); the in-flight table and
//! the ownership hand-back are in `virtio/gpu/blt_async.rs`. Design, the analysis of what the DDI
//! waited for, the ordering rules, the hazards and the hardware checklist:
//! `docs/zero-copy-present.md`, "Asynchronous composed present (BltAsync, BltNoMirror)".
//!
//! Scope. Both knobs apply to ONE class of Present: a Blt whose source is an adopted foreign
//! (NVK-on-RM) allocation the KMD copies as one (`ForeignCopy=1`) and whose destination is a KMD
//! standard buffer (DWM's redirection surface); with `BltAsyncVenus=1` also a Venus-native source.
//! Every other Blt takes the arm it always took. `helios_kmd_logic::blt_async::entry` decides, once
//! per Blt and before anything else, whether a knob acts; `BltEntry*`/`BltNoEntry*` say why not.
//!
//! Knobs (REG_DWORD in the service key, default 0 = the previous behaviour; read at every
//! StartDevice by [`reset_for_start`], and once on first use):
//!
//! * `BltAsync` 1: such a Blt returns from `DxgkDdiPresent` without a CPU wait. The copy is
//!   submitted either by the DDI itself (the producer has finished or there is none: DIRECT) or
//!   by the HPD worker through the WindowedBlt FIFO once the producer's boundary has been reached
//!   (DEFERRED). The Present's DMA fence retires with the copy.
//! * `BltNoMirror` 1: such a Blt does not CPU-copy the frame into the destination's system
//!   backing; the backing is marked "system copy invalid" instead, so a later page-in does not
//!   resurrect stale pixels and a later eviction pulls from the GPU blob.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::blt_async::COUNTERS`; atomics,
//! written to the registry by [`publish_counters`] from `publish_nvrm_counters`):
//!
//! * `BltAsyncKnob` / `BltNoMirKnob` / `BltVenusKnob`: the knobs in force.
//! * `BltEntrySeen`: Blts that reached the arm. `BltEntryDec`: of those, the ones that passed every
//!   precondition and were decided; `BltEntryOk`: of those, the ones a knob acts on (`BltAsyncN` +
//!   `BltAsyncFall` + the no-mirror Blts). `BltEntryWhy` / `BltEntryMask`: the last and the set of
//!   reasons none did (`blt_async::EntryWhy`, bit `code - 1`: 1 both knobs off, 2 snapshot, 3 Venus
//!   source with `BltAsyncVenus` 0, 4 foreign source with `ForeignCopy` 0, 5 destination not a
//!   standard buffer, 6 returned before the decision). Per reason: `BltNoEntryK` (knobs),
//!   `BltNoEntryM` (snapshot), `BltNoEntryF` (source not eligible), `BltNoEntryFc` (foreign,
//!   `ForeignCopy` off: also `FcOff`), `BltNoEntryS` (destination), `BltNoEntryO` (before the
//!   decision: `BltEntrySeen - BltEntryDec`).
//! * `BltAsyncN`: asynchronous Blts made (`BltAsyncDir` direct, `BltAsyncDefer` deferred to the
//!   worker because the producer had not finished, or the mirror is on, or an older frame for the
//!   destination was still queued).
//! * `BltAsyncInfl` / `BltAsyncPk`: in flight now (submitted or queued, copy not yet complete) and
//!   the most at once.
//! * `BltAsyncLat0..7`: submission to copy completion, 8 buckets (< 250 us, < 500 us, < 1 ms,
//!   < 2 ms, < 4 ms, < 8 ms, < 16 ms, more). `BltDeferUs`: microseconds a deferred Blt waited
//!   from the Present to its submission.
//! * `BltAsyncFail`: copies the host answered with an error (the Present still completes: the
//!   destination keeps the previous frame). `BltAsyncFall`: Blts that were eligible and fell back
//!   to the legacy arm; `BltAsyncWhy` the last reason (`blt_async::Why::code`), `BltAsyncMask`
//!   every reason seen (bit `code - 1`). `BltAsyncBusy`: of those, the destination was still
//!   owned by a reader or a writer that is not one of ours. `BltDrainN`: legacy Blts that waited
//!   for the queued copies of their destination first.
//! * `BltWaitN` / `BltWaitUs` / `BltWait0..7`: the legacy arm's CPU wait for the copy's fence (the
//!   same buckets): what `BltAsync` saves.
//! * `BltMirrorN` / `BltMirrorSk` / `BltMirrorUs`: CPU mirrors done, skipped by `BltNoMirror`,
//!   and the microseconds spent in them. `BltNoMirInv`: destination system copies newly marked
//!   invalid.
//! * `BltSrcBusy`: Presents of a source an earlier asynchronous copy was still reading (the
//!   swap-chain buffer rotation is shallower than the copy's latency, or a buffer is presented
//!   twice). `BltLookKnob` / `BltLookN`: the worker's lookahead depth (`BltLookahead`) and the
//!   copies it dispatched ahead of a front entry that could not go.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::blt_async::{self as ba, Route, Why};
use wdk_sys::ntddk::KeQueryInterruptTimePrecise;

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::ddi::present_packet::PresentSubmissionPrivate;
use crate::irql::PassiveLevel;
use crate::virtio::ctrl::BltSubmit;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;

/// The service-key values, read through `diag::knobs` (the one inventory).
const UNREAD: u32 = u32::MAX;
static ASYNC_KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static NO_MIRROR_KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static VENUS_KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static ASYNC_N: AtomicU32 = AtomicU32::new(0);
static DIRECT: AtomicU32 = AtomicU32::new(0);
static DEFERRED: AtomicU32 = AtomicU32::new(0);
static INFLIGHT: AtomicU32 = AtomicU32::new(0);
static PEAK: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
static FELL_BACK: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static WHY_MASK: AtomicU32 = AtomicU32::new(0);
static BUSY: AtomicU32 = AtomicU32::new(0);
static DEFER_US: AtomicU32 = AtomicU32::new(0);
static DRAINS: AtomicU32 = AtomicU32::new(0);
static LAT: [AtomicU32; ba::BUCKETS] = [const { AtomicU32::new(0) }; ba::BUCKETS];
static WAIT_N: AtomicU32 = AtomicU32::new(0);
static WAIT_US: AtomicU32 = AtomicU32::new(0);
static WAIT: [AtomicU32; ba::BUCKETS] = [const { AtomicU32::new(0) }; ba::BUCKETS];
static MIRROR_N: AtomicU32 = AtomicU32::new(0);
static MIRROR_SKIPPED: AtomicU32 = AtomicU32::new(0);
static MIRROR_US: AtomicU32 = AtomicU32::new(0);
static NOMIR_INVALID: AtomicU32 = AtomicU32::new(0);
static SRC_BUSY: AtomicU32 = AtomicU32::new(0);
static LOOKAHEAD: AtomicU32 = AtomicU32::new(ba::LOOKAHEAD_DEFAULT);
static LOOK_N: AtomicU32 = AtomicU32::new(0);
static ENTRY_SEEN: AtomicU32 = AtomicU32::new(0);
static ENTRY_DECIDED: AtomicU32 = AtomicU32::new(0);
static ENTRY_OK: AtomicU32 = AtomicU32::new(0);
static ENTRY_WHY: AtomicU32 = AtomicU32::new(0);
static ENTRY_MASK: AtomicU32 = AtomicU32::new(0);
static NO_ENTRY_KNOB: AtomicU32 = AtomicU32::new(0);
static NO_ENTRY_SNAPSHOT: AtomicU32 = AtomicU32::new(0);
static NO_ENTRY_SOURCE: AtomicU32 = AtomicU32::new(0);
static NO_ENTRY_FC_OFF: AtomicU32 = AtomicU32::new(0);
static NO_ENTRY_DST: AtomicU32 = AtomicU32::new(0);
// `DxgkDdiPresent`'s own wall time, by arm (`PrDdi*`): where the Present cost sits, in this DDI or
// in dxgkrnl before it calls us.
static PRES_BLT_N: AtomicU32 = AtomicU32::new(0);
static PRES_BLT_US: AtomicU32 = AtomicU32::new(0);
static PRES_BLT_MAX: AtomicU32 = AtomicU32::new(0);
static PRES_BLT: [AtomicU32; ba::BUCKETS] = [const { AtomicU32::new(0) }; ba::BUCKETS];
static PRES_FLIP_N: AtomicU32 = AtomicU32::new(0);
static PRES_FLIP_US: AtomicU32 = AtomicU32::new(0);
static PRES_FLIP_MAX: AtomicU32 = AtomicU32::new(0);

/// Interrupt time in 100 ns units; legal at any IRQL, no lock.
pub(crate) fn now_100ns() -> u64 {
    let mut qpc_timestamp = 0;
    // SAFETY: a scalar time read with a valid output location, legal at any IRQL.
    unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) }
}

#[inline(never)]
fn read_knob(cell: &AtomicU32, name: crate::diag::KnobName) -> bool {
    let v = crate::diag::read_config_dword(name, 0) != 0;
    cell.store(v as u32, Ordering::Relaxed);
    v
}

fn knob(cell: &AtomicU32, name: crate::diag::KnobName) -> bool {
    match cell.load(Ordering::Relaxed) {
        UNREAD => read_knob(cell, name),
        v => v != 0,
    }
}

/// `BltAsync` is on. One relaxed load once read.
pub(crate) fn async_on() -> bool {
    knob(&ASYNC_KNOB, crate::diag::knobs::BLT_ASYNC)
}

/// `BltNoMirror` is on. One relaxed load once read.
pub(crate) fn no_mirror_on() -> bool {
    knob(&NO_MIRROR_KNOB, crate::diag::knobs::BLT_NO_MIRROR)
}

/// `BltAsyncVenus` is on: the knobs also act on Venus-native sources. One relaxed load once read.
pub(crate) fn async_venus_on() -> bool {
    knob(&VENUS_KNOB, crate::diag::knobs::BLT_ASYNC_VENUS)
}

/// A new transport generation: the knobs are read again (a `reg add` + `pnputil /restart-device`
/// takes effect without a reboot), mirrored with the value in force, 0 included, and the counters
/// are zeroed so a value an earlier run left in the service key is never read as this
/// generation's. PASSIVE.
pub(crate) fn reset_for_start() {
    for cell in [
        &ASYNC_N,
        &DIRECT,
        &DEFERRED,
        &INFLIGHT,
        &PEAK,
        &FAILED,
        &FELL_BACK,
        &LAST_WHY,
        &WHY_MASK,
        &BUSY,
        &DEFER_US,
        &DRAINS,
        &WAIT_N,
        &WAIT_US,
        &MIRROR_N,
        &MIRROR_SKIPPED,
        &MIRROR_US,
        &NOMIR_INVALID,
        &ENTRY_SEEN,
        &ENTRY_DECIDED,
        &ENTRY_OK,
        &ENTRY_WHY,
        &ENTRY_MASK,
        &NO_ENTRY_KNOB,
        &NO_ENTRY_SNAPSHOT,
        &NO_ENTRY_SOURCE,
        &NO_ENTRY_FC_OFF,
        &NO_ENTRY_DST,
        &PRES_BLT_N,
        &PRES_BLT_US,
        &PRES_BLT_MAX,
        &PRES_FLIP_N,
        &PRES_FLIP_US,
        &PRES_FLIP_MAX,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
    for cell in LAT.iter().chain(WAIT.iter()).chain(PRES_BLT.iter()) {
        cell.store(0, Ordering::Relaxed);
    }
    SRC_BUSY.store(0, Ordering::Relaxed);
    LOOK_N.store(0, Ordering::Relaxed);
    let a = read_knob(&ASYNC_KNOB, crate::diag::knobs::BLT_ASYNC);
    let m = read_knob(&NO_MIRROR_KNOB, crate::diag::knobs::BLT_NO_MIRROR);
    let v = read_knob(&VENUS_KNOB, crate::diag::knobs::BLT_ASYNC_VENUS);
    let depth = ba::clamp_lookahead(crate::diag::read_config_dword(
        crate::diag::knobs::BLT_LOOKAHEAD,
        ba::LOOKAHEAD_DEFAULT,
    )) as u32;
    LOOKAHEAD.store(depth, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"BltAsyncKnob", a as u32);
    crate::diag::record_named_bytes(b"BltNoMirKnob", m as u32);
    crate::diag::record_named_bytes(b"BltVenusKnob", v as u32);
    crate::diag::record_named_bytes(b"BltLookKnob", depth);
}

/// How many entries of the WindowedBlt ready queue the worker looks at (`BltLookahead`). One
/// relaxed load; the registry is read at StartDevice only (the worker holds a spinlock).
pub(crate) fn lookahead() -> usize {
    LOOKAHEAD.load(Ordering::Relaxed) as usize
}

/// The worker dispatched a copy ahead of a front entry that could not go.
pub(crate) fn note_lookahead() {
    LOOK_N.fetch_add(1, Ordering::Relaxed);
}

/// A Blt reached the non-RM-primary Blt arm of `DxgkDdiPresent` (before any precondition).
pub(crate) fn note_entry_seen() {
    ENTRY_SEEN.fetch_add(1, Ordering::Relaxed);
}

/// The entry decision of a Blt that passed every precondition (`helios_kmd_logic::blt_async::
/// entry`): counted by outcome, and by the first reason neither knob acted.
pub(crate) fn note_entry(entry: ba::EntryDecision) {
    ENTRY_DECIDED.fetch_add(1, Ordering::Relaxed);
    let Some(why) = entry.why else {
        ENTRY_OK.fetch_add(1, Ordering::Relaxed);
        return;
    };
    ENTRY_WHY.store(why.code(), Ordering::Relaxed);
    ENTRY_MASK.fetch_or(why.bit(), Ordering::Relaxed);
    let cell = match why {
        ba::EntryWhy::KnobOff => &NO_ENTRY_KNOB,
        ba::EntryWhy::Snapshot => &NO_ENTRY_SNAPSHOT,
        ba::EntryWhy::NotForeign => &NO_ENTRY_SOURCE,
        ba::EntryWhy::ForeignCopyOff => &NO_ENTRY_FC_OFF,
        ba::EntryWhy::NotBuffer => &NO_ENTRY_DST,
        // Derived at publish time, never decided.
        ba::EntryWhy::Other => return,
    };
    cell.fetch_add(1, Ordering::Relaxed);
}

/// The source of a Present is still being read by an earlier asynchronous copy.
pub(crate) fn note_source_busy() {
    SRC_BUSY.fetch_add(1, Ordering::Relaxed);
}

/// The Level 5 frame edge a finished asynchronous copy owes (`blt_async::edge_owed`): atomics
/// only, legal at DISPATCH (the completion DPC) and at PASSIVE.
pub(crate) fn raise_edge(
    adapter: &AdapterContext,
    finish: ba::Finish,
    copy_ok: bool,
    destination: u32,
) {
    if let Some(edge) = ba::edge_owed(finish, copy_ok) {
        crate::virtio::rm_client::sysmem_flip::primary_changed(adapter, edge, destination);
    }
}

const LAT_NAMES: [&[u8]; ba::BUCKETS] = [
    b"BltAsyncLat0",
    b"BltAsyncLat1",
    b"BltAsyncLat2",
    b"BltAsyncLat3",
    b"BltAsyncLat4",
    b"BltAsyncLat5",
    b"BltAsyncLat6",
    b"BltAsyncLat7",
];

const PRES_NAMES: [&[u8]; ba::BUCKETS] = [
    b"PrDdiBlt0",
    b"PrDdiBlt1",
    b"PrDdiBlt2",
    b"PrDdiBlt3",
    b"PrDdiBlt4",
    b"PrDdiBlt5",
    b"PrDdiBlt6",
    b"PrDdiBlt7",
];

const WAIT_NAMES: [&[u8]; ba::BUCKETS] = [
    b"BltWait0",
    b"BltWait1",
    b"BltWait2",
    b"BltWait3",
    b"BltWait4",
    b"BltWait5",
    b"BltWait6",
    b"BltWait7",
];

/// Mirror the counters to the service key, once an event happened. PASSIVE only; with the NVRM
/// counters.
pub(crate) fn publish_counters() {
    let events = ASYNC_N.load(Ordering::Relaxed)
        | FELL_BACK.load(Ordering::Relaxed)
        | WAIT_N.load(Ordering::Relaxed)
        | MIRROR_N.load(Ordering::Relaxed)
        | MIRROR_SKIPPED.load(Ordering::Relaxed)
        | FAILED.load(Ordering::Relaxed)
        | SRC_BUSY.load(Ordering::Relaxed)
        | LOOK_N.load(Ordering::Relaxed)
        | ENTRY_SEEN.load(Ordering::Relaxed)
        | PRES_BLT_N.load(Ordering::Relaxed)
        | PRES_FLIP_N.load(Ordering::Relaxed);
    if events == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"BltAsyncN", ASYNC_N.load(Ordering::Relaxed));
    rec(b"BltAsyncDir", DIRECT.load(Ordering::Relaxed));
    rec(b"BltAsyncDefer", DEFERRED.load(Ordering::Relaxed));
    rec(b"BltAsyncInfl", INFLIGHT.load(Ordering::Relaxed));
    rec(b"BltAsyncPk", PEAK.load(Ordering::Relaxed));
    rec(b"BltAsyncFail", FAILED.load(Ordering::Relaxed));
    rec(b"BltAsyncFall", FELL_BACK.load(Ordering::Relaxed));
    rec(b"BltAsyncWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"BltAsyncMask", WHY_MASK.load(Ordering::Relaxed));
    rec(b"BltAsyncBusy", BUSY.load(Ordering::Relaxed));
    rec(b"BltDeferUs", DEFER_US.load(Ordering::Relaxed));
    rec(b"BltDrainN", DRAINS.load(Ordering::Relaxed));
    for (name, cell) in LAT_NAMES.iter().zip(LAT.iter()) {
        rec(name, cell.load(Ordering::Relaxed));
    }
    rec(b"BltWaitN", WAIT_N.load(Ordering::Relaxed));
    rec(b"BltWaitUs", WAIT_US.load(Ordering::Relaxed));
    for (name, cell) in WAIT_NAMES.iter().zip(WAIT.iter()) {
        rec(name, cell.load(Ordering::Relaxed));
    }
    rec(b"BltMirrorN", MIRROR_N.load(Ordering::Relaxed));
    rec(b"BltMirrorSk", MIRROR_SKIPPED.load(Ordering::Relaxed));
    rec(b"BltMirrorUs", MIRROR_US.load(Ordering::Relaxed));
    rec(b"BltNoMirInv", NOMIR_INVALID.load(Ordering::Relaxed));
    rec(b"BltSrcBusy", SRC_BUSY.load(Ordering::Relaxed));
    rec(b"BltLookN", LOOK_N.load(Ordering::Relaxed));
    // The entry decision. `BltNoEntryO` is derived: Blts of the arm that returned before the
    // decision (a precondition: capacity, resolution, format, kind, snapshot validation,
    // descriptors, extent). Not an independent count, so it cannot drift from the others.
    let seen = ENTRY_SEEN.load(Ordering::Relaxed);
    let decided = ENTRY_DECIDED.load(Ordering::Relaxed);
    let other = seen.saturating_sub(decided);
    let mut mask = ENTRY_MASK.load(Ordering::Relaxed);
    let mut why = ENTRY_WHY.load(Ordering::Relaxed);
    if other != 0 {
        mask |= ba::EntryWhy::Other.bit();
        if why == 0 {
            why = ba::EntryWhy::Other.code();
        }
    }
    rec(b"BltEntrySeen", seen);
    rec(b"BltEntryDec", decided);
    rec(b"BltEntryOk", ENTRY_OK.load(Ordering::Relaxed));
    rec(b"BltEntryWhy", why);
    rec(b"BltEntryMask", mask);
    rec(b"BltNoEntryK", NO_ENTRY_KNOB.load(Ordering::Relaxed));
    rec(b"BltNoEntryM", NO_ENTRY_SNAPSHOT.load(Ordering::Relaxed));
    rec(b"BltNoEntryF", NO_ENTRY_SOURCE.load(Ordering::Relaxed));
    rec(b"BltNoEntryFc", NO_ENTRY_FC_OFF.load(Ordering::Relaxed));
    rec(b"BltNoEntryS", NO_ENTRY_DST.load(Ordering::Relaxed));
    rec(b"BltNoEntryO", other);
    rec(b"PrDdiBltN", PRES_BLT_N.load(Ordering::Relaxed));
    rec(b"PrDdiBltUs", PRES_BLT_US.load(Ordering::Relaxed));
    rec(b"PrDdiBltMax", PRES_BLT_MAX.load(Ordering::Relaxed));
    for (name, cell) in PRES_NAMES.iter().zip(PRES_BLT.iter()) {
        rec(name, cell.load(Ordering::Relaxed));
    }
    rec(b"PrDdiFlipN", PRES_FLIP_N.load(Ordering::Relaxed));
    rec(b"PrDdiFlipUs", PRES_FLIP_US.load(Ordering::Relaxed));
    rec(b"PrDdiFlipMax", PRES_FLIP_MAX.load(Ordering::Relaxed));
}

/// `DxgkDdiPresent` returned after `dt` (100 ns units, the whole exported DDI): a Blt arm (`blt`)
/// or a flip arm. Atomics only, any IRQL; the Blt arm also keeps the histogram.
pub(crate) fn note_present_wall(blt: bool, dt: u64) {
    let us = ba::us32(dt);
    if blt {
        PRES_BLT_N.fetch_add(1, Ordering::Relaxed);
        PRES_BLT_US.fetch_add(us, Ordering::Relaxed);
        PRES_BLT_MAX.fetch_max(us, Ordering::Relaxed);
        PRES_BLT[ba::lat_bucket(dt)].fetch_add(1, Ordering::Relaxed);
    } else {
        PRES_FLIP_N.fetch_add(1, Ordering::Relaxed);
        PRES_FLIP_US.fetch_add(us, Ordering::Relaxed);
        PRES_FLIP_MAX.fetch_max(us, Ordering::Relaxed);
    }
}

// ---- counters, callable at any IRQL (atomics only) -----------------------------------------

fn dec(cell: &AtomicU32) {
    let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

/// One more asynchronous Blt in flight (submitted or queued).
pub(crate) fn note_infl_add() {
    let now = INFLIGHT.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// One asynchronous Blt left the in-flight set (completed, cancelled or abandoned).
pub(crate) fn note_infl_sub() {
    dec(&INFLIGHT);
}

/// The copy of an asynchronous Blt completed (`t_submit` is its submission's interrupt time).
pub(crate) fn note_copy_done(t_submit: u64, ok: bool) {
    let dt = now_100ns().saturating_sub(t_submit);
    LAT[ba::lat_bucket(dt)].fetch_add(1, Ordering::Relaxed);
    if !ok {
        FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// A deferred Blt left the FIFO for the ring: it waited `t_submit - t_queue`.
pub(crate) fn note_defer_wait(t_queue: u64, t_submit: u64) {
    DEFER_US.fetch_add(ba::us32(t_submit.saturating_sub(t_queue)), Ordering::Relaxed);
}

/// The legacy arm waited for the copy's fence from `t0` until now.
pub(crate) fn note_wait(t0: u64) {
    let dt = now_100ns().saturating_sub(t0);
    WAIT_N.fetch_add(1, Ordering::Relaxed);
    WAIT_US.fetch_add(ba::us32(dt), Ordering::Relaxed);
    WAIT[ba::lat_bucket(dt)].fetch_add(1, Ordering::Relaxed);
}

/// A CPU mirror of a Present destination ran from `t0` until now.
pub(crate) fn note_mirror(t0: u64) {
    let dt = now_100ns().saturating_sub(t0);
    MIRROR_N.fetch_add(1, Ordering::Relaxed);
    MIRROR_US.fetch_add(ba::us32(dt), Ordering::Relaxed);
}

/// The CPU mirror of a foreign-source Blt was skipped (`BltNoMirror`).
pub(crate) fn note_mirror_skipped() {
    MIRROR_SKIPPED.fetch_add(1, Ordering::Relaxed);
}

/// A Blt that was eligible took the legacy arm.
fn fall(why: Why) -> Taken {
    FELL_BACK.fetch_add(1, Ordering::Relaxed);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    WHY_MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if why == Why::DstBusy {
        BUSY.fetch_add(1, Ordering::Relaxed);
    }
    Taken::Legacy
}

/// The destination's system copy is stale from this frame on (`BltNoMirror`): mark it invalid so
/// a page-in does not copy older system pages over the blob the GPU copy is about to write, and
/// a later eviction (which copies blob to system) revalidates it. Nothing is marked when VidMm
/// holds no system pages for the destination (nothing could be resurrected, and the invalid set
/// is bounded). PASSIVE; spinlocks only.
pub(crate) fn mark_stale(adapter: &AdapterContext, resource_id: u32) {
    use helios_kmd_logic::paging::Mark;
    match adapter.system_backings.mark_stale_if_backed(resource_id) {
        Some(Mark::Newly) | Some(Mark::Overflow) => {
            NOMIR_INVALID.fetch_add(1, Ordering::Relaxed);
        }
        Some(Mark::Already) | Some(Mark::Ignored) | None => {}
    }
}

// ---- the route ------------------------------------------------------------------------------

/// What [`try_async`] did with a Blt.
pub(crate) enum Taken {
    /// Not asynchronous: the legacy arm runs (the reason was counted).
    Legacy,
    /// Submitted by the DDI; the Present's private record names this wire fence.
    Direct(u64),
    /// Queued for the HPD worker; the Present's private record carries this token.
    Deferred(u64),
}

/// Take the Blt asynchronously when the rules allow (`helios_kmd_logic::blt_async::decide`). The
/// caller has the entry decision (`ba::entry`: [`async_on`], a source class the knobs act on, a
/// standard-buffer destination, no snapshot), so `foreign_source` below is true by construction.
///
/// `Ok(Legacy)` leaves everything as it was: nothing is queued, owned or marked that the legacy
/// arm does not expect. `Err` is only the private record refusing the copy's fence AFTER the copy
/// was submitted (the capacity was checked before any host work, so it cannot happen; the arm
/// reports it as it always did).
///
/// # Safety
/// `args` is dxgkrnl's `DXGKARG_PRESENT` for this call (PASSIVE_LEVEL) and its private-data
/// pointer is writable for `DmaBufferPrivateDataSize` bytes.
pub(crate) unsafe fn try_async(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    boundary: Option<u64>,
) -> Result<Taken, NTSTATUS> {
    let destination_resource = destination.resource_id();
    let source_resource = source.resource_id();
    let facts =
        adapter.with_virtio(|v| v.blt_async_facts(boundary, destination_resource, source_resource));
    let Ok(facts) = facts else {
        return Ok(fall(Why::SubmitRefused));
    };
    if facts.source_busy {
        note_source_busy();
    }
    let (boundary_state, deferred_pending, room) =
        (facts.boundary, facts.deferred_pending, facts.room);
    // `GuestBlob`: a destination with a live guest blob needs no mirror, so the DIRECT route is
    // open to it as with `BltNoMirror` (one spinlock; the copy re-checks under the Venus mutex
    // and falls back when the guest blob went in between).
    let no_mirror = no_mirror_on();
    let guest_ready = adapter
        .system_backings
        .guest_record(destination_resource)
        .is_some_and(|record| record.copy_target());
    let route = ba::decide(ba::Facts {
        async_on: true,
        no_mirror_on: no_mirror || guest_ready,
        foreign_source: true,
        snapshot: false,
        dst_standard_buffer: matches!(destination, PresentDestinationDesc::StandardBuffer(_)),
        boundary: boundary_state,
        dst_deferred_pending: deferred_pending,
        table_has_room: room,
    });
    let taken = match route {
        Route::Legacy { why } | Route::LegacyAfterDrain { why } => Ok(fall(why)),
        Route::Direct => unsafe {
            direct(passive, adapter, args, source, destination, !no_mirror)
        },
        Route::Deferred => unsafe {
            deferred(passive, adapter, args, source, destination, boundary)
        },
    };
    // Every way out to the legacy arm (a refused queue, token or submission included): the arm
    // that follows must not reach the host before an older copy queued for the destination.
    if let Ok(Taken::Legacy) = taken {
        drain(passive, adapter, destination_resource);
    }
    taken
}

/// Wait (PASSIVE, bounded) until no queued copy names `resource_id` as its destination. The
/// legacy arm that follows must not reach the host before an older frame queued for the same
/// buffer, or the older frame would land last.
fn drain(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    // Nominal 320 ms, about 5 s at Windows' common timer quantum: the budget of
    // `begin_present_buffer_write_legacy`, which this wait precedes.
    let mut slices = 0u32;
    while slices < 320 {
        let pending = adapter
            .with_virtio(|v| v.blt_dst_deferred_pending(resource_id))
            .unwrap_or(false);
        if !pending {
            return;
        }
        if slices == 0 {
            DRAINS.fetch_add(1, Ordering::Relaxed);
        }
        crate::virtio::ctrl::sleep_ms(passive, 1);
        slices += 1;
    }
}

/// DIRECT: submit the copy from the DDI and return. The destination ownership is taken in the
/// same transport critical section as the enqueue and handed back by the completion DPC; the
/// Present's DMA fence retires with the copy's wire fence exactly as it did when the DDI waited
/// (`PresentSubmissionPrivate::merge_fence`).
///
/// `need_guest`: the route was open only because of the destination's guest blob (`GuestBlob`
/// with `BltNoMirror` 0). If the guest blob is gone by the time the Venus mutex is held, nothing
/// is submitted and the legacy arm (which mirrors) runs.
unsafe fn direct(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    need_guest: bool,
) -> Result<Taken, NTSTATUS> {
    let destination_resource = destination.resource_id();
    let copy = adapter.with_venus_client(passive, |client| {
        // Decided under the Venus mutex, with the predicate the copy applies: a copy into the
        // guest buffer writes the system pages themselves; any other copy makes them older
        // than the blob from here on (marked before the copy, spinlocks only).
        let guest = client.guest_target_live(destination_resource);
        if need_guest && !guest {
            return Err(VirtioError::DeviceError);
        }
        if !guest {
            mark_stale(adapter, destination_resource);
        }
        client.submit_present_blt_direct(adapter, source, destination)
    });
    // (`submit_present_blt_direct` passes the source's id down: the in-flight table holds a
    // read-ledger ticket on it until the copy retires.)
    let fence = match copy {
        Ok(Ok(BltSubmit::Fence(fence))) => fence,
        Ok(Ok(BltSubmit::DstBusy)) => return Ok(fall(Why::DstBusy)),
        // Queue pressure, no memory, a transport that is gone: the legacy arm decides what a
        // refusal means (a foreign source's refusal is a counted success there).
        Ok(Err(_)) | Err(_) => return Ok(fall(Why::SubmitRefused)),
    };
    // Capacity was checked before host work was queued, so this cannot fail. Merge preserves the
    // newest fence if dxgkrnl batches more than one Present into the same DMA private-data buffer.
    if let Err(status) = unsafe {
        PresentSubmissionPrivate::merge_fence(
            args.pDmaBufferPrivateData,
            args.DmaBufferPrivateDataSize,
            fence,
        )
    } {
        return Err(status);
    }
    ASYNC_N.fetch_add(1, Ordering::Relaxed);
    DIRECT.fetch_add(1, Ordering::Relaxed);
    Ok(Taken::Direct(fence))
}

/// DEFERRED: prepare the reusable copy, queue it in the WindowedBlt FIFO under the producer's
/// boundary and return. SubmitCommand admits it once the destination's residency is effective;
/// the HPD worker submits it when the boundary is ready and the destination can be written; its
/// ring completion (and, with the mirror on, the worker's mirror) terminalizes the token the
/// Present's DMA fence waits for.
unsafe fn deferred(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    boundary: Option<u64>,
) -> Result<Taken, NTSTATUS> {
    let Some(boundary) = boundary else {
        return Ok(fall(Why::NoBoundaryMirror));
    };
    let no_mirror_knob = no_mirror_on();
    // SAFETY of the lock order: scanout -> venus -> virtio, as the snapshot arm of the same
    // DDI. Cache preparation may block only while the Venus mutex is held; the FIFO insertion is
    // a preallocated spinlock-only mutation.
    let queued = adapter.with_scanout_lifecycle(passive, |lock| {
        // May copy into the destination's guest buffer (`GuestBlob`): such a copy owes no
        // mirror, so the worker gets `no_mirror` for it and marks nothing stale.
        let prepared = lock.with_venus_client(|client| {
            client.prepare_present_blt_guest(adapter, source, destination)
        });
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(VirtioError::DeviceError),
        };
        let no_mirror = no_mirror_knob || prepared.guest_target();
        adapter
            .with_virtio(|v| {
                v.queue_async_blt(adapter, source, destination, prepared, boundary, no_mirror)
            })
            .unwrap_or(Err(VirtioError::DeviceError))
    });
    let token = match queued {
        Ok(token) => token,
        Err(_) => return Ok(fall(Why::QueueRefused)),
    };
    if let Err(_status) = unsafe {
        PresentSubmissionPrivate::merge_windowed_blt_token(
            args.pDmaBufferPrivateData,
            args.DmaBufferPrivateDataSize,
            token,
            boundary,
        )
    } {
        // The record cannot carry the token (a request of another stream already owns it, or no
        // room): nothing was submitted, so cancel the request and let the legacy arm run.
        let _ = adapter.with_virtio(|v| v.cancel_windowed_blt(adapter, token, boundary));
        return Ok(fall(Why::TokenRefused));
    }
    ASYNC_N.fetch_add(1, Ordering::Relaxed);
    DEFERRED.fetch_add(1, Ordering::Relaxed);
    Ok(Taken::Deferred(token))
}
