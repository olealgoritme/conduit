//! `NvPgCheck` (default 0): read-back checks of native paging operations and of submitted
//! GPFIFO entries. The rules and the counter names are `helios_kmd_logic::pg_check`.
//!
//! * `0x1`: after every native TRANSFER / FILL / VIRTUAL_FILL / VIRTUAL_TRANSFER that ran,
//!   sampled pieces of both ends are compared ([`check_transfer`], [`check_fill`]): the system
//!   pages through a cached view of their frames, the RM memory through
//!   `native_memory::with_cpu_range`. `PgChkN`, `PgChkBad`, `PgChkLast` (`op_word`),
//!   `PgChkId`, `PgChkOff`, `PgChkSkip` (pieces that could not be read).
//! * `0x2` (or `0x4`): after every native `SubmitCommandToHwQueue`, the last entries of the
//!   submission are read through the GPU-side mapping ([`check_submit`]): `HwsGpChk`,
//!   `HwsZeroGp`, `HwsZeroGpE`, `HwsZeroGpLast`, `HwsZeroGpAll`, `HwsZeroGpOp`,
//!   `HwsZeroGpVa`, `HwsZeroGpRd` (reads that failed), `HwsGpChkIrql` (above PASSIVE: skipped).
//!
//! Whatever the knob, every native allocation's last paging operation and placement is kept
//! only while the knob is on (`LAST`, a direct-mapped table), for `HwsZeroGpOp`.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::pg_check as pc;
use wdk_sys::ntddk::{KeGetCurrentIrql, MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

use crate::adapter::AdapterContext;
use crate::ddi::PASSIVE_LEVEL_IRQL;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;

const UNREAD: u32 = u32::MAX;
static KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static SEQ: AtomicU32 = AtomicU32::new(0);
static LAST: SpinLock<pc::LastOps<1024>> = SpinLock::new(pc::LastOps::new());

static CHK_N: AtomicU32 = AtomicU32::new(0);
static CHK_BAD: AtomicU32 = AtomicU32::new(0);
static CHK_LAST: AtomicU32 = AtomicU32::new(0);
static CHK_ID: AtomicU32 = AtomicU32::new(0);
static CHK_OFF: AtomicU32 = AtomicU32::new(0);
static CHK_SKIP: AtomicU32 = AtomicU32::new(0);
static GP_CHK: AtomicU32 = AtomicU32::new(0);
static GP_ZERO: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_E: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_LAST: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_ALL: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_OP: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_VA: AtomicU32 = AtomicU32::new(0);
static GP_ZERO_RD: AtomicU32 = AtomicU32::new(0);
static GP_IRQL: AtomicU32 = AtomicU32::new(0);

/// The knob: read once at PASSIVE; above PASSIVE an unread knob is off.
pub(crate) fn knob() -> pc::Knob {
    let value = KNOB.load(Ordering::Relaxed);
    if value != UNREAD {
        return pc::Knob(value);
    }
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { KeGetCurrentIrql() } != PASSIVE_LEVEL_IRQL {
        return pc::Knob(0);
    }
    let read = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvPgCheck"), 0);
    let read = if read == UNREAD { 0 } else { read };
    KNOB.store(read, Ordering::Relaxed);
    pc::Knob(read)
}

/// The paging sequence number (low 16 bits go into `op_word`).
fn seq() -> u32 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Record native allocation `id`'s placement after a leaf update. Any IRQL.
pub(crate) fn note_place(id: u32, place: u32) {
    if !knob().any() {
        return;
    }
    LAST.lock().note_place(id, place);
}

/// Record native allocation `id`'s last content operation. Any IRQL.
pub(crate) fn note_op(id: u32, op: u32, dir: u32, path: u32) {
    if !knob().any() {
        return;
    }
    let word = pc::op_word(op, pc::place::UNKNOWN, dir, path, seq());
    LAST.lock().note_op(id, word);
}

/// The allocation is gone: its slot may be taken.
pub(crate) fn forget(id: u32) {
    if KNOB.load(Ordering::Relaxed) == UNREAD || KNOB.load(Ordering::Relaxed) == 0 {
        return;
    }
    LAST.lock().forget(id);
}

fn last_word(id: u32) -> u32 {
    LAST.lock().get(id).unwrap_or(0)
}

fn bad(op_word: u32, id: u32, offset: u64) {
    CHK_BAD.fetch_add(1, Ordering::Relaxed);
    CHK_LAST.store(op_word, Ordering::Relaxed);
    CHK_ID.store(id, Ordering::Relaxed);
    CHK_OFF.store(offset as u32, Ordering::Relaxed);
}

/// Copy `out.len()` bytes of the system page `pfn` from byte `at` (inside the page).
fn read_system(pfn: u64, at: u64, out: &mut [u8]) -> bool {
    if at + out.len() as u64 > pc::PAGE || pfn == 0 || pfn > (u64::MAX >> 12) {
        return false;
    }
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = (pfn << 12) as i64;
    // SAFETY: a page VidMm keeps locked for the paging operation (or while it is mapped for
    // the GPU); system RAM is cached; unmapped below.
    let view = unsafe { MmMapIoSpace(pa, pc::PAGE, _MEMORY_CACHING_TYPE::MmCached) } as *const u8;
    if view.is_null() {
        return false;
    }
    for (i, b) in out.iter_mut().enumerate() {
        // SAFETY: inside the mapped page (checked above).
        *b = unsafe { view.add(at as usize + i).read_volatile() };
    }
    // SAFETY: the view mapped above.
    unsafe { MmUnmapIoSpace(view as *mut core::ffi::c_void, pc::PAGE) };
    true
}

/// Copy `out.len()` bytes of native allocation `id`'s RM memory from byte `offset`.
fn read_native(passive: PassiveLevel, adapter: &AdapterContext, id: u32, offset: u64, out: &mut [u8]) -> bool {
    let len = out.len() as u64;
    // SAFETY: PASSIVE; the closure copies at most `out.len()` bytes out.
    let read = unsafe {
        crate::virtio::native_memory::with_cpu_range(passive, adapter, id, offset, len, |relative, native, got| {
            let at = relative as usize;
            let n = (got as usize).min(out.len().saturating_sub(at));
            for i in 0..n {
                out[at + i] = native.add(i).read_volatile();
            }
            true
        })
    };
    matches!(read, Ok(true))
}

/// `NvPgCheck` 0x1: after a transfer of `size` bytes between native allocation `id` at byte
/// `offset` and the system pages `pfns` (the operation's first byte `start` bytes into
/// `pfns[0]`), compare sampled pieces of both ends. `op` / `dir` / `path`: `pc::op`, `pc::dir`,
/// `pc::path`. PASSIVE (BuildPagingBuffer).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_transfer(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    size: u64,
    pfns: &[u64],
    start: u64,
    op: u32,
    dir: u32,
    path: u32,
) {
    if !knob().paging() {
        return;
    }
    let (pieces, n) = pc::pieces(size, start);
    let word = pc::with_place(pc::op_word(op, pc::place::UNKNOWN, dir, path, SEQ.load(Ordering::Relaxed)), pc::word_place(last_word(id)));
    for piece in &pieces[..n] {
        let at = start + piece.rel;
        let Some(&pfn) = pfns.get((at / pc::PAGE) as usize) else {
            CHK_SKIP.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let mut sys = [0u8; pc::PIECE as usize];
        let mut nat = [0u8; pc::PIECE as usize];
        let len = piece.len as usize;
        if !read_system(pfn, at % pc::PAGE, &mut sys[..len])
            || !read_native(passive, adapter, id, offset + piece.rel, &mut nat[..len])
        {
            CHK_SKIP.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        CHK_N.fetch_add(1, Ordering::Relaxed);
        if pc::checksum(&sys[..len]) != pc::checksum(&nat[..len]) {
            bad(word, id, offset + piece.rel);
        }
    }
}

/// `NvPgCheck` 0x1: after a fill of `size` bytes of native allocation `id` from byte `offset`
/// with `pattern`, check sampled pieces of its RM memory. PASSIVE.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_fill(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
    offset: u64,
    size: u64,
    pattern: u32,
    op: u32,
    path: u32,
) {
    if !knob().paging() {
        return;
    }
    let (pieces, n) = pc::pieces(size, offset % pc::PAGE);
    let word = pc::with_place(
        pc::op_word(op, pc::place::UNKNOWN, pc::dir::NONE, path, SEQ.load(Ordering::Relaxed)),
        pc::word_place(last_word(id)),
    );
    for piece in &pieces[..n] {
        let mut nat = [0u8; pc::PIECE as usize];
        let len = piece.len as usize;
        if !read_native(passive, adapter, id, offset + piece.rel, &mut nat[..len]) {
            CHK_SKIP.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        CHK_N.fetch_add(1, Ordering::Relaxed);
        if !pc::fill_matches(&nat[..len], piece.rel, pattern) {
            bad(word, id, offset + piece.rel);
        }
    }
}

/// `NvPgCheck` 0x2: the queue's last submission read back through the GPU-side mapping. Called
/// after a successful native `SubmitCommandToHwQueue`; PASSIVE only (counted otherwise).
pub(crate) fn check_submit(adapter: &AdapterContext, queue: &crate::virtio::native_channel::NativeQueue) {
    if !knob().submit() {
        return;
    }
    // SAFETY: KeGetCurrentIrql is callable at any IRQL.
    if unsafe { KeGetCurrentIrql() } != PASSIVE_LEVEL_IRQL {
        GP_IRQL.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: checked immediately above.
    let passive = unsafe { PassiveLevel::assume() };
    let Some(view) = queue.check_view() else {
        return;
    };
    let Some(runs) = pc::gp_runs(view.before, view.after, view.entries) else {
        return;
    };
    let mut words = [0u32; 2 * pc::MAX_GP_CHECK as usize];
    let mut n = 0usize;
    for (start, count) in runs {
        if count == 0 {
            continue;
        }
        let out = &mut words[2 * n..2 * (n + count as usize)];
        let status = crate::ddi::native_snap::read_gpu_words(
            passive,
            adapter,
            view.process_key,
            view.gpfifo_va + 8 * u64::from(start),
            out,
        );
        if status != helios_kmd_logic::native_snap::RD_OK {
            GP_ZERO_RD.fetch_add(1, Ordering::Relaxed);
            return;
        }
        n += count as usize;
    }
    GP_CHK.fetch_add(1, Ordering::Relaxed);
    let v = pc::gp_verdict(&words[..2 * n]);
    if v.zero == 0 {
        return;
    }
    GP_ZERO.fetch_add(1, Ordering::Relaxed);
    GP_ZERO_E.fetch_add(v.zero, Ordering::Relaxed);
    if v.last_zero || v.all_zero {
        if v.last_zero {
            GP_ZERO_LAST.fetch_add(1, Ordering::Relaxed);
        }
        if v.all_zero {
            GP_ZERO_ALL.fetch_add(1, Ordering::Relaxed);
        }
        GP_ZERO_OP.store(last_word(view.ring_id), Ordering::Relaxed);
        let last = (view.after % view.entries.max(1) + view.entries - 1) % view.entries.max(1);
        GP_ZERO_VA.store((view.gpfifo_va + 8 * u64::from(last)) as u32, Ordering::Relaxed);
    }
}

/// The counters, from the mirror pass (PASSIVE). Nothing while the knob is off.
pub(crate) fn publish() {
    let k = KNOB.load(Ordering::Relaxed);
    if k == UNREAD || k == 0 {
        return;
    }
    let rec = crate::diag::record_named_bytes;
    let r = |a: &AtomicU32| a.load(Ordering::Relaxed);
    rec(b"NvPgCheckOn", k);
    if pc::Knob(k).paging() {
        rec(b"PgChkN", r(&CHK_N));
        rec(b"PgChkBad", r(&CHK_BAD));
        rec(b"PgChkLast", r(&CHK_LAST));
        rec(b"PgChkId", r(&CHK_ID));
        rec(b"PgChkOff", r(&CHK_OFF));
        rec(b"PgChkSkip", r(&CHK_SKIP));
    }
    if pc::Knob(k).submit() {
        rec(b"HwsGpChk", r(&GP_CHK));
        rec(b"HwsZeroGp", r(&GP_ZERO));
        rec(b"HwsZeroGpE", r(&GP_ZERO_E));
        rec(b"HwsZeroGpLast", r(&GP_ZERO_LAST));
        rec(b"HwsZeroGpAll", r(&GP_ZERO_ALL));
        rec(b"HwsZeroGpOp", r(&GP_ZERO_OP));
        rec(b"HwsZeroGpVa", r(&GP_ZERO_VA));
        rec(b"HwsZeroGpRd", r(&GP_ZERO_RD));
        rec(b"HwsGpChkIrql", r(&GP_IRQL));
    }
}
