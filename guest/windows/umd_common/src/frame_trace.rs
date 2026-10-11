//! Per-frame timing trace (`UmdFrameTrace`): where the presenting thread's
//! time between two Presents went, one CSV line per Present.
//!
//! The question it answers is "is a slow frame slow inside the driver or
//! outside it". The presenting thread accumulates, per frame, the time it was
//! blocked at each driver wait point (front-end drains, synchronous maps,
//! GetData, flushes, a full command ring, the drain at Present) plus its DDI
//! call count and the front-end worker's busy time; at Present the totals go
//! into a lock-free ring as one record and are reset. A dumper thread appends
//! the ring to `C:\ProgramData\Helios\frametrace-<pid>.csv` a few times a
//! second, so the producer never touches a file or a lock.
//!
//! Timestamps are raw performance-counter ticks; the dumper converts them to
//! milliseconds. `qpc_ms` is the QPC time in ms, the same base as PresentMon's
//! `CPUStartQPCTime` with `--qpc_time_ms`, so the two files join on time.
//!
//! Pure `std` (the clock and the file are the caller's): builds and tests on
//! the Linux host.

use core::sync::atomic::{fence, AtomicU64, Ordering};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// What one frame accumulates. The discriminant indexes [`FrameAcc`] and the
/// accumulator part of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Acc {
    /// Immediate-context DDI calls that entered the front end.
    DdiCalls,
    /// Front-end drains for synchronous calls (all reasons but Present):
    /// count and ticks.
    SyncN,
    SyncTicks,
    /// Whole synchronous Map/Unmap calls (drain + DXVK's map, which may wait
    /// for the GPU): count and ticks.
    MapN,
    MapTicks,
    /// Whole GetData calls answered on the app thread.
    GetDataN,
    GetDataTicks,
    /// Whole Flush calls.
    FlushN,
    FlushTicks,
    /// Pushes that found the command ring full and waited for the worker.
    FullN,
    FullTicks,
    /// Discard maps that waited because too many discarded bytes were queued.
    DiscardWaitN,
    /// Front-end worker time spent running records (not spinning or parked).
    WorkerBusyTicks,
}

/// Number of accumulators.
pub const ACCS: usize = Acc::WorkerBusyTicks as usize + 1;

/// Words of a record: present QPC, interval, present-drain ticks, then the
/// accumulators.
pub const WORDS: usize = 3 + ACCS;

/// One frame's record.
pub type Record = [u64; WORDS];

/// The CSV header matching [`format_line`].
pub const CSV_HEADER: &str = "dev,qpc_ms,interval_ms,present_drain_ms,ddi_calls,sync_n,sync_ms,map_n,map_ms,\
getdata_n,getdata_ms,flush_n,flush_ms,full_n,full_ms,discard_wait_n,worker_busy_ms,blocked_ms";

/// The per-frame accumulators. Mostly written by the presenting thread;
/// atomics because free-threaded calls (destroys) may add from elsewhere.
#[derive(Default)]
pub struct FrameAcc {
    a: [AtomicU64; ACCS],
}

impl FrameAcc {
    #[inline(always)]
    pub fn add(&self, which: Acc, v: u64) {
        self.a[which as usize].fetch_add(v, Ordering::Relaxed);
    }

    /// Count one event of `n` and its duration in `ticks`.
    #[inline(always)]
    pub fn event(&self, n: Acc, ticks: Acc, dt: u64) {
        self.add(n, 1);
        self.add(ticks, dt);
    }

    /// Take the totals and reset them.
    pub fn take(&self) -> [u64; ACCS] {
        core::array::from_fn(|i| self.a[i].swap(0, Ordering::Relaxed))
    }
}

/// A slot: a sequence word (odd while being written, `2 * index + 2` once
/// record `index` is complete) and the record.
struct Slot {
    seq: AtomicU64,
    w: [AtomicU64; WORDS],
}

/// Single-producer ring of frame records with a seqlock per slot: the
/// producer never waits; a consumer that falls behind by more than the ring
/// loses the oldest records and counts them.
pub struct Ring {
    slots: Box<[Slot]>,
    mask: u64,
    write: AtomicU64,
    /// Consumer state: next index to read, records lost.
    read: Mutex<(u64, u64)>,
    /// Device number in the CSV.
    pub dev: u32,
}

impl Ring {
    /// A ring of `cap` records (rounded up to a power of two, at least 16).
    pub fn new(cap: usize, dev: u32) -> Self {
        let cap = cap.max(16).next_power_of_two();
        let slots = (0..cap)
            .map(|_| Slot {
                seq: AtomicU64::new(0),
                w: core::array::from_fn(|_| AtomicU64::new(0)),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            mask: cap as u64 - 1,
            write: AtomicU64::new(0),
            read: Mutex::new((0, 0)),
            dev,
        }
    }

    /// Append a record. One producer at a time.
    pub fn push(&self, r: &Record) {
        let i = self.write.load(Ordering::Relaxed);
        let s = &self.slots[(i & self.mask) as usize];
        s.seq.store(2 * i + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        for (d, v) in s.w.iter().zip(r) {
            d.store(*v, Ordering::Relaxed);
        }
        s.seq.store(2 * i + 2, Ordering::Release);
        self.write.store(i + 1, Ordering::Release);
    }

    /// Records pushed so far.
    pub fn written(&self) -> u64 {
        self.write.load(Ordering::Acquire)
    }

    /// Move every complete record not yet read into `out`. Returns the number
    /// of records lost so far (overwritten before they were read).
    pub fn drain_into(&self, out: &mut Vec<Record>) -> u64 {
        let mut st = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        let w = self.write.load(Ordering::Acquire);
        let cap = self.mask + 1;
        if w > st.0 + cap {
            st.1 += w - cap - st.0;
            st.0 = w - cap;
        }
        while st.0 < w {
            let i = st.0;
            let s = &self.slots[(i & self.mask) as usize];
            let s1 = s.seq.load(Ordering::Acquire);
            let r: Record = core::array::from_fn(|k| s.w[k].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            let s2 = s.seq.load(Ordering::Relaxed);
            if s1 == 2 * i + 2 && s2 == s1 {
                out.push(r);
            } else {
                st.1 += 1;
            }
            st.0 += 1;
        }
        st.1
    }
}

/// Ticks to milliseconds.
fn ms(ticks: u64, freq: u64) -> f64 {
    ticks as f64 * 1000.0 / freq.max(1) as f64
}

/// One CSV line (no newline) for record `r` of device `dev`.
pub fn format_line(dev: u32, r: &Record, freq: u64) -> String {
    let a = |x: Acc| r[3 + x as usize];
    let m = |x: Acc| ms(a(x), freq);
    let blocked = ms(
        r[2] + a(Acc::SyncTicks) + a(Acc::MapTicks) + a(Acc::GetDataTicks) + a(Acc::FlushTicks) + a(Acc::FullTicks),
        freq,
    );
    format!(
        "{dev},{:.4},{:.4},{:.4},{},{},{:.4},{},{:.4},{},{:.4},{},{:.4},{},{:.4},{},{:.4},{:.4}",
        ms(r[0], freq),
        ms(r[1], freq),
        ms(r[2], freq),
        a(Acc::DdiCalls),
        a(Acc::SyncN),
        m(Acc::SyncTicks),
        a(Acc::MapN),
        m(Acc::MapTicks),
        a(Acc::GetDataN),
        m(Acc::GetDataTicks),
        a(Acc::FlushN),
        m(Acc::FlushTicks),
        a(Acc::FullN),
        m(Acc::FullTicks),
        a(Acc::DiscardWaitN),
        m(Acc::WorkerBusyTicks),
        blocked,
    )
}

/// One device's tracer: the accumulators, the ring and the previous frame's
/// cumulative counters the next record subtracts.
pub struct FrameTracer {
    pub acc: FrameAcc,
    ring: Arc<Ring>,
    last_present: AtomicU64,
    last_full: (AtomicU64, AtomicU64),
    last_busy: AtomicU64,
}

impl FrameTracer {
    pub fn new(ring: Arc<Ring>) -> Self {
        Self {
            acc: FrameAcc::default(),
            ring,
            last_present: AtomicU64::new(0),
            last_full: (AtomicU64::new(0), AtomicU64::new(0)),
            last_busy: AtomicU64::new(0),
        }
    }

    /// Close the frame at a Present that entered at `t0` and finished its
    /// drain at `t1` (ticks). `full_n`, `full_ticks` and `worker_busy` are
    /// the queue's cumulative counters, turned into per-frame deltas here.
    pub fn present(&self, t0: u64, t1: u64, full_n: u64, full_ticks: u64, worker_busy: u64) {
        let prev = self.last_present.swap(t0, Ordering::Relaxed);
        let mut a = self.acc.take();
        a[Acc::FullN as usize] += full_n.saturating_sub(self.last_full.0.swap(full_n, Ordering::Relaxed));
        a[Acc::FullTicks as usize] += full_ticks.saturating_sub(self.last_full.1.swap(full_ticks, Ordering::Relaxed));
        a[Acc::WorkerBusyTicks as usize] += worker_busy.saturating_sub(self.last_busy.swap(worker_busy, Ordering::Relaxed));
        let mut r: Record = [0; WORDS];
        r[0] = t0;
        r[1] = if prev == 0 { 0 } else { t0.saturating_sub(prev) };
        r[2] = t1.saturating_sub(t0);
        r[3..].copy_from_slice(&a);
        self.ring.push(&r);
    }
}

/// Records each device ring holds before the dumper must have read them
/// (~16 s at 4000 fps).
pub const RING_RECORDS: usize = 1 << 16;
/// Lines after which the dumper stops writing (~1 h at 300 fps).
pub const MAX_LINES: u64 = 1 << 20;

/// The process-wide set of rings and the dumper thread.
struct Registry {
    rings: Vec<Arc<Ring>>,
    started: bool,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            rings: Vec::new(),
            started: false,
        })
    })
}

/// A new device ring, registered with the dumper; the first call starts the
/// dumper thread, which appends to whatever `open` returns (called once, on
/// the dumper thread) every `period`.
pub fn register(freq: u64, period: Duration, open: fn() -> Option<Box<dyn Write + Send>>) -> Arc<Ring> {
    let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
    let ring = Arc::new(Ring::new(RING_RECORDS, reg.rings.len() as u32));
    reg.rings.push(Arc::clone(&ring));
    if !reg.started {
        reg.started = true;
        let _ = std::thread::Builder::new()
            .name("helios-frame-trace".into())
            .spawn(move || dumper(freq, period, open));
    }
    ring
}

/// Write the CSV header, then every period the new records of every ring.
fn dumper(freq: u64, period: Duration, open: fn() -> Option<Box<dyn Write + Send>>) {
    let Some(mut out) = open() else {
        return;
    };
    let _ = writeln!(out, "{CSV_HEADER}");
    let mut lines = 0u64;
    let mut buf = Vec::new();
    let mut lost_seen = 0u64;
    while lines < MAX_LINES {
        std::thread::sleep(period);
        let rings = registry().lock().unwrap_or_else(PoisonError::into_inner).rings.clone();
        let mut lost = 0;
        for ring in &rings {
            buf.clear();
            lost += ring.drain_into(&mut buf);
            for r in &buf {
                let _ = writeln!(out, "{}", format_line(ring.dev, r, freq));
            }
            lines += buf.len() as u64;
        }
        if lost != lost_seen {
            let _ = writeln!(out, "# lost {} records", lost - lost_seen);
            lost_seen = lost;
        }
        let _ = out.flush();
    }
    let _ = writeln!(out, "# stopped after {lines} lines");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: u64) -> Record {
        core::array::from_fn(|k| i * 100 + k as u64)
    }

    #[test]
    fn ring_keeps_order_and_counts_losses() {
        let r = Ring::new(16, 0);
        let mut out = Vec::new();
        for i in 0..10 {
            r.push(&rec(i));
        }
        assert_eq!(r.drain_into(&mut out), 0);
        assert_eq!(out.len(), 10);
        assert_eq!(out[3], rec(3));
        out.clear();
        // 40 more: the consumer is 24 records behind a 16-record ring.
        for i in 10..50 {
            r.push(&rec(i));
        }
        assert_eq!(r.drain_into(&mut out), 24);
        assert_eq!(out.len(), 16);
        assert_eq!(out[0], rec(34));
        assert_eq!(out[15], rec(49));
        out.clear();
        assert_eq!(r.drain_into(&mut out), 24);
        assert!(out.is_empty());
    }

    #[test]
    fn ring_concurrent_reader_sees_only_whole_records() {
        let r = Arc::new(Ring::new(64, 0));
        let w = Arc::clone(&r);
        let n = 200_000u64;
        let t = std::thread::spawn(move || {
            for i in 0..n {
                let v = [i; WORDS];
                w.push(&v);
            }
        });
        let mut got = 0u64;
        let mut lost = 0;
        let mut last = None;
        let mut out = Vec::new();
        loop {
            out.clear();
            lost = r.drain_into(&mut out);
            for x in &out {
                assert!(x.iter().all(|v| *v == x[0]), "torn record {x:?}");
                if let Some(l) = last {
                    assert!(x[0] > l, "out of order");
                }
                last = Some(x[0]);
            }
            got += out.len() as u64;
            if t.is_finished() && r.written() == n {
                out.clear();
                lost = r.drain_into(&mut out);
                got += out.len() as u64;
                break;
            }
        }
        t.join().unwrap();
        assert_eq!(got + lost, n);
    }

    #[test]
    fn tracer_turns_cumulative_counters_into_deltas() {
        let ring = Arc::new(Ring::new(16, 3));
        let t = FrameTracer::new(Arc::clone(&ring));
        t.acc.add(Acc::DdiCalls, 5);
        t.acc.event(Acc::MapN, Acc::MapTicks, 70);
        t.present(1000, 1010, 2, 40, 500);
        t.acc.event(Acc::SyncN, Acc::SyncTicks, 9);
        t.present(4000, 4100, 3, 100, 1700);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out.len(), 2);
        let a = |r: &Record, x: Acc| r[3 + x as usize];
        assert_eq!(out[0][1], 0, "no interval before the first present");
        assert_eq!(out[0][2], 10);
        assert_eq!(a(&out[0], Acc::DdiCalls), 5);
        assert_eq!(a(&out[0], Acc::MapTicks), 70);
        assert_eq!(a(&out[0], Acc::FullN), 2);
        assert_eq!(a(&out[0], Acc::WorkerBusyTicks), 500);
        assert_eq!(out[1][1], 3000);
        assert_eq!(out[1][2], 100);
        assert_eq!(a(&out[1], Acc::DdiCalls), 0, "accumulators reset at present");
        assert_eq!(a(&out[1], Acc::SyncTicks), 9);
        assert_eq!(a(&out[1], Acc::FullN), 1);
        assert_eq!(a(&out[1], Acc::FullTicks), 60);
        assert_eq!(a(&out[1], Acc::WorkerBusyTicks), 1200);
    }

    #[test]
    fn csv_line_matches_header() {
        let mut r: Record = [0; WORDS];
        r[0] = 10_000_000; // 1 s at 10 MHz
        r[1] = 25_000;
        r[2] = 5_000;
        r[3 + Acc::DdiCalls as usize] = 1234;
        r[3 + Acc::SyncTicks as usize] = 10_000;
        let line = format_line(1, &r, 10_000_000);
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields.len(), CSV_HEADER.split(',').count());
        assert_eq!(fields[0], "1");
        assert_eq!(fields[1], "1000.0000");
        assert_eq!(fields[2], "2.5000");
        assert_eq!(fields[3], "0.5000");
        assert_eq!(fields[4], "1234");
        assert_eq!(fields[6], "1.0000");
        assert_eq!(*fields.last().unwrap(), "1.5000", "blocked = drain + sync");
    }
}
