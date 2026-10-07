//! The producer's memory in the KMD's copy-engine client (milestone M3c-1): the I/O half of the
//! dup + map cache. The rules (the table, the slots' handles and VA windows, the map flags and
//! their fallback, the lengths, the counter names) are `helios_kmd_logic::ce_dup`; the design is
//! `docs/rm-copy-engine-present.md` 2.3, 11.2 and 14.
//!
//! [`dup_map_record`] makes the two objects a `'HEF3'` record names (the producer's timeline
//! semaphore memory and the presented image) usable by the channel: per object, once,
//!
//! 1. `NV_ESC_RM_DUP_OBJECT` (`NVOS55`): `hClient` = the channel's client, `hParent` = its device,
//!    `hObject` = the slot's handle, `hClientSrc` / `hObjectSrc` = the record's
//!    `h_client` / `h_memory`. RM (or the host) may refuse it: counted (`CeDupFail`, `CeDupStat`,
//!    and `CeRmCall` = `0x34 << 24 | object` / `CeRmStat`), returned as the error, never fatal;
//! 2. an `NV50_MEMORY_VIRTUAL` at the slot's fixed window (RM's choice if refused) and
//!    `MAP_MEMORY_DMA`: the semaphore snooped with 4 KiB pages, the image with the PTE kind the
//!    record's modifier names (`ce_present::source_plan`: 0x06 for block-linear), first with big
//!    pages as nvk-rm 0005 maps video memory, then as system memory (`ce_dup::map_tries`). A map
//!    that fails gives the dup back.
//!
//! The result is cached per `(client, memory)` for the life of the channel (bounded,
//! least recently used out: `ce_dup::Cache`), so a swap chain costs its few dups once.
//! [`release_all`] gives everything back, youngest first: the channel's teardown calls it before
//! the channel group when the GPU is idle, right after it when a copy may still be stuck (the
//! group's free stops the channel), and always before the client's files close.
//!
//! LOCKING. `CACHE` is a leaf spinlock over plain data (no I/O under it). Every RM message runs at
//! PASSIVE with no lock held, by the thread that holds the channel's `IO_BUSY` (the HPD worker in
//! shadow mode, StopDevice after the worker was joined): the cache's plan and its update are
//! therefore never raced.
//!
//! SECURITY. The record's clients are not checked against the presenting process yet
//! (`ddi/ce_record.rs`, `record_client_owned_by_presenter` answers "unknown"): a dup here is made
//! on trust. The shadow mode (`RmCopyEngine` = 3) accepts that and counts it (`CeRecClient`); the
//! route that owns a Present (M3c-2) must not call [`dup_map_record`] before the check answers.

use core::sync::atomic::{AtomicU32, Ordering};

use super::ce_channel::{self as ce, GpuMap, Handles};
use super::Io;
use crate::device::StashedCeRecord;
use crate::sync::SpinLock;
use helios_kmd_logic::ce_dup::{self as cd, Cache, Key, Plan, What};
use helios_kmd_logic::ce_present::{self as cp, Gen};
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind};

/// GPU virtual addresses, in the CE channel's VA space, of one record's producer objects.
#[derive(Clone, Copy)]
pub(crate) struct Producer {
    /// The semaphore ENTRY: the semaphore memory's mapping plus the record's offset.
    pub sem_va: u64,
    /// The image: its mapping plus the record's plane offset.
    pub src_va: u64,
}

static CACHE: SpinLock<Cache> = SpinLock::new(Cache::new());

static DUP_N: AtomicU32 = AtomicU32::new(0);
static DUP_OK: AtomicU32 = AtomicU32::new(0);
static DUP_FAIL: AtomicU32 = AtomicU32::new(0);
static DUP_STAT: AtomicU32 = AtomicU32::new(0);
static MAP_OK: AtomicU32 = AtomicU32::new(0);
static MAP_FAIL: AtomicU32 = AtomicU32::new(0);
static MAP_STAT: AtomicU32 = AtomicU32::new(0);
static MAP_FLAGS: AtomicU32 = AtomicU32::new(0);
static FREED: AtomicU32 = AtomicU32::new(0);
static EVICT: AtomicU32 = AtomicU32::new(0);

/// `Layout` failure codes of [`dup_map_record`] (no RM call was made).
const WHY_SOURCE: u32 = 0x70;
const WHY_SEMAPHORE_LEN: u32 = 0x71;
const WHY_SOURCE_LEN: u32 = 0x72;

/// Dup the record's semaphore memory and source image into the KMD's client (cached per
/// `(h_client, h_memory)`, bounded table, same lifetime as the channel), GPU-map them (the source
/// with the PTE kind from the record's modifier), and return the VAs of the semaphore ENTRY
/// (memory VA + record offset) and of the image base (+ record offset). Never fatal: a refusal is
/// counted and returned. PASSIVE, no lock held, the caller holds the channel's `IO_BUSY`; `h` is
/// the channel's client (`ce_channel::handles`), `gen` its class generation.
#[inline(never)]
pub(crate) fn dup_map_record(
    io: &Io<'_>,
    h: &Handles,
    rec: &StashedCeRecord,
    gen: Gen,
) -> Result<Producer, Fail> {
    let r = rec.record;
    let (sem, src) = (r.semaphore, r.source);
    let desc = source_desc(rec);
    let plan = cp::source_plan(gen, &desc)
        .map_err(|w| Fail::new(FailKind::Layout, WHY_SOURCE | (w.code() << 8)))?;
    let sem_len = cd::semaphore_map_len(sem.offset)
        .ok_or(Fail::new(FailKind::Layout, WHY_SEMAPHORE_LEN))?;
    let src_len = cd::source_map_len(src.size).ok_or(Fail::new(FailKind::Layout, WHY_SOURCE_LEN))?;
    let sem_map = slot_for(
        io,
        h,
        Key {
            client: sem.h_client,
            memory: sem.h_memory,
            what: What::Semaphore,
            kind: None,
            len: sem_len,
        },
    )?;
    let src_map = slot_for(
        io,
        h,
        Key {
            client: src.h_client,
            memory: src.h_memory,
            what: What::Source,
            kind: plan.page_kind,
            len: src_len,
        },
    )?;
    let sem_va = sem_map
        .checked_add(sem.offset)
        .ok_or(Fail::new(FailKind::Layout, WHY_SEMAPHORE_LEN))?;
    let src_va = src_map
        .checked_add(plan.offset)
        .ok_or(Fail::new(FailKind::Layout, WHY_SOURCE_LEN))?;
    Ok(Producer { sem_va, src_va })
}

/// The record's image as `ce_present::source_plan` reads it.
pub(crate) fn source_desc(rec: &StashedCeRecord) -> cp::SourceDesc {
    let src = rec.record.source;
    cp::SourceDesc {
        offset: src.offset,
        size: src.size,
        modifier: src.modifier,
        pitch: src.pitch,
        width: src.width,
        height: src.height,
        fourcc: src.fourcc,
        compressed: src.flags & helios_protocol::HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED != 0,
    }
}

/// Dups refused so far (`CeDupFail`): a caller tells a refused dup from a refused map by it.
pub(super) fn dup_failures() -> u32 {
    DUP_FAIL.load(Ordering::Relaxed)
}

/// The 64-bit value at `offset` of the record's semaphore memory as the KMD's dup sees it, through
/// a short-lived CPU view (the memory is system memory: a control file). For the diagnosis of a
/// copy that did not complete (`CeShadowSem`). `None` when the semaphore is not cached or the view
/// failed. PASSIVE, the caller holds the channel's `IO_BUSY`.
pub(super) fn read_semaphore(io: &Io<'_>, h: &Handles, rec: &StashedCeRecord) -> Option<u64> {
    let sem = rec.record.semaphore;
    let e = CACHE.lock().find(sem.h_client, sem.h_memory, What::Semaphore)?;
    if sem.offset.checked_add(8)? > e.key.len {
        return None;
    }
    let (h_dup, _) = cd::handles(e.slot);
    let mut v = ce::cpu_map(
        io,
        h,
        rc::H_DEVICE,
        h_dup,
        ce::SYSMEM,
        e.key.len,
        wdk_sys::_MEMORY_CACHING_TYPE::MmCached,
    )
    .ok()?;
    // SAFETY: `offset + 8 <= len` (checked), inside the view mapped just above; 8-aligned (the
    // record parser requires it).
    let value = unsafe { ce::rd64(v.va, sem.offset) };
    if !ce::cpu_unmap(io, h, &mut v, true) {
        ce::note_soft();
    }
    Some(value)
}

/// Whether `(client, memory)` is dup'd and mapped now (any role).
pub(crate) fn is_cached(client: u32, memory: u32) -> bool {
    CACHE.lock().is_cached(client, memory)
}

/// The mapping of `key`: the cached one, or a new dup + map in the slot the table picks (an
/// evicted slot is given back first: its handles and window are reused).
fn slot_for(io: &Io<'_>, h: &Handles, key: Key) -> Result<u64, Fail> {
    let plan = CACHE.lock().plan(&key);
    let slot = match plan {
        Plan::Hit(e) => return Ok(e.va),
        Plan::Make { slot, evict } => {
            if let Some(old) = evict {
                CACHE.lock().remove(old.slot);
                EVICT.fetch_add(1, Ordering::Relaxed);
                give_back(io, h, old, true);
            }
            slot
        }
    };
    let (h_dup, h_virt) = cd::handles(slot);
    DUP_N.fetch_add(1, Ordering::Relaxed);
    if let Err(f) = dup(io, h, h_dup, key.client, key.memory) {
        DUP_FAIL.fetch_add(1, Ordering::Relaxed);
        DUP_STAT.store(cc::fail_word(f), Ordering::Relaxed);
        ce::note_call(cc::ESC_RM_DUP_OBJECT, key.memory, f);
        ce::note_rm_error();
        return Err(f);
    }
    DUP_OK.fetch_add(1, Ordering::Relaxed);
    let (first, second) = cd::map_tries(key.what, key.kind);
    let va = cd::slot_va(slot);
    let mut used = first;
    let mut mapped = ce::gpu_map_with(io, h, h_virt, h_dup, va, key.len, first.flags, first.kind);
    if let (Err(f), Some(t)) = (mapped, second) {
        if f.kind == FailKind::Rm {
            used = t;
            mapped = ce::gpu_map_with(io, h, h_virt, h_dup, va, key.len, t.flags, t.kind);
        }
    }
    match mapped {
        Ok(g) => {
            MAP_OK.fetch_add(1, Ordering::Relaxed);
            if key.what == What::Source {
                MAP_FLAGS.store(cd::map_word(used), Ordering::Relaxed);
            }
            CACHE.lock().insert(slot, key, g.va, used.flags);
            Ok(g.va)
        }
        Err(f) => {
            MAP_FAIL.fetch_add(1, Ordering::Relaxed);
            MAP_STAT.store(cc::fail_word(f), Ordering::Relaxed);
            ce::note_rm_error();
            // The dup alone goes back (the failed map gave back its own virtual allocation).
            if ce::rm_free(io, h, rc::H_DEVICE, h_dup).is_err() {
                ce::note_soft();
            }
            Err(f)
        }
    }
}

/// `NV_ESC_RM_DUP_OBJECT`: `object` of `client` as `h_new` of the channel's client, under its
/// device.
fn dup(io: &Io<'_>, h: &Handles, h_new: u32, client: u32, object: u32) -> Result<(), Fail> {
    let block = cc::nvos55(h.root, rc::H_DEVICE, h_new, client, object);
    let mut resp = [0u8; super::REPLY_MAX];
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(cc::ESC_RM_DUP_OBJECT, cc::NVOS55_BYTES as u32),
        &block,
        &[],
        &mut resp,
    )?;
    rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x73))?,
        cc::NVOS55_STATUS_AT,
    )
    .map(|_| ())
    .map_err(Fail::from)
}

/// Give one slot back: its GPU mapping (`UNMAP_MEMORY_DMA` + the virtual allocation's free), then
/// the dup. `send = false` sends nothing (StopDevice's flag is up or the deadline is spent: the
/// client's close takes them).
fn give_back(io: &Io<'_>, h: &Handles, e: cd::Entry, send: bool) {
    FREED.fetch_add(1, Ordering::Relaxed);
    if !send {
        return;
    }
    let (h_dup, h_virt) = cd::handles(e.slot);
    let g = GpuMap {
        virt: h_virt,
        mem: h_dup,
        va: e.va,
        len: e.key.len,
    };
    let mut ok = ce::gpu_unmap(io, h, &g);
    ok &= ce::rm_free(io, h, rc::H_DEVICE, h_dup).is_ok();
    if !ok {
        ce::note_soft();
    }
}

/// Drop every cached dup and mapping (StopDevice / channel teardown / strikes), reverse order of
/// making. PASSIVE, no lock held, the caller holds the channel's `IO_BUSY` (or the worker was
/// joined). A no-op when nothing is cached.
#[inline(never)]
pub(crate) fn release_all(io: &Io<'_>, h: &Handles) {
    loop {
        let Some(e) = CACHE.lock().take_youngest() else {
            break;
        };
        let send = !io.stopping() && !io.limit_spent();
        give_back(io, h, e, send);
    }
}

/// The transport is gone (`ce_channel::forget`): the sweep closed the client and everything in it.
pub(super) fn forget() {
    CACHE.lock().clear();
}

/// A new transport generation with the channel enabled (StartDevice, PASSIVE): zero the counters
/// and their service-key values.
pub(super) fn reset_for_start() {
    for c in [
        &DUP_N, &DUP_OK, &DUP_FAIL, &DUP_STAT, &MAP_OK, &MAP_FAIL, &MAP_STAT, &MAP_FLAGS, &FREED,
        &EVICT,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    publish(true);
}

/// Mirror the counters (PASSIVE), once a dup was asked for.
pub(super) fn publish_counters() {
    publish(false);
}

fn publish(always: bool) {
    if !always && DUP_N.load(Ordering::Relaxed) == 0 {
        return;
    }
    let live = cd::live_word(CACHE.lock().live());
    use crate::diag::record_named_bytes as rec;
    rec(b"CeDupN", DUP_N.load(Ordering::Relaxed));
    rec(b"CeDupOk", DUP_OK.load(Ordering::Relaxed));
    rec(b"CeDupFail", DUP_FAIL.load(Ordering::Relaxed));
    rec(b"CeDupStat", DUP_STAT.load(Ordering::Relaxed));
    rec(b"CeMapOk", MAP_OK.load(Ordering::Relaxed));
    rec(b"CeMapFail", MAP_FAIL.load(Ordering::Relaxed));
    rec(b"CeMapStat", MAP_STAT.load(Ordering::Relaxed));
    rec(b"CeMapFlags", MAP_FLAGS.load(Ordering::Relaxed));
    rec(b"CeDupLive", live);
    rec(b"CeDupFree", FREED.load(Ordering::Relaxed));
    rec(b"CeDupEvict", EVICT.load(Ordering::Relaxed));
}
