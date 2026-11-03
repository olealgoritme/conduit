//! Page-table operation ring (`NvPtRing`, REG_DWORD, default 0): the statics, the record entry
//! points and the publish. Layout and pure rules: `helios_kmd_logic::pt_ring` (host-tested).
//!
//! Every record site calls [`record`], which is one relaxed load while the knob is 0. With the
//! knob on a record is a `fetch_add` and six atomic stores into a ring of `pt::SLOTS` slots, plus
//! up to `pt::TOUCH_SPAN_MAX` stores into the last-touch table: legal at any IRQL (no lock, no
//! allocation).
//!
//! Published by the registry mirror's pass ([`publish`], PASSIVE): `NvPtRing` / `NvPtLast` at
//! most every [`PUBLISH_EVERY_MS`] while records arrive; after a native channel fault was seen
//! ([`fault`], from the HWS poll) the next pass writes `NvPtRingF` / `NvPtLastF` (the fault
//! snapshot; the table stops changing from the fault until then) and asks for that pass at
//! once. `NvPtOn` = the knob in force (re-read on every publish attempt, so it can be turned on
//! without a reboot), `NvPtHead` = records ever written (low 32 bits), `NvPtPubN` = snapshots,
//! `NvPtFaultN` = faults seen, `NvPtFaultT` = the ring index (head) at the last fault.

use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::pt_ring as pt;

#[allow(clippy::declare_interior_mutable_const)]
const Z64: AtomicU64 = AtomicU64::new(0);

static ON: AtomicU32 = AtomicU32::new(0);
static HEAD: AtomicU64 = AtomicU64::new(0);
static SEQ: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static TIME: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static VA: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static LEN: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static WORD: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static PROC: [AtomicU64; pt::SLOTS] = [Z64; pt::SLOTS];
static T_KEY: [AtomicU64; pt::TOUCH] = [Z64; pt::TOUCH];
static T_TIME: [AtomicU64; pt::TOUCH] = [Z64; pt::TOUCH];
static T_IDX: [AtomicU64; pt::TOUCH] = [Z64; pt::TOUCH];
/// The table keeps the state of the first fault until its snapshot is written.
static FROZEN: AtomicU32 = AtomicU32::new(0);
/// A fault snapshot is owed.
static FAULT_PENDING: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
static FAULT_HEAD: AtomicU64 = AtomicU64::new(0);
static LAST_PUB_MS: AtomicU32 = AtomicU32::new(0);
static LAST_PUB_HEAD: AtomicU64 = AtomicU64::new(0);
static PUB_N: AtomicU32 = AtomicU32::new(0);
/// The last `NvPgLeafWhy` (the reason a native leaf update was refused), for `K_UPT_OUT`.
static LAST_WHY: AtomicU32 = AtomicU32::new(0);

/// The shortest interval between two periodic snapshots, ms.
const PUBLISH_EVERY_MS: u32 = 2_000;

/// Whether operations are recorded.
#[inline(always)]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed) != 0
}

/// Record one operation on `[va, va + len)` of `process` (its key, 0 = none). Any IRQL.
#[inline]
pub(crate) fn record(kind: u8, flags: u8, detail: u16, aux: u32, va: u64, len: u64, process: usize) {
    if !on() {
        return;
    }
    let t = crate::adapter::foreign_scanout::now_100ns();
    let idx = HEAD.fetch_add(1, Ordering::Relaxed);
    let i = pt::slot(idx);
    SEQ[i].store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    TIME[i].store(t, Ordering::Relaxed);
    VA[i].store(va, Ordering::Relaxed);
    LEN[i].store(len, Ordering::Relaxed);
    WORD[i].store(pt::pack(kind, flags, detail, aux), Ordering::Relaxed);
    PROC[i].store(process as u64, Ordering::Relaxed);
    SEQ[i].store(idx + 1, Ordering::Release);
    if pt::touches(kind) && FROZEN.load(Ordering::Relaxed) == 0 {
        let tag = pt::touch_idx(idx + 1, kind);
        let (a, b) = pt::touch_runs(va, len);
        for (first, count) in [a, b] {
            for chunk in first..first + count {
                let e = pt::touch_index(chunk);
                T_KEY[e].store(chunk + 1, Ordering::Relaxed);
                T_TIME[e].store(t, Ordering::Relaxed);
                T_IDX[e].store(tag, Ordering::Relaxed);
            }
        }
    }
}

/// The reason the last native leaf update was refused (`NvPgLeafWhy`). Any IRQL.
#[inline]
pub(crate) fn note_why(why: u32) {
    if on() {
        LAST_WHY.store(why, Ordering::Relaxed);
    }
}

/// `K_UPT_OUT` of an UpdatePageTable that ran (`ok`; aux = the native id) or was refused
/// (aux = the `NvPgLeafWhy` noted meanwhile, 0 = none).
#[inline]
pub(crate) fn upt_done(ok: bool, id: u32, va: u64, len: u64, process: usize) {
    if on() {
        let why = LAST_WHY.swap(0, Ordering::Relaxed);
        let aux = if ok { id } else { why };
        record(pt::K_UPT_OUT, 0, u16::from(!ok), aux, va, len, process);
    }
}

/// A native channel fault was reported (the HWS poll, any IRQL up to DISPATCH): marked in the
/// ring, the table frozen until the fault snapshot, a mirror pass asked for.
pub(crate) fn fault(node: u32, cause: u32) {
    if !on() {
        return;
    }
    record(pt::K_FAULT, 0, node as u16, cause, 0, 0, 0);
    FROZEN.store(1, Ordering::Relaxed);
    FAULT_HEAD.store(HEAD.load(Ordering::Relaxed), Ordering::Relaxed);
    FAULTS.fetch_add(1, Ordering::Relaxed);
    FAULT_PENDING.store(1, Ordering::Release);
    crate::ddi::mirror_thread::request();
}

fn snapshot_ring(flags: u32) -> Option<alloc::vec::Vec<u8>> {
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    buf.try_reserve_exact(pt::SNAPSHOT_LEN).ok()?;
    buf.resize(pt::SNAPSHOT_LEN, 0);
    let head = HEAD.load(Ordering::Acquire);
    pt::encode_header(&mut buf, head, flags);
    for i in 0..pt::SLOTS {
        let seq = SEQ[i].load(Ordering::Acquire);
        let r = pt::Record {
            seq,
            t: TIME[i].load(Ordering::Relaxed),
            va: VA[i].load(Ordering::Relaxed),
            len: LEN[i].load(Ordering::Relaxed),
            w: WORD[i].load(Ordering::Relaxed),
            process: PROC[i].load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        // A slot a writer is in (or overwrote meanwhile) is published as never written.
        let r = if SEQ[i].load(Ordering::Relaxed) == seq { r } else { pt::Record::default() };
        pt::encode_slot(&mut buf, i, &r);
    }
    Some(buf)
}

fn snapshot_table(flags: u32) -> Option<alloc::vec::Vec<u8>> {
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    buf.try_reserve_exact(pt::TOUCH_SNAPSHOT_LEN).ok()?;
    buf.resize(pt::TOUCH_SNAPSHOT_LEN, 0);
    pt::encode_header(&mut buf, HEAD.load(Ordering::Acquire), flags);
    for e in 0..pt::TOUCH {
        pt::encode_touch(
            &mut buf,
            e,
            T_KEY[e].load(Ordering::Relaxed),
            T_TIME[e].load(Ordering::Relaxed),
            T_IDX[e].load(Ordering::Relaxed),
        );
    }
    Some(buf)
}

/// The mirror pass (PASSIVE): re-read the knob (at most every [`PUBLISH_EVERY_MS`]), write the
/// fault snapshot when one is owed, else the periodic one when records arrived.
pub(crate) fn publish() {
    let now = crate::adapter::AdapterContext::interrupt_time_ms().max(1);
    let pending = FAULT_PENDING.load(Ordering::Acquire) != 0;
    let last = LAST_PUB_MS.load(Ordering::Relaxed);
    if !pending && last != 0 && now.wrapping_sub(last) < PUBLISH_EVERY_MS {
        return;
    }
    LAST_PUB_MS.store(now, Ordering::Relaxed);
    let knob = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPtRing"), 0);
    let knob = u32::from(knob != 0 && knob != u32::MAX);
    if ON.swap(knob, Ordering::Relaxed) != knob {
        crate::diag::record_named_bytes(b"NvPtOn", knob);
    }
    if knob == 0 && !pending {
        return;
    }
    if pending {
        FAULT_PENDING.store(0, Ordering::Relaxed);
        let flags = pt::HDR_ON | pt::HDR_FAULT | pt::HDR_FROZEN;
        if let Some(ring) = snapshot_ring(flags) {
            crate::diag::record_named_binary(b"NvPtRingF", &ring);
        }
        if let Some(table) = snapshot_table(flags) {
            crate::diag::record_named_binary(b"NvPtLastF", &table);
        }
        FROZEN.store(0, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"NvPtFaultN", FAULTS.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"NvPtFaultT", FAULT_HEAD.load(Ordering::Relaxed) as u32);
    } else {
        let head = HEAD.load(Ordering::Acquire);
        if head == LAST_PUB_HEAD.swap(head, Ordering::Relaxed) {
            return;
        }
        if let Some(ring) = snapshot_ring(pt::HDR_ON) {
            crate::diag::record_named_binary(b"NvPtRing", &ring);
        }
        if let Some(table) = snapshot_table(pt::HDR_ON) {
            crate::diag::record_named_binary(b"NvPtLast", &table);
        }
    }
    PUB_N.fetch_add(1, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvPtPubN", PUB_N.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"NvPtHead", HEAD.load(Ordering::Relaxed) as u32);
}

/// `K_UPT_IN`: VidMm's `UpdatePageTable` as it arrived (any IRQL; reads its PTE array).
///
/// # Safety
/// `update` is dxgkrnl's live operation: `pPageTableEntries` holds `NumPageTableEntries`
/// entries (one with `Repeat`), or is null.
pub(crate) unsafe fn upt_in(update: &crate::dxgk::DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE, id: u32) {
    if !on() {
        return;
    }
    let count = update.NumPageTableEntries as usize;
    let repeat = update.Flags.Repeat() != 0;
    let mut flags = 0u8;
    if id != 0 {
        flags |= pt::UPT_NATIVE;
    }
    if update.PageTableLevel != 0 {
        flags |= pt::UPT_UPPER;
    }
    if repeat {
        flags |= pt::UPT_REPEAT;
    }
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { wdk_sys::ntddk::KeGetCurrentIrql() } != crate::ddi::PASSIVE_LEVEL_IRQL {
        flags |= pt::UPT_NOT_PASSIVE;
    }
    if update.hProcess.is_null()
        // SAFETY: dxgkrnl's hKmdProcess or null (checked).
        || unsafe { crate::device::ProcessContext::is_system_handle(update.hProcess as usize) }
    {
        flags |= pt::UPT_SYSTEM;
    }
    let mut segment = 0u16;
    if !update.pPageTableEntries.is_null() {
        let n = if repeat { count.min(1) } else { count };
        for index in 0..n {
            // SAFETY: the DDI contract above.
            let pte = unsafe { core::ptr::read_unaligned(update.pPageTableEntries.add(index)) };
            let bits = unsafe { pte.__bindgen_anon_1.__bindgen_anon_1 };
            if index == 0 {
                segment = bits.Segment() as u16;
            }
            if bits.Valid() != 0 {
                flags |= pt::UPT_ANY_VALID;
            } else {
                flags |= pt::UPT_ANY_INVALID;
            }
            if bits.Zero() != 0 {
                flags |= pt::UPT_ZERO;
            }
        }
    }
    // A non-leaf entry covers more than 4 KiB; the length is the leaf span all the same (the
    // UPT_UPPER flag says which).
    record(
        pt::K_UPT_IN,
        flags,
        segment,
        id,
        update.FirstPteVirtualAddress,
        count as u64 * 4096,
        update.hProcess as usize,
    );
}

/// Diagnosis (`ddi::native_snap`): the latest ring record that changed the mapping at `va`
/// (a kind `pt::touches` names) of `process` (0 = any; records without a process match any), and the
/// last-touch table's entry for its 64-KiB chunk: `(record, table (t, kind))`. Atomics only;
/// empty while the ring is off.
pub(crate) fn last_touch(va: u64, process: usize) -> (Option<pt::Record>, Option<(u64, u8)>) {
    if !on() {
        return (None, None);
    }
    let mut best: Option<pt::Record> = None;
    for i in 0..pt::SLOTS {
        let seq = SEQ[i].load(Ordering::Acquire);
        if seq == 0 || best.is_some_and(|b| b.seq >= seq) {
            continue;
        }
        let r = pt::Record {
            seq,
            t: TIME[i].load(Ordering::Relaxed),
            va: VA[i].load(Ordering::Relaxed),
            len: LEN[i].load(Ordering::Relaxed),
            w: WORD[i].load(Ordering::Relaxed),
            process: PROC[i].load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        if SEQ[i].load(Ordering::Relaxed) != seq {
            continue;
        }
        if pt::touches(r.kind())
            && r.covers(va)
            && (process == 0 || r.process == 0 || r.process == process as u64)
        {
            best = Some(r);
        }
    }
    let chunk = va >> pt::CHUNK_SHIFT;
    let e = pt::touch_index(chunk);
    let table = (T_KEY[e].load(Ordering::Relaxed) == chunk + 1).then(|| {
        (
            T_TIME[e].load(Ordering::Relaxed),
            (T_IDX[e].load(Ordering::Relaxed) >> 56) as u8,
        )
    });
    (best, table)
}

/// Whether the ring records (`NvPtRing`).
pub(crate) fn recording() -> bool {
    on()
}
