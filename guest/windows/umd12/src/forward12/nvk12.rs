//! NVK on RM (dxvk-on-nvk S5): ordering the runtime's WDDM context behind
//! engine work the KMD cannot see.
//!
//! On Venus every ExecuteCommandLists submits an `HE12` Render packet naming a
//! point on a registered producer stream, and the KMD withholds the packet's DMA
//! completion until that host point retires. That is what orders the runtime's
//! monitored-fence signals, presents and waits on the context behind the real
//! GPU work (`queue.rs`'s module doc). On NVK there is no Venus stream: the
//! engine's execution stream is a local timeline (vkd3d patch 0002), the KMD
//! has no RM-fence boundary yet (S4), and an `HE12` record without a stream is
//! refused at Render. So this driver orders the context itself:
//!
//! * **Monitored fence** (`Nvk12EclSync=0`; off by default since it deadlocked Basemark DX12). Each queue owns one
//!   WDDM monitored fence created through the kernel callbacks. Every NVK
//!   boundary appends, on the queue's own context, the runtime admission event
//!   (exactly as on Venus) followed by a GPU wait for the fence to reach the
//!   boundary's value. A per-queue worker waits on the engine's execution stream
//!   and signals the fence from the CPU when the value is reached. Everything the
//!   runtime queues on the context afterwards -- the app's fence signals, its
//!   presents, DWM's view of them -- waits behind the real GPU work; the app
//!   thread never blocks.
//! * **CPU wait** (`Nvk12EclSync=1`, the default, or if the fence cannot be created). After
//!   the admission event, the DDI waits for the boundary on the calling thread.
//!   A wait-before-signal pattern (Queue::Wait on a fence the app signals after
//!   ExecuteCommandLists returns) would deadlock that wait, so it is capped at
//!   2 s per call and counted. The wait first polls the stream with zero
//!   timeouts for `Nvk12EclSpinUs` (default 2 ms): NVK's own blocking wait
//!   costs at least a Windows timer tick, once per ECL.
//!
//! The order on the context matters: admission first (it only fires once the
//! runtime's earlier waits are satisfied, and the engine worker waits for it),
//! then the fence wait (which needs the engine work admission released).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use helios_umd_common::hr::{E_FAIL, E_NOTIMPL};
use helios_umd_common::refusals::RefusalCounter;
use helios_umd_common::throttle::LogThrottle;

use crate::{ddi12, device12, log_error, note_refusal};

/// The CPU-wait arm's cap per ExecuteCommandLists.
const CPU_WAIT_NS: u64 = 2_000_000_000;
/// Zero-timeout probes between two clock reads in [`spin_wait`].
const SPIN_PROBES_PER_CLOCK: u32 = 8;
/// Waits per ECL CPU-wait timing line.
const ECL_WAIT_LOG_EVERY: u64 = 4096;

/// Poll the engine's execution stream for `value` with zero-timeout waits for
/// up to `Nvk12EclSpinUs`. `Some(result)` when the poll settled it (reached,
/// or an engine error), `None` when the budget ran out (or is 0) and the
/// caller must block.
///
/// Why not just block: NVK's RM backend blocks on the non-stall event or in
/// `Sleep()`, which on Windows costs at least a timer tick per wait; the
/// per-ECL wait then dominates the frame (`knobs12::NVK12_ECL_SPIN_US`).
///
/// # Safety
/// `engine_queue` is the live engine queue.
unsafe fn spin_wait(engine_queue: usize, value: u64) -> Option<Result<bool, ddi12::HRESULT>> {
    let budget_us = crate::knobs12::nvk12_ecl_spin_us();
    if budget_us == 0 {
        return None;
    }
    let deadline = Instant::now() + Duration::from_micros(u64::from(budget_us));
    loop {
        for _ in 0..SPIN_PROBES_PER_CLOCK {
            // SAFETY: forwarded precondition.
            match unsafe { crate::bridge12::wait_execution(engine_queue, value, 0) } {
                Ok(false) => std::hint::spin_loop(),
                settled => return Some(settled),
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        // Let a runnable thread on this core (the engine's submit worker,
        // which releases this very work) go first.
        std::thread::yield_now();
    }
}

/// Per-process timing of the ECL CPU waits, so the per-frame cost is a number
/// in the log rather than an inference from PresentMon.
struct EclWaitStats {
    waits: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    /// Waits the poll settled (no blocking engine wait).
    polled: AtomicU64,
    /// Wait-time histogram, upper bounds in [`ECL_WAIT_BUCKETS_US`] (last: above).
    buckets: [AtomicU64; ECL_WAIT_BUCKETS_US.len() + 1],
}

/// Histogram bucket upper bounds (us). Separates a timer tick per wait
/// (1-2 ms each, every ECL) from one long wait per frame (a context wait the
/// admission event sat behind, e.g. a swap-chain buffer DWM has not released).
const ECL_WAIT_BUCKETS_US: [u64; 6] = [50, 200, 1000, 2000, 5000, 20000];

impl EclWaitStats {
    fn record(&self, elapsed: Duration, polled: bool) {
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let total = self.total_ns.fetch_add(ns, Ordering::Relaxed).saturating_add(ns);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        let us = ns / 1000;
        let bucket = ECL_WAIT_BUCKETS_US
            .iter()
            .position(|&bound| us < bound)
            .unwrap_or(ECL_WAIT_BUCKETS_US.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let polled_n = if polled {
            NVK_REFUSALS.cpu_wait_polled.bump();
            self.polled.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            NVK_REFUSALS.cpu_wait_blocked.bump();
            self.polled.load(Ordering::Relaxed)
        };
        let n = self.waits.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % ECL_WAIT_LOG_EVERY == 0 {
            // Window max: reset so each line says something about its own window.
            let max = self.max_ns.swap(0, Ordering::Relaxed);
            let b: [u64; ECL_WAIT_BUCKETS_US.len() + 1] =
                core::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed));
            log_error!(
                "NVK ECL CPU wait: {n} waits, avg {} us, window max {} us, polled {polled_n} \
                 blocked {}, hist <50us {} <200us {} <1ms {} <2ms {} <5ms {} <20ms {} >=20ms {} \
                 (Nvk12EclSpinUs={})",
                total / n / 1000,
                max / 1000,
                n - polled_n,
                b[0],
                b[1],
                b[2],
                b[3],
                b[4],
                b[5],
                b[6],
                crate::knobs12::nvk12_ecl_spin_us(),
            );
        }
    }
}

static ECL_WAIT_STATS: EclWaitStats = EclWaitStats {
    waits: AtomicU64::new(0),
    total_ns: AtomicU64::new(0),
    max_ns: AtomicU64::new(0),
    polled: AtomicU64::new(0),
    buckets: [const { AtomicU64::new(0) }; ECL_WAIT_BUCKETS_US.len() + 1],
};

/// The worker's wait slice: it rechecks its stop flag this often.
const WORKER_SLICE_NS: u64 = 100_000_000;

static NVK_LOG: LogThrottle = LogThrottle::new();

/// The kernel-callback halves the worker needs, copied out of the device so
/// the worker never touches device state.
#[derive(Clone, Copy)]
struct SignalTarget {
    signal_cb: unsafe extern "system" fn(
        ddi12::HANDLE,
        *const ddi12::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU,
    ) -> ddi12::HRESULT,
    h_rt_device: usize,
    fence: ddi12::D3DKMT_HANDLE,
}

// SAFETY: plain handles and a runtime function pointer; the runtime's kernel
// callbacks are callable from any thread of the process.
unsafe impl Send for SignalTarget {}

impl SignalTarget {
    fn signal(&self, value: u64) -> ddi12::HRESULT {
        let handle = self.fence;
        // SAFETY: zero is a valid bit pattern for this plain C struct; the fields
        // this SDK revision may append are left zero (no flags).
        let mut args: ddi12::D3DDDICB_SIGNALSYNCHRONIZATIONOBJECTFROMCPU =
            unsafe { core::mem::zeroed() };
        args.ObjectCount = 1;
        args.ObjectHandleArray = &handle;
        args.FenceValueArray = &value;
        // SAFETY: runtime callback with the runtime device handle and a fence
        // this driver created on it; both arrays outlive the call.
        unsafe { (self.signal_cb)(self.h_rt_device as ddi12::HANDLE, &args) }
    }
}

/// One queue's NVK ordering state. Created at the queue's first NVK boundary.
pub(crate) struct NvkSync {
    /// `Some` in the monitored-fence arm.
    fence: Option<FenceArm>,
}

struct FenceArm {
    target: SignalTarget,
    tx: Mutex<Option<Sender<u64>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    /// Highest value handed to the worker (for teardown).
    queued: AtomicU64,
}

impl NvkSync {
    /// The monitored-fence arm when the knob and the runtime allow it, else
    /// the CPU-wait arm.
    ///
    /// # Safety
    /// `dev` is the live device of the queue whose engine queue `engine_queue`
    /// is; the engine queue outlives this value (the queue drops it first).
    pub(crate) unsafe fn new(dev: &device12::HeliosD3D12Device, engine_queue: usize) -> Self {
        if crate::knobs12::nvk12_ecl_sync() != 0 {
            note_refusal(&NVK_REFUSALS.cpu_wait_arm);
            log_error!("NVK ECL sync: CPU wait (Nvk12EclSync=1)");
            return Self { fence: None };
        }
        // SAFETY: forwarded precondition.
        match unsafe { FenceArm::create(dev, engine_queue) } {
            Some(arm) => Self { fence: Some(arm) },
            None => {
                note_refusal(&NVK_REFUSALS.fence_create_failed);
                log_error!("NVK ECL sync: monitored fence unavailable, CPU wait instead");
                Self { fence: None }
            }
        }
    }

    /// Order the queue's context behind engine boundary `value`. Called on the
    /// entering DDI thread after the admission event was queued on `h_context`.
    ///
    /// # Safety
    /// `dev`/`h_context`/`engine_queue` are the live device, the queue's runtime
    /// context and its engine queue.
    pub(crate) unsafe fn order_context(
        &self,
        dev: &device12::HeliosD3D12Device,
        h_context: *mut core::ffi::c_void,
        engine_queue: usize,
        value: u64,
    ) -> Result<(), ddi12::HRESULT> {
        if let Some(arm) = &self.fence {
            // SAFETY: forwarded precondition.
            return unsafe { arm.order_context(dev, h_context, value) };
        }
        // CPU wait: poll first (`Nvk12EclSpinUs`), then block.
        let started = Instant::now();
        // SAFETY: the live engine queue.
        let polled = unsafe { spin_wait(engine_queue, value) };
        let result = match polled {
            Some(reached) => reached,
            // SAFETY: the live engine queue.
            None => unsafe { crate::bridge12::wait_execution(engine_queue, value, CPU_WAIT_NS) },
        };
        ECL_WAIT_STATS.record(started.elapsed(), polled.is_some());
        match result {
            Ok(true) => {
                NVK_REFUSALS.cpu_waits.bump();
                Ok(())
            }
            Ok(false) => {
                note_refusal(&NVK_REFUSALS.cpu_wait_timeouts);
                if let Some(k) = NVK_LOG.first_n_then_every(16, 4096) {
                    log_error!(
                        "NVK ECL CPU wait: value {value} not reached in 2 s (a wait-before-signal?), \
                         proceeding (x{})",
                        k + 1
                    );
                }
                Ok(())
            }
            Err(hr) => Err(hr),
        }
    }

    /// Wait on the CPU for `value` (present on scanout). True when reached.
    ///
    /// # Safety
    /// `engine_queue` is the live engine queue.
    pub(crate) unsafe fn wait_cpu(engine_queue: usize, value: u64, timeout_ns: u64) -> bool {
        // SAFETY: forwarded precondition.
        matches!(unsafe { crate::bridge12::wait_execution(engine_queue, value, timeout_ns) }, Ok(true))
    }

    /// Stop the worker after the context was destroyed (which drained it) and
    /// release the fence.
    ///
    /// # Safety
    /// `dev` is the live device the fence was created on.
    pub(crate) unsafe fn shutdown(&self, dev: &device12::HeliosD3D12Device) {
        if let Some(arm) = &self.fence {
            // SAFETY: forwarded precondition.
            unsafe { arm.shutdown(dev) };
        }
    }
}

impl FenceArm {
    unsafe fn create(dev: &device12::HeliosD3D12Device, engine_queue: usize) -> Option<Self> {
        if dev.kt_callbacks.is_null() {
            return None;
        }
        // SAFETY: the device keeps its runtime-owned callback table alive.
        let kt = unsafe { &*dev.kt_callbacks };
        let create_cb = kt.pfnCreateSynchronizationObject2Cb?;
        let signal_cb = kt.pfnSignalSynchronizationObjectFromCpuCb?;
        kt.pfnWaitForSynchronizationObjectFromGpuCb?;
        // SAFETY: zero is valid for this plain C struct.
        let mut args: ddi12::D3DDDICB_CREATESYNCHRONIZATIONOBJECT2 = unsafe { core::mem::zeroed() };
        args.Info.Type = ddi12::_D3DDDI_SYNCHRONIZATIONOBJECT_TYPE_D3DDDI_MONITORED_FENCE;
        // The union member for a monitored fence: zero initial value (already
        // zero; written so the arm is explicit).
        args.Info.__bindgen_anon_1.MonitoredFence.InitialFenceValue = 0;
        // SAFETY: runtime callback, runtime device handle, live args.
        let hr = unsafe { create_cb(dev.h_rt_device.handle, &mut args) };
        if hr < 0 || args.hSyncObject == 0 {
            log_error!("NVK ECL sync: CreateSynchronizationObject2(monitored fence) hr={:#010x}", hr as u32);
            return None;
        }
        let target = SignalTarget {
            signal_cb,
            h_rt_device: dev.h_rt_device.handle as usize,
            fence: args.hSyncObject,
        };
        let (tx, rx) = channel::<u64>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("helios-nvk12-ecl".into())
            .spawn(move || worker_main(engine_queue, target, rx, worker_stop))
            .ok()?;
        log_error!("NVK ECL sync: monitored fence {:#x}, worker started", target.fence);
        Some(Self {
            target,
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            stop,
            queued: AtomicU64::new(0),
        })
    }

    unsafe fn order_context(
        &self,
        dev: &device12::HeliosD3D12Device,
        h_context: *mut core::ffi::c_void,
        value: u64,
    ) -> Result<(), ddi12::HRESULT> {
        if dev.kt_callbacks.is_null() {
            return Err(E_NOTIMPL);
        }
        // SAFETY: the device keeps its runtime-owned callback table alive.
        let wait_cb =
            unsafe { (*dev.kt_callbacks).pfnWaitForSynchronizationObjectFromGpuCb }.ok_or(E_NOTIMPL)?;
        let handle = self.target.fence;
        // SAFETY: zero is valid for this plain C struct.
        let mut args: ddi12::D3DDDICB_WAITFORSYNCHRONIZATIONOBJECTFROMGPU =
            unsafe { core::mem::zeroed() };
        args.hContext = h_context;
        args.ObjectCount = 1;
        args.ObjectHandleArray = &handle;
        args.__bindgen_anon_1.MonitoredFenceValueArray = &value;
        // Hand the value to the worker BEFORE the wait is queued: it can only
        // signal what it was given, and the context must never wait for a value
        // nobody will signal.
        let sent = self
            .tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|tx| tx.send(value).is_ok());
        if !sent {
            note_refusal(&NVK_REFUSALS.worker_gone);
            return Err(E_FAIL);
        }
        self.queued.fetch_max(value, Ordering::AcqRel);
        // SAFETY: runtime callback; the queue's own context; arrays outlive it.
        let hr = unsafe { wait_cb(dev.h_rt_device.handle, &args) };
        if hr < 0 {
            note_refusal(&NVK_REFUSALS.gpu_wait_failed);
            if let Some(k) = NVK_LOG.first_n_then_every(16, 4096) {
                log_error!("NVK ECL sync: WaitForSynchronizationObjectFromGpu hr={:#010x} (x{})", hr as u32, k + 1);
            }
            return Err(hr);
        }
        NVK_REFUSALS.gpu_waits.bump();
        Ok(())
    }

    unsafe fn shutdown(&self, dev: &device12::HeliosD3D12Device) {
        self.stop.store(true, Ordering::Release);
        drop(self.tx.lock().unwrap_or_else(|p| p.into_inner()).take());
        if let Some(worker) = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = worker.join();
        }
        // Whatever the worker could not prove, release it now: nothing waits on
        // this context any more (it was destroyed), but a stale wait must not
        // outlive the fence.
        let queued = self.queued.load(Ordering::Acquire);
        if queued != 0 {
            let _ = self.target.signal(queued);
        }
        if !dev.kt_callbacks.is_null() {
            // SAFETY: the device keeps its runtime-owned callback table alive.
            if let Some(destroy) = unsafe { (*dev.kt_callbacks).pfnDestroySynchronizationObjectCb } {
                // SAFETY: zero is valid for this plain C struct.
                let mut args: ddi12::D3DDDICB_DESTROYSYNCHRONIZATIONOBJECT = unsafe { core::mem::zeroed() };
                args.hSyncObject = self.target.fence;
                // SAFETY: runtime callback, runtime device, the fence created above.
                let _ = unsafe { destroy(dev.h_rt_device.handle, &args) };
            }
        }
    }
}

/// The per-queue worker: wait for each handed value on the engine's execution
/// stream, then signal the fence to it, strictly in order. On device loss the
/// value is signalled anyway, so the context never waits for work that will
/// not finish (the device is removed and the runtime reports it).
///
/// ⛔ Never coalesce a burst to its maximum before waiting. Boundary N+1's
/// engine work is released by its admission event, which the runtime queued on
/// the context AFTER the GPU wait for boundary N's fence value. A worker that
/// waits for N+1 first therefore waits for work that cannot start until it
/// signals N: Basemark GPU DX12 deadlocked after its first frame exactly like
/// that (2026-10-06). Values already reached cost one cheap wait each.
fn worker_main(engine_queue: usize, target: SignalTarget, rx: Receiver<u64>, stop: Arc<AtomicBool>) {
    let mut signalled = 0u64;
    while let Ok(value) = rx.recv() {
        if value <= signalled {
            continue;
        }
        loop {
            // SAFETY: the queue joins this worker before releasing its engine queue.
            match unsafe { crate::bridge12::wait_execution(engine_queue, value, WORKER_SLICE_NS) } {
                Ok(true) => break,
                Ok(false) => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                }
                Err(hr) => {
                    note_refusal(&NVK_REFUSALS.worker_engine_error);
                    if let Some(k) = NVK_LOG.first_n_then_every(16, 4096) {
                        log_error!(
                            "NVK ECL worker: engine wait for {value} failed hr={:#010x}; signalling anyway (x{})",
                            hr as u32,
                            k + 1
                        );
                    }
                    break;
                }
            }
        }
        let hr = target.signal(value);
        if hr < 0 {
            note_refusal(&NVK_REFUSALS.cpu_signal_failed);
            if let Some(k) = NVK_LOG.first_n_then_every(16, 4096) {
                log_error!("NVK ECL worker: SignalSynchronizationObjectFromCpu({value}) hr={:#010x} (x{})", hr as u32, k + 1);
            }
        } else {
            NVK_REFUSALS.cpu_signals.bump();
        }
        signalled = value;
    }
}

struct NvkRefusals {
    /// Monitored-fence GPU waits queued on a context.
    gpu_waits: RefusalCounter,
    /// Worker CPU signals of the fence.
    cpu_signals: RefusalCounter,
    /// ECLs that waited on the CPU (CPU-wait arm).
    cpu_waits: RefusalCounter,
    /// The CPU-wait arm was chosen by the knob.
    cpu_wait_arm: RefusalCounter,
    /// The monitored fence could not be created; CPU wait instead. Expected zero.
    fence_create_failed: RefusalCounter,
    /// CPU waits that hit the 2 s cap. Expected zero.
    cpu_wait_timeouts: RefusalCounter,
    /// ECL CPU waits settled by the zero-timeout poll (`Nvk12EclSpinUs`).
    cpu_wait_polled: RefusalCounter,
    /// ECL CPU waits that fell back to the blocking engine wait.
    cpu_wait_blocked: RefusalCounter,
    /// The worker had exited when a value was handed to it. Expected zero.
    worker_gone: RefusalCounter,
    /// WaitForSynchronizationObjectFromGpu refused. Expected zero.
    gpu_wait_failed: RefusalCounter,
    /// The engine wait failed (device loss). Expected zero.
    worker_engine_error: RefusalCounter,
    /// SignalSynchronizationObjectFromCpu refused. Expected zero.
    cpu_signal_failed: RefusalCounter,
}

static NVK_REFUSALS: NvkRefusals = NvkRefusals {
    gpu_waits: RefusalCounter::new("Nvk12GpuWaits"),
    cpu_signals: RefusalCounter::new("Nvk12CpuSignals"),
    cpu_waits: RefusalCounter::new("Nvk12CpuWaits"),
    cpu_wait_arm: RefusalCounter::new("Nvk12CpuWaitArm"),
    fence_create_failed: RefusalCounter::new("Nvk12FenceCreateFailed"),
    cpu_wait_timeouts: RefusalCounter::new("Nvk12CpuWaitTimeouts"),
    cpu_wait_polled: RefusalCounter::new("Nvk12CpuWaitPolled"),
    cpu_wait_blocked: RefusalCounter::new("Nvk12CpuWaitBlocked"),
    worker_gone: RefusalCounter::new("Nvk12WorkerGone"),
    gpu_wait_failed: RefusalCounter::new("Nvk12GpuWaitFailed"),
    worker_engine_error: RefusalCounter::new("Nvk12WorkerEngineError"),
    cpu_signal_failed: RefusalCounter::new("Nvk12CpuSignalFailed"),
};

pub(crate) static REFUSALS: &[&RefusalCounter] = &[
    &NVK_REFUSALS.gpu_waits,
    &NVK_REFUSALS.cpu_signals,
    &NVK_REFUSALS.cpu_waits,
    &NVK_REFUSALS.cpu_wait_arm,
    &NVK_REFUSALS.fence_create_failed,
    &NVK_REFUSALS.cpu_wait_timeouts,
    &NVK_REFUSALS.cpu_wait_polled,
    &NVK_REFUSALS.cpu_wait_blocked,
    &NVK_REFUSALS.worker_gone,
    &NVK_REFUSALS.gpu_wait_failed,
    &NVK_REFUSALS.worker_engine_error,
    &NVK_REFUSALS.cpu_signal_failed,
];
