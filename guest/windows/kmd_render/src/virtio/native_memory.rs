//! Native NVIDIA RM memory service for the 610.88 Windows UMD personality.
//!
//! This path does not create, import, or adopt a GEM/Venus object.  The parent
//! supplies an already brought-up host RM client (`NativeClient`); this module
//! adds a `FERMI_VASPACE_A(GPU_DEVICE)`, real memory objects, and fixed
//! `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` mappings at VidMm's exact VAs.
//!
//! Locking: one PASSIVE mutex serializes RM operations on published objects and
//! every lifetime change; its wait is bounded (`LOCK_MAX_MS`), so a holder stuck
//! on the host fails its waiters with `Busy` instead of blocking them forever.
//! `create` makes the RM object of a new, not yet published allocation outside
//! the mutex.  The leaf spinlock is used only to copy or publish plain state; it
//! is never held over a host round trip, an MDL operation, or an allocation.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::ce_route as cr;
use helios_kmd_logic::desc_cache::{DescTable, Released};
use helios_kmd_logic::hot_path::Paging;
use helios_kmd_logic::leaf_plan as lp;
use helios_kmd_logic::rm_batch as rb;
use helios_kmd_logic::native_memory as nm;
use helios_kmd_logic::native_perf as np;
use helios_kmd_logic::native_teardown as nt;
use helios_kmd_logic::pg_gpu;
use helios_kmd_logic::pg_phase::{self as pg, Acc, Kind, Phase};
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::pt_ring as ptr;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client as rc;
use helios_kmd_logic::va_window as vw;
use wdk_sys::ntddk::{
    MmMapIoSpace, MmMapLockedPagesSpecifyCache, MmUnmapIoSpace, MmUnmapLockedPages,
};
use wdk_sys::{MDL, NTSTATUS, PHYSICAL_ADDRESS, PMDL, _MEMORY_CACHING_TYPE};

use super::gpu::DeviceOwner;
use super::nvrm::{self, Refusal};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::{PassiveMutex, PassiveMutexGuard, SpinLock};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const TIMEOUT_MS: u64 = 2_500;
const REPLY_MAX: usize = 512;
const H_VASPACE: u32 = 0x4e57_0001;
const FIRST_DYNAMIC_HANDLE: u32 = 0x4e57_1000;
const PIN_CHUNK_BYTES: u64 = u32::MAX as u64 & !(nm::PAGE_BYTES - 1);
const LOCAL_4K_MAP_FLAGS: u32 = 1 << 8;


/// VA watch (test knobs `NvWatch0`..`NvWatch3`: 4-KiB page numbers, 0 = off). Every
/// page-table update, mapping, eviction and unmap that covers a watched page is
/// logged in a 32-entry ring: `NvWa<k>` = the operation's first VA page, `NvWc<k>`
/// = op << 24 | detail << 16 | process & 0xffff, `NvWN` = events so far. Ops:
/// 1 UpdatePageTable seen (detail: bit0 native, bit1 level != 0, bit2 PTE 0 valid,
/// bit3 PTE 0 system page, bit4 not PASSIVE, bit5 system process), 2/3 foreign
/// map ok/failed, 4 foreign drop, 5/6 native vidmem map ok/failed, 7/8 system map
/// ok/failed, 10 unmapped (also by an eviction; detail = the `Error` on failure).
pub mod watch {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    static PAGES: [AtomicU64; 4] = [const { AtomicU64::new(u64::MAX) }; 4];
    static READ: AtomicU32 = AtomicU32::new(0);
    static EVENTS: AtomicU32 = AtomicU32::new(0);

    fn pages() -> [u64; 4] {
        if READ.load(Ordering::Acquire) == 0 {
            let names: [&'static [u8]; 4] = [b"NvWatch0", b"NvWatch1", b"NvWatch2", b"NvWatch3"];
            for (slot, name) in PAGES.iter().zip(names) {
                let v = crate::diag::read_config_dword(crate::diag::KnobName::new(name), 0);
                slot.store(if v == 0 { u64::MAX } else { u64::from(v) }, Ordering::Relaxed);
            }
            READ.store(1, Ordering::Release);
        }
        [0, 1, 2, 3].map(|i| PAGES[i].load(Ordering::Relaxed))
    }

    /// Whether [va, va + len) covers a watched page.
    pub fn covers(va: u64, len: u64) -> bool {
        let first = va >> 12;
        let end = va.saturating_add(len.max(1)).saturating_add(0xfff) >> 12;
        pages().iter().any(|&p| p != u64::MAX && p >= first && p < end)
    }

    pub fn event(va: u64, len: u64, op: u32, detail: u32, process: usize) {
        // The page-table ring (`NvPtRing`) takes every event; its kinds are these op numbers.
        crate::ddi::pt_ring::record(op as u8, 0, detail as u16, 0, va, len, process);
        if !covers(va, len) {
            return;
        }
        let n = EVENTS.fetch_add(1, Ordering::Relaxed);
        let k = (n % 32) as u8;
        let mut a = *b"NvWa00";
        a[4] = b'0' + k / 10;
        a[5] = b'0' + k % 10;
        crate::diag::record_named_bytes(&a, (va >> 12) as u32);
        let mut c = *b"NvWc00";
        c[4] = b'0' + k / 10;
        c[5] = b'0' + k % 10;
        crate::diag::record_named_bytes(&c, (op << 24) | ((detail & 0xff) << 16) | (process as u32 & 0xffff));
        crate::diag::record_named_bytes(b"NvWN", n + 1);
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// `NvPerfMem` (REG_DWORD, default 0): the memory-performance paths of
/// `helios_kmd_logic::native_perf::PerfMem`, read once (PASSIVE) and mirrored to
/// `NvPerfMemOn`. 0 keeps every old path.
pub(crate) fn perf() -> np::PerfMem {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPerfMem"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"NvPerfMemOn", value);
    }
    np::PerfMem(value)
}

/// `NvSysChunk` (REG_DWORD, default 0 = 128): pages per OS descriptor of a
/// system-page leaf update (`native_perf::system_chunk_pages`).
fn system_chunk_pages() -> usize {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSysChunk"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    np::system_chunk_pages(value)
}

/// Host round trips of native-memory RM work (every `Io::send` and OS-descriptor
/// registration). Leaf updates read the difference across one update; it also
/// counts round trips other threads make meanwhile outside the I/O mutex (a
/// concurrent `create`), so per-update figures are an upper bound.
static ROUND_TRIPS: AtomicU32 = AtomicU32::new(0);

fn round_trips() -> u32 {
    ROUND_TRIPS.load(Ordering::Relaxed)
}

/// Native-memory host round trips so far (`ddi::hot_path`'s `HpMemRtPs`). Any IRQL.
pub(crate) fn round_trip_count() -> u32 {
    round_trips()
}

/// `NvPaging` (REG_DWORD, default 0): `helios_kmd_logic::hot_path::Paging`, read
/// once (PASSIVE) and mirrored to `PgKnob`.
fn paging() -> Paging {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPaging"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    Paging(value)
}

/// `NvDescCacheMax` (REG_DWORD, default 256): idle OS descriptors kept for reuse
/// under `NvPaging` 0x2; 0 keeps none.
fn desc_cache_max() -> usize {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(
            crate::diag::KnobName::new(b"NvDescCacheMax"),
            helios_kmd_logic::desc_cache::DEFAULT_MAX_IDLE as u32,
        );
        if value == u32::MAX {
            value = helios_kmd_logic::desc_cache::DEFAULT_MAX_IDLE as u32;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    value as usize
}

/// Whether idle OS descriptors are kept (`NvPaging` 0x2 and a bound above 0).
fn desc_cache_on() -> bool {
    paging().desc_cache() && desc_cache_max() != 0
}

// Paging fast-path counters (`Pg*`), published by the mirror pass (`publish_hot`):
/// `PgBatchN` batches sent, `PgBatchCalls` RM calls they carried, `PgBatchRef`
/// batches the host refused whole (the host then gets single calls), `PgBatchErr`
/// batches that failed in transport, `PgBatchUs` / `PgBatchMaxUs` their round trips.
static BATCHES: AtomicU32 = AtomicU32::new(0);
static BATCH_CALLS: AtomicU32 = AtomicU32::new(0);
static BATCH_REFUSED: AtomicU32 = AtomicU32::new(0);
static BATCH_FAILED: AtomicU32 = AtomicU32::new(0);
static BATCH_US: AtomicU32 = AtomicU32::new(0);
static BATCH_MAX_US: AtomicU32 = AtomicU32::new(0);
/// `PgDescNew` OS descriptors registered, `PgDescHit` reused for the same pages
/// (`PgDescIdleHit` of them taken from the idle cache), `PgDescFree` freed,
/// `PgDescEvict` idle ones freed by the bound or their allocation's move;
/// `PgDescLive` / `PgDescIdle` the table now.
static DESC_NEW: AtomicU32 = AtomicU32::new(0);
static DESC_HIT: AtomicU32 = AtomicU32::new(0);
static DESC_IDLE_HIT: AtomicU32 = AtomicU32::new(0);
static DESC_FREED: AtomicU32 = AtomicU32::new(0);
static DESC_EVICTED: AtomicU32 = AtomicU32::new(0);
/// `PgIdxN` per-process lookups (evictions, CPU patches, process teardown),
/// `PgIdxScan` allocations they looked at (all of them before the index).
static IDX_LOOKUPS: AtomicU32 = AtomicU32::new(0);
static IDX_SCANNED: AtomicU32 = AtomicU32::new(0);

/// `NvEvict` and `NvQuiesceRef`, written by the registry mirror's pass
/// (`ddi::hot_path::publish`) instead of on every eviction / refusal; the `Pg*`
/// paging counters above. PASSIVE.
pub(crate) fn publish_hot() {
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    let rec = crate::diag::record_named_bytes;
    rec(b"PgKnob", paging().0);
    rec(b"PgBatchN", r(&BATCHES));
    rec(b"PgBatchCalls", r(&BATCH_CALLS));
    rec(b"PgBatchRef", r(&BATCH_REFUSED));
    rec(b"PgBatchErr", r(&BATCH_FAILED));
    rec(b"PgBatchUs", r(&BATCH_US));
    rec(b"PgBatchMaxUs", r(&BATCH_MAX_US));
    rec(b"PgDescNew", r(&DESC_NEW));
    rec(b"PgDescHit", r(&DESC_HIT));
    rec(b"PgDescIdleHit", r(&DESC_IDLE_HIT));
    rec(b"PgDescFree", r(&DESC_FREED));
    rec(b"PgDescEvict", r(&DESC_EVICTED));
    let (live, idle, procs, broken) = {
        let state = st();
        let (n, idle) = state.descs.counts();
        (n - idle, idle, state.by_process.len(), state.index_broken)
    };
    rec(b"PgDescLive", live as u32);
    rec(b"PgDescIdle", idle as u32);
    rec(b"PgIdxN", r(&IDX_LOOKUPS));
    rec(b"PgIdxScan", r(&IDX_SCANNED));
    rec(b"PgIdxProcs", procs as u32);
    rec(b"PgIdxBroken", u32::from(broken));
    // The leaf-update costs, also here so they are fresh between their own
    // (every 64th update) writes.
    for cost in [&LEAF_COST, &SYSTEM_COST] {
        rec(cost.names[0], r(&cost.updates));
        rec(cost.names[1], r(&cost.trips));
        rec(cost.names[2], r(&cost.micros));
        rec(cost.names[3], r(&cost.max_micros));
    }
    publish_phases();
    publish_va();
    let copied = r(&COPY_KIB);
    if copied != 0 || r(&COPY_US) != 0 {
        rec(b"PgCopyKiB", copied);
        rec(b"PgCopyUs", r(&COPY_US));
        rec(b"PgCopyFree", r(&COPIES_UNLOCKED));
        rec(b"PgCopyLeak", r(&COPY_LEAKED));
    }
    if r(&VIEW_KIB) != 0 || r(&VIEW_US) != 0 {
        rec(b"PgViewKiB", r(&VIEW_KIB));
        rec(b"PgViewUs", r(&VIEW_US));
    }
    publish_gpu();
    let evicted = EVICTED.load(Ordering::Relaxed);
    if evicted != 0 {
        crate::diag::record_named_bytes(b"NvEvict", evicted);
    }
    let refused = QUIESCE_REFUSED.load(Ordering::Relaxed);
    if refused != 0 {
        crate::diag::record_named_bytes(b"NvQuiesceRef", refused);
    }
}

/// Leaf-update cost counters (`NvPm*`), published on the first update and every
/// 64th: `NvPmUpd`/`NvPmRt`/`NvPmUs` video-memory updates, their round trips and
/// microseconds, `NvPmMaxUs` the longest; `NvPmSys*` the same for system-page
/// updates; `NvPmSkip` system-page updates that mapped what was mapped already.
struct LeafCost {
    updates: AtomicU32,
    trips: AtomicU32,
    micros: AtomicU32,
    max_micros: AtomicU32,
    names: [&'static [u8]; 4],
}

static LEAF_COST: LeafCost = LeafCost::new([b"NvPmUpd", b"NvPmRt", b"NvPmUs", b"NvPmMaxUs"]);
static SYSTEM_COST: LeafCost =
    LeafCost::new([b"NvPmSysUpd", b"NvPmSysRt", b"NvPmSysUs", b"NvPmSysMaxUs"]);
static SKIPPED_UNCHANGED: AtomicU32 = AtomicU32::new(0);
/// `NvPmBig` / `NvPmHuge`: video-memory mappings made with 64-KiB / 2-MiB pages;
/// `NvPmLgFail`: large-page mappings RM refused (mapped again with 4 KiB);
/// `NvPmHugeMem` / `NvPmHugeFail`: allocations given 2-MiB-page memory / refused it;
/// `NvPmDefer`: RM operations that deferred their TLB invalidate; `NvPmTlbInv`:
/// explicit invalidates after a deferred batch failed (`NvPmTlbErr`: refused).
static MAPPED_BIG: AtomicU32 = AtomicU32::new(0);
static MAPPED_HUGE: AtomicU32 = AtomicU32::new(0);
static LARGE_REFUSED: AtomicU32 = AtomicU32::new(0);
static HUGE_MEMORY: AtomicU32 = AtomicU32::new(0);
static HUGE_REFUSED: AtomicU32 = AtomicU32::new(0);
static DEFERRED: AtomicU32 = AtomicU32::new(0);
static TLB_RESCUE: AtomicU32 = AtomicU32::new(0);
static TLB_RESCUE_FAILED: AtomicU32 = AtomicU32::new(0);
/// Large-page mappings refused in a row; from [`LARGE_STREAK_LIMIT`] on no more
/// are tried (until the driver restarts), so a host whose RM refuses them all
/// costs at most that many extra round trips.
static LARGE_STREAK: AtomicU32 = AtomicU32::new(0);
const LARGE_STREAK_LIMIT: u32 = 32;

impl LeafCost {
    const fn new(names: [&'static [u8]; 4]) -> Self {
        Self {
            updates: AtomicU32::new(0),
            trips: AtomicU32::new(0),
            micros: AtomicU32::new(0),
            max_micros: AtomicU32::new(0),
            names,
        }
    }

    /// One update that started at `start` (100 ns) with `trips_before` round trips.
    fn note(&self, start: u64, trips_before: u32) {
        let micros =
            np::ticks_to_us(crate::adapter::foreign_scanout::now_100ns().saturating_sub(start));
        let trips = round_trips().wrapping_sub(trips_before);
        let n = self.updates.fetch_add(1, Ordering::Relaxed) + 1;
        let total_trips = self.trips.fetch_add(trips, Ordering::Relaxed).wrapping_add(trips);
        let total_micros = self
            .micros
            .fetch_add(micros, Ordering::Relaxed)
            .wrapping_add(micros);
        let max = self.max_micros.fetch_max(micros, Ordering::Relaxed).max(micros);
        if np::publish_due(n) {
            crate::diag::record_named_bytes(self.names[0], n);
            crate::diag::record_named_bytes(self.names[1], total_trips);
            crate::diag::record_named_bytes(self.names[2], total_micros);
            crate::diag::record_named_bytes(self.names[3], max);
        }
    }
}

/// Count `counter` and publish it under `name` on the publish cadence.
fn bump(counter: &AtomicU32, name: &[u8]) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if np::publish_due(n) {
        crate::diag::record_named_bytes(name, n);
    }
}
/// Replacements whose RM idle-channels quiesce was refused (`update_leaf`).
static QUIESCE_REFUSED: AtomicU32 = AtomicU32::new(0);
/// `with_cpu_range` temporary-view copies (`PgCopy*`, published by the mirror
/// pass): `PgCopyKiB` KiB copied, `PgCopyUs` time in the copies,
/// `PgCopyFree` copies run without the mutex (`NvPaging` 0x8; BuildPagingBuffer's
/// by default, unless `NvPaging` 0x20), `PgCopyLeak` views whose host half could
/// not be released after one (the mutex timed out).
static COPY_KIB: AtomicU32 = AtomicU32::new(0);
static COPY_US: AtomicU32 = AtomicU32::new(0);
static COPIES_UNLOCKED: AtomicU32 = AtomicU32::new(0);
static COPY_LEAKED: AtomicU32 = AtomicU32::new(0);
/// `PgViewKiB` / `PgViewUs`: `with_cpu_range` copies through a view the allocation
/// already has (its aperture view, its pinned system pages), always under the mutex.
static VIEW_KIB: AtomicU32 = AtomicU32::new(0);
static VIEW_US: AtomicU32 = AtomicU32::new(0);

fn view_copy(length: u64, f: impl FnOnce() -> bool) -> bool {
    let start = now();
    let out = f();
    VIEW_KIB.fetch_add(u32::try_from(length >> 10).unwrap_or(u32::MAX), Ordering::Relaxed);
    VIEW_US.fetch_add(np::ticks_to_us(now().saturating_sub(start)), Ordering::Relaxed);
    out
}
static IO_MUTEX: SpinLock<Option<&'static PassiveMutex>> = SpinLock::new(None);
static STATE: SpinLock<State> = SpinLock::new(State::new());

// ── where leaf updates spend their time (`helios_kmd_logic::pg_phase`) ─────────────────────
//
// Published by the mirror pass (`publish_phases`), never written here:
// * `PgPh<phase>` / `PgPh<phase>Mx` / `PgPh<phase>W`: system-page leaf updates
//   (`update_leaf_system`), the phase's total and longest segment (us), and its
//   part of the longest update; `PgPhN` updates timed, `PgPhSlow` those of 10 ms
//   and more, `PgPhWorstUs` the longest, `PgPhWorstPh` its dominant phase
//   (`pg_phase::Phase` number), `PgPhUntimed` updates no slot was free for.
//   `PgPv*`: the same for video-memory leaf updates (`update_leaf`).
// * `PgHoN<k>` / `PgHoUs<k>` / `PgHoMx<k>`: holds of the native-memory mutex by
//   kind `k` (`pg_phase::Kind`, hex): count, total, longest (us). `PgNmWaitN` /
//   `PgNmWaitUs` / `PgNmWaitMx`: waits for it, `PgNmWaitBy` the kind that held it
//   when the longest wait began.
// * `PgStWaitUs` / `PgStWaitMx` / `PgStWaitLn`: waits for the STATE spinlock
//   (total, longest, the source line of `native_memory.rs` that waited);
//   `PgStHoldUs` / `PgStHoldMx` / `PgStHoldLn`: the same for holding it.
// `NvPgPhase` (REG_DWORD, default 1): 0 turns the clock reads off (the counters
// then stay 0).


extern "system" {
    /// `PsGetCurrentThread()` (exported; `KeGetCurrentThread` is an inline): any IRQL.
    fn PsGetCurrentThread() -> usize;
}

fn me() -> usize {
    // SAFETY: a scalar read of the current thread; any IRQL.
    unsafe { PsGetCurrentThread() }
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// `NvPgPhase`, read once (PASSIVE). Before it is read (a DISPATCH caller first),
/// timing is on.
fn phase_on() -> bool {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let value = VALUE.load(Ordering::Relaxed);
    if value != u32::MAX {
        return value != 0;
    }
    if crate::irql::PassiveLevel::try_assume().is_none() {
        return true;
    }
    let mut value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPgPhase"), 1);
    if value == u32::MAX {
        value = 1;
    }
    VALUE.store(value, Ordering::Relaxed);
    value != 0
}

static PH_SLOTS: pg::Slots<8> = pg::Slots::new();
static PH_SYSTEM: pg::PhaseStats = pg::PhaseStats::new();
static PH_LEAF: pg::PhaseStats = pg::PhaseStats::new();
static HOLDS: pg::KindStats = pg::KindStats::new();
static ST_WAIT: pg::LineMax = pg::LineMax::new();
static ST_HOLD: pg::LineMax = pg::LineMax::new();

/// Add `ticks` of `acc` to the calling thread's timed update, if it has one.
fn ph_add(acc: Acc, ticks: u64) {
    if PH_SLOTS.any() {
        PH_SLOTS.add(me(), acc, ticks);
    }
}

/// Run `f` as a timed scope of `acc` (the descriptor lookup, the page-run table).
fn ph_scope<R>(acc: Acc, f: impl FnOnce() -> R) -> R {
    if !PH_SLOTS.any() {
        return f();
    }
    let begun = PH_SLOTS.scope_begin(me());
    if begun.is_none() {
        return f();
    }
    let start = now();
    let out = f();
    PH_SLOTS.scope_end(begun, acc, now().saturating_sub(start));
    out
}

/// One timed leaf update: its slot and stopwatch. Dropped without [`Watch::done`]
/// (a skipped update) it only gives the slot back.
struct Watch {
    slot: usize,
    clock: pg::Stopwatch,
    stats: &'static pg::PhaseStats,
}

impl Watch {
    fn start(stats: &'static pg::PhaseStats, phase: Phase) -> Option<Self> {
        if !phase_on() {
            return None;
        }
        let Some(slot) = PH_SLOTS.claim(me()) else {
            stats.note_untimed();
            return None;
        };
        Some(Self {
            slot,
            clock: pg::Stopwatch::start(phase, now(), PH_SLOTS.read(slot)),
            stats,
        })
    }

    fn to(&mut self, phase: Phase) {
        self.clock.switch(phase, now(), PH_SLOTS.read(self.slot));
    }

    fn done(self) {
        let b = self.clock.finish(now(), PH_SLOTS.read(self.slot));
        self.stats.note(&b);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        PH_SLOTS.release(self.slot);
    }
}

/// Continue `w` (if timed) in `phase`.
fn ph(w: &mut Option<Watch>, phase: Phase) {
    if let Some(w) = w.as_mut() {
        w.to(phase);
    }
}

/// The STATE spinlock, timed: the wait goes to the caller's update (`Spin`) and,
/// with the hold, to `PgSt*` under the caller's line.
struct StGuard {
    guard: crate::sync::SpinLockGuard<'static, State>,
    since: u64,
    line: u32,
}

#[track_caller]
fn st() -> StGuard {
    let line = core::panic::Location::caller().line();
    if !phase_on() {
        return StGuard { guard: STATE.lock(), since: 0, line };
    }
    let start = now();
    let guard = STATE.lock();
    let since = now();
    let waited = since.saturating_sub(start);
    ST_WAIT.note(waited, line);
    ph_add(Acc::Spin, waited);
    StGuard { guard, since, line }
}

impl core::ops::Deref for StGuard {
    type Target = State;
    fn deref(&self) -> &State {
        &self.guard
    }
}

impl core::ops::DerefMut for StGuard {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.guard
    }
}

impl Drop for StGuard {
    fn drop(&mut self) {
        if self.since != 0 {
            ST_HOLD.note(now().saturating_sub(self.since), self.line);
        }
    }
}

/// The native-memory mutex's holder, for hold times per kind (`PgHo*`) and the
/// holder a waiter waited for (`PgNmWaitBy`). Written by the owning thread only
/// (the KMUTEX is recursive: the outermost acquisition counts).
static HOLD_THREAD: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static HOLD_DEPTH: AtomicU32 = AtomicU32::new(0);
static HOLD_KIND: AtomicU32 = AtomicU32::new(0);
static HOLD_SINCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A held native-memory mutex. The bookkeeping runs before the mutex is released
/// (fields drop after `Drop::drop`).
pub(crate) struct IoGuard {
    _mutex: PassiveMutexGuard<'static>,
}

impl IoGuard {
    fn acquired(mutex: PassiveMutexGuard<'static>, kind: Kind, start: u64, holder: u32) -> Self {
        let thread = me();
        if HOLD_THREAD.load(Ordering::Relaxed) == thread {
            HOLD_DEPTH.fetch_add(1, Ordering::Relaxed);
        } else {
            let since = if phase_on() { now() } else { 0 };
            if since != 0 {
                let waited = since.saturating_sub(start);
                HOLDS.waited(holder, waited);
                ph_add(Acc::Mutex, waited);
            }
            HOLD_DEPTH.store(1, Ordering::Relaxed);
            HOLD_KIND.store(kind as u32, Ordering::Relaxed);
            HOLD_SINCE.store(since, Ordering::Relaxed);
            HOLD_THREAD.store(thread, Ordering::Release);
        }
        Self { _mutex: mutex }
    }
}

impl Drop for IoGuard {
    fn drop(&mut self) {
        if HOLD_DEPTH.fetch_sub(1, Ordering::Relaxed) == 1 {
            let since = HOLD_SINCE.load(Ordering::Relaxed);
            if since != 0 {
                HOLDS.held(HOLD_KIND.load(Ordering::Relaxed), now().saturating_sub(since));
            }
            HOLD_KIND.store(Kind::None as u32, Ordering::Relaxed);
            HOLD_THREAD.store(0, Ordering::Release);
        }
    }
}

/// The `PgPh*`, `PgPv*`, `PgHo*`, `PgNmWait*` and `PgSt*` values (mirror pass, PASSIVE).
fn publish_phases() {
    let rec = |name: ([u8; 14], usize), value: u32| crate::diag::record_named_bytes(&name.0[..name.1], value);
    for (prefix, stats) in [(&b"PgPh"[..], &PH_SYSTEM), (&b"PgPv"[..], &PH_LEAF)] {
        if stats.updates() == 0 && stats.untimed() == 0 {
            continue;
        }
        rec(pg::name(prefix, b"N", b""), stats.updates());
        rec(pg::name(prefix, b"Slow", b""), stats.slow());
        rec(pg::name(prefix, b"Untimed", b""), stats.untimed());
        rec(pg::name(prefix, b"WorstUs", b""), stats.worst_us());
        rec(pg::name(prefix, b"WorstPh", b""), stats.worst_phase() as u32);
        for p in Phase::ALL {
            rec(pg::name(prefix, p.name(), b""), stats.total_us(p));
            rec(pg::name(prefix, p.name(), b"Mx"), stats.max_us(p));
            rec(pg::name(prefix, p.name(), b"W"), stats.at_worst_us(p));
        }
    }
    for k in 1..pg::KINDS {
        let (n, total, max) = HOLDS.held_of(k);
        if n == 0 {
            continue;
        }
        let d = [pg::hex(k)];
        rec(pg::name(b"PgHoN", &d, b""), n);
        rec(pg::name(b"PgHoUs", &d, b""), total);
        rec(pg::name(b"PgHoMx", &d, b""), max);
    }
    let (n, total, max, by) = HOLDS.waits();
    if n != 0 {
        rec(pg::name(b"PgNmWaitN", b"", b""), n);
        rec(pg::name(b"PgNmWaitUs", b"", b""), total);
        rec(pg::name(b"PgNmWaitMx", b"", b""), max);
        rec(pg::name(b"PgNmWaitBy", b"", b""), by);
    }
    for (prefix, lm) in [(&b"PgStWait"[..], &ST_WAIT), (&b"PgStHold"[..], &ST_HOLD)] {
        let (total, max, line) = lm.get();
        if max == 0 {
            continue;
        }
        rec(pg::name(prefix, b"Us", b""), total);
        rec(pg::name(prefix, b"Mx", b""), max);
        rec(pg::name(prefix, b"Ln", b""), line);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotReady,
    Busy,
    Invalid,
    Unsupported,
    NoMemory,
    Host,
    WrongAddress,
    /// No longer produced (leaf updates repair overlapping records); kept so the
    /// published error numbers (`NvLfErr`, `NvFailE*`) keep their values.
    #[allow(dead_code)]
    Overlap,
    Poisoned,
}

impl Error {
    pub const fn ntstatus(self) -> NTSTATUS {
        match self {
            Self::NotReady => wdk_sys::STATUS_DEVICE_NOT_READY,
            Self::Busy => wdk_sys::STATUS_DEVICE_BUSY,
            Self::Invalid | Self::Overlap => wdk_sys::STATUS_INVALID_PARAMETER,
            Self::Unsupported => wdk_sys::STATUS_NOT_SUPPORTED,
            Self::NoMemory => wdk_sys::STATUS_NO_MEMORY,
            Self::Host | Self::WrongAddress | Self::Poisoned => wdk_sys::STATUS_IO_DEVICE_ERROR,
        }
    }
}

/// Base RM objects supplied by the parent's normal root/device/subdevice
/// bring-up.  No handle is guessed or borrowed from a process client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeClient {
    pub epoch: u64,
    pub ctl: u32,
    pub gpu: u32,
    pub root: u32,
    pub minor: u32,
    /// RM's GPU id (`CARD_INFO`), which picks the GPU's DRM node.
    pub gpu_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Created {
    pub id: u32,
    pub size: u64,
    pub segment: u32,
    pub system_memory: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuView {
    pub address: u64,
    pub len: u64,
    /// Zero for the direct `pSystemMem` view.  Aperture unmaps use this value
    /// only as a generation check for RM-backed views.
    pub generation: u32,
}

#[derive(Clone, Copy)]
struct HostMapping {
    process: usize,
    piece: nm::MappingPiece,
    virt: u32,
    /// The OS-descriptor memory object of VidMm's system pages this mapping
    /// shows (`update_leaf_system`), 0 when it maps the allocation's RM memory.
    desc: u32,
}

struct Pin {
    mdl: usize,
    pages: usize,
    kernel_va: u64,
}

struct CpuChunk {
    physical: u64,
    map_ch: u32,
    cookie: u64,
    rm_mapped: bool,
    host_id: u32,
}

struct CpuMapping {
    view: CpuView,
    chunks: Vec<CpuChunk>,
    kernel_va: u64,
    aperture_offset: Option<u64>,
}

struct Allocation {
    id: u32,
    epoch: u64,
    memory: u32,
    size: u64,
    system_memory: u64,
    system_kernel_va: u64,
    pins: Vec<Pin>,
    mappings: Vec<HostMapping>,
    cpu: Option<CpuMapping>,
    resident: Option<(u64, bool)>,
    poisoned: bool,
    /// What CreateAllocation wrote into the NVDA record. dxgkrnl hands the UMD
    /// the record the KMD's OpenAllocation writes (create's copy is dropped),
    /// so the same values are written again at open (`writeback_open`).
    writeback: Option<Writeback>,
    /// The 2D picture the D3D record describes (`nm::d3d_surface`), if any.
    surface: Option<nm::D3dSurface>,
    /// The host GEM handle of this memory on the native scanout DRM file
    /// (`native_scanout`), 0 until the allocation is first shown.
    scanout_gem: u32,
    /// The RM memory has 2-MiB pages (NvPerfMem 0x10): mappings may use them.
    huge: bool,
}

/// The record fields CreateAllocation wrote, re-applied at open.
#[derive(Clone, Copy)]
enum Writeback {
    /// Records of no known UMD: RM handles and 64-KiB-unit sizes.
    Native(nm::PrivateWriteback),
    /// D3D11/D3D12 records: the 610.88 KMD's field set (`nm::d3d_writeback`).
    D3d([(usize, u32); nm::D3D_WRITEBACK_FIELDS]),
    /// Vulkan (`NVGL`) records: `nm::nvgl_writeback`.
    Nvgl([(usize, u32); nm::NVGL_WRITEBACK_FIELDS]),
    /// CUDA (`**VN`) records: `nm::cuda_writeback`.
    Cuda([(usize, u32); nm::CUDA_WRITEBACK_FIELDS]),
}

impl Writeback {
    fn apply(&self, bytes: &mut [u8]) -> Result<(), nm::PrivateError> {
        match self {
            Self::Native(w) => nm::writeback_allocation_private(bytes, w),
            Self::D3d(fields) => nm::apply_fields(bytes, fields),
            Self::Nvgl(fields) => nm::apply_fields(bytes, fields),
            Self::Cuda(fields) => nm::apply_fields(bytes, fields),
        }
    }
}

struct State {
    client: Option<NativeClient>,
    allocations: Vec<Allocation>,
    spaces: Vec<(usize, u32)>,
    /// Per-process space -> its windows (`helios_kmd_logic::va_window`).
    windows: vw::Windows,
    /// System pages of NON-native allocations (dxgkrnl's monitored fences and
    /// the like) mapped into a process's space (`update_foreign_leaf`).
    foreign: Vec<HostMapping>,
    /// Every OS-descriptor object of the paging path (system-page and foreign
    /// leaf updates) with its pages: for CPU access to what the GPU sees there
    /// (`patch_gpu_bytes`), the unchanged checks, and reuse for the same pages
    /// (`helios_kmd_logic::desc_cache`, `NvPaging` 0x2).
    descs: DescTable,
    /// Per process, the allocations that have (or had: pruned lazily) a mapping
    /// in its space, so an update looks at that process's allocations only.
    by_process: Vec<(usize, Vec<u32>)>,
    /// A `by_process` record could not be made (no memory): every lookup scans
    /// all allocations, as before the index, until the next transport reset.
    index_broken: bool,
    orphan_pins: Vec<Pin>,
    idle_spaces: Vec<usize>,
    next_handle: u32,
    /// The VA space of the paging copy channel and its two windows (`gpu_paging`).
    pg_space: Option<PgSpace>,
}

impl State {
    const fn new() -> Self {
        Self {
            client: None,
            allocations: Vec::new(),
            spaces: Vec::new(),
            windows: vw::Windows::new(),
            foreign: Vec::new(),
            descs: DescTable::new(),
            by_process: Vec::new(),
            index_broken: false,
            orphan_pins: Vec::new(),
            idle_spaces: Vec::new(),
            next_handle: FIRST_DYNAMIC_HANDLE,
            pg_space: None,
        }
    }

    fn client(&self) -> Result<NativeClient, Error> {
        self.client.ok_or(Error::NotReady)
    }

    /// `allocations` is kept sorted by id (`create` inserts in place).
    fn index_of(&self, id: u32) -> Option<usize> {
        self.allocations.binary_search_by_key(&id, |a| a.id).ok()
    }

    fn allocation(&self, id: u32) -> Result<&Allocation, Error> {
        let i = self.index_of(id).ok_or(Error::Invalid)?;
        Ok(&self.allocations[i])
    }

    fn allocation_mut(&mut self, id: u32) -> Result<&mut Allocation, Error> {
        let i = self.index_of(id).ok_or(Error::Invalid)?;
        Ok(&mut self.allocations[i])
    }

    /// Record that allocation `id` has a mapping in `process`'s space.
    fn note_mapped(&mut self, process: usize, id: u32) {
        if self.index_broken {
            return;
        }
        let at = match self.by_process.iter().position(|(p, _)| *p == process) {
            Some(at) => at,
            None => {
                if self.by_process.try_reserve(1).is_err() {
                    self.index_broken = true;
                    return;
                }
                self.by_process.push((process, Vec::new()));
                self.by_process.len() - 1
            }
        };
        let ids = &mut self.by_process[at].1;
        if let Err(i) = ids.binary_search(&id) {
            if ids.try_reserve(1).is_err() {
                self.index_broken = true;
                return;
            }
            ids.insert(i, id);
        }
    }

    /// The allocations that may have a mapping in `process`'s space: the index's
    /// (a superset), or all of them while the index is broken.
    fn candidates(&self, process: usize, out: &mut Vec<u32>) -> Result<(), Error> {
        out.clear();
        if self.index_broken {
            out.try_reserve(self.allocations.len()).map_err(|_| Error::NoMemory)?;
            out.extend(self.allocations.iter().map(|a| a.id));
        } else if let Some((_, ids)) = self.by_process.iter().find(|(p, _)| *p == process) {
            out.try_reserve(ids.len()).map_err(|_| Error::NoMemory)?;
            out.extend_from_slice(ids);
        }
        IDX_SCANNED.fetch_add(out.len() as u32, Ordering::Relaxed);
        IDX_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Drop the ids of `process`'s index entry whose allocation is gone or has no
    /// mapping there any more.
    fn prune_index(&mut self, process: usize) {
        let Some(at) = self.by_process.iter().position(|(p, _)| *p == process) else {
            return;
        };
        let mut ids = core::mem::take(&mut self.by_process[at].1);
        ids.retain(|id| {
            self.index_of(*id).is_some_and(|i| {
                self.allocations[i].mappings.iter().any(|m| m.process == process)
            })
        });
        if ids.is_empty() {
            self.by_process.swap_remove(at);
        } else {
            self.by_process[at].1 = ids;
        }
    }

    fn mint(&mut self) -> Result<u32, Error> {
        let out = self.next_handle;
        if out == 0 || out >= 0x4e5f_ffff {
            return Err(Error::NoMemory);
        }
        self.next_handle = out.checked_add(1).ok_or(Error::NoMemory)?;
        Ok(out)
    }
}

fn mutex(passive: PassiveLevel) -> Result<&'static PassiveMutex, Error> {
    if let Some(mutex) = *IO_MUTEX.lock() {
        return Ok(mutex);
    }
    let mutex = Box::leak(Box::new(
        PassiveMutex::try_new(passive).ok_or(Error::NoMemory)?,
    ));
    let mut slot = IO_MUTEX.lock();
    if let Some(existing) = *slot {
        return Ok(existing);
    }
    *slot = Some(mutex);
    Ok(mutex)
}

/// The longest any caller waits for the native-memory mutex. Holders make host
/// round trips, each bounded and refused at once while the host does not answer
/// (`ctrl::raw_roundtrip`), so a holder normally lets go well within this; VidMm's
/// paging thread must still never wait on one indefinitely.
const LOCK_MAX_MS: u64 = 20_000;
/// Waits for the mutex that gave up after [`LOCK_MAX_MS`] (`NvMemLockTo`).
static LOCK_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

/// The native-memory mutex for an operation of `kind` (`PgHo*`); the wait goes to
/// the caller's timed update (`Mutex`) and to `PgNmWait*`.
fn lock(passive: PassiveLevel, kind: Kind) -> Result<IoGuard, Error> {
    let start = crate::adapter::foreign_scanout::now_100ns();
    let holder = HOLD_KIND.load(Ordering::Relaxed);
    match mutex(passive)?.lock_within(passive, LOCK_MAX_MS) {
        Some(guard) => Ok(IoGuard::acquired(guard, kind, start, holder)),
        None => {
            // An escape abort returns early; only a wait that ran out is a timeout.
            let waited = crate::adapter::foreign_scanout::now_100ns().saturating_sub(start);
            if waited >= LOCK_MAX_MS * 10_000 {
                let n = LOCK_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
                crate::diag::record_named_bytes(b"NvMemLockTo", n);
            }
            Err(Error::Busy)
        }
    }
}

pub(crate) struct Io<'a> {
    pub(super) passive: PassiveLevel,
    pub(super) adapter: &'a AdapterContext,
    pub(super) client: NativeClient,
}

impl Io<'_> {
    pub(super) fn send(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, Error> {
        ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
        let mut seen = None;
        let start = if PH_SLOTS.any() { now() } else { 0 };
        let sent = nvrm::forward(
            self.passive,
            self.adapter,
            KMD,
            0,
            req,
            resp,
            TIMEOUT_MS,
            0,
            0,
            &mut seen,
        );
        if start != 0 {
            ph_add(Acc::Rt, now().saturating_sub(start));
        }
        let n = sent.map_err(|_| Error::Host)?;
        if seen.is_some_and(|epoch| epoch != self.client.epoch) {
            return Err(Error::NotReady);
        }
        Ok(n)
    }

    pub(super) fn exchange(
        &self,
        file: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Error> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        let mut request = Vec::new();
        request
            .try_reserve_exact(total)
            .map_err(|_| Error::NoMemory)?;
        request.resize(total, 0);
        let n = rc::build_ioctl(&mut request, file, cmd, data, nested).ok_or(Error::Invalid)?;
        self.send(request.get(..n).ok_or(Error::Invalid)?, resp)
    }

    fn rm_alloc(
        &self,
        parent: u32,
        object: u32,
        class: u32,
        params: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let block = rc::nvos64(self.client.root, parent, object, class, params.len() as u32);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            params,
            &mut response,
        )?;
        let reply = rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map_err(|_| Error::Host)?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS64_STATUS_AT).map_err(
            |error| {
                // NvRmAlCls: the refused class; NvRmAlSt: NV_STATUS, 0x8000_0000|errno
                // for a host refusal, 0xffff_ffff for a malformed reply.
                crate::diag::record_named_bytes(b"NvRmAlCls", class);
                crate::diag::record_named_bytes(b"NvRmAlSt", rm_error_code(error));
                Error::Host
            },
        )?;
        Ok(reply.nested.to_vec())
    }

    fn rm_free(&self, parent: u32, object: u32) -> Result<(), Error> {
        let block = rc::nvos00(self.client.root, parent, object);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            self.client.ctl,
            rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS00_STATUS_AT)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }

    pub(super) fn open_file(&self, device_type: u32) -> Result<u32, Error> {
        let mut request = [0u8; 32];
        let n = rc::build_open(&mut request, device_type).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        rc::parse_open_reply(response.get(..len).ok_or(Error::Host)?).ok_or(Error::Host)
    }

    pub(super) fn close_file(&self, handle: u32) -> Result<(), Error> {
        if handle == 0 {
            return Ok(());
        }
        let mut request = [0u8; 32];
        let n = rc::build_close(&mut request, handle).ok_or(Error::Invalid)?;
        let mut response = [0u8; 64];
        let len = self.send(request.get(..n).ok_or(Error::Invalid)?, &mut response)?;
        response
            .get(..len)
            .and_then(rc::reply_status)
            .filter(|status| *status == 0)
            .map(|_| ())
            .ok_or(Error::Host)
    }

    fn register_fd(&self, channel: u32) -> Result<(), Error> {
        let params = rc::register_fd_params(self.client.ctl);
        let mut response = [0u8; REPLY_MAX];
        let n = self.exchange(
            channel,
            rc::nv_cmd(rc::ESC_REGISTER_FD, 4),
            &params,
            &[],
            &mut response,
        )?;
        rc::parse_ioctl_reply(response.get(..n).ok_or(Error::Host)?)
            .map(|_| ())
            .map_err(|_| Error::Host)
    }
}

/// Install the base client and its GPU_DEVICE VA space. Per-process spaces are
/// created on demand; the parent's base RM bring-up owns ctl/gpu/root. All VA
/// spaces are freed by the transport-wide client teardown.
pub fn install_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    client: NativeClient,
) -> Result<(), Error> {
    let _guard = lock(passive, Kind::Client)?;
    {
        let state = st();
        if state.client.is_some() || !state.allocations.is_empty() {
            return Err(Error::Busy);
        }
    }
    let io = Io {
        passive,
        adapter,
        client,
    };
    io.rm_alloc(
        nd::H_DEVICE,
        H_VASPACE,
        cc::FERMI_VASPACE_A,
        &cc::vaspace_params(),
    )?;
    {
        let mut state = st();
        state.spaces.clear();
        state.windows.clear();
        state.foreign.clear();
        state.descs.clear();
        state.by_process.clear();
        state.index_broken = false;
        state.pg_space = None;
        state
            .spaces
            .try_reserve_exact(1)
            .map_err(|_| Error::NoMemory)?;
    }
    st().client = Some(client);
    st().spaces.push((0, H_VASPACE));
    Ok(())
}

/// The shared client and VA space used by channel creation. The process token is
/// only an association key; it never selects or borrows a user-mode RM client.
pub fn channel_client(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
) -> Result<(NativeClient, u32), Error> {
    // Under the I/O mutex (a recursive KMUTEX: `create` already holds it), like
    // every other `ensure_space` caller, so two threads cannot both find no space
    // for one process and each allocate one.
    let _guard = lock(passive, Kind::Client)?;
    // Copy the client out first: a guard in an `if let` scrutinee lives through
    // the body (edition 2021), and `ensure_space` takes STATE again on this
    // non-reentrant spinlock (CreateContext spun forever in ensure_space).
    let existing = st().client.as_ref().copied();
    if let Some(client) = existing {
        let io = Io {
            passive,
            adapter,
            client,
        };
        let space = ensure_space(&io, process)?;
        return Ok((client, space));
    }
    // First user: bring the client up, then give this process its OWN space like
    // every later one. Answering the shared device space here bound the first
    // process's channels (DWM's, at every boot) to a space its mappings never
    // went to: ring fetch FAULT_PDE at 0x4000000.
    let client = super::nvda::memory_client(passive, adapter, process)?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let space = ensure_space(&io, process)?;
    Ok((client, space))
}

/// Return this process's real VA space, creating it on first use. VidMm's
/// address ranges remain process-scoped; no second process can alias them by
/// sharing a root VA space.
fn ensure_space(io: &Io<'_>, process: usize) -> Result<u32, Error> {
    if let Some((_, space)) = st()
        .spaces
        .iter()
        .find(|(owner, _)| *owner == process)
    {
        return Ok(*space);
    }
    st()
        .spaces
        .try_reserve_exact(1)
        .map_err(|_| Error::NoMemory)?;
    let space = st().mint()?;
    // NvVaDevice=1 (experiment): the device's own VA space (index GPU_DEVICE,
    // shared by every process, no window) as NVK on RM uses, to tell whether
    // the new per-process spaces are what the graphics copy engine fails on.
    let device_space =
        crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvVaDevice"), 0) == 1;
    let started = now();
    let reply = io.rm_alloc(
        nd::H_DEVICE,
        space,
        cc::FERMI_VASPACE_A,
        &if device_space { cc::vaspace_params() } else { cc::vaspace_params_vidmm() },
    )?;
    va_note(vw::Op::SpaceMake, started);
    if device_space {
        st().spaces.push((process, space));
        return Ok(space);
    }
    // Diagnosis: the space RM made (vaSize at +8, vaBase at +40 of the
    // returned NV_VASPACE_ALLOCATION_PARAMETERS), in 64-KiB units.
    crate::diag::record_named_bytes(
        b"NvSpaceSz",
        (rc::get64(&reply, 8).unwrap_or(u64::MAX) >> 16) as u32,
    );
    crate::diag::record_named_bytes(
        b"NvSpaceBase",
        (rc::get64(&reply, 40).unwrap_or(u64::MAX) >> 16) as u32,
    );
    // Two virtual objects over VidMm's VAs (`vw::LAYOUT`), before RM maps anything
    // of its own into the space. Low window: one object over all of VidMm's low VAs,
    // so RM's first-fit internal mappings (GR context buffers) land above 4.5 GiB, in
    // the [4 GiB, 8 GiB) range the KMD reserves from VidMm, and every VidMm VA below
    // 4 GiB is mapped inside this window at its exact address. High window: the same
    // above the reserved range, so RM's own first-fit mappings stay inside it instead
    // of growing into VAs VidMm hands out (they reached 0x2_3200_0000, and every later
    // fixed mapping there was refused with NO_MEMORY). The high window is optional:
    // without it, mappings there fall back to fixed virtual objects.
    //
    // LAZY by default (`vw::window_flags`, `NvVaWin`): RM then only reserves the range;
    // without it, it reserved and pinned every page table of it up front (1.2 s for
    // the high window, the same again to free it, all under this mutex).
    // `NvPtSparse` (default 0, `pt_ring::window_flags`): unmapped VAs of the window read zero
    // instead of faulting (bit 0 low window, bit 1 high window); RM never makes those lazy.
    let sparse = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPtSparse"), 0);
    let knob = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvVaWin"), 0);
    if st().windows.reserve(vw::LAYOUT.len()).is_err() {
        let _ = io.rm_free(nd::H_DEVICE, space);
        return Err(Error::NoMemory);
    }
    let mut made = [0u32; vw::LAYOUT.len()];
    for (i, (base, end, high)) in vw::LAYOUT.into_iter().enumerate() {
        // The guard of `st()` must end before the match, which locks STATE again.
        let minted = st().mint();
        let window = match minted {
            Ok(window) => window,
            Err(error) if !high => {
                let _ = io.rm_free(nd::H_DEVICE, space);
                return Err(error);
            }
            Err(_) => break,
        };
        let mut params = cc::virtual_params(io.client.root, space, Some(base), end - base);
        let extra = vw::window_flags(knob, ptr::window_flags(sparse, high));
        add_window_flags(&mut params, extra);
        let started = now();
        match io.rm_alloc(nd::H_DEVICE, window, cc::NV50_MEMORY_VIRTUAL, &params) {
            Ok(_) => {
                va_note(vw::Op::WinMake, started);
                made[i] = window;
            }
            Err(error) if !high => {
                // Frees the space's windows with it.
                let _ = io.rm_free(nd::H_DEVICE, space);
                return Err(error);
            }
            Err(error) => crate::diag::record_named_bytes(b"NvHighWin", error as u32 + 1),
        }
    }
    {
        let mut state = st();
        state.spaces.push((process, space));
        for (window, (base, end, _)) in made.into_iter().zip(vw::LAYOUT) {
            if window != 0 {
                // Room was reserved; the layout's windows neither overlap nor repeat.
                let _ = state.windows.add(vw::Window { space, handle: window, base, end });
            }
        }
    }
    crate::diag::record_named_bytes(b"NvSpaceProc", process as u32);
    crate::diag::record_named_bytes(b"NvSpaceH", space);
    // Diagnosis: where RM puts a non-fixed 2-MiB virtual object now (in 64-KiB
    // units; 0xffffffff = refused), i.e. where its own mappings would go.
    let minted = st().mint();
    if let Ok(probe) = minted {
        let params = cc::virtual_params(io.client.root, space, None, 0x20_0000);
        let at = match io.rm_alloc(nd::H_DEVICE, probe, cc::NV50_MEMORY_VIRTUAL, &params) {
            Ok(reply) => {
                let _ = io.rm_free(nd::H_DEVICE, probe);
                (rc::get64(&reply, 80).unwrap_or(u64::MAX) >> 16) as u32
            }
            Err(_) => u32::MAX,
        };
        crate::diag::record_named_bytes(b"NvSpaceFree", at);
    }
    Ok(space)
}

/// OR `extra` into the `NVOS32` flags (+8) of a window's `NV_MEMORY_ALLOCATION_PARAMS`,
/// count sparse windows (`NvPtSparseN`) and note the flags (`NvVaWinFlg`).
fn add_window_flags(params: &mut [u8], extra: u32) {
    VA_FLAGS.store(extra, Ordering::Relaxed);
    if extra == 0 {
        return;
    }
    let flags = rc::get32(params, 8).unwrap_or(0) | extra;
    params[8..12].copy_from_slice(&flags.to_le_bytes());
    if extra & ptr::NVOS32_ALLOC_FLAGS_SPARSE != 0 {
        static N: AtomicU32 = AtomicU32::new(0);
        crate::diag::record_named_bytes(b"NvPtSparseN", N.fetch_add(1, Ordering::Relaxed) + 1);
    }
}

/// Window and space RM calls, per `vw::Op` (`NvVaWin*`, `NvVaSp*`), published by
/// [`publish_hot`] (a registry write each is ~100 us; these are taken under the mutex).
static VA_STATS: SpinLock<[vw::Stat; vw::OPS]> = SpinLock::new([vw::Stat::ZERO; vw::OPS]);
/// The extra flags of the last window made (`NvVaWinFlg`), `u32::MAX` before the first.
static VA_FLAGS: AtomicU32 = AtomicU32::new(u32::MAX);

/// Count one `op` that began at `started` (`now()`).
fn va_note(op: vw::Op, started: u64) {
    let us = vw::us(now().saturating_sub(started));
    VA_STATS.lock()[op as usize].note(us);
}

fn publish_va() {
    let stats = *VA_STATS.lock();
    for (stat, [n, total, max]) in stats.iter().zip(vw::COUNTERS) {
        crate::diag::record_named_bytes(n.as_bytes(), stat.n);
        crate::diag::record_named_bytes(total.as_bytes(), stat.total_us);
        crate::diag::record_named_bytes(max.as_bytes(), stat.max_us);
    }
    crate::diag::record_named_bytes(vw::FLAGS_COUNTER.as_bytes(), VA_FLAGS.load(Ordering::Relaxed));
}

/// The window of `space` that holds [va, end), if any (`vw::LAYOUT`).
fn window_for(space: u32, va: u64, end: u64) -> Option<u32> {
    st().windows.find(space, va, end)
}

fn windows_of(space: u32) -> Vec<u32> {
    let mut out = Vec::new();
    // No memory: no window is freed on its own; the space's free takes them along.
    let _ = st().windows.of(space, &mut out);
    out
}

fn is_window(virt: u32) -> bool {
    st().windows.is_window(virt)
}


/// Validate a UMD GPFIFO allocation before it becomes a hardware queue. The
/// entire ring must be covered by one live native mapping at the exact VA.
pub fn ring_backing(
    id: u32,
    process: usize,
    gpfifo_va: u64,
    ring_bytes: u64,
) -> Result<super::native_channel::NativeRingBacking, Error> {
    let state = st();
    let allocation = state.allocation(id)?;
    let end = gpfifo_va.checked_add(ring_bytes).ok_or(Error::Invalid)?;
    if ring_bytes == 0 || ring_bytes > allocation.size {
        return Err(Error::Invalid);
    }
    // Only the queue's own process: every process has its own VA space and the
    // same VA in another one (a shared ring, a stale piece) is another mapping.
    let Some(mapping) = allocation.mappings.iter().find(|mapping| {
        mapping.process == process
            && mapping.piece.va <= gpfifo_va
            && mapping
                .piece
                .end()
                .is_some_and(|mapping_end| mapping_end >= end)
    }) else {
        // MakeResident is asynchronous: the UMD creates its queue before VidMm
        // has paged the ring in and updated its PTEs. RM's channel needs only
        // the GPFIFO VA, which VidMm maps before any submission runs; the
        // VA-space match is checked once mapped (va_space 0 = not yet).
        return Ok(super::native_channel::NativeRingBacking {
            memory: allocation.memory,
            mapped_bytes: ring_bytes,
            va_space: 0,
            id,
            offset: 0,
        });
    };
    let va_space = state
        .spaces
        .iter()
        .find(|(process, _)| *process == mapping.process)
        .map(|(_, space)| *space)
        .ok_or(Error::NotReady)?;
    Ok(super::native_channel::NativeRingBacking {
        memory: allocation.memory,
        mapped_bytes: ring_bytes,
        va_space,
        id,
        offset: mapping.piece.memory_offset + (gpfifo_va - mapping.piece.va),
    })
}

fn local_params(root: u32, size: u64) -> [u8; rc::MEM_ALLOC_BYTES] {
    rc::mem_alloc_params(
        root,
        &rc::SurfaceLayout {
            width: 0,
            height: 0,
            pitch: 0,
            size,
        },
    )
}

extern "C" {
    fn helios_lock_user_pages_seh(virtual_address: *mut c_void, length: u32) -> PMDL;
    fn helios_unlock_system_buffer(mdl: PMDL);
}

fn unlock_pins(pins: &mut Vec<Pin>) {
    for pin in pins.drain(..) {
        if pin.kernel_va != 0 {
            // SAFETY: this exact kernel mapping belongs to the pin and is removed
            // before its MDL is unlocked, at PASSIVE.
            unsafe { MmUnmapLockedPages(pin.kernel_va as *mut c_void, pin.mdl as PMDL) };
        }
        if pin.mdl != 0 {
            // SAFETY: each MDL was returned by helios_lock_user_pages_seh and is
            // owned by this pin.  PASSIVE, outside STATE's spinlock.
            unsafe { helios_unlock_system_buffer(pin.mdl as PMDL) };
        }
    }
}

fn preserve_ambiguous_pins(pins: &mut Vec<Pin>) {
    st().orphan_pins.append(pins);
}

/// A create that will not finish gives back its RM memory; the pins unlock only
/// when RM confirms the free (else the host may still name the pages).
fn abandon_memory(io: &Io<'_>, memory: u32, pins: &mut Vec<Pin>) {
    if io.rm_free(nd::H_DEVICE, memory).is_ok() {
        unlock_pins(pins);
    } else {
        preserve_ambiguous_pins(pins);
    }
}

fn pin_system_memory(system_memory: u64, size: u64) -> Result<(Vec<Pin>, Vec<u64>), Error> {
    let page_count = usize::try_from(size / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
    let mut pins = Vec::new();
    let mut pfns = Vec::new();
    pfns.try_reserve_exact(page_count)
        .map_err(|_| Error::NoMemory)?;
    let mut offset = 0u64;
    while offset < size {
        let len = (size - offset).min(PIN_CHUNK_BYTES);
        let address = system_memory.checked_add(offset).ok_or(Error::Invalid)?;
        let mdl = unsafe { helios_lock_user_pages_seh(address as *mut c_void, len as u32) };
        if mdl.is_null() {
            unlock_pins(&mut pins);
            return Err(Error::Invalid);
        }
        let pages = usize::try_from(len / nm::PAGE_BYTES).map_err(|_| Error::NoMemory)?;
        // SAFETY: a locked MDL's PFN array follows MDL and has `pages` entries.
        let chunk = unsafe {
            core::slice::from_raw_parts(
                (mdl as *const u8).add(core::mem::size_of::<MDL>()) as *const u64,
                pages,
            )
        };
        pfns.extend_from_slice(chunk);
        let kernel_va = if size <= PIN_CHUNK_BYTES {
            unsafe {
                MmMapLockedPagesSpecifyCache(
                    mdl,
                    0, // KernelMode: this view is valid in every process context
                    _MEMORY_CACHING_TYPE::MmCached,
                    core::ptr::null_mut(),
                    0,
                    16, // NormalPagePriority
                ) as u64
            }
        } else {
            0
        };
        pins.push(Pin {
            mdl: mdl as usize,
            pages,
            kernel_va,
        });
        offset += len;
    }
    if pfns.len() != page_count || pins.iter().map(|p| p.pages).sum::<usize>() != page_count {
        unlock_pins(&mut pins);
        return Err(Error::Invalid);
    }
    Ok((pins, pfns))
}

fn alloc_system_descriptor(io: &Io<'_>, memory: u32, pfns: &[u64], size: u64) -> Result<(), Error> {
    let (kind, deep, big) = ph_scope(Acc::Table, || nvrm::page_run_table(io.passive, pfns))
        .map_err(|_| Error::NoMemory)?;
    let block = cr::nvos02_osdesc(
        io.client.root,
        nd::H_DEVICE,
        memory,
        pfns.first()
            .copied()
            .unwrap_or(0)
            .wrapping_mul(nm::PAGE_BYTES),
        size,
    );
    let mut request = [0u8; rc::MSG_HDR + rc::IOCTL_REQ + cr::NVOS02_FD_BYTES];
    let n = rc::build_ioctl(
        &mut request,
        io.client.gpu,
        rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32),
        &block,
        &[],
    )
    .ok_or(Error::Invalid)?;
    let mut response = [0u8; REPLY_MAX];
    ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
    let start = if PH_SLOTS.any() { now() } else { 0 };
    let sent = nvrm::forward_kmd_registration(
        io.passive,
        io.adapter,
        request.get(..n).ok_or(Error::Invalid)?,
        kind,
        &deep,
        &mut response,
        TIMEOUT_MS,
    );
    if start != 0 {
        ph_add(Acc::Rt, now().saturating_sub(start));
    }
    let reply = match sent {
        Ok(len) => {
            drop(big);
            response.get(..len).ok_or(Error::Host)?
        }
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            // The host may have consumed the table and retained the registration;
            // the MDLs must remain locked until the client/transport is torn down.
            core::mem::forget(big);
            return Err(Error::Host);
        }
        Err(refusal) => {
            // NvSysHost: 0x2000_0000 | refusal kind for a transport/forward refusal.
            crate::diag::record_named_bytes(b"NvSysHost", 0x2000_0000 | refusal_code(&refusal));
            crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
            return Err(Error::Host);
        }
    };
    rc::rm_reply(reply, cr::NVOS02_STATUS_AT).map(|_| ()).map_err(|error| {
        // NvSysHost: RM's NV_STATUS, 0x1000_0000 | errno for a host refusal,
        // 0x3000_0000 for a malformed reply; NvSysPages: the descriptor's pages.
        let code = match error {
            rc::RmError::Status(status) => status,
            rc::RmError::Host(errno) => 0x1000_0000 | errno,
            rc::RmError::Malformed => 0x3000_0000,
        };
        crate::diag::record_named_bytes(b"NvSysHost", code);
        crate::diag::record_named_bytes(b"NvSysPages", pfns.len() as u32);
        // The refused page frames' range, and how many lie outside guest RAM
        // as Windows reports it below 4 GiB / above (pfn >= 0x80000 and < 0x100000).
        let lo = pfns.iter().copied().min().unwrap_or(0);
        let hi = pfns.iter().copied().max().unwrap_or(0);
        let hole = pfns.iter().filter(|&&p| (0x80000..0x100000).contains(&p)).count();
        crate::diag::record_named_bytes(b"NvSysPfnLo", lo as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHi", hi as u32);
        crate::diag::record_named_bytes(b"NvSysPfnHole", hole as u32);
        crate::diag::record_named_bytes(b"NvSysPfn0", pfns.first().copied().unwrap_or(0) as u32);
        Error::Host
    })
}

fn refusal_code(refusal: &Refusal) -> u32 {
    match refusal {
        Refusal::MsgType => 1,
        Refusal::NotOwned => 2,
        Refusal::NoResources => 3,
        Refusal::Forbidden => 4,
        Refusal::BadRange => 5,
        Refusal::Transport(_) => 6,
        #[allow(unreachable_patterns)]
        _ => 7,
    }
}

/// Create the real local-linear or pSystemMem OS-descriptor backing.  All
/// private-data validation has already completed in `kmd_logic`; no host
/// allocation is attempted for an unsupported shape.
static STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
fn step(n: u32) {
    STEP.store(n, Ordering::Relaxed);
}
/// The last checkpoint `create` reached (diagnostic for refused allocations).
pub fn last_step() -> u32 {
    STEP.load(Ordering::Relaxed)
}

pub fn create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    private: &nm::AllocationPrivate,
    private_bytes: &mut [u8],
) -> Result<Created, Error> {
    step(1);
    // The client, the base VA space and the new handle are taken under the mutex;
    // the RM object itself is made after it is released (no holder of the mutex
    // waits on this allocation's round trips, so VidMm's paging thread is never
    // queued behind a host that does not answer), and the record is published
    // under it again, only if the client generation is still the one used.
    let (client, epoch, space, memory) = {
        let _guard = lock(passive, Kind::Create)?;
        step(2);
        // As in `channel_client`: release STATE before `channel_client` re-takes it.
        let existing = st().client.as_ref().copied();
        let client = match existing {
            Some(client) => client,
            None => channel_client(passive, adapter, 0)?.0,
        };
        step(3);
        let epoch = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
        if epoch == 0 || epoch != client.epoch {
            return Err(Error::NotReady);
        }
        let io = Io {
            passive,
            adapter,
            client,
        };
        let space = ensure_space(&io, 0)?;
        step(4);
        let memory = st().mint()?;
        (client, epoch, space, memory)
    };
    let io = Io {
        passive,
        adapter,
        client,
    };
    let pins = Vec::new();
    let mut huge = false;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        return Err(Error::NoMemory);
    }

    let (actual_size, mut pins) = if private.system_backed() {
        let (mut pins, pfns) = pin_system_memory(private.system_memory, private.size)?;
        if let Err(error) = alloc_system_descriptor(&io, memory, &pfns, private.size) {
            // A timeout may have left the descriptor live.  Its memory handle is
            // deliberately leaked with the pins until the transport sweep.
            if matches!(error, Error::Host) {
                preserve_ambiguous_pins(&mut pins);
            } else if io.rm_free(nd::H_DEVICE, memory).is_ok() {
                unlock_pins(&mut pins);
            } else {
                preserve_ambiguous_pins(&mut pins);
            }
            return Err(error);
        }
        (private.size, pins)
    } else {
        step(5);
        let params = local_params(client.root, private.size);
        // NvPerfMem 0x10: GPU-only memory of 2 MiB and more with 2-MiB pages, so
        // its 2-MiB-aligned mappings can use huge PTEs; a refusal falls back to
        // the big-page memory every other allocation gets.
        let reply = if np::wants_huge_memory(
            perf(),
            np::is_gpu_only_record(private_bytes),
            false,
            private.size,
        ) {
            let mut huge_params = params;
            let (attr, attr2) = np::huge_attrs(rc::SURFACE_ATTR, rc::SURFACE_ATTR2);
            huge_params[24..28].copy_from_slice(&attr.to_le_bytes());
            huge_params[28..32].copy_from_slice(&attr2.to_le_bytes());
            huge_params[72..80].copy_from_slice(&np::HUGE_PAGE.to_le_bytes());
            match io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &huge_params) {
                Ok(reply) => {
                    huge = true;
                    bump(&HUGE_MEMORY, b"NvPmHugeMem");
                    Ok(reply)
                }
                Err(_) => {
                    bump(&HUGE_REFUSED, b"NvPmHugeFail");
                    io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params)
                }
            }
        } else {
            io.rm_alloc(nd::H_DEVICE, memory, rc::NV01_MEMORY_LOCAL_USER, &params)
        };
        match reply {
            Ok(reply) => {
                let actual = rc::get64(&reply, 64).unwrap_or(private.size);
                if actual < private.size
                    || actual % nm::PAGE_BYTES != 0
                    || actual >= nm::GPU_ADDRESS_LIMIT
                {
                    let _ = io.rm_free(nd::H_DEVICE, memory);
                    return Err(Error::Host);
                }
                (actual, pins)
            }
            Err(error) => return Err(error),
        }
    };

    step(6);
    let writeback = match nm::PrivateWriteback::new(
        memory,
        client.root,
        nd::H_DEVICE,
        actual_size,
        private.kind,
        rc::SURFACE_ATTR,
        rc::SURFACE_ATTR2,
        space,
    ) {
        Some(writeback) => writeback,
        None => {
            abandon_memory(&io, memory, &mut pins);
            return Err(Error::Invalid);
        }
    };
    step(7);
    // D3D records keep their inputs (0x8c, 0x10c..0x120) and get the D3D field
    // set; the size in 64-KiB units is what the UMD reserves VA for.
    // The picture fields are inputs; read them before the writeback below sets
    // +0xd0 (after which the record no longer reads as a D3D one).
    let surface = nm::d3d_surface(private_bytes);
    let writeback = if nm::is_d3d_record(private_bytes) {
        let units = u32::try_from(private.size / nm::ALLOCATION_UNIT).unwrap_or(u32::MAX);
        match nm::d3d_writeback(private_bytes, units) {
            Some(fields) => Writeback::D3d(fields),
            None => Writeback::Native(writeback),
        }
    } else if nm::is_nvgl_record(private_bytes) {
        // Vulkan records: the 610.88 KMD's NVGL field set, not CUDA's handles.
        match nm::nvgl_writeback(private_bytes) {
            Some(fields) => Writeback::Nvgl(fields),
            None => Writeback::Native(writeback),
        }
    } else if nm::is_cuda_record(private_bytes) {
        // CUDA records: the 610.88 KMD's field set (no RM handles in the record).
        match nm::cuda_writeback(private_bytes) {
            Some(fields) => Writeback::Cuda(fields),
            None => Writeback::Native(writeback),
        }
    } else {
        Writeback::Native(writeback)
    };
    if writeback.apply(private_bytes).is_err() {
        abandon_memory(&io, memory, &mut pins);
        return Err(Error::Invalid);
    }
    let segment = if private.system_backed() {
        nm::SYSTEM_MEMORY_SEGMENT_ID
    } else {
        crate::ddi::gpummu::MEMORY_SEGMENT_ID
    };
    let allocation = Allocation {
        id,
        epoch,
        memory,
        size: actual_size,
        system_memory: private.system_memory,
        system_kernel_va: if pins.len() == 1 {
            pins[0].kernel_va
        } else {
            0
        },
        pins,
        mappings: Vec::new(),
        cpu: None,
        resident: None,
        poisoned: false,
        writeback: Some(writeback),
        surface,
        scanout_gem: 0,
        huge,
    };
    // Publish under the mutex. A transport sweep that ran meanwhile (`forget`)
    // closed this client with the object in it: the record is dropped and its pins
    // kept, as for any object whose host state is unknown.
    let mut unpublished = Some(allocation);
    let refused = match lock(passive, Kind::Create) {
        Ok(_guard) => {
            let mut state = st();
            if state.client.as_ref().map(|c| c.epoch) != Some(epoch) {
                Some((Error::NotReady, false))
            } else if state.allocations.try_reserve_exact(1).is_err() {
                Some((Error::NoMemory, true))
            } else {
                if let Some(allocation) = unpublished.take() {
                    // Sorted by id (`State::index_of`): ids are minted before the
                    // RM object is made, so a slower create publishes a lower id.
                    let at = state.allocations.partition_point(|a| a.id < allocation.id);
                    state.allocations.insert(at, allocation);
                }
                None
            }
        }
        Err(error) => Some((error, true)),
    };
    if let Some((error, free)) = refused {
        // Outside the mutex: the object was never published, nothing else names it.
        let mut pins = unpublished.map(|a| a.pins).unwrap_or_default();
        if free && io.rm_free(nd::H_DEVICE, memory).is_ok() {
            unlock_pins(&mut pins);
        } else {
            preserve_ambiguous_pins(&mut pins);
        }
        return Err(error);
    }
    super::nvda::note_memory(memory);
    Ok(Created {
        id,
        size: actual_size,
        segment,
        system_memory: private.system_memory,
    })
}

fn rm_error_code(error: rc::RmError) -> u32 {
    match error {
        rc::RmError::Status(status) => status,
        rc::RmError::Host(errno) => 0x8000_0000 | errno,
        rc::RmError::Malformed => 0xffff_ffff,
    }
}

/// Whether the RM memory `memory` of a native allocation has 2-MiB pages. Takes
/// STATE: never call it with a STATE guard alive.
fn memory_is_huge(memory: u32) -> bool {
    st()
        .allocations
        .iter()
        .any(|a| a.memory == memory && a.huge)
}

/// Map `piece` of RM memory `memory` at its exact VA in `space` with the RM
/// page size the piece allows (NvPerfMem 0x08 big, 0x10 huge pages for video memory; a large-page mapping RM refuses is made
/// again with 4-KiB pages) and, with `defer`, the TLB invalidate left to a later
/// operation of the same leaf update (NvPerfMem 0x20).
#[allow(clippy::too_many_arguments)]
fn map_fixed_opts(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
    defer: bool,
) -> Result<u32, Error> {
    let defer_flag = if defer { np::MAP_FLAGS_DEFER_TLB } else { 0 };
    if !system_backed {
        let perf = perf();
        let huge = perf.huge_pages() && memory_is_huge(memory);
        let mut page = np::map_page_size(piece, perf.big_pages(), huge);
        if LARGE_STREAK.load(Ordering::Relaxed) >= LARGE_STREAK_LIMIT {
            // RM refused every large-page mapping lately: each try costs a round trip.
            page = np::MAP_PAGE_4K;
        }
        // Huge, then big pages; RM refusing one (or the fixed virtual object)
        // steps down, and 4 KiB below is what every mapping was before.
        while page != np::MAP_PAGE_4K {
            let flags = np::with_page_size(LOCAL_4K_MAP_FLAGS, page) | defer_flag;
            match map_fixed_flags(io, memory, allocation_size, false, space, virt, piece, flags, false) {
                Ok(used) => {
                    LARGE_STREAK.store(0, Ordering::Relaxed);
                    if page == np::MAP_PAGE_HUGE {
                        bump(&MAPPED_HUGE, b"NvPmHuge");
                    } else {
                        bump(&MAPPED_BIG, b"NvPmBig");
                    }
                    if defer {
                        bump(&DEFERRED, b"NvPmDefer");
                    }
                    return Ok(used);
                }
                Err(Error::Host) => {
                    bump(&LARGE_REFUSED, b"NvPmLgFail");
                    LARGE_STREAK.fetch_add(1, Ordering::Relaxed);
                    page = if page == np::MAP_PAGE_HUGE && perf.big_pages() {
                        np::MAP_PAGE_BIG
                    } else {
                        np::MAP_PAGE_4K
                    };
                }
                Err(error) => return Err(error),
            }
        }
    }
    let base = if system_backed {
        cc::MAP_FLAGS_SYSMEM
    } else {
        LOCAL_4K_MAP_FLAGS
    };
    let flags = base | defer_flag;
    let used = map_fixed_flags(io, memory, allocation_size, system_backed, space, virt, piece, flags, false)?;
    if defer {
        bump(&DEFERRED, b"NvPmDefer");
    }
    Ok(used)
}

#[allow(clippy::too_many_arguments)]
fn map_fixed_flags(
    io: &Io<'_>,
    memory: u32,
    allocation_size: u64,
    system_backed: bool,
    space: u32,
    virt: u32,
    piece: nm::MappingPiece,
    flags: u32,
    force_4k_virtual: bool,
) -> Result<u32, Error> {
    nm::validate_piece(piece, allocation_size).map_err(|_| Error::Invalid)?;
    let end = piece.end().ok_or(Error::Invalid)?;
    if let Some(window) = window_for(space, piece.va, end) {
        return map_in_window(io, memory, window, flags, piece);
    }
    // System pages map with 4-KiB PTEs at VidMm's 4-KiB-aligned VAs: their virtual
    // object must be 4-KiB-paged, or RM aligns it down to the big page. A retry of
    // a refused video-memory map asks for the same (`map_retry`).
    let params = if system_backed || force_4k_virtual {
        cc::virtual_params_4k(io.client.root, space, Some(piece.va), piece.len)
    } else {
        cc::virtual_params(io.client.root, space, Some(piece.va), piece.len)
    };
    io.rm_alloc(nd::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &params)
        .inspect_err(|_| {
            // The refused fixed VA (4-KiB page number) and length in pages.
            crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvMapLenPg", (piece.len >> 12) as u32);
        })?;
    let block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        virt,
        memory,
        piece.memory_offset,
        piece.len,
        flags,
    );
    let mut response = [0u8; REPLY_MAX];
    let mapped = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    );
    let got = match mapped {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
                crate::diag::record_named_bytes(b"NvRmMapSt", rm_error_code(error));
                Error::Host
            })?;
            cc::map_dma_va(reply.data).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    let got = match got {
        Ok(got) => got,
        Err(error) => {
            let _ = io.rm_free(nd::H_DEVICE, virt);
            return Err(error);
        }
    };
    if got != piece.va {
        // RM must not silently relocate a WDDM mapping.  Tear down the address
        // RM returned, not the address we wished it had returned.
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, virt, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        let _ = io.rm_free(nd::H_DEVICE, virt);
        note_wrong_address(piece.va, got);
        return Err(Error::WrongAddress);
    }
    Ok(virt)
}

/// `NvWrongVa` / `NvWrongGot`: the page numbers of the VA a fixed mapping asked for
/// and the one RM answered when they differ; `NvWrongN` how often.
fn note_wrong_address(want: u64, got: u64) {
    let n = WRONG_ADDRESS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvWrongN", n);
    crate::diag::record_named_bytes(b"NvWrongVa", (want >> 12) as u32);
    crate::diag::record_named_bytes(b"NvWrongGot", (got >> 12) as u32);
}

static WRONG_ADDRESS: AtomicU32 = AtomicU32::new(0);
static FAILED_MAPS: AtomicU32 = AtomicU32::new(0);

/// `NvFail<k>` (k = 0..7, a ring): the 4-KiB page number of the first VA of a leaf
/// update that left its range unmapped, `NvFailE<k>` = why (low byte the
/// `Error`, next byte the site: 1 native vidmem, 2 system pages, 3/4/5 foreign
/// pages' eviction / OS descriptor / mapping).
fn note_failed_map(va: u64, site: u32, error: Error) {
    let n = FAILED_MAPS.fetch_add(1, Ordering::Relaxed);
    let k = (n % 8) as u8;
    let mut name = *b"NvFail0";
    name[6] = b'0' + k;
    crate::diag::record_named_bytes(&name, (va >> 12) as u32);
    let mut why = *b"NvFailE0";
    why[7] = b'0' + k;
    crate::diag::record_named_bytes(&why, (site << 8) | error as u32);
    crate::diag::record_named_bytes(b"NvFailN", n + 1);
}

/// `NVOS46_FLAGS_DMA_OFFSET_FIXED_TRUE`: for a virtual object, `dmaOffset` is
/// the absolute VA of the mapping inside it.
const MAP_FLAGS_DMA_OFFSET_FIXED: u32 = 1 << 15;

/// Map `piece` at its exact VA inside the space's low window (no virtual object
/// of its own). Returns the window handle, which the mapping records.
fn map_in_window(
    io: &Io<'_>,
    memory: u32,
    window: u32,
    flags: u32,
    piece: nm::MappingPiece,
) -> Result<u32, Error> {
    let mut block = nm::nvos46_range(
        io.client.root,
        nd::H_DEVICE,
        window,
        memory,
        piece.memory_offset,
        piece.len,
        flags | MAP_FLAGS_DMA_OFFSET_FIXED,
    );
    block[cc::NVOS46_DMA_OFFSET_AT..cc::NVOS46_DMA_OFFSET_AT + 8]
        .copy_from_slice(&piece.va.to_le_bytes());
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    let reply = response.get(..n).ok_or(Error::Host)?;
    let reply = rc::rm_reply(reply, cc::NVOS46_STATUS_AT).map_err(|error| {
        crate::diag::record_named_bytes(b"NvRmWinSt", rm_error_code(error));
        crate::diag::record_named_bytes(b"NvMapVaPg", (piece.va >> 12) as u32);
        Error::Host
    })?;
    let got = cc::map_dma_va(reply.data).ok_or(Error::Host)?;
    if got != piece.va {
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, window, memory, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        note_wrong_address(piece.va, got);
        return Err(Error::WrongAddress);
    }
    Ok(window)
}

fn unmap_fixed(io: &Io<'_>, memory: u32, mapping: HostMapping) -> Result<(), Error> {
    unmap_fixed_opts(io, memory, mapping, false)
}

/// [`unmap_fixed`]; with `defer` the TLB invalidate is left to a later operation
/// of the same leaf update (NvPerfMem 0x20).
fn unmap_fixed_opts(io: &Io<'_>, memory: u32, mapping: HostMapping, defer: bool) -> Result<(), Error> {
    unmap_fixed_ex(io, memory, mapping, defer).map_err(|(_, error)| error)
}

/// [`unmap_fixed_opts`] that tells an unmap RM refused (`(true, _)`: the mapping
/// may still be there, nothing was freed) from a failure after it (`(false, _)`).
fn unmap_fixed_ex(io: &Io<'_>, memory: u32, mapping: HostMapping, defer: bool) -> Result<(), (bool, Error)> {
    watch::event(mapping.piece.va, mapping.piece.len, 10, 0, mapping.process);
    let memory = if mapping.desc != 0 { mapping.desc } else { memory };
    let mut block = nm::nvos47_unmap(
        io.client.root,
        nd::H_DEVICE,
        mapping.virt,
        memory,
        mapping.piece.va,
    );
    if defer {
        block[np::NVOS47_FLAGS_AT..np::NVOS47_FLAGS_AT + 4]
            .copy_from_slice(&np::UNMAP_FLAGS_DEFER_TLB.to_le_bytes());
        bump(&DEFERRED, b"NvPmDefer");
    }
    let mut response = [0u8; REPLY_MAX];
    let n = io
        .exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &block,
            &[],
            &mut response,
        )
        .map_err(|e| (true, e))?;
    rc::rm_reply(response.get(..n).ok_or((true, Error::Host))?, cc::NVOS47_STATUS_AT)
        .map_err(|_| (true, Error::Host))?;
    // The space's low window outlives its mappings (`release_process`).
    if !is_window(mapping.virt) {
        io.rm_free(nd::H_DEVICE, mapping.virt).map_err(|e| (false, e))?;
    }
    if mapping.desc != 0 {
        // Freed unless another mapping shows it or it is kept idle (`NvPaging` 0x2).
        release_desc(io, mapping.desc).map_err(|e| (false, e))?;
    }
    Ok(())
}

/// A recorded mapping RM would not unmap (or whose unmap's fate is unknown): its
/// virtual object is freed, which takes whatever it still maps with it, and its
/// descriptor reference dropped, so the record can go and the VA be mapped
/// again. A window mapping cannot be freed this way; a map over it is refused
/// and counted then. Best effort (`NvLfFixN`).
fn forget_refused(io: &Io<'_>, mapping: HostMapping, id: u32) {
    if !is_window(mapping.virt) {
        let _ = io.rm_free(nd::H_DEVICE, mapping.virt);
    }
    if mapping.desc != 0 {
        let _ = release_desc(io, mapping.desc);
    }
    leaf_fix(ptr::FIX_UNMAP, id, mapping.piece.va, mapping.piece.len, mapping.process);
}

/// An explicit TLB invalidate of `space` (`NV0080_CTRL_CMD_DMA_INVALIDATE_TLB`),
/// for a leaf update whose deferred operations were not followed by the one
/// that invalidates (it failed). Best effort, counted (`NvPmTlbInv`, refusals
/// `NvPmTlbErr`).
fn invalidate_tlb(io: &Io<'_>, space: u32) {
    let params = np::invalidate_tlb_params(space);
    let block = rc::nvos54(
        io.client.root,
        nd::H_DEVICE,
        np::CTRL_DMA_INVALIDATE_TLB,
        params.len() as u32,
    );
    let mut response = [0u8; REPLY_MAX];
    let sent = io.exchange(
        io.client.ctl,
        rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
        &block,
        &params,
        &mut response,
    );
    let ok = match sent {
        Ok(n) => response
            .get(..n)
            .is_some_and(|reply| rc::rm_reply(reply, rc::NVOS54_STATUS_AT).is_ok()),
        Err(_) => false,
    };
    bump(&TLB_RESCUE, b"NvPmTlbInv");
    crate::ddi::pt_ring::record(ptr::K_TLB_INV, 0, u16::from(!ok), space, 0, 0, 0);
    if !ok {
        bump(&TLB_RESCUE_FAILED, b"NvPmTlbErr");
    }
}

// ── paging fast paths: shared/cached OS descriptors, batched RM calls ─────────────────────

/// Whether this update's RM calls go to the host as `RmBatch`es: `NvPaging` 0x1,
/// the host's `NVGPU_CFG_RM_BATCH`, and no whole-batch refusal from it so far.
fn batch_on(io: &Io<'_>) -> bool {
    paging().batch()
        && BATCH_REFUSED.load(Ordering::Relaxed) == 0
        && io
            .adapter
            .with_virtio(|v| v.nvrm_device_features())
            .unwrap_or(0)
            & helios_protocol::NVGPU_CFG_RM_BATCH
            != 0
}

/// Why a batch did not come back answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchFailure {
    /// Nothing ran (the host refused it whole, or it never left): use single calls.
    NotSent,
    /// Unknown what ran (a timeout, a reset): treat every entry as failed and
    /// keep whatever the host may still read.
    Indeterminate(Error),
}

/// Send `batch` and leave its checked reply in `resp`.
fn send_batch(io: &Io<'_>, batch: &rb::Batch, resp: &mut Vec<u8>) -> Result<(), BatchFailure> {
    resp.clear();
    resp.try_reserve_exact(batch.reply_len())
        .map_err(|_| BatchFailure::NotSent)?;
    resp.resize(batch.reply_len(), 0);
    ROUND_TRIPS.fetch_add(1, Ordering::Relaxed);
    let start = crate::adapter::foreign_scanout::now_100ns();
    let sent = nvrm::forward_kmd_batch(io.passive, io.adapter, batch.request(), resp, TIMEOUT_MS);
    let ticks = crate::adapter::foreign_scanout::now_100ns().saturating_sub(start);
    ph_add(Acc::Rt, ticks);
    let us = np::ticks_to_us(ticks);
    BATCH_US.fetch_add(us, Ordering::Relaxed);
    BATCH_MAX_US.fetch_max(us, Ordering::Relaxed);
    let n = match sent {
        Ok((n, epoch)) => {
            if epoch != io.client.epoch {
                BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
                return Err(BatchFailure::Indeterminate(Error::NotReady));
            }
            n
        }
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::Indeterminate(Error::Host));
        }
        Err(Refusal::Transport(VirtioError::DeviceError)) => {
            // The transport is gone: what reached the host is unknown.
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::Indeterminate(Error::Host));
        }
        Err(_) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            return Err(BatchFailure::NotSent);
        }
    };
    resp.truncate(n);
    match rb::check_reply(resp, batch.caps()) {
        Ok(_) => {
            BATCHES.fetch_add(1, Ordering::Relaxed);
            BATCH_CALLS.fetch_add(batch.len() as u32, Ordering::Relaxed);
            Ok(())
        }
        Err(rb::ReplyError::Refused(_)) => {
            // Nothing ran. A host that refuses one refuses them all: single calls
            // from now on (`PgBatchRef`).
            BATCH_REFUSED.fetch_add(1, Ordering::Relaxed);
            Err(BatchFailure::NotSent)
        }
        Err(_) => {
            BATCH_FAILED.fetch_add(1, Ordering::Relaxed);
            Err(BatchFailure::Indeterminate(Error::Host))
        }
    }
}

/// Whether entry `index` of a checked batch reply was served and RM said OK.
fn entry_ok(resp: &[u8], caps: &[u32], index: usize) -> bool {
    rb::entry_reply(resp, caps, index).is_some_and(|e| e.served && !e.failed)
}

/// One mapping of `pfns` (one 4-KiB PTE each) at `va` through an OS descriptor.
#[derive(Clone, Copy)]
struct DescJob<'a> {
    va: u64,
    pfns: &'a [u64],
    defer: bool,
}

/// A made descriptor mapping: (virtual object or window, descriptor); or where it
/// failed (2 the descriptor, 3 mapping it) and why.
type DescMade = Result<(u32, u32), (u32, Error)>;

/// An OS descriptor over exactly `pfns`: one already made (`PgDescHit`) or a new
/// handle to register (`.1` true).
fn take_desc(pfns: &[u64], owner: u32) -> Result<(u32, bool), Error> {
    ph_scope(Acc::Lookup, || take_desc_inner(pfns, owner))
}

fn take_desc_inner(pfns: &[u64], owner: u32) -> Result<(u32, bool), Error> {
    let mut state = st();
    if let Some((desc, was_idle)) = state.descs.acquire(pfns, owner) {
        DESC_HIT.fetch_add(1, Ordering::Relaxed);
        if was_idle {
            DESC_IDLE_HIT.fetch_add(1, Ordering::Relaxed);
        }
        return Ok((desc, false));
    }
    Ok((state.mint()?, true))
}

/// A descriptor taken by [`take_desc`] whose mapping was not made: a new one is
/// freed, a reused one gives its reference back.
fn give_back_desc(io: &Io<'_>, desc: u32, new: bool) {
    if new {
        let _ = io.rm_free(nd::H_DEVICE, desc);
        DESC_FREED.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = release_desc(io, desc);
    }
}

/// One mapping of `desc` went: free it if nothing else maps it and it is not
/// kept idle (`NvPaging` 0x2), and free the idle ones the bound pushes out.
fn release_desc(io: &Io<'_>, desc: u32) -> Result<(), Error> {
    let mut evicted = Vec::new();
    let released = st()
        .descs
        .release(desc, desc_cache_on(), desc_cache_max(), &mut evicted);
    free_descs(io, &evicted, &DESC_EVICTED);
    match released {
        Released::Free => {
            DESC_FREED.fetch_add(1, Ordering::Relaxed);
            io.rm_free(nd::H_DEVICE, desc)
        }
        Released::Cached | Released::InUse => Ok(()),
    }
}

/// Free OS descriptors nothing maps (best effort; batched when on).
fn free_descs(io: &Io<'_>, descs: &[u32], counter: &AtomicU32) {
    if descs.is_empty() {
        return;
    }
    counter.fetch_add(descs.len() as u32, Ordering::Relaxed);
    if batch_on(io) && descs.len() > 1 {
        let mut batch = rb::Batch::new();
        let mut resp = Vec::new();
        let mut rest = descs;
        while !rest.is_empty() {
            batch.clear();
            let mut taken = 0;
            for desc in rest {
                let block = rc::nvos00(io.client.root, nd::H_DEVICE, *desc);
                let pushed = batch.push_ioctl(
                    io.client.ctl,
                    rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
                    &block,
                    &[],
                    None,
                    Some(rc::NVOS00_STATUS_AT),
                    0,
                    0,
                );
                if pushed.is_err() {
                    break;
                }
                taken += 1;
            }
            if taken == 0 || send_batch(io, &batch, &mut resp) == Err(BatchFailure::NotSent) {
                break;
            }
            rest = &rest[taken..];
        }
        if rest.is_empty() {
            return;
        }
        for desc in rest {
            let _ = io.rm_free(nd::H_DEVICE, *desc);
        }
        return;
    }
    for desc in descs {
        let _ = io.rm_free(nd::H_DEVICE, *desc);
    }
}

/// Drop the idle descriptors of allocation `id` (its system pages are no longer
/// known to be its own: it moved to video memory, or it is destroyed).
fn drop_idle_descs_of(io: &Io<'_>, id: u32) {
    let mut gone = Vec::new();
    st().descs.drop_idle_of(id, &mut gone);
    free_descs(io, &gone, &DESC_EVICTED);
}

/// Make `jobs`' descriptor mappings in `space` for allocation `owner` (0: foreign
/// pages), one result per job in `out`, in order. Batched under `NvPaging` 0x1,
/// else one round trip per call; descriptors over the same pages are reused.
fn map_descs(io: &Io<'_>, owner: u32, space: u32, jobs: &[DescJob<'_>], out: &mut Vec<DescMade>) -> Result<(), Error> {
    out.clear();
    out.try_reserve_exact(jobs.len()).map_err(|_| Error::NoMemory)?;
    let mut done = 0;
    if batch_on(io) {
        done = map_descs_batched(io, owner, space, jobs, out);
    }
    for job in &jobs[done..] {
        out.push(map_desc_single(io, owner, space, job));
    }
    Ok(())
}

/// One descriptor mapping, a round trip per call (the path before `NvPaging`).
fn map_desc_single(io: &Io<'_>, owner: u32, space: u32, job: &DescJob<'_>) -> DescMade {
    let len = (job.pfns.len() as u64)
        .checked_mul(nm::PAGE_BYTES)
        .ok_or((2, Error::Invalid))?;
    let (desc, new) = take_desc(job.pfns, owner).map_err(|e| (2, e))?;
    if new {
        alloc_system_descriptor(io, desc, job.pfns, len).map_err(|e| (2, e))?;
        DESC_NEW.fetch_add(1, Ordering::Relaxed);
    }
    // Minted first: a STATE guard in a `match` scrutinee would live through the
    // arms, and `give_back_desc` takes STATE again.
    let minted = st().mint();
    let virt = match minted {
        Ok(virt) => virt,
        Err(error) => {
            give_back_desc(io, desc, new);
            return Err((3, error));
        }
    };
    let shown = nm::MappingPiece {
        va: job.va,
        memory_offset: 0,
        len,
    };
    match map_fixed_opts(io, desc, len, true, space, virt, shown, job.defer) {
        Ok(used) => {
            if new {
                // Unrecorded (no memory): it is freed at its unmap, as before.
                let _ = st().descs.insert(desc, job.pfns, owner);
            }
            Ok((used, desc))
        }
        Err(error) => {
            give_back_desc(io, desc, new);
            Err((3, error))
        }
    }
}

/// What one job of a batch planned.
struct Planned {
    desc: u32,
    new_desc: bool,
    /// The virtual object to make, 0 when the job maps into a window.
    virt: u32,
    window: u32,
    len: u64,
    at_desc: Option<usize>,
    at_virt: Option<usize>,
    at_map: usize,
    /// An indirect page-run table the host reads while serving the batch.
    big: Option<super::hal::DmaBuffer>,
}

/// [`map_descs`] in batches of up to 16 jobs: each job's registration (when its
/// pages have no descriptor yet), virtual object (outside the windows) and map,
/// one group that stops at its first failure. Returns how many jobs it answered
/// (pushed into `out`); the rest go the single-call way.
fn map_descs_batched(io: &Io<'_>, owner: u32, space: u32, jobs: &[DescJob<'_>], out: &mut Vec<DescMade>) -> usize {
    let mut batch = rb::Batch::new();
    let mut resp = Vec::new();
    let mut plans: Vec<Planned> = Vec::new();
    let mut done = 0;
    while done < jobs.len() {
        batch.clear();
        plans.clear();
        let before = done;
        // Plan as many jobs as fit.
        for (k, job) in jobs[done..].iter().enumerate() {
            if plans.try_reserve(1).is_err() {
                break;
            }
            match plan_desc_job(io, owner, space, job, k as u16, &mut batch) {
                Ok(plan) => plans.push(plan),
                Err(None) => break, // full: send what is planned
                Err(Some(error)) => {
                    if plans.is_empty() {
                        // This job cannot be planned (no memory, a refused table).
                        out.push(Err((2, error)));
                        done += 1;
                    }
                    break;
                }
            }
        }
        if plans.is_empty() {
            if done == before {
                // Not even one job fits a batch: the rest go one call at a time.
                return done;
            }
            continue;
        }
        match send_batch(io, &batch, &mut resp) {
            Ok(()) => {
                for (plan, job) in plans.drain(..).zip(&jobs[done..]) {
                    out.push(finish_desc_job(io, owner, space, job, plan, &resp, batch.caps()));
                    done += 1;
                }
            }
            Err(BatchFailure::NotSent) => {
                // Nothing ran: undo the plans, the rest go one call at a time.
                for plan in plans.drain(..) {
                    if !plan.new_desc {
                        let _ = release_desc(io, plan.desc);
                    }
                }
                return done;
            }
            Err(BatchFailure::Indeterminate(error)) => {
                // The host may hold any of it and may still read the tables:
                // keep them, leak the handles (never minted again).
                for plan in plans.drain(..) {
                    core::mem::forget(plan.big);
                    out.push(Err((if plan.new_desc { 2 } else { 3 }, error)));
                    done += 1;
                }
            }
        }
    }
    done
}

/// Plan one job into `batch`: `Err(None)` when it does not fit (nothing taken),
/// `Err(Some(_))` when it cannot be planned at all.
fn plan_desc_job(
    io: &Io<'_>,
    owner: u32,
    space: u32,
    job: &DescJob<'_>,
    group: u16,
    batch: &mut rb::Batch,
) -> Result<Planned, Option<Error>> {
    let len = (job.pfns.len() as u64)
        .checked_mul(nm::PAGE_BYTES)
        .ok_or(Some(Error::Invalid))?;
    let end = job.va.checked_add(len).ok_or(Some(Error::Invalid))?;
    let window = window_for(space, job.va, end).unwrap_or(0);
    let (desc, new_desc) = take_desc(job.pfns, owner).map_err(Some)?;
    let mark = batch.mark();
    // A job goes in whole or not at all: undo what was taken and added.
    let undo = |batch: &mut rb::Batch, error: Option<Error>| {
        batch.rollback(mark);
        if !new_desc {
            let _ = release_desc(io, desc);
        }
        error
    };
    let pushed = |e: rb::PushError| match e {
        rb::PushError::Full => None,
        rb::PushError::NoMemory => Some(Error::NoMemory),
        rb::PushError::Invalid => Some(Error::Invalid),
    };
    let flags = rb::SKIP_GROUP_ON_FAIL;
    let mut big = None;
    let at_desc = if new_desc {
        let (kind, deep, table) = match ph_scope(Acc::Table, || nvrm::page_run_table(io.passive, job.pfns)) {
            Ok(t) => t,
            Err(_) => return Err(undo(batch, Some(Error::NoMemory))),
        };
        big = table;
        let block = cr::nvos02_osdesc(
            io.client.root,
            nd::H_DEVICE,
            desc,
            job.pfns.first().copied().unwrap_or(0).wrapping_mul(nm::PAGE_BYTES),
            len,
        );
        match batch.push_ioctl(
            io.client.gpu,
            rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32),
            &block,
            &[],
            Some((kind, &deep)),
            Some(cr::NVOS02_STATUS_AT),
            group,
            flags,
        ) {
            Ok(at) => Some(at),
            Err(e) => return Err(undo(batch, pushed(e))),
        }
    } else {
        None
    };
    let mut virt = 0;
    let at_virt = if window == 0 {
        let minted = st().mint();
        virt = match minted {
            Ok(virt) => virt,
            Err(error) => return Err(undo(batch, Some(error))),
        };
        let params = cc::virtual_params_4k(io.client.root, space, Some(job.va), len);
        let block = rc::nvos64(
            io.client.root,
            nd::H_DEVICE,
            virt,
            cc::NV50_MEMORY_VIRTUAL,
            params.len() as u32,
        );
        match batch.push_ioctl(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_ALLOC, rc::NVOS64_BYTES as u32),
            &block,
            &params,
            None,
            Some(rc::NVOS64_STATUS_AT),
            group,
            flags,
        ) {
            Ok(at) => Some(at),
            Err(e) => return Err(undo(batch, pushed(e))),
        }
    } else {
        None
    };
    let defer_flag = if job.defer { np::MAP_FLAGS_DEFER_TLB } else { 0 };
    let mut map_flags = cc::MAP_FLAGS_SYSMEM | defer_flag;
    let target = if window != 0 {
        map_flags |= MAP_FLAGS_DMA_OFFSET_FIXED;
        window
    } else {
        virt
    };
    let mut block = nm::nvos46_range(io.client.root, nd::H_DEVICE, target, desc, 0, len, map_flags);
    if window != 0 {
        block[cc::NVOS46_DMA_OFFSET_AT..cc::NVOS46_DMA_OFFSET_AT + 8]
            .copy_from_slice(&job.va.to_le_bytes());
    }
    let at_map = match batch.push_ioctl(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        None,
        Some(cc::NVOS46_STATUS_AT),
        group,
        flags,
    ) {
        Ok(at) => at,
        Err(e) => return Err(undo(batch, pushed(e))),
    };
    Ok(Planned {
        desc,
        new_desc,
        virt,
        window,
        len,
        at_desc,
        at_virt,
        at_map,
        big,
    })
}

/// Read one job's entries of an answered batch; undo what was made of a failed one.
fn finish_desc_job(
    io: &Io<'_>,
    owner: u32,
    space: u32,
    job: &DescJob<'_>,
    plan: Planned,
    resp: &[u8],
    caps: &[u32],
) -> DescMade {
    let _ = space;
    drop(plan.big);
    let desc_ok = plan.at_desc.is_none_or(|i| entry_ok(resp, caps, i));
    let virt_ok = plan.at_virt.is_none_or(|i| entry_ok(resp, caps, i));
    let map_reply = rb::entry_reply(resp, caps, plan.at_map);
    let mapped_at = map_reply
        .filter(|e| e.served && !e.failed)
        .and_then(|e| rc::parse_ioctl_reply(e.bytes).ok())
        .and_then(|reply| cc::map_dma_va(reply.data));
    if desc_ok && plan.at_desc.is_some() {
        DESC_NEW.fetch_add(1, Ordering::Relaxed);
    }
    if desc_ok && virt_ok && mapped_at == Some(job.va) {
        if job.defer {
            bump(&DEFERRED, b"NvPmDefer");
        }
        if plan.new_desc {
            let _ = st().descs.insert(plan.desc, job.pfns, owner);
        }
        return Ok((if plan.window != 0 { plan.window } else { plan.virt }, plan.desc));
    }
    // Failed: say where, and take down what was made.
    if !desc_ok {
        if let Some(e) = plan.at_desc.and_then(|i| rb::entry_reply(resp, caps, i)) {
            // NvSysHost: RM's NV_STATUS, 0x1000_0000 for a host refusal.
            let code = e.rm_status.unwrap_or(0x1000_0000);
            crate::diag::record_named_bytes(b"NvSysHost", code);
            crate::diag::record_named_bytes(b"NvSysPages", job.pfns.len() as u32);
        }
        // The rest of the group was skipped: nothing else exists.
        return Err((2, Error::Host));
    }
    let error = if let Some(got) = mapped_at {
        // RM must not relocate a WDDM mapping: take down what it made.
        let target = if plan.window != 0 { plan.window } else { plan.virt };
        let unmap = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, target, plan.desc, got);
        let mut ignored = [0u8; REPLY_MAX];
        let _ = io.exchange(
            io.client.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &unmap,
            &[],
            &mut ignored,
        );
        note_wrong_address(job.va, got);
        Error::WrongAddress
    } else {
        if let Some(e) = map_reply.filter(|e| e.served) {
            crate::diag::record_named_bytes(
                if plan.window != 0 { b"NvRmWinSt" } else { b"NvRmMapSt" },
                e.rm_status.unwrap_or(0xffff_ffff),
            );
        }
        if !virt_ok {
            crate::diag::record_named_bytes(b"NvMapVaPg", (job.va >> 12) as u32);
            crate::diag::record_named_bytes(b"NvMapLenPg", (plan.len >> 12) as u32);
        }
        Error::Host
    };
    if plan.virt != 0 && virt_ok {
        let _ = io.rm_free(nd::H_DEVICE, plan.virt);
    }
    give_back_desc(io, plan.desc, plan.new_desc);
    Err((3, error))
}

/// Unmap each `(memory, mapping, defer)` (the descriptor of a descriptor mapping
/// given back, [`release_desc`]): one result per item in `out`. Batched under
/// `NvPaging` 0x1; with `abort` the first failure stops the rest (as a loop of
/// [`unmap_fixed_opts`] that returns at its first error does).
fn unmap_many(io: &Io<'_>, items: &[(u32, HostMapping, bool)], abort: bool, out: &mut Vec<Result<(), Error>>) -> Result<(), Error> {
    let mut refused = Vec::new();
    unmap_many_ex(io, items, abort, out, &mut refused)
}

/// [`unmap_many`] that also tells, per item, whether the unmap itself was refused
/// or not attempted (`refused[i]`: the mapping may still be there and nothing of
/// it was freed) rather than failing after it.
fn unmap_many_ex(
    io: &Io<'_>,
    items: &[(u32, HostMapping, bool)],
    abort: bool,
    out: &mut Vec<Result<(), Error>>,
    refused: &mut Vec<bool>,
) -> Result<(), Error> {
    out.clear();
    refused.clear();
    out.try_reserve_exact(items.len()).map_err(|_| Error::NoMemory)?;
    refused.try_reserve_exact(items.len()).map_err(|_| Error::NoMemory)?;
    let mut done = 0;
    if batch_on(io) && !items.is_empty() {
        done = unmap_many_batched(io, items, abort, out, refused);
    }
    for (memory, mapping, defer) in &items[done..] {
        if abort && out.iter().any(|r| r.is_err()) {
            out.push(Err(Error::Host));
            refused.push(true);
            continue;
        }
        match unmap_fixed_ex(io, *memory, *mapping, *defer) {
            Ok(()) => {
                out.push(Ok(()));
                refused.push(false);
            }
            Err((was_refused, error)) => {
                out.push(Err(error));
                refused.push(was_refused);
            }
        }
    }
    Ok(())
}

fn unmap_many_batched(
    io: &Io<'_>,
    items: &[(u32, HostMapping, bool)],
    abort: bool,
    out: &mut Vec<Result<(), Error>>,
    refused: &mut Vec<bool>,
) -> usize {
    struct Plan {
        at_unmap: usize,
        at_virt: Option<usize>,
        at_desc: Option<usize>,
    }
    let mut batch = rb::Batch::new();
    let mut resp = Vec::new();
    let mut plans: Vec<Plan> = Vec::new();
    let mut done = 0;
    let cache = desc_cache_on();
    let max_idle = desc_cache_max();
    while done < items.len() {
        if abort && out.iter().any(|r| r.is_err()) {
            return done;
        }
        batch.clear();
        plans.clear();
        for (k, (memory, m, defer)) in items[done..].iter().enumerate() {
            if plans.try_reserve(1).is_err() {
                break;
            }
            let mark = batch.mark();
            let group = k as u16;
            let mut flags = rb::SKIP_GROUP_ON_FAIL;
            if abort {
                flags |= rb::ABORT_ON_FAIL;
            }
            let target = if m.desc != 0 { m.desc } else { *memory };
            let mut block = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, m.virt, target, m.piece.va);
            if *defer {
                block[np::NVOS47_FLAGS_AT..np::NVOS47_FLAGS_AT + 4]
                    .copy_from_slice(&np::UNMAP_FLAGS_DEFER_TLB.to_le_bytes());
            }
            let Ok(at_unmap) = batch.push_ioctl(
                io.client.ctl,
                rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
                &block,
                &[],
                None,
                Some(cc::NVOS47_STATUS_AT),
                group,
                flags,
            ) else {
                break;
            };
            // `Err(())`: it did not fit; the item goes whole into the next batch.
            let free = |batch: &mut rb::Batch, object: u32| -> Result<Option<usize>, ()> {
                let block = rc::nvos00(io.client.root, nd::H_DEVICE, object);
                batch
                    .push_ioctl(
                        io.client.ctl,
                        rc::nv_cmd(rc::ESC_RM_FREE, rc::NVOS00_BYTES as u32),
                        &block,
                        &[],
                        None,
                        Some(rc::NVOS00_STATUS_AT),
                        group,
                        rb::SKIP_GROUP_ON_FAIL,
                    )
                    .map(Some)
                    .map_err(|_| ())
            };
            let at_virt = if is_window(m.virt) { Ok(None) } else { free(&mut batch, m.virt) };
            let at_desc = if m.desc != 0 && st().descs.would_free(m.desc, cache, max_idle) {
                free(&mut batch, m.desc)
            } else {
                Ok(None)
            };
            let (Ok(at_virt), Ok(at_desc)) = (at_virt, at_desc) else {
                batch.rollback(mark);
                break;
            };
            plans.push(Plan { at_unmap, at_virt, at_desc });
        }
        if plans.is_empty() {
            return done;
        }
        match send_batch(io, &batch, &mut resp) {
            Ok(()) => {}
            Err(BatchFailure::NotSent) => return done,
            Err(BatchFailure::Indeterminate(error)) => {
                for _ in plans.drain(..) {
                    out.push(Err(error));
                    refused.push(true);
                    done += 1;
                }
                continue;
            }
        }
        let caps = batch.caps();
        for (plan, (_, m, _)) in plans.drain(..).zip(&items[done..]) {
            done += 1;
            if !entry_ok(&resp, caps, plan.at_unmap) {
                out.push(Err(Error::Host));
                refused.push(true);
                continue;
            }
            watch::event(m.piece.va, m.piece.len, 10, 0, m.process);
            let virt_ok = plan.at_virt.is_none_or(|i| entry_ok(&resp, caps, i));
            let mut result = if virt_ok { Ok(()) } else { Err(Error::Host) };
            if m.desc != 0 {
                match plan.at_desc {
                    Some(i) if entry_ok(&resp, caps, i) => {
                        st().descs.forget(m.desc);
                        DESC_FREED.fetch_add(1, Ordering::Relaxed);
                    }
                    // Not freed in the batch (kept, or its free was skipped).
                    _ => {
                        if let Err(error) = release_desc(io, m.desc) {
                            result = Err(error);
                        }
                    }
                }
            }
            out.push(result);
            refused.push(false);
        }
    }
    done
}

/// Write the record values CreateAllocation produced for native allocation
/// `id` into `bytes` (an NVDA record dxgkrnl passes to OpenAllocation). The
/// UMD reads sizes and handles from it: without them the D3D12 UMD reserved a
/// zero-size VA range (INVALID_PARAMETER) and removed its device.
/// What `native_scanout` needs to show an allocation: its RM memory, size,
/// 2D picture and the GEM handle it was given before (0 = none yet).
#[derive(Clone, Copy, Debug)]
pub struct ScanoutSource {
    pub memory: u32,
    pub size: u64,
    pub surface: nm::D3dSurface,
    pub gem: u32,
    pub system_backed: bool,
}

pub fn scanout_source(id: u32) -> Result<ScanoutSource, Error> {
    let state = st();
    let a = state.allocation(id)?;
    let surface = a.surface.ok_or(Error::Unsupported)?;
    Ok(ScanoutSource {
        memory: a.memory,
        size: a.size,
        surface,
        gem: a.scanout_gem,
        system_backed: a.system_memory != 0,
    })
}

/// The GEM handle and client epoch of a primary already imported for scanout on
/// the live client (`None`: not imported, poisoned, another epoch, no client).
/// STATE spinlock only, no I/O mutex: `native_scanout`'s flip fast path
/// (`NvHotPath` 0x1).
pub fn scanout_imported(id: u32) -> Option<(u32, u64)> {
    let state = st();
    let client = state.client.as_ref()?;
    let a = state.allocation(id).ok()?;
    if a.scanout_gem == 0 || a.poisoned || a.epoch != client.epoch || a.surface.is_none() {
        return None;
    }
    Some((a.scanout_gem, client.epoch))
}

/// The width and height of an allocation's 2D picture (DescribeAllocation).
pub fn surface_extent(id: u32) -> Option<(u32, u32)> {
    let state = st();
    let s = state.allocation(id).ok()?.surface?;
    Some((s.width, s.height))
}

pub fn set_scanout_gem(id: u32, gem: u32) -> Result<(), Error> {
    st().allocation_mut(id)?.scanout_gem = gem;
    Ok(())
}

/// Run `f` with the native-memory I/O (the PASSIVE mutex held, the KMD client).
pub(crate) fn with_io<R>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    f: impl FnOnce(&Io<'_>) -> Result<R, Error>,
) -> Result<R, Error> {
    let _guard = lock(passive, Kind::WithIo)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    f(&io)
}

pub fn writeback_open(id: u32, bytes: &mut [u8]) -> Result<(), Error> {
    let writeback = st().allocation(id)?.writeback.ok_or(Error::Invalid)?;
    writeback.apply(bytes).map_err(|_| Error::Invalid)
}

/// How many mappings `evict_range` dropped (NvEvict) and how many of their
/// outside remainders could not be shown again (NvEvictFail).
static EVICTED: AtomicU32 = AtomicU32::new(0);
static EVICT_FAILED: AtomicU32 = AtomicU32::new(0);

/// Which mappings [`evict_range`] drops.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Evict {
    /// Every allocation's but this one's, and the foreign ranges.
    Others(u32),
    /// Every allocation's and the foreign ranges.
    All,
    /// This allocation's only (a leaf update of its own range).
    Only(u32),
}

impl Evict {
    fn takes(self, id: u32) -> bool {
        match self {
            Self::Others(keep) => id != keep,
            Self::All => true,
            Self::Only(only) => id == only,
        }
    }

    fn foreign(self) -> bool {
        !matches!(self, Self::Only(_))
    }
}

/// Drop every mapping of `process` in `[first_va, first_va + len)` that VidMm's
/// page table no longer shows there (`which`: foreign ranges and other
/// allocations, or the updated allocation's own). The parts of a dropped
/// mapping outside the range are mapped again (RM unmaps whole objects), as
/// what they showed: the RM memory at the same offset, or the same system
/// pages. A recorded mapping RM will not unmap has its virtual object freed
/// instead (`forget_refused`); records that overlap each other keep the first
/// (`leaf_plan::overlapping_entries`). A part that cannot be shown again stays
/// unmapped and is counted (`NvEvictFail`, `NvLfFail`). With `defer` the unmaps
/// defer their TLB invalidates; the answer is whether one is still owed (no
/// later non-deferred operation here settled it). Callers hold the RM lock.
#[allow(clippy::too_many_arguments)]
fn evict_range(
    io: &Io<'_>,
    space: u32,
    process: usize,
    first_va: u64,
    len: u64,
    which: Evict,
    defer: bool,
) -> Result<bool, Error> {
    // NvEvictOff=1 (test knob): leave other mappings alone, as before eviction.
    static EVICT_OFF: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut off = EVICT_OFF.load(Ordering::Relaxed);
    if off == u32::MAX {
        off = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvEvictOff"), 0);
        EVICT_OFF.store(off, Ordering::Relaxed);
    }
    if off == 1 && which.foreign() {
        return Ok(false);
    }
    let hit = |m: &HostMapping| {
        m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
    };
    struct Victim {
        id: u32,
        memory: u32,
        size: u64,
        system_backed: bool,
        mapping: HostMapping,
        pfns: Option<Vec<u64>>,
    }
    let mut candidates = Vec::new();
    let (foreign, victims) = {
        let mut state = st();
        let mut foreign = Vec::new();
        let mut victims = Vec::new();
        let mut emptied = false;
        match which {
            Evict::Only(id) => {
                candidates.try_reserve(1).map_err(|_| Error::NoMemory)?;
                candidates.push(id);
            }
            _ => {
                // Only this process's allocations can map here (`by_process`).
                state.candidates(process, &mut candidates)?;
                if state.foreign.iter().any(|m| hit(m)) {
                    foreign.try_reserve(state.foreign.len()).map_err(|_| Error::NoMemory)?;
                    foreign.extend(state.foreign.iter().copied().filter(|m| hit(m)));
                    state.foreign.retain(|m| !hit(m));
                }
            }
        }
        for &id in candidates.iter().filter(|id| which.takes(**id)) {
            let Some(index) = state.index_of(id) else {
                emptied = true;
                continue;
            };
            let allocation = &state.allocations[index];
            let (memory, size, system_backed) =
                (allocation.memory, allocation.size, allocation.system_memory != 0);
            if !allocation.mappings.iter().any(|m| hit(m)) {
                emptied |= !allocation.mappings.iter().any(|m| m.process == process);
                continue;
            }
            for mapping in allocation.mappings.iter().copied().filter(|m| hit(m)) {
                let pfns = if mapping.desc != 0 {
                    state.descs.pfns(mapping.desc).and_then(|pfns| {
                        let mut copy = Vec::new();
                        copy.try_reserve_exact(pfns.len()).ok()?;
                        copy.extend_from_slice(pfns);
                        Some(copy)
                    })
                } else {
                    None
                };
                victims.try_reserve(1).map_err(|_| Error::NoMemory)?;
                victims.push(Victim { id, memory, size, system_backed, mapping, pfns });
            }
            state.allocations[index].mappings.retain(|m| !hit(m));
        }
        if emptied {
            state.prune_index(process);
        }
        (foreign, victims)
    };
    if foreign.is_empty() && victims.is_empty() {
        return Ok(false);
    }
    let own = !which.foreign();
    if !own {
        // `NvEvict`: published by the mirror pass (`publish_hot`).
        EVICTED.fetch_add((foreign.len() + victims.len()) as u32, Ordering::Relaxed);
    }
    for mapping in foreign.iter() {
        crate::ddi::pt_ring::record(ptr::K_EVICT, 0, 0, 0, mapping.piece.va, mapping.piece.len, process);
    }
    let failed = || {
        let n = EVICT_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvEvictFail", n);
    };
    // Every unmap in as few round trips as the batch allows (`NvPaging` 0x1).
    let mut items = Vec::new();
    items
        .try_reserve_exact(foreign.len() + victims.len())
        .map_err(|_| Error::NoMemory)?;
    items.extend(foreign.iter().map(|m| (0u32, *m, false)));
    items.extend(victims.iter().map(|v| (v.memory, v.mapping, defer)));
    let mut results = Vec::new();
    let mut refused = Vec::new();
    unmap_many_ex(io, &items, false, &mut results, &mut refused)?;
    let mut pending = false;
    for (k, mapping) in foreign.iter().enumerate() {
        if results.get(k).is_some_and(|r| r.is_err()) && refused.get(k).copied().unwrap_or(true) {
            forget_refused(io, *mapping, 0);
        }
    }
    // The outside parts of every dropped mapping: (victim, piece, distance).
    let mut rests: Vec<(usize, nm::MappingPiece, u64)> = Vec::new();
    for (v, victim) in victims.iter().enumerate() {
        let k = foreign.len() + v;
        let m = victim.mapping;
        if !own {
            crate::ddi::pt_ring::record(ptr::K_EVICT, 0, 0, victim.id, m.piece.va, m.piece.len, process);
        }
        match results.get(k) {
            Some(Ok(())) => pending |= defer,
            _ => {
                if refused.get(k).copied().unwrap_or(true) {
                    forget_refused(io, m, victim.id);
                }
            }
        }
        for (piece, delta) in lp::outside_parts(m.piece, first_va, len).into_iter().flatten() {
            rests.try_reserve(1).map_err(|_| Error::NoMemory)?;
            rests.push((v, piece, delta));
        }
    }
    if !rests.is_empty() {
        let pieces: Vec<nm::MappingPiece> = rests.iter().map(|r| r.1).collect();
        for drop_at in lp::overlapping_entries(&pieces).into_iter().rev() {
            let (v, piece, _) = rests.remove(drop_at);
            leaf_fix(ptr::FIX_OVERLAP, victims[v].id, piece.va, piece.len, process);
        }
    }
    let mut shown_all: Vec<(u32, HostMapping)> = Vec::new();
    for (v, piece, delta) in rests {
        let victim = &victims[v];
        let m = victim.mapping;
        let (va, bytes) = (piece.va, piece.len);
        let shown = if m.desc == 0 {
            let mut made = Vec::new();
            let result = map_retry(
                io,
                victim.memory,
                victim.size,
                victim.system_backed,
                space,
                piece,
                false,
                process,
                victim.id,
                &mut made,
            );
            if !made.is_empty() {
                pending = false;
            }
            for mapping in made {
                if shown_all.try_reserve(1).is_ok() {
                    shown_all.push((victim.id, mapping));
                } else {
                    let _ = unmap_fixed(io, victim.memory, mapping);
                }
            }
            result.map(|_| ())
        } else {
            (|| -> Result<(), Error> {
                let pfns = victim.pfns.as_ref().ok_or(Error::Invalid)?;
                let first = (delta / nm::PAGE_BYTES) as usize;
                let count = (bytes / nm::PAGE_BYTES) as usize;
                let sub = pfns.get(first..first + count).ok_or(Error::Invalid)?;
                let job = DescJob { va, pfns: sub, defer: false };
                let (used, desc) = map_desc_single(io, victim.id, space, &job).map_err(|(_, e)| e)?;
                pending = false;
                let mapping = HostMapping { process, piece, virt: used, desc };
                if shown_all.try_reserve(1).is_err() {
                    let _ = unmap_fixed(io, 0, mapping);
                    return Err(Error::NoMemory);
                }
                shown_all.push((victim.id, mapping));
                Ok(())
            })()
        };
        crate::ddi::pt_ring::record(ptr::K_EVICT_REST, 0, u16::from(shown.is_err()), victim.id, va, bytes, process);
        if let Err(error) = shown {
            failed();
            leaf_fail(0, 7, error, va, bytes, process);
        }
    }
    // Every shown part recorded under one STATE hold.
    let mut lost = Vec::new();
    {
        let mut state = st();
        for (id, mapping) in shown_all.iter().copied() {
            let recorded = match state.allocation_mut(id) {
                Ok(allocation) => {
                    if allocation.mappings.try_reserve(1).is_ok() {
                        allocation.mappings.push(mapping);
                        allocation
                            .mappings
                            .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
                        true
                    } else {
                        false
                    }
                }
                Err(_) => false,
            };
            if recorded {
                state.note_mapped(process, id);
            } else if lost.try_reserve(1).is_ok() {
                lost.push((id, mapping));
            }
        }
    }
    for (id, mapping) in lost {
        let memory = st().allocation(id).map(|a| a.memory).unwrap_or(0);
        let _ = unmap_fixed(io, memory, mapping);
        failed();
    }
    Ok(pending)
}

/// Map `piece` of the RM memory `memory` at its exact VA (`map_fixed_opts`). A
/// refusal is tried again with a 4-KiB-paged virtual object and 4-KiB PTEs for
/// the whole piece, then cut at every 64 KiB, then at every 4 KiB (at most
/// `leaf_plan::RETRY_PAGES_MAX` pages): a stale RM object at part of the range,
/// or a VA RM will not place a big-page object at, then costs only that part.
/// Every mapping made is appended to `out`; a part still refused is counted
/// (`NvLfFail`, ring `K_LEAF_FAIL`) and the answer is its error. `Ok(true)` when
/// the one mapping made deferred its TLB invalidate.
#[allow(clippy::too_many_arguments)]
fn map_retry(
    io: &Io<'_>,
    memory: u32,
    size: u64,
    system_backed: bool,
    space: u32,
    piece: nm::MappingPiece,
    defer: bool,
    process: usize,
    id: u32,
    out: &mut Vec<HostMapping>,
) -> Result<bool, Error> {
    out.try_reserve(1).map_err(|_| Error::NoMemory)?;
    let virt = st().mint()?;
    let first = map_fixed_opts(io, memory, size, system_backed, space, virt, piece, defer);
    watch::event(
        piece.va,
        piece.len,
        if first.is_ok() { 5 } else { 6 },
        first.as_ref().err().map_or(0, |e| *e as u32),
        process,
    );
    let error = match first {
        Ok(used) => {
            out.push(HostMapping { process, piece, virt: used, desc: 0 });
            return Ok(defer);
        }
        Err(error) => error,
    };
    note_failed_map(piece.va, 1, error);
    {
        // Who collided: the process key, the allocation, its space and how many
        // allocations / tracked mappings of this process exist.
        let state = st();
        let tracked = state
            .allocations
            .iter()
            .flat_map(|a| a.mappings.iter())
            .filter(|m| m.process == process)
            .count();
        crate::diag::record_named_bytes(b"NvMapProc", process as u32);
        crate::diag::record_named_bytes(b"NvMapId", id);
        crate::diag::record_named_bytes(b"NvMapSpace", space);
        crate::diag::record_named_bytes(b"NvMapTracked", tracked as u32);
        crate::diag::record_named_bytes(b"NvMapSpaces", state.spaces.len() as u32);
    }
    if matches!(error, Error::Invalid | Error::NoMemory) {
        // The piece itself is not mappable (outside the memory) or no handle is
        // left: no other shape helps.
        leaf_fail(13, 1, error, piece.va, piece.len, process);
        return Err(error);
    }
    let flags = if system_backed { cc::MAP_FLAGS_SYSMEM } else { LOCAL_4K_MAP_FLAGS };
    let try_4k = |p: nm::MappingPiece, out: &mut Vec<HostMapping>| -> Result<(), Error> {
        out.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let virt = st().mint()?;
        let used = map_fixed_flags(io, memory, size, system_backed, space, virt, p, flags, true)?;
        watch::event(p.va, p.len, 5, 0, process);
        out.push(HostMapping { process, piece: p, virt: used, desc: 0 });
        Ok(())
    };
    let mut pending: Vec<nm::MappingPiece> = Vec::new();
    let mut last = match try_4k(piece, out) {
        Ok(()) => {
            leaf_fix(ptr::FIX_RETRY, id, piece.va, piece.len, process);
            return Ok(false);
        }
        Err(error) => {
            pending.try_reserve(1).map_err(|_| Error::NoMemory)?;
            pending.push(piece);
            error
        }
    };
    for unit in [lp::BIG_PAGE, nm::PAGE_BYTES] {
        let pages: u64 = pending.iter().map(|p| p.len / nm::PAGE_BYTES).sum();
        if unit == nm::PAGE_BYTES && pages > lp::RETRY_PAGES_MAX {
            break;
        }
        let mut next = Vec::new();
        for p in pending.iter().copied() {
            let parts = lp::split_at(p, unit);
            if parts.len() <= 1 {
                // Already this small: tried as it is.
                next.try_reserve(1).map_err(|_| Error::NoMemory)?;
                next.push(p);
                continue;
            }
            for sub in parts {
                if let Err(error) = try_4k(sub, out) {
                    last = error;
                    next.try_reserve(1).map_err(|_| Error::NoMemory)?;
                    next.push(sub);
                }
            }
        }
        pending = next;
        if pending.is_empty() {
            leaf_fix(ptr::FIX_RETRY, id, piece.va, piece.len, process);
            return Ok(false);
        }
    }
    for p in pending {
        leaf_fail(13, 1, last, p.va, p.len, process);
    }
    Err(last)
}

/// A leaf update whose PTEs place a device-local native allocation in SYSTEM
/// pages. VidMm keeps CPU-accessed allocations there (the UMD's Lock2 view is
/// those pages), so the GPU must see the same pages, not the allocation's RM
/// video memory: GPFIFO entries and pushbuffers the UMD wrote were otherwise
/// invisible to the GPU. Whatever the process mapped in the range is dropped,
/// the pages (`pfns`, one per 4-KiB PTE) become an RM OS-descriptor object and
/// that is mapped at the exact VA. VidMm's Transfer operations move contents
/// between these pages and the RM memory when it changes the placement.
pub fn update_leaf_system(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    if pfns.is_empty() {
        return Err(Error::Invalid);
    }
    let mut none = Vec::new();
    none.try_reserve_exact(pfns.len()).map_err(|_| Error::NoMemory)?;
    none.resize(pfns.len(), None);
    // NvSysWhy: 1 dropping the old mappings (an error name in NvSysErr), 2 the
    // OS descriptor, 3 mapping it.
    let why = |code: u32, error: Error| {
        crate::diag::record_named_bytes(b"NvSysWhy", code);
        crate::diag::record_named_bytes(b"NvSysErr", error as u32);
        error
    };
    let start = crate::adapter::foreign_scanout::now_100ns();
    let trips = round_trips();
    // The phases (`PgPh*`): the clock starts with NvPmSys's.
    let mut watch = Watch::start(&PH_SYSTEM, Phase::Skip);
    let chunk_pages = system_chunk_pages();
    let perf = perf();
    // NvPerfMem 0x40: VidMm rewrites a range with the pages that are mapped there
    // already (a re-residency, a page-table rebuild): nothing to unmap or map.
    if perf.skip_unchanged()
        && system_mapping_unchanged(id, process, first_va, allocation_offset, pfns, chunk_pages)
    {
        bump(&SKIPPED_UNCHANGED, b"NvPmSkip");
        return Ok(());
    }
    ph(&mut watch, Phase::Other);
    let result = update_leaf_system_inner(
        passive,
        adapter,
        id,
        process,
        first_va,
        allocation_offset,
        pfns,
        &none,
        chunk_pages,
        perf.tlb_defer(),
        &why,
        &mut watch,
    );
    SYSTEM_COST.note(start, trips);
    if let Some(watch) = watch {
        watch.done();
    }
    if verify_on() {
        let expect: Vec<lp::Expect> = pfns.iter().map(|p| lp::Expect::Page(*p)).collect();
        verify_leaf(id, process, first_va, &expect);
    }
    result
}

/// Whether the allocation's mappings in `process` over the update's range are
/// exactly the system-page chunks the update would make (`native_perf`).
fn system_mapping_unchanged(
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
    chunk_pages: usize,
) -> bool {
    let state = st();
    let Ok(allocation) = state.allocation(id) else {
        return false;
    };
    if allocation.poisoned {
        return false;
    }
    let mut live = Vec::new();
    if live.try_reserve(allocation.mappings.len()).is_err() {
        return false;
    }
    for m in allocation.mappings.iter().filter(|m| m.process == process) {
        let shown = if m.desc != 0 {
            match state.descs.pfns(m.desc) {
                Some(pages) => Some(pages),
                // A descriptor whose pages were not kept: cannot tell.
                None => return false,
            }
        } else {
            None
        };
        live.push(np::LiveMapping { piece: m.piece, pfns: shown });
    }
    np::system_update_unchanged(&live, first_va, allocation_offset, pfns, chunk_pages)
}

#[allow(clippy::too_many_arguments)]
fn update_leaf_system_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pfns: &[u64],
    none: &[Option<nm::MappingPiece>],
    chunk_pages: usize,
    defer_on: bool,
    why: &dyn Fn(u32, Error) -> Error,
    watch: &mut Option<Watch>,
) -> Result<(), Error> {
    // `NvPaging` 0x4: one hold of the mutex for both halves (the KMUTEX is
    // recursive: the halves' own acquisitions return at once). Without it the
    // mutex is let go between them, and whatever queued for it meanwhile runs
    // before the map half.
    let _whole = if paging().one_lock() {
        Some(lock(passive, Kind::System).map_err(|error| why(1, error))?)
    } else {
        None
    };
    ph(watch, Phase::Other);
    update_leaf_staged(passive, adapter, id, process, first_va, allocation_offset, none, Kind::System, watch)
        .map_err(|error| why(1, error))?;

    ph(watch, Phase::Other);
    let _guard = lock(passive, Kind::System)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    ph(watch, Phase::Space);
    let space = ensure_space(&io, process)?;
    ph(watch, Phase::Other);
    // One OS descriptor per chunk (`NvSysChunk`, default 128 pages): RM refused
    // large ones (512 and 335 pages) with NV_ERR_INVALID_ADDRESS after pinning
    // them (bpftrace: every pin succeeded), and the range stayed unmapped (ring
    // fetch faults). A refused chunk still fails the update; the chunks mapped
    // before it stay. With NvPerfMem 0x20 every chunk's mapping but the last
    // defers its TLB invalidate. Pages that already have a descriptor reuse it,
    // and with `NvPaging` 0x1 every chunk's calls go in one round trip.
    let chunks = np::system_chunks(first_va, allocation_offset, pfns.len(), chunk_pages);
    let mut jobs = Vec::new();
    jobs.try_reserve_exact(chunks.len()).map_err(|_| Error::NoMemory)?;
    for (index, chunk) in chunks.iter().enumerate() {
        jobs.push(DescJob {
            va: chunk.va,
            pfns: pfns
                .get(chunk.first..chunk.first + chunk.pages)
                .ok_or(Error::Invalid)?,
            defer: np::defer_tlb(defer_on, index, chunks.len()),
        });
    }
    let mut made = Vec::new();
    ph(watch, Phase::Build);
    map_descs(&io, id, space, &jobs, &mut made)?;
    // Every mapping made: (piece, virtual object, descriptor). A refused chunk is
    // tried again in smaller descriptors (16 pages, then single pages, at most
    // 128 of those): RM refused large ones before, and one page it will not pin
    // should not cost the chunk's other pages. What is still refused stays
    // unmapped and is counted (`NvLfFail`).
    let mut ok: Vec<(nm::MappingPiece, u32, u32)> = Vec::new();
    ok.try_reserve_exact(chunks.len()).map_err(|_| Error::NoMemory)?;
    let mut refused: Vec<(usize, usize, Error)> = Vec::new();
    let mut pending = false;
    let piece_of = |first: usize, pages: usize| nm::MappingPiece {
        va: first_va + first as u64 * nm::PAGE_BYTES,
        memory_offset: allocation_offset + first as u64 * nm::PAGE_BYTES,
        len: pages as u64 * nm::PAGE_BYTES,
    };
    for ((chunk, job), result) in chunks.iter().zip(&jobs).zip(made) {
        match result {
            Ok((virt, desc)) => {
                pending = job.defer;
                watch::event(chunk.va, chunk.bytes(), 7, 0, process);
                ok.push((piece_of(chunk.first, chunk.pages), virt, desc));
            }
            Err((stage, error)) => {
                let error = why(stage, error);
                note_failed_map(chunk.va, 2, error);
                watch::event(chunk.va, chunk.bytes(), 8, error as u32, process);
                refused.try_reserve(1).map_err(|_| Error::NoMemory)?;
                refused.push((chunk.first, chunk.pages, error));
            }
        }
    }
    for sub_pages in [16usize, 1] {
        if refused.is_empty() {
            break;
        }
        let total: usize = refused.iter().map(|r| r.1).sum();
        if sub_pages == 1 && total > 128 {
            break;
        }
        let mut next = Vec::new();
        for (first, pages, error) in core::mem::take(&mut refused) {
            if pages <= sub_pages {
                next.try_reserve(1).map_err(|_| Error::NoMemory)?;
                next.push((first, pages, error));
                continue;
            }
            let subs = np::system_chunks(0, 0, pages, sub_pages);
            let mut sub_jobs = Vec::new();
            sub_jobs.try_reserve_exact(subs.len()).map_err(|_| Error::NoMemory)?;
            for sub in subs.iter() {
                let at = first + sub.first;
                sub_jobs.push(DescJob {
                    va: first_va + at as u64 * nm::PAGE_BYTES,
                    pfns: pfns.get(at..at + sub.pages).ok_or(Error::Invalid)?,
                    defer: false,
                });
            }
            let mut sub_made = Vec::new();
            map_descs(&io, id, space, &sub_jobs, &mut sub_made)?;
            for (sub, result) in subs.iter().zip(sub_made) {
                let at = first + sub.first;
                match result {
                    Ok((virt, desc)) => {
                        pending = false;
                        ok.try_reserve(1).map_err(|_| Error::NoMemory)?;
                        ok.push((piece_of(at, sub.pages), virt, desc));
                        let p = piece_of(at, sub.pages);
                        watch::event(p.va, p.len, 7, 0, process);
                        leaf_fix(ptr::FIX_RETRY, id, p.va, p.len, process);
                    }
                    Err((stage, error)) => {
                        next.try_reserve(1).map_err(|_| Error::NoMemory)?;
                        next.push((at, sub.pages, why(stage, error)));
                    }
                }
            }
        }
        refused = next;
    }
    let mut first_error = None;
    for (first, pages, error) in refused {
        let p = piece_of(first, pages);
        leaf_fail(14, 2, error, p.va, p.len, process);
        first_error.get_or_insert(error);
    }
    ph(watch, Phase::Book);
    // Every made mapping is recorded under ONE STATE hold and the list sorted
    // once. One that cannot be recorded (no memory, the allocation gone) is
    // taken down again.
    let recorded_all = !ok.is_empty() && {
        let mut state = st();
        let room = state
            .allocation_mut(id)
            .is_ok_and(|allocation| allocation.mappings.try_reserve(ok.len()).is_ok());
        if room {
            if let Ok(allocation) = state.allocation_mut(id) {
                for (piece, virt, desc) in ok.iter().copied() {
                    allocation.mappings.push(HostMapping { process, piece, virt, desc });
                }
                allocation
                    .mappings
                    .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
            }
            state.note_mapped(process, id);
        }
        room
    };
    if !ok.is_empty() && !recorded_all {
        for (piece, virt, desc) in ok.iter().copied() {
            let _ = unmap_fixed(&io, 0, HostMapping { process, piece, virt, desc });
            leaf_fail(14, 2, Error::NoMemory, piece.va, piece.len, process);
        }
        first_error.get_or_insert(Error::NoMemory);
    }
    if pending {
        // The last chunk would have invalidated for the deferred ones.
        invalidate_tlb(&io, space);
    }
    first_error.map_or(Ok(()), Err)
}

/// A leaf update of an allocation this driver has no RM memory for (dxgkrnl's
/// monitored-fence and progress-fence storage, any non-native allocation) in
/// a process with a native VA space. The GPU signals queue progress and
/// monitored fences by writing those VAs (0x10140...), so they must be mapped
/// in RM's space too: system pages (`pfns`, one per PTE) become an OS
/// descriptor mapped at the exact VA; `pfns` empty only drops what the
/// process mapped in [first_va, first_va + pages * 4 KiB).
pub fn update_foreign_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    first_va: u64,
    pages: u64,
    pfns: &[u64],
) -> Result<(), Error> {
    // dxgkrnl maps a process's monitored-fence and progress-fence pages before
    // the process makes any native allocation, often before anything brought
    // the KMD's RM client up: bring it up here, or those pages were never
    // mirrored (NvFgnFail with NotReady) and the GPU's fence writes faulted
    // (Xid 31, CE FAULT_PTE writes at the fence VAs).
    let up = st().client.is_some();
    if !up && !pfns.is_empty() {
        super::nvda::memory_client(passive, adapter, process)?;
    }
    let _guard = lock(passive, Kind::Foreign)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let len = pages.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    // `NvHotPath` 0x2: VidMm writes the pages that are mapped there already (a
    // re-residency, a page-table rebuild): nothing to unmap or map (`HpFgnSkip`).
    // A live foreign mapping means nothing else was mapped over it since: every
    // native update evicts the foreign mappings in its range.
    if crate::ddi::hot_path::knob(passive).skip_foreign_unchanged()
        && foreign_mapping_unchanged(process, first_va, len, pfns)
    {
        crate::ddi::hot_path::count(&crate::ddi::hot_path::FOREIGN_SKIPPED);
        watch::event(first_va, len, 2, 0x80, process);
        return Ok(());
    }
    let stale: Vec<HostMapping> = {
        let hit = |m: &HostMapping| {
            m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
        };
        let mut state = st();
        let mut stale = Vec::new();
        // Only the overlapping ones are copied out; the list is not rebuilt.
        let n = state.foreign.iter().filter(|m| hit(m)).count();
        if n != 0 {
            stale.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
            stale.extend(state.foreign.iter().copied().filter(|m| hit(m)));
            state.foreign.retain(|m| !hit(m));
        }
        stale
    };
    if !stale.is_empty() {
        let mut items = Vec::new();
        items.try_reserve_exact(stale.len()).map_err(|_| Error::NoMemory)?;
        items.extend(stale.iter().map(|m| (0u32, *m, false)));
        let mut results = Vec::new();
        unmap_many(&io, &items, false, &mut results)?;
    }
    if pfns.is_empty() {
        watch::event(first_va, len, 4, 0, process);
        return Ok(());
    }
    let space = ensure_space(&io, process)?;
    // Native allocations VidMm no longer shows here (see `update_leaf`).
    evict_range(&io, space, process, first_va, len, Evict::All, false)
        .inspect_err(|e| note_failed_map(first_va, 3, *e))?;
    let piece = nm::MappingPiece {
        va: first_va,
        memory_offset: 0,
        len,
    };
    // The descriptor (reused for the same pages), its virtual object and the map:
    // one round trip under `NvPaging` 0x1.
    let job = DescJob {
        va: first_va,
        pfns,
        defer: false,
    };
    let mut made = Vec::new();
    map_descs(&io, 0, space, core::slice::from_ref(&job), &mut made)?;
    let (used, desc) = match made.pop().unwrap_or(Err((3, Error::Host))) {
        Ok(made) => made,
        Err((stage, error)) => {
            note_failed_map(first_va, if stage == 2 { 4 } else { 5 }, error);
            if stage != 2 {
                watch::event(first_va, len, 3, error as u32, process);
            }
            return Err(error);
        }
    };
    watch::event(first_va, len, 2, 0, process);
    let mapping = HostMapping {
        process,
        piece,
        virt: used,
        desc,
    };
    let mut state = st();
    if state.foreign.try_reserve(1).is_err() {
        drop(state);
        let _ = unmap_fixed(&io, 0, mapping);
        return Err(Error::NoMemory);
    }
    state.foreign.push(mapping);
    Ok(())
}

/// Whether `process`'s foreign mappings over `[first_va, first_va + len)` are exactly
/// one mapping of exactly that range whose descriptor holds exactly `pfns`
/// (`helios_kmd_logic::hot_path::foreign_update_unchanged`).
fn foreign_mapping_unchanged(process: usize, first_va: u64, len: u64, pfns: &[u64]) -> bool {
    let state = st();
    let mut overlapping: [(nm::MappingPiece, Option<&[u64]>); 2] = [(
        nm::MappingPiece {
            va: 0,
            memory_offset: 0,
            len: 0,
        },
        None,
    ); 2];
    let mut n = 0usize;
    for m in state
        .foreign
        .iter()
        .filter(|m| m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
    {
        if n == overlapping.len() {
            return false;
        }
        let pages = if m.desc != 0 { state.descs.pfns(m.desc) } else { None };
        overlapping[n] = (m.piece, pages);
        n += 1;
    }
    helios_kmd_logic::hot_path::foreign_update_unchanged(&overlapping[..n], first_va, len, pfns)
}

/// CPU access to `len` bytes at GPU VA `va` of `process` exactly as the GPU
/// sees them: VidMm's system pages for a system-placed native allocation, the
/// RM memory otherwise. `visit` gets a temporary pointer; the range must not
/// cross a 4-KiB page. IRQL <= DISPATCH; the RM-memory case needs `passive`
/// (`Err(Busy)` without). `Ok(false)` when nothing native maps `va`.
pub fn patch_gpu_bytes(
    passive: Option<PassiveLevel>,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    len: u64,
    visit: impl FnOnce(*mut u8),
) -> Result<bool, Error> {
    if len == 0 || (va & 0xfff) + len > 0x1000 {
        return Err(Error::Invalid);
    }
    enum Where {
        System(u64),
        Rm(u32, u64),
    }
    let found = {
        let state = st();
        let look = |allocation: &Allocation| -> Option<Where> {
            let m = allocation.mappings.iter().find(|m| {
                m.process == process
                    && va >= m.piece.va
                    && m.piece.end().is_some_and(|end| va + len <= end)
            })?;
            let delta = va - m.piece.va;
            if m.desc != 0 {
                let pfn = state
                    .descs
                    .pfns(m.desc)
                    .and_then(|pfns| pfns.get((delta >> 12) as usize).copied())?;
                Some(Where::System((pfn << 12) | (va & 0xfff)))
            } else {
                Some(Where::Rm(allocation.id, m.piece.memory_offset + delta))
            }
        };
        // This process's allocations only (`by_process`); no allocation here (any
        // IRQL up to DISPATCH).
        IDX_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        if state.index_broken {
            IDX_SCANNED.fetch_add(state.allocations.len() as u32, Ordering::Relaxed);
            state.allocations.iter().find_map(look)
        } else {
            let ids = state
                .by_process
                .iter()
                .find(|(p, _)| *p == process)
                .map_or(&[][..], |(_, ids)| ids.as_slice());
            IDX_SCANNED.fetch_add(ids.len() as u32, Ordering::Relaxed);
            ids.iter()
                .filter_map(|id| state.index_of(*id).map(|i| &state.allocations[i]))
                .find_map(look)
        }
    };
    match found {
        None => Ok(false),
        Some(Where::System(physical)) => {
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = (physical & !0xfff) as i64;
            // SAFETY: a page VidMm keeps locked while it is mapped for the GPU;
            // MmCached matches system RAM, and the view is released below.
            let view = unsafe { MmMapIoSpace(pa, 0x1000, _MEMORY_CACHING_TYPE::MmCached) } as *mut u8;
            if view.is_null() {
                return Err(Error::NoMemory);
            }
            visit(unsafe { view.add((physical & 0xfff) as usize) });
            unsafe { MmUnmapIoSpace(view.cast(), 0x1000) };
            Ok(true)
        }
        Some(Where::Rm(id, offset)) => {
            // The RM-memory view needs PASSIVE; system pages work up to DISPATCH.
            let Some(passive) = passive else {
                return Err(Error::Busy);
            };
            let mut visit = Some(visit);
            // SAFETY: PASSIVE; the closure only writes inside the given range.
            unsafe {
                with_cpu_range(passive, adapter, id, offset, len, |_, native, got| {
                    if got == len {
                        if let Some(visit) = visit.take() {
                            visit(native);
                        }
                    }
                    true
                })
            }
        }
    }
}

/// `NvLfPad`: leaf-update pages past the end of the RM memory (VidMm's padding);
/// `NvLfPadOff` / `NvLfPadSz` the last one's offset and the memory size (4-KiB
/// pages). Mapped to the zero page under `NvLfPadMap` (default on), else left
/// unmapped.
fn note_padding(offset: u64, size: u64) {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % 256 == 0 {
        crate::diag::record_named_bytes(b"NvLfPad", n);
        crate::diag::record_named_bytes(b"NvLfPadOff", (offset >> 12) as u32);
        crate::diag::record_named_bytes(b"NvLfPadSz", (size >> 12) as u32);
    }
}

/// The leaf-update failure values. Registry values outlive a boot, so a counter
/// that did not move this boot would still show an old boot's number (`NvLf5`
/// = 0x16, `NvLfb` = 7, `NvLfd` = 2 stood unchanged from 405.273 to 405.290):
/// they are zeroed once, at the first native leaf update of the boot.
const LEAF_COUNTERS: [&[u8]; 34] = [
    b"NvLf0", b"NvLf1", b"NvLf2", b"NvLf3", b"NvLf4", b"NvLf5", b"NvLf6", b"NvLf7",
    b"NvLf8", b"NvLf9", b"NvLfa", b"NvLfb", b"NvLfc", b"NvLfd", b"NvLfe", b"NvLff",
    b"NvLfErr", b"NvLfVa", b"NvLfOff", b"NvLfFail", b"NvLfFailVa", b"NvLfFailWhy",
    b"NvLfFixN", b"NvLfFixWhy", b"NvLfMismatch", b"NvLfMisVa", b"NvLfRej",
    b"NvPgLeafWhy", b"NvPgFail", b"NvPgFailN", b"NvSysWhy", b"NvSysErr", b"NvEvictFail",
    b"NvLfPad",
];

/// Zero [`LEAF_COUNTERS`] once per boot (PASSIVE; the first native leaf update).
pub fn reset_leaf_counters_once(_passive: PassiveLevel) {
    static DONE: AtomicU32 = AtomicU32::new(0);
    if DONE.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    for name in LEAF_COUNTERS {
        crate::diag::record_named_force(name, 0);
    }
}

static LEAF_FAILS: AtomicU32 = AtomicU32::new(0);
static LEAF_FIXES: AtomicU32 = AtomicU32::new(0);
static LEAF_MISMATCH: AtomicU32 = AtomicU32::new(0);

/// Part of a leaf update (or a mapping an eviction had to show again) is NOT
/// what VidMm's page table says: `[va, va + len)` stays unmapped or stale.
/// `NvLfFail` counts them (0 on a clean run), `NvLfFailVa` the last VA (4-KiB
/// page), `NvLfFailWhy` = stage << 16 | site << 8 | Error (site: 1 video-memory
/// map, 2 system-page descriptor, 4 a PTE naming nothing mappable, 6 padding to
/// the zero page, 7 an evicted mapping's outside part); ring `K_LEAF_FAIL`.
fn leaf_fail(stage: u32, site: u32, error: Error, va: u64, len: u64, process: usize) {
    let n = LEAF_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvLfFail", n);
    crate::diag::record_named_bytes(b"NvLfFailVa", (va >> 12) as u32);
    crate::diag::record_named_bytes(b"NvLfFailWhy", (stage << 16) | (site << 8) | error as u32);
    crate::ddi::pt_ring::record(ptr::K_LEAF_FAIL, 0, stage as u16, (site << 8) | error as u32, va, len, process);
}

/// The driver's record of a range was repaired (or a refused map made in a
/// smaller shape) and the update went on: `NvLfFixN`, `NvLfFixWhy` = the last
/// `pt_ring::FIX_*`; ring `K_LEAF_FIX`.
fn leaf_fix(what: u16, id: u32, va: u64, len: u64, process: usize) {
    let n = LEAF_FIXES.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvLfFixN", n);
    crate::diag::record_named_bytes(b"NvLfFixWhy", u32::from(what));
    crate::ddi::pt_ring::record(ptr::K_LEAF_FIX, 0, what, id, va, len, process);
}

/// `NvLfPadMap` (REG_DWORD, default 1): VidMm's padding pages (valid PTEs past
/// the end of the RM memory) map to one zero page (`leaf_plan::pad_map_on`).
fn pad_map() -> bool {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvLfPadMap"), 1);
        if value == u32::MAX {
            value = 1;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    lp::pad_map_on(value)
}

/// `NvLfVerify` (REG_DWORD, default 0): after every native leaf update, compare
/// the driver's record of the range (what it had RM map) with VidMm's PTEs
/// page by page (`leaf_plan::mismatches`); `NvLfMismatch` counts the updates
/// that differ, `NvLfMisVa` the last one's VA (4-KiB page). RM's own PTEs hold
/// host physical addresses and cannot be compared with guest pages directly.
fn verify_on() -> bool {
    static VALUE: AtomicU32 = AtomicU32::new(u32::MAX);
    let mut value = VALUE.load(Ordering::Relaxed);
    if value == u32::MAX {
        value = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvLfVerify"), 0);
        if value == u32::MAX {
            value = 0;
        }
        VALUE.store(value, Ordering::Relaxed);
    }
    value != 0
}

/// [`verify_on`]'s check of allocation `id` in `process` against `expect`.
fn verify_leaf(id: u32, process: usize, first_va: u64, expect: &[lp::Expect]) {
    let bad = {
        let state = st();
        let Ok(allocation) = state.allocation(id) else { return };
        let len = expect.len() as u64 * nm::PAGE_BYTES;
        let mut recorded = Vec::new();
        for m in allocation
            .mappings
            .iter()
            .filter(|m| m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len))
        {
            let shown = if m.desc == 0 {
                lp::Shown::Memory
            } else {
                state.descs.pfns(m.desc).map_or(lp::Shown::Unknown, lp::Shown::Pages)
            };
            if recorded.try_reserve(1).is_err() {
                return;
            }
            recorded.push((m.piece, shown));
        }
        lp::mismatches(&recorded, first_va, expect)
    };
    if bad != 0 {
        let n = LEAF_MISMATCH.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvLfMismatch", n);
        crate::diag::record_named_bytes(b"NvLfMisVa", (first_va >> 12) as u32);
        crate::ddi::pt_ring::record(ptr::K_LEAF_FAIL, 1, 0xee, bad, first_va, expect.len() as u64 * nm::PAGE_BYTES, process);
    }
}

/// One zeroed, never-freed system page for padding mappings (`NvLfPadMap`):
/// its page frame number, `None` when it cannot be had. PASSIVE.
fn zero_pfn(_passive: PassiveLevel) -> Option<u64> {
    static PFN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let pfn = PFN.load(Ordering::Relaxed);
    if pfn == u64::MAX {
        return None;
    }
    if pfn != 0 {
        return Some(pfn);
    }
    let mut highest: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    highest.QuadPart = i64::MAX;
    // SAFETY: PASSIVE (the token); one page of non-paged, physically contiguous
    // memory, deliberately never freed (padding mappings may outlive anything).
    let va = unsafe { wdk_sys::ntddk::MmAllocateContiguousMemory(nm::PAGE_BYTES, highest) };
    if va.is_null() {
        PFN.store(u64::MAX, Ordering::Relaxed);
        return None;
    }
    // SAFETY: `va` owns the page just allocated.
    unsafe { core::ptr::write_bytes(va.cast::<u8>(), 0, nm::PAGE_BYTES as usize) };
    // SAFETY: a valid non-paged kernel address.
    let physical = unsafe { wdk_sys::ntddk::MmGetPhysicalAddress(va).QuadPart } as u64;
    let pfn = physical >> 12;
    if pfn == 0 {
        PFN.store(u64::MAX, Ordering::Relaxed);
        return None;
    }
    PFN.store(pfn, Ordering::Relaxed);
    Some(pfn)
}

/// Padding pages mapped to the zero page at most per run (one host mmap each).
const PAD_PAGES_MAX: u64 = 512;

/// Map VidMm's padding `pad` (pages past the RM memory's end) of allocation `id`
/// to the zero page: an OS descriptor that lists the page once per 4 KiB. The
/// mapping is recorded like a system-page one (its descriptor owned by `id`),
/// so every later update, eviction and teardown handles it as such.
#[allow(clippy::too_many_arguments)]
fn map_padding(
    io: &Io<'_>,
    id: u32,
    space: u32,
    process: usize,
    pad: nm::MappingPiece,
    out: &mut Vec<HostMapping>,
) -> Result<(), Error> {
    let pages = pad.len / nm::PAGE_BYTES;
    if pages > PAD_PAGES_MAX {
        return Err(Error::Unsupported);
    }
    let pfn = zero_pfn(io.passive).ok_or(Error::NoMemory)?;
    let mut pfns = Vec::new();
    pfns.try_reserve_exact(pages as usize).map_err(|_| Error::NoMemory)?;
    pfns.resize(pages as usize, pfn);
    out.try_reserve(1).map_err(|_| Error::NoMemory)?;
    let job = DescJob { va: pad.va, pfns: &pfns, defer: false };
    let (virt, desc) = map_desc_single(io, id, space, &job).map_err(|(_, e)| e)?;
    watch::event(pad.va, pad.len, 7, 0x9a, process);
    out.push(HostMapping { process, piece: pad, virt, desc });
    Ok(())
}

/// Apply one native leaf update of allocation `id`'s RM memory: after it the
/// process's GPU VA range `[first_va, first_va + pages)` shows exactly what
/// VidMm's PTEs say (`pieces[i]` = page `i`, `None` = invalid PTE: unmapped).
/// Non-leaf updates deliberately have no host operation.
///
/// The page table is the truth; the driver's record of what it mapped before
/// is repaired, never trusted to veto the update (`helios_kmd_logic::leaf_plan`
/// has the failure shapes that did). Whatever another allocation or a foreign
/// range still holds there goes, then this allocation's own old mappings in
/// the range (their outside parts shown again as what they were), then the new
/// pieces are mapped; a refused map is tried again in smaller shapes. Only a
/// part RM refuses in every shape is left unmapped, and counted loudly
/// (`NvLfFail`, ring `K_LEAF_FAIL`); `Err` then.
///
/// Counted per stage on an error (`NvLf<stage>`, `NvLfErr`, `NvLfVa`, `NvLfOff`),
/// and a watch / ring event op 11 with detail = the stage (0xff = no change
/// needed, 0xfe = mapped).
pub fn update_leaf(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
) -> Result<(), Error> {
    let start = crate::adapter::foreign_scanout::now_100ns();
    let trips = round_trips();
    // The phases (`PgPv*`).
    let mut watch = Watch::start(&PH_LEAF, Phase::Other);
    let result = update_leaf_staged(
        passive,
        adapter,
        id,
        process,
        first_va,
        allocation_offset,
        pieces,
        Kind::Leaf,
        &mut watch,
    );
    LEAF_COST.note(start, trips);
    if let Some(watch) = watch {
        watch.done();
    }
    if verify_on() {
        let size = st().allocation(id).map(|a| a.size).unwrap_or(0);
        let expect: Vec<lp::Expect> = pieces
            .iter()
            .map(|p| match p {
                None => lp::Expect::Unmapped,
                Some(p) => {
                    if p.memory_end().is_none_or(|e| e > size) {
                        lp::Expect::Any
                    } else {
                        lp::Expect::Memory(p.memory_offset)
                    }
                }
            })
            .collect();
        verify_leaf(id, process, first_va, &expect);
    }
    result
}

/// [`update_leaf`] without the cost counters (`update_leaf_system` counts its own);
/// the mutex is taken as `kind`, the phases go to `watch`.
#[allow(clippy::too_many_arguments)]
fn update_leaf_staged(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
    kind: Kind,
    watch: &mut Option<Watch>,
) -> Result<(), Error> {
    reset_leaf_counters_once(passive);
    // Per call, not a global: a stage another thread wrote was reported before.
    let mut stage = 1u32;
    let result = update_leaf_inner(
        passive,
        adapter,
        id,
        process,
        first_va,
        allocation_offset,
        pieces,
        kind,
        watch,
        &mut stage,
    );
    let len = pieces.len() as u64 * nm::PAGE_BYTES;
    watch::event(first_va, len, 11, stage, process);
    if let Err(error) = result {
        static COUNTS: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
        let k = (stage as usize).min(15);
        let n = COUNTS[k].fetch_add(1, Ordering::Relaxed) + 1;
        let digit = if k < 10 { b'0' + k as u8 } else { b'a' + k as u8 - 10 };
        crate::diag::record_named_bytes(&[b'N', b'v', b'L', b'f', digit], n);
        crate::diag::record_named_bytes(b"NvLfErr", (stage << 8) | error as u32);
        crate::diag::record_named_bytes(b"NvLfVa", (first_va >> 12) as u32);
        crate::diag::record_named_bytes(b"NvLfOff", (allocation_offset >> 12) as u32);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn update_leaf_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    process: usize,
    first_va: u64,
    allocation_offset: u64,
    pieces: &[Option<nm::MappingPiece>],
    kind: Kind,
    watch: &mut Option<Watch>,
    stage: &mut u32,
) -> Result<(), Error> {
    let _guard = lock(passive, kind)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    ph(watch, Phase::Other);
    *stage = 2;
    let count = pieces.len() as u64;
    let len = count.checked_mul(nm::PAGE_BYTES).ok_or(Error::Invalid)?;
    // Only this process's mappings of the allocation that overlap the update are
    // copied out; the allocation's whole list is not cloned.
    let mut recorded: Vec<lp::Recorded> = Vec::new();
    let (memory, size, system_backed) = {
        let state = st();
        let allocation = state.allocation(id)?;
        // Another client's memory: nothing of it can be mapped in this one.
        if allocation.epoch != client.epoch {
            return Err(Error::Poisoned);
        }
        let hit = |m: &&HostMapping| {
            m.process == process && nm::ranges_overlap(m.piece.va, m.piece.len, first_va, len)
        };
        let n = allocation.mappings.iter().filter(hit).count();
        recorded.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        recorded.extend(
            allocation
                .mappings
                .iter()
                .filter(hit)
                .map(|m| lp::Recorded { piece: m.piece, system: m.desc != 0 }),
        );
        (allocation.memory, allocation.size, allocation.system_memory != 0)
    };
    *stage = 4;
    let plan = lp::plan_leaf(first_va, allocation_offset, size, pieces).map_err(|_| Error::Invalid)?;
    if !plan.map.is_empty() || !plan.pad.is_empty() {
        // Placed in video memory: VidMm's system pages for it are its to free, so
        // its idle OS descriptors go (`helios_kmd_logic::desc_cache`).
        ph(watch, Phase::Unmap);
        drop_idle_descs_of(&io, id);
    }
    *stage = 3;
    ph(watch, Phase::Space);
    let space = ensure_space(&io, process)?;
    ph(watch, Phase::Other);

    // GPU VAs are per process: only this process's other mappings can collide.
    // The page table VidMm writes is the truth: whatever another allocation or
    // a foreign range still holds at these VAs is gone from VidMm's view, so it
    // goes from RM's too (RM refused the new fixed mapping over the stale one:
    // NV_ERR_INVALID_ARGUMENT, and the GPU faulted on the unmapped range).
    *stage = 8;
    ph(watch, Phase::Evict);
    evict_range(&io, space, process, first_va, len, Evict::Others(id), false)?;
    ph(watch, Phase::Other);
    *stage = 9;
    if lp::unchanged(&recorded, first_va, len, &plan) {
        *stage = 0xff;
        return Ok(());
    }
    *stage = 10;

    // Replacing a live mapping: VidMm only changes an allocation's placement once
    // no GPU work that uses it is outstanding (it waits for the paging and
    // context fences first), so RM's idle-channels quiesce is only a belt on
    // top. It is best effort: its refusal (Busy) had VidMm's every move between
    // system pages and video memory skipped (NvPgLeafWhy 0x311).
    // `NvHotPath` 0x4 skips it (`HpQuiSkip`); otherwise it is timed (`HpQuiN`, `HpQuiUs`,
    // `HpQuiMaxUs`): one host round trip that waits for the client's channels.
    if !recorded.is_empty() {
        ph(watch, Phase::Quiesce);
        if crate::ddi::hot_path::knob(passive).no_quiesce() {
            crate::ddi::hot_path::count(&crate::ddi::hot_path::QUIESCE_SKIPPED);
            crate::ddi::pt_ring::record(ptr::K_QUIESCE, 0, 2, id, first_va, len, process);
        } else {
            let started = crate::adapter::foreign_scanout::now_100ns();
            let quiesced = super::native_channel::quiesce(passive, adapter, process);
            crate::ddi::hot_path::note_quiesce(
                crate::adapter::foreign_scanout::now_100ns().saturating_sub(started),
            );
            crate::ddi::pt_ring::record(ptr::K_QUIESCE, 0, u16::from(quiesced.is_err()), id, first_va, len, process);
            if quiesced.is_err() {
                // `NvQuiesceRef`: published by the mirror pass (`publish_hot`).
                QUIESCE_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // This allocation's old mappings in the range go; their parts outside it are
    // shown again as what they were (RM memory at the same offset, or the same
    // system pages). NvPerfMem 0x20: the unmaps and every map but the last defer
    // their TLB invalidate; one owed at the end is made explicitly.
    *stage = 11;
    let pad_on = pad_map();
    let operations = plan.map.len() + if pad_on { plan.pad.len() } else { 0 };
    let defer_on = perf().tlb_defer();
    ph(watch, Phase::Unmap);
    let mut pending = evict_range(&io, space, process, first_va, len, Evict::Only(id), defer_on && operations != 0)?;
    ph(watch, Phase::Build);

    *stage = 13;
    let mut made: Vec<HostMapping> = Vec::new();
    let mut first_error = None;
    for (index, piece) in plan.map.iter().copied().enumerate() {
        let defer = np::defer_tlb(defer_on, index, operations);
        let before = made.len();
        let result = map_retry(&io, memory, size, system_backed, space, piece, defer, process, id, &mut made);
        if made.len() != before {
            pending = matches!(result, Ok(true));
        }
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    for pad in plan.pad.iter().copied() {
        note_padding(pad.memory_offset, size);
        if !pad_on {
            continue;
        }
        match map_padding(&io, id, space, process, pad, &mut made) {
            Ok(()) => pending = false,
            Err(error) => {
                watch::event(pad.va, pad.len, 8, error as u32, process);
                leaf_fail(13, 6, error, pad.va, pad.len, process);
            }
        }
    }
    if plan.rejected != 0 {
        crate::diag::record_named_bytes(b"NvLfRej", plan.rejected);
        leaf_fail(4, 4, Error::Invalid, first_va, len, process);
        first_error.get_or_insert(Error::Invalid);
    }
    if pending {
        // The last operation would have invalidated for the deferred ones.
        invalidate_tlb(&io, space);
    }

    ph(watch, Phase::Book);
    let orphaned = {
        let mut state = st();
        let room = state
            .allocation_mut(id)
            .is_ok_and(|allocation| allocation.mappings.try_reserve(made.len()).is_ok());
        if room {
            let any = !made.is_empty();
            if let Ok(allocation) = state.allocation_mut(id) {
                allocation.mappings.extend(made.drain(..));
                allocation
                    .mappings
                    .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
            }
            if any {
                state.note_mapped(process, id);
            }
            Vec::new()
        } else {
            // Gone meanwhile (or no memory to record): what was made comes down.
            made
        }
    };
    if !orphaned.is_empty() {
        for mapping in orphaned.iter().copied() {
            let _ = unmap_fixed(&io, memory, mapping);
            leaf_fail(13, if mapping.desc != 0 { 6 } else { 1 }, Error::NoMemory, mapping.piece.va, mapping.piece.len, process);
        }
        first_error.get_or_insert(Error::NoMemory);
    }
    if first_error.is_none() {
        *stage = 0xfe;
    }
    first_error.map_or(Ok(()), Err)
}

/// Mark whether an address space has been quiesced for a live replacement.
/// The scheduler/root-page-table integration calls this; absence means busy.
pub fn set_address_space_idle(process: usize, idle: bool) {
    let mut state = st();
    if idle {
        if !state.idle_spaces.contains(&process) {
            let _ = state.idle_spaces.try_reserve_exact(1);
            if !state.idle_spaces.contains(&process) {
                state.idle_spaces.push(process);
            }
        }
    } else {
        state.idle_spaces.retain(|p| *p != process);
    }
}

pub fn notify_residency(id: u32, physical_address: u64, resident: bool) -> Result<(), Error> {
    let mut state = st();
    let allocation = state.allocation_mut(id)?;
    allocation.resident = resident.then_some((physical_address, true));
    Ok(())
}

fn release_cpu_chunk(io: &Io<'_>, memory: u32, chunk: &mut CpuChunk) -> Result<(), Error> {
    if chunk.map_ch == 0 {
        return Ok(());
    }
    if chunk.host_id != 0 {
        nvrm::release_host_map_within(
            io.passive,
            io.adapter,
            chunk.map_ch,
            chunk.host_id,
            TIMEOUT_MS,
        )
        .map_err(|_| Error::Host)?;
        chunk.host_id = 0;
    }
    if chunk.rm_mapped {
        let block = rc::nvos34(io.client.root, nd::H_DEVICE, memory, chunk.cookie);
        let mut response = [0u8; REPLY_MAX];
        let n = io.exchange(
            io.client.ctl,
            rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, rc::NVOS34_BYTES as u32),
            &block,
            &[],
            &mut response,
        )?;
        rc::rm_reply(response.get(..n).ok_or(Error::Host)?, rc::NVOS34_STATUS_AT)
            .map_err(|_| Error::Host)?;
        chunk.cookie = 0;
        chunk.rm_mapped = false;
    }
    io.close_file(chunk.map_ch)?;
    chunk.map_ch = 0;
    Ok(())
}

fn cpu_map_chunk(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    expected_gpa: Option<u64>,
) -> Result<CpuChunk, Error> {
    let region = nvrm::region_for(io.adapter, io.client.minor).ok_or(Error::Host)?;
    let expected = match expected_gpa {
        Some(base) => Some(base.checked_add(offset).ok_or(Error::WrongAddress)?),
        None => None,
    };
    let window_offset = match expected {
        Some(expected) => Some(
            expected
                .checked_sub(region.base)
                .ok_or(Error::WrongAddress)?,
        ),
        None => None,
    };
    let map_ch = io.open_file(io.client.minor)?;
    if let Err(error) = io.register_fd(map_ch) {
        let _ = io.close_file(map_ch);
        return Err(error);
    }
    let mut chunk = CpuChunk {
        physical: 0,
        map_ch,
        cookie: 0,
        rm_mapped: false,
        host_id: 0,
    };
    let block = rc::nvos33_with_fd(
        io.client.root,
        nd::H_DEVICE,
        allocation.memory,
        offset,
        len,
        map_ch,
    );
    let mut response = [0u8; REPLY_MAX];
    let cookie = match io.exchange(
        io.client.ctl,
        rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
        &block,
        &[],
        &mut response,
    ) {
        Ok(n) => (|| {
            let reply = response.get(..n).ok_or(Error::Host)?;
            let reply = rc::rm_reply(reply, rc::NVOS33_STATUS_AT).map_err(|_| Error::Host)?;
            rc::map_cookie(&reply).ok_or(Error::Host)
        })(),
        Err(error) => Err(error),
    };
    match cookie {
        Ok(cookie) => {
            chunk.cookie = cookie;
            chunk.rm_mapped = true;
        }
        Err(error) => {
            let _ = io.close_file(map_ch);
            return Err(error);
        }
    }
    let mapped = match window_offset {
        Some(window_offset) => nvrm::host_mmap_at_within(
            io.passive,
            io.adapter,
            KMD,
            map_ch,
            true,
            0,
            window_offset,
            len,
            TIMEOUT_MS,
        ),
        None => nvrm::host_mmap_within(
            io.passive, io.adapter, KMD, map_ch, true, 0, len, TIMEOUT_MS,
        ),
    };
    let mapped = match mapped {
        Ok(mapped) => {
            chunk.host_id = mapped.host_id;
            mapped
        }
        Err(_) => {
            let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
            return Err(Error::Host);
        }
    };
    if mapped.size < len {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::Host);
    }
    let physical = region.base.checked_add(mapped.offset);
    if physical.is_none_or(|physical| expected.is_some_and(|expected| physical != expected)) {
        let _ = release_cpu_chunk(io, allocation.memory, &mut chunk);
        return Err(Error::WrongAddress);
    }
    chunk.physical = physical.ok_or(Error::WrongAddress)?;
    Ok(chunk)
}

fn cpu_map(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    aperture_offset: Option<u64>,
    expected_gpa: Option<u64>,
) -> Result<CpuMapping, Error> {
    cpu_map_as(io, allocation, offset, len, aperture_offset, expected_gpa, _MEMORY_CACHING_TYPE::MmNonCached)
}

/// [`cpu_map`] whose guest view of RM memory has caching `cache`.
fn cpu_map_as(
    io: &Io<'_>,
    allocation: &Allocation,
    offset: u64,
    len: u64,
    aperture_offset: Option<u64>,
    expected_gpa: Option<u64>,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<CpuMapping, Error> {
    let end = offset.checked_add(len).ok_or(Error::Invalid)?;
    if len == 0
        || end > allocation.size
        || (allocation.system_memory == 0
            && (offset & (nm::PAGE_BYTES - 1) != 0 || len & (nm::PAGE_BYTES - 1) != 0))
        || aperture_offset.is_some() != expected_gpa.is_some()
        || (expected_gpa.is_none() && len > nvrm::MAX_MAP_BYTES)
    {
        return Err(Error::Invalid);
    }
    if allocation.system_memory != 0 {
        // Separate MDL mappings are not contiguous. Never expose the first
        // chunk (or a process-local user VA) as a view of the whole allocation.
        if allocation.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = allocation
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(CpuMapping {
            view: CpuView {
                address,
                len,
                generation: 0,
            },
            chunks: Vec::new(),
            kernel_va: 0,
            aperture_offset,
        });
    }

    let count =
        usize::try_from((len - 1) / nvrm::MAX_MAP_BYTES + 1).map_err(|_| Error::NoMemory)?;
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let mut chunk_offset = offset;
    while chunk_offset < end {
        let chunk_len = (end - chunk_offset).min(nvrm::MAX_MAP_BYTES);
        match cpu_map_chunk(io, allocation, chunk_offset, chunk_len, expected_gpa) {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => {
                for chunk in chunks.iter_mut() {
                    let _ = release_cpu_chunk(io, allocation.memory, chunk);
                }
                return Err(error);
            }
        }
        chunk_offset += chunk_len;
    }
    let physical = match expected_gpa {
        Some(expected) => expected,
        None => chunks
            .first()
            .map(|chunk| chunk.physical)
            .ok_or(Error::Host)?,
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = physical as i64;
    // A fixed aperture has contiguous placements by construction, so one
    // aggregate guest view is valid. Dynamic mappings are limited to one chunk.
    let kernel_va = unsafe { MmMapIoSpace(pa, len, cache) } as u64;
    let mut mapping = CpuMapping {
        view: CpuView {
            address: kernel_va,
            len,
            generation: chunks.first().map_or(0, |chunk| chunk.host_id),
        },
        chunks,
        kernel_va,
        aperture_offset,
    };
    if kernel_va == 0 {
        let _ = cpu_unmap_for(io, allocation.memory, &mut mapping);
        return Err(Error::NoMemory);
    }
    Ok(mapping)
}

fn cpu_unmap_for(io: &Io<'_>, memory: u32, mapping: &mut CpuMapping) -> Result<(), Error> {
    if mapping.kernel_va != 0 {
        unsafe { MmUnmapIoSpace(mapping.kernel_va as *mut c_void, mapping.view.len) };
        mapping.kernel_va = 0;
    }
    let mut ok = true;
    // A failed step leaves that chunk's fd and later steps intact for retry,
    // but does not prevent teardown of any other chunk.
    for chunk in mapping.chunks.iter_mut() {
        if release_cpu_chunk(io, memory, chunk).is_err() {
            ok = false;
        }
    }
    if ok {
        Ok(())
    } else {
        Err(Error::Host)
    }
}

/// Run a synchronous content operation over a range of the allocation's real
/// CPU bytes. Dynamic placements need not be contiguous, so temporary mappings
/// are made and released one page-rounded, bounded chunk at a time.
pub unsafe fn with_cpu_range(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    f: impl FnMut(u64, *mut u8, u64) -> bool,
) -> Result<bool, Error> {
    unsafe { with_cpu_range_as(passive, adapter, id, offset, length, false, f) }
}

/// [`with_cpu_range`] for BuildPagingBuffer's own operations: the copy through a temporary view
/// runs without the mutex unless `NvPaging` 0x20 (`Paging::paging_copy_unlocked`).
pub unsafe fn with_cpu_range_paging(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    f: impl FnMut(u64, *mut u8, u64) -> bool,
) -> Result<bool, Error> {
    unsafe { with_cpu_range_as(passive, adapter, id, offset, length, true, f) }
}

unsafe fn with_cpu_range_as(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    paging_op: bool,
    mut f: impl FnMut(u64, *mut u8, u64) -> bool,
) -> Result<bool, Error> {
    let mut guard = Some(lock(passive, Kind::CpuRange)?);
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (snapshot, existing, pending) = {
        let state = st();
        let a = state.allocation(id)?;
        let pending = a
            .cpu
            .as_ref()
            .is_some_and(|cpu| cpu.aperture_offset.is_none());
        let existing = a.cpu.as_ref().and_then(|cpu| {
            (cpu.aperture_offset.is_some() && (a.system_memory != 0 || cpu.kernel_va != 0))
                .then_some(cpu.view)
        });
        (
            Allocation {
                id: a.id,
                epoch: a.epoch,
                memory: a.memory,
                size: a.size,
                system_memory: a.system_memory,
                system_kernel_va: a.system_kernel_va,
                pins: Vec::new(),
                mappings: Vec::new(),
                cpu: None,
                resident: None,
                poisoned: a.poisoned,
                writeback: a.writeback,
                surface: a.surface,
                scanout_gem: 0,
                huge: a.huge,
            },
            existing,
            pending,
        )
    };
    let end = offset.checked_add(length).ok_or(Error::Invalid)?;
    if end > snapshot.size {
        return Err(Error::Invalid);
    }
    if length == 0 {
        return Ok(true);
    }
    if pending {
        // A previous temporary teardown failed after its guest view was removed.
        // Retry the retained host state before making another mapping.
        let mut mapping = st()
            .allocation_mut(id)?
            .cpu
            .take()
            .ok_or(Error::Invalid)?;
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            st().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
    } else if st().allocation(id)?.cpu.is_some() && existing.is_none() {
        return Err(Error::Busy);
    }
    if let Some(view) = existing {
        let address = view.address.checked_add(offset).ok_or(Error::Invalid)?;
        return Ok(view_copy(length, || f(0, address as *mut u8, length)));
    }
    if snapshot.system_memory != 0 {
        if snapshot.system_kernel_va == 0 {
            return Err(Error::Unsupported);
        }
        let address = snapshot
            .system_kernel_va
            .checked_add(offset)
            .ok_or(Error::Invalid)?;
        return Ok(view_copy(length, || f(0, address as *mut u8, length)));
    }

    // `NvPaging` 0x8 (BuildPagingBuffer's copies: unless 0x20): the copy runs without
    // the mutex (the temporary view is this call's alone; only its RM map and unmap
    // need the mutex). 0x10: the view is
    // write-combined, and a full fence drains the combining buffers after the copy.
    let knob = paging();
    let cache = if knob.copy_wc() {
        _MEMORY_CACHING_TYPE::MmWriteCombined
    } else {
        _MEMORY_CACHING_TYPE::MmNonCached
    };
    let mut completed = 0u64;
    while completed < length {
        let request_offset = offset + completed;
        let map_offset = request_offset & !(nm::PAGE_BYTES - 1);
        let page_offset = request_offset - map_offset;
        let chunk_len = (length - completed).min(nvrm::MAX_MAP_BYTES - page_offset);
        let request_end = request_offset + chunk_len;
        let map_end = request_end
            .checked_add(nm::PAGE_BYTES - 1)
            .map(|end| end & !(nm::PAGE_BYTES - 1))
            .unwrap_or(snapshot.size)
            .min(snapshot.size);
        if guard.is_none() {
            guard = Some(lock(passive, Kind::CpuRange)?);
        }
        let mut mapping =
            cpu_map_as(&io, &snapshot, map_offset, map_end - map_offset, None, None, cache)?;
        let unlocked = if paging_op {
            knob.paging_copy_unlocked()
        } else {
            knob.copy_unlocked()
        };
        if unlocked {
            guard = None;
            COPIES_UNLOCKED.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: page_offset is within the page-rounded mapping made above.
        let address = (mapping.view.address as *mut u8).add(page_offset as usize);
        let copy_start = now();
        let accepted = f(completed, address, chunk_len);
        if knob.copy_wc() {
            core::sync::atomic::fence(Ordering::SeqCst);
        }
        COPY_KIB.fetch_add(u32::try_from(chunk_len >> 10).unwrap_or(u32::MAX), Ordering::Relaxed);
        COPY_US.fetch_add(np::ticks_to_us(now().saturating_sub(copy_start)), Ordering::Relaxed);
        if guard.is_none() {
            match lock(passive, Kind::CpuRange) {
                Ok(g) => guard = Some(g),
                Err(error) => {
                    // Without the mutex the host half cannot be released; the guest
                    // view goes, the host state is kept for a later retry (the
                    // `pending` path) when the record can hold it.
                    if mapping.kernel_va != 0 {
                        MmUnmapIoSpace(mapping.kernel_va as *mut c_void, mapping.view.len);
                        mapping.kernel_va = 0;
                    }
                    let unstored = {
                        let mut state = st();
                        match state.allocation_mut(id) {
                            Ok(allocation) if allocation.cpu.is_none() => {
                                allocation.cpu = Some(mapping);
                                None
                            }
                            _ => Some(mapping),
                        }
                    };
                    if let Some(mapping) = unstored {
                        // The host may still name it: never released, never reused.
                        core::mem::forget(mapping);
                        COPY_LEAKED.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err(error);
                }
            }
        }
        if let Err(error) = cpu_unmap_for(&io, snapshot.memory, &mut mapping) {
            // Keep partially released host state reachable for a later retry or
            // transport teardown; never silently drop a live mapping.
            st().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
        if !accepted {
            return Ok(false);
        }
        completed += chunk_len;
    }
    Ok(true)
}

// ── VidMm paging on the copy engine (`NvPgGpu`, `helios_kmd_logic::pg_gpu`) ─────────────────
//
// `gpu_paging` runs a paging FILL / TRANSFER of a native video-memory allocation on the KMD's
// paging copy channel (`native_channel::pg_channel`) instead of the CPU: per chunk, the
// allocation's range is mapped into the paging space's video window and the system pages
// (an RM OS descriptor over them, shared / cached as the leaf updates' are) into its system
// window, the push runs and is waited for, and both are unmapped again. Only the RM calls run
// under the native-memory mutex (`NvPgGpu` 0x100 holds it throughout). Anything the copy
// engine cannot take returns `false` and the caller copies on the CPU as before.
//
// Counters (mirror pass, `publish_gpu`): `PgGpuKnob`; `PgGpuN0..3` operations done on the copy
// engine (transfer, fill, virtual fill, virtual transfer), `PgGpuKiB` their KiB, `PgGpuUs` /
// `PgGpuMaxUs` their time (map, copy, unmap; the paging latency), `PgGpuWaitUs` the part
// spent waiting for the copy engine, `PgGpuMapUs` the RM map / unmap part, `PgGpuChunks`
// chunks; `PgGpuFallN` operations given back to the CPU and `PgGpuFallWhy` the last reason
// (low byte, `fall::*`; the error above it); `PgGpuCpuN` / `PgGpuCpuKiB` paging copies done
// on the CPU (`with_cpu_range` from BuildPagingBuffer); `PgGpuNoGo` / `PgGpuNoGoWhy` the path
// turned off for the transport generation (a paging-space mapping that could not be undone);
// the channel's `PgGpuChState` (`rm_ce_channel::Svc` word), `PgGpuChErr` (bring-up step << 24 |
// error), `PgGpuEngine`, `PgGpuToken`, `PgGpuHung` releases that did not land.

/// The paging copy channel's VA space and its two windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PgSpace {
    space: u32,
    vid: u32,
    sys: u32,
}

/// Why an operation went back to the CPU (`PgGpuFallWhy` low byte).
mod fall {
    pub const LOCK: u32 = 1;
    pub const CLIENT: u32 = 2;
    pub const ALLOCATION: u32 = 3;
    pub const SYSTEM_BACKED: u32 = 4;
    pub const SPACE: u32 = 5;
    pub const CHANNEL: u32 = 6;
    pub const SHAPE: u32 = 7;
    pub const VID_MAP: u32 = 8;
    pub const DESCRIPTOR: u32 = 9;
    pub const SYS_MAP: u32 = 10;
    pub const RUN: u32 = 11;
    pub const UNMAP: u32 = 12;
    pub const NO_GO: u32 = 13;
}

static GPU_N: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static GPU_KIB: AtomicU32 = AtomicU32::new(0);
static GPU_US: AtomicU32 = AtomicU32::new(0);
static GPU_MAX_US: AtomicU32 = AtomicU32::new(0);
static GPU_MAP_US: AtomicU32 = AtomicU32::new(0);
static GPU_CHUNKS: AtomicU32 = AtomicU32::new(0);
static GPU_FALL: AtomicU32 = AtomicU32::new(0);
static GPU_FALL_WHY: AtomicU32 = AtomicU32::new(0);
static GPU_NOGO: AtomicU32 = AtomicU32::new(0);
static GPU_NOGO_N: AtomicU32 = AtomicU32::new(0);
static CPU_PAGING_N: AtomicU32 = AtomicU32::new(0);
static CPU_PAGING_KIB: AtomicU32 = AtomicU32::new(0);

/// `NvPgGpu` and `NvPgGpuMin`, read once (PASSIVE).
pub(crate) fn gpu_knob() -> (pg_gpu::Knob, u32) {
    static KNOB: AtomicU32 = AtomicU32::new(u32::MAX);
    static MIN: AtomicU32 = AtomicU32::new(0);
    let mut value = KNOB.load(Ordering::Relaxed);
    if value == u32::MAX {
        let read = crate::diag::read_config_dword(
            crate::diag::KnobName::new(b"NvPgGpu"),
            pg_gpu::DEFAULT_KNOB,
        );
        let min = crate::diag::read_config_dword(
            crate::diag::KnobName::new(b"NvPgGpuMin"),
            pg_gpu::DEFAULT_MIN_BYTES,
        );
        MIN.store(min, Ordering::Relaxed);
        value = if read == u32::MAX { pg_gpu::DEFAULT_KNOB } else { read };
        KNOB.store(value, Ordering::Relaxed);
    }
    (pg_gpu::Knob(value), MIN.load(Ordering::Relaxed))
}

/// Whether a paging operation `op` of `bytes` goes to the copy engine first.
pub(crate) fn gpu_wants(op: pg_gpu::Op, bytes: u64) -> bool {
    let (knob, min) = gpu_knob();
    pg_gpu::wants(knob, op, bytes, min) && GPU_NOGO.load(Ordering::Relaxed) == 0
}

/// A paging copy that ran on the CPU (`PgGpuCpuN` / `PgGpuCpuKiB`).
pub(crate) fn note_cpu_paging(bytes: u64) {
    CPU_PAGING_N.fetch_add(1, Ordering::Relaxed);
    CPU_PAGING_KIB.fetch_add(u32::try_from(bytes >> 10).unwrap_or(u32::MAX), Ordering::Relaxed);
}

fn publish_gpu() {
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    let rec = crate::diag::record_named_bytes;
    let (knob, _) = gpu_knob();
    if knob.0 == 0 {
        // Off: only how much BuildPagingBuffer copied on the CPU.
        if r(&CPU_PAGING_N) != 0 {
            rec(b"PgGpuCpuN", r(&CPU_PAGING_N));
            rec(b"PgGpuCpuKiB", r(&CPU_PAGING_KIB));
        }
        return;
    }
    rec(b"PgGpuKnob", knob.0);
    for (i, name) in [b"PgGpuN0", b"PgGpuN1", b"PgGpuN2", b"PgGpuN3"].iter().enumerate() {
        rec(&name[..], r(&GPU_N[i]));
    }
    rec(b"PgGpuKiB", r(&GPU_KIB));
    rec(b"PgGpuUs", r(&GPU_US));
    rec(b"PgGpuMaxUs", r(&GPU_MAX_US));
    let wait = super::native_channel::pg_channel::WAIT_US.load(Ordering::Relaxed);
    rec(b"PgGpuWaitUs", u32::try_from(wait).unwrap_or(u32::MAX));
    rec(b"PgGpuMapUs", r(&GPU_MAP_US));
    rec(b"PgGpuChunks", r(&GPU_CHUNKS));
    rec(b"PgGpuCpuN", r(&CPU_PAGING_N));
    rec(b"PgGpuCpuKiB", r(&CPU_PAGING_KIB));
    rec(b"PgGpuFallN", r(&GPU_FALL));
    rec(b"PgGpuFallWhy", r(&GPU_FALL_WHY));
    rec(b"PgGpuNoGo", r(&GPU_NOGO_N));
    rec(b"PgGpuNoGoWhy", r(&GPU_NOGO));
    let (err, engine, token, hung, _) = super::native_channel::pg_channel::counters();
    rec(b"PgGpuChState", super::native_channel::pg_channel::state_word());
    rec(b"PgGpuChErr", err);
    rec(b"PgGpuEngine", engine);
    rec(b"PgGpuToken", token);
    rec(b"PgGpuHung", hung);
}

/// The system side of a copy-engine paging operation: its page frames (locked by VidMm for the
/// operation) and the operation's first byte, counted from the start of `pfns[0]`.
#[derive(Clone, Copy)]
pub struct GpuSystem<'a> {
    pub pfns: &'a [u64],
    pub start: u64,
}

/// Turn the path off for the transport generation of `epoch` (a paging-space mapping stayed).
fn gpu_no_go(epoch: u64, why: u32) {
    if st().client.as_ref().map(|c| c.epoch) == Some(epoch) {
        GPU_NOGO.store(why, Ordering::Relaxed);
        GPU_NOGO_N.fetch_add(1, Ordering::Relaxed);
    }
}

/// The paging space, made on first use: a new VA space (RM places the channel's own objects
/// in it) and two fixed windows (`pg_gpu::VID_WINDOW_VA`, `pg_gpu::SYS_WINDOW_VA`) the
/// operations map into at fixed offsets. Under the mutex.
fn ensure_pg_space(io: &Io<'_>) -> Result<PgSpace, Error> {
    if let Some(space) = st().pg_space {
        return Ok(space);
    }
    let space = st().mint()?;
    io.rm_alloc(nd::H_DEVICE, space, cc::FERMI_VASPACE_A, &cc::vaspace_params_vidmm())?;
    let mut made = PgSpace { space, vid: 0, sys: 0 };
    let windows = [
        (pg_gpu::VID_WINDOW_VA, pg_gpu::VID_WINDOW_BYTES),
        (pg_gpu::SYS_WINDOW_VA, pg_gpu::SYS_WINDOW_BYTES),
    ];
    for (i, (va, len)) in windows.into_iter().enumerate() {
        let minted = st().mint();
        let window = match minted {
            Ok(w) => w,
            Err(error) => {
                let _ = io.rm_free(nd::H_DEVICE, space);
                return Err(error);
            }
        };
        let params = cc::virtual_params(io.client.root, space, Some(va), len);
        if let Err(error) = io.rm_alloc(nd::H_DEVICE, window, cc::NV50_MEMORY_VIRTUAL, &params) {
            // Frees the windows made so far with it.
            let _ = io.rm_free(nd::H_DEVICE, space);
            return Err(error);
        }
        if i == 0 {
            made.vid = window;
        } else {
            made.sys = window;
        }
    }
    st().pg_space = Some(made);
    Ok(made)
}

/// What one chunk mapped.
struct PgMapped {
    /// The OS descriptor of the system window, if one is mapped.
    desc: Option<u32>,
}

fn pg_unmap_one(io: &Io<'_>, window: u32, memory: u32, va: u64) -> Result<(), Error> {
    let block = nm::nvos47_unmap(io.client.root, nd::H_DEVICE, window, memory, va);
    let mut response = [0u8; REPLY_MAX];
    let n = io.exchange(
        io.client.ctl,
        rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
        &block,
        &[],
        &mut response,
    )?;
    rc::rm_reply(response.get(..n).ok_or(Error::Host)?, cc::NVOS47_STATUS_AT)
        .map(|_| ())
        .map_err(|_| Error::Host)
}

/// Map one chunk: `vm` of `memory` into the video window, the pages `sp` of `system` into the
/// system window. On failure nothing stays mapped (`Err(reason)`), unless an unmap failed
/// too (then the path is off: [`gpu_no_go`]).
fn pg_map_chunk(
    io: &Io<'_>,
    id: u32,
    memory: u32,
    space: PgSpace,
    vm: pg_gpu::VidMap,
    system: Option<(GpuSystem<'_>, pg_gpu::SysPages)>,
) -> Result<PgMapped, u32> {
    let piece = nm::MappingPiece {
        va: pg_gpu::VID_WINDOW_VA,
        memory_offset: vm.map_offset,
        len: vm.map_len,
    };
    let mut mapped = Err(Error::Invalid);
    if vm.big {
        let flags = np::with_page_size(LOCAL_4K_MAP_FLAGS, np::MAP_PAGE_BIG);
        mapped = map_in_window(io, memory, space.vid, flags, piece);
    }
    if mapped.is_err() {
        mapped = map_in_window(io, memory, space.vid, LOCAL_4K_MAP_FLAGS, piece);
    }
    if let Err(error) = mapped {
        return Err(fall::VID_MAP | (error as u32) << 8);
    }
    let undo_vid = |io: &Io<'_>| {
        if pg_unmap_one(io, space.vid, memory, pg_gpu::VID_WINDOW_VA).is_err() {
            gpu_no_go(io.client.epoch, fall::UNMAP);
        }
    };
    let Some((system, sp)) = system else {
        return Ok(PgMapped { desc: None });
    };
    let Some(pfns) = system.pfns.get(sp.first..sp.first + sp.count) else {
        undo_vid(io);
        return Err(fall::SHAPE);
    };
    let len = sp.count as u64 * pg_gpu::PAGE;
    let (desc, new) = match take_desc(pfns, id) {
        Ok(taken) => taken,
        Err(error) => {
            undo_vid(io);
            return Err(fall::DESCRIPTOR | (error as u32) << 8);
        }
    };
    if new {
        if let Err(error) = alloc_system_descriptor(io, desc, pfns, len) {
            undo_vid(io);
            return Err(fall::DESCRIPTOR | (error as u32) << 8);
        }
        DESC_NEW.fetch_add(1, Ordering::Relaxed);
    }
    let piece = nm::MappingPiece {
        va: pg_gpu::SYS_WINDOW_VA,
        memory_offset: 0,
        len,
    };
    if let Err(error) = map_in_window(io, desc, space.sys, cc::MAP_FLAGS_SYSMEM, piece) {
        give_back_desc(io, desc, new);
        undo_vid(io);
        return Err(fall::SYS_MAP | (error as u32) << 8);
    }
    if new {
        // Unrecorded (no memory): it is freed at its unmap below.
        let _ = st().descs.insert(desc, pfns, id);
    }
    Ok(PgMapped { desc: Some(desc) })
}

/// Unmap one chunk's windows. `Err` when a window could not be emptied (the path is then off).
fn pg_unmap_chunk(io: &Io<'_>, memory: u32, space: PgSpace, mapped: &PgMapped) -> Result<(), ()> {
    let mut ok = true;
    if let Some(desc) = mapped.desc {
        if pg_unmap_one(io, space.sys, desc, pg_gpu::SYS_WINDOW_VA).is_ok() {
            // Freed unless another mapping shows it or it is kept idle (`NvPaging` 0x2).
            if release_desc(io, desc).is_err() {
                // The descriptor stays allocated; the window is empty.
            }
        } else {
            ok = false;
        }
    }
    if pg_unmap_one(io, space.vid, memory, pg_gpu::VID_WINDOW_VA).is_err() {
        ok = false;
    }
    if ok {
        Ok(())
    } else {
        Err(())
    }
}

/// Run a paging operation of native allocation `id` on the copy engine: `work` over
/// `[offset, offset + length)` of its video memory, with `system`'s pages for a transfer.
/// `true`: done (the bytes are in place when this returns); `false`: nothing is pending on
/// the GPU and the caller copies on the CPU. PASSIVE (BuildPagingBuffer).
pub fn gpu_paging(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    op: pg_gpu::Op,
    id: u32,
    offset: u64,
    length: u64,
    work: pg_gpu::Work,
    system: Option<GpuSystem<'_>>,
) -> bool {
    let start = now();
    let result = gpu_paging_inner(passive, adapter, id, offset, length, work, system);
    let us = np::ticks_to_us(now().saturating_sub(start));
    match result {
        Ok(chunks) => {
            GPU_N[op.index()].fetch_add(1, Ordering::Relaxed);
            GPU_KIB.fetch_add(u32::try_from(length >> 10).unwrap_or(u32::MAX), Ordering::Relaxed);
            GPU_US.fetch_add(us, Ordering::Relaxed);
            GPU_MAX_US.fetch_max(us, Ordering::Relaxed);
            GPU_CHUNKS.fetch_add(chunks, Ordering::Relaxed);
            true
        }
        Err(why) => {
            GPU_FALL.fetch_add(1, Ordering::Relaxed);
            GPU_FALL_WHY.store(why, Ordering::Relaxed);
            false
        }
    }
}

fn gpu_paging_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    length: u64,
    work: pg_gpu::Work,
    system: Option<GpuSystem<'_>>,
) -> Result<u32, u32> {
    use super::native_channel::pg_channel;
    if GPU_NOGO.load(Ordering::Relaxed) != 0 {
        return Err(fall::NO_GO);
    }
    let (knob, _) = gpu_knob();
    let op_guard = pg_channel::lock(passive, LOCK_MAX_MS).ok_or(fall::LOCK)?;
    let map_started = now();
    let mut guard = Some(lock(passive, Kind::GpuPaging).map_err(|_| fall::LOCK)?);
    let client = st().client().map_err(|_| fall::CLIENT)?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (memory, size) = {
        let state = st();
        let a = state.allocation(id).map_err(|_| fall::ALLOCATION)?;
        if a.poisoned {
            return Err(fall::ALLOCATION);
        }
        if a.system_memory != 0 {
            return Err(fall::SYSTEM_BACKED);
        }
        (a.memory, a.size)
    };
    let space = ensure_pg_space(&io).map_err(|error| fall::SPACE | (error as u32) << 8)?;
    if !knob.hold_lock() {
        guard = None;
    }
    let mut map_ticks = now().saturating_sub(map_started);
    let ready = pg_channel::ensure(passive, adapter, &client, space.space, &op_guard);
    if let Err(error) = ready {
        return Err(fall::CHANNEL | (error as u32) << 8);
    }
    let mut done = 0u64;
    let mut chunks = 0u32;
    let result = loop {
        if done >= length {
            break Ok(chunks);
        }
        let n = pg_gpu::chunk_len(length, done);
        let Some(vm) = offset.checked_add(done).and_then(|at| pg_gpu::vid_map(at, n, size)) else {
            break Err(fall::SHAPE);
        };
        let sp = match system {
            Some(s) => match s
                .start
                .checked_add(done)
                .and_then(|at| pg_gpu::sys_pages(at, n, s.pfns.len()))
            {
                Some(sp) => Some((s, sp)),
                None => break Err(fall::SHAPE),
            },
            None => None,
        };
        let t = now();
        if guard.is_none() {
            match lock(passive, Kind::GpuPaging) {
                Ok(g) => guard = Some(g),
                Err(_) => break Err(fall::LOCK),
            }
        }
        if st().client.as_ref().map(|c| c.epoch) != Some(client.epoch) {
            break Err(fall::CLIENT);
        }
        let mapped = match pg_map_chunk(&io, id, memory, space, vm, sp) {
            Ok(mapped) => mapped,
            Err(why) => break Err(why),
        };
        if !knob.hold_lock() {
            guard = None;
        }
        map_ticks += now().saturating_sub(t);
        let vid_va = pg_gpu::VID_WINDOW_VA + vm.delta;
        let sys_va = pg_gpu::SYS_WINDOW_VA + sp.map_or(0, |(_, p)| p.intra);
        let ran = pg_channel::run(passive, adapter, &client, &op_guard, n, |p, first, sem, seq| {
            pg_gpu::push(p, helios_kmd_logic::ce_present::Gen::Gb202, first, work, vid_va, sys_va, n, sem, seq)
        });
        let t = now();
        if guard.is_none() {
            match lock(passive, Kind::GpuPaging) {
                Ok(g) => guard = Some(g),
                Err(_) => {
                    // The windows stay mapped: no later operation may use them.
                    gpu_no_go(client.epoch, fall::UNMAP);
                    break Err(fall::LOCK);
                }
            }
        }
        let unmapped = pg_unmap_chunk(&io, memory, space, &mapped);
        if !knob.hold_lock() {
            guard = None;
        }
        map_ticks += now().saturating_sub(t);
        if let Err(error) = ran {
            if unmapped.is_err() {
                gpu_no_go(client.epoch, fall::UNMAP);
            }
            break Err(fall::RUN | (error as u32) << 8);
        }
        done += n;
        chunks += 1;
        if unmapped.is_err() {
            gpu_no_go(client.epoch, fall::UNMAP);
            // This chunk is done; the rest cannot use the windows.
            if done < length {
                break Err(fall::UNMAP);
            }
        }
    };
    drop(guard);
    GPU_MAP_US.fetch_add(np::ticks_to_us(map_ticks), Ordering::Relaxed);
    result
}

/// Map a native CPU-host-aperture request.  pSystemMem is already the direct
/// Lock2 view.  Device local memory uses NVOS33 + the RM window, and succeeds
/// only when that window placement is exactly dxgkrnl's requested GPA.
pub fn map_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    aperture_offset: u64,
    expected_gpa: u64,
) -> Result<CpuView, Error> {
    let _guard = lock(passive, Kind::ApertureMap)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let snapshot = {
        let state = st();
        let a = state.allocation(id)?;
        Allocation {
            id: a.id,
            epoch: a.epoch,
            memory: a.memory,
            size: a.size,
            system_memory: a.system_memory,
            system_kernel_va: a.system_kernel_va,
            pins: Vec::new(),
            mappings: Vec::new(),
            cpu: None,
            resident: None,
            poisoned: a.poisoned,
            writeback: a.writeback,
            surface: a.surface,
            scanout_gem: 0,
            huge: a.huge,
        }
    };
    let mut mapping = cpu_map(
        &io,
        &snapshot,
        0,
        snapshot.size,
        Some(aperture_offset),
        Some(expected_gpa),
    )?;
    let view = mapping.view;
    let mut state = st();
    let allocation = state.allocation_mut(id)?;
    if allocation.cpu.is_some() {
        drop(state);
        let mut mapping = mapping;
        let _ = cpu_unmap_for(&io, snapshot.memory, &mut mapping);
        return Err(Error::Busy);
    }
    allocation.cpu = Some(mapping);
    Ok(view)
}

pub fn unmap_aperture(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    aperture_offset: u64,
) -> Result<(), Error> {
    let _guard = lock(passive, Kind::ApertureUnmap)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (id, memory) = {
        let state = st();
        let allocation = state
            .allocations
            .iter()
            .find(|a| {
                a.cpu
                    .as_ref()
                    .is_some_and(|cpu| cpu.aperture_offset == Some(aperture_offset))
            })
            .ok_or(Error::Invalid)?;
        (allocation.id, allocation.memory)
    };
    let mut mapping = st()
        .allocation_mut(id)?
        .cpu
        .take()
        .ok_or(Error::Invalid)?;
    match cpu_unmap_for(&io, memory, &mut mapping) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Preserve whatever remains live for a later retry or transport
            // teardown; a failed host unmap is not permission to forget it.
            st().allocation_mut(id)?.cpu = Some(mapping);
            Err(error)
        }
    }
}

/// Destroy an allocation only after every GPU and CPU view is gone.  Failure
/// leaves the record and its MDLs alive; the caller must not report reclamation
/// or unlock pSystemMem early.
pub fn destroy(passive: PassiveLevel, adapter: &AdapterContext, id: u32) -> Result<(), Error> {
    crate::ddi::pg_check::forget(id);
    let guard = lock(passive, Kind::Destroy)?;
    let client = st().client()?;
    let io = Io {
        passive,
        adapter,
        client,
    };
    let (memory, mappings, gem) = {
        let state = st();
        let allocation = state.allocation(id)?;
        (
            allocation.memory,
            allocation.mappings.clone(),
            allocation.scanout_gem,
        )
    };
    // An exiting process's channels stop before its mappings go (`hws::exit_gate`). The
    // keys are live: release_process (under this mutex) drops or retires a process's
    // records before its ProcessContext is freed.
    {
        let mut gated: Vec<usize> = Vec::new();
        for m in mappings.iter() {
            if m.process != 0 && nt::is_live_key(m.process) && !gated.contains(&m.process) {
                if gated.try_reserve(1).is_err() {
                    break;
                }
                gated.push(m.process);
                // SAFETY: a live process key under the I/O mutex (see above).
                unsafe { crate::ddi::hws::exit_gate(adapter, m.process) };
            }
        }
    }
    // A CPU view dxgkrnl did not unmap first (an UnmapApertureSegment that came
    // above PASSIVE leaves it, `cpu_host_aperture`) goes here: nothing maps the
    // allocation through the aperture once it is destroyed.
    let cpu = st().allocation_mut(id)?.cpu.take();
    if let Some(mut mapping) = cpu {
        if let Err(error) = cpu_unmap_for(&io, memory, &mut mapping) {
            st().allocation_mut(id)?.cpu = Some(mapping);
            return Err(error);
        }
    }
    if gem != 0 {
        super::native_scanout::release_gem(&io, gem);
        st().allocation_mut(id)?.scanout_gem = 0;
    }
    // Every mapping is tried (in as few round trips as `NvPaging` 0x1 allows); one
    // RM refuses stays recorded (and the allocation with it), the others are gone
    // and not named again.
    let mut items = Vec::new();
    items.try_reserve_exact(mappings.len()).map_err(|_| Error::NoMemory)?;
    items.extend(mappings.iter().map(|m| (memory, *m, false)));
    let mut results = Vec::new();
    unmap_many(&io, &items, false, &mut results)?;
    // The record's mappings are the copy taken above (only holders of the mutex
    // change them): drop the unmapped ones in one pass (it was a STATE hold and a
    // scan of the whole list per mapping).
    let mut unmapped = Ok(());
    let mut kept = Vec::new();
    kept.try_reserve_exact(results.iter().filter(|r| r.is_err()).count())
        .map_err(|_| Error::NoMemory)?;
    for (mapping, result) in mappings.iter().zip(&results) {
        if let Err(error) = result {
            unmapped = Err(*error);
            kept.push((mapping.virt, mapping.piece.va));
        }
    }
    {
        let mut state = st();
        let allocation = state.allocation_mut(id)?;
        if unmapped.is_ok() {
            allocation.mappings.clear();
        } else {
            let tried = &mappings;
            allocation.mappings.retain(|live| {
                kept.contains(&(live.virt, live.piece.va))
                    || !tried.iter().any(|m| m.virt == live.virt && m.piece.va == live.piece.va)
            });
        }
    }
    unmapped?;
    // Its pages are VidMm's to free now: no idle descriptor may outlive it.
    drop_idle_descs_of(&io, id);
    io.rm_free(nd::H_DEVICE, memory)?;
    super::nvda::forget_memory(memory);
    let mut allocation = {
        let mut state = st();
        let index = state.index_of(id).ok_or(Error::Invalid)?;
        state.allocations.remove(index)
    };
    // The record is gone and RM freed the memory: nothing names the pages, so
    // they unlock outside the mutex (MmUnlockPages of a large allocation is long).
    drop(guard);
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// `DxgkDdiDestroyProcess`: release every RM virtual object the process still
/// maps and its VA space. Process contexts are keyed by address, so a later
/// process can reuse the key: left behind, the dead process's fixed-VA objects
/// made RM refuse the new process's identical VidMm VAs (the D3D11 UMD's GPFIFO
/// at 0x4000000, NV_ERR_INSUFFICIENT_RESOURCES). Returns how many mappings
/// could not be released (they stay recorded and the space is kept).
pub fn release_process(passive: PassiveLevel, adapter: &AdapterContext, process: usize) -> u32 {
    if process == 0 {
        return 0;
    }
    let (kept, retired) = release_process_inner(passive, adapter, process);
    if kept != 0 && !retired {
        // An early exit (the mutex, no memory, the transport) left the process's records,
        // space and foreign ranges under its key: the freed ProcessContext's address,
        // which the next process can get. Retired here as on the normal path, or that
        // process inherited the dead one's space and stale mappings. `NvPrRelRetire`.
        retire_process(process);
        let n = RELEASE_RETIRED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvPrRelRetire", n);
    }
    kept
}

/// `NvPrRelRetire`: release_process early exits whose records were retired.
static RELEASE_RETIRED: AtomicU32 = AtomicU32::new(0);

/// File everything still recorded under `process` under the dead key
/// (`native_teardown::retire_owner`) and empty its index: the key is a freed
/// `ProcessContext`'s address that a new process may get.
fn retire_process(process: usize) {
    let mut state = st();
    for allocation in state.allocations.iter_mut() {
        let mut changed = false;
        for mapping in allocation.mappings.iter_mut() {
            let owner = nt::retire_owner(mapping.process, process);
            changed |= owner != mapping.process;
            mapping.process = owner;
        }
        if changed {
            allocation
                .mappings
                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
        }
    }
    for mapping in state.foreign.iter_mut() {
        mapping.process = nt::retire_owner(mapping.process, process);
    }
    for (owner, _) in state.spaces.iter_mut() {
        *owner = nt::retire_owner(*owner, process);
    }
    state.idle_spaces.retain(|p| *p != process);
    if let Some(at) = state.by_process.iter().position(|(p, _)| *p == process) {
        state.by_process.swap_remove(at);
    }
}

/// [`release_process`]'s work: `(kept, retired)`, retired when what stayed was already
/// filed under the dead key.
fn release_process_inner(passive: PassiveLevel, adapter: &AdapterContext, process: usize) -> (u32, bool) {
    let Ok(_guard) = lock(passive, Kind::Process) else {
        return (1, false);
    };
    let Ok(client) = st().client() else {
        // No native client: nothing was ever mapped for anyone.
        let mut state = st();
        state.spaces.retain(|(owner, _)| *owner != process);
        state.idle_spaces.retain(|p| *p != process);
        return (0, true);
    };
    let io = Io {
        passive,
        adapter,
        client,
    };
    let owned: Vec<(u32, u32, HostMapping)> = {
        let state = st();
        let mut owned = Vec::new();
        let mut candidates = Vec::new();
        if state.candidates(process, &mut candidates).is_err() {
            return (1, false);
        }
        for id in candidates {
            let Some(i) = state.index_of(id) else { continue };
            let allocation = &state.allocations[i];
            for mapping in allocation.mappings.iter().filter(|m| m.process == process) {
                if owned.try_reserve(1).is_err() {
                    return (1, false);
                }
                owned.push((allocation.id, allocation.memory, *mapping));
            }
        }
        owned
    };
    let mut kept = 0;
    let foreign: Vec<HostMapping> = {
        let mut state = st();
        let mut mine = Vec::new();
        if mine.try_reserve(state.foreign.len()).is_err() {
            return (1, false);
        }
        state.foreign.retain(|m| {
            if m.process == process {
                mine.push(*m);
                false
            } else {
                true
            }
        });
        mine
    };
    // The foreign ranges taken out above go back (under the dead key, as for an unmap RM
    // refused) when nothing of them was unmapped: dropped from the records they stayed
    // mapped with nothing left to name them.
    let put_back = |foreign: &[HostMapping]| {
        let mut state = st();
        if state.foreign.try_reserve(foreign.len()).is_ok() {
            state.foreign.extend(foreign.iter().map(|m| HostMapping {
                process: nt::DEAD_PROCESS,
                ..*m
            }));
        }
    };
    // Every unmap in as few round trips as `NvPaging` 0x1 allows.
    let mut items = Vec::new();
    if items.try_reserve_exact(foreign.len() + owned.len()).is_err() {
        put_back(&foreign);
        return (1, false);
    }
    items.extend(foreign.iter().map(|m| (0u32, *m, false)));
    items.extend(owned.iter().map(|(_, memory, m)| (*memory, *m, false)));
    let mut results = Vec::new();
    if unmap_many(&io, &items, false, &mut results).is_err() {
        put_back(&foreign);
        return (1, false);
    }
    let (foreign_results, owned_results) = results.split_at(foreign.len());
    for (mapping, result) in foreign.into_iter().zip(foreign_results) {
        if result.is_err() {
            kept += 1;
            // Still mapped: recorded under the dead key (see below).
            let mut state = st();
            if state.foreign.try_reserve(1).is_ok() {
                state.foreign.push(HostMapping {
                    process: nt::DEAD_PROCESS,
                    ..mapping
                });
            }
        }
    }
    // Drop the released mappings in one pass per allocation (it was a STATE hold
    // and a scan of the allocation's list per mapping). Only holders of the mutex
    // add mappings, so the process's mappings are still the ones collected above.
    let mut failed = Vec::new();
    if failed
        .try_reserve_exact(owned_results.iter().filter(|r| r.is_err()).count())
        .is_err()
    {
        // No memory: nothing is dropped from the records, and the process's
        // space stays (as for a mapping RM would not release).
        return (1, false);
    }
    for ((id, _, mapping), result) in owned.iter().zip(owned_results) {
        if result.is_err() {
            kept += 1;
            failed.push((*id, mapping.virt, mapping.piece.va));
        }
    }
    {
        let mut state = st();
        let mut last = None;
        for (id, _, _) in owned.iter() {
            if last == Some(*id) {
                continue;
            }
            last = Some(*id);
            if let Ok(allocation) = state.allocation_mut(*id) {
                allocation.mappings.retain(|live| {
                    live.process != process
                        || failed.contains(&(*id, live.virt, live.piece.va))
                });
            }
        }
    }
    if kept == 0 {
        let space = st()
            .spaces
            .iter()
            .find(|(owner, _)| *owner == process)
            .map(|(_, space)| *space);
        if let Some(space) = space {
            for window in windows_of(space) {
                let started = now();
                if io.rm_free(nd::H_DEVICE, window).is_ok() {
                    va_note(vw::Op::WinFree, started);
                    st().windows.remove(window);
                }
            }
            let started = now();
            if io.rm_free(nd::H_DEVICE, space).is_ok() {
                va_note(vw::Op::SpaceFree, started);
                let mut state = st();
                // A window RM would not free alone went with its space.
                state.windows.remove_space(space);
                state.spaces.retain(|(owner, _)| *owner != process);
            } else {
                kept += 1;
            }
        }
        st().idle_spaces.retain(|p| *p != process);
    }
    if kept != 0 {
        // What RM would not release stays recorded, but no longer under this key:
        // the key is the freed ProcessContext's address, and a new process at the
        // same address must get a fresh space, not the dead one's. Allocation
        // teardown and the transport's sweep still find it.
        let mut state = st();
        for allocation in state.allocations.iter_mut() {
            for mapping in allocation.mappings.iter_mut() {
                mapping.process = nt::retire_owner(mapping.process, process);
            }
        }
        for allocation in state.allocations.iter_mut() {
            allocation
                .mappings
                .sort_unstable_by_key(|mapping| (mapping.process, mapping.piece.va));
        }
        for (owner, _) in state.spaces.iter_mut() {
            *owner = nt::retire_owner(*owner, process);
        }
        state.idle_spaces.retain(|p| *p != process);
    }
    // A new process may get this key: its index starts empty. (What stayed is
    // recorded under the dead key, which no update names.)
    {
        let mut state = st();
        if let Some(at) = state.by_process.iter().position(|(p, _)| *p == process) {
            state.by_process.swap_remove(at);
        }
    }
    (kept, true)
}

/// Drop only guest-owned state after the transport already destroyed every RM
/// object.  No host request is sent and no pSystemMem page is unlocked early.
pub fn discard(id: u32) -> Result<(), Error> {
    let mut state = st();
    let index = state.index_of(id).ok_or(Error::Invalid)?;
    let mut allocation = state.allocations.remove(index);
    // The transport destroyed every RM object: its idle descriptors are gone too.
    let mut dead = Vec::new();
    state.descs.drop_idle_of(id, &mut dead);
    drop(state);
    drop(dead);
    if let Some(cpu) = allocation.cpu.as_mut() {
        if cpu.kernel_va != 0 {
            // PASSIVE (DestroyAllocation); the host mapping is already gone.
            unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
            cpu.kernel_va = 0;
        }
    }
    unlock_pins(&mut allocation.pins);
    Ok(())
}

/// Forget host objects after the transport-wide close sweep.  Kernel CPU views
/// and MDLs still belong to the guest and are released here, at PASSIVE.
pub fn forget(passive: PassiveLevel) {
    // The paging copy channel first (its lock comes before this module's mutex).
    super::native_channel::pg_channel::forget(passive);
    GPU_NOGO.store(0, Ordering::Relaxed);
    // Unbounded: a skipped forget would leave the old generation's client in place
    // and every later `create` refused as NotReady. Holders' round trips are
    // bounded, so this wait ends.
    let Ok(_guard) = mutex(passive).and_then(|m| m.lock(passive).ok_or(Error::Busy)) else {
        return;
    };
    let mut state = st();
    let mut allocations = core::mem::take(&mut state.allocations);
    state.client = None;
    state.spaces.clear();
    state.windows.clear();
    state.foreign.clear();
    // The sweep closed every RM object: the descriptors went with them.
    state.descs.clear();
    state.by_process.clear();
    state.index_broken = false;
    state.idle_spaces.clear();
    state.pg_space = None;
    // Pins kept because the host's answer was unknown: the sweep closed every
    // object that could name them.
    let mut orphans = core::mem::take(&mut state.orphan_pins);
    drop(state);
    unlock_pins(&mut orphans);
    for allocation in allocations.iter_mut() {
        if let Some(cpu) = allocation.cpu.as_mut() {
            if cpu.kernel_va != 0 {
                unsafe { MmUnmapIoSpace(cpu.kernel_va as *mut c_void, cpu.view.len) };
                cpu.kernel_va = 0;
            }
        }
        unlock_pins(&mut allocation.pins);
    }
}

/// What [`lookup_va`] found mapped at a VA (diagnosis: `ddi::native_snap`).
#[derive(Clone, Copy, Default)]
pub struct VaHit {
    pub process: usize,
    /// 1 a native allocation's RM memory, 2 VidMm's system pages of a native allocation
    /// (an OS-descriptor mapping), 3 a non-native allocation's system pages (`foreign`).
    pub kind: u8,
    pub id: u32,
    pub memory: u32,
    pub size: u64,
    pub piece_va: u64,
    pub piece_len: u64,
    pub memory_offset: u64,
    /// The physical address of the byte at the VA for kinds 2 and 3 (0 = unknown).
    pub physical: u64,
    /// VidMm's last residency notification (`notify_residency`): the physical address.
    pub resident: Option<u64>,
    pub system_memory: bool,
    pub poisoned: bool,
    pub huge: bool,
}

/// Diagnosis (`ddi::native_snap`, `NvSnapVa`): every mapping this KMD made that covers `va`
/// in `process`'s space (0 = in any process), up to `out.len()`. Read-only: one state lock,
/// no host call; any IRQL up to DISPATCH. Returns how many were written.
pub fn lookup_va(process: usize, va: u64, out: &mut [VaHit]) -> usize {
    let state = st();
    let mut n = 0usize;
    let covers = |m: &HostMapping| {
        (process == 0 || m.process == process)
            && va >= m.piece.va
            && m.piece.end().is_some_and(|end| va < end)
    };
    let mut visit = |allocation: &Allocation, n: &mut usize| {
        for m in allocation.mappings.iter().filter(|m| covers(m)) {
            if *n >= out.len() {
                return;
            }
            let delta = va - m.piece.va;
            let physical = if m.desc != 0 {
                state
                    .descs
                    .pfns(m.desc)
                    .and_then(|pfns| pfns.get((delta >> 12) as usize).copied())
                    .map_or(0, |pfn| (pfn << 12) | (va & 0xfff))
            } else {
                0
            };
            out[*n] = VaHit {
                process: m.process,
                kind: if m.desc != 0 { 2 } else { 1 },
                id: allocation.id,
                memory: allocation.memory,
                size: allocation.size,
                piece_va: m.piece.va,
                piece_len: m.piece.len,
                memory_offset: m.piece.memory_offset,
                physical,
                resident: allocation.resident.map(|(pa, _)| pa),
                system_memory: allocation.system_memory != 0,
                poisoned: allocation.poisoned,
                huge: allocation.huge,
            };
            *n += 1;
        }
    };
    let indexed = process != 0 && !state.index_broken;
    if indexed {
        let ids = state
            .by_process
            .iter()
            .find(|(p, _)| *p == process)
            .map_or(&[][..], |(_, ids)| ids.as_slice());
        for id in ids {
            if let Some(i) = state.index_of(*id) {
                visit(&state.allocations[i], &mut n);
            }
        }
    } else {
        for allocation in state.allocations.iter() {
            visit(allocation, &mut n);
        }
    }
    for m in state.foreign.iter().filter(|m| covers(m)) {
        if n >= out.len() {
            break;
        }
        let delta = va - m.piece.va;
        let physical = state
            .descs
            .pfns(m.desc)
            .and_then(|pfns| pfns.get((delta >> 12) as usize).copied())
            .map_or(0, |pfn| (pfn << 12) | (va & 0xfff));
        out[n] = VaHit {
            process: m.process,
            kind: 3,
            piece_va: m.piece.va,
            piece_len: m.piece.len,
            physical,
            ..VaHit::default()
        };
        n += 1;
    }
    n
}
