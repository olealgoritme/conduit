//! Single-producer command ring and its worker thread: the transport of the
//! D3D11 threaded front end (`guest/windows/docs/d3d11-threaded-frontend.md`).
//!
//! A record is a closure plus optional trailing bytes, written in place into a
//! fixed byte ring by the producer and called, in order, by one worker thread.
//! The producer is whichever thread is inside an immediate-context DDI; the
//! runtime serialises those, so there is one producer at a time and the
//! runtime's own lock orders successive producer threads. Every other thread
//! (free-threaded destroys, deferred-context recycles) only waits on positions
//! or uses the destroy queue.
//!
//! Positions are monotonic byte cursors. `pos` returned by [`CmdQueue::push`]
//! is the cursor just past the record; the record has run once
//! [`CmdQueue::done`] reaches it.
//!
//! Layout of a record (8-byte aligned, never straddling the end of the ring):
//!
//! ```text
//! | Header { call, len, extra_off, extra_len } | closure bytes | extra bytes |
//! ```
//!
//! `call == None` is a pad record covering the tail of the ring. A tail shorter
//! than a header is an implicit pad (no record ever starts there).
//!
//! Pure `std`: builds and tests on the Linux host.

use core::cell::Cell;
use core::mem::{align_of, size_of};
use core::sync::atomic::{fence, AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Keeps a value on its own cache line.
#[repr(align(64))]
struct Line<T>(T);

type CallFn = unsafe fn(closure: *mut u8, extra: *const u8, extra_len: usize);

#[repr(C)]
struct Header {
    call: Option<CallFn>,
    len: u32,
    extra_off: u32,
    extra_len: u32,
}

const fn round8(n: usize) -> usize {
    (n + 7) & !7
}

/// Bytes of a record header, rounded to the record alignment.
pub const HEADER_BYTES: usize = round8(size_of::<Header>());

/// Smallest and largest ring the queue accepts (bytes, rounded to a power of
/// two in between).
pub const MIN_RING_BYTES: usize = 4096;
pub const MAX_RING_BYTES: usize = 64 << 20;

/// Worker spin on an empty ring before it parks.
const WORKER_SPIN: Duration = Duration::from_micros(100);
/// A waiter's spin before it sleeps on the condvar.
const WAITER_SPIN: Duration = Duration::from_micros(100);
/// Records between two waiter/destroy checks while the worker is busy.
const BATCH: u32 = 16;
/// Park timeouts: the first park after work is short (backstop against a
/// wake the producer's relaxed check missed), later ones are long.
const PARK_SHORT: Duration = Duration::from_millis(1);
const PARK_MID: Duration = Duration::from_millis(4);
const PARK_LONG: Duration = Duration::from_millis(50);
/// Consecutive idle parks before the worker parks for [`PARK_LONG`] (about a
/// quarter second of idleness at [`PARK_MID`]).
const PARKS_BEFORE_LONG: u32 = 64;

unsafe fn call_closure<F: FnOnce(&[u8])>(closure: *mut u8, extra: *const u8, extra_len: usize) {
    // SAFETY: `closure` points at an `F` written by `push` and read exactly
    // once, here; `extra` spans `extra_len` initialised bytes of the record.
    let f = unsafe { core::ptr::read(closure as *mut F) };
    f(unsafe { core::slice::from_raw_parts(extra, extra_len) });
}

thread_local! {
    /// The queue whose worker this thread is (0 = none).
    static CURRENT_WORKER: Cell<usize> = const { Cell::new(0) };
}

/// Lock a mutex, ignoring poison (`panic = "abort"` makes poisoning
/// unreachable in the drivers; tests may unwind).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A destruction queued until the worker passes `pos`.
struct Retire {
    pos: u64,
    run: Box<dyn FnOnce()>,
}

/// Counters, read by [`CmdQueue::summary`].
#[derive(Default)]
struct Stats {
    records: AtomicU64,
    drains: AtomicU64,
    drain_waits: AtomicU64,
    drain_wait_us: AtomicU64,
    drain_wait_max_us: AtomicU64,
    full_waits: AtomicU64,
    parks: AtomicU64,
    wakes: AtomicU64,
    retire_queued: AtomicU64,
    retire_inline: AtomicU64,
}

/// The ring, its worker and the coordination around them.
/// What every push reads: written only at setup, start and shutdown, so it
/// stays shared-clean in both cores' caches.
#[repr(C, align(64))]
struct Hot {
    buf: *mut u8,
    cap: usize,
    started: AtomicBool,
    stop: AtomicBool,
}

/// The producer's own line: published write cursor + its cached view of the
/// read cursor.
#[repr(C, align(64))]
struct ProducerLine {
    write: AtomicU64,
    read_cache: AtomicU64,
}

#[repr(C)]
pub struct CmdQueue {
    hot: Hot,
    prod: ProducerLine,
    /// Consumer line: records before this cursor have run.
    read: Line<AtomicU64>,
    /// The consumer's cached view of the write cursor: it reloads the
    /// producer's line only when it has caught up with this, not per record.
    write_cache: Line<AtomicU64>,
    /// The worker announced it is about to park.
    sleeping: Line<AtomicBool>,
    /// Errors stashed by the worker; read (one relaxed load) on every DDI
    /// entry, written only on failure.
    error: Line<AtomicI32>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    worker_thread: std::sync::OnceLock<std::thread::Thread>,
    /// Worker-written counters and the waiter count, away from the lines
    /// above.
    waiters: Line<AtomicUsize>,
    wait_lock: Mutex<()>,
    wait_cv: Condvar,
    retire: Mutex<VecDeque<Retire>>,
    retire_pending: AtomicBool,
    stats: Line<Stats>,
    name: String,
    /// True once waiting for the worker is pointless (process teardown has
    /// killed it): waits return and shutdown does not join.
    abandoned: std::sync::OnceLock<fn() -> bool>,
    /// Captured on the producer thread when the worker starts, applied on the
    /// worker (the drivers mirror the producer's scheduling priority).
    thread_init: std::sync::OnceLock<(fn() -> isize, fn(isize))>,
}

// SAFETY: the ring buffer is written only by the (single, externally
// serialised) producer in the region between `read` and the reserved end, and
// read only by the worker in the region between `read` and `write`; the
// cursors' release/acquire pairs order those accesses. Closures and retire
// entries are `!Send` in general (they hold raw DDI pointers); the caller of
// the unsafe push/retire functions guarantees they may run on the worker.
unsafe impl Send for CmdQueue {}
unsafe impl Sync for CmdQueue {}

/// Why a push did not happen.
#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    /// The record would be larger than a quarter of the ring; the caller must
    /// take its synchronous path.
    TooLarge,
    /// The queue is shut down.
    Stopped,
}

impl CmdQueue {
    /// A queue with a ring of about `ring_bytes` (rounded up to a power of two
    /// and clamped to [`MIN_RING_BYTES`, `MAX_RING_BYTES`]). The worker thread
    /// starts on the first push.
    pub fn new(ring_bytes: usize, name: &str) -> Arc<Self> {
        let cap = ring_bytes
            .clamp(MIN_RING_BYTES, MAX_RING_BYTES)
            .next_power_of_two();
        let layout = Layout::from_size_align(cap, 64).expect("ring layout");
        // SAFETY: non-zero size.
        let buf = unsafe { alloc_zeroed(layout) };
        assert!(!buf.is_null(), "ring allocation failed");
        Arc::new(Self {
            hot: Hot {
                buf,
                cap,
                started: AtomicBool::new(false),
                stop: AtomicBool::new(false),
            },
            prod: ProducerLine {
                write: AtomicU64::new(0),
                read_cache: AtomicU64::new(0),
            },
            read: Line(AtomicU64::new(0)),
            write_cache: Line(AtomicU64::new(0)),
            sleeping: Line(AtomicBool::new(false)),
            error: Line(AtomicI32::new(0)),
            worker: Mutex::new(None),
            worker_thread: std::sync::OnceLock::new(),
            waiters: Line(AtomicUsize::new(0)),
            wait_lock: Mutex::new(()),
            wait_cv: Condvar::new(),
            retire: Mutex::new(VecDeque::new()),
            retire_pending: AtomicBool::new(false),
            stats: Line(Stats::default()),
            name: name.to_string(),
            abandoned: std::sync::OnceLock::new(),
            thread_init: std::sync::OnceLock::new(),
        })
    }

    /// Install the check that says the worker may be gone without having
    /// exited (Windows: `RtlDllShutdownInProgress`, process termination kills
    /// every other thread before DLL teardown runs). Set once.
    pub fn set_abandon_check(&self, check: fn() -> bool) {
        let _ = self.abandoned.set(check);
    }

    /// Install `capture` (run on the producer thread that starts the worker)
    /// and `apply` (run first thing on the worker with its result). Set once,
    /// before the first push.
    pub fn set_thread_init(&self, capture: fn() -> isize, apply: fn(isize)) {
        let _ = self.thread_init.set((capture, apply));
    }

    fn is_abandoned(&self) -> bool {
        self.abandoned.get().is_some_and(|f| f())
    }

    /// Bytes a record of closure type `F` with `extra_len` trailing bytes
    /// occupies; a push succeeds iff this is at most [`max_record`](Self::max_record).
    pub const fn record_bytes<F>(extra_len: usize) -> usize {
        round8(HEADER_BYTES + round8(size_of::<F>()) + extra_len)
    }

    /// Ring capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.hot.cap
    }

    /// Largest record (header + closure + extra) a push accepts.
    pub fn max_record(&self) -> usize {
        self.hot.cap / 4
    }

    /// The cursor past the last published record.
    pub fn written(&self) -> u64 {
        self.prod.write.load(Ordering::Acquire)
    }

    /// The cursor past the last record that has run.
    pub fn done(&self) -> u64 {
        self.read.0.load(Ordering::Acquire)
    }

    /// True on this queue's worker thread.
    pub fn is_worker(&self) -> bool {
        CURRENT_WORKER.with(|c| c.get()) == self as *const Self as usize
    }

    /// True on any queue's worker thread.
    pub fn on_any_worker() -> bool {
        CURRENT_WORKER.with(|c| c.get()) != 0
    }

    /// Append a record: `f` runs on the worker with the `extra_len` bytes that
    /// `fill` wrote. Returns the record's end position.
    ///
    /// # Safety
    /// - Only one thread pushes at a time, and successive pushing threads are
    ///   ordered by a happens-before edge (the D3D11 runtime's device lock).
    /// - `f` and everything it captures must be valid to move to and run on the
    ///   worker thread (raw pointers it holds must stay valid until it runs).
    /// - Must not be called from the worker thread.
    pub unsafe fn push<F>(
        self: &Arc<Self>,
        extra_len: usize,
        fill: impl FnOnce(&mut [u8]),
        f: F,
    ) -> Result<u64, PushError>
    where
        F: FnOnce(&[u8]),
    {
        const { assert!(align_of::<F>() <= 8, "record closures must be at most 8-byte aligned") };
        debug_assert!(!self.is_worker(), "push from the worker thread");
        if self.hot.stop.load(Ordering::Relaxed) {
            return Err(PushError::Stopped);
        }
        let extra_off = HEADER_BYTES + round8(size_of::<F>());
        let len = round8(extra_off + extra_len);
        if len > self.max_record() {
            return Err(PushError::TooLarge);
        }
        if !self.hot.started.load(Ordering::Relaxed) {
            self.start();
        }
        let mask = self.hot.cap - 1;
        let w = self.prod.write.load(Ordering::Relaxed);
        let pos = (w as usize) & mask;
        let tail = self.hot.cap - pos;
        let start = if tail < len { w + tail as u64 } else { w };
        let end = start + len as u64;
        // Room: everything before `end - cap` must have been consumed.
        let need_read = end.saturating_sub(self.hot.cap as u64);
        if self.prod.read_cache.load(Ordering::Relaxed) < need_read {
            let mut r = self.read.0.load(Ordering::Acquire);
            if r < need_read {
                self.stats.0.full_waits.fetch_add(1, Ordering::Relaxed);
                self.wait_done(need_read);
                r = self.read.0.load(Ordering::Acquire);
            }
            self.prod.read_cache.store(r, Ordering::Relaxed);
        }
        // SAFETY: [w, end) lies in consumed space (checked above) and only the
        // producer writes there until `write` is published.
        unsafe {
            if start != w && tail >= HEADER_BYTES {
                (self.hot.buf.add(pos) as *mut Header).write(Header {
                    call: None,
                    len: tail as u32,
                    extra_off: 0,
                    extra_len: 0,
                });
            }
            let p = self.hot.buf.add((start as usize) & mask);
            (p as *mut Header).write(Header {
                call: Some(call_closure::<F>),
                len: len as u32,
                extra_off: extra_off as u32,
                extra_len: extra_len as u32,
            });
            (p.add(HEADER_BYTES) as *mut F).write(f);
            fill(core::slice::from_raw_parts_mut(p.add(extra_off), extra_len));
        }
        self.prod.write.store(end, Ordering::Release);
        // Cheap check; a wake this misses is caught by the worker's short park
        // timeout and by every waiting path's full kick.
        if self.sleeping.0.load(Ordering::Relaxed) {
            self.wake_worker();
        }
        Ok(end)
    }

    fn start(self: &Arc<Self>) {
        if self.hot.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let q = Arc::clone(self);
        let init = self.thread_init.get().map(|&(capture, apply)| (capture(), apply));
        let handle = std::thread::Builder::new()
            .name(self.name.clone())
            .spawn(move || {
                if let Some((value, apply)) = init {
                    apply(value);
                }
                q.worker_main()
            })
            .expect("spawn front-end worker");
        let _ = self.worker_thread.set(handle.thread().clone());
        *lock(&self.worker) = Some(handle);
    }

    fn wake_worker(&self) {
        if self.sleeping.0.swap(false, Ordering::AcqRel) {
            if let Some(t) = self.worker_thread.get() {
                self.stats.0.wakes.fetch_add(1, Ordering::Relaxed);
                t.unpark();
            }
        }
    }

    /// Make sure the worker will look at the ring: a full fence, then wake it
    /// if it announced sleep.
    pub fn kick(&self) {
        fence(Ordering::SeqCst);
        if self.sleeping.0.load(Ordering::Relaxed) {
            self.wake_worker();
        }
    }

    /// Wait until the worker has run every record ending at or before `pos`.
    /// Returns true when it had to wait. On the worker thread it returns at
    /// once (the worker runs records in order, so anything it could wait for
    /// is either done or behind the record it is running).
    pub fn wait_done(&self, pos: u64) -> bool {
        self.wait_done_spinning(pos, WAITER_SPIN)
    }

    /// [`wait_done`](Self::wait_done) that spins up to `spin` before it
    /// sleeps: for a waiter with nothing else to do whose wait is normally
    /// short (the render thread at Present waiting for the frame's tail), a
    /// sleep/wake handshake would add its latency to every frame.
    pub fn wait_done_spinning(&self, pos: u64, spin: Duration) -> bool {
        if self.read.0.load(Ordering::Acquire) >= pos || self.is_worker() {
            return false;
        }
        let t0 = Instant::now();
        self.kick();
        while t0.elapsed() < spin {
            if self.read.0.load(Ordering::Acquire) >= pos {
                self.note_wait(t0);
                return true;
            }
            core::hint::spin_loop();
        }
        let mut g = lock(&self.wait_lock);
        self.waiters.0.fetch_add(1, Ordering::SeqCst);
        while self.read.0.load(Ordering::SeqCst) < pos {
            if !self.hot.started.load(Ordering::Acquire) || self.is_abandoned() {
                break; // never started (cannot happen in use) or killed at exit
            }
            self.kick();
            g = self
                .wait_cv
                .wait_timeout(g, PARK_SHORT)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        self.waiters.0.fetch_sub(1, Ordering::SeqCst);
        drop(g);
        self.note_wait(t0);
        true
    }

    fn note_wait(&self, t0: Instant) {
        let us = t0.elapsed().as_micros() as u64;
        self.stats.0.drain_waits.fetch_add(1, Ordering::Relaxed);
        self.stats.0.drain_wait_us.fetch_add(us, Ordering::Relaxed);
        self.stats.0.drain_wait_max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Wait until every published record has run. Returns true when it had
    /// to wait.
    pub fn drain(&self) -> bool {
        self.stats.0.drains.fetch_add(1, Ordering::Relaxed);
        self.wait_done(self.written())
    }

    /// [`drain`](Self::drain) spinning up to `spin` before sleeping.
    pub fn drain_spinning(&self, spin: Duration) -> bool {
        self.stats.0.drains.fetch_add(1, Ordering::Relaxed);
        self.wait_done_spinning(self.written(), spin)
    }

    /// Run `run` once the worker has passed `pos` (normally the producer's
    /// [`written`](Self::written) position at the time the object died).
    /// Runs inline when it already has.
    ///
    /// # Safety
    /// `run` must be valid to run on the worker thread or on the caller's.
    pub unsafe fn retire(&self, pos: u64, run: impl FnOnce() + 'static) {
        if self.read.0.load(Ordering::Acquire) >= pos || self.hot.stop.load(Ordering::Acquire) {
            self.stats.0.retire_inline.fetch_add(1, Ordering::Relaxed);
            run();
            return;
        }
        self.stats.0.retire_queued.fetch_add(1, Ordering::Relaxed);
        lock(&self.retire).push_back(Retire {
            pos,
            run: Box::new(run),
        });
        self.retire_pending.store(true, Ordering::Release);
        // If the worker passed `pos` (and checked the queue) between our load
        // and the push, nobody would run it until the next batch: kick.
        self.kick();
    }

    /// Run, on the calling thread, the queued destructions the worker has
    /// already passed (callers that must not overtake them, e.g. a resource
    /// destroy that follows destroys of its views).
    pub fn run_ready_retired(&self) {
        self.run_retired(false);
    }

    fn run_retired(&self, all: bool) {
        if !self.retire_pending.load(Ordering::Acquire) {
            return;
        }
        let done = self.read.0.load(Ordering::Acquire);
        let ready: Vec<Retire> = {
            let mut q = lock(&self.retire);
            let mut ready = Vec::new();
            let mut i = 0;
            while i < q.len() {
                if all || q[i].pos <= done {
                    ready.push(q.remove(i).expect("index in range"));
                } else {
                    i += 1;
                }
            }
            self.retire_pending.store(!q.is_empty(), Ordering::Release);
            ready
        };
        for r in ready {
            (r.run)();
        }
    }

    /// Record an error a worker-side call wanted to report. `priority` decides
    /// which error survives when several arrive before the producer reports
    /// them (higher wins; equal keeps the first).
    pub fn stash_error(&self, hr: i32, priority: impl Fn(i32) -> u8) {
        let mut cur = self.error.0.load(Ordering::Relaxed);
        loop {
            if cur != 0 && priority(cur) >= priority(hr) {
                return;
            }
            match self
                .error
                .0
                .compare_exchange(cur, hr, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }

    /// Take the stashed error, if any (one relaxed load when there is none).
    pub fn take_error(&self) -> Option<i32> {
        if self.error.0.load(Ordering::Relaxed) == 0 {
            return None;
        }
        match self.error.0.swap(0, Ordering::AcqRel) {
            0 => None,
            hr => Some(hr),
        }
    }

    /// Drain, run every queued destruction, stop and join the worker. Pushes
    /// after this fail with [`PushError::Stopped`]; retires run inline.
    pub fn shutdown(&self) {
        if self.is_abandoned() {
            // Process teardown: the worker is gone or about to be; joining or
            // replaying DXVK work now would only hang.
            self.hot.stop.store(true, Ordering::Release);
            return;
        }
        self.drain();
        self.hot.stop.store(true, Ordering::Release);
        self.kick();
        if let Some(t) = self.worker_thread.get() {
            t.unpark();
        }
        let handle = lock(&self.worker).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        // Worker never started, or exited: run what is left here. (Normally
        // nothing: the worker empties the ring before it exits and pushes
        // fail once `stop` is set.)
        //
        // ⚠ The join above would hang if this ran under the loader lock
        // outside process exit (a FreeLibrary teardown): the exiting worker
        // needs that lock. DestroyDevice is not called from DllMain.
        // SAFETY: nothing else consumes the ring once the worker has exited.
        while unsafe { self.run_one() } {}
        self.run_retired(true);
    }

    /// One bounded line of counters.
    pub fn summary(&self) -> String {
        let s = &self.stats.0;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "records={} bytes={} drains={} waits={} wait_us={} wait_max_us={} full_waits={} \
             parks={} wakes={} retire_queued={} retire_inline={}",
            r(&s.records),
            self.written(),
            r(&s.drains),
            r(&s.drain_waits),
            r(&s.drain_wait_us),
            r(&s.drain_wait_max_us),
            r(&s.full_waits),
            r(&s.parks),
            r(&s.wakes),
            r(&s.retire_queued),
            r(&s.retire_inline),
        )
    }

    /// Run the next record, if any. Consumer side only.
    unsafe fn run_one(&self) -> bool {
        let r = self.read.0.load(Ordering::Relaxed);
        if r == self.write_cache.0.load(Ordering::Relaxed) {
            let w = self.prod.write.load(Ordering::Acquire);
            if r == w {
                return false;
            }
            self.write_cache.0.store(w, Ordering::Relaxed);
        }
        let mask = self.hot.cap - 1;
        let pos = (r as usize) & mask;
        let tail = self.hot.cap - pos;
        if tail < HEADER_BYTES {
            self.read.0.store(r + tail as u64, Ordering::Release);
            return true;
        }
        // SAFETY: [r, w) holds published records; the header at `pos` was
        // written before `write` was released.
        unsafe {
            let p = self.hot.buf.add(pos);
            let h = core::ptr::read(p as *const Header);
            if let Some(call) = h.call {
                call(p.add(HEADER_BYTES), p.add(h.extra_off as usize), h.extra_len as usize);
            }
            self.read.0.store(r + h.len as u64, Ordering::Release);
        }
        true
    }

    /// After a batch of records or when the ring ran empty: wake waiters and
    /// run destructions the worker has passed.
    fn after_batch(&self) {
        fence(Ordering::SeqCst);
        if self.waiters.0.load(Ordering::Relaxed) != 0 {
            let _g = lock(&self.wait_lock);
            self.wait_cv.notify_all();
        }
        self.run_retired(false);
    }

    fn ring_empty(&self) -> bool {
        self.read.0.load(Ordering::Relaxed) == self.prod.write.load(Ordering::Acquire)
    }

    fn worker_main(self: Arc<Self>) {
        CURRENT_WORKER.with(|c| c.set(Arc::as_ptr(&self) as usize));
        let mut since_check = 0u32;
        let mut idle_parks = 0u32;
        loop {
            // SAFETY: this is the only consumer.
            if unsafe { self.run_one() } {
                since_check += 1;
                if since_check >= BATCH {
                    self.stats.0.records.fetch_add(u64::from(since_check), Ordering::Relaxed);
                    since_check = 0;
                    self.after_batch();
                }
                idle_parks = 0;
                continue;
            }
            self.stats.0.records.fetch_add(u64::from(since_check), Ordering::Relaxed);
            since_check = 0;
            self.after_batch();
            let t0 = Instant::now();
            while self.ring_empty() && t0.elapsed() < WORKER_SPIN && !self.hot.stop.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
            if !self.ring_empty() {
                continue;
            }
            if self.hot.stop.load(Ordering::Acquire) {
                break;
            }
            self.sleeping.0.store(true, Ordering::SeqCst);
            if !self.ring_empty() || self.hot.stop.load(Ordering::SeqCst) || self.retire_pending.load(Ordering::SeqCst) {
                self.sleeping.0.store(false, Ordering::Relaxed);
                if self.retire_pending.load(Ordering::Relaxed) {
                    self.run_retired(false);
                }
                continue;
            }
            self.stats.0.parks.fetch_add(1, Ordering::Relaxed);
            std::thread::park_timeout(match idle_parks {
                0 => PARK_SHORT,
                n if n < PARKS_BEFORE_LONG => PARK_MID,
                _ => PARK_LONG,
            });
            idle_parks = idle_parks.saturating_add(1);
            self.sleeping.0.store(false, Ordering::Relaxed);
        }
        // SAFETY: still the only consumer.
        while unsafe { self.run_one() } {}
        self.after_batch();
        CURRENT_WORKER.with(|c| c.set(0));
    }
}

impl Drop for CmdQueue {
    fn drop(&mut self) {
        if self.is_abandoned() {
            return; // leak the ring at process teardown
        }
        // `shutdown` is the contract; a queue dropped without it (tests, a
        // failed device creation) still runs what it holds, so no closure
        // leaks its captures.
        // SAFETY: no worker can exist: it holds an Arc to us.
        while unsafe { self.run_one() } {}
        self.run_retired(true);
        // SAFETY: allocated in `new` with the same layout.
        unsafe { dealloc(self.hot.buf, Layout::from_size_align(self.hot.cap, 64).expect("ring layout")) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn spin_until(cond: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !cond() {
            assert!(t0.elapsed() < Duration::from_secs(20), "timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn runs_in_order_with_extra_bytes_across_many_wraps() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-order");
        let seen = Arc::new(Mutex::new(Vec::new()));
        for i in 0..20_000u32 {
            let n = (i % 37) as usize; // varying sizes force pads at every tail length
            let seen = Arc::clone(&seen);
            unsafe {
                q.push(
                    n,
                    |b| b.iter_mut().enumerate().for_each(|(k, x)| *x = (i as usize + k) as u8),
                    move |extra: &[u8]| {
                        assert_eq!(extra.len(), n);
                        for (k, x) in extra.iter().enumerate() {
                            assert_eq!(*x, (i as usize + k) as u8);
                        }
                        lock(&seen).push(i);
                    },
                )
                .unwrap();
            }
        }
        q.drain();
        let seen = lock(&seen);
        assert_eq!(seen.len(), 20_000);
        assert!(seen.windows(2).all(|w| w[0] + 1 == w[1]));
        q.shutdown();
    }

    #[test]
    fn every_tail_length_is_handled() {
        // A ring of 4 KiB and records of exactly 8-byte steps hit tails of
        // 8, 16 (< header on 64-bit) and >= header.
        let q = CmdQueue::new(MIN_RING_BYTES, "t-tail");
        let count = Arc::new(AtomicU64::new(0));
        for round in 0..2000usize {
            let extra = (round * 8) % 200;
            let c = Arc::clone(&count);
            unsafe {
                q.push(extra, |_| {}, move |_: &[u8]| {
                    c.fetch_add(1, Ordering::Relaxed);
                })
                .unwrap();
            }
        }
        q.drain();
        assert_eq!(count.load(Ordering::Relaxed), 2000);
        q.shutdown();
    }

    #[test]
    fn too_large_records_are_refused() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-large");
        let r = unsafe { q.push(q.max_record(), |_| {}, |_: &[u8]| {}) };
        assert_eq!(r, Err(PushError::TooLarge));
        q.shutdown();
    }

    #[test]
    fn backpressure_never_overwrites_unread_records() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-full");
        let next = Arc::new(AtomicU64::new(0));
        for i in 0..3000u64 {
            let next = Arc::clone(&next);
            unsafe {
                q.push(
                    200,
                    |b| b.fill((i & 0xff) as u8),
                    move |extra: &[u8]| {
                        assert!(extra.iter().all(|&x| x == (i & 0xff) as u8));
                        assert_eq!(next.fetch_add(1, Ordering::Relaxed), i);
                        if i % 97 == 0 {
                            std::thread::sleep(Duration::from_micros(200));
                        }
                    },
                )
                .unwrap();
            }
        }
        q.drain();
        assert_eq!(next.load(Ordering::Relaxed), 3000);
        assert!(q.summary().contains("full_waits="));
        q.shutdown();
    }

    #[test]
    fn closures_are_dropped_exactly_once() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-drop");
        let token = Arc::new(());
        for _ in 0..500 {
            let t = Arc::clone(&token);
            unsafe { q.push(0, |_| {}, move |_: &[u8]| drop(t)).unwrap() };
        }
        q.drain();
        assert_eq!(Arc::strong_count(&token), 1);
        q.shutdown();
    }

    #[test]
    fn wait_done_covers_exactly_the_records_before_the_position() {
        let q = CmdQueue::new(1 << 16, "t-wait");
        let gate = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicU64::new(0));
        let g = Arc::clone(&gate);
        let r1 = Arc::clone(&ran);
        let p1 = unsafe {
            q.push(0, |_| {}, move |_: &[u8]| {
                while !g.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                r1.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap()
        };
        let r2 = Arc::clone(&ran);
        let p2 = unsafe {
            q.push(0, |_| {}, move |_: &[u8]| {
                r2.fetch_add(10, Ordering::Relaxed);
            })
            .unwrap()
        };
        assert!(p2 > p1);
        assert!(q.done() < p1);
        let waiter = {
            let q = Arc::clone(&q);
            let ran = Arc::clone(&ran);
            std::thread::spawn(move || {
                q.wait_done(p1);
                assert!(ran.load(Ordering::Relaxed) >= 1);
            })
        };
        std::thread::sleep(Duration::from_millis(5));
        gate.store(true, Ordering::Release);
        waiter.join().unwrap();
        q.wait_done(p2);
        assert_eq!(ran.load(Ordering::Relaxed), 11);
        q.shutdown();
    }

    #[test]
    fn retire_waits_for_the_worker_and_runs_inline_when_idle() {
        let q = CmdQueue::new(1 << 16, "t-retire");
        let gate = Arc::new(AtomicBool::new(false));
        let used = Arc::new(AtomicBool::new(false));
        let freed = Arc::new(AtomicBool::new(false));
        let (g, u) = (Arc::clone(&gate), Arc::clone(&used));
        unsafe {
            q.push(0, |_| {}, move |_: &[u8]| {
                while !g.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                u.store(true, Ordering::Release);
            })
            .unwrap();
        }
        let pos = q.written();
        let (u, f) = (Arc::clone(&used), Arc::clone(&freed));
        // From another thread, as a free-threaded destroy would.
        let destroyer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || unsafe {
                q.retire(pos, move || {
                    assert!(u.load(Ordering::Acquire), "retired before its last use ran");
                    f.store(true, Ordering::Release);
                });
            })
        };
        destroyer.join().unwrap();
        assert!(!freed.load(Ordering::Acquire));
        gate.store(true, Ordering::Release);
        spin_until(|| freed.load(Ordering::Acquire));

        // Idle worker: inline.
        q.drain();
        let inline = Arc::new(AtomicBool::new(false));
        let i2 = Arc::clone(&inline);
        unsafe { q.retire(q.written(), move || i2.store(true, Ordering::Release)) };
        assert!(inline.load(Ordering::Acquire));
        q.shutdown();
    }

    #[test]
    fn producer_threads_can_change_under_an_external_lock() {
        let q = CmdQueue::new(1 << 14, "t-multi");
        let produce = Arc::new(Mutex::new(0u64)); // the "runtime device lock"
        let seen = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (q, produce, seen) = (Arc::clone(&q), Arc::clone(&produce), Arc::clone(&seen));
                std::thread::spawn(move || {
                    for _ in 0..5000 {
                        let mut seq = lock(&produce);
                        let me = *seq;
                        *seq += 1;
                        let seen2 = Arc::clone(&seen);
                        unsafe {
                            q.push(16, |b| b.copy_from_slice(&[me as u8; 16]), move |extra: &[u8]| {
                                assert!(extra.iter().all(|&x| x == me as u8));
                                assert_eq!(seen2.fetch_add(1, Ordering::Relaxed), me);
                            })
                            .unwrap();
                        }
                        if me % 1000 == 0 {
                            q.drain();
                            assert!(seen.load(Ordering::Relaxed) > me);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        q.drain();
        assert_eq!(seen.load(Ordering::Relaxed), 20_000);
        q.shutdown();
    }

    #[test]
    fn worker_parks_and_is_woken_by_a_push() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-park");
        let n = Arc::new(AtomicU64::new(0));
        for round in 0..20 {
            let c = Arc::clone(&n);
            unsafe { q.push(0, |_| {}, move |_: &[u8]| { c.fetch_add(1, Ordering::Relaxed); }).unwrap() };
            // Long enough for the worker to spin out and park.
            std::thread::sleep(Duration::from_millis(3));
            spin_until(|| n.load(Ordering::Relaxed) == round + 1);
        }
        q.shutdown();
    }

    #[test]
    fn thread_init_runs_on_the_worker_with_the_producer_value() {
        static SEEN: AtomicU64 = AtomicU64::new(0);
        let q = CmdQueue::new(MIN_RING_BYTES, "t-init");
        q.set_thread_init(|| 42, |v| SEEN.store(v as u64, Ordering::Relaxed));
        unsafe { q.push(0, |_| {}, |_: &[u8]| {}).unwrap() };
        q.drain();
        assert_eq!(SEEN.load(Ordering::Relaxed), 42);
        q.shutdown();
    }

    #[test]
    fn errors_keep_the_highest_priority() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-err");
        let prio = |hr: i32| if hr == -2 { 2 } else { 1 };
        assert_eq!(q.take_error(), None);
        q.stash_error(-1, prio);
        q.stash_error(-3, prio); // same priority: first stays
        q.stash_error(-2, prio); // higher: replaces
        q.stash_error(-1, prio); // lower: ignored
        assert_eq!(q.take_error(), Some(-2));
        assert_eq!(q.take_error(), None);
        q.shutdown();
    }

    #[test]
    fn shutdown_runs_pending_records_and_retires_and_refuses_new_pushes() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-stop");
        let n = Arc::new(AtomicU64::new(0));
        for _ in 0..100 {
            let c = Arc::clone(&n);
            unsafe { q.push(0, |_| {}, move |_: &[u8]| { c.fetch_add(1, Ordering::Relaxed); }).unwrap() };
        }
        q.shutdown();
        assert_eq!(n.load(Ordering::Relaxed), 100);
        assert_eq!(unsafe { q.push(0, |_| {}, |_: &[u8]| {}) }, Err(PushError::Stopped));
        let r = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&r);
        unsafe { q.retire(u64::MAX, move || r2.store(true, Ordering::Relaxed)) };
        assert!(r.load(Ordering::Relaxed));
    }

    /// Producer-side cost per record with the worker consuming:
    /// `cargo test --release --lib -- --ignored --nocapture producer_cost`.
    #[test]
    #[ignore]
    fn producer_cost() {
        let q = CmdQueue::new(4 << 20, "t-bench");
        let sink = Arc::new(AtomicU64::new(0));
        let n = 2_000_000u64;
        let t0 = Instant::now();
        for i in 0..n {
            let s = Arc::as_ptr(&sink) as usize;
            unsafe {
                q.push(32, |b| b.fill(i as u8), move |extra: &[u8]| {
                    (*(s as *const AtomicU64)).fetch_add(extra[0] as u64, Ordering::Relaxed);
                })
                .unwrap();
            }
        }
        let push = t0.elapsed();
        q.drain();
        let all = t0.elapsed();
        println!(
            "push {:.1} ns/record, push+drain {:.1} ns/record, {}",
            push.as_nanos() as f64 / n as f64,
            all.as_nanos() as f64 / n as f64,
            q.summary()
        );
        q.shutdown();
    }

    #[test]
    fn abandoned_queue_waits_return() {
        static GONE: AtomicBool = AtomicBool::new(false);
        let q = CmdQueue::new(MIN_RING_BYTES, "t-abandon");
        q.set_abandon_check(|| GONE.load(Ordering::Relaxed));
        let gate = Arc::new(AtomicBool::new(false));
        let g = Arc::clone(&gate);
        unsafe {
            q.push(0, |_| {}, move |_: &[u8]| {
                while !g.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            })
            .unwrap();
        }
        GONE.store(true, Ordering::Relaxed);
        assert!(q.drain()); // returns although the record has not run
        GONE.store(false, Ordering::Relaxed);
        gate.store(true, Ordering::Release);
        q.shutdown();
    }

    #[test]
    fn record_bytes_matches_the_push_limit() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-size");
        let max = q.max_record();
        let ok = max - CmdQueue::record_bytes::<[u8; 0]>(0);
        assert!(CmdQueue::record_bytes::<[u8; 0]>(ok) <= max);
        assert!(unsafe { q.push(ok, |_| {}, |_: &[u8]| {}) }.is_ok());
        assert!(CmdQueue::record_bytes::<[u8; 0]>(ok + 8) > max);
        q.shutdown();
    }

    #[test]
    fn is_worker_is_true_only_inside_records() {
        let q = CmdQueue::new(MIN_RING_BYTES, "t-tid");
        assert!(!q.is_worker());
        let inside = Arc::new(AtomicBool::new(false));
        let (q2, i2) = (Arc::clone(&q), Arc::clone(&inside));
        unsafe {
            q.push(0, |_| {}, move |_: &[u8]| {
                i2.store(q2.is_worker() && CmdQueue::on_any_worker(), Ordering::Relaxed);
                // A drain from inside a record must not deadlock.
                assert!(!q2.drain());
            })
            .unwrap();
        }
        q.drain();
        assert!(inside.load(Ordering::Relaxed));
        q.shutdown();
    }
}
