//! "Adapter-wide device removed" instrument: the I/O half. The rules, the rings and the sticky
//! first-fatal record are `helios_kmd_logic::device_lost` (host-tested); this file keeps the
//! statics, takes the clock and the thread id, and writes the registry. Design, the counters to
//! read, in order, and what each means: `docs/zero-copy-present.md`, "Adapter-wide device
//! removed".
//!
//! WHAT IT IS. A tester saw every D3D device on the adapter get `D3DDDIERR_DEVICEREMOVED` with no
//! TDR event, no dump and no device restart. The KMD answers `STATUS_SUCCESS` from nearly every
//! DDI by design, so the interesting questions are: did ANY DDI answer something else (and which
//! first), was a DDI inside the driver for a very long time (a paging operation or a teardown
//! blocked behind a host round trip), and did the paging path do anything unusual to a primary.
//! Nothing here changes what the driver answers or when.
//!
//! IRQL. Everything that records is atomics and a clock read, legal at any IRQL (the wrapped
//! DDIs include `SetVidPnSourceAddress` at DIRQL, the submit DDIs at DISPATCH). The registry is
//! written only by [`publish_block`], at PASSIVE, and EVERY call site names its [`Trigger`]
//! there: `stall_diag::publish_counters` (the periodic mirror, the stuck-only escape publisher
//! and the StartDevice zero write: `Periodic`), `StopDevice` (`Stop`), and the `DestroyDevice`
//! wrapper (`Teardown`). The block is about 75 values and up to 96 ring entries, so the trigger
//! decides whether it is worth writing: a teardown DDI writes it only for a suspect, fatal or
//! slow event (`serious_dirty`), never because an expected refusal moved a ring. One function,
//! so the publisher can be redirected to another thread by changing only [`publish_block`].

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::device_lost::{
    self as dl, ddi, EventRing, FirstFatal, InFlight, Longest, PagingKind, Verdict,
};
use helios_kmd_logic::vsync_rate::{ms_from_100ns, UNITS_PER_MS};

use crate::dxgk::NTSTATUS;

#[link(name = "ntoskrnl")]
extern "system" {
    fn PsGetCurrentThreadId() -> *mut c_void;
}

/// The calling thread's id, for the first-fatal record and the Venus mutex's longest holder.
fn thread_id() -> u32 {
    // SAFETY: a scalar read of the current thread's cid; callable at any IRQL.
    (unsafe { PsGetCurrentThreadId() } as usize) as u32
}

fn irql() -> u32 {
    // SAFETY: callable at any IRQL.
    (unsafe { wdk_sys::ntddk::KeGetCurrentIrql() }) as u32
}

fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

// ---- the DDI wrappers' state ----------------------------------------------------------------

/// The last 16 non-success returns, the last 8 outside the DDI's expected set, the last 8 calls
/// that took 250 ms or more.
static FAILS: EventRing<{ dl::FAIL_RING_LEN }> = EventRing::new();
static SUSPECTS: EventRing<{ dl::SUSPECT_RING_LEN }> = EventRing::new();
static SLOW: EventRing<{ dl::SLOW_RING_LEN }> = EventRing::new();
/// The first status of the verdict `Fatal`, for the image's lifetime. First wins.
static FIRST: FirstFatal = FirstFatal::new();
static INFLIGHT: InFlight = InFlight::new();
/// The longest wrapped call: milliseconds, the DDI id, when it ended.
static LONGEST_CALL: Longest = Longest::new();
static SUSPECT_N: AtomicU32 = AtomicU32::new(0);
static SLOW_N: AtomicU32 = AtomicU32::new(0);

const ZERO: AtomicU32 = AtomicU32::new(0);
/// Calls per DDI id (`N*`), and the time of the last call of a few (`T*`).
static CALLS: [AtomicU32; ddi::MAX] = [ZERO; ddi::MAX];
static LAST_CALL_T: [AtomicU32; ddi::MAX] = [ZERO; ddi::MAX];

/// A wrapped DDI is about to run. Returns the start time (100 ns) for [`leave`]. Any IRQL.
pub(crate) fn enter(id: u32) -> u64 {
    let now = now_100ns();
    let ms = ms_from_100ns(now);
    INFLIGHT.enter(id, ms);
    if let Some(c) = CALLS.get(id as usize) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(t) = LAST_CALL_T.get(id as usize) {
        t.store(ms, Ordering::Relaxed);
    }
    now
}

/// The wrapped DDI `id` returned `status`. `started` is [`enter`]'s value; `hint` is whatever
/// names the object (a handle's low bits, the escape code, the power state). Any IRQL.
pub(crate) fn leave(id: u32, started: u64, status: NTSTATUS, hint: u32) {
    let now = now_100ns();
    let now_ms = ms_from_100ns(now);
    // Recorded BEFORE this call leaves the in-flight set, so the first-fatal record names the
    // DDI that failed among the ones that were running.
    if status != 0 {
        note_failure(id, status, hint, now_ms);
    }
    INFLIGHT.leave(id);
    let took_ms = (now.saturating_sub(started) / UNITS_PER_MS).min(u32::MAX as u64) as u32;
    if took_ms >= dl::slow_call_ms(id) {
        SLOW_N.fetch_add(1, Ordering::Relaxed);
        SLOW.push(took_ms, dl::pack_ddi_hint(id, hint), now_ms);
        LONGEST_CALL.note(took_ms, id, now_ms);
    }
}

/// A status a callback of dxgkrnl's gave the KMD (`DxgkCbIndicateChildStatus`, the DMA-completion
/// notify): the same rings and the same first-fatal rule, with no call to time. Any IRQL.
pub(crate) fn note_cb(id: u32, status: NTSTATUS, hint: u32) {
    if status != 0 {
        note_failure(id, status, hint, ms_from_100ns(now_100ns()));
    }
}

fn note_failure(id: u32, status: NTSTATUS, hint: u32, now_ms: u32) {
    let packed = dl::pack_ddi_hint(id, hint);
    // The routine refusals of the query DDIs stay out of the ring (they would push out the
    // entries that matter); everything else, expected or not, goes in.
    let seq = if dl::ring_worthy(id, status) {
        FAILS.push(status as u32, packed, now_ms)
    } else {
        FAILS.count()
    };
    match dl::verdict(id, status) {
        Verdict::Ok | Verdict::Expected => {}
        Verdict::Suspect => {
            SUSPECT_N.fetch_add(1, Ordering::Relaxed);
            SUSPECTS.push(status as u32, packed, now_ms);
        }
        Verdict::Fatal => {
            SUSPECT_N.fetch_add(1, Ordering::Relaxed);
            SUSPECTS.push(status as u32, packed, now_ms);
            FIRST.note(|f| {
                let (lo, hi) = INFLIGHT.mask();
                f.ddi.store(id, Ordering::Relaxed);
                f.status.store(status as u32, Ordering::Relaxed);
                f.t.store(now_ms, Ordering::Relaxed);
                f.thread.store(thread_id(), Ordering::Relaxed);
                f.hint.store(hint, Ordering::Relaxed);
                f.irql.store(irql(), Ordering::Relaxed);
                f.seq.store(seq, Ordering::Relaxed);
                f.inflight_lo.store(lo, Ordering::Relaxed);
                f.inflight_hi.store(hi, Ordering::Relaxed);
            });
        }
    }
}

// ---- the paging operations ------------------------------------------------------------------

static PG_LAST_OP: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
static PG_LAST_RES: AtomicU32 = AtomicU32::new(0);
static PG_LAST_AL: AtomicU32 = AtomicU32::new(0);
static PG_LAST_SZ: AtomicU32 = AtomicU32::new(0);
static PG_LAST_T: AtomicU32 = AtomicU32::new(0);
static PG_LAST_US: AtomicU32 = AtomicU32::new(0);
static PG_EV_TOT: AtomicU32 = AtomicU32::new(0);
static PG_EV_OK: AtomicU32 = AtomicU32::new(0);
static PG_EV_SKIP: AtomicU32 = AtomicU32::new(0);
static PG_EV_NO: AtomicU32 = AtomicU32::new(0);
static PG_EV_BAD: AtomicU32 = AtomicU32::new(0);
static PG_PI_OK: AtomicU32 = AtomicU32::new(0);
static PG_PI_SKIP: AtomicU32 = AtomicU32::new(0);
/// The longest `BuildPagingBuffer` call (microseconds, the content mutex wait included): its
/// value, the operation, when it ended.
static PG_LONG: Longest = Longest::new();
/// The longest wait for the content mutex, and the calls that could not take it at all.
static PG_MTX_MAX: AtomicU32 = AtomicU32::new(0);
static PG_MTX_FAIL: AtomicU32 = AtomicU32::new(0);

/// One `BuildPagingBuffer` call finished. `op` is the `DXGK_BUILDPAGINGBUFFER_OPERATION`,
/// `handle` the allocation handle's low 32 bits (0 for none), `size` the bytes the operation
/// names (low 32 bits), `kind` what [`dl::transfer_kind`] / the virtual direction made of it,
/// `result` a [`dl::paging_result`] code, `started` the entry time (100 ns). Atomics only.
pub(crate) fn paging_done(
    op: u32,
    handle: u32,
    size: u32,
    kind: PagingKind,
    result: u32,
    started: u64,
) {
    let now = now_100ns();
    let now_ms = ms_from_100ns(now);
    let us = (now.saturating_sub(started) / 10).min(u32::MAX as u64) as u32;
    PG_LAST_OP.store(op, Ordering::Relaxed);
    PG_LAST_RES.store(result, Ordering::Relaxed);
    PG_LAST_AL.store(handle, Ordering::Relaxed);
    PG_LAST_SZ.store(size, Ordering::Relaxed);
    PG_LAST_US.store(us, Ordering::Relaxed);
    PG_LAST_T.store(now_ms, Ordering::Release);
    PG_LONG.note(us, op, now_ms);
    use dl::paging_result as r;
    match kind {
        PagingKind::Evict => {
            PG_EV_TOT.fetch_add(1, Ordering::Relaxed);
            let counter = match result {
                r::EXECUTED => &PG_EV_OK,
                r::SKIPPED => &PG_EV_SKIP,
                r::NOT_OURS | r::NO_BAR => &PG_EV_NO,
                _ => &PG_EV_BAD,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        PagingKind::PageIn => match result {
            r::EXECUTED => {
                PG_PI_OK.fetch_add(1, Ordering::Relaxed);
            }
            r::SKIPPED | r::NO_GUARD | r::BAD_IRQL => {
                PG_PI_SKIP.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        },
        PagingKind::Other => {}
    }
}

/// The wait for the paging content mutex took `us` microseconds (`got` = it was taken).
pub(crate) fn paging_mutex_wait(us: u32, got: bool) {
    PG_MTX_MAX.fetch_max(us, Ordering::Relaxed);
    if !got {
        PG_MTX_FAIL.fetch_add(1, Ordering::Relaxed);
    }
}

// ---- the Venus mutex and the scanout mutex --------------------------------------------------

/// Venus mutex acquisitions, the longest wait to get it (ms), the longest hold (ms, tagged with
/// the holder's thread, at what time it was released), and the acquisition time of the current
/// holder (100 ns; 0 = free).
static VN_N: AtomicU32 = AtomicU32::new(0);
static VN_WAIT_MAX: AtomicU32 = AtomicU32::new(0);
static VN_HOLD: Longest = Longest::new();
static VN_ACQ_AT: AtomicU64 = AtomicU64::new(0);
static VN_HOLDER: AtomicU32 = AtomicU32::new(0);
static SC_HOLD: Longest = Longest::new();

/// The Venus mutex was acquired; `wait_started` is the 100 ns time the wait began.
pub(crate) fn venus_acquired(wait_started: u64) {
    let now = now_100ns();
    VN_N.fetch_add(1, Ordering::Relaxed);
    let wait_ms = (now.saturating_sub(wait_started) / UNITS_PER_MS).min(u32::MAX as u64) as u32;
    VN_WAIT_MAX.fetch_max(wait_ms, Ordering::Relaxed);
    VN_HOLDER.store(thread_id(), Ordering::Relaxed);
    VN_ACQ_AT.store(now.max(1), Ordering::Release);
}

/// The Venus mutex is about to be released.
pub(crate) fn venus_released() {
    let now = now_100ns();
    let at = VN_ACQ_AT.swap(0, Ordering::AcqRel);
    if at != 0 {
        let hold_ms = (now.saturating_sub(at) / UNITS_PER_MS).min(u32::MAX as u64) as u32;
        VN_HOLD.note(hold_ms, VN_HOLDER.load(Ordering::Relaxed), ms_from_100ns(now));
    }
}

/// The scanout mutex was held for `hold_ms` (taken at `StallT`-clock `acquired_ms`).
pub(crate) fn scanout_released(hold_ms: u32, now_ms: u32) {
    SC_HOLD.note(hold_ms, 0, now_ms);
}

// ---- the registry ---------------------------------------------------------------------------

/// What the ring publish last wrote, so an unchanged state is not written again.
static PUBLISHED_SIG: AtomicU32 = AtomicU32::new(0);
static PUBLISHED_ONCE: AtomicU32 = AtomicU32::new(0);

/// A signature of everything the rings and the first-fatal count hold: it moves when any of
/// them does.
fn signature() -> u32 {
    FAILS
        .count()
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(SUSPECTS.count().wrapping_mul(0x85EB_CA6B))
        .wrapping_add(SLOW.count().wrapping_mul(0xC2B2_AE35))
        .wrapping_add(FIRST.count().wrapping_mul(0x27D4_EB2F))
}

/// As [`signature`], for the rings that mean something: the suspect ring, the slow-call ring and
/// the fatal count (an escape returning `STATUS_INVALID_PARAMETER` to a probing tool moves the
/// plain failure ring and must not make the escape thread write the key twice a second).
fn serious_signature() -> u32 {
    SUSPECTS
        .count()
        .wrapping_mul(0x85EB_CA6B)
        .wrapping_add(SLOW.count().wrapping_mul(0xC2B2_AE35))
        .wrapping_add(FIRST.count().wrapping_mul(0x27D4_EB2F))
}

static PUBLISHED_SERIOUS: AtomicU32 = AtomicU32::new(0);

/// Some ring or the fatal count moved since the last [`publish`]. Atomics only.
pub(crate) fn dirty() -> bool {
    PUBLISHED_ONCE.load(Ordering::Relaxed) == 0
        || signature() != PUBLISHED_SIG.load(Ordering::Relaxed)
}

/// A suspect or fatal status or a slow call since the last [`publish`]. Atomics only.
pub(crate) fn serious_dirty() -> bool {
    serious_signature() != PUBLISHED_SERIOUS.load(Ordering::Relaxed)
}

/// The DDIs in flight now, ids 0..32 as a bitmask.
pub(crate) fn inflight_low() -> u32 {
    INFLIGHT.mask().0
}

/// How long the Venus mutex has been held by its current holder, in ms (0 = free).
pub(crate) fn venus_held_ms() -> u32 {
    let at = VN_ACQ_AT.load(Ordering::Acquire);
    if at == 0 {
        return 0;
    }
    // A held mutex younger than a millisecond still reads "held": 1, never 0.
    ((now_100ns().saturating_sub(at) / UNITS_PER_MS).min(u32::MAX as u64) as u32).max(1)
}

/// Why the block is being written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The periodic mirror (`stall_diag::publish_counters`, StartDevice's zero write, the escape
    /// thread's stuck-only publisher): written when something moved, or when the last write is
    /// 30 s old, so the worker's `REFRESH_POST` step does not carry the block every time.
    Periodic,
    /// A teardown DDI that ran at PASSIVE (`DestroyDevice`): only for a suspect, fatal or slow event.
    Teardown,
    /// `StopDevice`, before its first hive flush: always.
    Stop,
}

/// Interrupt time (ms) of the last [`publish`] (0 = never).
static PUBLISHED_AT: AtomicU32 = AtomicU32::new(0);
/// A periodic write is due after this long without one.
const PERIODIC_MAX_AGE_MS: u32 = 30_000;

/// THE call site of [`publish`]: write the block if `why` says it is worth it. PASSIVE.
pub(crate) fn publish_block(why: Trigger) {
    let due = match why {
        Trigger::Stop => true,
        Trigger::Teardown => serious_dirty(),
        Trigger::Periodic => {
            dirty() || {
                let last = PUBLISHED_AT.load(Ordering::Relaxed);
                let now = ms_from_100ns(now_100ns());
                last == 0 || now.wrapping_sub(last) >= PERIODIC_MAX_AGE_MS
            }
        }
    };
    if due {
        publish();
    }
}

fn hex(n: usize) -> u8 {
    let d = (n & 15) as u8;
    if d < 10 {
        b'0' + d
    } else {
        b'A' + (d - 10)
    }
}

/// Write the newest `len` entries of `ring` as `<stem><kind><hex index>`, index 0 = newest. On
/// the first publish of the image the slots past the count are written too (zeros), so a value
/// an earlier boot left in the key is never read as this one's.
fn dump_ring<const N: usize>(ring: &EventRing<N>, stem: [u8; 2], first_publish: bool) {
    for i in 0..N {
        let entry = ring.nth_newest(i);
        if entry.is_none() && !first_publish {
            break;
        }
        let (a, b, t) = entry.unwrap_or((0, 0, 0));
        for (kind, value) in [(b'S', a), (b'D', b), (b'T', t)] {
            let name = [stem[0], stem[1], kind, hex(i >> 4), hex(i)];
            crate::diag::record_named_bytes(&name, value);
        }
    }
}

/// The call counts: (name, DDI id).
const COUNT_NAMES: [(&[u8], u32); 12] = [
    (b"NPreempt", ddi::PREEMPT_COMMAND),
    (b"NResetTmo", ddi::RESET_FROM_TIMEOUT),
    (b"NRestartTmo", ddi::RESTART_FROM_TIMEOUT),
    (b"NResetEng", ddi::RESET_ENGINE),
    (b"NCreateDev", ddi::CREATE_DEVICE),
    (b"NDestroyDev", ddi::DESTROY_DEVICE),
    (b"NCreateCtx", ddi::CREATE_CONTEXT),
    (b"NDestroyCtx", ddi::DESTROY_CONTEXT),
    (b"NCreateProc", ddi::CREATE_PROCESS),
    (b"NDestroyProc", ddi::DESTROY_PROCESS),
    (b"NStopDev", ddi::STOP_DEVICE),
    (b"NSetPower", ddi::SET_POWER_STATE),
];

/// Mirror the whole block to the service key. PASSIVE_LEVEL only (every write is a synchronous
/// `RtlWriteRegistryValue`): about 60 values and, when a ring moved, up to 96 more.
fn publish() {
    use crate::diag::record_named_bytes as rec;
    let first_publish = PUBLISHED_ONCE.swap(1, Ordering::Relaxed) == 0;
    PUBLISHED_SIG.store(signature(), Ordering::Relaxed);
    PUBLISHED_SERIOUS.store(serious_signature(), Ordering::Relaxed);
    let now_ms = ms_from_100ns(now_100ns());
    PUBLISHED_AT.store(now_ms.max(1), Ordering::Relaxed);
    rec(b"DdiPubT", now_ms);

    // 1. The sticky first-fatal record.
    rec(b"LostN", FIRST.count());
    // The fields only once the winner finished writing them: a record read mid-write would
    // mix two events' values. A publish that catches it skips the fields; the next one has them
    // (`LostN` nonzero with `LostDdi` 0 on that run means "caught mid-write").
    if FIRST.is_ready() {
        rec(b"LostDdi", FIRST.ddi.load(Ordering::Relaxed));
        rec(b"LostSt", FIRST.status.load(Ordering::Relaxed));
        rec(b"LostT", FIRST.t.load(Ordering::Relaxed));
        rec(b"LostThr", FIRST.thread.load(Ordering::Relaxed));
        rec(b"LostHint", FIRST.hint.load(Ordering::Relaxed));
        rec(b"LostIrql", FIRST.irql.load(Ordering::Relaxed));
        rec(b"LostSeq", FIRST.seq.load(Ordering::Relaxed));
        rec(b"LostInfL", FIRST.inflight_lo.load(Ordering::Relaxed));
        rec(b"LostInfH", FIRST.inflight_hi.load(Ordering::Relaxed));
    } else if FIRST.count() == 0 {
        // Nothing fatal yet: zero the fields so a previous boot's record is not read as this one's.
        for name in [
            &b"LostDdi"[..], b"LostSt", b"LostT", b"LostThr", b"LostHint", b"LostIrql",
            b"LostSeq", b"LostInfL", b"LostInfH",
        ] {
            rec(name, 0);
        }
    }

    // 2. The totals, the rings, who is inside a DDI right now, the longest calls.
    rec(b"DdiFailN", FAILS.count());
    rec(b"DdiSuspN", SUSPECT_N.load(Ordering::Relaxed));
    dump_ring(&FAILS, [b'D', b'd'], first_publish);
    dump_ring(&SUSPECTS, [b'D', b'x'], first_publish);
    rec(b"DdiSlowN", SLOW_N.load(Ordering::Relaxed));
    dump_ring(&SLOW, [b'D', b'z'], first_publish);
    rec(b"DdiLongMs", LONGEST_CALL.value.load(Ordering::Relaxed));
    rec(b"DdiLongId", LONGEST_CALL.tag.load(Ordering::Relaxed));
    rec(b"DdiLongT", LONGEST_CALL.t.load(Ordering::Relaxed));
    let (lo, hi) = INFLIGHT.mask();
    rec(b"DdiInflL", lo);
    rec(b"DdiInflH", hi);
    let (old_id, old_ms) = INFLIGHT.oldest(now_ms).unwrap_or((0, 0));
    rec(b"DdiOldId", old_id);
    rec(b"DdiOldMs", old_ms);

    // 3. The DDIs a TDR or a teardown drives.
    for (name, id) in COUNT_NAMES {
        rec(name, CALLS[id as usize].load(Ordering::Relaxed));
    }
    rec(b"TResetTmo", LAST_CALL_T[ddi::RESET_FROM_TIMEOUT as usize].load(Ordering::Relaxed));
    rec(b"TPreempt", LAST_CALL_T[ddi::PREEMPT_COMMAND as usize].load(Ordering::Relaxed));

    // 4. The paging path.
    rec(b"PgLastOp", PG_LAST_OP.load(Ordering::Relaxed));
    rec(b"PgLastRes", PG_LAST_RES.load(Ordering::Relaxed));
    rec(b"PgLastAl", PG_LAST_AL.load(Ordering::Relaxed));
    rec(b"PgLastSz", PG_LAST_SZ.load(Ordering::Relaxed));
    rec(b"PgLastT", PG_LAST_T.load(Ordering::Relaxed));
    rec(b"PgLastUs", PG_LAST_US.load(Ordering::Relaxed));
    rec(b"PgEvTot", PG_EV_TOT.load(Ordering::Relaxed));
    rec(b"PgEvOk", PG_EV_OK.load(Ordering::Relaxed));
    rec(b"PgEvSkip", PG_EV_SKIP.load(Ordering::Relaxed));
    rec(b"PgEvNo", PG_EV_NO.load(Ordering::Relaxed));
    rec(b"PgEvBad", PG_EV_BAD.load(Ordering::Relaxed));
    rec(b"PgPiOk", PG_PI_OK.load(Ordering::Relaxed));
    rec(b"PgPiSkip", PG_PI_SKIP.load(Ordering::Relaxed));
    rec(b"PgLongUs", PG_LONG.value.load(Ordering::Relaxed));
    rec(b"PgLongOp", PG_LONG.tag.load(Ordering::Relaxed));
    rec(b"PgLongT", PG_LONG.t.load(Ordering::Relaxed));
    rec(b"PgMtxMaxUs", PG_MTX_MAX.load(Ordering::Relaxed));
    rec(b"PgMtxFail", PG_MTX_FAIL.load(Ordering::Relaxed));

    // 5. The Venus mutex (held now: its age) and the scanout mutex.
    rec(b"VnLkN", VN_N.load(Ordering::Relaxed));
    rec(b"VnLkWaitMs", VN_WAIT_MAX.load(Ordering::Relaxed));
    rec(b"VnLkHoldMs", VN_HOLD.value.load(Ordering::Relaxed));
    rec(b"VnLkHoldT", VN_HOLD.t.load(Ordering::Relaxed));
    rec(b"VnLkThr", VN_HOLD.tag.load(Ordering::Relaxed));
    rec(b"VnLkHeldMs", venus_held_ms());
    rec(b"ScLkHoldMs", SC_HOLD.value.load(Ordering::Relaxed));
}
