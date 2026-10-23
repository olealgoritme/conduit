//! The D3D11 threaded front end (`guest/windows/docs/d3d11-threaded-frontend.md`).
//!
//! When enabled for a device, the immediate context's DDI slots are replaced by
//! the wrappers below. A deferred wrapper snapshots its arguments (handles as
//! their identity words, arrays and small payloads by value) and pushes a
//! closure into the device's [`CmdQueue`]; the queue's worker thread later calls
//! the unchanged forward function with pseudo-handles over the snapshot. Calls
//! that must answer now are split (dynamic-buffer maps, GetData, object
//! destruction) or synchronous (drain, then the unchanged forward function on
//! the calling thread). `d3d11_context` drains for any other app-thread path.
//!
//! Nothing here is installed unless [`Frontend`] exists for the device, so with
//! the knob off every table and code path is exactly as before.

use super::*;
use crate::device_funcs::HELIOS_TAG_DEVICE;
use core::mem::size_of;
use helios_umd_common::cmd_queue::CmdQueue;
use helios_umd_common::cmd_record::FrontendSync;
use helios_umd_common::frame_trace::{self, Acc, FrameTracer};
use helios_umd_common::present_window::PresentWindow;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// The per-device object
// ---------------------------------------------------------------------------

/// Why a call took the synchronous path. Indexes [`Stats::sync`].
#[derive(Clone, Copy)]
pub(crate) enum SyncReason {
    Dxgi,
    Map,
    Unmap,
    Update,
    Discard,
    FlushShared,
    DestroyResource,
    CommandList,
    Oversize,
    Keyed,
    SafetyNet,
    Query,
    Destroy,
    Flush,
}

const SYNC_REASONS: [&str; 14] = [
    "dxgi",
    "map",
    "unmap",
    "update",
    "discard",
    "flush_shared",
    "destroy_resource",
    "command_list",
    "oversize",
    "keyed",
    "safety_net",
    "query",
    "destroy",
    "flush",
];

#[derive(Default)]
struct Stats {
    sync: [AtomicU64; 14],
    sync_waited: [AtomicU64; 14],
    split_discard: AtomicU64,
    split_no_overwrite: AtomicU64,
    discard_waits: AtomicU64,
    getdata_early: AtomicU64,
    getdata_hints: AtomicU64,
    errors_reported: AtomicU64,
    errors_dropped: AtomicU64,
    /// The drain at Present: how long the app thread waited for the worker's
    /// tail of the frame. Buckets <50, <200, <500, <1000, <2000, >=2000 µs.
    presents: AtomicU64,
    present_drain_us: AtomicU64,
    present_drain_max_us: AtomicU64,
    present_drain_hist: [AtomicU64; 6],
    /// `UmdPresentAhead`: Presents queued without a drain (their wait for the window is in the
    /// drain figures above), those of them that waited, and rotations queued behind them.
    queued_presents: AtomicU64,
    queued_waits: AtomicU64,
    queued_rotations: AtomicU64,
}

/// Present drains between two summary lines (a process that never destroys
/// its device, like most games at exit, still leaves its counters in the log).
const PRESENTS_PER_SUMMARY: u64 = 8192;

/// How long the render thread spins at Present before it sleeps.
const PRESENT_DRAIN_SPIN: std::time::Duration = std::time::Duration::from_millis(4);

/// Outstanding bytes of discarded dynamic-buffer slices the worker has not yet
/// handed to DXVK (whose `ThrottleDiscard` bounds them from there). Above this
/// the app thread waits for the worker.
const MAX_OUTSTANDING_DISCARD_BYTES: u64 = 64 << 20;

/// An `UpdateSubresourceUP` payload up to this size is copied into the record;
/// larger ones take the synchronous path.
const MAX_UPDATE_PAYLOAD: usize = 64 << 10;

/// One immediate context's threaded front end. Lives in
/// `HeliosDevice::frontend`; created at CreateDevice, shut down first thing in
/// DestroyDevice.
pub(crate) struct Frontend {
    q: Arc<CmdQueue>,
    discard_bytes: AtomicU64,
    stats: Stats,
    /// Call classes `UmdFrontendSync` forces synchronous (A/B isolation).
    force: FrontendSync,
    /// `UmdFrameTrace`: per-Present timing records.
    trace: Option<Tracer>,
    /// `UmdPresentAhead` non-zero: the Present records still queued on the worker. `None` =
    /// every Present drains.
    ahead: Option<std::sync::Mutex<PresentWindow>>,
}

thread_local! {
    /// The front end whose Present drain the DXGI Present running on this thread still owes
    /// (`UmdPresentAhead`): set by the Present wrappers, taken by `present.rs` where the Present
    /// decides between queueing and draining ([`take_owed_present`]).
    static OWED_PRESENT: core::cell::Cell<*const Frontend> =
        const { core::cell::Cell::new(core::ptr::null()) };
}

/// `RtlDllShutdownInProgress`: process teardown has started, other threads
/// are gone, and waiting for the worker would hang.
fn dll_shutdown_in_progress() -> bool {
    type ShutdownFn = unsafe extern "system" fn() -> u8;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    static FN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let f = *FN.get_or_init(|| {
        // SAFETY: ntdll is always loaded; both names are NUL-terminated.
        unsafe {
            let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
            if ntdll.is_null() {
                0
            } else {
                GetProcAddress(ntdll, b"RtlDllShutdownInProgress\0".as_ptr()) as usize
            }
        }
    });
    if f == 0 {
        return false;
    }
    // SAFETY: the export takes no arguments and returns a BOOLEAN.
    unsafe { core::mem::transmute::<usize, ShutdownFn>(f)() != 0 }
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThread() -> *mut c_void;
    fn GetThreadPriority(thread: *mut c_void) -> i32;
    fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
}

// ---------------------------------------------------------------------------
// Per-frame timing trace (`UmdFrameTrace`)
// ---------------------------------------------------------------------------

#[inline(always)]
fn qpc() -> u64 {
    let mut v = 0i64;
    // SAFETY: plain out-parameter.
    unsafe { QueryPerformanceCounter(&mut v) };
    v as u64
}

fn qpc_freq() -> u64 {
    let mut v = 0i64;
    // SAFETY: plain out-parameter.
    unsafe { QueryPerformanceFrequency(&mut v) };
    v.max(1) as u64
}

/// The trace file: `C:\ProgramData\Helios\frametrace-<pid>.csv`, else the
/// sandbox directory beside it.
fn open_trace_file() -> Option<Box<dyn std::io::Write + Send>> {
    let name = format!("frametrace-{}.csv", std::process::id());
    for dir in [r"C:\ProgramData\Helios", r"C:\ProgramData\Helios\sandbox"] {
        let path = std::path::Path::new(dir).join(&name);
        if let Ok(f) = std::fs::File::create(&path) {
            log_error!("frame trace: writing {}", path.display());
            return Some(Box::new(std::io::BufWriter::new(f)));
        }
    }
    log_error!("frame trace: no file could be created; trace off");
    None
}

thread_local! {
    /// Timed whole calls the current thread is inside: their drains are part
    /// of their own time, not of `sync_ms`.
    static TIMED_DEPTH: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// The device's tracer and the counter frequency.
struct Tracer {
    t: FrameTracer,
    freq: u64,
}

impl Tracer {
    fn us_to_ticks(&self, us: u64) -> u64 {
        (u128::from(us) * u128::from(self.freq) / 1_000_000) as u64
    }
}

/// A whole call timed into one category (count + ticks) when tracing.
struct Timed<'a> {
    on: Option<(&'a Tracer, Acc, Acc, u64)>,
}

impl Drop for Timed<'_> {
    fn drop(&mut self) {
        if let Some((t, n, ticks, t0)) = self.on {
            t.t.acc.event(n, ticks, qpc().saturating_sub(t0));
            TIMED_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
}

/// The starting producer's scheduling priority (games often raise their
/// render thread's); the worker takes the same so it is not preempted by the
/// app's own worker threads more than the thread it serves.
fn producer_priority() -> isize {
    // SAFETY: pseudo-handle of the calling thread.
    unsafe { GetThreadPriority(GetCurrentThread()) as isize }
}

fn apply_priority(priority: isize) {
    const THREAD_PRIORITY_ERROR_RETURN: isize = 0x7fff_ffff;
    if priority != THREAD_PRIORITY_ERROR_RETURN {
        // SAFETY: pseudo-handle of the calling (worker) thread.
        unsafe { SetThreadPriority(GetCurrentThread(), priority as i32) };
    }
}

impl Frontend {
    pub(crate) fn new() -> Self {
        let q = CmdQueue::new(crate::knobs::frontend_ring_bytes(), "helios-d3d11-frontend");
        q.set_abandon_check(dll_shutdown_in_progress);
        q.set_thread_init(producer_priority, apply_priority);
        let force = crate::knobs::frontend_sync();
        let ahead = crate::knobs::umd_present_ahead();
        log_error!(
            "D3D11 threaded front end: on (ring {} KiB, synchronous classes: {}, UmdPresentAhead {})",
            q.capacity() / 1024,
            force.names(),
            ahead
        );
        let trace = (crate::knobs::UMD_FRAME_TRACE.get() != 0).then(|| {
            let freq = qpc_freq();
            let ring = frame_trace::register(freq, std::time::Duration::from_millis(250), open_trace_file);
            log_error!("frame trace: device {} traced (QPC {} Hz)", ring.dev, freq);
            Tracer {
                t: FrameTracer::new(ring),
                freq,
            }
        });
        Self {
            q,
            discard_bytes: AtomicU64::new(0),
            stats: Stats::default(),
            force,
            trace,
            ahead: (ahead != 0).then(|| std::sync::Mutex::new(PresentWindow::new(ahead))),
        }
    }

    /// Time the rest of the calling scope as one `n`/`ticks` event (nothing
    /// when not tracing).
    #[inline(always)]
    fn timed(&self, n: Acc, ticks: Acc) -> Timed<'_> {
        Timed {
            on: self.trace.as_ref().map(|t| {
                TIMED_DEPTH.with(|d| d.set(d.get() + 1));
                (t, n, ticks, qpc())
            }),
        }
    }

    /// Drain, run queued destructions, stop the worker. DestroyDevice calls
    /// this before any device state is torn down.
    pub(crate) fn shutdown(&self) {
        self.q.shutdown();
        log_error!("{}", self.summary());
    }

    pub(crate) fn summary(&self) -> String {
        let s = &self.stats;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut sync = String::new();
        for (i, name) in SYNC_REASONS.iter().enumerate() {
            let n = r(&s.sync[i]);
            if n != 0 {
                sync.push_str(&format!(" {name}={n}/{}", r(&s.sync_waited[i])));
            }
        }
        let presents = r(&s.presents).max(1);
        let hist: Vec<String> = s.present_drain_hist.iter().map(|a| r(a).to_string()).collect();
        format!(
            "D3D11 front end: {} | sync(calls/waited):{} | present drain: n={} avg_us={} max_us={} \
             hist(<50/<200/<500/<1000/<2000/more)={} | queued presents={} waited={} rotations={} \
             | discard={} no_overwrite={} discard_waits={} getdata_early={} getdata_hints={} \
             errors={}/{} dropped",
            self.q.summary(),
            sync,
            r(&s.presents),
            r(&s.present_drain_us) / presents,
            r(&s.present_drain_max_us),
            hist.join("/"),
            r(&s.queued_presents),
            r(&s.queued_waits),
            r(&s.queued_rotations),
            r(&s.split_discard),
            r(&s.split_no_overwrite),
            r(&s.discard_waits),
            r(&s.getdata_early),
            r(&s.getdata_hints),
            r(&s.errors_reported),
            r(&s.errors_dropped),
        )
    }

    /// True on this front end's worker thread.
    #[inline(always)]
    pub(crate) fn is_worker(&self) -> bool {
        self.q.is_worker()
    }

    /// Report a stashed worker-side error on the app thread. Only
    /// `D3DDDIERR_DEVICEREMOVED` may be set from an arbitrary immediate-context
    /// DDI; anything else the worker hit is counted and dropped.
    #[inline(always)]
    unsafe fn enter(&self, h: Hdevice) -> bool {
        if let Some(t) = &self.trace {
            t.t.acc.add(Acc::DdiCalls, 1);
        }
        let Some(hr) = self.q.take_error() else {
            return false;
        };
        if hr == crate::hr::D3DDDIERR_DEVICEREMOVED {
            self.stats.errors_reported.fetch_add(1, Ordering::Relaxed);
            set_runtime_error(h, hr);
            true
        } else {
            let n = self.stats.errors_dropped.fetch_add(1, Ordering::Relaxed);
            if n < 16 {
                log_error!("D3D11 front end: worker error 0x{:08x} dropped (x{})", hr as u32, n + 1);
            }
            false
        }
    }

    /// Store an error a deferred forward function raised on the worker.
    pub(crate) fn stash_error(&self, hr: i32) {
        self.q.stash_error(hr, |hr| u8::from(hr == crate::hr::D3DDDIERR_DEVICEREMOVED) + 1);
    }

    /// Drain for a call that takes the synchronous path.
    #[inline(always)]
    pub(crate) fn sync(&self, reason: SyncReason) {
        let i = reason as usize;
        self.stats.sync[i].fetch_add(1, Ordering::Relaxed);
        let t0 = self.trace.as_ref().map(|_| qpc());
        if self.q.drain() {
            self.stats.sync_waited[i].fetch_add(1, Ordering::Relaxed);
        }
        if let (Some(t), Some(t0)) = (&self.trace, t0) {
            if TIMED_DEPTH.with(|d| d.get()) == 0 {
                t.t.acc.event(Acc::SyncN, Acc::SyncTicks, qpc().saturating_sub(t0));
            }
        }
    }

    /// [`sync`](Self::sync), then report a removal the worker hit while
    /// draining. Returns true when the device is removed (the caller then
    /// fails the call, e.g. a Map returns no mapping).
    #[inline(always)]
    unsafe fn sync_enter(&self, h: Hdevice, reason: SyncReason) -> bool {
        unsafe {
            if self.enter(h) {
                return true;
            }
            self.sync(reason);
            self.enter(h)
        }
    }

    /// The drain before a Present, timed; every [`PRESENTS_PER_SUMMARY`]
    /// presents the summary goes to the log.
    pub(crate) unsafe fn present_drain(&self, h: Hdevice) {
        let t0 = std::time::Instant::now();
        let q0 = self.trace.as_ref().map(|_| qpc());
        // Spin (no sleep/wake handshake) for the frame's tail: the worker is
        // normally busy right up to the Present, and the render thread has
        // nothing else to do until it is done.
        unsafe {
            if !self.enter(h) {
                let i = SyncReason::Dxgi as usize;
                self.stats.sync[i].fetch_add(1, Ordering::Relaxed);
                if self.q.drain_spinning(PRESENT_DRAIN_SPIN) {
                    self.stats.sync_waited[i].fetch_add(1, Ordering::Relaxed);
                }
                self.enter(h);
            }
        }
        self.note_present(t0, q0);
    }

    /// `UmdPresentAhead`: does this front end queue eligible Presents instead of draining?
    #[inline(always)]
    pub(crate) fn presents_ahead(&self) -> bool {
        self.ahead.is_some()
    }

    /// Queue `f` (a DXGI call's immediate-context half, `UmdPresentAhead`) behind everything
    /// recorded so far. Returns the position past it (0 when it ran inline).
    ///
    /// # Safety
    /// As every record: `f` runs on the worker with the device alive (destruction drains).
    pub(crate) unsafe fn queue_dxgi(&self, f: impl FnOnce() + 'static) -> u64 {
        unsafe { self.record0(move |_| f()) }
    }

    /// A rotation queued with [`queue_dxgi`](Self::queue_dxgi) (counted only).
    pub(crate) fn note_queued_rotation(&self) {
        self.stats.queued_rotations.fetch_add(1, Ordering::Relaxed);
    }

    /// The end of a Present queued at `pos` instead of drained: report a stashed worker error,
    /// then wait only if more than `UmdPresentAhead` Presents are still queued on the worker.
    /// Timed and traced as the drain it replaces.
    pub(crate) unsafe fn present_queued(&self, h: Hdevice, pos: u64) {
        let t0 = std::time::Instant::now();
        let q0 = self.trace.as_ref().map(|_| qpc());
        unsafe { self.enter(h) };
        self.stats.queued_presents.fetch_add(1, Ordering::Relaxed);
        let wait = self
            .ahead
            .as_ref()
            .and_then(|w| lock_ignore_poison(w).push(pos, self.q.done()));
        if let Some(p) = wait {
            if self.q.wait_done_spinning(p, PRESENT_DRAIN_SPIN) {
                self.stats.queued_waits.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.note_present(t0, q0);
    }

    /// The Present's share of the stats and the frame trace (`present_drain`, `present_queued`).
    fn note_present(&self, t0: std::time::Instant, q0: Option<u64>) {
        let us = t0.elapsed().as_micros() as u64;
        if let (Some(t), Some(q0)) = (&self.trace, q0) {
            let (full_n, full_us) = self.q.full_waits();
            t.t.present(
                q0,
                qpc(),
                full_n,
                t.us_to_ticks(full_us),
                t.us_to_ticks(self.q.worker_busy_us()),
            );
        }
        let s = &self.stats;
        let n = s.presents.fetch_add(1, Ordering::Relaxed) + 1;
        s.present_drain_us.fetch_add(us, Ordering::Relaxed);
        s.present_drain_max_us.fetch_max(us, Ordering::Relaxed);
        let bucket = match us {
            0..=49 => 0,
            50..=199 => 1,
            200..=499 => 2,
            500..=999 => 3,
            1000..=1999 => 4,
            _ => 5,
        };
        s.present_drain_hist[bucket].fetch_add(1, Ordering::Relaxed);
        if n % PRESENTS_PER_SUMMARY == 0 {
            log_error!("{}", self.summary());
        }
    }

    /// The safety net in `d3d11_context`: an app-thread path reached the
    /// immediate context while records are queued.
    #[inline(always)]
    pub(crate) fn order_for_app_thread(&self) {
        // The thread test first: the worker calls this on every forward call,
        // and reading the producer's cursor line there would bounce that line
        // between the two cores once per record.
        if !self.q.is_worker() && self.q.done() != self.q.written() {
            self.sync(SyncReason::SafetyNet);
        }
    }

    /// Push a record. Returns the position past it (0 when it ran inline).
    #[inline(always)]
    unsafe fn record<F: FnOnce(&[u8])>(&self, extra: usize, fill: impl FnOnce(&mut [u8]), f: F) -> u64 {
        if CmdQueue::record_bytes::<F>(extra) > self.q.max_record() {
            // 8-byte aligned, as record bytes are (`Pack` relies on it).
            let mut words = vec![0u64; extra.div_ceil(8)];
            // SAFETY: `words` spans at least `extra` bytes.
            let bytes = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, extra) };
            fill(bytes);
            self.sync(SyncReason::Oversize);
            f(bytes);
            return 0;
        }
        match unsafe { self.q.push(extra, fill, f) } {
            Ok(pos) => {
                if self.force.has(FrontendSync::SERIAL) {
                    self.q.wait_done(pos);
                }
                pos
            }
            // Only after shutdown (the device is being destroyed); the closure
            // is dropped unrun (a discard token releases its slice on drop).
            Err(_) => 0,
        }
    }

    #[inline(always)]
    unsafe fn record0<F: FnOnce(&[u8])>(&self, f: F) -> u64 {
        unsafe { self.record(0, |_| {}, f) }
    }
}

/// The front end of the device behind `h`, when it has one. The wrappers are
/// installed only in device tables, so `h` is a device; the tag check keeps a
/// deferred-context handle from ever being read as one.
#[inline(always)]
pub(crate) unsafe fn fe_of<'a>(h: Hdevice) -> Option<&'a Frontend> {
    let p = h.pDrvPrivate as *const crate::device_funcs::HeliosDevice;
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null device-table handle points at a live private block
    // whose first word is the tag.
    unsafe {
        if (*p).tag != HELIOS_TAG_DEVICE {
            return None;
        }
        (*p).frontend.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Handle snapshots
// ---------------------------------------------------------------------------

use helios_umd_common::cmd_record::{packed, pseudo_handle, snapshot_word, Pack, Unpack, MAX_HANDLES};

/// Most handles one bind DDI passes; larger counts take the synchronous path.
const MAX_BIND_HANDLES: usize = MAX_HANDLES;

/// The identity word behind a handle region (see `cmd_record`).
#[inline(always)]
unsafe fn word(p: *mut c_void) -> usize {
    unsafe { snapshot_word(p) }
}

/// The replay `pDrvPrivate` for a snapshot word held in the caller's frame.
#[inline(always)]
fn pseudo(w: *const usize) -> *mut c_void {
    // SAFETY: callers pass the address of a live local or record word.
    unsafe { pseudo_handle(w) }
}

/// The D3D10/11 DDI handle structs: one `pDrvPrivate` pointer each.
trait Handle: Copy {
    fn p(self) -> *mut c_void;
    fn from_p(p: *mut c_void) -> Self;
}

macro_rules! impl_handle {
    ($($t:ident),+ $(,)?) => {$(
        impl Handle for ddi::$t {
            #[inline(always)]
            fn p(self) -> *mut c_void {
                self.pDrvPrivate
            }
            #[inline(always)]
            fn from_p(p: *mut c_void) -> Self {
                Self { pDrvPrivate: p }
            }
        }
        // Arrays of these are read and built as arrays of `pDrvPrivate`.
        const _: () = assert!(size_of::<ddi::$t>() == size_of::<*mut c_void>());
    )+};
}

impl_handle!(
    D3D10DDI_HRESOURCE,
    D3D10DDI_HSHADERRESOURCEVIEW,
    D3D10DDI_HRENDERTARGETVIEW,
    D3D10DDI_HDEPTHSTENCILVIEW,
    D3D11DDI_HUNORDEREDACCESSVIEW,
    D3D10DDI_HSAMPLER,
    D3D10DDI_HSHADER,
    D3D10DDI_HELEMENTLAYOUT,
    D3D10DDI_HBLENDSTATE,
    D3D10DDI_HDEPTHSTENCILSTATE,
    D3D10DDI_HRASTERIZERSTATE,
    D3D10DDI_HQUERY,
    D3D11DDI_HCOMMANDLIST,
);

/// Snapshot of one handle argument.
#[inline(always)]
unsafe fn snap<H: Handle>(h: H) -> usize {
    unsafe { word(h.p()) }
}

/// The pseudo-handle for a snapshot word held in the caller's frame.
#[inline(always)]
fn unsnap<H: Handle>(w: &usize) -> H {
    H::from_p(pseudo(w))
}

/// Pack the snapshot words of `n` handles of type `H`.
#[inline(always)]
unsafe fn pack_handles<H: Handle>(p: &mut Pack<'_>, src: *const H, n: usize) {
    unsafe { p.handles(src.cast::<*mut c_void>(), n) }
}

/// Run `f` with an array of `n` pseudo-handles over `n` snapshot words, or a
/// null array when the original was null.
#[inline(always)]
unsafe fn with_handles<H: Handle, R>(
    words: *const usize,
    n: usize,
    was_null: bool,
    f: impl FnOnce(*const H) -> R,
) -> R {
    unsafe {
        helios_umd_common::cmd_record::with_pseudo_handles(words, n, was_null, |p| f(p.cast::<H>()))
    }
}

/// Copy four values behind a possibly-null pointer.
#[inline(always)]
unsafe fn four<T: Copy + Default>(p: *const T) -> ([T; 4], bool) {
    if p.is_null() {
        ([T::default(); 4], true)
    } else {
        unsafe { ([*p, *p.add(1), *p.add(2), *p.add(3)], false) }
    }
}

// ---------------------------------------------------------------------------
// Deferred wrappers: scalars and single handles
// ---------------------------------------------------------------------------

/// A wrapper whose arguments are all plain values: record the call as is.
macro_rules! defer_plain {
    ($name:ident => $orig:ident ( $($a:ident : $t:ty),* )) => {
        unsafe extern "system" fn $name(h: Hdevice $(, $a: $t)*) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h $(, $a)*) };
            };
            unsafe {
                fe.enter(h);
                fe.record0(move |_| $orig(h $(, $a)*));
            }
        }
    };
}

defer_plain!(fe_draw => draw(vertex_count: u32, start_vertex: u32));
defer_plain!(fe_draw_indexed => draw_indexed(index_count: u32, start_index: u32, base_vertex: i32));
defer_plain!(fe_draw_instanced => draw_instanced(a: u32, b: u32, c: u32, d: u32));
defer_plain!(fe_draw_indexed_instanced => draw_indexed_instanced(a: u32, b: u32, c: u32, d: i32, e: u32));
defer_plain!(fe_draw_auto => draw_auto());
defer_plain!(fe_dispatch => dispatch(x: u32, y: u32, z: u32));
defer_plain!(fe_ia_set_topology => ia_set_topology(topo: ddi::D3D10_DDI_PRIMITIVE_TOPOLOGY));

/// A wrapper whose first argument after the device is one handle, followed
/// by plain values.
macro_rules! defer_handle {
    ($name:ident => $orig:ident ( $hx:ident : $ht:ident $(, $a:ident : $t:ty)* )) => {
        unsafe extern "system" fn $name(h: Hdevice, $hx: ddi::$ht $(, $a: $t)*) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h, $hx $(, $a)*) };
            };
            unsafe {
                fe.enter(h);
                let w = snap($hx);
                fe.record0(move |_| {
                    let w = w;
                    $orig(h, unsnap::<ddi::$ht>(&w) $(, $a)*)
                });
            }
        }
    };
}

defer_handle!(fe_vs_set_shader => vs_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_ps_set_shader => ps_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_gs_set_shader => gs_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_hs_set_shader => hs_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_ds_set_shader => ds_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_cs_set_shader => cs_set_shader(s: D3D10DDI_HSHADER));
defer_handle!(fe_ia_set_input_layout => ia_set_input_layout(l: D3D10DDI_HELEMENTLAYOUT));
defer_handle!(fe_ia_set_index_buffer => ia_set_index_buffer(b: D3D10DDI_HRESOURCE, format: ddi::DXGI_FORMAT, offset: u32));
defer_handle!(fe_set_rasterizer_state => set_rasterizer_state(s: D3D10DDI_HRASTERIZERSTATE));
defer_handle!(fe_set_depth_stencil_state => set_depth_stencil_state(s: D3D10DDI_HDEPTHSTENCILSTATE, stencil_ref: u32));
defer_handle!(fe_clear_dsv => clear_dsv(v: D3D10DDI_HDEPTHSTENCILVIEW, flags: u32, depth: f32, stencil: u8));
defer_handle!(fe_draw_instanced_indirect => draw_instanced_indirect(b: D3D10DDI_HRESOURCE, offset: u32));
defer_handle!(fe_draw_indexed_instanced_indirect => draw_indexed_instanced_indirect(b: D3D10DDI_HRESOURCE, offset: u32));
defer_handle!(fe_dispatch_indirect => dispatch_indirect(b: D3D10DDI_HRESOURCE, offset: u32));
defer_handle!(fe_set_resource_min_lod => set_resource_min_lod(r: D3D10DDI_HRESOURCE, min_lod: f32));
defer_handle!(fe_gen_mips => gen_mips(v: D3D10DDI_HSHADERRESOURCEVIEW));
defer_handle!(fe_set_predication => set_predication(q: D3D10DDI_HQUERY, value: i32));

unsafe extern "system" fn fe_resource_resolve_subresource(
    h: Hdevice,
    dst: ddi::D3D10DDI_HRESOURCE,
    dst_sub: u32,
    src: ddi::D3D10DDI_HRESOURCE,
    src_sub: u32,
    format: ddi::DXGI_FORMAT,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_resolve_subresource(h, dst, dst_sub, src, src_sub, format) };
    };
    unsafe {
        fe.enter(h);
        let (wd, ws) = (snap(dst), snap(src));
        fe.record0(move |_| {
            let (wd, ws) = (wd, ws);
            resource_resolve_subresource(h, unsnap(&wd), dst_sub, unsnap(&ws), src_sub, format)
        });
    }
}

/// `WithIfaces` setters: class instances are not implemented and the
/// forward functions ignore the three extra arguments, so the record is the
/// plain setter's.
macro_rules! defer_shader_ifaces {
    ($name:ident => $plain:ident) => {
        unsafe extern "system" fn $name(
            h: Hdevice,
            s: ddi::D3D10DDI_HSHADER,
            _num_class_instances: u32,
            _class_instance_ids: *const u32,
            _pointer_data: *const ddi::D3D11DDIARG_POINTERDATA,
        ) {
            unsafe { $plain(h, s) }
        }
    };
}

defer_shader_ifaces!(fe_vs_set_shader_with_ifaces => fe_vs_set_shader);
defer_shader_ifaces!(fe_ps_set_shader_with_ifaces => fe_ps_set_shader);
defer_shader_ifaces!(fe_gs_set_shader_with_ifaces => fe_gs_set_shader);
defer_shader_ifaces!(fe_hs_set_shader_with_ifaces => fe_hs_set_shader);
defer_shader_ifaces!(fe_ds_set_shader_with_ifaces => fe_ds_set_shader);
defer_shader_ifaces!(fe_cs_set_shader_with_ifaces => fe_cs_set_shader);

/// Copies take two handles; the copy-region forms add plain values and a box.
unsafe extern "system" fn fe_resource_copy(h: Hdevice, dst: ddi::D3D10DDI_HRESOURCE, src: ddi::D3D10DDI_HRESOURCE) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_copy(h, dst, src) };
    };
    unsafe {
        fe.enter(h);
        let (wd, ws) = (snap(dst), snap(src));
        fe.record0(move |_| {
            let (wd, ws) = (wd, ws);
            resource_copy(h, unsnap(&wd), unsnap(&ws))
        });
    }
}

unsafe fn box_copy(b: *const ddi::D3D10_DDI_BOX) -> Option<ddi::D3D10_DDI_BOX> {
    (!b.is_null()).then(|| unsafe { *b })
}

fn box_ptr(b: &Option<ddi::D3D10_DDI_BOX>) -> *const ddi::D3D10_DDI_BOX {
    b.as_ref().map_or(core::ptr::null(), |b| b as *const _)
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn fe_resource_copy_region(
    h: Hdevice,
    dst: ddi::D3D10DDI_HRESOURCE,
    dst_sub: u32,
    x: u32,
    y: u32,
    z: u32,
    src: ddi::D3D10DDI_HRESOURCE,
    src_sub: u32,
    bx: *const ddi::D3D10_DDI_BOX,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_copy_region(h, dst, dst_sub, x, y, z, src, src_sub, bx) };
    };
    unsafe {
        fe.enter(h);
        let (wd, ws, b) = (snap(dst), snap(src), box_copy(bx));
        fe.record0(move |_| {
            let (wd, ws, b) = (wd, ws, b);
            resource_copy_region(h, unsnap(&wd), dst_sub, x, y, z, unsnap(&ws), src_sub, box_ptr(&b))
        });
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn fe_resource_copy_region_11_1(
    h: Hdevice,
    dst: ddi::D3D10DDI_HRESOURCE,
    dst_sub: u32,
    x: u32,
    y: u32,
    z: u32,
    src: ddi::D3D10DDI_HRESOURCE,
    src_sub: u32,
    bx: *const ddi::D3D10_DDI_BOX,
    flags: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_copy_region_11_1(h, dst, dst_sub, x, y, z, src, src_sub, bx, flags) };
    };
    unsafe {
        fe.enter(h);
        let (wd, ws, b) = (snap(dst), snap(src), box_copy(bx));
        fe.record0(move |_| {
            let (wd, ws, b) = (wd, ws, b);
            resource_copy_region_11_1(h, unsnap(&wd), dst_sub, x, y, z, unsnap(&ws), src_sub, box_ptr(&b), flags)
        });
    }
}

unsafe extern "system" fn fe_copy_structure_count(
    h: Hdevice,
    dst: ddi::D3D10DDI_HRESOURCE,
    offset: u32,
    src: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { copy_structure_count(h, dst, offset, src) };
    };
    unsafe {
        fe.enter(h);
        let (wd, ws) = (snap(dst), snap(src));
        fe.record0(move |_| {
            let (wd, ws) = (wd, ws);
            copy_structure_count(h, unsnap(&wd), offset, unsnap(&ws))
        });
    }
}

// --- clears and state with small fixed arrays --------------------------------

unsafe extern "system" fn fe_clear_rtv(h: Hdevice, v: ddi::D3D10DDI_HRENDERTARGETVIEW, color: *mut f32) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { clear_rtv(h, v, color) };
    };
    unsafe {
        fe.enter(h);
        let (w, (c, null)) = (snap(v), four(color as *const f32));
        fe.record0(move |_| {
            let (w, mut c) = (w, c);
            let p = if null { core::ptr::null_mut() } else { c.as_mut_ptr() };
            clear_rtv(h, unsnap(&w), p)
        });
    }
}

unsafe extern "system" fn fe_clear_uav_uint(h: Hdevice, v: ddi::D3D11DDI_HUNORDEREDACCESSVIEW, values: *const u32) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { clear_uav_uint(h, v, values) };
    };
    unsafe {
        fe.enter(h);
        let (w, (c, null)) = (snap(v), four(values));
        fe.record0(move |_| {
            let (w, c) = (w, c);
            clear_uav_uint(h, unsnap(&w), if null { core::ptr::null() } else { c.as_ptr() })
        });
    }
}

unsafe extern "system" fn fe_clear_uav_float(h: Hdevice, v: ddi::D3D11DDI_HUNORDEREDACCESSVIEW, values: *const f32) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { clear_uav_float(h, v, values) };
    };
    unsafe {
        fe.enter(h);
        let (w, (c, null)) = (snap(v), four(values));
        fe.record0(move |_| {
            let (w, c) = (w, c);
            clear_uav_float(h, unsnap(&w), if null { core::ptr::null() } else { c.as_ptr() })
        });
    }
}

unsafe extern "system" fn fe_set_blend_state(
    h: Hdevice,
    s: ddi::D3D10DDI_HBLENDSTATE,
    factor: *const f32,
    sample_mask: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { set_blend_state(h, s, factor, sample_mask) };
    };
    unsafe {
        fe.enter(h);
        let (w, (f, null)) = (snap(s), four(factor));
        fe.record0(move |_| {
            let (w, f) = (w, f);
            set_blend_state(h, unsnap(&w), if null { core::ptr::null() } else { f.as_ptr() }, sample_mask)
        });
    }
}

// --- arrays --------------------------------------------------------------------

/// The `pfn*SetConstantBuffers` / `SetShaderResources` / `SetSamplers`
/// shape: `(start, num, handles*)`.
macro_rules! defer_bind {
    ($name:ident => $orig:ident, $ht:ident) => {
        unsafe extern "system" fn $name(h: Hdevice, start: u32, num: u32, hs: *const ddi::$ht) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h, start, num, hs) };
            };
            unsafe {
                fe.enter(h);
                let n = num as usize;
                if n > MAX_BIND_HANDLES {
                    fe.sync(SyncReason::Oversize);
                    return $orig(h, start, num, hs);
                }
                let null = hs.is_null();
                fe.record(
                    packed::<usize>(n),
                    |b| pack_handles(&mut Pack::new(b), hs, n),
                    move |b| {
                        let words = Unpack::new(b).words(n);
                        with_handles::<ddi::$ht, _>(words, n, null, |p| $orig(h, start, num, p))
                    },
                );
            }
        }
    };
}

defer_bind!(fe_vs_set_constant_buffers => vs_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_ps_set_constant_buffers => ps_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_gs_set_constant_buffers => gs_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_hs_set_constant_buffers => hs_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_ds_set_constant_buffers => ds_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_cs_set_constant_buffers => cs_set_constant_buffers, D3D10DDI_HRESOURCE);
defer_bind!(fe_vs_set_shader_resources => vs_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_ps_set_shader_resources => ps_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_gs_set_shader_resources => gs_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_hs_set_shader_resources => hs_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_ds_set_shader_resources => ds_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_cs_set_shader_resources => cs_set_shader_resources, D3D10DDI_HSHADERRESOURCEVIEW);
defer_bind!(fe_vs_set_samplers => vs_set_samplers, D3D10DDI_HSAMPLER);
defer_bind!(fe_ps_set_samplers => ps_set_samplers, D3D10DDI_HSAMPLER);
defer_bind!(fe_gs_set_samplers => gs_set_samplers, D3D10DDI_HSAMPLER);
defer_bind!(fe_hs_set_samplers => hs_set_samplers, D3D10DDI_HSAMPLER);
defer_bind!(fe_ds_set_samplers => ds_set_samplers, D3D10DDI_HSAMPLER);
defer_bind!(fe_cs_set_samplers => cs_set_samplers, D3D10DDI_HSAMPLER);

/// `pfn*SetConstantBuffers` of the >= 11.1 tables: `(start, num, buffers*,
/// firstConstant*, numConstants*)`.
macro_rules! defer_cb1 {
    ($name:ident => $stage:ident) => {
        unsafe extern "system" fn $name(
            h: Hdevice,
            start: u32,
            num: u32,
            bufs: *const ddi::D3D10DDI_HRESOURCE,
            first: *const u32,
            count: *const u32,
        ) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { set_constant_buffers1_common(h, ShaderStage::$stage, start, num, bufs, first, count) };
            };
            unsafe {
                fe.enter(h);
                let n = num as usize;
                if n > MAX_BIND_HANDLES {
                    fe.sync(SyncReason::Oversize);
                    return set_constant_buffers1_common(h, ShaderStage::$stage, start, num, bufs, first, count);
                }
                let (nb, nf, nc) = (bufs.is_null(), first.is_null(), count.is_null());
                fe.record(
                    packed::<usize>(n) + 2 * packed::<u32>(n),
                    |b| {
                        let mut p = Pack::new(b);
                        pack_handles(&mut p, bufs, n);
                        p.copy(first, n);
                        p.copy(count, n);
                    },
                    move |b| {
                        let mut u = Unpack::new(b);
                        let words = u.words(n);
                        let first = u.array::<u32>(n, nf);
                        let count = u.array::<u32>(n, nc);
                        with_handles::<ddi::D3D10DDI_HRESOURCE, _>(words, n, nb, |p| {
                            set_constant_buffers1_common(h, ShaderStage::$stage, start, num, p, first, count)
                        })
                    },
                );
            }
        }
    };
}

defer_cb1!(fe_vs_set_constant_buffers1 => Vs);
defer_cb1!(fe_ps_set_constant_buffers1 => Ps);
defer_cb1!(fe_gs_set_constant_buffers1 => Gs);
defer_cb1!(fe_hs_set_constant_buffers1 => Hs);
defer_cb1!(fe_ds_set_constant_buffers1 => Ds);
defer_cb1!(fe_cs_set_constant_buffers1 => Cs);

unsafe extern "system" fn fe_ia_set_vertex_buffers(
    h: Hdevice,
    start: u32,
    num: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    strides: *const u32,
    offsets: *const u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { ia_set_vertex_buffers(h, start, num, bufs, strides, offsets) };
    };
    unsafe {
        fe.enter(h);
        let n = num as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return ia_set_vertex_buffers(h, start, num, bufs, strides, offsets);
        }
        let (nb, ns, no) = (bufs.is_null(), strides.is_null(), offsets.is_null());
        fe.record(
            packed::<usize>(n) + 2 * packed::<u32>(n),
            |b| {
                let mut p = Pack::new(b);
                pack_handles(&mut p, bufs, n);
                p.copy(strides, n);
                p.copy(offsets, n);
            },
            move |b| {
                let mut u = Unpack::new(b);
                let words = u.words(n);
                let strides = u.array::<u32>(n, ns);
                let offsets = u.array::<u32>(n, no);
                with_handles::<ddi::D3D10DDI_HRESOURCE, _>(words, n, nb, |p| {
                    ia_set_vertex_buffers(h, start, num, p, strides, offsets)
                })
            },
        );
    }
}

unsafe extern "system" fn fe_so_set_targets(
    h: Hdevice,
    num: u32,
    clear: u32,
    bufs: *const ddi::D3D10DDI_HRESOURCE,
    offsets: *const u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { so_set_targets(h, num, clear, bufs, offsets) };
    };
    unsafe {
        fe.enter(h);
        let n = num as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return so_set_targets(h, num, clear, bufs, offsets);
        }
        let (nb, no) = (bufs.is_null(), offsets.is_null());
        fe.record(
            packed::<usize>(n) + packed::<u32>(n),
            |b| {
                let mut p = Pack::new(b);
                pack_handles(&mut p, bufs, n);
                p.copy(offsets, n);
            },
            move |b| {
                let mut u = Unpack::new(b);
                let words = u.words(n);
                let offsets = u.array::<u32>(n, no);
                with_handles::<ddi::D3D10DDI_HRESOURCE, _>(words, n, nb, |p| so_set_targets(h, num, clear, p, offsets))
            },
        );
    }
}

unsafe extern "system" fn fe_cs_set_uavs(
    h: Hdevice,
    start: u32,
    num: u32,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    counts: *const u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { cs_set_uavs(h, start, num, uavs, counts) };
    };
    unsafe {
        fe.enter(h);
        let n = num as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return cs_set_uavs(h, start, num, uavs, counts);
        }
        let (nu, nc) = (uavs.is_null(), counts.is_null());
        fe.record(
            packed::<usize>(n) + packed::<u32>(n),
            |b| {
                let mut p = Pack::new(b);
                pack_handles(&mut p, uavs, n);
                p.copy(counts, n);
            },
            move |b| {
                let mut u = Unpack::new(b);
                let words = u.words(n);
                let counts = u.array::<u32>(n, nc);
                with_handles::<ddi::D3D11DDI_HUNORDEREDACCESSVIEW, _>(words, n, nu, |p| {
                    cs_set_uavs(h, start, num, p, counts)
                })
            },
        );
    }
}

unsafe extern "system" fn fe_set_viewports(h: Hdevice, num: u32, clear: u32, vps: *const ddi::D3D10_DDI_VIEWPORT) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { set_viewports(h, num, clear, vps) };
    };
    unsafe {
        fe.enter(h);
        let n = num as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return set_viewports(h, num, clear, vps);
        }
        let null = vps.is_null();
        fe.record(
            packed::<ddi::D3D10_DDI_VIEWPORT>(n),
            |b| Pack::new(b).copy(vps, n),
            move |b| set_viewports(h, num, clear, Unpack::new(b).array(n, null)),
        );
    }
}

unsafe extern "system" fn fe_set_scissor_rects(h: Hdevice, num: u32, clear: u32, rects: *const ddi::D3D10_DDI_RECT) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { set_scissor_rects(h, num, clear, rects) };
    };
    unsafe {
        fe.enter(h);
        let n = num as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return set_scissor_rects(h, num, clear, rects);
        }
        let null = rects.is_null();
        fe.record(
            packed::<ddi::D3D10_DDI_RECT>(n),
            |b| Pack::new(b).copy(rects, n),
            move |b| set_scissor_rects(h, num, clear, Unpack::new(b).array(n, null)),
        );
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn fe_set_render_targets(
    h: Hdevice,
    rtvs: *const ddi::D3D10DDI_HRENDERTARGETVIEW,
    num_views: u32,
    clear_slots: u32,
    dsv: ddi::D3D10DDI_HDEPTHSTENCILVIEW,
    uavs: *const ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    uav_counts: *const u32,
    uav_start: u32,
    num_uavs: u32,
    uav_range_start: u32,
    uav_range_size: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe {
            set_render_targets(
                h, rtvs, num_views, clear_slots, dsv, uavs, uav_counts, uav_start, num_uavs, uav_range_start,
                uav_range_size,
            )
        };
    };
    unsafe {
        fe.enter(h);
        let (nr, nu) = (num_views as usize, num_uavs as usize);
        if nr > MAX_BIND_HANDLES || nu > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return set_render_targets(
                h, rtvs, num_views, clear_slots, dsv, uavs, uav_counts, uav_start, num_uavs, uav_range_start,
                uav_range_size,
            );
        }
        let (null_r, null_u, null_c) = (rtvs.is_null(), uavs.is_null(), uav_counts.is_null());
        let wd = snap(dsv);
        fe.record(
            packed::<usize>(nr) + packed::<usize>(nu) + packed::<u32>(nu),
            |b| {
                let mut p = Pack::new(b);
                pack_handles(&mut p, rtvs, nr);
                pack_handles(&mut p, uavs, nu);
                p.copy(uav_counts, nu);
            },
            move |b| {
                let wd = wd;
                let mut u = Unpack::new(b);
                let rw = u.words(nr);
                let uw = u.words(nu);
                let counts = u.array::<u32>(nu, null_c);
                with_handles::<ddi::D3D10DDI_HRENDERTARGETVIEW, _>(rw, nr, null_r, |r| {
                    with_handles::<ddi::D3D11DDI_HUNORDEREDACCESSVIEW, _>(uw, nu, null_u, |uv| {
                        set_render_targets(
                            h,
                            r,
                            num_views,
                            clear_slots,
                            unsnap(&wd),
                            uv,
                            counts,
                            uav_start,
                            num_uavs,
                            uav_range_start,
                            uav_range_size,
                        )
                    })
                })
            },
        );
    }
}

// --- D3D11.1 views by runtime tag ------------------------------------------------

/// `Discard`: a resource discard of a CPU-visible buffer goes through DXVK's
/// `Map(DISCARD)` and so must not run on the worker while the app thread owns
/// dynamic-buffer maps; it is synchronous. Rect discards are dropped by the
/// forward function before it touches the context, so they run inline.
unsafe extern "system" fn fe_discard_11_1(
    h: Hdevice,
    handle_type: ddi::D3D11DDI_HANDLETYPE,
    handle: *mut c_void,
    rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { discard_11_1(h, handle_type, handle, rects, num_rects) };
    };
    unsafe {
        fe.enter(h);
        if num_rects != 0 {
            return discard_11_1(h, handle_type, handle, rects, num_rects);
        }
        if handle_type == ddi::D3D11DDI_HANDLETYPE_D3D10DDI_HT_RESOURCE
            && resource_state_at(handle).is_some_and(|s| s.buffer_raw != 0)
        {
            fe.sync(SyncReason::Discard);
            return discard_11_1(h, handle_type, handle, rects, num_rects);
        }
        let w = word(handle);
        fe.record0(move |_| {
            let w = w;
            discard_11_1(h, handle_type, pseudo(&w), core::ptr::null(), 0)
        });
    }
}

unsafe extern "system" fn fe_clear_view_11_1(
    h: Hdevice,
    view_type: ddi::D3D11DDI_HANDLETYPE,
    view: *mut c_void,
    color: *const f32,
    rects: *const ddi::D3D10_DDI_RECT,
    num_rects: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { clear_view_11_1(h, view_type, view, color, rects, num_rects) };
    };
    unsafe {
        fe.enter(h);
        let n = num_rects as usize;
        if n > MAX_BIND_HANDLES {
            fe.sync(SyncReason::Oversize);
            return clear_view_11_1(h, view_type, view, color, rects, num_rects);
        }
        let (w, (c, null_c), null_r) = (word(view), four(color), rects.is_null());
        fe.record(
            packed::<ddi::D3D10_DDI_RECT>(n),
            |b| Pack::new(b).copy(rects, n),
            move |b| {
                let (w, c) = (w, c);
                let r = Unpack::new(b).array::<ddi::D3D10_DDI_RECT>(n, null_r);
                clear_view_11_1(h, view_type, pseudo(&w), if null_c { core::ptr::null() } else { c.as_ptr() }, r, num_rects)
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Begin and End both stamp the query's position: GetData answers "still
/// drawing" until the worker has replayed the latest of them, so DXVK never
/// sees a GetData between a queued Begin and its End.
macro_rules! defer_query {
    ($name:ident => $orig:ident) => {
        unsafe extern "system" fn $name(h: Hdevice, q: ddi::D3D10DDI_HQUERY) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h, q) };
            };
            unsafe {
                fe.enter(h);
                let w = snap(q);
                let pos = fe.record0(move |_| {
                    let w = w;
                    $orig(h, unsnap(&w))
                });
                set_query_end_pos(q, pos);
            }
        }
    };
}

defer_query!(fe_query_begin => query_begin);
defer_query!(fe_query_end => query_end);

/// `pfnQueryGetData`, split: answered on the app thread.
unsafe extern "system" fn fe_query_get_data(
    h: Hdevice,
    q: ddi::D3D10DDI_HQUERY,
    data: *mut c_void,
    data_size: u32,
    flags: u32,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { query_get_data(h, q, data, data_size, flags) };
    };
    let _t = fe.timed(Acc::GetDataN, Acc::GetDataTicks);
    unsafe {
        // A stashed device removal wins over "still drawing".
        if fe.enter(h) {
            return;
        }
        if fe.force.has(FrontendSync::QUERIES) {
            if !fe.sync_enter(h, SyncReason::Query) {
                query_get_data(h, q, data, data_size, flags);
            }
            return;
        }
        let unflagged = flags & (D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32) == 0;
        if query_end_pos(q) > fe.q.done() {
            fe.stats.getdata_early.fetch_add(1, Ordering::Relaxed);
            fe.q.kick();
            if unflagged {
                query_hint(fe, h, q);
            }
            set_runtime_error(h, crate::hr::DXGI_DDI_ERR_WASSTILLDRAWING);
            return;
        }
        let Some(context) = helios_device(h).and_then(|d| d.dxvk.d3d11_context()) else {
            return query_get_data(h, q, data, data_size, flags);
        };
        let outcome = query_get_data_on(
            h,
            &context,
            q,
            data,
            data_size,
            flags | D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        );
        if outcome == GetDataOutcome::Pending && unflagged {
            query_hint(fe, h, q);
        }
    }
}

/// Queue the flush an unflagged pending GetData asks DXVK for, at most one
/// outstanding per query: a new one only once the worker ran the last.
unsafe fn query_hint(fe: &Frontend, h: Hdevice, q: ddi::D3D10DDI_HQUERY) {
    unsafe {
        if query_hint_pos(q) > fe.q.done() {
            return;
        }
        fe.stats.getdata_hints.fetch_add(1, Ordering::Relaxed);
        let w = snap(q);
        let pos = fe.record0(move |_| query_flush_hint(h, w));
        set_query_hint_pos(q, pos);
        fe.q.kick();
    }
}

// ---------------------------------------------------------------------------
// Map / Unmap / UpdateSubresource
// ---------------------------------------------------------------------------

const DDI_MAP_WRITE_DISCARD: ddi::D3D10_DDI_MAP = 4;
const DDI_MAP_WRITE_NOOVERWRITE: ddi::D3D10_DDI_MAP = 5;

unsafe fn write_mapped(mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE, data: usize, pitch: u32) {
    if !mapped.is_null() {
        unsafe {
            (*mapped).pData = data as *mut c_void;
            (*mapped).RowPitch = pitch;
            (*mapped).DepthPitch = pitch;
        }
    }
}

/// A discarded slice on its way to the worker. `emit` hands it to DXVK in
/// replay order; a record dropped unrun (queue already shut down) releases it.
struct DiscardSlice {
    h: Hdevice,
    buffer: usize,
    slice: usize,
}

impl DiscardSlice {
    /// `park`: let the worker pre-allocate the buffer's next slice.
    unsafe fn emit(self, park: bool) {
        let this = core::mem::ManuallyDrop::new(self);
        if let Some(dev) = unsafe { helios_device(this.h) } {
            unsafe { dev.dxvk.fe_emit_invalidate(this.buffer, this.slice, park) };
        }
    }
}

impl Drop for DiscardSlice {
    fn drop(&mut self) {
        // SAFETY: the token was never consumed by `emit`.
        if let Some(dev) = unsafe { helios_device(self.h) } {
            unsafe { dev.dxvk.fe_release_slice(self.slice) };
        }
    }
}

/// The dynamic-map slots (`Dynamic{ConstantBuffer,IABuffer,Resource}Map*`):
/// a dynamic buffer's DISCARD / NO_OVERWRITE map is answered on the app
/// thread; everything else is synchronous.
unsafe extern "system" fn fe_map_dynamic(
    h: Hdevice,
    res: ddi::D3D10DDI_HRESOURCE,
    sub: u32,
    map_type: ddi::D3D10_DDI_MAP,
    flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_map(h, res, sub, map_type, flags, mapped) };
    };
    unsafe {
        if fe.enter(h) {
            write_mapped(mapped, 0, 0);
            return;
        }
        let split = !fe.force.has(FrontendSync::MAPS);
        if let (true, Some(st), Some(dev)) = (split, resource_state(res), helios_device(h)) {
            if st.dynamic_buffer && st.buffer_raw != 0 && sub == 0 {
                let buffer = st.buffer_raw;
                let bytes = st.byte_width;
                if map_type == DDI_MAP_WRITE_DISCARD {
                    match dev.dxvk.fe_map_discard(buffer) {
                        crate::bridge::FrontendDiscard::Slice { slice, data } => {
                            fe.stats.split_discard.fetch_add(1, Ordering::Relaxed);
                            write_mapped(mapped, data, bytes);
                            let out = fe.discard_bytes.fetch_add(u64::from(bytes), Ordering::Relaxed) + u64::from(bytes);
                            let token = DiscardSlice { h, buffer, slice };
                            let park = !fe.force.has(FrontendSync::NO_PARK);
                            fe.record0(move |_| {
                                token.emit(park);
                                if let Some(fe) = fe_of(h) {
                                    fe.discard_bytes.fetch_sub(u64::from(bytes), Ordering::Relaxed);
                                }
                            });
                            if out > MAX_OUTSTANDING_DISCARD_BYTES {
                                fe.stats.discard_waits.fetch_add(1, Ordering::Relaxed);
                                if let Some(t) = &fe.trace {
                                    t.t.acc.add(Acc::DiscardWaitN, 1);
                                }
                                let _t = fe.timed(Acc::SyncN, Acc::SyncTicks);
                                fe.q.drain();
                            }
                            return;
                        }
                        crate::bridge::FrontendDiscard::DeviceLost => {
                            write_mapped(mapped, 0, 0);
                            set_runtime_error(h, crate::hr::D3DDDIERR_DEVICEREMOVED);
                            return;
                        }
                        crate::bridge::FrontendDiscard::NotMappable => {}
                    }
                } else if map_type == DDI_MAP_WRITE_NOOVERWRITE {
                    let data = dev.dxvk.fe_buffer_map_ptr(buffer);
                    if data != 0 {
                        fe.stats.split_no_overwrite.fetch_add(1, Ordering::Relaxed);
                        write_mapped(mapped, data, bytes);
                        return;
                    }
                }
            }
        }
        let _t = fe.timed(Acc::MapN, Acc::MapTicks);
        if fe.sync_enter(h, SyncReason::Map) {
            write_mapped(mapped, 0, 0);
            return;
        }
        resource_map(h, res, sub, map_type, flags, mapped)
    }
}

/// `DynamicConstantBufferMapNoOverwrite` (>= 11.1 tables).
unsafe extern "system" fn fe_dynamic_cb_map_no_overwrite(
    h: Hdevice,
    res: ddi::D3D10DDI_HRESOURCE,
    sub: u32,
    map_type: ddi::D3D10_DDI_MAP,
    flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    if unsafe { fe_of(h) }.is_none() {
        return unsafe { dynamic_cb_map_no_overwrite(h, res, sub, map_type, flags, mapped) };
    }
    unsafe { fe_map_dynamic(h, res, sub, map_type, flags, mapped) }
}

/// Every other map slot (`ResourceMap`, `StagingResourceMap`): synchronous.
unsafe extern "system" fn fe_map_sync(
    h: Hdevice,
    res: ddi::D3D10DDI_HRESOURCE,
    sub: u32,
    map_type: ddi::D3D10_DDI_MAP,
    flags: u32,
    mapped: *mut ddi::D3D10DDI_MAPPED_SUBRESOURCE,
) {
    let fe = unsafe { fe_of(h) };
    let _t = fe.map(|fe| fe.timed(Acc::MapN, Acc::MapTicks));
    if let Some(fe) = fe {
        if unsafe { fe.sync_enter(h, SyncReason::Map) } {
            unsafe { write_mapped(mapped, 0, 0) };
            return;
        }
    }
    unsafe { resource_map(h, res, sub, map_type, flags, mapped) }
}

/// The dynamic unmap slots: DXVK's `Unmap` of a buffer does nothing, so a
/// dynamic buffer's unmap records nothing; anything else is synchronous.
unsafe extern "system" fn fe_unmap_dynamic(h: Hdevice, res: ddi::D3D10DDI_HRESOURCE, sub: u32) {
    let Some(fe) = (unsafe { fe_of(h) }) else {
        return unsafe { resource_unmap(h, res, sub) };
    };
    unsafe {
        fe.enter(h);
        if sub == 0 && resource_state(res).is_some_and(|s| s.dynamic_buffer && s.buffer_raw != 0) {
            return;
        }
    }
    let _t = fe.timed(Acc::MapN, Acc::MapTicks);
    unsafe { fe.sync_enter(h, SyncReason::Unmap) };
    unsafe { resource_unmap(h, res, sub) }
}

unsafe extern "system" fn fe_unmap_sync(h: Hdevice, res: ddi::D3D10DDI_HRESOURCE, sub: u32) {
    let fe = unsafe { fe_of(h) };
    let _t = fe.map(|fe| fe.timed(Acc::MapN, Acc::MapTicks));
    if let Some(fe) = fe {
        unsafe { fe.sync_enter(h, SyncReason::Unmap) };
    }
    unsafe { resource_unmap(h, res, sub) }
}

/// Whether an `UpdateSubresourceUP` can be recorded: a non-dynamic buffer
/// destination and a payload of at most [`MAX_UPDATE_PAYLOAD`] bytes. Returns
/// the payload length.
unsafe fn update_payload(
    res: ddi::D3D10DDI_HRESOURCE,
    sub: u32,
    bx: *const ddi::D3D10_DDI_BOX,
    data: *const c_void,
) -> Option<usize> {
    let st = unsafe { resource_state(res) }?;
    if st.buffer_raw == 0 || st.dynamic_buffer || sub != 0 || data.is_null() {
        return None;
    }
    let (left, right) = if bx.is_null() {
        (0u32, st.byte_width)
    } else {
        let b = unsafe { *bx };
        (b.left as u32, b.right as u32)
    };
    if right <= left || right > st.byte_width {
        return None;
    }
    let len = (right - left) as usize;
    (len <= MAX_UPDATE_PAYLOAD).then_some(len)
}

macro_rules! defer_update {
    ($name:ident => $orig:ident ( $($extra:ident : $t:ty),* )) => {
        #[allow(clippy::too_many_arguments)]
        unsafe extern "system" fn $name(
            h: Hdevice,
            res: ddi::D3D10DDI_HRESOURCE,
            sub: u32,
            bx: *const ddi::D3D10_DDI_BOX,
            data: *const c_void,
            row_pitch: u32,
            depth_pitch: u32
            $(, $extra: $t)*
        ) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h, res, sub, bx, data, row_pitch, depth_pitch $(, $extra)*) };
            };
            unsafe {
                fe.enter(h);
                let payload = if fe.force.has(FrontendSync::UPDATES) {
                    None
                } else {
                    update_payload(res, sub, bx, data)
                };
                let Some(len) = payload else {
                    fe.sync(SyncReason::Update);
                    return $orig(h, res, sub, bx, data, row_pitch, depth_pitch $(, $extra)*);
                };
                let (w, b) = (snap(res), box_copy(bx));
                fe.record(
                    len,
                    |dst| dst.copy_from_slice(core::slice::from_raw_parts(data as *const u8, len)),
                    move |payload| {
                        let (w, b) = (w, b);
                        $orig(
                            h,
                            unsnap(&w),
                            sub,
                            box_ptr(&b),
                            payload.as_ptr() as *const c_void,
                            row_pitch,
                            depth_pitch
                            $(, $extra)*
                        )
                    },
                );
            }
        }
    };
}

defer_update!(fe_update_subresource => resource_update_subresource());
defer_update!(fe_update_subresource_11_1 => resource_update_subresource_11_1(copy_flags: u32));

// ---------------------------------------------------------------------------
// Flush
// ---------------------------------------------------------------------------

/// The device-removed check and the scan-out event retry stay on the app
/// thread. Without shared resources the worker only calls
/// `ID3D11DeviceContext::Flush`; with them the flush gate is a synchronous
/// contract (the runtime releases the keyed mutex when `pfnFlush` returns,
/// and the gate may call `pfnRenderCb`), so it drains and runs inline.
unsafe fn fe_flush_common(fe: &Frontend, h: Hdevice) {
    let _t = fe.timed(Acc::FlushN, Acc::FlushTicks);
    unsafe {
        fe.enter(h);
        if report_if_removed(h, "Flush") {
            return;
        }
        if fe.force.has(FrontendSync::FLUSH) {
            fe.sync(SyncReason::Flush);
            flush(h);
            return;
        }
        let shared = helios_device(h).is_some_and(|d| !lock_ignore_poison(&d.nvk_keyed_resources).is_empty());
        if shared {
            fe.sync(SyncReason::FlushShared);
            flush(h);
            return;
        }
        fe.record0(move |_| {
            if let Some(context) = d3d11_context(h) {
                context.Flush();
            }
        });
        // A Flush is often followed by a wait outside the D3D11 API; make sure
        // a parked worker is not missed by `push`'s cheap wake check.
        fe.q.kick();
        // Unlike the inline path, this retry no longer runs after DXVK's
        // flush; it only re-attempts the scan-out event registration and is
        // retried at every flush and present.
        super::present::retry_scanout_event(h);
    }
}

unsafe extern "system" fn fe_flush(h: Hdevice) {
    match unsafe { fe_of(h) } {
        Some(fe) => unsafe { fe_flush_common(fe, h) },
        None => unsafe { flush(h) },
    }
}

unsafe extern "system" fn fe_flush_11_1(h: Hdevice, flags: u32) -> ddi::BOOL {
    match unsafe { fe_of(h) } {
        Some(fe) => {
            unsafe { fe_flush_common(fe, h) };
            1
        }
        None => unsafe { flush_11_1(h, flags) },
    }
}

unsafe extern "system" fn fe_flush_wddm2_0(h: Hdevice, context_type: u32, flags: u32) -> ddi::BOOL {
    match unsafe { fe_of(h) } {
        Some(fe) => {
            unsafe { fe_flush_common(fe, h) };
            1
        }
        None => unsafe { flush_wddm2_0(h, context_type, flags) },
    }
}

// ---------------------------------------------------------------------------
// Synchronous immediate-context calls
// ---------------------------------------------------------------------------

macro_rules! sync_ddi {
    ($name:ident => $orig:ident ( $($a:ident : $t:ty),* ) $(-> $r:ty)?, $reason:ident) => {
        #[allow(clippy::too_many_arguments)]
        unsafe extern "system" fn $name(h: Hdevice $(, $a: $t)*) $(-> $r)? {
            if let Some(fe) = unsafe { fe_of(h) } {
                unsafe { fe.sync_enter(h, SyncReason::$reason) };
            }
            unsafe { $orig(h $(, $a)*) }
        }
    };
}

sync_ddi!(fe_command_list_execute => command_list_execute(cl: ddi::D3D11DDI_HCOMMANDLIST), CommandList);
sync_ddi!(fe_release_resource_wddm2_1 => release_resource_wddm2_1(r: ddi::D3D10DDI_HRESOURCE, token: ddi::HANDLE), Keyed);
sync_ddi!(fe_acquire_resource_wddm2_1 => acquire_resource_wddm2_1(r: ddi::D3D10DDI_HRESOURCE, token: ddi::HANDLE), Keyed);

/// `DestroyResource` calls `pfnDeallocateCb` and region-keyed registries:
/// wait until the worker is past every record recorded so far, then destroy
/// on this thread. May run on any thread (free-threaded destroy).
unsafe extern "system" fn fe_destroy_resource(h: Hdevice, res: ddi::D3D10DDI_HRESOURCE) {
    let fe = unsafe { fe_of(h) };
    let _t = fe.map(|fe| fe.timed(Acc::SyncN, Acc::SyncTicks));
    if let Some(fe) = fe {
        let i = SyncReason::DestroyResource as usize;
        fe.stats.sync[i].fetch_add(1, Ordering::Relaxed);
        if fe.q.wait_done(fe.q.written()) {
            fe.stats.sync_waited[i].fetch_add(1, Ordering::Relaxed);
        }
        // Destroys of this resource's views queued before it must not be
        // overtaken (the worker runs them only at its next batch boundary).
        fe.q.run_ready_retired();
    }
    unsafe { destroy_resource(h, res) }
}

// ---------------------------------------------------------------------------
// Destruction behind the worker
// ---------------------------------------------------------------------------

/// The other destroys: no runtime callback, no immediate context. Queued until
/// the worker is past every record recorded before the object died, then run
/// with a pseudo-handle over the identity word (inline when it already is).
macro_rules! retire_destroy {
    ($name:ident => $orig:ident, $ht:ident) => {
        unsafe extern "system" fn $name(h: Hdevice, x: ddi::$ht) {
            let Some(fe) = (unsafe { fe_of(h) }) else {
                return unsafe { $orig(h, x) };
            };
            if x.pDrvPrivate.is_null() {
                return unsafe { $orig(h, x) };
            }
            if fe.force.has(FrontendSync::DESTROYS) {
                fe.sync(SyncReason::Destroy);
                fe.q.run_ready_retired();
                return unsafe { $orig(h, x) };
            }
            let pos = fe.q.written();
            if fe.q.done() >= pos {
                return unsafe { $orig(h, x) };
            }
            let w = unsafe { *(x.pDrvPrivate as *const usize) };
            unsafe {
                fe.q.retire(pos, move || {
                    let mut w = w;
                    $orig(h, ddi::$ht { pDrvPrivate: (&mut w as *mut usize).cast() })
                });
            }
        }
    };
}

retire_destroy!(fe_destroy_srv => destroy_srv, D3D10DDI_HSHADERRESOURCEVIEW);
retire_destroy!(fe_destroy_rtv => destroy_rtv, D3D10DDI_HRENDERTARGETVIEW);
retire_destroy!(fe_destroy_dsv => destroy_dsv, D3D10DDI_HDEPTHSTENCILVIEW);
retire_destroy!(fe_destroy_uav => destroy_uav, D3D11DDI_HUNORDEREDACCESSVIEW);
retire_destroy!(fe_destroy_shader => destroy_shader, D3D10DDI_HSHADER);
retire_destroy!(fe_destroy_blend_state => destroy_blend_state, D3D10DDI_HBLENDSTATE);
retire_destroy!(fe_destroy_depth_state => destroy_depth_state, D3D10DDI_HDEPTHSTENCILSTATE);
retire_destroy!(fe_destroy_raster_state => destroy_raster_state, D3D10DDI_HRASTERIZERSTATE);
retire_destroy!(fe_destroy_sampler => destroy_sampler, D3D10DDI_HSAMPLER);
retire_destroy!(fe_destroy_element_layout => destroy_element_layout, D3D10DDI_HELEMENTLAYOUT);
retire_destroy!(fe_destroy_query => destroy_query, D3D10DDI_HQUERY);
retire_destroy!(fe_destroy_command_list => destroy_command_list, D3D11DDI_HCOMMANDLIST);

// ---------------------------------------------------------------------------
// DXGI: every call that orders against the immediate context drains
// ---------------------------------------------------------------------------

macro_rules! sync_dxgi {
    ($name:ident => $orig:ident, $arg:ident, present) => {
        unsafe extern "system" fn $name(arg: *mut ddi::$arg) -> i32 {
            if !arg.is_null() {
                let h = unsafe { dxgi_device_handle((*arg).hDevice) };
                if let Some(fe) = unsafe { fe_of(h) } {
                    unsafe { fe.present_drain(h) };
                }
            }
            unsafe { $orig(arg) }
        }
    };
    ($name:ident => $orig:ident, $arg:ident) => {
        unsafe extern "system" fn $name(arg: *mut ddi::$arg) -> i32 {
            if !arg.is_null() {
                let h = unsafe { dxgi_device_handle((*arg).hDevice) };
                if let Some(fe) = unsafe { fe_of(h) } {
                    unsafe { fe.sync_enter(h, SyncReason::Dxgi) };
                }
            }
            unsafe { $orig(arg) }
        }
    };
}

/// `UmdPresentAhead`: a single-surface Present of an NVK device runs with its drain owed instead
/// of paid up front. `present.rs` takes the debt where the Present decides
/// ([`take_owed_present`]): a Present it queues pays nothing but the window
/// ([`Frontend::present_queued`]), any other drains there, before its first immediate-context
/// use. A Present that returns before deciding (a removed device, a refused argument) drains
/// here after it, so no Present leaves without one or the other.
unsafe fn present_owing(
    fe: &Frontend,
    h: Hdevice,
    single_surface: bool,
    call: impl FnOnce() -> i32,
) -> i32 {
    let nvk = unsafe { helios_device(h) }.is_some_and(|d| d.dxvk.is_nvk());
    if !fe.presents_ahead() || !single_surface || !nvk {
        unsafe { fe.present_drain(h) };
        return call();
    }
    OWED_PRESENT.with(|c| c.set(fe as *const Frontend));
    let hr = call();
    if !OWED_PRESENT.with(|c| c.replace(core::ptr::null())).is_null() {
        unsafe { fe.present_drain(h) };
    }
    hr
}

/// The front end whose Present drain the current Present owes, if it is `h`'s
/// ([`present_owing`]). The caller now owes it: it either queues the Present
/// ([`Frontend::present_queued`]) or calls [`Frontend::present_drain`] before it
/// touches the immediate context.
pub(crate) unsafe fn take_owed_present<'a>(h: Hdevice) -> Option<&'a Frontend> {
    let fe = unsafe { fe_of(h) }?;
    let owed = OWED_PRESENT.with(|c| c.get());
    if owed.is_null() || !core::ptr::eq(owed, fe) {
        return None;
    }
    OWED_PRESENT.with(|c| c.set(core::ptr::null()));
    Some(fe)
}

unsafe extern "system" fn fe_dxgi_present(arg: *mut ddi::DXGI_DDI_ARG_PRESENT) -> i32 {
    if !arg.is_null() {
        let h = unsafe { dxgi_device_handle((*arg).hDevice) };
        if let Some(fe) = unsafe { fe_of(h) } {
            let call = || unsafe { dxgi_present(arg) };
            return unsafe { present_owing(fe, h, true, call) };
        }
    }
    unsafe { dxgi_present(arg) }
}

unsafe extern "system" fn fe_dxgi_present1(arg: *mut ddi::DXGI_DDI_ARG_PRESENT1) -> i32 {
    if !arg.is_null() {
        let h = unsafe { dxgi_device_handle((*arg).hDevice) };
        if let Some(fe) = unsafe { fe_of(h) } {
            let single = unsafe { (*arg).SurfacesToPresent == 1 };
            let call = || unsafe { dxgi_present1(arg) };
            return unsafe { present_owing(fe, h, single, call) };
        }
    }
    unsafe { dxgi_present1(arg) }
}

unsafe extern "system" fn fe_dxgi_present1_6_1(arg: *mut ddi::DXGI1_6_1_DDI_ARG_PRESENT) -> i32 {
    if !arg.is_null() {
        let h = unsafe { dxgi_device_handle((*arg).hDevice) };
        if let Some(fe) = unsafe { fe_of(h) } {
            let single = unsafe { (*arg).SurfacesToPresent == 1 };
            let call = || unsafe { dxgi_present1_6_1(arg) };
            return unsafe { present_owing(fe, h, single, call) };
        }
    }
    unsafe { dxgi_present1_6_1(arg) }
}

/// `RotateResourceIdentities`: with `UmdPresentAhead` an NVK device's rotation is queued behind
/// the Presents it follows ([`dxgi_rotate_resource_identities_queued`]); otherwise it drains.
unsafe extern "system" fn fe_dxgi_rotate_resource_identities(
    arg: *mut ddi::DXGI_DDI_ARG_ROTATE_RESOURCE_IDENTITIES,
) -> i32 {
    if !arg.is_null() {
        let h = unsafe { dxgi_device_handle((*arg).hDevice) };
        if let Some(fe) = unsafe { fe_of(h) } {
            if fe.presents_ahead() && unsafe { helios_device(h) }.is_some_and(|d| d.dxvk.is_nvk()) {
                unsafe { fe.enter(h) };
                return unsafe { dxgi_rotate_resource_identities_queued(arg, fe) };
            }
            unsafe { fe.sync_enter(h, SyncReason::Dxgi) };
        }
    }
    unsafe { dxgi_rotate_resource_identities(arg) }
}

sync_dxgi!(fe_dxgi_set_display_mode => dxgi_set_display_mode, DXGI_DDI_ARG_SETDISPLAYMODE);
sync_dxgi!(fe_dxgi_blt => dxgi_blt, DXGI_DDI_ARG_BLT);
sync_dxgi!(fe_dxgi_resolve_shared_resource => dxgi_resolve_shared_resource, DXGI_DDI_ARG_RESOLVESHAREDRESOURCE);
sync_dxgi!(fe_dxgi_blt1 => dxgi_blt1, DXGI_DDI_ARG_BLT1);
sync_dxgi!(fe_dxgi_offer_resources => dxgi_offer_resources, DXGI_DDI_ARG_OFFERRESOURCES);
sync_dxgi!(fe_dxgi_reclaim_resources => dxgi_reclaim_resources, DXGI_DDI_ARG_RECLAIMRESOURCES);
sync_dxgi!(fe_dxgi_present_mpo => dxgi_present_mpo, DXGI_DDI_ARG_PRESENTMULTIPLANEOVERLAY, present);
sync_dxgi!(fe_dxgi_offer_resources1 => dxgi_offer_resources1, DXGI_DDI_ARG_OFFERRESOURCES1);
sync_dxgi!(fe_dxgi_reclaim_resources1 => dxgi_reclaim_resources1, DXGI_DDI_ARG_RECLAIMRESOURCES1);
sync_dxgi!(fe_dxgi_present_mpo1 => dxgi_present_mpo1, DXGI1_6_1_DDI_ARG_PRESENTMULTIPLANEOVERLAY, present);

// ---------------------------------------------------------------------------
// Table installation (after the normal install chain, only for a device with
// a front end; the chain mirrors `tables.rs` so every slot keeps its type)
// ---------------------------------------------------------------------------

/// The 11.0-typed slots. Slots retyped by 11.1 are overwritten by
/// [`frontend_install_11_1`] on >= 11.1 tables, exactly as `install_11_1`
/// overwrites `install`.
pub(crate) unsafe fn frontend_install_11_0(funcs: *mut ddi::D3D11DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        return;
    }
    let f = unsafe { &mut *funcs };
    // Deferred.
    f.pfnDraw = Some(fe_draw);
    f.pfnDrawIndexed = Some(fe_draw_indexed);
    f.pfnDrawInstanced = Some(fe_draw_instanced);
    f.pfnDrawIndexedInstanced = Some(fe_draw_indexed_instanced);
    f.pfnDrawAuto = Some(fe_draw_auto);
    f.pfnDrawInstancedIndirect = Some(fe_draw_instanced_indirect);
    f.pfnDrawIndexedInstancedIndirect = Some(fe_draw_indexed_instanced_indirect);
    f.pfnDispatch = Some(fe_dispatch);
    f.pfnDispatchIndirect = Some(fe_dispatch_indirect);
    f.pfnIaSetTopology = Some(fe_ia_set_topology);
    f.pfnIaSetInputLayout = Some(fe_ia_set_input_layout);
    f.pfnIaSetVertexBuffers = Some(fe_ia_set_vertex_buffers);
    f.pfnIaSetIndexBuffer = Some(fe_ia_set_index_buffer);
    f.pfnVsSetShader = Some(fe_vs_set_shader);
    f.pfnPsSetShader = Some(fe_ps_set_shader);
    f.pfnGsSetShader = Some(fe_gs_set_shader);
    f.pfnHsSetShader = Some(fe_hs_set_shader);
    f.pfnDsSetShader = Some(fe_ds_set_shader);
    f.pfnCsSetShader = Some(fe_cs_set_shader);
    f.pfnVsSetShaderWithIfaces = Some(fe_vs_set_shader_with_ifaces);
    f.pfnPsSetShaderWithIfaces = Some(fe_ps_set_shader_with_ifaces);
    f.pfnGsSetShaderWithIfaces = Some(fe_gs_set_shader_with_ifaces);
    f.pfnHsSetShaderWithIfaces = Some(fe_hs_set_shader_with_ifaces);
    f.pfnDsSetShaderWithIfaces = Some(fe_ds_set_shader_with_ifaces);
    f.pfnCsSetShaderWithIfaces = Some(fe_cs_set_shader_with_ifaces);
    f.pfnVsSetConstantBuffers = Some(fe_vs_set_constant_buffers);
    f.pfnPsSetConstantBuffers = Some(fe_ps_set_constant_buffers);
    f.pfnGsSetConstantBuffers = Some(fe_gs_set_constant_buffers);
    f.pfnHsSetConstantBuffers = Some(fe_hs_set_constant_buffers);
    f.pfnDsSetConstantBuffers = Some(fe_ds_set_constant_buffers);
    f.pfnCsSetConstantBuffers = Some(fe_cs_set_constant_buffers);
    f.pfnVsSetShaderResources = Some(fe_vs_set_shader_resources);
    f.pfnPsSetShaderResources = Some(fe_ps_set_shader_resources);
    f.pfnGsSetShaderResources = Some(fe_gs_set_shader_resources);
    f.pfnHsSetShaderResources = Some(fe_hs_set_shader_resources);
    f.pfnDsSetShaderResources = Some(fe_ds_set_shader_resources);
    f.pfnCsSetShaderResources = Some(fe_cs_set_shader_resources);
    f.pfnVsSetSamplers = Some(fe_vs_set_samplers);
    f.pfnPsSetSamplers = Some(fe_ps_set_samplers);
    f.pfnGsSetSamplers = Some(fe_gs_set_samplers);
    f.pfnHsSetSamplers = Some(fe_hs_set_samplers);
    f.pfnDsSetSamplers = Some(fe_ds_set_samplers);
    f.pfnCsSetSamplers = Some(fe_cs_set_samplers);
    f.pfnSetRenderTargets = Some(fe_set_render_targets);
    f.pfnSetViewports = Some(fe_set_viewports);
    f.pfnSetScissorRects = Some(fe_set_scissor_rects);
    f.pfnSoSetTargets = Some(fe_so_set_targets);
    f.pfnCsSetUnorderedAccessViews = Some(fe_cs_set_uavs);
    f.pfnSetRasterizerState = Some(fe_set_rasterizer_state);
    f.pfnSetDepthStencilState = Some(fe_set_depth_stencil_state);
    f.pfnSetBlendState = Some(fe_set_blend_state);
    f.pfnSetPredication = Some(fe_set_predication);
    f.pfnSetResourceMinLOD = Some(fe_set_resource_min_lod);
    f.pfnClearRenderTargetView = Some(fe_clear_rtv);
    f.pfnClearDepthStencilView = Some(fe_clear_dsv);
    f.pfnClearUnorderedAccessViewUint = Some(fe_clear_uav_uint);
    f.pfnClearUnorderedAccessViewFloat = Some(fe_clear_uav_float);
    f.pfnResourceCopy = Some(fe_resource_copy);
    f.pfnResourceConvert = Some(fe_resource_copy);
    f.pfnResourceCopyRegion = Some(fe_resource_copy_region);
    f.pfnResourceConvertRegion = Some(fe_resource_copy_region);
    f.pfnResourceResolveSubresource = Some(fe_resource_resolve_subresource);
    f.pfnCopyStructureCount = Some(fe_copy_structure_count);
    f.pfnGenMips = Some(fe_gen_mips);
    f.pfnQueryBegin = Some(fe_query_begin);
    f.pfnQueryEnd = Some(fe_query_end);
    f.pfnResourceUpdateSubresourceUP = Some(fe_update_subresource);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(fe_update_subresource);
    f.pfnFlush = Some(fe_flush);
    // Split.
    f.pfnQueryGetData = Some(fe_query_get_data);
    f.pfnDynamicConstantBufferMapDiscard = Some(fe_map_dynamic);
    f.pfnDynamicIABufferMapDiscard = Some(fe_map_dynamic);
    f.pfnDynamicIABufferMapNoOverwrite = Some(fe_map_dynamic);
    f.pfnDynamicResourceMapDiscard = Some(fe_map_dynamic);
    f.pfnDynamicConstantBufferUnmap = Some(fe_unmap_dynamic);
    f.pfnDynamicIABufferUnmap = Some(fe_unmap_dynamic);
    f.pfnDynamicResourceUnmap = Some(fe_unmap_dynamic);
    f.pfnDestroyShaderResourceView = Some(fe_destroy_srv);
    f.pfnDestroyRenderTargetView = Some(fe_destroy_rtv);
    f.pfnDestroyDepthStencilView = Some(fe_destroy_dsv);
    f.pfnDestroyUnorderedAccessView = Some(fe_destroy_uav);
    f.pfnDestroyShader = Some(fe_destroy_shader);
    f.pfnDestroyBlendState = Some(fe_destroy_blend_state);
    f.pfnDestroyDepthStencilState = Some(fe_destroy_depth_state);
    f.pfnDestroyRasterizerState = Some(fe_destroy_raster_state);
    f.pfnDestroySampler = Some(fe_destroy_sampler);
    f.pfnDestroyElementLayout = Some(fe_destroy_element_layout);
    f.pfnDestroyQuery = Some(fe_destroy_query);
    f.pfnDestroyCommandList = Some(fe_destroy_command_list);
    // Synchronous.
    f.pfnResourceMap = Some(fe_map_sync);
    f.pfnStagingResourceMap = Some(fe_map_sync);
    f.pfnResourceUnmap = Some(fe_unmap_sync);
    f.pfnStagingResourceUnmap = Some(fe_unmap_sync);
    f.pfnDestroyResource = Some(fe_destroy_resource);
    f.pfnCommandListExecute = Some(fe_command_list_execute);
}

pub(crate) unsafe fn frontend_install_11_1(funcs: *mut ddi::D3D11_1DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        return;
    }
    unsafe { frontend_install_11_0(funcs.cast()) };
    let f = unsafe { &mut *funcs };
    f.pfnVsSetConstantBuffers = Some(fe_vs_set_constant_buffers1);
    f.pfnPsSetConstantBuffers = Some(fe_ps_set_constant_buffers1);
    f.pfnGsSetConstantBuffers = Some(fe_gs_set_constant_buffers1);
    f.pfnHsSetConstantBuffers = Some(fe_hs_set_constant_buffers1);
    f.pfnDsSetConstantBuffers = Some(fe_ds_set_constant_buffers1);
    f.pfnCsSetConstantBuffers = Some(fe_cs_set_constant_buffers1);
    f.pfnFlush = Some(fe_flush_11_1);
    f.pfnResourceCopyRegion = Some(fe_resource_copy_region_11_1);
    f.pfnResourceConvertRegion = Some(fe_resource_copy_region_11_1);
    f.pfnResourceUpdateSubresourceUP = Some(fe_update_subresource_11_1);
    f.pfnDefaultConstantBufferUpdateSubresourceUP = Some(fe_update_subresource_11_1);
    f.pfnDiscard = Some(fe_discard_11_1);
    f.pfnClearView = Some(fe_clear_view_11_1);
    f.pfnDynamicConstantBufferMapNoOverwrite = Some(fe_dynamic_cb_map_no_overwrite);
}

pub(crate) unsafe fn frontend_install_wddm1_3(funcs: *mut ddi::D3DWDDM1_3DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        return;
    }
    // The tiled-resource and marker slots reach the context only through
    // `d3d11_context`, whose ordering drain covers them.
    unsafe { frontend_install_11_1(funcs.cast()) };
}

pub(crate) unsafe fn frontend_install_wddm2_3(funcs: *mut ddi::D3DWDDM2_2DDI_DEVICEFUNCS) {
    if funcs.is_null() {
        return;
    }
    unsafe { frontend_install_wddm1_3(funcs.cast()) };
    let f = unsafe { &mut *funcs };
    f.pfnFlush = Some(fe_flush_wddm2_0);
    f.pfnAcquireResource = Some(fe_acquire_resource_wddm2_1);
    f.pfnReleaseResource = Some(fe_release_resource_wddm2_1);
}

pub(crate) unsafe fn frontend_install_dxgi(funcs: *mut ddi::DXGI_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    let f = unsafe { &mut *funcs };
    f.pfnPresent = Some(fe_dxgi_present);
    f.pfnSetDisplayMode = Some(fe_dxgi_set_display_mode);
    f.pfnRotateResourceIdentities = Some(fe_dxgi_rotate_resource_identities);
    f.pfnBlt = Some(fe_dxgi_blt);
}

pub(crate) unsafe fn frontend_install_dxgi_1_1(funcs: *mut ddi::DXGI1_1_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    unsafe { frontend_install_dxgi(funcs.cast()) };
    let f = unsafe { &mut *funcs };
    f.pfnResolveSharedResource = Some(fe_dxgi_resolve_shared_resource);
}

pub(crate) unsafe fn frontend_install_dxgi_1_3(funcs: *mut ddi::DXGI1_3_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    unsafe { frontend_install_dxgi_1_1(funcs.cast()) };
    let f = unsafe { &mut *funcs };
    f.pfnBlt1 = Some(fe_dxgi_blt1);
    f.pfnOfferResources = Some(fe_dxgi_offer_resources);
    f.pfnReclaimResources = Some(fe_dxgi_reclaim_resources);
    f.pfnPresentMultiplaneOverlay = Some(fe_dxgi_present_mpo);
    f.pfnPresent1 = Some(fe_dxgi_present1);
}

pub(crate) unsafe fn frontend_install_dxgi_1_6_1(funcs: *mut ddi::DXGI1_6_1_DDI_BASE_FUNCTIONS) {
    if funcs.is_null() {
        return;
    }
    unsafe { frontend_install_dxgi_1_1(funcs.cast()) };
    let f = unsafe { &mut *funcs };
    f.pfnBlt1 = Some(fe_dxgi_blt1);
    f.pfnOfferResources1 = Some(fe_dxgi_offer_resources1);
    f.pfnReclaimResources = Some(fe_dxgi_reclaim_resources);
    f.pfnPresentMultiplaneOverlay = Some(fe_dxgi_present_mpo);
    f.pfnPresent1 = Some(fe_dxgi_present1_6_1);
    f.pfnPresentMultiplaneOverlay1 = Some(fe_dxgi_present_mpo1);
    f.pfnReclaimResources1 = Some(fe_dxgi_reclaim_resources1);
}
