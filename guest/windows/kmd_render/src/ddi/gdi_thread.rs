//! The GDI executor's own thread (`GdiAccel` = 1). The executor (`ddi/gdi_exec.rs`) used to run
//! on the HPD worker, which also programs flips and dispatches the Present copies; a CPU-path GDI
//! operation over a large window took 105 ms there (359.1, `GdiUsMax`). Here a slow job delays
//! only GDI.
//!
//! Started lazily by the first translated GDI command buffer (PASSIVE, `gdi_accel::translate`), so
//! with the knob off no thread exists. Stopped and joined in `AdapterContext::stop_hpd`, in front
//! of the HPD worker; a thread that could not be joined latches `hpd_worker_leaked` (the adapter it
//! reads stays allocated) and keeps this module from ever starting again ([`leaked`]). Without the
//! thread (creation failed, leaked) the HPD worker runs the executor as before.
//!
//! The same lifecycle as `ddi/mirror_thread.rs`.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, KeWaitForSingleObject, PsTerminateSystemThread};
use wdk_sys::{KEVENT, PVOID};

use crate::adapter::AdapterContext;
use crate::dxgk::*;

struct Ev(UnsafeCell<KEVENT>);
// SAFETY: a KEVENT is a dispatcher object, shared by design; initialised before the thread exists
// and only touched through the Ke* calls.
unsafe impl Sync for Ev {}

/// Work or stop. SynchronizationEvent.
static REQ: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));
/// Exited latch. NotificationEvent.
static EXITED: Ev = Ev(UnsafeCell::new(unsafe { core::mem::zeroed() }));

static THREAD: AtomicUsize = AtomicUsize::new(0);
static ADAPTER: AtomicUsize = AtomicUsize::new(0);
static STOPPING: AtomicU32 = AtomicU32::new(0);
static LEAKED: AtomicU32 = AtomicU32::new(0);
/// Serialises `ensure_started` callers (0 idle, 1 starting).
static STARTING: AtomicU32 = AtomicU32::new(0);

extern "C" {
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

/// The thread exists and is not stopping: the HPD worker leaves the executor to it.
#[inline]
pub(crate) fn running() -> bool {
    THREAD.load(Ordering::Acquire) != 0 && STOPPING.load(Ordering::Acquire) == 0
}

/// A stop was asked: the executor abandons the job in hand (StopDevice discharges it).
#[inline]
pub(crate) fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire) != 0
}

#[inline]
pub(crate) fn leaked() -> bool {
    LEAKED.load(Ordering::Acquire) != 0
}

/// Wake the thread. Any IRQL up to DISPATCH.
pub(crate) fn kick() {
    if THREAD.load(Ordering::Acquire) != 0 {
        // SAFETY: REQ was initialised before THREAD was published.
        unsafe { KeSetEvent(REQ.0.get(), 0, 0) };
    }
}

/// Start the thread once (PASSIVE). Idempotent; refuses after a leak or during a stop.
///
/// # Safety
/// PASSIVE_LEVEL. `adapter` outlives the thread: [`stop`] (from `stop_hpd`) joins it first.
pub(crate) unsafe fn ensure_started(adapter: &AdapterContext) {
    if THREAD.load(Ordering::Acquire) != 0 || LEAKED.load(Ordering::Acquire) != 0 || !adapter.hpd_running() {
        return;
    }
    if STARTING.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    if THREAD.load(Ordering::Acquire) == 0 {
        STOPPING.store(0, Ordering::Release);
        ADAPTER.store(adapter as *const AdapterContext as usize, Ordering::Release);
        // SAFETY: no thread uses the events now (none exists); PASSIVE.
        unsafe {
            KeInitializeEvent(REQ.0.get(), 1, 0);
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
            // Work queued before the thread existed went to the HPD worker; anything left is
            // picked up by the first pass.
            kick();
        } else {
            ADAPTER.store(0, Ordering::Release);
        }
    }
    STARTING.store(0, Ordering::Release);
}

/// Stop and join (PASSIVE; blocks up to 5 s). Idempotent.
pub(crate) fn stop() {
    let h = THREAD.swap(0, Ordering::AcqRel);
    if h == 0 {
        ADAPTER.store(0, Ordering::Release);
        return;
    }
    STOPPING.store(1, Ordering::Release);
    // SAFETY: initialised event; PASSIVE.
    unsafe { KeSetEvent(REQ.0.get(), 0, 0) };
    const JOIN_TIMEOUT_100NS: i64 = -50_000_000;
    let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
    timeout.QuadPart = JOIN_TIMEOUT_100NS;
    // SAFETY: the exited latch is a NotificationEvent set by the thread just before it ends.
    let exited = unsafe { KeWaitForSingleObject(EXITED.0.get() as PVOID, 0, 0, 0, &mut timeout) };
    let mut joined = exited == STATUS_SUCCESS;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let mut obj: PVOID = core::ptr::null_mut();
    // SAFETY: `h` is the live handle `ensure_started` created; PsThreadType validates it.
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
        LEAKED.store(1, Ordering::Release);
    } else {
        ADAPTER.store(0, Ordering::Release);
    }
    // SAFETY: closing the handle we created.
    let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
}

/// The executor's idle wait: on its expiry the last burst's counters are mirrored.
const IDLE_PUBLISH_100NS: i64 = 300 * 10_000;
/// `STATUS_TIMEOUT` of a timed `KeWaitForSingleObject`.
const STATUS_WAIT_TIMEOUT: i32 = 0x0000_0102;

unsafe extern "C" fn routine(_context: *mut c_void) {
    // SAFETY: a system thread runs at PASSIVE_LEVEL.
    let passive = unsafe { crate::irql::PassiveLevel::assume() };
    // Bring the copy-engine channel up now, at the first GDI buffer of the session, rather than
    // inside the first job that needs it (`GdiChUpUs`, ~13 ms once per start).
    let a = ADAPTER.load(Ordering::Acquire);
    if a != 0 && STOPPING.load(Ordering::Acquire) == 0 {
        // SAFETY: as in the loop below.
        crate::ddi::gdi_exec::warm_up(passive, unsafe { &*(a as *const AdapterContext) });
    }
    loop {
        // SAFETY: initialised event; a kick or a stop wakes it.
        // A bounded wait: an idle period mirrors the counters of the last burst
        // (`gdi_exec::publish_if_due`, throttled while jobs run).
        let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = -IDLE_PUBLISH_100NS;
        let st = unsafe { KeWaitForSingleObject(REQ.0.get() as PVOID, 0, 0, 0, &mut timeout) };
        if STOPPING.load(Ordering::Acquire) != 0 {
            break;
        }
        if st == STATUS_WAIT_TIMEOUT {
            crate::ddi::gdi_exec::publish_if_due(true);
            continue;
        }
        let a = ADAPTER.load(Ordering::Acquire);
        if a == 0 {
            continue;
        }
        // SAFETY: `stop` joins this thread before the adapter can be freed (or latches the leak,
        // which keeps it allocated).
        let adapter = unsafe { &*(a as *const AdapterContext) };
        while crate::ddi::gdi_exec::service(passive, adapter, true) {
            if STOPPING.load(Ordering::Acquire) != 0 {
                break;
            }
        }
    }
    // SAFETY: the latch `stop` waits on; then the thread ends.
    unsafe { KeSetEvent(EXITED.0.get(), 0, 0) };
    let _ = unsafe { PsTerminateSystemThread(STATUS_SUCCESS) };
}
