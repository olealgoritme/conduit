//! The registry mirror's own thread: every registry write that is not part of a flip.
//!
//! `stall_diag::publish_counters` (the stall-diagnosis block, the `Vs*` heartbeat values, the
//! device-lost block, the `FlipLat*` / `IfGap*` / `Fa*` blocks of `flip_lat` / `flip_announce`: well
//! over a hundred `RtlWriteRegistryValue` calls) used to run ON THE HPD WORKER, between two flips,
//! from `queue_active_scanout_refresh` (`HpdSite` 19) and from the Nv* mirror. One pass is a few
//! milliseconds to tens of them (T6, 332.1: `FlipMaxUs` 29 ms at site 19 in both rows), a whole
//! 240 Hz period or several, during which no flip is programmed: the stalls of `IfStall8`, and the
//! busy worker that made one flip a second decline its announce (`FaNoBusy`). v333 moved that
//! block; the `Vp*` dump (`HpdSite` 12, `scanout_trace::dump`: about 120 values), the `Nv*` mirror
//! (`HpdSite` 11, `publish_nvrm_counters`) and the pacing snapshot (`pacing_publish`: about 40)
//! stayed on the worker, and the 333 hardware run showed `FlipMaxUs` 16 to 31 ms at site 12
//! (`docs/kmd-rm-client.md` 15.18.16). They run here now.
//!
//! The worker (or the dump, or the Nv* mirror, or the refresh) only REQUESTS a pass
//! ([`request_bits`]: two atomic operations and a `KeSetEvent`, legal at any IRQL up to DISPATCH),
//! naming what it wants besides the base block ([`DUMP`], [`NV`], [`PACING`]); the thread does it,
//! then rests (500 ms with changed-only writes, 1 s without: `hpd_wake::mirror_rest_ms`), and
//! requests that arrive meanwhile coalesce into one pass. A pass:
//!
//! * runs at a priority BELOW the worker's (`MirPrio`, default 6 against the worker's 8), so it
//!   never delays the flip path for a processor;
//! * writes only values that changed (`MirChanged`, `diag::mirror`; every value again at least
//!   every 30 s), which makes a pass a few dozen writes instead of a hundred and fifty;
//! * rests for a millisecond after every `MirYield` writes (default 32: the system timer may round
//!   it up), so it does not hold a processor and the registry lock for tens of milliseconds.
//!
//! The adapter: the dump and the pacing snapshot read it, so [`start`] keeps its address for the
//! thread; [`stop`] clears it first, joins, and a thread that could not be joined ([`leaked`]) makes
//! `AdapterContext::stop_hpd` latch `hpd_worker_leaked`, which keeps the context from being freed
//! under it.
//!
//! Lifetime: started with the HPD worker (`AdapterContext::init_hpd`), stopped and joined in
//! front of it (`stop_hpd`). Without the thread (creation failed, render-only, `MirrorThread` 0)
//! [`running`] is false and the callers publish inline as before; the worker then gates the dump
//! and the Nv* mirror to moments with no flip in hand (`hpd_wake::dump_gate`).

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use helios_kmd_logic::hpd_wake;
use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, KeWaitForSingleObject, PsTerminateSystemThread};
use wdk_sys::{KEVENT, PVOID};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

/// The base block (`stall_diag::publish_counters`): every pass writes it.
pub(crate) const BASE: u32 = 1;
/// The periodic `Vp*` dump (`scanout_trace::dump`).
pub(crate) const DUMP: u32 = 2;
/// The `Nv*` counter mirror (`publish_nvrm_counters`).
pub(crate) const NV: u32 = 4;
/// The scanout pacing snapshot (`AdapterContext::pacing_publish`).
pub(crate) const PACING: u32 = 8;

struct Ev(UnsafeCell<KEVENT>);
// SAFETY: a KEVENT is a dispatcher object, shared by design; it is initialised before the thread
// exists and only touched through the Ke* calls.
unsafe impl Sync for Ev {}

/// Wake: a request, or stop. SynchronizationEvent (auto-clearing).
static REQ: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));
/// Rest: set by stop so the rest between two passes (and between two writes) ends at once.
/// NotificationEvent.
static STOP: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));
/// Exited latch. NotificationEvent.
static EXITED: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));

static THREAD: AtomicUsize = AtomicUsize::new(0);
/// The adapter the dump and the pacing snapshot read (0 = none): set by [`start`], cleared first
/// thing by [`stop`].
static ADAPTER: AtomicUsize = AtomicUsize::new(0);
/// The thread did not end within the join timeout: it may still wait on the events below, so they
/// must never be initialised again (the HPD worker's `hpd_worker_leaked`). Set by [`stop`],
/// refuses [`start`] for the rest of the image's life. `MirLeak` mirrors it.
static LEAKED: AtomicU32 = AtomicU32::new(0);
static STOPPING: AtomicU32 = AtomicU32::new(0);
/// What the next pass is to do (the bits above); 0 = nothing asked.
static WANTED: AtomicU32 = AtomicU32::new(0);
/// Passes published by the thread (`MirRuns`), requests (`MirReqs`), the longest pass in
/// microseconds (`MirMaxUs`) and the last (`MirLastUs`); passes that carried the dump, the Nv*
/// mirror and the pacing snapshot (`MirDumps`, `MirNvs`, `MirPaces`); rests taken inside passes
/// (`MirYlds`); the interrupt time (ms) of the last full refresh (0 = none).
static RUNS: AtomicU32 = AtomicU32::new(0);
static REQS: AtomicU32 = AtomicU32::new(0);
static MAX_US: AtomicU32 = AtomicU32::new(0);
static LAST_US: AtomicU32 = AtomicU32::new(0);
static DUMPS: AtomicU32 = AtomicU32::new(0);
static NVS: AtomicU32 = AtomicU32::new(0);
static PACES: AtomicU32 = AtomicU32::new(0);
static YLDS: AtomicU32 = AtomicU32::new(0);
static LAST_FULL_MS: AtomicU32 = AtomicU32::new(0);
/// `MirPrio`, `MirYield` and `MirChanged` in force (read at every start), and the priority the
/// thread had before it was lowered (`MirPrioOld`; 0 = not lowered).
static PRIO: AtomicU32 = AtomicU32::new(0);
static YIELD: AtomicU32 = AtomicU32::new(0);
static CHANGED: AtomicU32 = AtomicU32::new(1);
static PRIO_OLD: AtomicU32 = AtomicU32::new(0);

extern "C" {
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

extern "system" {
    /// `PsGetCurrentThread()` (exported; `KeGetCurrentThread` is an inline in wdm.h and is not): any IRQL.
    fn PsGetCurrentThread() -> usize;
    /// `KeSetPriorityThread(PKTHREAD, KPRIORITY)`: the previous priority. PASSIVE..DISPATCH.
    fn KeSetPriorityThread(thread: usize, priority: i32) -> i32;
}

/// Whether the thread exists: the callers then request instead of publishing inline.
#[inline]
pub(crate) fn running() -> bool {
    THREAD.load(Ordering::Acquire) != 0 && STOPPING.load(Ordering::Acquire) == 0
}

/// The thread could not be joined at the last [`stop`] and may still run: the adapter it reads must
/// not be freed.
#[inline]
pub(crate) fn leaked() -> bool {
    LEAKED.load(Ordering::Acquire) != 0
}

/// Ask for one publish pass (the base block). Atomics and `KeSetEvent(Wait = FALSE)`: any IRQL up
/// to DISPATCH.
pub(crate) fn request() {
    request_bits(BASE);
}

/// Ask for one publish pass with `bits` ([`DUMP`], [`NV`], [`PACING`]; the base block is always
/// part of it). Atomics and `KeSetEvent(Wait = FALSE)`: any IRQL up to DISPATCH. A request made
/// from inside a pass (the dump and the Nv* mirror ask for the base block, which the pass writes
/// anyway) is satisfied by the pass and dropped.
pub(crate) fn request_bits(bits: u32) {
    if crate::diag::mirror_in_pass() {
        return;
    }
    REQS.fetch_add(1, Ordering::Relaxed);
    if WANTED.fetch_or(bits | BASE, Ordering::AcqRel) == 0 {
        // SAFETY: REQ was initialised by `start` before the thread (and so `running`) existed.
        unsafe { KeSetEvent(REQ.0.get(), 0, 0) };
    }
}

/// Start the thread. PASSIVE (StartDevice). Idempotent.
///
/// # Safety
/// PASSIVE_LEVEL. `adapter` is the HPD worker's context and outlives the thread: [`stop`] joins it
/// before the context can go away, and a thread it could not join latches `hpd_worker_leaked`.
pub(crate) unsafe fn start(adapter: &AdapterContext) {
    if THREAD.load(Ordering::Acquire) != 0 {
        return;
    }
    // Kill switch: `MirrorThread` 0 leaves `running()` false, so every caller publishes inline on
    // the worker as before this thread existed. Read at every StartDevice, mirrored as `MirThrEff`.
    let want = crate::diag::read_config_dword(crate::diag::knobs::MIRROR_THREAD, 1) != 0;
    if LEAKED.load(Ordering::Acquire) != 0 {
        // A thread of an earlier start may still wait on these events: initialising them again
        // would corrupt the dispatcher objects it is queued on. Inline publishing from now on.
        crate::diag::record_named_bytes(b"MirThrEff", 0);
        crate::diag::record_named_bytes(b"MirLeak", 1);
        return;
    }
    crate::diag::record_named_bytes(b"MirThrEff", u32::from(want));
    if !want {
        return;
    }
    // The pass knobs, clamped (`hpd_wake`), mirrored as what is in force.
    let prio = hpd_wake::clamp_mirror_prio(crate::diag::read_config_dword(
        crate::diag::knobs::MIRROR_PRIO,
        hpd_wake::MIRROR_PRIO_DEFAULT,
    ));
    let yield_every = hpd_wake::clamp_mirror_yield(crate::diag::read_config_dword(
        crate::diag::knobs::MIRROR_YIELD,
        hpd_wake::MIRROR_YIELD_DEFAULT,
    ));
    let changed = crate::diag::read_config_dword(crate::diag::knobs::MIRROR_CHANGED, 1) != 0;
    PRIO.store(prio, Ordering::Relaxed);
    YIELD.store(yield_every, Ordering::Relaxed);
    CHANGED.store(u32::from(changed), Ordering::Relaxed);
    crate::diag::record_named_bytes(b"MirPrioEff", prio);
    crate::diag::record_named_bytes(b"MirYldEff", yield_every);
    crate::diag::record_named_bytes(b"MirChgEff", u32::from(changed));
    for c in [
        &REQS, &RUNS, &MAX_US, &LAST_US, &DUMPS, &NVS, &PACES, &YLDS, &LAST_FULL_MS, &PRIO_OLD,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    crate::diag::mirror_reset_counts();
    crate::diag::mirror_forget_all();
    crate::diag::record_named_bytes(b"MirLeak", 0);
    STOPPING.store(0, Ordering::Release);
    WANTED.store(0, Ordering::Release);
    ADAPTER.store(adapter as *const AdapterContext as usize, Ordering::Release);
    // SAFETY: no thread uses the events now (none exists); PASSIVE.
    unsafe {
        KeInitializeEvent(REQ.0.get(), 1, 0);
        KeInitializeEvent(STOP.0.get(), 0, 0);
        KeInitializeEvent(EXITED.0.get(), 0, 0);
    }
    let mut handle: wdk_sys::HANDLE = core::ptr::null_mut();
    const THREAD_ALL_ACCESS: u32 = 0x001F_FFFF;
    // SAFETY: PASSIVE_LEVEL; a kernel system thread with no context.
    let st = unsafe {
        wdk_sys::ntddk::PsCreateSystemThread(
            &mut handle,
            THREAD_ALL_ACCESS,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            Some(routine),
            core::ptr::null_mut(),
        )
    };
    if st == STATUS_SUCCESS && !handle.is_null() {
        THREAD.store(handle as usize, Ordering::Release);
    } else {
        ADAPTER.store(0, Ordering::Release);
    }
}

/// Stop and join the thread. PASSIVE; blocks up to 5 s. Idempotent; called before the HPD worker
/// is stopped.
pub(crate) fn stop() {
    let h = THREAD.swap(0, Ordering::AcqRel);
    // From here no pass starts a step that reads the adapter.
    ADAPTER.store(0, Ordering::Release);
    if h == 0 {
        return;
    }
    STOPPING.store(1, Ordering::Release);
    // SAFETY: initialised events; PASSIVE.
    unsafe {
        KeSetEvent(STOP.0.get(), 0, 0);
        KeSetEvent(REQ.0.get(), 0, 0);
    }
    const JOIN_TIMEOUT_100NS: i64 = -50_000_000;
    let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
    timeout.QuadPart = JOIN_TIMEOUT_100NS;
    // SAFETY: the exited latch is a NotificationEvent set by the thread just before it ends.
    let exited = unsafe { KeWaitForSingleObject(EXITED.0.get() as PVOID, 0, 0, 0, &mut timeout) };
    let mut joined = exited == STATUS_SUCCESS;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let mut obj: PVOID = core::ptr::null_mut();
    // SAFETY: `h` is the live handle `start` created; PsThreadType validates it.
    let st = unsafe {
        wdk_sys::ntddk::ObReferenceObjectByHandle(
            h as wdk_sys::HANDLE,
            SYNCHRONIZE,
            *PsThreadType,
            0,
            &mut obj,
            core::ptr::null_mut(),
        )
    };
    if st == STATUS_SUCCESS && !obj.is_null() {
        let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = JOIN_TIMEOUT_100NS;
        // SAFETY: waiting on the ETHREAD at PASSIVE_LEVEL, then releasing our reference.
        let wait = unsafe { KeWaitForSingleObject(obj, 0, 0, 0, &mut timeout) };
        unsafe { wdk_sys::ntddk::ObfDereferenceObject(obj) };
        joined = wait == STATUS_SUCCESS;
    }
    if !joined {
        // Trade a hang for a live thread: counted (`MirLeak`), and `start` refuses to run again.
        LEAKED.store(1, Ordering::Release);
        crate::diag::record_named_bytes(b"MirLeak", 1);
    }
    // SAFETY: closing the handle we created.
    let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
}

/// A short rest inside a pass (`diag::record_named_bytes` calls it after every `MirYield` writes):
/// a one-millisecond relative wait on the stop latch, so a stop ends it at once and the system
/// timer's rounding is the only cost. Only ever called by the thread, at PASSIVE.
pub(crate) fn rest() {
    YLDS.fetch_add(1, Ordering::Relaxed);
    let mut interval: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
    interval.QuadPart = -10_000;
    // SAFETY: initialised NotificationEvent with a relative timeout; PASSIVE (the thread).
    let _ = unsafe { KeWaitForSingleObject(STOP.0.get() as PVOID, 0, 0, 0, &mut interval) };
}

/// Lower this thread below the HPD worker (`MirPrio`).
fn lower_priority() {
    let prio = PRIO.load(Ordering::Relaxed);
    if prio == 0 {
        return;
    }
    // SAFETY: the current thread, PASSIVE; KeSetPriorityThread returns the previous priority.
    let old = unsafe { KeSetPriorityThread(PsGetCurrentThread(), prio as i32) };
    PRIO_OLD.store(old.max(0) as u32, Ordering::Relaxed);
}

unsafe extern "C" fn routine(_context: *mut c_void) {
    lower_priority();
    loop {
        // SAFETY: initialised event; no timeout: a request or stop wakes it.
        let _ = unsafe {
            KeWaitForSingleObject(REQ.0.get() as PVOID, 0, 0, 0, core::ptr::null_mut())
        };
        if STOPPING.load(Ordering::Acquire) != 0 {
            break;
        }
        let mask = WANTED.swap(0, Ordering::AcqRel) | BASE;
        let full = run_pass(mask);
        // Rest: requests meanwhile coalesce into the next pass; stop ends the rest at once.
        let rest_ms = if full {
            hpd_wake::MIRROR_REST_FULL_MS
        } else {
            hpd_wake::mirror_rest_ms(CHANGED.load(Ordering::Relaxed) != 0)
        };
        let mut rest: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        rest.QuadPart = -(rest_ms as i64 * 10_000);
        // SAFETY: initialised NotificationEvent with a relative timeout.
        let _ = unsafe { KeWaitForSingleObject(STOP.0.get() as PVOID, 0, 0, 0, &mut rest) };
        if STOPPING.load(Ordering::Acquire) != 0 {
            break;
        }
    }
    // SAFETY: the latch `stop` waits on; then the thread ends.
    unsafe { KeSetEvent(EXITED.0.get(), 0, 0) };
    let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
}

/// One pass: the steps `mask` names, then the base block and this module's own counters. Returns
/// whether it was a full refresh (every value written).
fn run_pass(mask: u32) -> bool {
    let t0 = crate::adapter::foreign_scanout::now_100ns();
    let now_ms = AdapterContext::interrupt_time_ms().max(1);
    let full = hpd_wake::mirror_full_due(now_ms, LAST_FULL_MS.load(Ordering::Relaxed));
    if full {
        // Every value again: one that was deleted or edited by hand comes back.
        crate::diag::mirror_forget_all();
        LAST_FULL_MS.store(now_ms, Ordering::Relaxed);
    }
    crate::diag::mirror_begin_pass(
        CHANGED.load(Ordering::Relaxed) != 0,
        YIELD.load(Ordering::Relaxed),
    );
    let a = ADAPTER.load(Ordering::Acquire);
    if a != 0 && STOPPING.load(Ordering::Acquire) == 0 {
        // SAFETY: `start` stored the context of the worker this thread belongs to, `stop` clears
        // it before it joins, and a thread it cannot join keeps the context allocated
        // (`hpd_worker_leaked`).
        let adapter = unsafe { &*(a as *const AdapterContext) };
        if mask & DUMP != 0 {
            let d0 = crate::adapter::foreign_scanout::now_100ns();
            crate::ddi::scanout_trace::dump(adapter);
            let us = (crate::adapter::foreign_scanout::now_100ns().saturating_sub(d0) / 10)
                .min(u32::MAX as u64) as u32;
            crate::ddi::stall_diag::note_dump(us);
            DUMPS.fetch_add(1, Ordering::Relaxed);
        }
        if mask & PACING != 0 && STOPPING.load(Ordering::Acquire) == 0 {
            adapter.pacing_publish();
            PACES.fetch_add(1, Ordering::Relaxed);
        }
    }
    if mask & NV != 0 && STOPPING.load(Ordering::Acquire) == 0 {
        crate::ddi::publish_nvrm_counters();
        NVS.fetch_add(1, Ordering::Relaxed);
    }
    crate::ddi::stall_diag::publish_counters();
    publish_own();
    crate::diag::mirror_end_pass();
    let us = (crate::adapter::foreign_scanout::now_100ns().saturating_sub(t0) / 10)
        .min(u32::MAX as u64) as u32;
    LAST_US.store(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);
    RUNS.fetch_add(1, Ordering::Relaxed);
    full
}

/// This module's own counters, written by the thread beside the block it published (inside the
/// pass, so unchanged ones are skipped like every other).
fn publish_own() {
    use crate::diag::record_named_bytes as rec;
    rec(b"MirReqs", REQS.load(Ordering::Relaxed));
    rec(b"MirRuns", RUNS.load(Ordering::Relaxed));
    rec(b"MirLastUs", LAST_US.load(Ordering::Relaxed));
    rec(b"MirMaxUs", MAX_US.load(Ordering::Relaxed));
    rec(b"MirDumps", DUMPS.load(Ordering::Relaxed));
    rec(b"MirNvs", NVS.load(Ordering::Relaxed));
    rec(b"MirPaces", PACES.load(Ordering::Relaxed));
    rec(b"MirYlds", YLDS.load(Ordering::Relaxed));
    rec(b"MirPrioOld", PRIO_OLD.load(Ordering::Relaxed));
    let (writes, skipped) = crate::diag::mirror_write_counts();
    rec(b"MirWrN", writes);
    rec(b"MirSkipN", skipped);
}
