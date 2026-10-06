//! The registry mirror's own thread: `stall_diag::publish_counters` (the stall-diagnosis block,
//! the `Vs*` heartbeat values, the device-lost block, the `FlipLat*` / `IfGap*` / `Fa*` blocks of
//! `flip_lat` / `flip_announce`: well over a hundred `RtlWriteRegistryValue` calls) used to run
//! ON THE HPD WORKER, between two flips, from `queue_active_scanout_refresh` (`HpdSite` 19) and
//! from the Nv* mirror. One pass is a few milliseconds to tens of them (T6, 332.1: `FlipMaxUs`
//! 29 ms at site 19 in both rows), a whole 240 Hz period or several, during which no flip is
//! programmed: the stalls of `IfStall8`, and the busy worker that made one flip a second decline
//! its announce (`FaNoBusy`).
//!
//! This thread takes those passes. The worker (or the dump, or the Nv* mirror) only REQUESTS one
//! ([`request`]: two atomic operations and a `KeSetEvent`, legal at any IRQL up to DISPATCH); the
//! thread publishes, then rests for [`MIN_INTERVAL_MS`] (requests that arrive meanwhile coalesce
//! into one pass), so the block is at most about a second stale and never more than one write pass
//! per second. Nothing here touches the adapter: the block is statics and the service key, which
//! is also why `publish_from_escape` can already run it from an escape thread.
//!
//! Lifetime: started with the HPD worker (`AdapterContext::init_hpd`), stopped and joined in
//! front of it (`stop_hpd`). Without the thread (creation failed, render-only) [`running`] is
//! false and the callers publish inline as before.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, KeWaitForSingleObject, PsTerminateSystemThread};
use wdk_sys::{KEVENT, PVOID};

use crate::dxgk::*;

/// Least time between two publish passes.
const MIN_INTERVAL_MS: i64 = 1000;

struct Ev(UnsafeCell<KEVENT>);
// SAFETY: a KEVENT is a dispatcher object, shared by design; it is initialised before the thread
// exists and only touched through the Ke* calls.
unsafe impl Sync for Ev {}

/// Wake: a request, or stop. SynchronizationEvent (auto-clearing).
static REQ: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));
/// Rest: set by stop so the rest between two passes ends at once. NotificationEvent.
static STOP: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));
/// Exited latch. NotificationEvent.
static EXITED: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));

static THREAD: AtomicUsize = AtomicUsize::new(0);
static STOPPING: AtomicU32 = AtomicU32::new(0);
static WANTED: AtomicU32 = AtomicU32::new(0);
/// Passes published by the thread (`MirRuns`), requests (`MirReqs`), the longest pass in
/// microseconds (`MirMaxUs`) and the last (`MirLastUs`).
static RUNS: AtomicU32 = AtomicU32::new(0);
static REQS: AtomicU32 = AtomicU32::new(0);
static MAX_US: AtomicU32 = AtomicU32::new(0);
static LAST_US: AtomicU32 = AtomicU32::new(0);

extern "C" {
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

/// Whether the thread exists: the callers then request instead of publishing inline.
#[inline]
pub(crate) fn running() -> bool {
    THREAD.load(Ordering::Acquire) != 0 && STOPPING.load(Ordering::Acquire) == 0
}

/// Ask for one publish pass. Atomics and `KeSetEvent(Wait = FALSE)`: any IRQL up to DISPATCH.
pub(crate) fn request() {
    REQS.fetch_add(1, Ordering::Relaxed);
    if WANTED.swap(1, Ordering::AcqRel) == 0 {
        // SAFETY: REQ was initialised by `start` before the thread (and so `running`) existed.
        unsafe { KeSetEvent(REQ.0.get(), 0, 0) };
    }
}

/// Start the thread. PASSIVE (StartDevice). Idempotent.
///
/// # Safety
/// PASSIVE_LEVEL.
pub(crate) unsafe fn start() {
    if THREAD.load(Ordering::Acquire) != 0 {
        return;
    }
    STOPPING.store(0, Ordering::Release);
    WANTED.store(0, Ordering::Release);
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
    }
}

/// Stop and join the thread. PASSIVE; blocks up to 5 s. Idempotent; called before the HPD worker
/// is stopped.
pub(crate) fn stop() {
    let h = THREAD.swap(0, Ordering::AcqRel);
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
    let _ = unsafe { KeWaitForSingleObject(EXITED.0.get() as PVOID, 0, 0, 0, &mut timeout) };
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
        let _ = unsafe { KeWaitForSingleObject(obj, 0, 0, 0, &mut timeout) };
        unsafe { wdk_sys::ntddk::ObfDereferenceObject(obj) };
    }
    // SAFETY: closing the handle we created.
    let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
}

unsafe extern "C" fn routine(_context: *mut c_void) {
    loop {
        // SAFETY: initialised event; no timeout: a request or stop wakes it.
        let _ = unsafe {
            KeWaitForSingleObject(REQ.0.get() as PVOID, 0, 0, 0, core::ptr::null_mut())
        };
        if STOPPING.load(Ordering::Acquire) != 0 {
            break;
        }
        WANTED.store(0, Ordering::Release);
        let t0 = crate::adapter::foreign_scanout::now_100ns();
        crate::ddi::stall_diag::publish_counters();
        publish_own();
        let us = (crate::adapter::foreign_scanout::now_100ns().saturating_sub(t0) / 10)
            .min(u32::MAX as u64) as u32;
        LAST_US.store(us, Ordering::Relaxed);
        MAX_US.fetch_max(us, Ordering::Relaxed);
        RUNS.fetch_add(1, Ordering::Relaxed);
        // Rest: requests meanwhile coalesce into the next pass; stop ends the rest at once.
        let mut rest: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        rest.QuadPart = -(MIN_INTERVAL_MS * 10_000);
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

/// This module's own counters, written by the thread beside the block it published.
fn publish_own() {
    use crate::diag::record_named_bytes as rec;
    rec(b"MirReqs", REQS.load(Ordering::Relaxed));
    rec(b"MirRuns", RUNS.load(Ordering::Relaxed));
    rec(b"MirLastUs", LAST_US.load(Ordering::Relaxed));
    rec(b"MirMaxUs", MAX_US.load(Ordering::Relaxed));
}
