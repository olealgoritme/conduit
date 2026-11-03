//! Hang snapshots of the native NVIDIA channels: the I/O half of
//! `helios_kmd_logic::native_snap` (layout, decoder and trigger rules live there and are
//! host-tested; the host decoder is `pr198-tools/agent-snap/snapdec.py`).
//!
//! Runs only on the registry mirror thread (`ddi::mirror_thread`), at PASSIVE, at most once a
//! second ([`publish`]; the thread wakes at least every second in the NVIDIA personality):
//!
//! * `NvSnapMs` (default 3000, 0 = off; clamped 500..600000): a native queue whose progress
//!   fence is short of its last submission and stayed put this long triggers an automatic
//!   snapshot, and again every 5 s while the stall lasts (at most `AUTO_MAX` per boot, 2 s
//!   apart). A queue whose error notifier gets set triggers one too. Independent of `NvHangMs`
//!   (recovery). USERD GP_GET is recorded but not used: GB20x does not write it back.
//! * `NvSnapNow` = nonzero: one snapshot now, every queue in detail; reset to 0.
//! * `NvSnapVa` = VA >> 12: what the KMD has mapped at that VA in any process (allocation,
//!   RM memory or system pages, VidMm residency, the page-table ring's last operation there,
//!   the first 8 bytes) as `NvSnapVaR` (a blob with only a VA table) and `NvSnapVaQ` (the
//!   query), `NvSnapVaN` (hits), `NvSnapVaK` (first hit kind | resident << 8), `NvSnapVaId`
//!   (its allocation); reset to 0.
//!
//! Each queue keeps its last submissions (progress fence id, GPFIFO range; `NativeQueue::submit`);
//! the first one the progress fence does not show complete is the snapshot's focus: the GPFIFO
//! window is read around it and its segments (and two before) decoded. The NVIDIA UMDs open
//! their submissions with runs of NOP entries (all zero): the focus starts at the stuck
//! submission's first non-NOP entry (`ns::skip_nops`, at most `ns::NOP_SCAN` entries scanned),
//! so the window and the decoded segments are its real work, not its NOP gap. The ring, pushbuffers
//! and semaphores are read by GPU VA through the KMD's mapping table (`lookup_va`): VidMm's
//! system pages for a native system-memory allocation, the RM object's CPU view otherwise.
//!
//! A snapshot is `NvSnapBlob` (REG_BINARY, the latest) and `NvSnapB<seq % 4>` (the last four);
//! kept apart from those: `NvSnapLong` (the automatic one with the longest stall), `NvSnapErr`
//! (the first error-notifier one) and `NvSnapPre` (the automatic one before it). With `NvSnapN` (snapshots), `NvSnapT` (interrupt time, ms), `NvSnapWhy` (reason | the
//! trigger channel id << 8), `NvSnapTok` (the trigger's work-submit token), `NvSnapStall` (its
//! ms without progress), `NvSnapLen`, `NvSnapQ` (queue records), `NvSnapUs` (duration),
//! `NvSnapAuto` (automatic ones this boot), `NvSnapFail` (snapshots that could not allocate),
//! `NvSnapMsEff` (`NvSnapMs` in force).
//!
//! The registered queues are pinned (`hws::snap_slots`, DestroyHwQueue waits) only while
//! their own state, USERD and GPFIFO windows are copied; the pushbuffer reads, the decoding,
//! the VA lookups and the semaphore reads run after, by process key and GPU VA through
//! `native_memory::lookup_va` (read-only) and the KMD's own CPU views. Every memory read stops
//! after `READ_BUDGET_100NS`; no registry write happens anywhere but here.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::native_snap as ns;
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

use crate::adapter::AdapterContext;
use crate::ddi::hws;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::native_channel::{NativeQueue, QueueSnap};
use crate::virtio::native_memory::{self as nmem, VaHit};

/// The watch and the knobs run at most this often (ms).
const TICK_MS: u32 = 1_000;
/// The blob's room kept for the VA table.
const VA_RESERVE: usize = ns::VA_FIXED + ns::VA_TABLE_CAP * ns::VA_BYTES;
/// One fully detailed queue record at most.
const QUEUE_MAX_BYTES: usize = ns::QREC_FIXED
    + ns::MAX_SEGMENTS
        * (ns::SEG_FIXED
            + (ns::HEAD_DWORDS + ns::TAIL_DWORDS) * 4
            + ns::SEM_CAP * ns::SEM_BYTES
            + ns::REF_CAP * ns::REF_BYTES);
/// Dwords read per chunk while decoding a segment (one page).
const CHUNK_DWORDS: usize = 1024;

static WATCHES: SpinLock<[(usize, ns::StallWatch); hws::SNAP_SLOTS]> =
    SpinLock::new([(0, ns::StallWatch::new()); hws::SNAP_SLOTS]);
static LAST_TICK_MS: AtomicU32 = AtomicU32::new(0);
static SNAP_MS: AtomicU32 = AtomicU32::new(u32::MAX);
static SEQ: AtomicU32 = AtomicU32::new(0);
static AUTO_N: AtomicU32 = AtomicU32::new(0);
static LAST_AUTO_T: AtomicU64 = AtomicU64::new(0);
static FAILS: AtomicU32 = AtomicU32::new(0);
/// Retention: the longest stall an automatic snapshot has shown this boot (ms, `NvSnapLong`),
/// whether the first error-notifier snapshot was kept (`NvSnapErr`, with the automatic one
/// before it as `NvSnapPre`), and that last automatic snapshot.
static LONG_MS: AtomicU32 = AtomicU32::new(0);
static ERR_KEPT: AtomicU32 = AtomicU32::new(0);
static LAST_AUTO: SpinLock<Option<Vec<u8>>> = SpinLock::new(None);

/// The mirror pass's step (PASSIVE, on the mirror thread): knobs, the stall watch, and a
/// snapshot or a VA query when one is due. NVIDIA personality only.
pub(crate) fn publish(adapter: &AdapterContext) {
    if !crate::ddi::wddm_surface::nvidia_selected() {
        return;
    }
    let Some(passive) = PassiveLevel::try_assume() else {
        return;
    };
    let now_ms = AdapterContext::interrupt_time_ms().max(1);
    let last = LAST_TICK_MS.load(Ordering::Relaxed);
    if last != 0 && now_ms.wrapping_sub(last) < TICK_MS {
        return;
    }
    LAST_TICK_MS.store(now_ms, Ordering::Relaxed);
    let snap_ms = crate::diag::read_config_dword(
        crate::diag::KnobName::new(b"NvSnapMs"),
        ns::DEFAULT_SNAP_MS,
    );
    if SNAP_MS.swap(snap_ms, Ordering::Relaxed) != snap_ms {
        crate::diag::record_named_bytes(b"NvSnapMsEff", snap_ms);
    }
    let va_query = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSnapVa"), 0);
    if va_query != 0 && va_query != u32::MAX {
        query_va(passive, adapter, va_query);
        crate::diag::record_named_force(b"NvSnapVa", 0);
    }
    let manual = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvSnapNow"), 0);

    // The stall watch, over every registered queue.
    let Some(mut slots) = heap(hws::SNAP_SLOTS, hws::SNAP_EMPTY) else {
        return;
    };
    let n = hws::snap_slots(&mut slots, false);
    let now = crate::adapter::foreign_scanout::now_100ns();
    let timeout = ns::snap_timeout_100ns(snap_ms);
    let mut trigger: Option<(u32, usize)> = None;
    {
        let mut watches = WATCHES.lock();
        for s in &slots[..n] {
            let entry = &mut watches[s.index as usize % hws::SNAP_SLOTS];
            if entry.0 != s.queue {
                *entry = (s.queue, ns::StallWatch::new());
            }
            let sample = ns::StallSample {
                now,
                busy: busy(s),
                progress: s.progress,
                gp_get: s.gp_get,
                error: s.error,
            };
            let reason = match entry.1.tick(sample, timeout) {
                ns::Trigger::None => continue,
                ns::Trigger::Stall => ns::reason::AUTO,
                ns::Trigger::Error => ns::reason::ERROR,
            };
            if trigger.is_none() {
                trigger = Some((reason, s.queue));
            }
        }
        // Slots no longer registered forget their queue.
        for (i, entry) in watches.iter_mut().enumerate() {
            if entry.0 != 0 && !slots[..n].iter().any(|s| s.index as usize == i && s.queue == entry.0) {
                *entry = (0, ns::StallWatch::new());
            }
        }
    }
    if manual != 0 && manual != u32::MAX {
        crate::diag::record_named_force(b"NvSnapNow", 0);
        take(passive, adapter, ns::reason::MANUAL, 0, snap_ms);
        return;
    }
    if let Some((reason, queue)) = trigger {
        let last = LAST_AUTO_T.load(Ordering::Relaxed);
        if AUTO_N.load(Ordering::Relaxed) < ns::AUTO_MAX
            && (last == 0 || now.saturating_sub(last) >= ns::AUTO_MIN_GAP_100NS)
        {
            LAST_AUTO_T.store(now, Ordering::Relaxed);
            let auto = AUTO_N.fetch_add(1, Ordering::Relaxed) + 1;
            crate::diag::record_named_bytes(b"NvSnapAuto", auto);
            take(passive, adapter, reason, queue, snap_ms);
        }
    }
}

/// Work the progress fence does not show complete. (USERD GP_GET is not used: GB20x does not
/// write it back, so it reads 0 on every queue.)
fn busy(s: &hws::SnapSlot) -> bool {
    s.flags & ns::Q_OUTSTANDING != 0
}

fn stalled_ms(queue: usize, now: u64) -> u32 {
    WATCHES
        .lock()
        .iter()
        .find(|(q, _)| *q == queue)
        .map_or(0, |(_, w)| w.stalled_ms(now))
}

/// What phase 1 (queues pinned) copies of one queue.
struct Copied {
    slot: hws::SnapSlot,
    snap: QueueSnap,
    hw_tsg: u32,
}

/// The reads' shared context.
struct Reader<'a> {
    passive: PassiveLevel,
    adapter: &'a AdapterContext,
    deadline: u64,
    budget_hit: bool,
}

/// `out.len()` dwords at `va` of `process`'s space as the GPU fetches them (VidMm's system
/// pages through the KMD's mapping table, or the allocation's RM memory), for
/// `ddi::pg_check`. `ns::RD_*`. PASSIVE.
pub(crate) fn read_gpu_words(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    out: &mut [u32],
) -> u32 {
    let mut reader = Reader {
        passive,
        adapter,
        deadline: crate::adapter::foreign_scanout::now_100ns() + ns::READ_BUDGET_100NS,
        budget_hit: false,
    };
    reader.words(process, va, out)
}

impl Reader<'_> {
    /// Read `out.len()` dwords at `va` of `process`'s space, page by page. `RD_*`.
    fn words(&mut self, process: usize, va: u64, out: &mut [u32]) -> u32 {
        let mut done = 0usize;
        while done < out.len() {
            if crate::adapter::foreign_scanout::now_100ns() >= self.deadline {
                self.budget_hit = true;
                return ns::RD_BUDGET;
            }
            let addr = va + 4 * done as u64;
            let n = (out.len() - done).min(((0x1000 - (addr & 0xfff)) / 4) as usize);
            let status = self.page_part(process, addr, &mut out[done..done + n]);
            if status != ns::RD_OK {
                return status;
            }
            done += n;
        }
        ns::RD_OK
    }

    /// Dwords inside one page.
    fn page_part(&mut self, process: usize, addr: u64, out: &mut [u32]) -> u32 {
        let mut hits = [VaHit::default(); 1];
        if nmem::lookup_va(process, addr, &mut hits) == 0 {
            return ns::RD_NOT_MAPPED;
        }
        let hit = hits[0];
        if hit.kind == 2 || hit.kind == 3 {
            if hit.physical == 0 {
                return ns::RD_ERROR;
            }
            let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
            pa.QuadPart = (hit.physical & !0xfff) as i64;
            // SAFETY: a page VidMm keeps locked while it is mapped for the GPU (what
            // `patch_gpu_bytes` maps too); system RAM is cached; unmapped below.
            let view = unsafe { MmMapIoSpace(pa, 0x1000, _MEMORY_CACHING_TYPE::MmCached) } as *const u8;
            if view.is_null() {
                return ns::RD_ERROR;
            }
            let base = (hit.physical & 0xfff) as usize;
            for (i, v) in out.iter_mut().enumerate() {
                // SAFETY: `base + 4 * i` stays inside the mapped page (the caller splits at
                // page ends).
                *v = unsafe { (view.add(base + 4 * i) as *const u32).read_volatile() };
            }
            // SAFETY: the view mapped above.
            unsafe { MmUnmapIoSpace(view as *mut c_void, 0x1000) };
            return ns::RD_OK;
        }
        let offset = hit.memory_offset + (addr - hit.piece_va);
        let len = out.len() as u64 * 4;
        // SAFETY: PASSIVE; the closure copies at most `out.len()` dwords out.
        let read = unsafe {
            nmem::with_cpu_range(self.passive, self.adapter, hit.id, offset, len, |relative, native, got| {
                let at = relative as usize / 4;
                let k = (got as usize / 4).min(out.len().saturating_sub(at));
                for i in 0..k {
                    out[at + i] = (native as *const u32).add(i).read_volatile();
                }
                true
            })
        };
        match read {
            Ok(true) => ns::RD_OK,
            Ok(false) => ns::RD_NOT_MAPPED,
            Err(_) => ns::RD_ERROR,
        }
    }

    /// Fill a VA-table entry: the KMD's mapping there, the page-table ring's last operation,
    /// and the 8 (or 4) bytes at the VA.
    fn resolve(&mut self, e: &mut ns::VaEntry) {
        let process = e.process as usize;
        let mut hits = [VaHit::default(); 1];
        let found = nmem::lookup_va(process, e.va, &mut hits) != 0;
        fill_hit(e, found.then_some(&hits[0]));
        fill_touch(e, process);
        let mut words = [0u32; 2];
        let n = if e.va % 8 == 0 { 2 } else { 1 };
        e.value_status = if found { self.words(process, e.va & !3, &mut words[..n]) } else { ns::RD_NOT_MAPPED };
        if e.value_status == ns::RD_OK {
            e.value = u64::from(words[0]) | u64::from(words[1]) << 32;
        }
    }
}

fn fill_hit(e: &mut ns::VaEntry, hit: Option<&VaHit>) {
    let Some(h) = hit else {
        e.hit = ns::HIT_NONE;
        return;
    };
    e.process = h.process as u64;
    e.hit = match h.kind {
        1 => ns::HIT_NATIVE_RM,
        2 => ns::HIT_NATIVE_SYSTEM,
        _ => ns::HIT_FOREIGN,
    };
    e.id = h.id;
    e.memory = h.memory;
    e.piece_va = h.piece_va;
    e.piece_len = h.piece_len;
    e.memory_offset = h.memory_offset;
    e.size = h.size;
    let mut flags = 0;
    if let Some(pa) = h.resident {
        flags |= ns::V_RESIDENT_KNOWN;
        e.resident_pa = pa;
    }
    if h.poisoned {
        flags |= ns::V_POISONED;
    }
    if h.system_memory {
        flags |= ns::V_SYSTEM_MEMORY;
    }
    if h.huge {
        flags |= ns::V_HUGE;
    }
    e.flags |= flags;
}

fn fill_touch(e: &mut ns::VaEntry, process: usize) {
    let (record, table) = crate::ddi::pt_ring::last_touch(e.va, process);
    if let Some(r) = record {
        e.ring_kind = u32::from(r.kind());
        e.ring_t = r.t;
        e.ring_va = r.va;
        e.ring_len = r.len.min(u64::from(u32::MAX)) as u32;
    }
    if let Some((t, kind)) = table {
        e.flags |= ns::V_TOUCH_TABLE;
        e.touch_kind = u32::from(kind);
        if record.is_none() {
            e.ring_t = t;
        }
    }
}

fn heap<T: Clone>(n: usize, fill: T) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, fill);
    Some(v)
}

/// Take one snapshot and publish it. PASSIVE (the mirror thread).
fn take(passive: PassiveLevel, adapter: &AdapterContext, reason: u32, trigger: usize, snap_ms: u32) {
    let t0 = crate::adapter::foreign_scanout::now_100ns();
    let (Some(mut buf), Some(mut store), Some(mut words)) = (
        heap(ns::BLOB_CAP, 0u8),
        heap(ns::VA_TABLE_CAP, ns::VaEntry::default()),
        heap(CHUNK_DWORDS, 0u32),
    ) else {
        crate::diag::record_named_bytes(b"NvSnapFail", FAILS.fetch_add(1, Ordering::Relaxed) + 1);
        return;
    };
    let mut copied: Vec<Copied> = Vec::new();
    if copied.try_reserve_exact(hws::SNAP_SLOTS).is_err() {
        crate::diag::record_named_bytes(b"NvSnapFail", FAILS.fetch_add(1, Ordering::Relaxed) + 1);
        return;
    }
    let mut scanner: Vec<ns::Scanner> = Vec::new();
    if scanner.try_reserve_exact(1).is_err() {
        crate::diag::record_named_bytes(b"NvSnapFail", FAILS.fetch_add(1, Ordering::Relaxed) + 1);
        return;
    }
    scanner.push(ns::Scanner::new(ns::DEFAULT_CLASSES));
    let scanner = &mut scanner[0];

    // Phase 1: the queues pinned, their own state copied.
    let Some(mut slots) = heap(hws::SNAP_SLOTS, hws::SNAP_EMPTY) else {
        crate::diag::record_named_bytes(b"NvSnapFail", FAILS.fetch_add(1, Ordering::Relaxed) + 1);
        return;
    };
    let n = hws::snap_slots(&mut slots, true);
    for s in &slots[..n] {
        // SAFETY: pinned by `snap_slots(.., true)` until `snap_unpin`.
        let queue = unsafe { &*(s.queue as *const NativeQueue) };
        let snap = queue.snap();
        let hw_tsg = queue.context().map_or(0xffff, |c| c.hw_tsg_id(passive));
        copied.push(Copied { slot: *s, snap, hw_tsg });
    }
    hws::snap_unpin();

    // The trigger's queue first, then the busy ones, then the rest.
    copied.sort_unstable_by_key(|c| {
        if c.slot.queue == trigger && trigger != 0 {
            0
        } else if busy(&c.slot) || c.slot.error {
            1
        } else {
            2
        }
    });

    let now = crate::adapter::foreign_scanout::now_100ns();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let (submits, ticks, hang_ms) = hws::snap_counters();
    let trigger_token = copied
        .iter()
        .find(|c| c.slot.queue == trigger && trigger != 0)
        .map_or(0, |c| c.snap.token);
    let trigger_stall = if trigger != 0 { stalled_ms(trigger, now) } else { 0 };
    let mut reader = Reader {
        passive,
        adapter,
        deadline: t0 + ns::READ_BUDGET_100NS,
        budget_hit: false,
    };
    let mut table = ns::VaTable::new(&mut store);
    let mut w = ns::Writer::new(&mut buf);
    ns::write_header(&mut w, seq, reason, trigger_token, now, snap_ms, hang_ms, submits, ticks);
    let mut records = 0u32;
    let mut flags = 0u32;
    for c in copied.iter() {
        let room = w.room();
        if room < VA_RESERVE + ns::QREC_FIXED {
            flags |= ns::FLAG_TRUNCATED;
            break;
        }
        let full_room = room >= VA_RESERVE + QUEUE_MAX_BYTES;
        let detail = full_room
            && (reason == ns::reason::MANUAL || c.slot.queue == trigger || busy(&c.slot) || c.slot.error);
        if !full_room {
            flags |= ns::FLAG_TRUNCATED;
        }
        write_queue(&mut w, &mut reader, &mut table, scanner, &mut words, c, detail, c.slot.queue == trigger, now);
        records += 1;
    }
    // The VA table, each entry resolved now.
    for e in table.entries[..table.n].iter_mut() {
        reader.resolve(e);
    }
    if table.full {
        flags |= ns::FLAG_VA_TABLE_FULL;
    }
    ns::write_va_table(&mut w, table.used(), crate::ddi::pt_ring::recording());
    if reader.budget_hit {
        flags |= ns::FLAG_BUDGET;
    }
    if w.overflowed() {
        flags |= ns::FLAG_TRUNCATED;
    }
    let us = (crate::adapter::foreign_scanout::now_100ns().saturating_sub(t0) / 10).min(u64::from(u32::MAX)) as u32;
    ns::finish_header(&mut w, records, us, flags);
    let len = w.len();
    let blob = &buf[..len];
    crate::diag::record_named_binary(b"NvSnapBlob", blob);
    keep(reason, seq, blob, copied.iter().filter(|c| busy(&c.slot)).map(|c| stalled_ms(c.slot.queue, now)).max().unwrap_or(0));
    let ring = [b'N', b'v', b'S', b'n', b'a', b'p', b'B', b'0' + (seq % 4) as u8];
    crate::diag::record_named_binary(&ring, blob);
    crate::diag::record_named_bytes(b"NvSnapN", seq);
    crate::diag::record_named_bytes(b"NvSnapT", AdapterContext::interrupt_time_ms());
    crate::diag::record_named_bytes(b"NvSnapWhy", reason | ns::token_chid(trigger_token) << 8);
    crate::diag::record_named_bytes(b"NvSnapTok", trigger_token);
    crate::diag::record_named_bytes(b"NvSnapStall", trigger_stall);
    crate::diag::record_named_bytes(b"NvSnapLen", len as u32);
    crate::diag::record_named_bytes(b"NvSnapQ", records);
    crate::diag::record_named_bytes(b"NvSnapUs", us);
}

/// Keep the snapshots worth keeping beside the latest (`NvSnapBlob`) and the last four
/// (`NvSnapB0..3`), which a run of later triggers overwrites: `NvSnapLong` the automatic one
/// with the longest stall of a busy queue (`NvSnapLongMs`, `NvSnapLongN` its seq), the first
/// error-notifier one `NvSnapErr` (`NvSnapErrN`) and the automatic one taken last before it
/// `NvSnapPre` (`NvSnapPreN`): the state closest before RM put the channel in error.
fn keep(reason: u32, seq: u32, blob: &[u8], longest_ms: u32) {
    if reason == ns::reason::AUTO {
        if longest_ms > LONG_MS.load(Ordering::Relaxed) {
            LONG_MS.store(longest_ms, Ordering::Relaxed);
            crate::diag::record_named_binary(b"NvSnapLong", blob);
            crate::diag::record_named_bytes(b"NvSnapLongMs", longest_ms);
            crate::diag::record_named_bytes(b"NvSnapLongN", seq);
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(blob.len()).is_ok() {
            copy.extend_from_slice(blob);
            // seq in the copy's header is the blob's own.
            let old = LAST_AUTO.lock().replace(copy);
            drop(old);
        }
    }
    if reason == ns::reason::ERROR && ERR_KEPT.swap(1, Ordering::Relaxed) == 0 {
        crate::diag::record_named_binary(b"NvSnapErr", blob);
        crate::diag::record_named_bytes(b"NvSnapErrN", seq);
        let pre = LAST_AUTO.lock().take();
        if let Some(pre) = pre {
            crate::diag::record_named_binary(b"NvSnapPre", &pre);
            let pre_seq = pre.get(16..20).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            crate::diag::record_named_bytes(b"NvSnapPreN", pre_seq);
        }
    }
}

/// USERD GP_GET as a dword index.
const fn cp_gp_get() -> usize {
    (helios_kmd_logic::ce_present::USERD_GP_GET / 4) as usize
}

#[allow(clippy::too_many_arguments)]
fn write_queue(
    w: &mut ns::Writer<'_>,
    reader: &mut Reader<'_>,
    table: &mut ns::VaTable<'_>,
    scanner: &mut ns::Scanner,
    words: &mut [u32],
    c: &Copied,
    detail: bool,
    is_trigger: bool,
    now: u64,
) {
    let s = &c.snap;
    let process = s.process_key;
    let gp_get = s.userd[cp_gp_get()];
    let mut flags = c.slot.flags;
    for (on, bit) in [
        (s.graphics, ns::Q_GRAPHICS),
        (s.dead, ns::Q_DEAD),
        (detail, ns::Q_DETAIL),
        (c.slot.error, ns::Q_ERROR),
        (is_trigger, ns::Q_TRIGGER),
        (s.copy_bound, ns::Q_COPY_BOUND),
    ] {
        if on {
            flags |= bit;
        }
    }
    let entries = s.ring_entries.max(1);
    let history_kept = (s.history_n as usize).min(ns::HISTORY);
    let history = &s.history[..history_kept];
    let stuck = ns::stuck_submission(history, s.fence_now);
    if stuck.is_some() {
        flags |= ns::Q_FOCUS_FROM_HISTORY;
    }
    let (focus_start, focus_end) = ns::focus_range(history, s.fence_now, s.kmd_gp_put, entries);
    // The NVIDIA UMDs open their submissions with runs of NOP entries (all zero); the window
    // and the decoded segments start at the stuck submission's first real entry instead.
    let focus_start = if stuck.is_some() {
        let found = first_real_entry(reader, process, s.gpfifo_va, focus_start, focus_end, entries);
        ns::skip_nops(focus_start, focus_end, entries, found)
    } else {
        focus_start
    };
    let first = ns::window_first(focus_start, entries);
    let window_n = (ns::WINDOW as u32).min(entries);
    // The ring through its GPU VA (VidMm's system pages when the ring is a native
    // system-memory allocation): what the GPU fetches, not the allocation's RM object.
    let mut raw = [0u32; 2 * ns::WINDOW];
    let mut ring_status = ns::RD_OK;
    let mut done = 0u32;
    while done < window_n && ring_status == ns::RD_OK {
        let index = (first + done) % entries;
        let run = (window_n - done).min(entries - index);
        let at = 2 * done as usize;
        ring_status = reader.words(
            process,
            s.gpfifo_va + 8 * u64::from(index),
            &mut raw[at..at + 2 * run as usize],
        );
        done += run;
    }
    let mut window_words = [(0u32, 0u32); ns::WINDOW];
    for (i, e) in window_words.iter_mut().enumerate().take(window_n as usize) {
        *e = (raw[2 * i], raw[2 * i + 1]);
    }
    let mut last = [0u32; 2];
    let last_index = (s.kmd_gp_put % entries + entries - 1) % entries;
    let last_status = reader.words(process, s.gpfifo_va + 8 * u64::from(last_index), &mut last);
    let fields = ns::QueueFields {
        slot: c.slot.index,
        node: c.slot.node,
        pid: s.pid,
        flags,
        veid: s.veid,
        token: s.token,
        tsg: s.tsg,
        ctxshare: s.ctxshare,
        channel: s.channel,
        hw_tsg: c.hw_tsg,
        process: process as u64,
        context: s.context_key as u64,
        gpfifo_va: s.gpfifo_va,
        ring_entries: s.ring_entries,
        kmd_gp_put: s.kmd_gp_put,
        gp_get,
        userd_gp_put: s.userd[(helios_kmd_logic::ce_present::USERD_GP_PUT / 4) as usize],
        submitted: c.slot.submitted,
        signalled: c.slot.signalled,
        fence_now: s.fence_now,
        fence_gpu: s.fence_gpu,
        stall_ms: stalled_ms(c.slot.queue, now),
        notifier_info32: s.notifier_info32,
        notifier_word: s.notifier_word,
        ring_status,
        window_first: first,
        window_n,
        focus_start,
        focus_end,
        last_lo: last[0],
        last_hi: last[1],
        last_status,
        history_n: s.history_n,
    };
    let mut history_rec = [ns::Submission::default(); ns::HISTORY];
    history_rec[..history_kept].copy_from_slice(history);
    let mark = ns::begin_queue(w, &fields, &s.userd, &window_words, &history_rec);
    let mut window = [ns::GpEntry::default(); ns::WINDOW];
    if ring_status == ns::RD_OK {
        for (i, (lo, hi)) in window_words.iter().enumerate().take(window_n as usize) {
            window[i] = ns::gp_entry(*lo, *hi);
        }
    }
    // The ring and the progress fence are VAs worth a lookup too.
    let _ = table.intern(process as u64, s.gpfifo_va);
    let _ = table.intern(process as u64, s.fence_gpu);
    let mut segments = 0u32;
    if detail && ring_status == ns::RD_OK {
        let window = &window[..window_n as usize];
        let (picks, n) = ns::pick_segments(window, first, entries, focus_start, focus_end);
        // One decoder state over the segments in GPFIFO order (subchannel bindings
        // default to the D3D layout; SET_OBJECTs seen update them).
        *scanner = ns::Scanner::new(ns::DEFAULT_CLASSES);
        let span = (focus_end % entries + entries - focus_start % entries) % entries;
        for &p in &picks[..n] {
            let p = p as usize;
            let entry = window[p];
            let index = (first + p as u32) % entries;
            let into = (index + entries - focus_start % entries) % entries;
            let sflags = if into < span.max(1) {
                ns::S_FOCUS | if into == 0 { ns::S_FOCUS_FIRST } else { 0 }
            } else {
                ns::S_BEFORE
            };
            write_segment(w, reader, table, scanner, words, process, entry, index, sflags);
            segments += 1;
        }
    }
    ns::end_queue(w, mark, segments);
}

/// The offset from `focus_start` of the first non-NOP entry of `[focus_start, focus_end)`
/// (at most `ns::NOP_SCAN` entries read), `None` if all read are NOPs or a read failed.
fn first_real_entry(
    reader: &mut Reader<'_>,
    process: usize,
    gpfifo_va: u64,
    focus_start: u32,
    focus_end: u32,
    entries: u32,
) -> Option<u32> {
    if entries == 0 {
        return None;
    }
    let span = (focus_end % entries + entries - focus_start % entries) % entries;
    let scan = span.min(ns::NOP_SCAN);
    let mut buf = [0u32; 64];
    let mut k = 0u32;
    while k < scan {
        let index = (focus_start + k) % entries;
        let run = (scan - k).min(32).min(entries - index);
        let words = &mut buf[..2 * run as usize];
        if reader.words(process, gpfifo_va + 8 * u64::from(index), words) != ns::RD_OK {
            return None;
        }
        for i in 0..run {
            let e = ns::gp_entry(words[2 * i as usize], words[2 * i as usize + 1]);
            if !ns::is_nop(e) {
                return Some(k + i);
            }
        }
        k += run;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn write_segment(
    w: &mut ns::Writer<'_>,
    reader: &mut Reader<'_>,
    table: &mut ns::VaTable<'_>,
    scanner: &mut ns::Scanner,
    words: &mut [u32],
    process: usize,
    entry: ns::GpEntry,
    index: u32,
    sflags: u32,
) {
    let len = entry.dwords;
    let tail_start = len.saturating_sub(ns::TAIL_DWORDS as u32);
    scanner.begin_segment(tail_start);
    let mut head = [0u32; ns::HEAD_DWORDS];
    let n_head = (len as usize).min(ns::HEAD_DWORDS);
    let scan_len = len.min(ns::SCAN_MAX_DWORDS);
    let mut read_status = ns::RD_OK;
    let mut scanned = 0u32;
    let mut status = ns::scan_status::COMPLETE;
    'scan: while scanned < scan_len {
        let va = entry.va + 4 * u64::from(scanned);
        let n = ((scan_len - scanned) as usize)
            .min(words.len())
            .min(((0x1000 - (va & 0xfff)) / 4) as usize);
        let st = reader.words(process, va, &mut words[..n]);
        if st != ns::RD_OK {
            read_status = st;
            status = ns::scan_status::READ_FAILED;
            break;
        }
        for (i, &d) in words[..n].iter().enumerate() {
            let at = scanned as usize + i;
            if at < n_head {
                head[at] = d;
            }
            if !scanner.feed(d) {
                scanned += i as u32 + 1;
                status = if scanner.ended {
                    ns::scan_status::ENDED
                } else {
                    ns::scan_status::LOST
                };
                break 'scan;
            }
        }
        scanned += n as u32;
    }
    if status == ns::scan_status::COMPLETE && scan_len < len {
        status = ns::scan_status::CAPPED;
    }
    // Head dwords the scan stopped short of.
    let mut head_got = n_head.min(scanned as usize);
    if head_got < n_head && read_status == ns::RD_OK {
        let rest = &mut head[head_got..n_head];
        if reader.words(process, entry.va + 4 * head_got as u64, rest) == ns::RD_OK {
            head_got = n_head;
        }
    }
    let mut tail = [0u32; ns::TAIL_DWORDS];
    let n_tail = (len - tail_start) as usize;
    let tail_status = reader.words(process, entry.va + 4 * u64::from(tail_start), &mut tail[..n_tail]);
    if read_status == ns::RD_OK && tail_status != ns::RD_OK {
        read_status = tail_status;
    }
    let n_tail = if tail_status == ns::RD_OK { n_tail } else { 0 };
    let va_index = table.intern(process as u64, entry.va).map_or(u32::MAX, |(i, _)| i);
    let fields = ns::SegFields {
        index,
        va: entry.va,
        len,
        read_status,
        scanned,
        scan_status: status,
        lost_at: scanner.lost_at.unwrap_or(u32::MAX),
        sems_seen: scanner.sems_seen,
        n_sems: scanner.sems().len() as u32,
        tail_start,
        tail_aligned: scanner.tail_aligned,
        n_head: head_got as u32,
        n_tail: n_tail as u32,
        mme_calls: scanner.mme_calls,
        refs_seen: scanner.refs_seen,
        n_refs: scanner.refs().len() as u32,
        va_index,
        flags: sflags,
    };
    let mark = ns::begin_segment(w, &fields);
    for d in head[..head_got].iter().chain(tail[..n_tail].iter()) {
        w.u32(*d);
    }
    for sem in scanner.sems() {
        let i = table.intern(process as u64, sem.addr).map_or(u32::MAX, |(i, _)| i);
        ns::write_sem(w, sem, i);
    }
    for r in scanner.refs() {
        let i = table.intern(process as u64, r.va).map_or(u32::MAX, |(i, _)| i);
        ns::write_ref(w, r, i);
    }
    ns::end_segment(w, mark);
}

/// `NvSnapVa`: what is mapped at `page << 12` in any process. PASSIVE.
fn query_va(passive: PassiveLevel, adapter: &AdapterContext, page: u32) {
    let va = u64::from(page) << 12;
    let mut hits = [VaHit::default(); 8];
    let n = nmem::lookup_va(0, va, &mut hits);
    let mut store = [ns::VaEntry::default(); 8];
    let mut reader = Reader {
        passive,
        adapter,
        deadline: crate::adapter::foreign_scanout::now_100ns() + ns::READ_BUDGET_100NS,
        budget_hit: false,
    };
    let count = n.max(1);
    for (i, e) in store.iter_mut().enumerate().take(count) {
        *e = ns::VaEntry {
            va,
            process: if i < n { hits[i].process as u64 } else { 0 },
            value_status: ns::RD_NOT_READ,
            ..ns::VaEntry::default()
        };
        if i < n {
            fill_hit(e, Some(&hits[i]));
            fill_touch(e, hits[i].process);
            let mut words = [0u32; 2];
            e.value_status = reader.words(hits[i].process, va, &mut words);
            if e.value_status == ns::RD_OK {
                e.value = u64::from(words[0]) | u64::from(words[1]) << 32;
            }
        } else {
            e.hit = ns::HIT_NONE;
            fill_touch(e, 0);
        }
    }
    let mut buf = [0u8; ns::HDR_BYTES + ns::VA_FIXED + 8 * ns::VA_BYTES];
    let mut w = ns::Writer::new(&mut buf);
    let now = crate::adapter::foreign_scanout::now_100ns();
    let (submits, ticks, hang_ms) = hws::snap_counters();
    ns::write_header(
        &mut w,
        0,
        ns::reason::VA_QUERY,
        0,
        now,
        SNAP_MS.load(Ordering::Relaxed),
        hang_ms,
        submits,
        ticks,
    );
    ns::write_va_table(&mut w, &store[..count], crate::ddi::pt_ring::recording());
    ns::finish_header(&mut w, 0, 0, 0);
    let len = w.len();
    crate::diag::record_named_binary(b"NvSnapVaR", &buf[..len]);
    crate::diag::record_named_bytes(b"NvSnapVaQ", page);
    crate::diag::record_named_bytes(b"NvSnapVaN", n as u32);
    let first = &store[0];
    let resident = u32::from(first.flags & ns::V_RESIDENT_KNOWN != 0);
    crate::diag::record_named_bytes(b"NvSnapVaK", first.hit | resident << 8);
    crate::diag::record_named_bytes(b"NvSnapVaId", first.id);
}
