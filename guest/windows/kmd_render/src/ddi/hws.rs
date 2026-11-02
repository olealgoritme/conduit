//! NVIDIA personality (`NvUmd` != 0) only: WDDM 2.7 hardware GPU scheduling.
//!
//! NVIDIA's UMDs create their contexts with `D3DKMTCreateContextVirtual`
//! (`HwQueueSupported | DisableGpuTimeout`, a 0xC80-byte "NVDA" blob) and then
//! `D3DKMTCreateHwQueue` on that context. dxgkrnl only accepts that when the
//! adapter runs hardware scheduling, which NVIDIA's 610.88 KMD turns on by
//! asking `DxgkCbQueryFeatureSupport(DXGK_FEATURE_HWSCH, STABLE)` at StartDevice
//! and by registering the context-based scheduling DDIs below. It does NOT
//! register `CreateHwContext` / `DestroyHwContext` / `SwitchToHwContextList`:
//! dxgkrnl passes the `DxgkDdiCreateContext` handle as `hHwContext` to
//! `DxgkDdiCreateHwQueue`. `lib.rs` builds that table only in this personality;
//! the default Helios table is unchanged.
//!
//! What is real and what is a stand-in:
//!
//! * `CreateContext` with NVDA private data on node 0 or 4 builds the native
//!   Blackwell TSG (`virtio::native_channel`) and answers the captured
//!   `DXGK_CONTEXTINFO`; queues on it are real channels, submission writes
//!   GP_PUT + the doorbell.
//! * HW-queue completion: the UMD's DMA buffer ends with a semaphore release
//!   of the progress fence (`RingBufferFenceRelease` is 0). This KMD has no
//!   interrupt source for native channels, so an INTERIM bounded poll reads each
//!   queue's progress fence through dxgkrnl's kernel CPU view and raises
//!   `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED` on the queue's node when it
//!   moved: after every submit, and from a high-resolution timer at most every
//!   1 ms while any queue has outstanding work (10 ms after a second without
//!   progress; stopped when nothing is outstanding). A notifier/event from the
//!   host RM channel should replace it.
//! * `SuspendContext` reports "already suspended" (STATUS_SUCCESS) without
//!   preempting anything: see [`dxgkddi_suspend_context`].
//! * `SetupPriorityBands`, `SetSchedulingLogBuffer`,
//!   `SetContextSchedulingProperties`, `NotifyContextPriorityChange` store what
//!   they are given and succeed; nothing consumes it yet (the host RM runlist
//!   scheduler does the scheduling). No scheduling-log entries are written.
//! * Fault and hang recovery for native queues ([`recover_native`]): the poll
//!   watches each queue's channel error notifier and, with work outstanding, its
//!   progress fence and GP_GET (`helios_kmd_logic::native_hang`). A fault, or no
//!   movement for `NvHangMs` (default 10 s, 0 = hang detection off), raises
//!   `DXGK_INTERRUPT_GPU_ENGINE_TIMEOUT` on the queue's node; `ResetHwEngine`
//!   then takes the affected contexts' TSGs off the runlist, resets their
//!   channels (`NvRcReset` bit 0, default on), completes their progress fences
//!   and invalidates the contexts (`DxgkCbInvalidateHwContext`). A report that
//!   no reset answers within 3 s is force-completed by the poll itself, so no
//!   wait on a dead channel outlives timeout + grace. `ResumeHwEngine` kicks
//!   the poll. The host engine itself is never reset.
//! * `ValidateSubmitCommand` succeeds; `SubmitCommandToHwQueue` validates its
//!   own private data.
//! * Software HW queues ([`SoftQueue`]): queues on contexts without NVDA
//!   private data (dxgkrnl's system / paging, GDI / CDD contexts) run every
//!   submission through the legacy `SubmitCommandVirtual` body
//!   (`submit_command::submit_virtual`): flip arming, present / execution /
//!   GDI-job decoding and the WDDM completion FIFO, exactly as that DMA buffer
//!   would run on the legacy path. Where the legacy path reports
//!   `DMA_COMPLETED`, the queue's progress fence is written instead and
//!   `MONITORED_FENCE_SIGNALED` raised for the node ([`HwQueueDone`],
//!   delivered at submit time or by the completion DPC in
//!   `interrupt::drain_used_and_complete`).
//! * `SignalMonitoredFence` (kernel submissions, `RingBufferFenceRelease` 0): a
//!   RenderGdi / PresentBlt request on a context with software queues is held
//!   and written when that context's next software-queue submission completes
//!   ([`HwQueueDone`]'s second write). Anything else is written with the CPU at
//!   the time of the call and the interrupt raised: correct for paging buffers
//!   (this KMD's paging executor ran the operation synchronously in
//!   BuildPagingBuffer), EARLY for RenderGdi / PresentBlt on NVDA contexts.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::adapter::AdapterContext;
use crate::device::{ContextHandleRef, DeviceHandleRef};
use crate::dxgk::*;
use crate::sync::SpinLock;
use crate::virtio::native_channel::{NativeContext, NativeQueue};
use helios_kmd_logic::hws::{self as logic, PollNext, Progress};
use helios_kmd_logic::native_channel as native_format;
use helios_kmd_logic::native_hang::{self as hang, Action, Watch};

// ── counters (atomics; mirrored at PASSIVE by `publish_counters`) ──────────────────────────────
static HWS_ENABLED: AtomicU32 = AtomicU32::new(0);
static HWS_QUERY_STATUS: AtomicU32 = AtomicU32::new(0);
static CTX_NATIVE: AtomicU32 = AtomicU32::new(0);
static CTX_NATIVE_FAIL: AtomicU32 = AtomicU32::new(0);
static CTX_NVDA_BAD: AtomicU32 = AtomicU32::new(0);
/// NVDA-private contexts without HW queues (dxgkrnl-scheduled, `HwsCtxSched`).
static CTX_NVDA_SCHED: AtomicU32 = AtomicU32::new(0);
static QUEUES: AtomicU32 = AtomicU32::new(0);
static QUEUE_FAIL: AtomicU32 = AtomicU32::new(0);
static SOFT_QUEUES: AtomicU32 = AtomicU32::new(0);
static SOFT_SUBMITS: AtomicU32 = AtomicU32::new(0);
static SOFT_LEGACY: AtomicU32 = AtomicU32::new(0);
static SOFT_DONE: AtomicU32 = AtomicU32::new(0);
static SOFT_PURGED: AtomicU32 = AtomicU32::new(0);
static SOFT_ABANDONED: AtomicU32 = AtomicU32::new(0);
static SMF_DEFERRED: AtomicU32 = AtomicU32::new(0);
static SMF_EARLY: AtomicU32 = AtomicU32::new(0);
static SMF_TAKEN: AtomicU32 = AtomicU32::new(0);

/// `DXGK_KERNEL_SUBMISSION_BUILD_PAGING_BUFFER` (`DXGK_KERNEL_SUBMISSION_TYPE` 0).
const KERNEL_SUBMISSION_PAGING: u32 = 0;

/// A HW queue on a context without native channel state: dxgkrnl's own system /
/// paging contexts (HWS creates queues for them during AddAdapter, and fails the
/// adapter STATUS_NOT_SUPPORTED if that is refused), GDI / CDD contexts, and
/// NVDA contexts on nodes with no backing yet.
///
/// On a context without NVDA private data (`legacy`) the DMA buffers carry the
/// same work the legacy path runs (presents and flips, RenderKm / RenderGdi
/// jobs, paging), so each submission goes through `submit_command::submit_virtual`
/// and completes when the legacy path would report `DMA_COMPLETED` for it
/// ([`HwQueueDone`]). On an NVDA context without a native TSG there is no
/// legacy equivalent (NVIDIA command streams for an engine this KMD does not
/// back): the submission completes at once.
const HW_TAG_SOFT_QUEUE: u32 = 0x5357_5148; // "HQWS"

#[repr(C)]
struct SoftQueue {
    tag: u32,
    node: u32,
    adapter: *mut AdapterContext,
    fence_cpu: *mut u64,
    /// The hContext this queue was created on (`DxgkDdiCreateHwQueue`'s
    /// `hHwContext`). dxgkrnl destroys every queue before its context.
    context: HANDLE,
    /// Submissions run through the legacy path (a context without NVDA data).
    legacy: bool,
    /// The context is one of dxgkrnl's own (`hws::is_kernel_context`): its
    /// private data is always the KMD's.
    kernel: bool,
}

/// What a software HW queue submission reports once its work is complete: its
/// progress fence, and the monitored fence a `SignalMonitoredFence` held for it.
/// Carried by the WDDM completion FIFO entry (`virtio::gpu::WddmPending`).
#[derive(Clone, Copy)]
pub(crate) struct HwQueueDone {
    /// The `SoftQueue` as an identity (`DestroyHwQueue` purges by it).
    queue: usize,
    /// dxgkrnl's kernel CPU view of the queue's progress fence, and the
    /// submission's `HwQueueProgressFenceId`.
    progress_cpu: usize,
    progress_id: u64,
    /// The held `SignalMonitoredFence` (cpu 0 = none).
    extra_cpu: usize,
    extra_value: u64,
}

impl HwQueueDone {
    pub(crate) fn queue(&self) -> usize {
        self.queue
    }

    /// Write both fences (never moving either backwards). Atomics only: legal at
    /// any IRQL, and race-free against another completion of the same queue.
    pub(crate) fn write(&self) {
        fence_max(self.extra_cpu, self.extra_value);
        fence_max(self.progress_cpu, self.progress_id);
    }
}

fn fence_max(cpu: usize, value: u64) {
    if cpu == 0 || cpu & 7 != 0 {
        return;
    }
    // SAFETY: dxgkrnl's kernel CPU view of a monitored fence, 8-aligned (checked)
    // and live while the queue (or the fence object) is: entries of a destroyed
    // queue are purged first. Fence values only move forward.
    unsafe { (*(cpu as *const AtomicU64)).fetch_max(value, Ordering::AcqRel) };
}

/// Deliver one software HW queue completion: write its fences, then raise
/// `MONITORED_FENCE_SIGNALED` for `node`. Callers hold the WDDM notification
/// lock (the completion DPC and the immediate arm of `note_and_maybe_signal`).
///
/// # Safety
/// `dxgkrnl` is the live adapter's callback table; <= DISPATCH_LEVEL.
pub(crate) unsafe fn complete_soft_submission(
    dxgkrnl: &DXGKRNL_INTERFACE,
    done: &HwQueueDone,
    node: u32,
) -> NTSTATUS {
    done.write();
    SOFT_DONE.fetch_add(1, Ordering::Relaxed);
    // SAFETY: as the caller's.
    let status = unsafe { crate::ddi::submit_command::signal_monitored_fence(dxgkrnl, node) };
    if status == STATUS_SUCCESS {
        MF_SIGNALS.fetch_add(1, Ordering::Relaxed);
    }
    status
}

/// A completion whose interrupt could not be raised (the fence is written):
/// raise it again now and on the next poll tick. Outside the notification lock.
pub(crate) fn retry_monitored_fence(adapter: &AdapterContext, node: u32) {
    after_submit(adapter, 0, None, node_bit(node));
}

/// A software HW queue entry completed by a scheduler reset
/// (`VirtioGpu::terminal_abandon_wddm_epoch`). Atomics only.
pub(crate) fn note_soft_abandoned() {
    SOFT_ABANDONED.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
/// `h` is a queue handle this driver minted (native or soft), or null.
unsafe fn soft_queue(h: IN_CONST_HANDLE) -> Option<&'static SoftQueue> {
    if h.is_null() {
        return None;
    }
    // SAFETY: both queue kinds start with a u32 tag (`#[repr(C)]`).
    let tag = unsafe { *(h as *const u32) };
    (tag == HW_TAG_SOFT_QUEUE).then(|| unsafe { &*(h as *const SoftQueue) })
}

static QUEUE_FULL: AtomicU32 = AtomicU32::new(0);
static SUBMITS: AtomicU32 = AtomicU32::new(0);
static SUBMIT_FAIL: AtomicU32 = AtomicU32::new(0);
static MF_SIGNALS: AtomicU32 = AtomicU32::new(0);
static POLL_TICKS: AtomicU32 = AtomicU32::new(0);
static SUSPENDS: AtomicU32 = AtomicU32::new(0);
static RESUMES: AtomicU32 = AtomicU32::new(0);
static SCHED_PROPS: AtomicU32 = AtomicU32::new(0);
static PRIO_CHANGES: AtomicU32 = AtomicU32::new(0);
static BANDS: AtomicU32 = AtomicU32::new(0);
static LOG_BUFFERS: AtomicU32 = AtomicU32::new(0);
static ENGINE_RESETS: AtomicU32 = AtomicU32::new(0);
static ENGINE_RESUMES: AtomicU32 = AtomicU32::new(0);
static SMF: AtomicU32 = AtomicU32::new(0);
static SMF_KINDS: AtomicU32 = AtomicU32::new(0);
/// Native queue recovery (`recover_native`, the poll's watch): faults and hangs
/// reported, reports force-completed without a reset, queues recovered, contexts
/// invalidated, the last `DxgkCbInvalidateHwContext` / `RESET_CHANNEL` /
/// TSG-unschedule status, the last cause (`native_hang::Cause::code`) and the
/// engine-timeout interrupts dxgkrnl refused.
static NAT_FAULTS: AtomicU32 = AtomicU32::new(0);
static NAT_HANGS: AtomicU32 = AtomicU32::new(0);
static NAT_FORCED: AtomicU32 = AtomicU32::new(0);
static NAT_RECOVERED: AtomicU32 = AtomicU32::new(0);
static NAT_INVALIDATED: AtomicU32 = AtomicU32::new(0);
static NAT_INVALIDATE_ST: AtomicU32 = AtomicU32::new(0);
static NAT_RESET_ST: AtomicU32 = AtomicU32::new(0);
static NAT_UNSCHED_ST: AtomicU32 = AtomicU32::new(0);
static NAT_LAST_CAUSE: AtomicU32 = AtomicU32::new(0);
static NAT_TDR_FAIL: AtomicU32 = AtomicU32::new(0);
/// `NvHangMs` (u32::MAX until read) and `NvRcReset`, read at CreateHwQueue.
static HANG_MS: AtomicU32 = AtomicU32::new(u32::MAX);
static RC_RESET: AtomicU32 = AtomicU32::new(hang::DEFAULT_RC_RESET);

/// Mirror the `Hws*` counters at PASSIVE (from `publish_nvrm_counters`). Writes
/// nothing in the default personality.
pub(crate) fn publish_counters() {
    if !crate::ddi::wddm_surface::nvidia_selected() {
        return;
    }
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"HwsCtxNv", r(&CTX_NATIVE));
    crate::diag::record_named_bytes(b"HwsCtxNvFail", r(&CTX_NATIVE_FAIL));
    crate::diag::record_named_bytes(b"HwsCtxBad", r(&CTX_NVDA_BAD));
    crate::diag::record_named_bytes(b"HwsCtxSched", r(&CTX_NVDA_SCHED));
    crate::diag::record_named_bytes(b"HwsQueue", r(&QUEUES));
    crate::diag::record_named_bytes(b"HwsSoftQ", r(&SOFT_QUEUES));
    crate::diag::record_named_bytes(b"HwsSoftSub", r(&SOFT_SUBMITS));
    crate::diag::record_named_bytes(b"HwsQueueFail", r(&QUEUE_FAIL));
    crate::diag::record_named_bytes(b"HwsQFull", r(&QUEUE_FULL));
    crate::diag::record_named_bytes(b"HwsSubmit", r(&SUBMITS));
    crate::diag::record_named_bytes(b"HwsSubmitFail", r(&SUBMIT_FAIL));
    crate::diag::record_named_bytes(b"HwsMfSig", r(&MF_SIGNALS));
    crate::diag::record_named_bytes(b"HwsPollTick", r(&POLL_TICKS));
    crate::diag::record_named_bytes(b"HwsSuspend", r(&SUSPENDS));
    crate::diag::record_named_bytes(b"HwsResume", r(&RESUMES));
    crate::diag::record_named_bytes(b"HwsSchedProp", r(&SCHED_PROPS));
    crate::diag::record_named_bytes(b"HwsPrioChg", r(&PRIO_CHANGES));
    crate::diag::record_named_bytes(b"HwsBands", r(&BANDS));
    crate::diag::record_named_bytes(b"HwsLogBuf", r(&LOG_BUFFERS));
    crate::diag::record_named_bytes(b"HwsEngReset", r(&ENGINE_RESETS));
    crate::diag::record_named_bytes(b"HwsEngResume", r(&ENGINE_RESUMES));
    crate::diag::record_named_bytes(b"HwsSmf", r(&SMF));
    crate::diag::record_named_bytes(b"HwsSmfKind", r(&SMF_KINDS));
    crate::diag::record_named_bytes(b"HwsSoftLeg", r(&SOFT_LEGACY));
    crate::diag::record_named_bytes(b"HwsSoftDone", r(&SOFT_DONE));
    crate::diag::record_named_bytes(b"HwsSoftPurge", r(&SOFT_PURGED));
    crate::diag::record_named_bytes(b"HwsSoftAbn", r(&SOFT_ABANDONED));
    crate::diag::record_named_bytes(b"HwsSmfDefer", r(&SMF_DEFERRED));
    crate::diag::record_named_bytes(b"HwsSmfEarly", r(&SMF_EARLY));
    crate::diag::record_named_bytes(b"HwsSmfTaken", r(&SMF_TAKEN));
    crate::diag::record_named_bytes(b"HwsNatFault", r(&NAT_FAULTS));
    crate::diag::record_named_bytes(b"HwsNatHang", r(&NAT_HANGS));
    crate::diag::record_named_bytes(b"HwsNatForce", r(&NAT_FORCED));
    crate::diag::record_named_bytes(b"HwsNatRecov", r(&NAT_RECOVERED));
    crate::diag::record_named_bytes(b"HwsNatInval", r(&NAT_INVALIDATED));
    crate::diag::record_named_bytes(b"HwsNatInvSt", r(&NAT_INVALIDATE_ST));
    crate::diag::record_named_bytes(b"HwsNatRstSt", r(&NAT_RESET_ST));
    crate::diag::record_named_bytes(b"HwsNatUnsSt", r(&NAT_UNSCHED_ST));
    crate::diag::record_named_bytes(b"HwsNatCause", r(&NAT_LAST_CAUSE));
    crate::diag::record_named_bytes(b"HwsNatTdrF", r(&NAT_TDR_FAIL));
}

/// Read `NvHangMs` / `NvRcReset` from the service key (PASSIVE, CreateHwQueue).
fn read_recovery_knobs() {
    let ms = crate::diag::read_config_dword(
        crate::diag::KnobName::new(b"NvHangMs"),
        hang::DEFAULT_HANG_MS,
    );
    HANG_MS.store(ms, Ordering::Relaxed);
    let rc = crate::diag::read_config_dword(
        crate::diag::KnobName::new(b"NvRcReset"),
        hang::DEFAULT_RC_RESET,
    );
    RC_RESET.store(rc, Ordering::Relaxed);
}

fn hang_timeout() -> Option<u64> {
    match HANG_MS.load(Ordering::Relaxed) {
        u32::MAX => hang::hang_timeout_100ns(hang::DEFAULT_HANG_MS),
        ms => hang::hang_timeout_100ns(ms),
    }
}

// ── StartDevice: the HWSCH handshake ─────────────────────────────────────────────────────────

/// NVIDIA personality only: `DxgkCbQueryFeatureSupport(DXGK_FEATURE_HWSCH,
/// DXGK_FEATURE_SUPPORT_STABLE)`, as NVIDIA's KMD does at StartDevice. Records
/// `NvHwsEn` (dxgkrnl's `Enabled`) and `NvHwsQSt` (the callback's status;
/// 0xFFFFFFFF = no callback). PASSIVE, after the started state is published.
pub(crate) fn query_hwsch_at_start(adapter: &AdapterContext) {
    if !crate::ddi::wddm_surface::nvidia_selected() {
        return;
    }
    let mut enabled = 0u32;
    let mut status = u32::MAX;
    if let Some(dxgkrnl) = adapter.dxgkrnl_opt() {
        if let Some(query) = dxgkrnl.DxgkCbQueryFeatureSupport {
            // SAFETY: an all-zero DXGKARGCB_QUERYFEATURESUPPORT is valid plain data.
            let mut args: DXGKARGCB_QUERYFEATURESUPPORT = unsafe { core::mem::zeroed() };
            args.DeviceHandle = dxgkrnl.DeviceHandle;
            args.FeatureId = logic::FEATURE_HWSCH as DXGK_FEATURE_ID;
            args.DriverSupportState = logic::FEATURE_SUPPORT_STABLE;
            // SAFETY: the live adapter's callback table; PASSIVE (StartDevice).
            let st = unsafe { query(&mut args) };
            status = st as u32;
            if st >= 0 {
                enabled = u32::from(args.Enabled != 0);
            }
        }
    }
    HWS_ENABLED.store(enabled, Ordering::Relaxed);
    HWS_QUERY_STATUS.store(status, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvHwsEn", enabled);
    crate::diag::record_named_bytes(b"NvHwsQSt", status);
}

// ── per-context scheduling state ─────────────────────────────────────────────────────────────

/// What the WDDM 2.7 scheduling DDIs stored for one context. Recorded, not yet
/// acted on: the host RM runlist scheduler schedules the native channels.
/// (`dead_code`: stored for the real scheduler integration and for debugger
/// inspection; nothing reads most fields yet.)
#[allow(dead_code)]
pub(crate) struct ContextSched {
    suspended: AtomicU32,
    suspend_fence: AtomicU64,
    priority_band: AtomicU32,
    realtime_level: AtomicU32,
    in_process_priority: AtomicU32,
    quantum: AtomicU64,
    grace_same: AtomicU64,
    grace_lower: AtomicU64,
    scheduling_priority: AtomicU32,
}

impl ContextSched {
    pub(crate) const fn new() -> Self {
        Self {
            suspended: AtomicU32::new(0),
            suspend_fence: AtomicU64::new(0),
            priority_band: AtomicU32::new(0),
            realtime_level: AtomicU32::new(0),
            in_process_priority: AtomicU32::new(0),
            quantum: AtomicU64::new(0),
            grace_same: AtomicU64::new(0),
            grace_lower: AtomicU64::new(0),
            scheduling_priority: AtomicU32::new(0),
        }
    }
}

// ── the progress registry and its interim poll ───────────────────────────────────────────────

const MAX_QUEUES: usize = 64;

#[derive(Clone, Copy)]
struct Slot {
    /// `&NativeQueue` as an address; 0 = free. The queue lives in its boxed
    /// `HwQueue` until DestroyHwQueue, which removes the slot first.
    queue: usize,
    node: u32,
    progress: Progress,
    /// Fault / hang detection and recovery state (`native_hang`).
    watch: Watch,
}

const EMPTY: Slot = Slot {
    queue: 0,
    node: 0,
    progress: Progress::starting_at(0),
    watch: Watch::new(),
};

struct Registry {
    slots: [Slot; MAX_QUEUES],
    /// The poll timer is set (or about to be): a submit must not re-arm it, or
    /// submits faster than the period would push the expiry out forever.
    armed: bool,
    idle_ticks: u32,
    /// Nodes owed one more `MONITORED_FENCE_SIGNALED` (a CPU-side
    /// SignalMonitoredFence, delivered again on the next tick).
    pending_nodes: u32,
    /// Recoveries in progress: while nonzero the queues they picked are used
    /// outside this lock, so no slot may be removed (DestroyHwQueue waits).
    recovering: u32,
}

static REGISTRY: SpinLock<Registry> = SpinLock::new(Registry {
    slots: [EMPTY; MAX_QUEUES],
    armed: false,
    idle_ticks: 0,
    pending_nodes: 0,
    recovering: 0,
});

fn node_bit(node: u32) -> u32 {
    if node < 32 {
        1 << node
    } else {
        0
    }
}

fn register_queue(queue: &NativeQueue, node: u32) -> bool {
    let initial = queue.read_progress();
    let mut r = REGISTRY.lock();
    match r.slots.iter_mut().find(|slot| slot.queue == 0) {
        Some(slot) => {
            *slot = Slot {
                queue: queue as *const NativeQueue as usize,
                node,
                progress: Progress::starting_at(initial),
                watch: Watch::new(),
            };
            true
        }
        None => false,
    }
}

/// Remove a queue's slot. `Err(())` while a recovery is using registered queues
/// outside the lock (the caller waits and retries).
fn try_unregister_queue(queue: usize) -> Result<Option<Slot>, ()> {
    let mut r = REGISTRY.lock();
    if r.recovering != 0 {
        return Err(());
    }
    Ok(r
        .slots
        .iter_mut()
        .find(|slot| slot.queue == queue)
        .map(|slot| core::mem::replace(slot, EMPTY)))
}

/// The longest DestroyHwQueue waits for a recovery to finish (each of its host
/// round trips is bounded, and refused at once while the host does not answer).
const UNREGISTER_WAIT_MS: u64 = 60_000;

/// [`try_unregister_queue`], waiting out a recovery in progress. PASSIVE
/// (DestroyHwQueue). After [`UNREGISTER_WAIT_MS`] the slot is left registered and
/// `Err(())` returned: the queue must then not be freed.
fn unregister_queue(queue: usize) -> Result<Option<Slot>, ()> {
    let deadline = crate::adapter::foreign_scanout::now_100ns()
        .saturating_add(UNREGISTER_WAIT_MS * 10_000);
    loop {
        match try_unregister_queue(queue) {
            Ok(slot) => return Ok(slot),
            Err(()) if crate::adapter::foreign_scanout::now_100ns() < deadline => {
                let Some(passive) = crate::irql::PassiveLevel::try_assume() else {
                    return Err(());
                };
                crate::virtio::ctrl::sleep_ms(passive, 1);
            }
            Err(()) => return Err(()),
        }
    }
}

fn restore_slot(saved: Slot) {
    let mut r = REGISTRY.lock();
    if let Some(slot) = r.slots.iter_mut().find(|slot| slot.queue == 0) {
        *slot = saved;
    }
}

/// Raise `DXGK_INTERRUPT_MONITORED_FENCE_SIGNALED` once per node in `nodes`.
/// Callable at <= DISPATCH (`notify_at_dirql` synchronizes with the ISR).
fn signal_nodes(adapter: &AdapterContext, nodes: u32) {
    if nodes == 0 {
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    for node in 0..32u32 {
        if nodes & (1 << node) != 0 {
            // SAFETY: the live adapter's callback table; <= DISPATCH.
            let status = unsafe { crate::ddi::submit_command::signal_monitored_fence(dxgkrnl, node) };
            if status == STATUS_SUCCESS {
                MF_SIGNALS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Arm the poll if it is not armed. Called with the decision already taken
/// under the registry lock; the timer is set outside it.
fn arm(adapter: &AdapterContext, due_100ns: i64) {
    if !adapter.arm_hws_poll(due_100ns) {
        // No timer this adapter lifetime: progress is then only observed at
        // submit time (and by the next submit). Counted by `HwsPollTick == 0`.
        REGISTRY.lock().armed = false;
    }
}

/// One poll tick (the high-resolution timer callback, DISPATCH_LEVEL).
pub(crate) fn poll_tick(adapter: &AdapterContext) {
    POLL_TICKS.fetch_add(1, Ordering::Relaxed);
    let mut nodes = 0u32;
    let mut timeout_nodes = 0u32;
    let now = crate::adapter::foreign_scanout::now_100ns();
    let hang_timeout = hang_timeout();
    // The Vulkan ICD's copy channels (`ddi::nv_copy`), outside the registry lock.
    let (copy_busy, copy_moved) = crate::ddi::nv_copy::poll(adapter, now, hang_timeout);
    let next = {
        let mut r = REGISTRY.lock();
        let mut any_outstanding = copy_busy;
        let mut progressed = copy_moved;
        for slot in r.slots.iter_mut().filter(|slot| slot.queue != 0) {
            // SAFETY: a registered queue is live until DestroyHwQueue removed its
            // slot under this lock (see `Slot::queue`).
            let queue = unsafe { &*(slot.queue as *const NativeQueue) };
            let value = queue.read_progress();
            if slot.progress.observe(value) {
                nodes |= node_bit(slot.node);
                progressed = true;
            }
            let sample = hang::Sample {
                now,
                outstanding: slot.progress.outstanding(),
                progress: value,
                gp_get: queue.gp_get(),
                error: queue.channel_error(),
            };
            match slot.watch.tick(sample, hang_timeout) {
                Action::None => {}
                Action::Report(cause) => {
                    let counter = match cause {
                        hang::Cause::Fault(_) => &NAT_FAULTS,
                        hang::Cause::Hang => &NAT_HANGS,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    NAT_LAST_CAUSE.store(cause.code(), Ordering::Relaxed);
                    timeout_nodes |= node_bit(slot.node);
                }
                Action::ForceComplete(_) => {
                    // No reset came: complete the queue here (atomics only), so
                    // nothing waits on it; its context stays valid until dxgkrnl or
                    // the UMD tears it down.
                    NAT_FORCED.fetch_add(1, Ordering::Relaxed);
                    queue.mark_dead();
                    queue.complete_progress(hang::completion_value(slot.progress.submitted, value));
                    nodes |= node_bit(slot.node);
                    progressed = true;
                }
            }
            // A report waiting for its reset keeps the poll alive for the grace
            // fallback, outstanding work or not.
            any_outstanding |= slot.progress.outstanding() || slot.watch.in_grace();
        }
        nodes |= core::mem::take(&mut r.pending_nodes);
        if progressed || !any_outstanding {
            r.idle_ticks = 0;
        } else {
            r.idle_ticks = r.idle_ticks.saturating_add(1);
        }
        let next = logic::poll_next(any_outstanding, r.idle_ticks);
        r.armed = matches!(next, PollNext::Rearm(_));
        next
    };
    if let PollNext::Rearm(due) = next {
        arm(adapter, due);
    }
    signal_nodes(adapter, nodes);
    report_engine_timeouts(adapter, timeout_nodes);
}

/// Raise `DXGK_INTERRUPT_GPU_ENGINE_TIMEOUT` once per node in `nodes`; dxgkrnl
/// answers with `DxgkDdiResetHwEngine` for the node. <= DISPATCH.
fn report_engine_timeouts(adapter: &AdapterContext, nodes: u32) {
    if nodes == 0 {
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    for node in 0..32u32 {
        if nodes & (1 << node) != 0 {
            // SAFETY: the live adapter's callback table; <= DISPATCH.
            let status =
                unsafe { crate::ddi::submit_command::signal_gpu_engine_timeout(dxgkrnl, node) };
            if status != STATUS_SUCCESS {
                NAT_TDR_FAIL.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

// ── native queue recovery ────────────────────────────────────────────────────────────────────

/// What [`recover_native`] recovers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recover {
    /// `DxgkDdiResetHwEngine` for this node: the contexts of the queues reported
    /// faulted or hung on it, plus any whose error notifier is set now.
    Engine(u32),
    /// `DxgkDdiResetEngine` for this node: only the contexts of reported queues.
    Flagged(u32),
    /// An adapter reset (`DxgkDdiResetFromTimeout`): every native queue. No
    /// context is invalidated (the adapter reset ends every device anyway).
    All,
}

#[derive(Clone, Copy)]
struct Victim {
    queue: usize,
    context: usize,
    node: u32,
    submitted: u64,
}

const NO_VICTIM: Victim = Victim {
    queue: 0,
    context: 0,
    node: 0,
    submitted: 0,
};

/// Recover native queues from a fault or hang: every queue of an affected
/// context is marked dead (no submission reaches its channel again) and its
/// progress fence completed to everything submitted, so dxgkrnl's waits, process
/// teardown and the stop-time scheduler flush end. At PASSIVE each affected
/// context's TSG is also taken off the runlist, its channels reset (`NvRcReset`
/// bit 0) and, unless the whole adapter is being reset, the context invalidated
/// with `DxgkCbInvalidateHwContext` (dxgkrnl puts its device in error; the UMD
/// sees a removed device and creates new contexts). Returns how many queues were
/// recovered. NVIDIA personality only; 0 otherwise.
pub(crate) fn recover_native(adapter: &AdapterContext, what: Recover) -> u32 {
    if !crate::ddi::wddm_surface::nvidia_selected() {
        return 0;
    }
    let mut victims = [NO_VICTIM; MAX_QUEUES];
    let mut count = 0usize;
    {
        let mut r = REGISTRY.lock();
        let mut contexts = [0usize; MAX_QUEUES];
        let mut n_contexts = 0usize;
        for slot in r.slots.iter().filter(|slot| slot.queue != 0 && !slot.watch.is_dead()) {
            // SAFETY: registered queues are live while their slot is (see `Slot`).
            let queue = unsafe { &*(slot.queue as *const NativeQueue) };
            let culprit = match what {
                Recover::All => true,
                Recover::Engine(node) => {
                    slot.node == node
                        && (slot.watch.flagged().is_some() || queue.channel_error().is_some())
                }
                Recover::Flagged(node) => slot.node == node && slot.watch.flagged().is_some(),
            };
            let key = queue.context().map_or(0, |c| c as *const NativeContext as usize);
            if culprit && key != 0 && !contexts[..n_contexts].contains(&key) {
                contexts[n_contexts] = key;
                n_contexts += 1;
            }
        }
        if n_contexts == 0 {
            return 0;
        }
        for slot in r.slots.iter_mut().filter(|slot| slot.queue != 0 && !slot.watch.is_dead()) {
            // SAFETY: as above.
            let queue = unsafe { &*(slot.queue as *const NativeQueue) };
            let key = queue.context().map_or(0, |c| c as *const NativeContext as usize);
            if key != 0 && contexts[..n_contexts].contains(&key) && count < MAX_QUEUES {
                victims[count] = Victim {
                    queue: slot.queue,
                    context: key,
                    node: slot.node,
                    submitted: slot.progress.submitted,
                };
                count += 1;
                slot.watch.mark_dead();
            }
        }
        // The picked queues stay registered (and so alive) until this drops to 0.
        r.recovering += 1;
    }
    let victims = &victims[..count];
    let mut nodes = 0u32;
    for v in victims {
        // SAFETY: pinned by `recovering` (DestroyHwQueue cannot remove the slot).
        let queue = unsafe { &*(v.queue as *const NativeQueue) };
        queue.mark_dead();
        queue.complete_progress(hang::completion_value(v.submitted, queue.read_progress()));
        nodes |= node_bit(v.node);
    }
    NAT_RECOVERED.fetch_add(count as u32, Ordering::Relaxed);
    if let Some(passive) = crate::irql::PassiveLevel::try_assume() {
        let reset = RC_RESET.load(Ordering::Relaxed) & 1 != 0;
        for (i, v) in victims.iter().enumerate() {
            if victims[..i].iter().any(|w| w.context == v.context) {
                continue;
            }
            // SAFETY: a queue's context outlives it, and the queue is pinned.
            let context = unsafe { &*(v.context as *const NativeContext) };
            let unscheduled = context.unschedule(passive);
            NAT_UNSCHED_ST.store(native_code(unscheduled), Ordering::Relaxed);
            if reset {
                for w in victims.iter().filter(|w| w.context == v.context) {
                    // SAFETY: pinned, as above.
                    let queue = unsafe { &*(w.queue as *const NativeQueue) };
                    NAT_RESET_ST.store(native_code(queue.reset_channel(passive)), Ordering::Relaxed);
                }
            }
            if what != Recover::All {
                invalidate_context(adapter, context.dxg_context());
            }
        }
    }
    REGISTRY.lock().recovering -= 1;
    // The completed fences: dxgkrnl re-reads the nodes' monitored fences.
    after_submit(adapter, 0, None, nodes);
    count as u32
}

/// 0 for success, 0x100 + the error's discriminant otherwise (diagnostics).
fn native_code(result: Result<(), crate::virtio::native_channel::NativeError>) -> u32 {
    match result {
        Ok(()) => 0,
        Err(error) => 0x100 | error as u32,
    }
}

/// `DxgkCbInvalidateHwContext` for dxgkrnl's context handle `handle` as the
/// primary cause of an engine reset. PASSIVE.
fn invalidate_context(adapter: &AdapterContext, handle: usize) {
    if handle == 0 {
        NAT_INVALIDATE_ST.store(u32::MAX, Ordering::Relaxed);
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        return;
    };
    let Some(invalidate) = dxgkrnl.DxgkCbInvalidateHwContext else {
        NAT_INVALIDATE_ST.store(u32::MAX - 1, Ordering::Relaxed);
        return;
    };
    // SAFETY: an all-zero DXGKARGCB_INVALIDATEHWCONTEXT is valid plain data;
    // CollateralDamage 0 (Flags.Value 0) = this context caused the reset.
    let mut args: DXGKARGCB_INVALIDATEHWCONTEXT = unsafe { core::mem::zeroed() };
    args.hAdapter = dxgkrnl.DeviceHandle;
    args.hHwContext = handle as HANDLE;
    // SAFETY: the live adapter's callback table; PASSIVE (the caller holds the token).
    let status = unsafe { invalidate(&args) };
    NAT_INVALIDATE_ST.store(status as u32, Ordering::Relaxed);
    if status == STATUS_SUCCESS {
        NAT_INVALIDATED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Make sure the poll runs (a copy-channel submission, `ddi::nv_copy`). Any IRQL
/// up to DISPATCH.
pub(crate) fn kick_poll(adapter: &AdapterContext) {
    let arm_now = {
        let mut r = REGISTRY.lock();
        r.idle_ticks = 0;
        let arm_now = !r.armed;
        r.armed = true;
        arm_now
    };
    if arm_now {
        arm(adapter, -logic::POLL_PERIOD_100NS);
    }
}

/// After a submit (or a kernel-side fence signal): record it, look once at the
/// storage, and make sure the poll runs.
fn after_submit(adapter: &AdapterContext, queue: usize, fence_id: Option<u64>, extra_nodes: u32) {
    let mut nodes = 0u32;
    let arm_now = {
        let mut r = REGISTRY.lock();
        if let Some(slot) = r.slots.iter_mut().find(|slot| slot.queue == queue && queue != 0) {
            if let Some(id) = fence_id {
                slot.progress.note_submit(id);
            }
            // SAFETY: as in `poll_tick`.
            let q = unsafe { &*(slot.queue as *const NativeQueue) };
            if slot.progress.observe(q.read_progress()) {
                nodes |= node_bit(slot.node);
            }
        }
        r.pending_nodes |= extra_nodes;
        r.idle_ticks = 0;
        let arm_now = !r.armed;
        r.armed = true;
        arm_now
    };
    if arm_now {
        arm(adapter, -logic::POLL_PERIOD_100NS);
    }
    signal_nodes(adapter, nodes);
}

// ── contexts ─────────────────────────────────────────────────────────────────────────────────

/// `DxgkDdiCreateContext` in the NVIDIA personality. Contexts without NVDA
/// private data take the default path unchanged. An NVDA-private HW-queue
/// context on a node this KMD can back (0, 4) gets a native TSG; on any other
/// of the 16 nodes it is created without channel state (its queues are
/// refused). Either way the captured `DXGK_CONTEXTINFO` is answered.
///
/// # Safety
/// As `DxgkDdiCreateContext` (PASSIVE_LEVEL).
pub unsafe extern "C" fn dxgkddi_create_context(
    h_device: *mut c_void,
    create_context: *mut DXGKARG_CREATECONTEXT,
) -> NTSTATUS {
    if h_device.is_null() || create_context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live args for this call.
    let args = unsafe { &mut *create_context };
    // On input hContext is dxgkrnl's handle for the context (the one
    // `DxgkCbInvalidateHwContext` names); CreateContext overwrites it with ours.
    let runtime_context = args.hContext as usize;
    let private = if args.PrivateDriverDataSize == 0 || args.pPrivateDriverData.is_null() {
        &[][..]
    } else {
        // SAFETY: dxgkrnl supplies PrivateDriverDataSize readable bytes.
        unsafe {
            core::slice::from_raw_parts(
                args.pPrivateDriverData as *const u8,
                args.PrivateDriverDataSize as usize,
            )
        }
    };
    if !native_format::is_context_private(private) {
        // A context with another NVDA header (the Vulkan ICD's node-3 copy
        // context, 0xdf1 bytes, no HW queues) is scheduled by dxgkrnl through
        // SubmitCommand; its 0x35c-byte submit records need the captured
        // DmaBufferPrivateDataSize, or dxgkrnl refuses every SubmitCommand on
        // it with INVALID_PARAMETER.
        let nvda = native_format::is_nvda_context_header(private);
        // SAFETY: forwarded unchanged.
        let status =
            unsafe { crate::device::create_context_with(h_device, create_context, None, nvda) };
        if status == STATUS_SUCCESS && nvda {
            CTX_NVDA_SCHED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: still dxgkrnl's live args.
            let args = unsafe { &mut *create_context };
            set_nvda_context_info(args);
            // The Vulkan ICD's copy context runs its work on a channel of its own.
            if helios_kmd_logic::nv_copy::is_copy_context(private, args.NodeOrdinal) {
                // SAFETY: PASSIVE; the live hDevice and the hContext just made.
                unsafe { crate::ddi::nv_copy::create_channel(h_device, args.hContext as usize) };
            }
        }
        return status;
    }
    // SAFETY: Value is the plain UINT arm of DXGK_CREATECONTEXTFLAGS.
    let flags = unsafe { args.Flags.__bindgen_anon_1.Value };
    let parsed = match native_format::parse_hws_context_private(
        private,
        args.NodeOrdinal,
        args.EngineAffinity,
        flags,
    ) {
        Ok(parsed) => parsed,
        Err(_) => {
            // Loud (`HwsCtxBad`), not fatal: the context exists, its queues are
            // refused.
            CTX_NVDA_BAD.fetch_add(1, Ordering::Relaxed);
            // SAFETY: forwarded unchanged.
            return unsafe {
                crate::device::create_context_with(h_device, create_context, None, true)
            };
        }
    };
    let native = if parsed.native {
        let Some(device) = (unsafe { DeviceHandleRef::from_raw(h_device) }) else {
            return STATUS_INVALID_PARAMETER;
        };
        // SAFETY: PASSIVE; `h_device` is live.
        match unsafe { super::scheduler::create_native_context(h_device, &device) } {
            Ok(mut native) => {
                native.set_dxg_context(runtime_context);
                Some(native)
            }
            Err(status) => {
                CTX_NATIVE_FAIL.fetch_add(1, Ordering::Relaxed);
                return status;
            }
        }
    } else {
        None
    };
    let is_native = native.is_some();
    // SAFETY: both pointers were null-checked above, so this cannot fail and drop
    // the native TSG without its teardown.
    let status =
        unsafe { crate::device::create_context_with(h_device, create_context, native, true) };
    if status != STATUS_SUCCESS {
        return status;
    }
    if is_native {
        CTX_NATIVE.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: still dxgkrnl's live args.
    set_nvda_context_info(unsafe { &mut *create_context });
    STATUS_SUCCESS
}

/// The `DXGK_CONTEXTINFO` NVIDIA's 610.88 KMD answers for every NVDA-private
/// context, HW-queue or not.
fn set_nvda_context_info(args: &mut DXGKARG_CREATECONTEXT) {
    let info = logic::NVDA_CONTEXT_INFO;
    args.ContextInfo.DmaBufferSize = info.dma_buffer_size;
    args.ContextInfo.DmaBufferSegmentSet = info.dma_buffer_segment_set;
    args.ContextInfo.DmaBufferPrivateDataSize = info.dma_buffer_private_data_size;
    args.ContextInfo.Caps.__bindgen_anon_1.Value = info.caps;
}

/// DestroyContext's native half (`device::dxgkddi_destroy_context`).
pub(crate) fn destroy_native_context(
    passive: crate::irql::PassiveLevel,
    native: &mut NativeContext,
) -> Result<(), NTSTATUS> {
    super::scheduler::destroy_native_context(passive, native)
}

// ── HW queues ────────────────────────────────────────────────────────────────────────────────

/// `DxgkDdiCreateHwQueue`: `h_hw_context` is the hContext from CreateContext.
pub unsafe extern "C" fn dxgkddi_create_hw_queue(
    h_hw_context: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_CREATEHWQUEUE,
) -> NTSTATUS {
    crate::diag::record(0x0700_0003);
    if h_hw_context.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl passes a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(h_hw_context as HANDLE) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: dxgkrnl's live args; the context outlives its queues.
    let args = unsafe { &mut *args };
    let Some(native) = context.native() else {
        let legacy = !context.nvda();
        let queue = Box::new(SoftQueue {
            tag: HW_TAG_SOFT_QUEUE,
            node: context.node_ordinal(),
            adapter: context.adapter_ptr(),
            fence_cpu: args.HwQueueProgressFenceCPUVirtualAddress as *mut u64,
            context: h_hw_context as HANDLE,
            legacy,
            kernel: logic::is_kernel_context(context.create_flags()),
        });
        if legacy {
            context.soft_queues().fetch_add(1, Ordering::AcqRel);
        }
        args.hHwQueue = Box::into_raw(queue) as HANDLE;
        SOFT_QUEUES.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    };
    // PASSIVE: the recovery knobs, read again with every native queue.
    read_recovery_knobs();
    let status = unsafe { super::scheduler::create_native_queue(context.adapter_ptr(), native, args) };
    if status != STATUS_SUCCESS {
        QUEUE_FAIL.fetch_add(1, Ordering::Relaxed);
        return status;
    }
    // SAFETY: the handle was just minted by `create_native_queue`.
    let registered = unsafe { super::scheduler::native_queue_of(args.hHwQueue) }
        .is_some_and(|queue| register_queue(queue, context.node_ordinal()));
    if !registered {
        QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
        // A queue whose completion can never be reported must not exist.
        // SAFETY: the handle just minted; not yet known to dxgkrnl.
        let _ = unsafe { super::scheduler::dxgkddi_destroy_hw_queue(args.hHwQueue) };
        args.hHwQueue = core::ptr::null_mut();
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    QUEUES.fetch_add(1, Ordering::Relaxed);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_destroy_hw_queue(h_hw_queue: IN_CONST_HANDLE) -> NTSTATUS {
    if let Some(queue) = unsafe { soft_queue(h_hw_queue) } {
        if queue.legacy {
            // SAFETY: PASSIVE (DestroyHwQueue); the queue's context and adapter
            // outlive it.
            unsafe { retire_soft_queue(queue, h_hw_queue as usize) };
        }
        // SAFETY: minted by Box::into_raw in CreateHwQueue; freed once here.
        drop(unsafe { Box::from_raw(h_hw_queue as *mut SoftQueue) });
        return STATUS_SUCCESS;
    }
    // SAFETY: a handle this driver minted, or null (both handled).
    let key = unsafe { super::scheduler::native_queue_of(h_hw_queue) }
        .map_or(0, |queue| queue as *const NativeQueue as usize);
    let saved = if key != 0 {
        match unregister_queue(key) {
            Ok(saved) => saved,
            // A recovery still uses the queue: it must not be freed now.
            Err(()) => return STATUS_DEVICE_BUSY,
        }
    } else {
        None
    };
    // SAFETY: forwarded unchanged.
    let status = unsafe { super::scheduler::dxgkddi_destroy_hw_queue(h_hw_queue) };
    if status != STATUS_SUCCESS {
        // The queue (and its fence storage) still lives; keep reporting it.
        if let Some(saved) = saved {
            restore_slot(saved);
        }
    }
    status
}

/// DestroyHwQueue of a legacy software queue: its FIFO entries leave without a
/// fence write (the storage goes with the queue), a held `SignalMonitoredFence`
/// aimed at its progress fence is dropped, and one aimed elsewhere is written
/// now once the context has no software queue left to carry it.
///
/// # Safety
/// `queue` is live; its `context` and `adapter` are live; PASSIVE_LEVEL.
unsafe fn retire_soft_queue(queue: &SoftQueue, key: usize) {
    // SAFETY: the context outlives its queues (WDDM contract).
    let context = unsafe { ContextHandleRef::from_raw(queue.context) };
    if let Some(adapter) = unsafe { queue.adapter.as_ref() } {
        let purged = adapter.with_wddm_notify_lock(|guard| {
            guard
                .with_virtio(|order, v| v.purge_hw_queue(order, key))
                .unwrap_or(0)
        });
        if purged != 0 {
            SOFT_PURGED.fetch_add(purged, Ordering::Relaxed);
            // The node's next entry may be ready now.
            crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
        }
    }
    let Some(context) = context else {
        return;
    };
    let last = context.soft_queues().fetch_sub(1, Ordering::AcqRel) == 1;
    let orphan = {
        let mut stash = context.smf_stash().lock();
        match *stash {
            // Aimed at the storage that is going away: nothing left to write.
            Some(w) if w.cpu as usize == queue.fence_cpu as usize => {
                *stash = None;
                None
            }
            // No queue left on the context to carry it.
            Some(_) if last => stash.take(),
            _ => None,
        }
    };
    if let Some(w) = orphan {
        fence_max(w.cpu as usize, w.value);
        SMF_EARLY.fetch_add(1, Ordering::Relaxed);
        if let Some(adapter) = unsafe { queue.adapter.as_ref() } {
            after_submit(adapter, 0, None, node_bit(queue.node));
        }
    }
}

pub unsafe extern "C" fn dxgkddi_submit_command_to_hw_queue(
    h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SUBMITCOMMANDTOHWQUEUE,
) -> NTSTATUS {
    if h_adapter.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live args.
    let submit = unsafe { &*args };
    if let Some(queue) = unsafe { soft_queue(submit.hHwQueue) } {
        if queue.adapter != h_adapter as *mut AdapterContext {
            return STATUS_INVALID_PARAMETER;
        }
        let cpu = if submit.HwQueueProgressFenceCpuVa.is_null() {
            queue.fence_cpu
        } else {
            submit.HwQueueProgressFenceCpuVa as *mut u64
        };
        SOFT_SUBMITS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: our adapter (checked above).
        let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
        if !queue.legacy {
            // NVDA context on an unbacked node: nothing to run, complete now.
            fence_max(cpu as usize, submit.HwQueueProgressFenceId);
            signal_nodes(adapter, node_bit(queue.node));
            return STATUS_SUCCESS;
        }
        // SAFETY: dxgkrnl's live args (this call) and a live queue.
        unsafe { submit_soft_legacy(adapter, queue, submit, cpu as usize) };
        return STATUS_SUCCESS;
    }
    // SAFETY: forwarded unchanged; it validates the handle and args.
    let status = unsafe { super::scheduler::dxgkddi_submit_command_to_hw_queue(h_adapter, args) };
    if status != STATUS_SUCCESS {
        SUBMIT_FAIL.fetch_add(1, Ordering::Relaxed);
        return status;
    }
    SUBMITS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: validated by the submit above (non-null args, our queue, our adapter).
    let args = unsafe { &*args };
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let key = unsafe { super::scheduler::native_queue_of(args.hHwQueue) }
        .map_or(0, |queue| queue as *const NativeQueue as usize);
    after_submit(adapter, key, Some(args.HwQueueProgressFenceId), 0);
    STATUS_SUCCESS
}

/// A legacy software queue's submission: the `DXGKARG_SUBMITCOMMANDVIRTUAL` the
/// legacy path would have been handed for this DMA buffer, run through
/// `submit_command::submit_virtual`, which completes it through [`HwQueueDone`].
///
/// # Safety
/// `submit` is dxgkrnl's live args (the private data is valid only for this
/// call, which is all the legacy path reads it for); `queue` is live.
unsafe fn submit_soft_legacy(
    adapter: &AdapterContext,
    queue: &SoftQueue,
    submit: &DXGKARG_SUBMITCOMMANDTOHWQUEUE,
    progress_cpu: usize,
) {
    // SAFETY: the context outlives its queues (WDDM contract).
    let held = unsafe { ContextHandleRef::from_raw(queue.context) }
        .and_then(|context| context.smf_stash().lock().take());
    if held.is_some() {
        SMF_TAKEN.fetch_add(1, Ordering::Relaxed);
    }
    let done = HwQueueDone {
        queue: submit.hHwQueue as usize,
        progress_cpu,
        progress_id: submit.HwQueueProgressFenceId,
        extra_cpu: held.map_or(0, |w| w.cpu as usize),
        extra_value: held.map_or(0, |w| w.value),
    };
    // SAFETY: Value is the plain UINT arm of DXGK_SUBMITCOMMANDFLAGS.
    let flags = unsafe { submit.Flags.__bindgen_anon_1.Value };
    let (private, private_size) =
        if submit.pDmaBufferPrivateData.is_null() || submit.DmaBufferPrivateDataSize == 0 {
            (core::ptr::null_mut(), 0)
        } else {
            (submit.pDmaBufferPrivateData, submit.DmaBufferPrivateDataSize)
        };
    // SAFETY: an all-zero DXGKARG_SUBMITCOMMANDVIRTUAL is valid plain data.
    let mut virt: DXGKARG_SUBMITCOMMANDVIRTUAL = unsafe { core::mem::zeroed() };
    virt.hContext = queue.context;
    virt.DmaBufferVirtualAddress = submit.DmaBufferVirtualAddress;
    virt.DmaBufferSize = submit.DmaBufferSize;
    virt.pDmaBufferPrivateData = private;
    virt.DmaBufferPrivateDataSize = private_size;
    virt.DmaBufferUmdPrivateDataSize =
        logic::soft_submit_umd_private_size(queue.kernel, flags, private_size);
    // Not part of any SubmissionFenceId sequence: completion is `done`.
    virt.SubmissionFenceId = 0;
    virt.Flags = submit.Flags;
    virt.NodeOrdinal = queue.node;
    virt.EngineOrdinal = 0;
    SOFT_LEGACY.fetch_add(1, Ordering::Relaxed);
    // SAFETY: built from dxgkrnl's live args for this call; <= DISPATCH.
    unsafe { crate::ddi::submit_command::submit_virtual(adapter, &virt, Some(done)) };
}

// ── scheduler control DDIs ───────────────────────────────────────────────────────────────────

static BAND_GRACE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static BAND_QUANTUM: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static BAND_PROCESS_GRACE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static BAND_NORMAL_PERCENT: AtomicU32 = AtomicU32::new(0);

/// Stores the band configuration; the host RM runlist scheduler does the
/// actual time slicing, so nothing reads it yet.
pub unsafe extern "C" fn dxgkddi_setup_priority_bands(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETUPPRIORITYBANDS,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    BANDS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    for i in 0..4 {
        BAND_GRACE[i].store(args.gracePeriodForBand[i], Ordering::Relaxed);
        BAND_QUANTUM[i].store(args.processQuantumForBand[i], Ordering::Relaxed);
        BAND_PROCESS_GRACE[i].store(args.processGracePeriodForBand[i], Ordering::Relaxed);
    }
    BAND_NORMAL_PERCENT.store(args.targetNormalBandPercentage, Ordering::Relaxed);
    STATUS_SUCCESS
}

/// Stored per node; nothing writes log entries yet (`dead_code` for that reason).
#[allow(dead_code)]
#[derive(Clone, Copy)]
struct LogBuffer {
    cpu_va: usize,
    gpu_va: u64,
    entries: u32,
    interrupt_entry: u32,
}

const NO_LOG: LogBuffer = LogBuffer {
    cpu_va: 0,
    gpu_va: 0,
    entries: 0,
    interrupt_entry: 0,
};

static LOGS: SpinLock<[LogBuffer; logic_node_count()]> = SpinLock::new([NO_LOG; logic_node_count()]);

const fn logic_node_count() -> usize {
    native_format::HWS_NODE_COUNT as usize
}

/// Stores the per-node scheduling log buffer. No entries are written: dxgkrnl
/// initialised `FirstFreeEntryIndex` / `WraparoundCount` to zero, which this
/// leaves untouched and therefore valid ("no entries yet").
pub unsafe extern "C" fn dxgkddi_set_scheduling_log_buffer(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETSCHEDULINGLOGBUFFER,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    let node = args.NodeOrdinal as usize;
    if node >= logic_node_count()
        || args.EngineOrdinal != 0
        || !logic::log_buffer_ok(args.NumberOfEntries, args.LogBufferCpuVa as u64)
    {
        return STATUS_INVALID_PARAMETER;
    }
    LOG_BUFFERS.fetch_add(1, Ordering::Relaxed);
    LOGS.lock()[node] = LogBuffer {
        cpu_va: args.LogBufferCpuVa as usize,
        gpu_va: args.LogBufferGpuVa,
        entries: args.NumberOfEntries,
        interrupt_entry: args.InterruptEntry,
    };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_set_context_scheduling_properties(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SETCONTEXTSCHEDULINGPROPERTIES,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    // SAFETY: a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(args.hContext) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    SCHED_PROPS.fetch_add(1, Ordering::Relaxed);
    let s = context.hws();
    s.priority_band
        .store(args.priorityBand as u32, Ordering::Relaxed);
    s.realtime_level
        .store(args.realtimeBandPriorityLevel as u32, Ordering::Relaxed);
    s.in_process_priority
        .store(args.inProcessPriority as u32, Ordering::Relaxed);
    s.quantum.store(args.quantum, Ordering::Relaxed);
    s.grace_same
        .store(args.gracePeriodSamePriority, Ordering::Relaxed);
    s.grace_lower
        .store(args.gracePeriodLowerPriority, Ordering::Relaxed);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_notify_context_priority_change(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_NOTIFYCONTEXTPRIORITYCHANGE,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    // SAFETY: a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(args.hContext) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    PRIO_CHANGES.fetch_add(1, Ordering::Relaxed);
    let s = context.hws();
    s.scheduling_priority
        .store(args.SchedulingPriority as u32, Ordering::Relaxed);
    s.in_process_priority
        .store(args.InProcessPriority as u32, Ordering::Relaxed);
    STATUS_SUCCESS
}

/// Returns STATUS_SUCCESS, which the DDI contract defines as "the context is
/// already suspended", so no `DXGK_INTERRUPT_SUSPEND_CONTEXT_COMPLETED` is owed.
///
/// WHY THAT IS TOLERABLE TODAY: this KMD does not own a GPU scheduler. A native
/// context's channels are scheduled by the host RM on its runlist, where they
/// are idle-preemptible at any time, and the guest never needs the GPU off a
/// context to move its memory (allocations are host RM objects whose GPU VAs
/// do not change under the context). Legacy (non-native) contexts have no
/// channel at all. The suspend fence is recorded.
///
/// TODO: real preemption — disable the TSG's runlist entry on the host
/// (NVA06C_CTRL_CMD_GPFIFO_SCHEDULE false / preempt), then report the fence
/// with SUSPEND_CONTEXT_COMPLETED and return STATUS_PENDING.
pub unsafe extern "C" fn dxgkddi_suspend_context(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_SUSPENDCONTEXT,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    // SAFETY: a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(args.hContext) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    SUSPENDS.fetch_add(1, Ordering::Relaxed);
    let s = context.hws();
    let last = s.suspend_fence.load(Ordering::Relaxed);
    if logic::suspend_fence_newer(last, args.contextSuspendFence) {
        s.suspend_fence
            .store(args.contextSuspendFence, Ordering::Relaxed);
    }
    s.suspended.store(1, Ordering::Relaxed);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn dxgkddi_resume_context(
    _h_adapter: IN_CONST_HANDLE,
    args: IN_CONST_PDXGKARG_RESUMECONTEXT,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live input struct.
    let args = unsafe { &*args };
    // SAFETY: a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(args.hContext) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    RESUMES.fetch_add(1, Ordering::Relaxed);
    context.hws().suspended.store(0, Ordering::Relaxed);
    STATUS_SUCCESS
}

/// Recovers the native contexts on the node that were reported faulted or hung
/// (or whose channel error notifier is set now) and invalidates them
/// ([`recover_native`]); healthy contexts on the node keep running, since the
/// host engine itself is not reset. Then kicks the progress poll.
pub unsafe extern "C" fn dxgkddi_reset_hw_engine(
    h_adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_RESETHWENGINE,
) -> NTSTATUS {
    if h_adapter.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    ENGINE_RESETS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: our adapter context; dxgkrnl's live args.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    let node = unsafe { (*args).NodeOrdinal };
    let _ = recover_native(adapter, Recover::Engine(node));
    after_submit(adapter, 0, None, 0);
    STATUS_SUCCESS
}

/// Best effort, as [`dxgkddi_reset_hw_engine`].
pub unsafe extern "C" fn dxgkddi_resume_hw_engine(
    h_adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_RESUMEHWENGINE,
) -> NTSTATUS {
    if h_adapter.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    ENGINE_RESUMES.fetch_add(1, Ordering::Relaxed);
    // SAFETY: our adapter context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
    after_submit(adapter, 0, None, 0);
    STATUS_SUCCESS
}

/// Accepts every submission: `SubmitCommandToHwQueue` validates the native
/// private data itself (size, GP_PUT range) before touching the ring.
pub unsafe extern "C" fn dxgkddi_validate_submit_command(
    _h_adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_VALIDATESUBMITCOMMAND,
) -> NTSTATUS {
    if args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

/// Kernel submissions (paging, RenderGdi, PresentBlt) ask for a GPU write of
/// the monitored fence at the end of their DMA buffer (with
/// `RingBufferFenceRelease` 0 that is how the HW queue's progress fence of a
/// kernel submission gets written). Nothing is appended to the DMA buffer.
///
/// RenderGdi / PresentBlt on a context with legacy software queues: the write
/// is held on the context and attached to its next software-queue submission,
/// which writes it when the legacy completion gate releases that buffer
/// (`HwQueueDone`; `HwsSmfDefer` held, `HwsSmfTaken` attached). A second request
/// for a different fence before that submission writes the first one at once
/// (`HwsSmfEarly`). This relies on dxgkrnl's order for a kernel submission
/// (build the buffer, SignalMonitoredFence, SubmitCommandToHwQueue on the same
/// context); a held write whose buffer is never submitted waits for the
/// context's next submission.
///
/// Everything else (paging, NVDA contexts) is written by the CPU now and the
/// node is signalled (now and again on the next poll tick, in case dxgkrnl
/// registers its wait after this call): correct for paging (the executor ran
/// the operation synchronously in BuildPagingBuffer), EARLY for RenderGdi /
/// PresentBlt on NVDA contexts (counted in `HwsSmfKind` bits 1/2).
pub unsafe extern "C" fn dxgkddi_signal_monitored_fence(
    h_context: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_SIGNALMONITOREDFENCE,
) -> NTSTATUS {
    if h_context.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: dxgkrnl's live args.
    let args = unsafe { &mut *args };
    // SAFETY: a live hContext this driver returned.
    let Some(context) = (unsafe { ContextHandleRef::from_raw(h_context as HANDLE) }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(adapter) = context.adapter() else {
        return STATUS_INVALID_PARAMETER;
    };
    SMF.fetch_add(1, Ordering::Relaxed);
    let kind = args.KernelSubmissionType as u32;
    if kind < 32 {
        SMF_KINDS.fetch_or(1 << kind, Ordering::Relaxed);
    }
    let fence = args.MonitoredFenceCpuVa as usize;
    if fence == 0 || fence % 8 != 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let node = node_bit(context.node_ordinal());
    // A RenderGdi / PresentBlt buffer on a context with legacy software queues:
    // it is submitted on one of them next and runs through the legacy path, so
    // the write is held for that submission's completion (`HwQueueDone`) instead
    // of landing before its work. Paging keeps the immediate write: its work
    // already ran inside BuildPagingBuffer, and VidMm's paging progress must not
    // depend on a later submission arriving.
    if kind != KERNEL_SUBMISSION_PAGING
        && !context.nvda()
        && context.soft_queues().load(Ordering::Acquire) != 0
    {
        let new = logic::FenceWrite {
            cpu: fence as u64,
            value: args.MonitoredFenceValue,
        };
        let displaced = {
            let mut stash = context.smf_stash().lock();
            let (held, displaced) = logic::smf_stash_merge(*stash, new);
            *stash = Some(held);
            displaced
        };
        SMF_DEFERRED.fetch_add(1, Ordering::Relaxed);
        if let Some(w) = displaced {
            // A second fence before a submission took the first: write the first
            // now (the pre-deferral behaviour) rather than lose it.
            fence_max(w.cpu as usize, w.value);
            SMF_EARLY.fetch_add(1, Ordering::Relaxed);
            signal_nodes(adapter, node);
            after_submit(adapter, 0, None, node);
        }
        return STATUS_SUCCESS;
    }
    let fence = fence as *mut u64;
    // SAFETY: dxgkrnl's kernel CPU view of the monitored fence, 8-aligned
    // (checked), live for the call. Never moved backwards.
    unsafe {
        if fence.read_volatile() < args.MonitoredFenceValue {
            fence.write_volatile(args.MonitoredFenceValue);
        }
    }
    signal_nodes(adapter, node);
    after_submit(adapter, 0, None, node);
    STATUS_SUCCESS
}
