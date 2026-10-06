//! The five embedded kernel dispatcher objects and their lifecycles: the venus
//! and scanout mutexes, the HPD worker's wake/exit events and thread, and the
//! VSync heartbeat timer/DPC pair.
//!
//! Moved verbatim out of `adapter.rs` by T8/R1101 and, for the DPC itself, out
//! of `ddi/start_device.rs` by T8/R1102. Co-locating `start_vsync`,
//! `quiesce_vsync`/`stop_vsync` and `vsync_dpc_routine` is the point: the
//! start/resume paths arm the already-initialized KTIMER and terminal stop owns
//! the free-after-DPC contract
//! (`KeCancelTimer` then `KeFlushQueuedDpcs` before RemoveDevice frees the
//! context), so the cancel/flush/free argument is now stated once instead of
//! being split across two modules.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64};

use wdk_sys::ntddk::{KeInitializeEvent, KeSetEvent, KeWaitForSingleObject};
use wdk_sys::PVOID;

use crate::dxgk::*;

use super::AdapterContext;

/// One event per epoch, not one per timer tick. This is a diagnostic cursor
/// only; the VSync protocol remains untouched.
static LAST_TIMELINE_VSYNC_EPOCH: AtomicU64 = AtomicU64::new(0);

/// `VsyncRateMhz` knob, read at StartDevice: 0 = the vsync heartbeat follows the
/// mode's refresh rate, nonzero forces its rate (millihertz).
pub static VSYNC_RATE_OVERRIDE_MHZ: AtomicU32 = AtomicU32::new(0);

/// The retrace rate the vsync heartbeat runs at, in millihertz.
fn vsync_rate_mhz(adapter: &AdapterContext) -> u32 {
    match VSYNC_RATE_OVERRIDE_MHZ.load(core::sync::atomic::Ordering::Relaxed) {
        0 => adapter.effective_refresh_mhz(),
        forced => forced,
    }
}

/// Opaque `PEX_TIMER` storage. `wdk-sys` 0.5.1 does not bind the Windows 8.1
/// ExXxx timer API yet, so retain only the pointer representation the WDK
/// declares (`struct _EX_TIMER *`) and keep the ABI declarations here.
type ExTimer = *mut c_void;
type ExTimerCallback = unsafe extern "system" fn(timer: ExTimer, context: PVOID);

/// `EX_TIMER_HIGH_RESOLUTION` from wdm.h. Do not combine it with
/// `EX_TIMER_NO_WAKE`: the WDK documents those attributes as mutually
/// exclusive.
const EX_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0004;
const BOOLEAN_TRUE: u8 = 1;

#[link(name = "ntoskrnl")]
extern "system" {
    /// Windows 8.1+: allocate the one system timer object used for this
    /// adapter's entire PnP lifetime. Called only from AddDevice/PASSIVE.
    fn ExAllocateTimer(
        callback: Option<ExTimerCallback>,
        callback_context: PVOID,
        attributes: u32,
    ) -> ExTimer;
    /// Cancel a pending Ex timer. `Parameters` is documented as NULL.
    fn ExCancelTimer(timer: ExTimer, parameters: PVOID) -> u8;
    /// Delete a system-allocated timer. The final RemoveDevice call passes
    /// `Cancel=TRUE, Wait=TRUE`, which is legal at PASSIVE_LEVEL and proves no
    /// callback still holds the adapter context before it is freed.
    fn ExDeleteTimer(timer: ExTimer, cancel: u8, wait: u8, parameters: PVOID) -> u8;
    /// Set a one-shot Ex timer. High-resolution timers require a negative,
    /// relative DueTime; `relative_due` below provides that invariant.
    fn ExSetTimer(timer: ExTimer, due_time: i64, period: i64, parameters: PVOID) -> u8;
}

extern "C" {
    /// `extern POBJECT_TYPE *PsThreadType;` (ntddk.h) — the thread object type, for
    /// `ObReferenceObjectByHandle` validation when joining the HPD worker. A data
    /// export (ntoskrnl.lib), not in the wdk-sys function bindings, so declared here
    /// (same pattern as `ExEventObjectType` in `ddi/escape.rs`).
    static PsThreadType: *mut wdk_sys::POBJECT_TYPE;
}

impl AdapterContext {
    /// Idempotent-ish: does nothing if a thread handle is already stored.
    ///
    /// # Safety
    /// `self` must be at its final heap address and `dxgkrnl` already saved.
    pub unsafe fn init_hpd(&self) {
        use core::sync::atomic::Ordering;
        if self.hpd_thread.load(Ordering::Acquire) != 0 {
            return;
        }
        self.hpd_stop.store(0, Ordering::Release);
        self.scanout_refresh_pending.store(0, Ordering::Release);
        self.scanout_flush_inflight.store(0, Ordering::Release);
        // ⚠ `host_bound_scanout_resource` is zeroed here WITHOUT clearing
        // `active_scanout_resource`, so after a StopDevice/StartDevice cycle
        // the two disagree and `queue_active_scanout_refresh_locked` reaches
        // the `host_bound != resource_id` test below. That is the ONE path
        // that made the deleted async-bind arm reachable, and it is why the
        // refusal survives the deletion as `RfUnb` rather than falling through
        // to a RESOURCE_FLUSH against a resource the host never bound.
        self.host_bound_scanout_resource.store(0, Ordering::Release);
        let mut handle: wdk_sys::HANDLE = core::ptr::null_mut();
        const THREAD_ALL_ACCESS: u32 = 0x001F_FFFF;
        // SAFETY: PASSIVE_LEVEL; a kernel system thread in the system process
        // running `hpd_thread_routine` with this stable context as its argument.
        let st = unsafe {
            wdk_sys::ntddk::PsCreateSystemThread(
                &mut handle,
                THREAD_ALL_ACCESS,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                Some(crate::ddi::hpd::hpd_thread_routine),
                self as *const _ as PVOID,
            )
        };
        if st == STATUS_SUCCESS && !handle.is_null() {
            self.hpd_thread.store(handle as usize, Ordering::Release);
        } else {
            // 0x0B00_00EA = HPD-worker-create-failed. It was 0x0B00_00E7, which
            // ddi/lifecycle.rs also records for venus-bring-up-failed — and BOTH
            // happen inside the StartDevice window, so the ring could not
            // disambiguate them. `StHpd` vs `StVnu` already distinguish the two
            // as named counters; this makes the ring agree.
            crate::diag::record(0x0B00_00EA);
            crate::diag::fault(crate::diag::FaultCounter::StHpd, st as u32);
        }
    }

    /// Whether the HPD worker thread exists. It is absent in the render-only
    /// configuration (`DisplayHalf=0`) and when its creation failed.
    pub fn hpd_running(&self) -> bool {
        self.hpd_thread.load(core::sync::atomic::Ordering::Acquire) != 0
    }

    /// Wake the HPD worker to re-indicate connection (from the interrupt DPC at
    /// DISPATCH_LEVEL — KeSetEvent with Wait=FALSE is legal there).
    pub fn signal_hpd(&self) {
        self.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
    }

    /// [`Self::signal_hpd`] naming who asks (`hpd_wake::cause`): counted per cause (`HpdSg*`) and
    /// recorded in the mask the worker takes at its next wake (`HpdWkSrc`). Atomics and
    /// `KeSetEvent(Wait = FALSE)`: any IRQL up to DISPATCH.
    pub fn signal_hpd_for(&self, cause: u32) {
        crate::ddi::stall_diag::note_signal(cause);
        // SAFETY: hpd_event was initialized in place by init_kernel_events.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
    }

    /// Stop + join the HPD worker before teardown (StopDevice / Drop). Idempotent.
    /// PASSIVE_LEVEL — it blocks on the worker's exit.
    pub fn stop_hpd(&self) {
        use core::sync::atomic::Ordering;
        let h = self.hpd_thread.swap(0, Ordering::AcqRel);
        if h == 0 {
            return;
        }
        self.hpd_stop.store(1, Ordering::Release);
        // SAFETY: initialized event; wake the worker so it observes hpd_stop.
        unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
        // Join: reference the thread object from its handle, wait for it to exit,
        // deref, then close the handle. Without the join, RemoveDevice could free
        // this context while the worker still runs → UAF.
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const KERNEL_MODE: i8 = 0;
        let mut obj: PVOID = core::ptr::null_mut();
        // SAFETY: `h` is a live thread handle from PsCreateSystemThread; PsThreadType
        // validates it. On success we hold a reference to the ETHREAD.
        let st = unsafe {
            wdk_sys::ntddk::ObReferenceObjectByHandle(
                h as wdk_sys::HANDLE,
                SYNCHRONIZE,
                *PsThreadType,
                KERNEL_MODE,
                &mut obj,
                core::ptr::null_mut(),
            )
        };
        // The join is the ONLY thing keeping RemoveDevice from freeing a context
        // the worker still dereferences. Two ways it used to fail silently:
        //
        // 1. If ObReferenceObjectByHandle failed, the wait was skipped entirely,
        //    the handle was closed, and stop_hpd returned () - success-shaped.
        //    StopDevice then dropped the transport and RemoveDevice freed the
        //    box while the worker was still touching adapter.hpd_event and the
        //    scanout fields. A use-after-free with no breadcrumb.
        // 2. The wait passed a NULL Timeout - the only unbounded wait in this
        //    file - while the worker can be parked in a synchronous host
        //    round-trip (set_scanout_blob) or under the venus mutex. A wedged
        //    host hung PnP stop forever, again with no counter.
        //
        // Both now mean "a worker may still be running", which is recorded and
        // latched so the free path can consult it.
        // 5 s, relative (negative = relative to now, in 100 ns units). Long
        // enough for the worst observed set_scanout_blob round-trip, short
        // enough that PnP stop does not hang indefinitely.
        const JOIN_TIMEOUT_100NS: i64 = -50_000_000;

        // Primary join: the worker's own "I exited" NotificationEvent, set at
        // both of its exit sites immediately before PsTerminateSystemThread.
        // This does NOT depend on the handle-to-object lookup below succeeding,
        // which is the failure that used to skip the wait entirely. A
        // NotificationEvent stays signalled, so the join cannot miss it.
        let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
        timeout.QuadPart = JOIN_TIMEOUT_100NS;
        // SAFETY: initialized in place by init_kernel_events; PASSIVE_LEVEL.
        let exited =
            unsafe { KeWaitForSingleObject(self.hpd_exited.get() as PVOID, 0, 0, 0, &mut timeout) };
        let mut joined = exited == STATUS_SUCCESS;

        if st == STATUS_SUCCESS && !obj.is_null() {
            // Secondary: the thread object itself. The exit event is set just
            // BEFORE PsTerminateSystemThread, so this closes the remaining
            // window between the two - the worker is not yet fully torn down
            // when it signals.
            let mut timeout: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
            timeout.QuadPart = JOIN_TIMEOUT_100NS;
            // SAFETY: waiting on the ETHREAD dispatcher object at PASSIVE_LEVEL.
            let wait = unsafe { KeWaitForSingleObject(obj, 0, 0, 0, &mut timeout) };
            // SAFETY: releasing the reference taken above.
            unsafe { wdk_sys::ntddk::ObfDereferenceObject(obj) };
            joined = wait == STATUS_SUCCESS;
        }
        if !joined {
            // Trading a hang (or a UAF) for a permanent allocation leak plus a
            // live worker. That is the correct trade for a kernel driver, but it
            // must be counted: on a healthy host StHpdX never moves.
            self.hpd_worker_leaked.store(1, Ordering::Release);
            crate::diag::fault(crate::diag::FaultCounter::StHpdX, st as u32);
        }
        // SAFETY: closing the thread handle we created.
        let _ = unsafe { wdk_sys::ntddk::ZwClose(h as wdk_sys::HANDLE) };
    }

    /// A worker thread handle is registered (`init_hpd` ran and `stop_hpd` has not taken it):
    /// the StartDevice-entry breadcrumb `EntHpdTh`.
    pub(crate) fn hpd_worker_registered(&self) -> bool {
        self.hpd_thread.load(core::sync::atomic::Ordering::Relaxed) != 0
    }

    /// True if [`Self::stop_hpd`] could not prove the worker exited, so this
    /// context must never be freed. Consulted by `dxgkddi_remove_device`.
    pub fn hpd_worker_may_be_running(&self) -> bool {
        self.hpd_worker_leaked
            .load(core::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Interrupt time in whole milliseconds, wrapping at 2^32 (49.7 days): the
    /// time base of every `*VsT` / `VpDmpT` diagnostic value. Any IRQL.
    pub(crate) fn interrupt_time_ms() -> u32 {
        let mut qpc_timestamp = 0;
        // SAFETY: a scalar clock read; `qpc_timestamp` is a live local.
        let now = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
        helios_kmd_logic::vsync_rate::ms_from_100ns(now)
    }

    /// Interrupt time of the tick that last advanced `vsync_count`, in
    /// milliseconds (0 before the first), the companion of that count.
    pub(crate) fn vsync_last_ms(&self) -> u32 {
        helios_kmd_logic::vsync_rate::ms_from_100ns(
            self.vsync_last_100ns
                .load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// `VsMinGap` as published: smallest tick gap in 100 ns units, saturated,
    /// `u32::MAX` while none has been measured.
    pub(crate) fn vsync_min_gap_published(&self) -> u32 {
        helios_kmd_logic::vsync_rate::publish_gap(
            self.vsync_min_gap_100ns
                .load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// True once AddDevice successfully allocated the system high-resolution
    /// timer. The choice is immutable for this adapter lifetime: a failed
    /// allocation deliberately preserves the proven embedded-KTIMER fallback.
    #[inline]
    fn vsync_uses_ex_timer(&self) -> bool {
        self.vsync_ex_timer
            .load(core::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Arm the selected one-shot source. The caller supplies the relative,
    /// negative 100-ns interval produced by `vsync_deadline::relative_due`.
    /// It is callable at PASSIVE_LEVEL from lifecycle start/resume and at
    /// DISPATCH_LEVEL from either timer callback; neither path allocates,
    /// sleeps, polls, nor waits.
    unsafe fn set_vsync_one_shot(&self, due_time: i64) {
        debug_assert!(due_time < 0);
        let ex_timer = self
            .vsync_ex_timer
            .load(core::sync::atomic::Ordering::Acquire);
        if ex_timer != 0 {
            // SAFETY: nonzero only after successful ExAllocateTimer in
            // init_kernel_events. Drop deletes it only after stop_vsync has
            // cancelled it and ExDeleteTimer(…, wait=TRUE) drains callbacks.
            unsafe {
                ExSetTimer(ex_timer as ExTimer, due_time, 0, core::ptr::null_mut());
            }
        } else {
            let mut due: wdk_sys::LARGE_INTEGER = unsafe { core::mem::zeroed() };
            due.QuadPart = due_time;
            // SAFETY: the embedded KTIMER/KDPC were initialized in place before
            // publication; a zero period preserves one-shot fixed-phase timing.
            unsafe {
                wdk_sys::ntddk::KeSetTimerEx(self.vsync_timer.get(), due, 0, self.vsync_dpc.get())
            };
        }
    }

    /// Cancel the selected one-shot source. A system timer can have an expiry
    /// callback in flight after cancellation; it remains safe because the
    /// callback gates itself on `vsync_armed`, and final removal calls
    /// ExDeleteTimer(cancel=TRUE, wait=TRUE) before freeing this context.
    unsafe fn cancel_vsync_one_shot(&self) {
        crate::ddi::stall_diag::note_vsync_cancel();
        let ex_timer = self
            .vsync_ex_timer
            .load(core::sync::atomic::Ordering::Acquire);
        if ex_timer != 0 {
            // SAFETY: valid system timer for the adapter lifetime. WDK requires
            // NULL cancel parameters.
            unsafe { ExCancelTimer(ex_timer as ExTimer, core::ptr::null_mut()) };
        } else {
            // SAFETY: initialized embedded fallback timer.
            unsafe { wdk_sys::ntddk::KeCancelTimer(self.vsync_timer.get()) };
        }
    }

    /// Final RemoveDevice-only owner action for the optional Ex timer. This is
    /// intentionally not part of StopDevice/D3 quiesce: those lifecycles retain
    /// the same adapter and resume the same timer source. `wait=TRUE` is the
    /// no-UAF proof for the callback's immutable adapter context.
    pub(crate) fn delete_vsync_ex_timer(&mut self) {
        use core::sync::atomic::Ordering;
        // The pointers stay PUBLISHED until `ExDeleteTimer(wait)` has returned: a callback that
        // passed its checks before `stop_vsync` and is still running loaded a nonzero pointer
        // and must find the timer valid (the delete waits for it); one that starts later cannot,
        // the delete cancelled the timer first. Storing 0 BEFORE the delete let such a callback
        // read 0 and call `ExSetTimer(NULL)` (the watchdog), or fall to the KTIMER fallback and
        // arm a DPC inside a context about to be freed (the heartbeat).
        let wd_timer = self.vsync_wd_timer.load(Ordering::Acquire);
        let ex_timer = self.vsync_ex_timer.load(Ordering::Acquire);
        if wd_timer == 0 && ex_timer == 0 {
            return;
        }
        // A callback blocked in `DxgkCbSynchronizeExecution` hangs the waits below: `StopSub`
        // names this step (`VsCbIn` above `VsCbOut` says why).
        crate::ddi::stall_diag::stop_sub(helios_kmd_logic::stall_diag::stop_sub::REMOVE_TIMER);
        if wd_timer != 0 {
            // SAFETY: as below; `stop_vsync` already cleared `vsync_wd_on`, so a callback does
            // not re-arm, and cancel+wait drains one in flight.
            unsafe {
                ExDeleteTimer(
                    wd_timer as ExTimer,
                    BOOLEAN_TRUE,
                    BOOLEAN_TRUE,
                    core::ptr::null_mut(),
                );
            }
            self.vsync_wd_timer.store(0, Ordering::Release);
        }
        if ex_timer != 0 {
            // SAFETY: Drop runs only from RemoveDevice at PASSIVE_LEVEL, after
            // stop_vsync closed the delivery gate and cancelled the timer.
            // ExDeleteTimer with cancel+wait drains any in-flight callback before
            // the adapter storage containing its context pointer is released.
            unsafe {
                ExDeleteTimer(
                    ex_timer as ExTimer,
                    BOOLEAN_TRUE,
                    BOOLEAN_TRUE,
                    core::ptr::null_mut(),
                );
            }
            self.vsync_ex_timer.store(0, Ordering::Release);
        }
    }

    /// Start a new display lifecycle's VSync heartbeat. This is the sole path
    /// that opens the CRTC-VSYNC delivery gate; a D0 resume deliberately uses
    /// [`Self::resume_vsync`] instead so it preserves ControlInterrupt's gate.
    ///
    /// # Safety
    /// `self` must be at its final heap address (dxgkrnl holds it as the miniport
    /// device context) and `dxgkrnl` must already be saved (StartDevice ordering).
    pub unsafe fn start_vsync(&self) {
        self.vsync_enabled
            .store(1, core::sync::atomic::Ordering::Release);
        unsafe { self.arm_vsync() };
        // The independent watchdog starts after the chain it watches, with the heartbeat's
        // reference already set by the arm.
        unsafe { self.start_vsync_wd() };
    }

    /// Re-arm after a transient D3 quiesce. Keeps the delivery decision most
    /// recently made by `DxgkDdiControlInterrupt`.
    ///
    /// # Safety
    /// Same stable-address and published-callback preconditions as
    /// [`Self::start_vsync`]. PASSIVE_LEVEL only.
    pub unsafe fn resume_vsync(&self) {
        unsafe { self.arm_vsync() };
        // The watchdog timer stopped with the quiesce (`quiesce_vsync`): no new activity in a
        // power-down window, and it comes back with the heartbeat it watches.
        unsafe { self.start_vsync_wd() };
    }

    /// Arm the already-initialized one-shot timer. The final arm check closes
    /// a StopDevice/D3 race: cancellation may win while this PASSIVE path is
    /// calculating a deadline, but it can never touch an uninitialized object.
    unsafe fn arm_vsync(&self) {
        use wdk_sys::ntddk::KeQueryInterruptTimePrecise;
        if self
            .vsync_armed
            .swap(1, core::sync::atomic::Ordering::AcqRel)
            != 0
        {
            return;
        }
        // The first tick after an arm has no predecessor: do not measure its gap
        // against a tick from before a quiesce/resume.
        self.vsync_gap_prev_100ns
            .store(0, core::sync::atomic::Ordering::Relaxed);
        // SAFETY: KTIMER/KDPC were initialized at this stable address by
        // `init_kernel_events`, before the adapter became visible to dxgkrnl.
        unsafe {
            // VidPn advertises the host's refresh rate (60 Hz without one). Arm
            // the first exact-phase one-shot in 100 ns units; the DPC advances
            // the stored deadline from this anchor instead of accumulating its
            // own dispatch latency.
            let mut qpc_timestamp = 0;
            let now = KeQueryInterruptTimePrecise(&mut qpc_timestamp);
            // `VsArmN`, and the watchdog's reference (`stall_diag::vsync_reference`).
            crate::ddi::stall_diag::note_vsync_armed(now);
            let period = helios_kmd_logic::vsync_deadline::period_100ns(vsync_rate_mhz(self));
            let Some(deadline) = helios_kmd_logic::vsync_deadline::next(now, now, period) else {
                self.vsync_armed
                    .store(0, core::sync::atomic::Ordering::Release);
                self.vsync_deadline_100ns
                    .store(0, core::sync::atomic::Ordering::Release);
                return;
            };
            self.vsync_deadline_100ns
                .store(deadline, core::sync::atomic::Ordering::Release);
            let due = helios_kmd_logic::vsync_deadline::relative_due(deadline, now);
            self.set_vsync_one_shot(due);
            if self.vsync_armed.load(core::sync::atomic::Ordering::Acquire) == 0 {
                self.cancel_vsync_one_shot();
                self.vsync_deadline_100ns
                    .store(0, core::sync::atomic::Ordering::Release);
            }
        }
    }

    /// The heartbeat's watchdog (`helios_kmd_logic::hpd_wake::vsync_watch`), the T5 anomaly 2 fix:
    /// a heartbeat that is meant to run but has not ticked for 250 ms (16 periods at 60 Hz) is
    /// re-armed, and one that was quiesced although the adapter is in D0 and dxgkrnl has the
    /// delivery gate open is armed again (`may_arm`: PASSIVE callers only, like `arm_vsync`).
    /// A flip is retired only by a CRTC_VSYNC carrying its address, so a dead heartbeat strands
    /// the desktop's flips (DWM presented 5 flips and then waited, `FlipIss` 5). Counted:
    /// `VsRevN`, `VsArmN`. Called from the HPD worker's every pass and from the escape thread;
    /// healthy cost is a few loads and one clock read. Legal at any IRQL up to DISPATCH when
    /// `may_arm` is false (`ExSetTimer` is).
    pub(crate) fn vsync_watch(&self, may_arm: bool) {
        use core::sync::atomic::Ordering;
        use helios_kmd_logic::hpd_wake::VsyncWatch;
        use wdk_sys::ntddk::KeQueryInterruptTimePrecise;
        // v327: `VsWatchdog` 0 (the default) is KMD 325: no watchdog, nothing re-arms a heartbeat
        // but StartDevice and a D0 power call.
        let level = crate::ddi::stall_diag::vs_watchdog();
        if level == 0 || !self.display_half() {
            return;
        }
        let armed = self.vsync_armed.load(Ordering::Acquire) != 0;
        let reference = crate::ddi::stall_diag::vsync_reference();
        let mut qpc_timestamp = 0;
        // SAFETY: a scalar clock read; `qpc_timestamp` is a live local.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
        let period = helios_kmd_logic::vsync_deadline::period_100ns(vsync_rate_mhz(self));
        match helios_kmd_logic::hpd_wake::vsync_watch_level(
            level,
            armed,
            true,
            crate::ddi::stall_diag::adapter_d0(),
            self.vsync_enabled.load(Ordering::Acquire) != 0,
            now,
            reference,
            period,
        ) {
            VsyncWatch::Ok => {}
            VsyncWatch::Revive => {
                self.revive_heartbeat(reference, now, period);
            }
            VsyncWatch::Resume => {
                if may_arm {
                    // SAFETY: PASSIVE_LEVEL caller (`may_arm`), final heap address, dxgkrnl saved.
                    unsafe { self.arm_vsync() };
                }
            }
        }
    }

    /// Set the one-shot again for a heartbeat found silent (the worker-side watchdog and the
    /// independent watchdog timer). `reference` is the silence reference the caller read: one of
    /// several racing callers wins and re-bases it, the others do nothing. `true` for the winner
    /// that set the one-shot. Legal at any IRQL up to DISPATCH (`ExSetTimer` is).
    pub(crate) fn revive_heartbeat(&self, reference: u64, now: u64, period: u64) -> bool {
        use core::sync::atomic::Ordering;
        // A heartbeat that is no longer armed (a quiesce won the race with the caller's read) is
        // not revived: neither `VsRevN` nor the `VsCanN` of the cancel below is bumped for it.
        if self.vsync_armed.load(Ordering::Acquire) == 0 {
            return false;
        }
        if !crate::ddi::stall_diag::note_vsync_revived(reference, now) {
            return false;
        }
        let Some(deadline) = helios_kmd_logic::vsync_deadline::next(now, now, period) else {
            return false;
        };
        self.vsync_gap_prev_100ns.store(0, Ordering::Relaxed);
        self.vsync_deadline_100ns.store(deadline, Ordering::Release);
        let due = helios_kmd_logic::vsync_deadline::relative_due(deadline, now);
        // SAFETY: the one-shot was initialized at AddDevice; setting it is legal at
        // DISPATCH_LEVEL and below, and replaces a pending expiry.
        unsafe { self.set_vsync_one_shot(due) };
        if self.vsync_armed.load(Ordering::Acquire) == 0 {
            // A quiesce raced the revive: it wins, as in `arm_vsync`.
            // SAFETY: as above.
            unsafe { self.cancel_vsync_one_shot() };
        }
        true
    }

    /// Arm the independent watchdog timer (v329): a 250 ms one-shot chain of its own, started at
    /// StartDevice and cancelled at StopDevice, that does not depend on the heartbeat chain, the
    /// worker or an escape. A no-op when `VsWdTimer` is 0 or no timer could be allocated.
    unsafe fn start_vsync_wd(&self) {
        use core::sync::atomic::Ordering;
        let timer = self.vsync_wd_timer.load(Ordering::Acquire);
        if timer == 0 || !crate::ddi::stall_diag::vs_wd_timer() {
            return;
        }
        if self.vsync_wd_on.swap(1, Ordering::AcqRel) != 0 {
            return;
        }
        crate::ddi::stall_diag::note_wd_on(true);
        // SAFETY: allocated at AddDevice; one-shot, relative due time.
        unsafe {
            ExSetTimer(
                timer as ExTimer,
                helios_kmd_logic::vsync_wd::WD_PERIOD_100NS,
                0,
                core::ptr::null_mut(),
            );
        }
        if self.vsync_wd_on.load(Ordering::Acquire) == 0 {
            // SAFETY: as above; a StopDevice raced this start and wins.
            unsafe { ExCancelTimer(timer as ExTimer, core::ptr::null_mut()) };
        }
    }

    /// Stop the watchdog timer (StopDevice / RemoveDevice, PASSIVE). An expiry already running
    /// re-checks `vsync_wd_on` after it re-arms, so it ends cancelled; `ExDeleteTimer(wait)` at
    /// RemoveDevice drains it before the context is freed.
    fn stop_vsync_wd(&self) {
        use core::sync::atomic::Ordering;
        self.vsync_wd_on.store(0, Ordering::Release);
        crate::ddi::stall_diag::note_wd_on(false);
        let timer = self.vsync_wd_timer.load(Ordering::Acquire);
        if timer != 0 {
            // SAFETY: valid system timer for the adapter lifetime; NULL parameters per the WDK.
            unsafe { ExCancelTimer(timer as ExTimer, core::ptr::null_mut()) };
        }
    }

    /// One tick of the independent watchdog timer (DISPATCH, atomics and `ExSetTimer` only). It
    /// re-arms ITSELF FIRST, then looks at the heartbeat: armed, in D0 and silent for at least
    /// max(250 ms, 16 periods) is re-armed (`VsRevN`, `VsWdFixN`); a silent heartbeat with a tick
    /// callback entered and not returned is counted (`VsWdHungN`: a blocked callback cannot be
    /// helped by a re-arm). What it saw when it acted is kept (`VsWdS*`), and every
    /// `PUBLISH_EVERY_TICKS` ticks, or at once after acting, the HPD worker is woken to write
    /// the heartbeat block (`VsLiveT`), because the worker is otherwise asleep with an infinite
    /// wait and the registry mirror stays at its last pass.
    fn vsync_wd_tick(&self) {
        use core::sync::atomic::Ordering;
        use helios_kmd_logic::vsync_wd::{self, WdAction};
        use wdk_sys::ntddk::KeQueryInterruptTimePrecise;
        // Loaded ONCE, before anything else: `delete_vsync_ex_timer` publishes 0 only after
        // `ExDeleteTimer(wait)` has returned, which cannot happen while this callback runs, so a
        // nonzero value stays valid for the whole call and 0 means there is nothing to do.
        let timer = self.vsync_wd_timer.load(Ordering::Acquire);
        if timer == 0 || self.vsync_wd_on.load(Ordering::Acquire) == 0 {
            return;
        }
        // SAFETY: `timer` is the live watchdog timer (see above), one-shot, relative due time.
        unsafe {
            ExSetTimer(
                timer as ExTimer,
                vsync_wd::WD_PERIOD_100NS,
                0,
                core::ptr::null_mut(),
            );
        }
        if self.vsync_wd_on.load(Ordering::Acquire) == 0 {
            // SAFETY: as above; StopDevice raced the re-arm and wins.
            unsafe { ExCancelTimer(timer as ExTimer, core::ptr::null_mut()) };
            return;
        }
        // Outside D0 the watchdog does nothing at all (no decision, no counter, no worker wake):
        // there is no new activity in a power-down or shutdown window. It stays armed so it is
        // back with the adapter.
        if !crate::ddi::stall_diag::adapter_d0() {
            return;
        }
        let mut qpc_timestamp = 0;
        // SAFETY: a scalar clock read; `qpc_timestamp` is a live local.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) };
        let armed = self.vsync_armed.load(Ordering::Acquire) != 0;
        let reference = crate::ddi::stall_diag::vsync_reference();
        let last_tick = crate::ddi::stall_diag::vsync_last_tick();
        let period = helios_kmd_logic::vsync_deadline::period_100ns(vsync_rate_mhz(self));
        let (cb_entered, cb_returned) = crate::ddi::stall_diag::cb_counts();
        let display_half = self.display_half();
        let action = vsync_wd::decide(vsync_wd::WdInput {
            armed,
            display_half,
            adapter_d0: crate::ddi::stall_diag::adapter_d0(),
            now,
            last_tick,
            reference,
            period,
            cb_entered,
            cb_returned,
        });
        let silent = if armed && display_half {
            vsync_wd::silent_for_100ns(now, last_tick, reference)
        } else {
            None
        };
        let age_ms = vsync_wd::age_ms(silent);
        let now_ms = helios_kmd_logic::vsync_rate::ms_from_100ns(now);
        let tick_n = crate::ddi::stall_diag::note_wd_tick(now_ms, age_ms);
        let acted = action != WdAction::Idle;
        if acted {
            crate::ddi::stall_diag::note_wd_snapshot(
                now_ms,
                armed,
                helios_kmd_logic::vsync_rate::ms_from_100ns(reference),
                helios_kmd_logic::vsync_rate::ms_from_100ns(
                    self.vsync_deadline_100ns.load(Ordering::Acquire),
                ),
                age_ms,
            );
            match action {
                WdAction::Fix => {
                    if self.revive_heartbeat(reference, now, period) {
                        crate::ddi::stall_diag::note_wd_acted(true);
                    }
                }
                WdAction::Hung => crate::ddi::stall_diag::note_wd_acted(false),
                WdAction::Idle => {}
            }
        }
        if display_half && vsync_wd::publish_due(tick_n, acted) && self.hpd_running() {
            crate::ddi::stall_diag::request_live_publish();
            self.signal_hpd();
        }
    }

    /// Quiesce for a transient D3 transition, preserving ControlInterrupt's
    /// delivery gate for the later D0 resume. PASSIVE_LEVEL only.
    pub fn quiesce_vsync(&self) {
        self.stop_vsync_wd();
        self.disarm_vsync();
    }

    /// StopDevice/RemoveDevice terminal teardown. In addition to cancelling
    /// the heartbeat, close the delivery gate so no later queued DPC can emit
    /// a CRTC VSync packet.
    pub fn stop_vsync(&self) {
        self.vsync_enabled
            .store(0, core::sync::atomic::Ordering::Release);
        self.stop_vsync_wd();
        self.disarm_vsync();
    }

    /// Cancel + drain an initialized timer/DPC pair. PASSIVE_LEVEL only.
    fn disarm_vsync(&self) {
        use core::sync::atomic::Ordering;
        if self.vsync_armed.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        // `VsDisN`; the watchdog has nothing to watch from here.
        crate::ddi::stall_diag::note_vsync_disarmed();
        // SAFETY: the selected timer was initialized before publication. An Ex
        // timer stays allocated until final RemoveDevice, where ExDeleteTimer's
        // cancel+wait drains its callback; the embedded fallback retains its
        // established KeFlushQueuedDpcs drain at each quiesce.
        unsafe {
            self.cancel_vsync_one_shot();
            if !self.vsync_uses_ex_timer() {
                wdk_sys::ntddk::KeFlushQueuedDpcs();
                // The fallback DPC re-arms itself. Its post-arm lifecycle check
                // plus this second cancel closes the final rearm boundary.
                self.cancel_vsync_one_shot();
            }
        }
        self.vsync_deadline_100ns.store(0, Ordering::Release);
        self.vsync_gap_prev_100ns.store(0, Ordering::Relaxed);
    }

    /// Initialize the embedded kernel dispatcher objects. MUST be called once,
    /// after the context reaches its final (heap) address and before any DDI
    /// can run — `dxgkddi_add_device` calls it right after boxing.
    ///
    /// # Safety
    /// `self` must be at its final address and not yet visible to any other
    /// thread. PASSIVE_LEVEL only: it allocates the optional system
    /// high-resolution timer exactly once, before this context is published.
    pub unsafe fn init_kernel_events(&self) {
        // SAFETY: final stable address, before publication. NotificationEvent
        // wakes all competing submitters; each must retry the protected enqueue.
        unsafe { KeInitializeEvent(self.control_space_event.get(), 0, 0) };
        // SAFETY: per the fn contract; SynchronizationEvent (type 1), initially
        // signaled (the mutex starts free).
        unsafe { KeInitializeEvent(self.venus_mutex.get(), 1, 1) };
        // Same synchronization-event mutex shape as `venus_mutex`, but with a
        // distinct lock order and purpose: scanout lifecycle operations never
        // hold this while acquiring it recursively.
        unsafe { KeInitializeEvent(self.scanout_mutex.get(), 1, 1) };
        // HPD worker wake event: SynchronizationEvent (auto-clears on a satisfied
        // wait), initially unsignaled — the worker's own timeout drives the first
        // indication; later signals come from the config-change DPC.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_event.get(), 1, 0) };
        // Worker-exited latch: NotificationEvent (type 0) so it STAYS signalled
        // once set, initially unsignaled. A synchronization event would be
        // consumed by the first waiter and a second stop_hpd would block.
        // SAFETY: per the fn contract; stable in-place KEVENT storage.
        unsafe { KeInitializeEvent(self.hpd_exited.get(), 0, 0) };
        // VSync objects are dispatcher objects too. Initialize them at the
        // final adapter address before the context is published; StartDevice
        // and power callbacks only arm/cancel this already-valid pair.
        unsafe {
            wdk_sys::ntddk::KeInitializeDpc(
                self.vsync_dpc.get(),
                Some(vsync_dpc_routine),
                self as *const _ as PVOID,
            );
            wdk_sys::ntddk::KeInitializeTimerEx(
                self.vsync_timer.get(),
                wdk_sys::_TIMER_TYPE::SynchronizationTimer,
            );
        };
        // `ExAllocateTimer` is the default-resolution fix. It returns NULL on
        // resource failure, in which case the already-initialized KTIMER/DPC
        // above is the deliberate fallback. The callback context is this final,
        // immovable adapter address and neither source is armed until StartDevice.
        // SAFETY: AddDevice calls create/init_kernel_events at PASSIVE_LEVEL,
        // before the context is published to dxgkrnl or any timer can fire.
        let ex_timer = unsafe {
            ExAllocateTimer(
                Some(vsync_ex_timer_callback),
                self as *const _ as PVOID,
                EX_TIMER_HIGH_RESOLUTION,
            )
        };
        if !ex_timer.is_null() {
            self.vsync_ex_timer
                .store(ex_timer as usize, core::sync::atomic::Ordering::Release);
        }
        // The independent watchdog's timer (v329): default resolution (it ticks every 250 ms), its
        // own callback, the same immutable context. NULL = no watchdog this adapter lifetime
        // (`VsWdNoTm`); the heartbeat itself is unaffected.
        // SAFETY: as above.
        let wd_timer =
            unsafe { ExAllocateTimer(Some(vsync_wd_callback), self as *const _ as PVOID, 0) };
        if wd_timer.is_null() {
            crate::ddi::stall_diag::note_wd_no_timer();
        } else {
            self.vsync_wd_timer
                .store(wd_timer as usize, core::sync::atomic::Ordering::Release);
        }
    }
}

/// Common DISPATCH_LEVEL tick for the high-resolution Ex callback and the
/// embedded-KTIMER fallback DPC. It retains the fixed-phase, one-shot rule:
/// advance from the preceding interrupt-time deadline, skip any missed periods,
/// and arm just one future expiration. No timer source performs a catch-up burst.
unsafe fn service_vsync_tick(adapter: &AdapterContext) {
    use core::sync::atomic::Ordering;
    // THE RELEASE EDGE FOR THE TWO THINGS THAT WAIT ON NOTHING, and the reason it
    // is here: a WDDM head held by `WddmHoldMs`, or blocked past its `WddmHeadMs`
    // bound, is not waiting on any completion — so no used-ring DPC is guaranteed
    // to arrive and look at it again. This 60 Hz tick is the one periodic DISPATCH
    // edge this driver already owns; at 16.7 ms granularity it releases a 100 ms
    // hold with ~17 ms of slop, which neither the experiment's
    // three-orders-of-magnitude signal nor a 250 ms liveness bound cares about.
    //
    // Deliberately BEFORE the display gates below, so the release does not also
    // depend on `vsync_enabled`.
    //
    // ⛔ THE SECOND TEST IS STATE, NOT THE KNOB, and that is not a style choice:
    // `WddmHeadMs` DEFAULTS TO 250, so `WDDM_HEAD_MS != 0` is true on every
    // shipping boot and would queue a DPC on all 60 ticks a second forever, on a
    // desktop with no D3D12 client — a permanent DISPATCH tax on the compositor
    // path. `wddm_head_bound_due` is non-false only while a head is genuinely
    // armed AND past its deadline, and it CLAIMS the deadline, so one arm buys at
    // most one prompt.
    //
    // COST WHEN NEITHER APPLIES, which is the shipping steady state: one relaxed
    // 32-bit load for the knob, one relaxed 64-bit load inside
    // `wddm_head_bound_due`, and NO clock read — it samples the time only after a
    // non-zero deadline proves there is something to compare against.
    if crate::virtio::gpu::WDDM_HOLD_MS.load(Ordering::Relaxed) != 0
        || crate::virtio::VirtioGpu::wddm_head_bound_due()
    {
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
    }
    // The foreign scanout source's no-present watchdog: ends a source whose lapse the HPD
    // worker has not polled in time. One relaxed load while no user source is live.
    adapter.foreign_scanout_tick();
    if !adapter.display_half() || adapter.vsync_armed.load(Ordering::Acquire) == 0 {
        // `VsEarlyN`: a tick that ended the chain (nothing re-arms it below this line).
        crate::ddi::stall_diag::note_vsync_early();
        return;
    }
    // A one-shot timer is required because the fallback KTIMER's recurring
    // period is integer milliseconds: 16 ms is 62.5 Hz and 17 ms is 58.8 Hz,
    // while the mode contract exposed to Windows is the host's exact rate
    // (60/1 by default, `period_100ns` for any other). Advance from
    // the prior interrupt-time deadline so ordinary callback latency does not
    // become drift; if delayed across several periods, skip to one future
    // deadline rather than emitting a burst of synthetic retraces.
    //
    // The block's value is the interrupt time this tick was serviced at, for
    // `vsync_last_100ns`.
    let tick_time_100ns = unsafe {
        use wdk_sys::ntddk::KeQueryInterruptTimePrecise;

        let mut qpc_timestamp = 0;
        let now = KeQueryInterruptTimePrecise(&mut qpc_timestamp);
        let previous = adapter.vsync_deadline_100ns.load(Ordering::Acquire);
        // Preserve the original phase after the first arm.  Substituting
        // `now` whenever the DPC is late would turn normal dispatch latency
        // into cumulative phase drift, defeating the one-shot scheme.
        let anchor = if previous == 0 { now } else { previous };
        let period = helios_kmd_logic::vsync_deadline::period_100ns(vsync_rate_mhz(adapter));
        // Gap statistics (diag `VsMinGap` / `VsFast`): the evidence that the
        // heartbeat does not burst. A few relaxed accesses, no lock and no
        // registry access; the tick is serialized (one-shot, rearmed below), and
        // a lost update in a race with arm/disarm only blurs a diagnostic.
        let gap_prev = adapter.vsync_gap_prev_100ns.load(Ordering::Relaxed);
        adapter.vsync_gap_prev_100ns.store(now, Ordering::Relaxed);
        if let Some(gap) = helios_kmd_logic::vsync_rate::tick_gap(gap_prev, now) {
            if gap < adapter.vsync_min_gap_100ns.load(Ordering::Relaxed) {
                adapter.vsync_min_gap_100ns.store(gap, Ordering::Relaxed);
            }
            if helios_kmd_logic::vsync_rate::is_fast(gap, period) {
                adapter.vsync_fast.fetch_add(1, Ordering::Relaxed);
            }
        }
        let Some(deadline) = helios_kmd_logic::vsync_deadline::next(anchor, now, period) else {
            // Interrupt-time representation exhausted. The current one-shot
            // has fired; leave it disarmed rather than schedule an immediate
            // 100 ns retry loop.
            adapter.vsync_deadline_100ns.store(0, Ordering::Release);
            adapter.vsync_armed.store(0, Ordering::Release);
            crate::ddi::stall_diag::note_vsync_exhausted();
            return;
        };
        adapter
            .vsync_deadline_100ns
            .store(deadline, Ordering::Release);
        let due = helios_kmd_logic::vsync_deadline::relative_due(deadline, now);
        adapter.set_vsync_one_shot(due);
        // StopDevice may clear the lifecycle arm and cancel immediately before
        // the arm above. Re-check after arming so that ordering also ends
        // cancelled;
        // if StopDevice races after this load, its own cancel wins.
        if adapter.vsync_armed.load(Ordering::Acquire) == 0 {
            adapter.cancel_vsync_one_shot();
            return;
        }
        now
    };
    // Stall diagnosis (`ddi::stall_diag`): the consecutive-pending-tick count `VsPendN` and its
    // maximum, and, only with `FlipWdogMs` set, the flip watchdog. Atomics only (DISPATCH),
    // before the delivery gate below so a disabled delivery does not blind the count; the
    // watchdog's kept address is read by the `phys` load further down, in this very tick.
    crate::ddi::stall_diag::on_vsync_tick(
        adapter,
        helios_kmd_logic::vsync_deadline::period_100ns(vsync_rate_mhz(adapter)),
    );
    // ControlInterrupt may close only the delivery gate at DIRQL. Keep the
    // one-shot heartbeat free-running while disabled so a later enable needs no
    // illegal timer operation and resumes on the next nominal retrace.
    if adapter.vsync_enabled.load(Ordering::Acquire) == 0 {
        // `VsOffN` (`ddi::stall_diag`): ticks that ran with the delivery gate closed.
        crate::ddi::stall_diag::note_gate_closed_tick();
        // `FfAsyncWin`: a programming pending behind a closed gate is not waited on for a tick
        // that will deliver; the heartbeat runs regardless, so it wakes the worker here.
        if crate::virtio::foreign_flip::early_wake()
            && adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0
        {
            crate::virtio::foreign_flip::note_gate_wake();
            adapter.signal_hpd();
        }
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    // A CRTC_VSYNC describes the pixels the scanout pipeline would read at
    // this retrace. Every enabled retrace must report it, including while a
    // newer SetVidPnSourceAddress is still being programmed at PASSIVE_LEVEL.
    // Until that producer-ready primary has been successfully published,
    // `last_primary_address` remains the last actually displayed primary and
    // is therefore the truthful address to report.
    //
    let phys = adapter.last_primary_address.load(Ordering::Acquire) as i64;
    // SAFETY: live callback interface; signal_crtc_vsync raises to DIRQL internally
    // via DxgkCbSynchronizeExecution and delivers the CRTC_VSYNC packet.
    // `VsCbSyncB` / `VsCbSyncOk` / `VsCbSyncT`: a sync that begins and never returns is visible.
    crate::ddi::stall_diag::cb_sync_begin(helios_kmd_logic::vsync_rate::ms_from_100ns(
        tick_time_100ns,
    ));
    let status = unsafe {
        crate::ddi::submit_command::signal_crtc_vsync(dxgkrnl, phys, crate::ddi::vidpn::CHILD_UID)
    };
    crate::ddi::stall_diag::cb_sync_end(status);
    let epoch = adapter.scanout_bound_epoch.load(Ordering::Acquire);
    if status == STATUS_SUCCESS {
        // Record every callback that actually reached dxgkrnl. At ~60 Hz the
        // fixed 32768-entry ring retains several minutes, and this is the
        // causal heartbeat a trace needs rather than a stale sampled mirror.
        crate::ddi::scanout_timeline::note(
            crate::ddi::scanout_timeline::kind::VBLANK_TICK,
            crate::ddi::scanout_timeline::flag::SUCCESS,
            epoch,
            0,
            phys as u64,
            adapter.active_scanout_resource.load(Ordering::Acquire),
            if adapter.vsync_uses_ex_timer() {
                crate::ddi::scanout_timeline::vblank_source::EX_HIGH_RESOLUTION
            } else {
                crate::ddi::scanout_timeline::vblank_source::KTIMER_FALLBACK
            },
        );
        if epoch != 0 && LAST_TIMELINE_VSYNC_EPOCH.swap(epoch, Ordering::AcqRel) != epoch {
            crate::ddi::scanout_timeline::note(
                crate::ddi::scanout_timeline::kind::VBLANK_EPOCH,
                crate::ddi::scanout_timeline::flag::SUCCESS,
                epoch,
                0,
                phys as u64,
                adapter.active_scanout_resource.load(Ordering::Acquire),
                0,
            );
        }
    }
    // SetVidPnSourceAddress may run inside the synchronized MMIO-flip callback
    // above at DIRQL. It can only publish the exact hAllocation there. Back at
    // this timer DPC's DISPATCH_LEVEL, wake the PASSIVE worker that is allowed
    // to take the Venus mutex and issue the host scanout commands.
    // Signal after the synchronized callback. This covers both a request that
    // was already pending and one that the callback just published. Replacing
    // this with a before/after nonzero comparison loses a wake if another CPU
    // consumes the old request while the callback publishes its successor.
    if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }
    // Time first, then the count: a reader that loads the count and then the time
    // sees a pair that is at most one tick apart, never a time older than its
    // count.
    adapter
        .vsync_last_100ns
        .store(tick_time_100ns, Ordering::Relaxed);
    adapter.vsync_count.fetch_add(1, Ordering::Relaxed);
}

/// `EXT_CALLBACK` for the preferred system-allocated high-resolution timer.
/// Ex timer callbacks run at DISPATCH_LEVEL. The context is the final,
/// immutable adapter address supplied to ExAllocateTimer; final RemoveDevice
/// waits in ExDeleteTimer before freeing it.
unsafe extern "system" fn vsync_ex_timer_callback(_timer: ExTimer, context: PVOID) {
    if context.is_null() {
        return;
    }
    // SAFETY: context was set exactly once from the final AdapterContext address
    // before publication. ExDeleteTimer(cancel=TRUE, wait=TRUE) in Drop keeps it
    // live until this callback has returned.
    let adapter = unsafe { &*(context as *const AdapterContext) };
    // `VsCbIn` / `VsCbOut`: entered and returned; a blocked callback is `VsCbIn` above `VsCbOut`.
    crate::ddi::stall_diag::cb_enter();
    unsafe { service_vsync_tick(adapter) };
    crate::ddi::stall_diag::cb_leave();
}

/// `EXT_CALLBACK` of the independent watchdog timer (DISPATCH_LEVEL). Same context rule as
/// [`vsync_ex_timer_callback`].
unsafe extern "system" fn vsync_wd_callback(_timer: ExTimer, context: PVOID) {
    if context.is_null() {
        return;
    }
    // SAFETY: the final adapter address, live until `ExDeleteTimer(wait=TRUE)` has drained this
    // callback at RemoveDevice.
    let adapter = unsafe { &*(context as *const AdapterContext) };
    adapter.vsync_wd_tick();
}

/// Embedded KTIMER fallback DPC. It is selected only if ExAllocateTimer failed
/// in AddDevice; the common service function records that source as aux=0 on
/// every successful VBLANK_TICK.
pub unsafe extern "C" fn vsync_dpc_routine(
    _dpc: *mut KDPC,
    context: *mut c_void,
    _arg1: *mut c_void,
    _arg2: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: context is the adapter pointer passed to KeInitializeDpc; valid
    // for the adapter lifetime (final removal cancels and flushes this fallback
    // DPC before freeing the context).
    let adapter = unsafe { &*(context as *const AdapterContext) };
    // `VsCbIn` / `VsCbOut`: entered and returned; a blocked callback is `VsCbIn` above `VsCbOut`.
    crate::ddi::stall_diag::cb_enter();
    unsafe { service_vsync_tick(adapter) };
    crate::ddi::stall_diag::cb_leave();
}
