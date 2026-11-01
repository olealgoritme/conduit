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
//! * `ResetHwEngine` / `ResumeHwEngine` succeed without touching the host
//!   engine (best effort: they only kick the progress poll).
//! * `ValidateSubmitCommand` succeeds; `SubmitCommandToHwQueue` validates its
//!   own private data.
//! * `SignalMonitoredFence` writes the fence value with the CPU at the time of
//!   the call and raises the interrupt: correct for paging buffers (this KMD's
//!   paging executor ran the operation synchronously in BuildPagingBuffer),
//!   EARLY for RenderGdi / PresentBlt kernel submissions (TODO: defer to the
//!   DMA completion of that buffer).

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

// ── counters (atomics; mirrored at PASSIVE by `publish_counters`) ──────────────────────────────
static HWS_ENABLED: AtomicU32 = AtomicU32::new(0);
static HWS_QUERY_STATUS: AtomicU32 = AtomicU32::new(0);
static CTX_NATIVE: AtomicU32 = AtomicU32::new(0);
static CTX_NATIVE_FAIL: AtomicU32 = AtomicU32::new(0);
static CTX_NVDA_BAD: AtomicU32 = AtomicU32::new(0);
static QUEUES: AtomicU32 = AtomicU32::new(0);
static QUEUE_FAIL: AtomicU32 = AtomicU32::new(0);
static SOFT_QUEUES: AtomicU32 = AtomicU32::new(0);
static SOFT_SUBMITS: AtomicU32 = AtomicU32::new(0);

/// A HW queue on a context without native channel state: dxgkrnl's own system /
/// paging contexts (HWS creates queues for them during AddAdapter, and fails the
/// adapter STATUS_NOT_SUPPORTED if that is refused), and NVDA contexts on nodes
/// with no backing yet. Their DMA buffers carry no GPU work this KMD must run
/// (paging operations already complete inside BuildPagingBuffer), so a
/// submission completes at once: the progress fence is written and the node's
/// monitored-fence interrupt raised, as the legacy path completes its DMA.
const HW_TAG_SOFT_QUEUE: u32 = 0x5357_5148; // "HQWS"

#[repr(C)]
struct SoftQueue {
    tag: u32,
    node: u32,
    adapter: *mut AdapterContext,
    fence_cpu: *mut u64,
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

fn write_fence(cpu: *mut u64, value: u64) {
    if cpu.is_null() || (cpu as usize) & 7 != 0 {
        return;
    }
    // SAFETY: dxgkrnl's progress-fence CPU mapping, valid for the queue's life;
    // it only ever moves forward.
    unsafe {
        if core::ptr::read_volatile(cpu) < value {
            core::ptr::write_volatile(cpu, value);
        }
    }
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
}

const EMPTY: Slot = Slot {
    queue: 0,
    node: 0,
    progress: Progress::starting_at(0),
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
}

static REGISTRY: SpinLock<Registry> = SpinLock::new(Registry {
    slots: [EMPTY; MAX_QUEUES],
    armed: false,
    idle_ticks: 0,
    pending_nodes: 0,
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
            };
            true
        }
        None => false,
    }
}

fn unregister_queue(queue: usize) -> Option<Slot> {
    let mut r = REGISTRY.lock();
    let slot = r.slots.iter_mut().find(|slot| slot.queue == queue)?;
    Some(core::mem::replace(slot, EMPTY))
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
    let next = {
        let mut r = REGISTRY.lock();
        let mut any_outstanding = false;
        let mut progressed = false;
        for slot in r.slots.iter_mut().filter(|slot| slot.queue != 0) {
            // SAFETY: a registered queue is live until DestroyHwQueue removed its
            // slot under this lock (see `Slot::queue`).
            let queue = unsafe { &*(slot.queue as *const NativeQueue) };
            if slot.progress.observe(queue.read_progress()) {
                nodes |= node_bit(slot.node);
                progressed = true;
            }
            any_outstanding |= slot.progress.outstanding();
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
        // SAFETY: forwarded unchanged.
        return unsafe { crate::device::create_context_with(h_device, create_context, None) };
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
            return unsafe { crate::device::create_context_with(h_device, create_context, None) };
        }
    };
    let native = if parsed.native {
        let Some(device) = (unsafe { DeviceHandleRef::from_raw(h_device) }) else {
            return STATUS_INVALID_PARAMETER;
        };
        // SAFETY: PASSIVE; `h_device` is live.
        match unsafe { super::scheduler::create_native_context(h_device, &device) } {
            Ok(native) => Some(native),
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
    let status = unsafe { crate::device::create_context_with(h_device, create_context, native) };
    if status != STATUS_SUCCESS {
        return status;
    }
    if is_native {
        CTX_NATIVE.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: still dxgkrnl's live args.
    let args = unsafe { &mut *create_context };
    let info = logic::NVDA_CONTEXT_INFO;
    args.ContextInfo.DmaBufferSize = info.dma_buffer_size;
    args.ContextInfo.DmaBufferSegmentSet = info.dma_buffer_segment_set;
    args.ContextInfo.DmaBufferPrivateDataSize = info.dma_buffer_private_data_size;
    args.ContextInfo.Caps.__bindgen_anon_1.Value = info.caps;
    STATUS_SUCCESS
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
        let queue = Box::new(SoftQueue {
            tag: HW_TAG_SOFT_QUEUE,
            node: context.node_ordinal(),
            adapter: context.adapter_ptr(),
            fence_cpu: args.HwQueueProgressFenceCPUVirtualAddress as *mut u64,
        });
        args.hHwQueue = Box::into_raw(queue) as HANDLE;
        SOFT_QUEUES.fetch_add(1, Ordering::Relaxed);
        return STATUS_SUCCESS;
    };
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
    if unsafe { soft_queue(h_hw_queue) }.is_some() {
        // SAFETY: minted by Box::into_raw in CreateHwQueue; freed once here.
        drop(unsafe { Box::from_raw(h_hw_queue as *mut SoftQueue) });
        return STATUS_SUCCESS;
    }
    // SAFETY: a handle this driver minted, or null (both handled).
    let key = unsafe { super::scheduler::native_queue_of(h_hw_queue) }
        .map_or(0, |queue| queue as *const NativeQueue as usize);
    let saved = if key != 0 { unregister_queue(key) } else { None };
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
        write_fence(cpu, submit.HwQueueProgressFenceId);
        SOFT_SUBMITS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: our adapter (checked above).
        let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
        signal_nodes(adapter, node_bit(queue.node));
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

/// Best effort: the host RM owns the engine, so nothing is reset here. Kicks
/// the progress poll so whatever completed is reported. TODO: an RM channel
/// recovery (RC) path for native channels.
pub unsafe extern "C" fn dxgkddi_reset_hw_engine(
    h_adapter: IN_CONST_HANDLE,
    args: INOUT_PDXGKARG_RESETHWENGINE,
) -> NTSTATUS {
    if h_adapter.is_null() || args.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    ENGINE_RESETS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: our adapter context.
    let adapter = unsafe { &*(h_adapter as *const AdapterContext) };
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
/// the monitored fence at the end of their DMA buffer. This KMD's paging
/// executor already ran the operation synchronously in BuildPagingBuffer, so
/// the value is written by the CPU now and the node is signalled (now and
/// again on the next poll tick, in case dxgkrnl registers its wait after this
/// call). Nothing is appended to the DMA buffer.
///
/// TODO: RenderGdi / PresentBlt run asynchronously here; for those this signal
/// is EARLY (counted in `HwsSmfKind` bits 1/2). Defer it to the DMA completion
/// of the buffer.
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
    let fence = fence as *mut u64;
    // SAFETY: dxgkrnl's kernel CPU view of the monitored fence, 8-aligned
    // (checked), live for the call. Never moved backwards.
    unsafe {
        if fence.read_volatile() < args.MonitoredFenceValue {
            fence.write_volatile(args.MonitoredFenceValue);
        }
    }
    let node = node_bit(context.node_ordinal());
    signal_nodes(adapter, node);
    after_submit(adapter, 0, None, node);
    STATUS_SUCCESS
}
