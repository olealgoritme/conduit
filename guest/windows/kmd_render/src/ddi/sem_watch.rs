//! `NvSemWatch` (+ `NvSemWatchHi`): one GPU semaphore VA watched end to end, to find who
//! writes it (layout and encodings: `helios_kmd_logic::sem_watch`). Off (the default) costs one
//! relaxed load per hook.
//!
//! * Releases (`NvSwR*`): every native HW-queue submission is scanned at submit (PASSIVE, its
//!   GPFIFO entries and segments read as the GPU fetches them, bounded) for semaphore releases
//!   to the VA; copy-context channel submissions (`nv_copy`, DISPATCH) are queued and scanned
//!   by the mirror pass; the copy context's CPU executor reports its releases directly.
//!   Submissions this KMD completes without running anything (an NVDA HW-queue context on a
//!   node without a channel, an NVDA SubmitCommand context without a channel or record) are
//!   counted per source: `NvSwDrop<src>` count, `NvSwDropNd<src>` node mask, `NvSwDropT<src>`
//!   last time (ms); their releases can never happen.
//! * Value (`NvSwV*`): the mirror pass samples the 8 (or 4) bytes at the VA in every process
//!   that maps it, through the KMD's GPU-side mapping table (what the GPU reads), every
//!   `sem_watch::SAMPLE_MS`, and records each change; `NvSwVal` / `NvSwValHi` the last value,
//!   `NvSwVp` its process key (low 32 bits), `NvSwVs` the read status (`native_snap::RD_*`).
//! * CPU mappings (`NvSwM*`): `MapCpuHostAperture` / `UnmapCpuHostAperture` of the allocation
//!   holding the VA (`NvSwAlloc`, found by the mirror pass; events before that are kept in an
//!   early ring of every native allocation and moved over), its paging content operations
//!   (`pg_check::op_word`) and its destruction. The page is also added to the page-table watch
//!   (`native_memory::watch`, `NvWa*` / `NvWc*`).
//!
//! Counters: `NvSwOn` (VA low 32 bits in force), `NvSwSub` native submissions scanned,
//! `NvSwSeg` segments scanned, `NvSwRdF` segment reads that failed, `NvSwIrql` hooks not at
//! PASSIVE, `NvSwCq` / `NvSwCqLost` copy submissions queued / lost (queue full).
//! Registry writes happen on the mirror thread only.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::native_snap as ns;
use helios_kmd_logic::sem_watch::{self as sw, map_op, src, Ring, RING};

use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::native_memory::{self as nmem, VaHit};

/// The watched VA (0 = off).
static TARGET: AtomicU64 = AtomicU64::new(0);
/// The allocation holding it (0 = not found yet).
static ALLOC_ID: AtomicU32 = AtomicU32::new(0);
static LAST_KNOB_MS: AtomicU32 = AtomicU32::new(0);
static LAST_SAMPLE_MS: AtomicU32 = AtomicU32::new(0);

static RELEASES: SpinLock<Ring> = SpinLock::new(Ring::new());
static VALUES: SpinLock<Ring> = SpinLock::new(Ring::new());
static MAPS: SpinLock<Ring> = SpinLock::new(Ring::new());
/// Every native allocation's map events until the watched one is known.
const EARLY: usize = 128;
static EARLY_MAPS: SpinLock<([[u32; 4]; EARLY], u32)> = SpinLock::new(([[0; 4]; EARLY], 0));

/// The last sampled value per process (key, value, valid).
const PROCS: usize = 4;
static LAST_VALUES: SpinLock<[(usize, u64, bool); PROCS]> = SpinLock::new([(0, 0, false); PROCS]);

static SUBMITS: AtomicU32 = AtomicU32::new(0);
static SEGMENTS: AtomicU32 = AtomicU32::new(0);
static READ_FAILS: AtomicU32 = AtomicU32::new(0);
static NOT_PASSIVE: AtomicU32 = AtomicU32::new(0);
static COPY_QUEUED: AtomicU32 = AtomicU32::new(0);
static COPY_LOST: AtomicU32 = AtomicU32::new(0);
static DROPS: [AtomicU32; src::COUNT] = [const { AtomicU32::new(0) }; src::COUNT];
static DROP_NODES: [AtomicU32; src::COUNT] = [const { AtomicU32::new(0) }; src::COUNT];
static DROP_T: [AtomicU32; src::COUNT] = [const { AtomicU32::new(0) }; src::COUNT];

/// A copy-context submission waiting for the mirror pass's scan.
#[derive(Clone, Copy)]
struct CopyJob {
    process: usize,
    node: u32,
    t_ms: u32,
    n: u32,
    entries: [(u64, u32); COPY_ENTRIES],
}
const COPY_ENTRIES: usize = 16;
const COPY_JOBS: usize = 32;
const NO_JOB: CopyJob = CopyJob { process: 0, node: 0, t_ms: 0, n: 0, entries: [(0, 0); COPY_ENTRIES] };
static COPY_PENDING: SpinLock<([CopyJob; COPY_JOBS], usize)> = SpinLock::new(([NO_JOB; COPY_JOBS], 0));

/// The watched VA, if the watch is on. Any IRQL.
pub(crate) fn target() -> Option<u64> {
    match TARGET.load(Ordering::Relaxed) {
        0 => None,
        va => Some(va),
    }
}

/// The watched VA's 4-KiB page (`native_memory::watch`). Any IRQL.
pub(crate) fn page() -> Option<u64> {
    target().map(|va| va >> 12)
}

/// How long the mirror thread may sleep without a request (ms): the sampling period while the
/// watch is on.
pub(crate) fn wake_ms(default_ms: i64) -> i64 {
    if target().is_some() {
        i64::from(sw::SAMPLE_MS).min(default_ms)
    } else {
        default_ms
    }
}

fn now_ms() -> u32 {
    AdapterContext::interrupt_time_ms()
}

fn heap_scanner() -> Option<Vec<ns::Scanner>> {
    let mut v: Vec<ns::Scanner> = Vec::new();
    v.try_reserve_exact(1).ok()?;
    v.push(ns::Scanner::new(ns::DEFAULT_CLASSES));
    Some(v)
}

/// Scan one segment of `process` for releases to the target; `false` when a read failed.
fn scan_segment(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    scanner: &mut ns::Scanner,
    words: &mut [u32],
    process: usize,
    va: u64,
    dwords: u32,
    limit: u32,
) -> bool {
    scanner.begin_segment(u32::MAX);
    let len = dwords.min(limit);
    let mut done = 0u32;
    while done < len {
        let at = va + 4 * u64::from(done);
        let n = ((len - done) as usize).min(words.len()).min(((0x1000 - (at & 0xfff)) / 4) as usize);
        if crate::ddi::native_snap::read_gpu_words(passive, adapter, process, at, &mut words[..n]) != ns::RD_OK {
            READ_FAILS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        for &d in &words[..n] {
            if !scanner.feed(d) {
                return true;
            }
        }
        done += n as u32;
    }
    true
}

fn record_hits(scanner: &ns::Scanner, source: u32, node: u32, chid: u32, who: u32, t: u32) {
    if scanner.hits_seen == 0 {
        return;
    }
    let mut ring = RELEASES.lock();
    for op in scanner.hits() {
        ring.push([sw::release_word(source, op.kind, node, op.op, chid), op.payload as u32, t, who]);
    }
}

/// After a successful native `SubmitCommandToHwQueue` (PASSIVE): scan the submission's
/// segments for releases to the target.
pub(crate) fn after_native_submit(adapter: &AdapterContext, queue: &crate::virtio::native_channel::NativeQueue) {
    let Some(target) = target() else {
        return;
    };
    let Some(passive) = PassiveLevel::try_assume() else {
        NOT_PASSIVE.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(view) = queue.check_view() else {
        return;
    };
    let entries = view.entries.max(1);
    let span = (view.after % entries + entries - view.before % entries) % entries;
    let count = span.min(sw::SUBMIT_SEGMENTS);
    let Some(mut scanner) = heap_scanner() else {
        return;
    };
    let scanner = &mut scanner[0];
    scanner.set_watch(&[target]);
    let mut words = [0u32; 256];
    let mut gp = [0u32; 2];
    let t = now_ms();
    let chid = ns::token_chid(queue.token().0);
    let who = queue.pid();
    SUBMITS.fetch_add(1, Ordering::Relaxed);
    for k in 0..count {
        let index = (view.before + k) % entries;
        if crate::ddi::native_snap::read_gpu_words(passive, adapter, view.process_key, view.gpfifo_va + 8 * u64::from(index), &mut gp)
            != ns::RD_OK
        {
            READ_FAILS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let e = ns::gp_entry(gp[0], gp[1]);
        if e.dwords == 0 {
            continue;
        }
        SEGMENTS.fetch_add(1, Ordering::Relaxed);
        scan_segment(passive, adapter, scanner, &mut words, view.process_key, e.va, e.dwords, sw::SUBMIT_SEG_DWORDS);
        record_hits(scanner, src::NATIVE, queue.veid(), chid, who, t);
    }
}

/// A copy-context channel submission (`nv_copy::admit`, DISPATCH): queued for the mirror
/// pass's scan.
pub(crate) fn note_copy_submit(process: usize, node: u32, entries: &[helios_kmd_logic::nv_copy::GpEntry]) {
    if target().is_none() {
        return;
    }
    let mut job = NO_JOB;
    job.process = process;
    job.node = node;
    job.t_ms = now_ms();
    for (slot, e) in job.entries.iter_mut().zip(entries.iter()) {
        *slot = (e.va, e.dwords);
        job.n += 1;
    }
    let mut pending = COPY_PENDING.lock();
    if pending.1 >= COPY_JOBS {
        COPY_LOST.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let n = pending.1;
    pending.0[n] = job;
    pending.1 += 1;
    COPY_QUEUED.fetch_add(1, Ordering::Relaxed);
}

/// The copy context's CPU executor released `payload` at `va` of `process` (PASSIVE).
pub(crate) fn note_cpu_release(process: usize, va: u64, payload: u64) {
    let Some(target) = target() else {
        return;
    };
    if va != target && va.wrapping_add(4) != target {
        return;
    }
    RELEASES.lock().push([sw::release_word(src::COPY_CPU, 0, 0, 0, 0), payload as u32, now_ms(), process as u32]);
}

/// A submission completed without running anything (its releases never happen). Any IRQL.
pub(crate) fn note_drop(source: u32, node: u32) {
    if target().is_none() {
        return;
    }
    let i = (source as usize).min(src::COUNT - 1);
    DROPS[i].fetch_add(1, Ordering::Relaxed);
    DROP_NODES[i].fetch_or(1u32.checked_shl(node).unwrap_or(0), Ordering::Relaxed);
    DROP_T[i].store(now_ms(), Ordering::Relaxed);
}

/// A CPU-mapping / lifetime event of native allocation `id` (`map_op`). Any IRQL.
pub(crate) fn note_map(id: u32, op: u32, b: u32, d: u32) {
    if target().is_none() || id == 0 {
        return;
    }
    let rec = [sw::map_word(op, id), b, now_ms(), d];
    if ALLOC_ID.load(Ordering::Relaxed) == id {
        MAPS.lock().push(rec);
    } else {
        let mut early = EARLY_MAPS.lock();
        let n = early.1;
        early.0[n as usize % EARLY] = rec;
        early.1 = n.wrapping_add(1);
    }
}

/// A paging content operation of native allocation `id` (`pg_check::op_word`). Any IRQL.
pub(crate) fn note_paging(id: u32, word: u32) {
    note_map(id, map_op::PAGING, word, 0);
}

/// The mirror pass (PASSIVE): knobs, the value sample, the queued copy scans, publishing.
pub(crate) fn tick(adapter: &AdapterContext) {
    if !crate::ddi::wddm_surface::nvidia_selected() {
        return;
    }
    let Some(passive) = PassiveLevel::try_assume() else {
        return;
    };
    let now = now_ms().max(1);
    let last = LAST_KNOB_MS.load(Ordering::Relaxed);
    if last == 0 || now.wrapping_sub(last) >= 1_000 {
        LAST_KNOB_MS.store(now, Ordering::Relaxed);
        let lo = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSemWatch"), 0);
        let hi = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSemWatchHi"), 0);
        let va = sw::target(lo, hi).unwrap_or(0);
        if TARGET.swap(va, Ordering::Relaxed) != va {
            ALLOC_ID.store(0, Ordering::Relaxed);
            *LAST_VALUES.lock() = [(0, 0, false); PROCS];
        }
        if va != 0 {
            crate::diag::record_named_bytes(b"NvSwOn", va as u32);
        }
    }
    let Some(target) = target() else {
        return;
    };
    let sampled = LAST_SAMPLE_MS.load(Ordering::Relaxed);
    if sampled == 0 || now.wrapping_sub(sampled) >= sw::SAMPLE_MS {
        LAST_SAMPLE_MS.store(now, Ordering::Relaxed);
        sample(passive, adapter, target, now);
    }
    scan_copy_jobs(passive, adapter, target);
    publish();
}

fn sample(passive: PassiveLevel, adapter: &AdapterContext, target: u64, now: u32) {
    let mut hits = [VaHit::default(); PROCS];
    let n = nmem::lookup_va(0, target, &mut hits);
    for h in &hits[..n] {
        if (h.kind == 1 || h.kind == 2) && h.id != 0 && ALLOC_ID.load(Ordering::Relaxed) == 0 {
            ALLOC_ID.store(h.id, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"NvSwAlloc", h.id);
            crate::diag::record_named_bytes(b"NvSwAllocMem", h.memory);
            // Events of this allocation from before it was known.
            let early = *EARLY_MAPS.lock();
            let kept = (early.1 as usize).min(EARLY);
            let mut maps = MAPS.lock();
            for k in 0..kept {
                let rec = early.0[(early.1 as usize - kept + k) % EARLY];
                if rec[0] & 0xff_ffff == h.id & 0xff_ffff {
                    maps.push(rec);
                }
            }
        }
        let mut words = [0u32; 2];
        let n_words = if target % 8 == 0 { 2 } else { 1 };
        let status = crate::ddi::native_snap::read_gpu_words(passive, adapter, h.process, target & !3, &mut words[..n_words]);
        crate::diag::record_named_bytes(b"NvSwVs", status);
        if status != ns::RD_OK {
            continue;
        }
        let value = u64::from(words[0]) | u64::from(words[1]) << 32;
        let changed = {
            let mut last = LAST_VALUES.lock();
            match last.iter_mut().find(|(p, _, valid)| *valid && *p == h.process) {
                Some(slot) => {
                    let changed = slot.1 != value;
                    slot.1 = value;
                    changed
                }
                None => {
                    if let Some(slot) = last.iter_mut().find(|(_, _, valid)| !*valid) {
                        *slot = (h.process, value, true);
                    }
                    true
                }
            }
        };
        if changed {
            VALUES.lock().push([value as u32, (value >> 32) as u32, now, h.process as u32]);
        }
        crate::diag::record_named_bytes(b"NvSwVal", value as u32);
        crate::diag::record_named_bytes(b"NvSwValHi", (value >> 32) as u32);
        crate::diag::record_named_bytes(b"NvSwVp", h.process as u32);
    }
}

fn scan_copy_jobs(passive: PassiveLevel, adapter: &AdapterContext, target: u64) {
    let mut jobs = [NO_JOB; COPY_JOBS];
    let n = {
        let mut pending = COPY_PENDING.lock();
        let n = pending.1;
        jobs[..n].copy_from_slice(&pending.0[..n]);
        pending.1 = 0;
        n
    };
    if n == 0 {
        return;
    }
    let Some(mut scanner) = heap_scanner() else {
        return;
    };
    let scanner = &mut scanner[0];
    scanner.set_watch(&[target]);
    let mut words = [0u32; 256];
    for job in &jobs[..n] {
        for &(va, dwords) in &job.entries[..job.n as usize] {
            if dwords == 0 {
                continue;
            }
            SEGMENTS.fetch_add(1, Ordering::Relaxed);
            scan_segment(passive, adapter, scanner, &mut words, job.process, va, dwords, sw::SUBMIT_SEG_DWORDS);
            record_hits(scanner, src::COPY_CHANNEL, job.node, 0, job.process as u32, job.t_ms);
        }
    }
}

fn publish_ring(ring: &SpinLock<Ring>, letter: u8, count_name: &[u8]) {
    let mut out = [(0usize, [0u32; 4]); RING];
    let (k, total) = {
        let mut r = ring.lock();
        let k = r.take_new(&mut out);
        (k, r.n)
    };
    if k == 0 {
        return;
    }
    for (slot, rec) in &out[..k] {
        for (f, v) in rec.iter().enumerate() {
            crate::diag::record_named_bytes(&sw::name(letter, b'a' + f as u8, *slot), *v);
        }
    }
    crate::diag::record_named_bytes(count_name, total);
}

fn publish() {
    publish_ring(&RELEASES, b'R', b"NvSwRN");
    publish_ring(&VALUES, b'V', b"NvSwVN");
    publish_ring(&MAPS, b'M', b"NvSwMN");
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvSwSub", r(&SUBMITS));
    crate::diag::record_named_bytes(b"NvSwSeg", r(&SEGMENTS));
    crate::diag::record_named_bytes(b"NvSwRdF", r(&READ_FAILS));
    crate::diag::record_named_bytes(b"NvSwIrql", r(&NOT_PASSIVE));
    crate::diag::record_named_bytes(b"NvSwCq", r(&COPY_QUEUED));
    crate::diag::record_named_bytes(b"NvSwCqLost", r(&COPY_LOST));
    for s in [src::SOFT_DROP, src::COPY_DROP] {
        let i = s as usize;
        let n = r(&DROPS[i]);
        if n == 0 {
            continue;
        }
        let digit = b'0' + s as u8;
        crate::diag::record_named_bytes(&[b'N', b'v', b'S', b'w', b'D', b'r', b'o', b'p', digit], n);
        crate::diag::record_named_bytes(
            &[b'N', b'v', b'S', b'w', b'D', b'r', b'o', b'p', b'N', b'd', digit],
            r(&DROP_NODES[i]),
        );
        crate::diag::record_named_bytes(&[b'N', b'v', b'S', b'w', b'D', b'r', b'o', b'p', b'T', digit], r(&DROP_T[i]));
    }
}
