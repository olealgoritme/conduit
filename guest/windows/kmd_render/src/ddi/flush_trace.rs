//! The flush gate's timeline, KMD side (`docs/flush-gate.md` section 9).
//!
//! DIAGNOSTIC ONLY. Nothing here decides anything the driver does, with ONE exception that
//! is off by default: the `FlGSyncMs` knob ([`sync_wait`]). With the knob at 0 (the default)
//! every function below only records: it writes atomics, takes no lock that the driver did
//! not already take on the same path, and returns nothing the callers act on.
//!
//! Three points of a `HEFL` packet are recorded into a 64-event ring
//! ([`helios_kmd_logic::flush_trace::Ring`], atomics only) and into counters:
//!
//! * RENDER, `DxgkDdiRender` (PASSIVE): the record was resolved ([`note_render`]); what it
//!   left for SubmitCommand is stashed on its context.
//! * SUBMIT, `DxgkDdiSubmitCommand` (DISPATCH): the DMA buffer reached the scheduler's
//!   submit ([`submit_trace`], [`SubmitTrace::record`]); the boundary it decoded is compared
//!   with the one the Render merged.
//! * RETIRE, the completion DPC, or SubmitCommand itself when the fence was satisfiable at
//!   once ([`note_retire`], and [`SubmitTrace::record`] for [`Disposition::Immediate`]).
//!
//! The gate kind (Venus stream point, RM fence, wire rung) rides on every event. The counters
//! are published by [`publish_counters`] from `publish_nvrm_counters` (PASSIVE).
//!
//! Pairing Render with SubmitCommand is per context and one deep: dxgkrnl issues a context's
//! Render and SubmitCommand in order, so the SubmitCommand that follows a `HEFL` Render on
//! the same context is, normally, that Render's buffer. Two Renders before one SubmitCommand
//! (batching) count `FlGBat`; a `HEFL` Render whose buffer dxgkrnl never submits would hand
//! its record to the NEXT submit of the context (a present's, say). That mis-pairing is
//! bounded to one record and shows as `FlGSub` below `FlGRec` plus a mismatch.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::flush_trace::{
    self as ft, flag, kind, Insert, Outstanding, PendingFlush, Ring, Summary,
};

use crate::adapter::AdapterContext;
use crate::ddi::present_packet::PresentSubmissionBoundary;
use crate::device::ContextHandleRef;
use crate::dxgk::HANDLE;

static RING: Ring = Ring::new();
static OUTSTANDING: Outstanding = Outstanding::new();

/// `FlGSub`: DMA buffers that reached SubmitCommand carrying a `HEFL` Render's record.
static SUBMITS: AtomicU32 = AtomicU32::new(0);
/// `FlGBat`: Renders that found an earlier `HEFL` Render of the context still waiting.
static BATCHED: AtomicU32 = AtomicU32::new(0);
/// `FlGMat`: SubmitCommands whose decoded boundary was the merged one.
static MATCHED: AtomicU32 = AtomicU32::new(0);
/// `FlGMis`: SubmitCommands that did not find it.
static MISMATCHED: AtomicU32 = AtomicU32::new(0);
/// `FlGExempt`: SubmitCommands of a record that replaced an earlier pending one (batched
/// into one DMA buffer): counted in `FlGSub`, in neither `FlGMat` nor `FlGMis`.
static EXEMPT: AtomicU32 = AtomicU32::new(0);
/// `FlGSigFail`: fences satisfiable at SubmitCommand whose `DMA_COMPLETED` could not be
/// delivered there (notification failed, or no callback table); not tracked.
static SIGNAL_FAILED: AtomicU32 = AtomicU32::new(0);
/// `FlGSyncSkip`: `FlGSyncMs` waits skipped because the live IRQL was above PASSIVE.
static SYNC_SKIPPED: AtomicU32 = AtomicU32::new(0);
/// `FlGEmpty`: SubmitCommands that found NO private-data record at all (a subset of
/// `FlGMis` when the Render expected one): the buffer's private data was not the one the
/// Render wrote, or dxgkrnl handed SubmitCommand a different range.
static EMPTY: AtomicU32 = AtomicU32::new(0);
/// `FlGImm`: fences completed inside SubmitCommand, never queued (the gate held nothing).
static IMMEDIATE: AtomicU32 = AtomicU32::new(0);
/// `FlGRet`: fences delivered (`DMA_COMPLETED`) that were tracked from their submit.
static RETIRED: AtomicU32 = AtomicU32::new(0);
/// `FlGReb`: of those, released by the `WddmHeadMs` rebase.
static REBASED: AtomicU32 = AtomicU32::new(0);
/// Sum of the submit-to-retire lags of the QUEUED fences, microseconds (feeds `FlGLagAvg`).
static LAG_SUM_US: AtomicU64 = AtomicU64::new(0);
/// `FlGLagMax`: the longest submit-to-retire lag of a queued fence, microseconds.
static LAG_MAX_US: AtomicU32 = AtomicU32::new(0);
/// `FlGUnord`: Venus transport submissions that entered while a queued `HEFL` fence was
/// outstanding. Adapter-global: it includes DWM's and the releasing context's own next
/// frame. Evidence that nothing holds other work behind the gate, not of one consumer.
static OVERLAP: AtomicU32 = AtomicU32::new(0);
/// `FlGTblFull`: a queued fence the outstanding table had no room for (not tracked).
static TABLE_FULL: AtomicU32 = AtomicU32::new(0);
/// `FlGAbn`: tracked fences dropped because the scheduler epoch was abandoned.
static ABANDONED: AtomicU32 = AtomicU32::new(0);

/// `FlGSyncMs`, clamped (`ft::clamp_sync_ms`). 0 = off.
static SYNC_MS: AtomicU32 = AtomicU32::new(0);
/// `FlGSyncWt`: Renders that waited (the boundary was not yet ready when asked).
static SYNC_WAITS: AtomicU32 = AtomicU32::new(0);
/// `FlGSyncTmo`: of those, waits that hit the knob's bound with the boundary still not ready.
static SYNC_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
/// `FlGSyncMaxUs`: the longest wait, microseconds.
static SYNC_MAX_US: AtomicU32 = AtomicU32::new(0);

/// Snapshot `FlGSyncMs` from the service key (PASSIVE, transport init).
pub(crate) fn init_from_registry() {
    let ms = ft::clamp_sync_ms(crate::diag::read_config_dword(
        crate::diag::knobs::FLG_SYNC_MS,
        0,
    ));
    SYNC_MS.store(ms, Ordering::Relaxed);
    // The value in force, mirrored at every transport init (the counters' own mirror is gated on
    // a Render having been seen, so a knob set back to 0 left the previous value showing).
    crate::diag::record_named_bytes(b"FlGSyncEff", ms);
    // A restart (`pnputil /restart-device`) must not inherit queued-fence slots of the
    // previous transport: those fences will never retire.
    note_abandon();
}

/// The `FlGSyncMs` value in force (0 = off).
pub(crate) fn sync_ms() -> u32 {
    SYNC_MS.load(Ordering::Relaxed)
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// What `flush_gate_record` resolved, for [`note_render`].
pub(crate) struct RenderInfo {
    /// The record asked for a stream point (`HELIOS_FLUSH_GATE_FLAG_STREAM`) / an RM fence.
    pub carrier_stream: bool,
    pub carrier_fence: bool,
    /// The stream point value the record named (`command.value`).
    pub asked_value: u32,
    /// The tagged boundary the packet carries after the merge, if it kept one.
    pub boundary: Option<u64>,
    /// The record was counted `FlGDeg`.
    pub degraded: bool,
    /// The wire floor the packet was stamped with, if it was.
    pub floor: Option<u64>,
}

/// RENDER: record the event and leave what SubmitCommand needs on the context.
/// PASSIVE (`DxgkDdiRender`); allocates nothing.
pub(crate) fn note_render(context: Option<&ContextHandleRef<'_>>, info: &RenderInfo) {
    let stamp = now();
    let mut flags = 0u8;
    match (info.boundary, info.carrier_stream, info.carrier_fence) {
        (Some(_), true, _) => flags |= flag::STREAM,
        (Some(_), false, true) => flags |= flag::FENCE,
        _ => flags |= flag::WIRE,
    }
    if info.degraded {
        flags |= flag::DEGRADED;
    }
    if info.floor.is_some() {
        flags |= flag::STAMPED;
    }
    if info.boundary.is_some() && info.carrier_stream && info.asked_value == 0 {
        flags |= flag::ZERO_POINT;
    }
    let expected_boundary = info.boundary.unwrap_or(0);
    let expected_floor = if info.boundary.is_none() {
        info.floor.unwrap_or(0)
    } else {
        0
    };
    let ctx_low = context.map_or(0, |c| c.trace_id());
    let batched = context.is_some_and(|c| {
        c.stash_flush_pending(PendingFlush {
            stamp_100ns: stamp,
            render_flags: flags,
            expected_boundary,
            expected_floor,
            // Set by the stash itself, under the context's lock.
            replaced: false,
        })
    });
    if batched {
        flags |= flag::BATCHED;
        BATCHED.fetch_add(1, Ordering::Relaxed);
    }
    RING.record(
        kind::RENDER,
        flags,
        ctx_low,
        0,
        info.asked_value,
        if expected_boundary != 0 {
            expected_boundary
        } else {
            expected_floor
        },
        stamp,
    );
}

/// What SubmitCommand found for a context that had a `HEFL` Render pending.
pub(crate) struct SubmitTrace {
    pending: PendingFlush,
    ctx_low: u32,
    decoded_boundary: u64,
    decoded_fence: u64,
    empty: bool,
}

/// SUBMIT, step 1 (DISPATCH, before the notification lock): take the record the context's
/// last `HEFL` Render left. `None` for every other submission (one atomic swap).
///
/// # Safety
/// `h_context` is the `hContext` of this SubmitCommand (null is tolerated).
pub(crate) unsafe fn submit_trace(
    h_context: HANDLE,
    present_fence: Option<PresentSubmissionBoundary>,
) -> Option<SubmitTrace> {
    // SAFETY: the same handle the execution-record decode of this SubmitCommand reads.
    let context = unsafe { ContextHandleRef::from_raw(h_context) }?;
    let pending = context.take_flush_pending()?;
    Some(SubmitTrace {
        pending,
        ctx_low: context.trace_id(),
        decoded_boundary: present_fence.map_or(0, |p| p.stream_boundary),
        decoded_fence: present_fence.map_or(0, |p| p.gpu_fence_id),
        empty: present_fence.is_none(),
    })
}

/// What SubmitCommand did with the fence of a traced packet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Left in the WDDM FIFO for the completion DPC.
    Queued,
    /// Satisfiable at once and `DMA_COMPLETED` was delivered inside SubmitCommand.
    Immediate,
    /// Satisfiable at once but the notification failed, or dxgkrnl's callback table was
    /// unavailable: not delivered, not tracked (`FlGSigFail`).
    SignalFailed,
}

impl SubmitTrace {
    /// SUBMIT, step 2: record the event (and, for [`Disposition::Immediate`], the retire),
    /// inside the notification lock and AFTER the signal decision, so the completion DPC
    /// cannot retire the fence before its submit is recorded and the event says what
    /// really happened. Atomics only.
    pub(crate) fn record(&self, fence: u32, disposition: Disposition) {
        let stamp = now();
        let p = &self.pending;
        SUBMITS.fetch_add(1, Ordering::Relaxed);
        let matched = ft::submit_matches(
            p.expected_boundary,
            p.expected_floor,
            self.decoded_boundary,
            self.decoded_fence,
        );
        let mut flags = ft::gate_bits(p.render_flags);
        if matched {
            flags |= flag::MATCH;
        }
        match ft::account(p, matched) {
            ft::Account::Matched => MATCHED.fetch_add(1, Ordering::Relaxed),
            ft::Account::Mismatched => MISMATCHED.fetch_add(1, Ordering::Relaxed),
            ft::Account::Exempt => {
                flags |= flag::EXEMPT;
                EXEMPT.fetch_add(1, Ordering::Relaxed)
            }
        };
        if self.empty {
            flags |= flag::EMPTY;
            EMPTY.fetch_add(1, Ordering::Relaxed);
        }
        match disposition {
            Disposition::Immediate => flags |= flag::IMMEDIATE,
            Disposition::SignalFailed => {
                flags |= flag::SIGNAL_FAILED;
                SIGNAL_FAILED.fetch_add(1, Ordering::Relaxed);
            }
            Disposition::Queued => {}
        }
        RING.record(
            kind::SUBMIT,
            flags,
            self.ctx_low,
            fence,
            ft::lag_us(p.stamp_100ns, stamp),
            if self.decoded_boundary != 0 {
                self.decoded_boundary
            } else {
                self.decoded_fence
            },
            stamp,
        );
        match disposition {
            Disposition::Queued => {
                if OUTSTANDING.insert(fence, stamp) == Insert::Refused {
                    TABLE_FULL.fetch_add(1, Ordering::Relaxed);
                }
            }
            Disposition::Immediate => self.retire_at_submit(fence),
            // Nothing was delivered and nothing is queued by this call: not tracked.
            Disposition::SignalFailed => {}
        }
    }

    /// RETIRE, for a fence SubmitCommand completed itself (after a successful
    /// `DMA_COMPLETED`): the gate held nothing.
    fn retire_at_submit(&self, fence: u32) {
        IMMEDIATE.fetch_add(1, Ordering::Relaxed);
        RETIRED.fetch_add(1, Ordering::Relaxed);
        RING.record(
            kind::RETIRE,
            ft::gate_bits(self.pending.render_flags) | flag::AT_SUBMIT,
            self.ctx_low,
            fence,
            0,
            0,
            now(),
        );
    }
}

/// RETIRE: `DMA_COMPLETED` for `fence` was delivered by the completion DPC. A fence that was
/// not tracked from a `HEFL` submit costs one relaxed load (nothing outstanding) or a scan of
/// 16 slots. Atomics only: callable at DISPATCH with the notification lock held.
pub(crate) fn note_retire(fence: u32, rebased: bool) {
    if OUTSTANDING.live() == 0 {
        return;
    }
    let Some(submitted) = OUTSTANDING.take(fence) else {
        return;
    };
    let stamp = now();
    let lag = ft::lag_us(submitted, stamp);
    RETIRED.fetch_add(1, Ordering::Relaxed);
    LAG_SUM_US.fetch_add(lag as u64, Ordering::Relaxed);
    LAG_MAX_US.fetch_max(lag, Ordering::Relaxed);
    let mut flags = 0u8;
    let mut ctx_low = 0u32;
    match RING.latest_for_fence(kind::SUBMIT, fence) {
        Some(submit) => {
            flags |= submit.flags & (flag::GATE_STREAM | flag::GATE_FENCE | flag::GATE_WIRE);
            ctx_low = submit.ctx_low;
        }
        None => flags |= flag::NO_SUBMIT,
    }
    if rebased {
        flags |= flag::REBASED;
        REBASED.fetch_add(1, Ordering::Relaxed);
    }
    RING.record(kind::RETIRE, flags, ctx_low, fence, lag, 0, stamp);
}

/// The scheduler epoch was abandoned (preempt / reset / timeout): fences still tracked will
/// never retire. Atomics only.
pub(crate) fn note_abandon() {
    let dropped = OUTSTANDING.clear();
    if dropped != 0 {
        ABANDONED.fetch_add(dropped, Ordering::Relaxed);
    }
}

/// A Venus transport submission entered (`enqueue_submit_inner`, DISPATCH, `virtio_lock`
/// held): one relaxed load; counts it when a queued `HEFL` fence is outstanding.
#[inline]
pub(crate) fn note_transport_submit() {
    if OUTSTANDING.live() != 0 {
        OVERLAP.fetch_add(1, Ordering::Relaxed);
    }
}

/// `FlGSyncMs`: hold the Render until the boundary its packet carries has retired, so the
/// runtime's key release follows the GPU completion on the CPU. Returns immediately when
/// the knob is 0, or when nothing was carried, or when it is already ready.
///
/// Bounded: at most `FlGSyncMs` (clamped to `ft::SYNC_MS_MAX`) plus one timer tick of
/// overshoot (`KeDelayExecutionThread` rounds a 1 ms sleep up to the timer granularity,
/// ~15.6 ms by default). PASSIVE only, and CHECKED: the live IRQL is read with
/// `PassiveLevel::try_assume` (the `assume` token is only a counter) and a caller above
/// PASSIVE returns without sleeping (`FlGSyncSkip`). No lock is held across the wait: each
/// poll takes `virtio_lock` for one read-only readiness test and drops it before the
/// sleep. A transport that is down or failed ends the wait (nothing could complete).
pub(crate) fn sync_wait(
    adapter: &AdapterContext,
    boundary: Option<u64>,
    floor: Option<u64>,
) {
    let ms = sync_ms();
    if ms == 0 {
        return;
    }
    let boundary = boundary.unwrap_or(0);
    let floor = if boundary == 0 { floor.unwrap_or(0) } else { 0 };
    if boundary == 0 && floor == 0 {
        return;
    }
    let ready = |adapter: &AdapterContext| {
        adapter
            .with_virtio(|v| v.flush_gate_ready(boundary, floor))
            // Transport down: nothing can retire; do not burn the budget.
            .unwrap_or(true)
    };
    if ready(adapter) {
        return;
    }
    // About to sleep: only at a PASSIVE_LEVEL that `KeGetCurrentIrql` confirms.
    let Some(passive) = crate::irql::PassiveLevel::try_assume() else {
        SYNC_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    SYNC_WAITS.fetch_add(1, Ordering::Relaxed);
    let started = now();
    let deadline = started.saturating_add(ms as u64 * 10_000);
    let mut timed_out = false;
    loop {
        crate::virtio::ctrl::sleep_ms(passive, 1);
        // Interrupt-loss tolerance, as `ctrl::wait_block`: take whatever completed, so a
        // lost interrupt costs one slice and cannot masquerade as a timeout.
        let _ = adapter.with_virtio(|v| v.drain_used());
        if ready(adapter) {
            break;
        }
        if now() >= deadline {
            timed_out = true;
            break;
        }
    }
    if timed_out {
        SYNC_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
    }
    SYNC_MAX_US.fetch_max(ft::lag_us(started, now()), Ordering::Relaxed);
}

/// The counters of the run as the pure rules read them.
fn summary() -> Summary {
    Summary {
        render: crate::ddi::submit_command::FLUSH_GATE_RECORDS.load(Ordering::Relaxed),
        submit: SUBMITS.load(Ordering::Relaxed),
        batched: BATCHED.load(Ordering::Relaxed),
        matched: MATCHED.load(Ordering::Relaxed),
        mismatched: MISMATCHED.load(Ordering::Relaxed),
        immediate: IMMEDIATE.load(Ordering::Relaxed),
        retired: RETIRED.load(Ordering::Relaxed),
        rebased: REBASED.load(Ordering::Relaxed),
        lag_sum_us: LAG_SUM_US.load(Ordering::Relaxed),
        overlap: OVERLAP.load(Ordering::Relaxed),
    }
}

/// Registry names of the newest events, `FlGEv0` the newest.
const EVENT_NAMES: [&[u8]; 8] = [
    b"FlGEv0", b"FlGEv1", b"FlGEv2", b"FlGEv3", b"FlGEv4", b"FlGEv5", b"FlGEv6", b"FlGEv7",
];

/// Mirror the counters and the newest ring events into the registry. PASSIVE only.
///
/// Read order: `FlGRec` (Renders) against `FlGSub` (packets submitted): fewer submits than
/// Renders means dxgkrnl never submitted some packets. `FlGMis` / `FlGEmpty`: the decoded
/// record was not the merged one / absent. `FlGImm`: retired at SubmitCommand (the gate held
/// nothing). `FlGLagAvg` / `FlGLagMax`: how long queued fences really waited (microseconds).
/// `FlGUnord`: transport submissions while a gate was open. `FlGVerdict`: the pure reduction
/// (`flush_trace::Verdict`: 0 NoData, 1 NotSubmitted, 2 BoundaryLost, 3 RetiredAtSubmit,
/// 4 Rebased, 5 ConsumerUnordered, 6 GateHonest). `FlGEv0..7`: the newest events packed by
/// `flush_trace::pack_event` (kind 2 bits, flags 8, aux 22; kind 1 RENDER, 2 SUBMIT, 3
/// RETIRE; 0 empty); `FlGEvSeq` is the ring's cursor.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    // Nothing to say (and ~25 registry writes saved per present edge) until a `HEFL` Render
    // has been seen or the knob is on: an absent `FlG*` value reads as zero.
    if RING.cursor() == 0 && sync_ms() == 0 {
        return;
    }
    let s = summary();
    // The `FlGSyncMs` value in force (clamped): 0 = off. Not published under the knob's own
    // name, which is the operator's input.
    rec(b"FlGSyncEff", sync_ms());
    rec(b"FlGSub", s.submit);
    rec(b"FlGBat", s.batched);
    rec(b"FlGMat", s.matched);
    rec(b"FlGMis", s.mismatched);
    rec(b"FlGEmpty", EMPTY.load(Ordering::Relaxed));
    rec(b"FlGExempt", EXEMPT.load(Ordering::Relaxed));
    rec(b"FlGSigFail", SIGNAL_FAILED.load(Ordering::Relaxed));
    rec(b"FlGSyncSkip", SYNC_SKIPPED.load(Ordering::Relaxed));
    rec(b"FlGImm", s.immediate);
    rec(b"FlGRet", s.retired);
    rec(b"FlGReb", s.rebased);
    let queued = s.retired.saturating_sub(s.immediate) as u64;
    let avg = if queued == 0 {
        0
    } else {
        (s.lag_sum_us / queued).min(u32::MAX as u64) as u32
    };
    rec(b"FlGLagAvg", avg);
    rec(b"FlGLagMax", LAG_MAX_US.load(Ordering::Relaxed));
    rec(b"FlGUnord", s.overlap);
    rec(b"FlGTblFull", TABLE_FULL.load(Ordering::Relaxed));
    rec(b"FlGAbn", ABANDONED.load(Ordering::Relaxed));
    rec(b"FlGVerdict", ft::verdict(&s) as u32);
    rec(b"FlGSyncWt", SYNC_WAITS.load(Ordering::Relaxed));
    rec(b"FlGSyncTmo", SYNC_TIMEOUTS.load(Ordering::Relaxed));
    rec(b"FlGSyncMaxUs", SYNC_MAX_US.load(Ordering::Relaxed));
    let head = RING.cursor();
    rec(b"FlGEvSeq", head);
    for (back, name) in EVENT_NAMES.iter().enumerate() {
        let packed = head
            .checked_sub(back as u32)
            .and_then(|seq| RING.read(seq))
            .map_or(0, |e| ft::pack_event(e.kind, e.flags, e.aux));
        rec(name, packed);
    }
}
