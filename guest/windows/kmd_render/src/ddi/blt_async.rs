//! Asynchronous composed present (`BltAsync`) and the dropped CPU mirror (`BltNoMirror`): the I/O
//! half. The decisions are `helios_kmd_logic::blt_async` (host-tested); the in-flight table and
//! the ownership hand-back are in `virtio/gpu/blt_async.rs`. Design, the analysis of what the DDI
//! waited for, the ordering rules, the hazards and the hardware checklist:
//! `docs/zero-copy-present.md`, "Asynchronous composed present (BltAsync, BltNoMirror)".
//!
//! Scope. Both knobs apply to ONE class of Present: a Blt whose source is an adopted foreign
//! (NVK-on-RM) allocation and whose destination is a KMD standard buffer (DWM's redirection
//! surface). Every other Blt takes the arm it always took.
//!
//! Knobs (REG_DWORD in the service key, default 0 = the previous behaviour; read at every
//! StartDevice by [`reset_for_start`], and once on first use):
//!
//! * `BltAsync` 1: such a Blt returns from `DxgkDdiPresent` without a CPU wait. The copy is
//!   submitted either by the DDI itself (the producer has finished or there is none: DIRECT) or
//!   by the HPD worker through the WindowedBlt FIFO once the producer's boundary has been reached
//!   (DEFERRED). The Present's DMA fence retires with the copy.
//! * `BltNoMirror` 1: such a Blt does not CPU-copy the frame into the destination's system
//!   backing; the backing is marked "system copy invalid" instead, so a later page-in does not
//!   resurrect stale pixels and a later eviction pulls from the GPU blob.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::blt_async::COUNTERS`; atomics,
//! written to the registry by [`publish_counters`] from `publish_nvrm_counters`):
//!
//! * `BltAsyncKnob` / `BltNoMirKnob`: the knobs in force.
//! * `BltAsyncN`: asynchronous Blts made (`BltAsyncDir` direct, `BltAsyncDefer` deferred to the
//!   worker because the producer had not finished, or the mirror is on, or an older frame for the
//!   destination was still queued).
//! * `BltAsyncInfl` / `BltAsyncPk`: in flight now (submitted or queued, copy not yet complete) and
//!   the most at once.
//! * `BltAsyncLat0..7`: submission to copy completion, 8 buckets (< 250 us, < 500 us, < 1 ms,
//!   < 2 ms, < 4 ms, < 8 ms, < 16 ms, more). `BltDeferUs`: microseconds a deferred Blt waited
//!   from the Present to its submission.
//! * `BltAsyncFail`: copies the host answered with an error (the Present still completes: the
//!   destination keeps the previous frame). `BltAsyncFall`: Blts that were eligible and fell back
//!   to the legacy arm; `BltAsyncWhy` the last reason (`blt_async::Why::code`), `BltAsyncMask`
//!   every reason seen (bit `code - 1`). `BltAsyncBusy`: of those, the destination was still
//!   owned by a reader or a writer that is not one of ours. `BltDrainN`: legacy Blts that waited
//!   for the queued copies of their destination first.
//! * `BltWaitN` / `BltWaitUs` / `BltWait0..7`: the legacy arm's CPU wait for the copy's fence (the
//!   same buckets): what `BltAsync` saves.
//! * `BltMirrorN` / `BltMirrorSk` / `BltMirrorUs`: CPU mirrors done, skipped by `BltNoMirror`,
//!   and the microseconds spent in them. `BltNoMirInv`: destination system copies newly marked
//!   invalid.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::blt_async::{self as ba, Route, Why};
use wdk_sys::ntddk::KeQueryInterruptTimePrecise;

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::ddi::present_packet::PresentSubmissionPrivate;
use crate::irql::PassiveLevel;
use crate::virtio::ctrl::BltSubmit;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;

/// The service-key values, read through `diag::knobs` (the one inventory).
const UNREAD: u32 = u32::MAX;
static ASYNC_KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static NO_MIRROR_KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static ASYNC_N: AtomicU32 = AtomicU32::new(0);
static DIRECT: AtomicU32 = AtomicU32::new(0);
static DEFERRED: AtomicU32 = AtomicU32::new(0);
static INFLIGHT: AtomicU32 = AtomicU32::new(0);
static PEAK: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
static FELL_BACK: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static WHY_MASK: AtomicU32 = AtomicU32::new(0);
static BUSY: AtomicU32 = AtomicU32::new(0);
static DEFER_US: AtomicU32 = AtomicU32::new(0);
static DRAINS: AtomicU32 = AtomicU32::new(0);
static LAT: [AtomicU32; ba::BUCKETS] = [const { AtomicU32::new(0) }; ba::BUCKETS];
static WAIT_N: AtomicU32 = AtomicU32::new(0);
static WAIT_US: AtomicU32 = AtomicU32::new(0);
static WAIT: [AtomicU32; ba::BUCKETS] = [const { AtomicU32::new(0) }; ba::BUCKETS];
static MIRROR_N: AtomicU32 = AtomicU32::new(0);
static MIRROR_SKIPPED: AtomicU32 = AtomicU32::new(0);
static MIRROR_US: AtomicU32 = AtomicU32::new(0);
static NOMIR_INVALID: AtomicU32 = AtomicU32::new(0);

/// Interrupt time in 100 ns units; legal at any IRQL, no lock.
pub(crate) fn now_100ns() -> u64 {
    let mut qpc_timestamp = 0;
    // SAFETY: a scalar time read with a valid output location, legal at any IRQL.
    unsafe { KeQueryInterruptTimePrecise(&mut qpc_timestamp) }
}

#[inline(never)]
fn read_knob(cell: &AtomicU32, name: crate::diag::KnobName) -> bool {
    let v = crate::diag::read_config_dword(name, 0) != 0;
    cell.store(v as u32, Ordering::Relaxed);
    v
}

fn knob(cell: &AtomicU32, name: crate::diag::KnobName) -> bool {
    match cell.load(Ordering::Relaxed) {
        UNREAD => read_knob(cell, name),
        v => v != 0,
    }
}

/// `BltAsync` is on. One relaxed load once read.
pub(crate) fn async_on() -> bool {
    knob(&ASYNC_KNOB, crate::diag::knobs::BLT_ASYNC)
}

/// `BltNoMirror` is on. One relaxed load once read.
pub(crate) fn no_mirror_on() -> bool {
    knob(&NO_MIRROR_KNOB, crate::diag::knobs::BLT_NO_MIRROR)
}

/// A new transport generation: the knobs are read again (a `reg add` + `pnputil /restart-device`
/// takes effect without a reboot), mirrored with the value in force, 0 included, and the counters
/// are zeroed so a value an earlier run left in the service key is never read as this
/// generation's. PASSIVE.
pub(crate) fn reset_for_start() {
    for cell in [
        &ASYNC_N,
        &DIRECT,
        &DEFERRED,
        &INFLIGHT,
        &PEAK,
        &FAILED,
        &FELL_BACK,
        &LAST_WHY,
        &WHY_MASK,
        &BUSY,
        &DEFER_US,
        &DRAINS,
        &WAIT_N,
        &WAIT_US,
        &MIRROR_N,
        &MIRROR_SKIPPED,
        &MIRROR_US,
        &NOMIR_INVALID,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
    for cell in LAT.iter().chain(WAIT.iter()) {
        cell.store(0, Ordering::Relaxed);
    }
    let a = read_knob(&ASYNC_KNOB, crate::diag::knobs::BLT_ASYNC);
    let m = read_knob(&NO_MIRROR_KNOB, crate::diag::knobs::BLT_NO_MIRROR);
    crate::diag::record_named_bytes(b"BltAsyncKnob", a as u32);
    crate::diag::record_named_bytes(b"BltNoMirKnob", m as u32);
}

const LAT_NAMES: [&[u8]; ba::BUCKETS] = [
    b"BltAsyncLat0",
    b"BltAsyncLat1",
    b"BltAsyncLat2",
    b"BltAsyncLat3",
    b"BltAsyncLat4",
    b"BltAsyncLat5",
    b"BltAsyncLat6",
    b"BltAsyncLat7",
];

const WAIT_NAMES: [&[u8]; ba::BUCKETS] = [
    b"BltWait0",
    b"BltWait1",
    b"BltWait2",
    b"BltWait3",
    b"BltWait4",
    b"BltWait5",
    b"BltWait6",
    b"BltWait7",
];

/// Mirror the counters to the service key, once an event happened. PASSIVE only; with the NVRM
/// counters.
pub(crate) fn publish_counters() {
    let events = ASYNC_N.load(Ordering::Relaxed)
        | FELL_BACK.load(Ordering::Relaxed)
        | WAIT_N.load(Ordering::Relaxed)
        | MIRROR_N.load(Ordering::Relaxed)
        | MIRROR_SKIPPED.load(Ordering::Relaxed)
        | FAILED.load(Ordering::Relaxed);
    if events == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"BltAsyncN", ASYNC_N.load(Ordering::Relaxed));
    rec(b"BltAsyncDir", DIRECT.load(Ordering::Relaxed));
    rec(b"BltAsyncDefer", DEFERRED.load(Ordering::Relaxed));
    rec(b"BltAsyncInfl", INFLIGHT.load(Ordering::Relaxed));
    rec(b"BltAsyncPk", PEAK.load(Ordering::Relaxed));
    rec(b"BltAsyncFail", FAILED.load(Ordering::Relaxed));
    rec(b"BltAsyncFall", FELL_BACK.load(Ordering::Relaxed));
    rec(b"BltAsyncWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"BltAsyncMask", WHY_MASK.load(Ordering::Relaxed));
    rec(b"BltAsyncBusy", BUSY.load(Ordering::Relaxed));
    rec(b"BltDeferUs", DEFER_US.load(Ordering::Relaxed));
    rec(b"BltDrainN", DRAINS.load(Ordering::Relaxed));
    for (name, cell) in LAT_NAMES.iter().zip(LAT.iter()) {
        rec(name, cell.load(Ordering::Relaxed));
    }
    rec(b"BltWaitN", WAIT_N.load(Ordering::Relaxed));
    rec(b"BltWaitUs", WAIT_US.load(Ordering::Relaxed));
    for (name, cell) in WAIT_NAMES.iter().zip(WAIT.iter()) {
        rec(name, cell.load(Ordering::Relaxed));
    }
    rec(b"BltMirrorN", MIRROR_N.load(Ordering::Relaxed));
    rec(b"BltMirrorSk", MIRROR_SKIPPED.load(Ordering::Relaxed));
    rec(b"BltMirrorUs", MIRROR_US.load(Ordering::Relaxed));
    rec(b"BltNoMirInv", NOMIR_INVALID.load(Ordering::Relaxed));
}

// ---- counters, callable at any IRQL (atomics only) -----------------------------------------

fn dec(cell: &AtomicU32) {
    let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

/// One more asynchronous Blt in flight (submitted or queued).
pub(crate) fn note_infl_add() {
    let now = INFLIGHT.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// One asynchronous Blt left the in-flight set (completed, cancelled or abandoned).
pub(crate) fn note_infl_sub() {
    dec(&INFLIGHT);
}

/// The copy of an asynchronous Blt completed (`t_submit` is its submission's interrupt time).
pub(crate) fn note_copy_done(t_submit: u64, ok: bool) {
    let dt = now_100ns().saturating_sub(t_submit);
    LAT[ba::lat_bucket(dt)].fetch_add(1, Ordering::Relaxed);
    if !ok {
        FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// A deferred Blt left the FIFO for the ring: it waited `t_submit - t_queue`.
pub(crate) fn note_defer_wait(t_queue: u64, t_submit: u64) {
    DEFER_US.fetch_add(ba::us32(t_submit.saturating_sub(t_queue)), Ordering::Relaxed);
}

/// The legacy arm waited for the copy's fence from `t0` until now.
pub(crate) fn note_wait(t0: u64) {
    let dt = now_100ns().saturating_sub(t0);
    WAIT_N.fetch_add(1, Ordering::Relaxed);
    WAIT_US.fetch_add(ba::us32(dt), Ordering::Relaxed);
    WAIT[ba::lat_bucket(dt)].fetch_add(1, Ordering::Relaxed);
}

/// A CPU mirror of a Present destination ran from `t0` until now.
pub(crate) fn note_mirror(t0: u64) {
    let dt = now_100ns().saturating_sub(t0);
    MIRROR_N.fetch_add(1, Ordering::Relaxed);
    MIRROR_US.fetch_add(ba::us32(dt), Ordering::Relaxed);
}

/// The CPU mirror of a foreign-source Blt was skipped (`BltNoMirror`).
pub(crate) fn note_mirror_skipped() {
    MIRROR_SKIPPED.fetch_add(1, Ordering::Relaxed);
}

/// A Blt that was eligible took the legacy arm.
fn fall(why: Why) -> Taken {
    FELL_BACK.fetch_add(1, Ordering::Relaxed);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    WHY_MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if why == Why::DstBusy {
        BUSY.fetch_add(1, Ordering::Relaxed);
    }
    Taken::Legacy
}

/// The destination's system copy is stale from this frame on (`BltNoMirror`): mark it invalid so
/// a page-in does not copy older system pages over the blob the GPU copy is about to write, and
/// a later eviction (which copies blob to system) revalidates it. Nothing is marked when VidMm
/// holds no system pages for the destination (nothing could be resurrected, and the invalid set
/// is bounded). PASSIVE; spinlocks only.
pub(crate) fn mark_stale(adapter: &AdapterContext, resource_id: u32) {
    use helios_kmd_logic::paging::Mark;
    match adapter.system_backings.mark_stale_if_backed(resource_id) {
        Some(Mark::Newly) | Some(Mark::Overflow) => {
            NOMIR_INVALID.fetch_add(1, Ordering::Relaxed);
        }
        Some(Mark::Already) | Some(Mark::Ignored) | None => {}
    }
}

// ---- the route ------------------------------------------------------------------------------

/// What [`try_async`] did with a Blt.
pub(crate) enum Taken {
    /// Not asynchronous: the legacy arm runs (the reason was counted).
    Legacy,
    /// Submitted by the DDI; the Present's private record names this wire fence.
    Direct(u64),
    /// Queued for the HPD worker; the Present's private record carries this token.
    Deferred(u64),
}

/// Take the Blt asynchronously when the rules allow (`helios_kmd_logic::blt_async::decide`). The
/// caller has checked [`async_on`] and that the source is a foreign allocation and the
/// destination a standard buffer (the descriptors say which).
///
/// `Ok(Legacy)` leaves everything as it was: nothing is queued, owned or marked that the legacy
/// arm does not expect. `Err` is only the private record refusing the copy's fence AFTER the copy
/// was submitted (the capacity was checked before any host work, so it cannot happen; the arm
/// reports it as it always did).
///
/// # Safety
/// `args` is dxgkrnl's `DXGKARG_PRESENT` for this call (PASSIVE_LEVEL) and its private-data
/// pointer is writable for `DmaBufferPrivateDataSize` bytes.
pub(crate) unsafe fn try_async(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    boundary: Option<u64>,
) -> Result<Taken, NTSTATUS> {
    let destination_resource = destination.resource_id();
    let facts = adapter.with_virtio(|v| v.blt_async_facts(boundary, destination_resource));
    let Ok((boundary_state, deferred_pending, room)) = facts else {
        return Ok(fall(Why::SubmitRefused));
    };
    let route = ba::decide(ba::Facts {
        async_on: true,
        no_mirror_on: no_mirror_on(),
        foreign_source: true,
        snapshot: false,
        dst_standard_buffer: matches!(destination, PresentDestinationDesc::StandardBuffer(_)),
        boundary: boundary_state,
        dst_deferred_pending: deferred_pending,
        table_has_room: room,
    });
    match route {
        Route::Legacy { why } => Ok(fall(why)),
        Route::LegacyAfterDrain { why } => {
            drain(passive, adapter, destination_resource);
            Ok(fall(why))
        }
        Route::Direct => unsafe { direct(passive, adapter, args, source, destination) },
        Route::Deferred => unsafe {
            deferred(passive, adapter, args, source, destination, boundary)
        },
    }
}

/// Wait (PASSIVE, bounded) until no queued copy names `resource_id` as its destination. The
/// legacy arm that follows must not reach the host before an older frame queued for the same
/// buffer, or the older frame would land last.
fn drain(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    DRAINS.fetch_add(1, Ordering::Relaxed);
    // Nominal 320 ms, about 5 s at Windows' common timer quantum: the budget of
    // `begin_present_buffer_write_legacy`, which this wait precedes.
    let mut slices = 0u32;
    while slices < 320 {
        let pending = adapter
            .with_virtio(|v| v.blt_dst_deferred_pending(resource_id))
            .unwrap_or(false);
        if !pending {
            return;
        }
        crate::virtio::ctrl::sleep_ms(passive, 1);
        slices += 1;
    }
}

/// DIRECT: submit the copy from the DDI and return. The destination ownership is taken in the
/// same transport critical section as the enqueue and handed back by the completion DPC; the
/// Present's DMA fence retires with the copy's wire fence exactly as it did when the DDI waited
/// (`PresentSubmissionPrivate::merge_fence`).
unsafe fn direct(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
) -> Result<Taken, NTSTATUS> {
    // Before the copy: from here the system pages are older than the blob.
    mark_stale(adapter, destination.resource_id());
    let copy = adapter.with_venus_client(passive, |client| {
        client.submit_present_blt_direct(adapter, source, destination)
    });
    let fence = match copy {
        Ok(Ok(BltSubmit::Fence(fence))) => fence,
        Ok(Ok(BltSubmit::DstBusy)) => return Ok(fall(Why::DstBusy)),
        // Queue pressure, no memory, a transport that is gone: the legacy arm decides what a
        // refusal means (a foreign source's refusal is a counted success there).
        Ok(Err(_)) | Err(_) => return Ok(fall(Why::SubmitRefused)),
    };
    // Capacity was checked before host work was queued, so this cannot fail. Merge preserves the
    // newest fence if dxgkrnl batches more than one Present into the same DMA private-data buffer.
    if let Err(status) = unsafe {
        PresentSubmissionPrivate::merge_fence(
            args.pDmaBufferPrivateData,
            args.DmaBufferPrivateDataSize,
            fence,
        )
    } {
        return Err(status);
    }
    ASYNC_N.fetch_add(1, Ordering::Relaxed);
    DIRECT.fetch_add(1, Ordering::Relaxed);
    Ok(Taken::Direct(fence))
}

/// DEFERRED: prepare the reusable copy, queue it in the WindowedBlt FIFO under the producer's
/// boundary and return. SubmitCommand admits it once the destination's residency is effective;
/// the HPD worker submits it when the boundary is ready and the destination can be written; its
/// ring completion (and, with the mirror on, the worker's mirror) terminalizes the token the
/// Present's DMA fence waits for.
unsafe fn deferred(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    boundary: Option<u64>,
) -> Result<Taken, NTSTATUS> {
    let Some(boundary) = boundary else {
        return Ok(fall(Why::NoBoundaryMirror));
    };
    let no_mirror = no_mirror_on();
    // SAFETY of the lock order: scanout -> venus -> virtio, as the snapshot arm of the same
    // DDI. Cache preparation may block only while the Venus mutex is held; the FIFO insertion is
    // a preallocated spinlock-only mutation.
    let queued = adapter.with_scanout_lifecycle(passive, |lock| {
        let prepared = lock.with_venus_client(|client| {
            client.prepare_present_blt(adapter, source, destination)
        });
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(VirtioError::DeviceError),
        };
        adapter
            .with_virtio(|v| {
                v.queue_async_blt(adapter, source, destination, prepared, boundary, no_mirror)
            })
            .unwrap_or(Err(VirtioError::DeviceError))
    });
    let token = match queued {
        Ok(token) => token,
        Err(_) => return Ok(fall(Why::QueueRefused)),
    };
    if let Err(_status) = unsafe {
        PresentSubmissionPrivate::merge_windowed_blt_token(
            args.pDmaBufferPrivateData,
            args.DmaBufferPrivateDataSize,
            token,
            boundary,
        )
    } {
        // The record cannot carry the token (a request of another stream already owns it, or no
        // room): nothing was submitted, so cancel the request and let the legacy arm run.
        let _ = adapter.with_virtio(|v| v.cancel_windowed_blt(adapter, token, boundary));
        return Ok(fall(Why::TokenRefused));
    }
    ASYNC_N.fetch_add(1, Ordering::Relaxed);
    DEFERRED.fetch_add(1, Ordering::Relaxed);
    Ok(Taken::Deferred(token))
}
