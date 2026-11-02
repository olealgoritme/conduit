//! WDDM 3.x hardware flip queue for the NVIDIA personality (`NvMpo3` bit 7).
//!
//! NVIDIA's KMD runs DWM's MPO3 flips through the hardware flip queue: dxgkrnl hands
//! flips with a `PresentId` and a `TargetFlipTime` to
//! `SetVidPnSourceAddressWithMultiPlaneOverlay3`, gives the driver a flip queue log
//! (`SetFlipQueueLogBuffer`), learns completed flips from that log through the MPO3
//! vsync (`FirstFreeFlipQueueLogEntryIndex`) or `UpdateFlipQueueLog`, and chooses which
//! vsyncs interrupt it with `SetInterruptTargetPresentId`. The MPO3 vsync carries no
//! address, so without the log dxgkrnl never sees a flip complete.
//!
//! The queue logic is `helios_kmd_logic::hw_flip_queue`; this module keeps one queue
//! for source 0 / plane 0 and runs it from the vsync tick (`adapter::kobj`), inside
//! `DxgkCbSynchronizeExecution` so it is serialized with the DIRQL DDIs. Showing a flip
//! is the legacy `SetVidPnSourceAddress` (the native-scanout arm puts NVIDIA's memory
//! on the host display).
//!
//! Counters: `HfqSub` submissions, `HfqNow` shown at submission, `HfqFull` refused
//! (queue full), `HfqLog` log buffers set (`HfqLogN` its size), `HfqTick` ticks that
//! logged, `HfqInt` vsync interrupts raised, `HfqCan` cancels, `HfqTgt` last interrupt
//! target (low 32 bits), `HfqFeat` dxgkrnl's DXGK_FEATURE_HWFLIPQUEUE answer
//! (0xFFFFFFFF = not asked).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use helios_kmd_logic::hw_flip_queue as fq;

use crate::dxgk::*;

/// `STATUS_RETRY`: the queue is full; dxgkrnl resubmits after a drain.
const STATUS_RETRY_DRAIN: NTSTATUS = 0xC000_022D_u32 as NTSTATUS;

/// What showing a flip needs: plane 0's primary.
#[derive(Clone, Copy)]
pub(crate) struct Show {
    segment: UINT,
    address: i64,
    allocation: HANDLE,
    context: HANDLE,
    immediate: UINT,
    on_next_vsync: UINT,
    duration: UINT,
}

struct State {
    queue: fq::Queue<Show>,
    log: *mut DXGK_FLIPQUEUE_LOG_ENTRY,
    adapter: HANDLE,
}

struct Global(UnsafeCell<State>);
// SAFETY: every access goes through `with`, which serializes on LOCK.
unsafe impl Sync for Global {}

static STATE: Global = Global(UnsafeCell::new(State {
    queue: fq::Queue::new(),
    log: core::ptr::null_mut(),
    adapter: core::ptr::null_mut(),
}));
static LOCK: AtomicBool = AtomicBool::new(false);
/// dxgkrnl's answer to DXGK_FEATURE_HWFLIPQUEUE (1 = enabled).
static FEATURE: AtomicU32 = AtomicU32::new(u32::MAX);

static SUBMITS: AtomicU32 = AtomicU32::new(0);
static SHOWN_NOW: AtomicU32 = AtomicU32::new(0);
static FULL: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);
static INTERRUPTS: AtomicU32 = AtomicU32::new(0);
static CANCELS: AtomicU32 = AtomicU32::new(0);
static LOGS: AtomicU32 = AtomicU32::new(0);
static LOG_ENTRIES: AtomicU32 = AtomicU32::new(0);
static TARGET: AtomicU32 = AtomicU32::new(0);
static LAST_RET: AtomicU32 = AtomicU32::new(0);

/// The counters as registry values; the mirror thread's pass (PASSIVE_LEVEL).
pub(crate) fn publish() {
    if !enabled() {
        return;
    }
    for (name, counter) in [
        (&b"HfqSub"[..], &SUBMITS),
        (b"HfqNow", &SHOWN_NOW),
        (b"HfqFull", &FULL),
        (b"HfqTick", &TICKS),
        (b"HfqInt", &INTERRUPTS),
        (b"HfqCan", &CANCELS),
        (b"HfqLog", &LOGS),
        (b"HfqLogN", &LOG_ENTRIES),
        (b"HfqTgt", &TARGET),
        (b"HfqRet", &LAST_RET),
    ] {
        crate::diag::record_named_bytes(name, counter.load(Ordering::Relaxed));
    }
}

/// The DIRQL DDIs and the tick (itself inside `DxgkCbSynchronizeExecution`) are already
/// serialized by the interrupt lock; `SetFlipQueueLogBuffer` may come at a lower IRQL,
/// so the state still takes this spin lock. Never held across a call that can block.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: LOCK grants exclusive access.
    let r = f(unsafe { &mut *STATE.0.get() });
    LOCK.store(false, Ordering::Release);
    r
}

/// DIRQL-safe: an atomic add only. The registry values are written by [`publish`]
/// from the mirror thread at PASSIVE_LEVEL (`diag::record_named_bytes` takes locks and
/// may write the registry, which changed the IRQL inside a DIRQL DDI: bugcheck 0x113/0x10).
fn count(counter: &AtomicU32) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Whether the hardware flip queue path is on (knob part, NVIDIA personality).
pub(crate) fn enabled() -> bool {
    crate::ddi::mpo3::part(crate::ddi::mpo3::PART_HWFQ)
}

/// The `MaxHwQueuedFlips` to report: only when dxgkrnl enabled the feature.
pub(crate) fn max_hw_queued_flips() -> UINT {
    if enabled() && FEATURE.load(Ordering::Relaxed) == 1 {
        fq::MAX_HW_QUEUED_FLIPS
    } else {
        0
    }
}

fn qpc() -> u64 {
    // SAFETY: KeQueryPerformanceCounter is callable at any IRQL.
    let counter = unsafe { wdk_sys::ntddk::KeQueryPerformanceCounter(core::ptr::null_mut()) };
    // SAFETY: LARGE_INTEGER is a plain i64 union.
    unsafe { counter.QuadPart as u64 }
}

/// StartDevice (PASSIVE): ask dxgkrnl whether the hardware flip queue may be used, as
/// NVIDIA's KMD does.
pub(crate) fn query_feature_at_start(dxgkrnl: &DXGKRNL_INTERFACE) {
    if !enabled() {
        return;
    }
    let mut enabled = 0u32;
    if let Some(query) = dxgkrnl.DxgkCbQueryFeatureSupport {
        // SAFETY: an all-zero DXGKARGCB_QUERYFEATURESUPPORT is valid plain data.
        let mut args: DXGKARGCB_QUERYFEATURESUPPORT = unsafe { core::mem::zeroed() };
        args.DeviceHandle = dxgkrnl.DeviceHandle;
        args.FeatureId = fq::FEATURE_HWFLIPQUEUE as DXGK_FEATURE_ID;
        args.DriverSupportState = helios_kmd_logic::hws::FEATURE_SUPPORT_STABLE;
        // SAFETY: the live adapter's callback table; PASSIVE (StartDevice).
        let st = unsafe { query(&mut args) };
        if st >= 0 {
            enabled = u32::from(args.Enabled != 0);
        } else {
            crate::diag::record_named_bytes(b"HfqFeatSt", st as u32);
        }
    }
    FEATURE.store(enabled, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"HfqFeat", enabled);
}

/// Plane 0 of an MPO3 flip. Returns the DDI status; `retry` is set when the caller must
/// return STATUS_RETRY with `HwFlipQueueDrainNeeded`.
pub(crate) unsafe fn submit(
    adapter: IN_CONST_HANDLE,
    plane: &DXGK_MULTIPLANE_OVERLAY_PLANE3,
    target_time: u64,
    duration: UINT,
    retry: &mut bool,
) -> NTSTATUS {
    count(&SUBMITS);
    // SAFETY: plain bitfield union reads.
    let flags = unsafe { plane.InputFlags.__bindgen_anon_1.__bindgen_anon_1 };
    let mut payload = None;
    if flags.Enabled() != 0 && plane.ContextCount != 0 && !plane.ppContextData.is_null() {
        // SAFETY: ContextCount >= 1 entries, per the plane contract.
        let Some(context) = (unsafe { (*plane.ppContextData).as_ref() }) else {
            return STATUS_INVALID_PARAMETER;
        };
        payload = Some(Show {
            // SAFETY: plain union reads of the context data.
            segment: UINT::from(unsafe { context.__bindgen_anon_1.SegmentId }),
            address: unsafe { context.__bindgen_anon_2.SegmentAddress.as_ref().QuadPart },
            allocation: context.hAllocation,
            context: context.hContext,
            immediate: flags.FlipImmediate(),
            on_next_vsync: flags.FlipOnNextVSync(),
            duration,
        });
    }
    let flip = fq::Flip { present_id: plane.PresentId, target_time, payload };
    let now = qpc();
    let outcome = with(|s| {
        s.adapter = adapter as HANDLE;
        s.queue.submit(flip, now)
    });
    match outcome {
        fq::Submit::ShowNow(show) => {
            count(&SHOWN_NOW);
            if let Some(show) = show {
                return unsafe { program(adapter, &show) };
            }
            crate::ddi::mpo3::note_published();
            STATUS_SUCCESS
        }
        fq::Submit::Queued => STATUS_SUCCESS,
        fq::Submit::Full => {
            count(&FULL);
            *retry = true;
            STATUS_RETRY_DRAIN
        }
    }
}

/// Show `show` through the legacy flip entry. Any IRQL up to DIRQL, as that entry.
unsafe fn program(adapter: IN_CONST_HANDLE, show: &Show) -> NTSTATUS {
    // SAFETY: an all-zero DXGKARG_SETVIDPNSOURCEADDRESS is a valid value.
    let mut legacy = unsafe { core::mem::zeroed::<DXGKARG_SETVIDPNSOURCEADDRESS>() };
    legacy.VidPnSourceId = 0;
    legacy.PrimarySegment = show.segment;
    legacy.PrimaryAddress.QuadPart = show.address;
    legacy.hAllocation = show.allocation;
    legacy.ContextCount = 1;
    legacy.Context[0] = show.context;
    // SAFETY: plain bitfield union writes.
    let out = unsafe { &mut legacy.Flags.__bindgen_anon_1.__bindgen_anon_1 };
    out.set_FlipImmediate(show.immediate);
    out.set_FlipOnNextVSync(show.on_next_vsync);
    legacy.Duration = show.duration;
    let status = unsafe { crate::ddi::traced::set_vidpn_source_address(adapter, &legacy) };
    if status != STATUS_SUCCESS {
        LAST_RET.store(status as u32, Ordering::Relaxed);
    }
    status
}

struct TickCtx {
    dxgkrnl: *const DXGKRNL_INTERFACE,
    target_id: u32,
    deliver: bool,
}

/// The vsync tick (DISPATCH): advances the queue and, when the interrupt target asks
/// for it and the vsync is enabled (`deliver`), raises the MPO3 vsync.
pub(crate) fn tick(dxgkrnl: &DXGKRNL_INTERFACE, target_id: u32, deliver: bool) -> NTSTATUS {
    let Some(sync) = dxgkrnl.DxgkCbSynchronizeExecution else {
        return STATUS_DEVICE_NOT_READY;
    };
    let ctx = TickCtx { dxgkrnl, target_id, deliver };
    let mut ret: BOOLEAN = 0;
    // SAFETY: valid DeviceHandle; routine and context live for the call.
    unsafe {
        sync(
            dxgkrnl.DeviceHandle,
            Some(tick_routine),
            &ctx as *const TickCtx as *mut core::ffi::c_void,
            0,
            &mut ret,
        )
    }
}

/// At DIRQL, serialized with the flip DDIs.
unsafe extern "C" fn tick_routine(context: *mut core::ffi::c_void) -> BOOLEAN {
    // SAFETY: the TickCtx `tick` passed, live for the synchronous call.
    let ctx = unsafe { &*(context as *const TickCtx) };
    // SAFETY: the live callback interface.
    let dxgkrnl = unsafe { &*ctx.dxgkrnl };
    let now = qpc();
    let (result, adapter, first_free) = with(|s| {
        let log = s.log;
        let entries = s.queue.log_entries();
        let result = s.queue.tick(now, |index, present_id| {
            if !log.is_null() && index < entries {
                // SAFETY: the OS log buffer holds `entries` entries (SetFlipQueueLogBuffer).
                unsafe {
                    let entry = &mut *log.add(index as usize);
                    entry.PresentId = present_id;
                    entry.PresentTimestamp = now;
                }
            }
        });
        (result, s.adapter, s.queue.first_free())
    });
    if result.logged != 0 {
        count(&TICKS);
    }
    if let Some(show) = result.show {
        match show {
            // SAFETY: DIRQL is a legal level for the legacy flip entry.
            Some(show) if !adapter.is_null() => unsafe {
                program(adapter, &show);
            },
            _ => crate::ddi::mpo3::note_published(),
        }
    }
    if !(result.interrupt && ctx.deliver) {
        return 1;
    }
    count(&INTERRUPTS);
    let mut info = unsafe { core::mem::zeroed::<DXGK_MULTIPLANE_OVERLAY_VSYNC_INFO3>() };
    info.LayerIndex = 0;
    info.FirstFreeFlipQueueLogEntryIndex = first_free;
    let mut interrupt = unsafe { core::mem::zeroed::<DXGKARGCB_NOTIFY_INTERRUPT_DATA>() };
    interrupt.InterruptType =
        _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3;
    // SAFETY: the arm for DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3.
    let vsync = unsafe { interrupt.__bindgen_anon_1.CrtcVsyncWithMultiPlaneOverlay3.as_mut() };
    vsync.VidPnTargetId = ctx.target_id;
    vsync.MultiPlaneOverlayVsyncInfoCount = 1;
    vsync.pMultiPlaneOverlayVsyncInfo = &mut info;
    let mut frequency = unsafe { core::mem::zeroed::<wdk_sys::LARGE_INTEGER>() };
    // SAFETY: callable at any IRQL.
    let counter = unsafe { wdk_sys::ntddk::KeQueryPerformanceCounter(&mut frequency) };
    vsync.GpuFrequency = unsafe { frequency.QuadPart } as u64;
    vsync.GpuClockCounter = unsafe { counter.QuadPart } as u64;
    if let Some(notify) = dxgkrnl.DxgkCbNotifyInterrupt {
        // SAFETY: DIRQL inside DxgkCbSynchronizeExecution; packet and `info` live.
        unsafe { notify(dxgkrnl.DeviceHandle, &mut interrupt) };
    }
    if let Some(queue_dpc) = dxgkrnl.DxgkCbQueueDpc {
        // SAFETY: as above.
        unsafe { queue_dpc(dxgkrnl.DeviceHandle) };
    }
    1
}

struct LogCtx {
    log: *mut DXGK_FLIPQUEUE_LOG_ENTRY,
    entries: u32,
}

unsafe extern "C" fn set_log_routine(context: *mut core::ffi::c_void) -> BOOLEAN {
    // SAFETY: the LogCtx `set_flip_queue_log_buffer` passed, live for the call.
    let ctx = unsafe { &*(context as *const LogCtx) };
    with(|s| {
        s.log = ctx.log;
        s.queue.set_log(ctx.entries);
    });
    1
}

/// `DxgkDdiSetFlipQueueLogBuffer`. It may arrive below DIRQL, so the state is changed
/// inside `DxgkCbSynchronizeExecution`: the spin lock is then never held at a lower IRQL
/// on a CPU where a DIRQL flip DDI could interrupt it.
pub unsafe extern "C" fn set_flip_queue_log_buffer(
    adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETFLIPQUEUELOGBUFFER,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a valid argument block.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if args.LayerIndex != 0 {
        return STATUS_SUCCESS;
    }
    count(&LOGS);
    LOG_ENTRIES.store(args.NumberOfEntries, Ordering::Relaxed);
    let ctx = LogCtx {
        log: args.LogBufferAddress,
        entries: if args.LogBufferAddress.is_null() { 0 } else { args.NumberOfEntries },
    };
    let context = adapter as *const crate::adapter::AdapterContext;
    // SAFETY: dxgkrnl hands back our AdapterContext, valid while the adapter lives.
    let dxgkrnl = unsafe { context.as_ref() }.and_then(|a| a.dxgkrnl_opt());
    match dxgkrnl.and_then(|d| d.DxgkCbSynchronizeExecution.map(|sync| (d, sync))) {
        Some((d, sync)) => {
            let mut ret: BOOLEAN = 0;
            // SAFETY: valid DeviceHandle; routine and context live for the call.
            unsafe {
                sync(
                    d.DeviceHandle,
                    Some(set_log_routine),
                    &ctx as *const LogCtx as *mut core::ffi::c_void,
                    0,
                    &mut ret,
                )
            }
        }
        None => {
            // SAFETY: as the synchronized routine, called directly.
            unsafe { set_log_routine(&ctx as *const LogCtx as *mut core::ffi::c_void) };
            STATUS_SUCCESS
        }
    }
}

pub unsafe extern "C" fn update_flip_queue_log(
    _adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_UPDATEFLIPQUEUELOG,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a writable argument block.
    let Some(args) = (unsafe { args.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    args.FirstFreeFlipQueueLogEntryIndex =
        if args.LayerIndex == 0 { with(|s| s.queue.first_free()) } else { 0 };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn set_interrupt_target_present_id(
    _adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETINTERRUPTTARGETPRESENTID,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a valid argument block.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    if args.LayerIndex == 0 {
        with(|s| s.queue.set_interrupt_target(args.InterruptTargetPresentId));
        TARGET.store(args.InterruptTargetPresentId as u32, Ordering::Relaxed);
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn cancel_flips(
    _adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_CANCELFLIPS,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a writable argument block with PlaneCount plane pointers.
    let Some(args) = (unsafe { args.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    count(&CANCELS);
    for i in 0..args.PlaneCount as usize {
        // SAFETY: i < PlaneCount.
        let Some(plane) = (unsafe { (*args.ppPlanes.add(i)).as_mut() }) else {
            continue;
        };
        plane.PresentIdCancelled = if plane.LayerIndex == 0 {
            with(|s| s.queue.cancel_from(plane.PresentIdCancelRequested))
        } else {
            0
        };
    }
    STATUS_SUCCESS
}

/// `DxgkDdiControlInterrupt3`: the vsync by its state, everything else through the
/// older entry.
pub unsafe extern "C" fn control_interrupt3(
    adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_CONTROLINTERRUPT3,
) -> NTSTATUS {
    // SAFETY: dxgkrnl passes a valid argument block.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let vsync = args.InterruptType == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC
        || args.InterruptType
            == _DXGK_INTERRUPT_TYPE::DXGK_INTERRUPT_CRTC_VSYNC_WITH_MULTIPLANE_OVERLAY3;
    // SAFETY: plain C enum union reads; which arm is meaningful depends on the type.
    let (vsync_state, interrupt_state) =
        unsafe { (args.__bindgen_anon_1.CrtcVsyncState, args.__bindgen_anon_1.InterruptState) };
    let enable = if vsync {
        vsync_state == _DXGK_CRTC_VSYNC_STATE::DXGK_VSYNC_ENABLE
    } else {
        interrupt_state == _DXGK_INTERRUPT_STATE::DXGK_INTERRUPT_ENABLE
    };
    unsafe {
        crate::ddi::interrupt::dxgkddi_control_interrupt(
            adapter,
            args.InterruptType,
            BOOLEAN::from(enable),
        )
    }
}

/// Adapter stop: drop queued flips and the log (dxgkrnl provides a new one).
pub(crate) fn reset() {
    with(|s| {
        s.queue.reset();
        s.log = core::ptr::null_mut();
        s.adapter = core::ptr::null_mut();
    });
}
