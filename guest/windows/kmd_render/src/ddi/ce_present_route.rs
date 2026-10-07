//! The copy-engine Present route (`RmCopyEngine` = 1, milestone M3c-2): the I/O half of the
//! decision, the dispatch and the completion. The pure rules are `helios_kmd_logic::ce_route`; the
//! RM calls are `virtio/rm_client/ce_route.rs`; the design, the decision table, the failure matrix
//! and the hardware procedure are `docs/rm-copy-engine-present.md` section 15.
//!
//! HOW A PRESENT TAKES THE ROUTE. Only a Blt the `BltAsync` entry decision admitted (a foreign
//! NVK-on-RM source into a KMD standard buffer, `BltAsync` 1, `ForeignCopy` 1, no snapshot)
//! reaches [`try_route`], before `blt_async::try_async`. With every fact of
//! `ce_route::decide` in favour it is queued exactly as a `BltAsync` DEFERRED copy is (the Venus
//! copy prepared, the WindowedBlt request queued under the producer's boundary, its token merged
//! into the Present's private record) and recorded here as a JOB keyed by that request. The HPD
//! worker then prepares the job (the producer's dup and GPU mapping, the destination's OS
//! descriptor: [`service`]) and, when the FIFO dispatches the request (admitted by SubmitCommand,
//! the producer's boundary ready, the destination writable and taken as `KmdWriter`), submits the
//! copy-engine push instead of the Venus copy ([`dispatch`]). Any refusal at any point leaves the
//! request what it would have been without the route: its Venus copy runs (`CeRtFall`,
//! `CeRtWhy`). The completion (polled: [`settle`]) terminalizes the token through
//! `VirtioGpu::complete_ce_blt` (the deferred copy's own terminal with the mirror forced off: the
//! copy engine wrote the very pages the destination's readers read), which hands the destination
//! back and retires the source's ledger ticket; the Present's DMA fence then retires on that
//! terminal, as a deferred copy's does. A copy that does not complete within
//! `ce_route::COMPLETE_MS` is DISCHARGED (the terminal with a failure: the fence retires, the
//! destination keeps the previous frame) and the destination is poisoned for good.
//!
//! KNOB. `RmCopyEngine` (read at StartDevice by `ce_channel::reset_for_start`). Anything but 1:
//! every entry point below is one relaxed load (`on`, or `ACTIVE`), nothing is queued, counted or
//! written, and the Present path is the one it was.
//!
//! LOCKING. `STATE` is a leaf spinlock over plain data (the jobs, the destinations, the route's
//! strikes). Order: `STATE` -> the channel's `STATE` (the submit, a poll); nothing else is taken
//! under it, nothing allocated, nothing waited on. A pin is never dropped under it. The worker
//! takes the content transaction BEFORE the channel's `IO_BUSY` (the paging path's order).

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use helios_kmd_logic::ce_present::{self as cp, Retire, SourceDesc, SourcePlan, SurfaceLayout};
use helios_kmd_logic::ce_record::ClientCheck;
use helios_kmd_logic::ce_route::{self as cr, Desc, Dst, Facts, JobState, Jobs, Prep, RetirePlan, Why};
use helios_kmd_logic::rm_ce_channel as cc;

use crate::adapter::{AdapterContext, GuestPin, SystemBackingGuard};
use crate::ddi::present_packet::PresentSubmissionPrivate;
use crate::device::{ContextHandleRef, StashedCeRecord};
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::rm_client::ce_route as rio;
use crate::virtio::venus::{OptimalPresentImageDesc, PresentDestinationDesc};
use crate::virtio::VirtioError;

// ---- counters (`helios_kmd_logic::ce_route::COUNTERS`, written only here) ------------------------

static SEEN: AtomicU32 = AtomicU32::new(0);
static ROUTED: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static FALL: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static MASK: AtomicU32 = AtomicU32::new(0);
static DISP_FALL: AtomicU32 = AtomicU32::new(0);
static CLIENT: AtomicU32 = AtomicU32::new(0);
static STRIKE: AtomicU32 = AtomicU32::new(0);
static STRUCK: AtomicU32 = AtomicU32::new(0);
static CH_STRIKE: AtomicU32 = AtomicU32::new(0);
static OFF: AtomicU32 = AtomicU32::new(0);
static POISON: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);
static TIMEOUT: AtomicU32 = AtomicU32::new(0);
static UP: AtomicU32 = AtomicU32::new(0);
static INFL: AtomicU32 = AtomicU32::new(0);
static PEAK: AtomicU32 = AtomicU32::new(0);
static DEC_US: AtomicU32 = AtomicU32::new(0);
static DUP_US: AtomicU32 = AtomicU32::new(0);
static SUB_US: AtomicU32 = AtomicU32::new(0);
static DONE_US: AtomicU32 = AtomicU32::new(0);
static DONE_MAX: AtomicU32 = AtomicU32::new(0);
static POLL_US: AtomicU32 = AtomicU32::new(0);
static DST_NEW: AtomicU32 = AtomicU32::new(0);
static DST_DROP: AtomicU32 = AtomicU32::new(0);
static DST_LIVE: AtomicU32 = AtomicU32::new(0);
static RUNS: AtomicU32 = AtomicU32::new(0);

/// Nonzero while any job or destination record exists: every hook's fast exit.
static ACTIVE: AtomicU32 = AtomicU32::new(0);
/// A Present found the channel down and asked the worker for a bring-up.
static WANT_UP: AtomicU32 = AtomicU32::new(0);

fn now_100ns() -> u64 {
    crate::ddi::blt_async::now_100ns()
}

fn now_ms() -> u64 {
    now_100ns() / 10_000
}

fn add_us(cell: &AtomicU32, t0: u64) -> u32 {
    let us = cr::us(t0, now_100ns());
    cell.fetch_add(us, Ordering::Relaxed);
    us
}

/// `RmCopyEngine` is 1. One relaxed load.
fn on() -> bool {
    crate::virtio::rm_client::ce_channel::knob_mode() == cc::Mode::Route
}

/// A new generation (StartDevice, PASSIVE): zero the counters and write zeros over their values
/// (only with the route on: with the knob at 0 nothing here writes the registry).
pub(crate) fn reset_for_start() {
    for c in [
        &SEEN, &ROUTED, &DONE, &FALL, &WHY, &MASK, &DISP_FALL, &CLIENT, &STRIKE, &STRUCK,
        &CH_STRIKE, &OFF, &POISON, &LEAK, &TIMEOUT, &UP, &INFL, &PEAK, &DEC_US, &DUP_US, &SUB_US,
        &DONE_US, &DONE_MAX, &POLL_US, &DST_NEW, &DST_DROP, &DST_LIVE, &RUNS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    WANT_UP.store(0, Ordering::Relaxed);
    if on() {
        write_counters();
    }
}

/// Mirror the counters once the route saw a Present. PASSIVE only; with the NVRM counter block.
pub(crate) fn publish_counters() {
    if SEEN.load(Ordering::Relaxed) == 0 && UP.load(Ordering::Relaxed) == 0 {
        return;
    }
    write_counters();
}

fn write_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"CeRtSeen", SEEN.load(Ordering::Relaxed));
    rec(b"CeRtRouted", ROUTED.load(Ordering::Relaxed));
    rec(b"CeRtDone", DONE.load(Ordering::Relaxed));
    rec(b"CeRtFall", FALL.load(Ordering::Relaxed));
    rec(b"CeRtWhy", WHY.load(Ordering::Relaxed));
    rec(b"CeRtMask", MASK.load(Ordering::Relaxed));
    rec(b"CeRtDispFall", DISP_FALL.load(Ordering::Relaxed));
    rec(b"CeRtClient", CLIENT.load(Ordering::Relaxed));
    rec(b"CeRtStrike", STRIKE.load(Ordering::Relaxed));
    rec(b"CeRtStruck", STRUCK.load(Ordering::Relaxed));
    rec(b"CeRtChStrike", CH_STRIKE.load(Ordering::Relaxed));
    rec(b"CeRtOff", OFF.load(Ordering::Relaxed));
    rec(b"CeRtPoison", POISON.load(Ordering::Relaxed));
    rec(b"CeRtLeak", LEAK.load(Ordering::Relaxed));
    rec(b"CeRtTimeout", TIMEOUT.load(Ordering::Relaxed));
    rec(b"CeRtUp", UP.load(Ordering::Relaxed));
    rec(b"CeRtInfl", INFL.load(Ordering::Relaxed));
    rec(b"CeRtPeak", PEAK.load(Ordering::Relaxed));
    rec(b"CeRtDecUs", DEC_US.load(Ordering::Relaxed));
    rec(b"CeRtDupUs", DUP_US.load(Ordering::Relaxed));
    rec(b"CeRtSubUs", SUB_US.load(Ordering::Relaxed));
    rec(b"CeRtDoneUs", DONE_US.load(Ordering::Relaxed));
    rec(b"CeRtDoneMax", DONE_MAX.load(Ordering::Relaxed));
    rec(b"CeRtPollUs", POLL_US.load(Ordering::Relaxed));
    rec(b"CeRtDstNew", DST_NEW.load(Ordering::Relaxed));
    rec(b"CeRtDstDrop", DST_DROP.load(Ordering::Relaxed));
    rec(b"CeRtDstLive", DST_LIVE.load(Ordering::Relaxed));
    rec(b"CeRtRuns", RUNS.load(Ordering::Relaxed));
}

/// A Present (or a dispatch) keeps the Venus copy.
fn fall(why: Why) {
    FALL.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if why.at_dispatch() || why == Why::Stale {
        DISP_FALL.fetch_add(1, Ordering::Relaxed);
    }
    if why == Why::Client {
        CLIENT.fetch_add(1, Ordering::Relaxed);
    }
}

fn infl_add() {
    let now = INFL.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn infl_sub() {
    let _ = INFL.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

// ---- state --------------------------------------------------------------------------------------

/// What a job carries from the Present to the dispatch.
#[derive(Clone, Copy)]
struct Payload {
    rec: StashedCeRecord,
    plan: SourcePlan,
    remap: cp::Remap,
    dst_pitch: u32,
    lines: u32,
}

/// One destination: the pure record, the pin of its lease set while a descriptor may name it,
/// and what the worker needs to make the descriptor.
struct DstEntry {
    dst: Dst,
    pin: Option<GuestPin>,
    /// `[0, cover)`: `pitch * height` rounded up to pages (`guest_blob::cover_len`).
    cover: u64,
}

struct State {
    jobs: Jobs<Payload, { cr::MAX_JOBS }>,
    dsts: [Option<DstEntry>; cr::MAX_DSTS],
    chan: cr::Chan,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    jobs: Jobs::new(),
    dsts: [const { None }; cr::MAX_DSTS],
    chan: cr::Chan::new(),
});

impl State {
    fn dst_index(&self, resource_id: u32) -> Option<usize> {
        self.dsts
            .iter()
            .position(|d| d.as_ref().is_some_and(|d| d.dst.resource_id == resource_id))
    }

    fn dst(&mut self, resource_id: u32) -> Option<&mut DstEntry> {
        let i = self.dst_index(resource_id)?;
        self.dsts[i].as_mut()
    }

    fn free_dst_slot(&self) -> Option<usize> {
        self.dsts.iter().position(Option::is_none)
    }

    fn idle(&self) -> bool {
        self.jobs.is_empty() && self.dsts.iter().all(Option::is_none)
    }
}

fn refresh_active(g: &State) {
    ACTIVE.store(u32::from(!g.idle()), Ordering::Release);
}

/// A destination failed (a strike): count it, and once it is struck out.
fn strike_dst(d: &mut Dst, why: cp::Why) {
    let was = d.route.is_disabled();
    d.route.on_failure(why);
    STRIKE.fetch_add(1, Ordering::Relaxed);
    if !was && d.route.is_disabled() {
        STRUCK.fetch_add(1, Ordering::Relaxed);
    }
}

/// A route strike (a channel that broke with copies in flight, a copy that never completed).
fn strike_route(g: &mut State) {
    CH_STRIKE.fetch_add(1, Ordering::Relaxed);
    if g.chan.strike() {
        OFF.store(1, Ordering::Relaxed);
    }
}

// ---- the Present --------------------------------------------------------------------------------

/// The destination of the Blt as the Present arm resolved it.
#[derive(Clone, Copy)]
pub(crate) struct DstInfo {
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub dxgi_format: u32,
    pub alloc_size: u64,
}

/// The Blt arm's `BltAsync` branch, before `blt_async::try_async`: take the copy-engine route
/// when `ce_route::decide` says so. `Some(token)`: the copy is queued (its Venus copy prepared as
/// the fallback) and the Present's private record carries the request's token; the caller
/// completes the Present as a deferred copy's (`present_complete`). `None`: nothing was queued or
/// merged and the caller continues exactly as without the route (the reason was counted). One
/// relaxed load with the knob at anything but 1. PASSIVE (`DxgkDdiPresent`), no lock held.
///
/// # Safety
/// `args` is dxgkrnl's `DXGKARG_PRESENT` for this call and its private-data pointer is writable
/// for `DmaBufferPrivateDataSize` bytes (checked by the Blt arm before any host work).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(crate) unsafe fn try_route(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    context: Option<&ContextHandleRef<'_>>,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    dst: DstInfo,
    boundary: Option<u64>,
) -> Option<u64> {
    if !on() {
        return None;
    }
    let t0 = now_100ns();
    SEEN.fetch_add(1, Ordering::Relaxed);
    let decided = decide_present(adapter, context, destination, dst, boundary);
    add_us(&DEC_US, t0);
    let (payload, boundary) = match decided {
        Ok(v) => v,
        Err(why) => {
            fall(why);
            return None;
        }
    };
    // Queued exactly as a deferred `BltAsync` copy is (`ddi/blt_async.rs::deferred`): the Venus
    // copy prepared now is the fallback the worker submits whenever the copy engine does not.
    let no_mirror_knob = crate::ddi::blt_async::no_mirror_on();
    let queued = adapter.with_scanout_lifecycle(passive, |lock| {
        let prepared = lock.with_venus_client(|client| {
            client.prepare_present_blt_guest(adapter, source, destination)
        });
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(VirtioError::DeviceError),
        };
        let no_mirror = no_mirror_knob || prepared.guest_target();
        adapter
            .with_virtio(|v| {
                v.queue_async_blt(adapter, source, destination, prepared, boundary, no_mirror)
            })
            .unwrap_or(Err(VirtioError::DeviceError))
    });
    let Ok(token) = queued else {
        fall(Why::Queue);
        return None;
    };
    // The job before the token reaches the private record: SubmitCommand (which admits the
    // request) cannot run before this Present returns, so the worker cannot dispatch it earlier.
    // A job that could not be recorded leaves the request a plain deferred Venus copy.
    let recorded = {
        let mut g = STATE.lock();
        let ok = ensure_dst(&mut g, dst) && g.jobs.add(token, boundary, dst.resource_id, payload);
        refresh_active(&g);
        ok
    };
    // SAFETY: the caller's contract.
    if let Err(_status) = unsafe {
        PresentSubmissionPrivate::merge_windowed_blt_token(
            args.pDmaBufferPrivateData,
            args.DmaBufferPrivateDataSize,
            token,
            boundary,
        )
    } {
        // Nothing was submitted: the request goes, and the Present continues without the route.
        let _ = adapter.with_virtio(|v| v.cancel_windowed_blt(adapter, token, boundary));
        let mut g = STATE.lock();
        let _ = g.jobs.remove(token, boundary);
        refresh_active(&g);
        drop(g);
        fall(Why::Queue);
        return None;
    }
    if recorded {
        ROUTED.fetch_add(1, Ordering::Relaxed);
        // The worker prepares the job (dup, descriptor) before the request can be dispatched.
        adapter.signal_hpd();
    } else {
        fall(Why::Full);
    }
    Some(token)
}

/// The destination's record, made when there is none (a free slot). `false`: no room.
fn ensure_dst(g: &mut State, dst: DstInfo) -> bool {
    if g.dst_index(dst.resource_id).is_some() {
        return true;
    }
    let Some(slot) = g.free_dst_slot() else {
        return false;
    };
    let Ok(cover) = helios_kmd_logic::guest_blob::cover_len(dst.pitch, dst.height, dst.alloc_size)
    else {
        return false;
    };
    g.dsts[slot] = Some(DstEntry {
        dst: Dst::new(dst.resource_id, slot as u8),
        pin: None,
        cover,
    });
    true
}

/// Gather `ce_route::Facts` and decide. The record is taken (read and cleared) only for the
/// Present whose own fence it travelled with.
fn decide_present(
    adapter: &AdapterContext,
    context: Option<&ContextHandleRef<'_>>,
    destination: PresentDestinationDesc,
    dst: DstInfo,
    boundary: Option<u64>,
) -> Result<(Payload, u64), Why> {
    let Some(boundary) = boundary else {
        return Err(Why::NoBoundary);
    };
    let Some(rec) = context.and_then(|c| c.take_ce_record(boundary)) else {
        return Err(Why::NoRecord);
    };
    let presenter = context.and_then(ContextHandleRef::creator_process).unwrap_or(0);
    let client = helios_kmd_logic::ce_record::both_owned(
        crate::ddi::ce_record::record_client_owned_by_presenter(
            adapter,
            presenter,
            rec.record.semaphore.h_client,
        ),
        crate::ddi::ce_record::record_client_owned_by_presenter(
            adapter,
            presenter,
            rec.record.source.h_client,
        ),
    );
    let view = rio::chan_view();
    let route_off = view.disabled || STATE.lock().chan.off();
    if !view.up && view.may_bring_up && !route_off && client == ClientCheck::Owned {
        // The lazy bring-up is the worker's (never inside this DDI).
        if WANT_UP.swap(1, Ordering::AcqRel) == 0 {
            UP.fetch_add(1, Ordering::Relaxed);
            adapter.signal_hpd();
        }
    }
    let plan = match view.gen {
        Some(gen) if view.up => source_facts(gen, &rec, dst),
        _ => Err(Why::ChannelDown),
    };
    let destination_ok = destination_facts(adapter, destination, dst, presenter);
    let (dst_state, room) = {
        let g = STATE.lock();
        let state = match g.dst_index(dst.resource_id).and_then(|i| g.dsts[i].as_ref()) {
            Some(e) => e.dst.admits(),
            None => Ok(()),
        };
        let room = g.jobs.has_room()
            && (g.dst_index(dst.resource_id).is_some() || g.free_dst_slot().is_some());
        (state, room)
    };
    let facts = Facts {
        boundary: true,
        record: true,
        client,
        route_off,
        channel_up: view.up,
        source: plan.map(|_| ()),
        destination: destination_ok,
        dst_state,
        room,
    };
    cr::decide(&facts)?;
    let (plan, remap) = plan?;
    Ok((
        Payload {
            rec,
            plan,
            remap,
            dst_pitch: dst.pitch,
            lines: dst.height,
        },
        boundary,
    ))
}

/// The source plan and the remap, or why the source is refused.
fn source_facts(
    gen: cp::Gen,
    rec: &StashedCeRecord,
    dst: DstInfo,
) -> Result<(SourcePlan, cp::Remap), Why> {
    let s = rec.record.source;
    let desc = SourceDesc {
        offset: s.offset,
        size: s.size,
        modifier: s.modifier,
        pitch: s.pitch,
        width: s.width,
        height: s.height,
        fourcc: s.fourcc,
        compressed: s.flags & helios_protocol::HELIOS_RM_COPY_SOURCE_FLAG_COMPRESSED != 0,
    };
    let plan = cp::source_plan(gen, &desc).map_err(|_| Why::Source)?;
    let dst_fourcc = cp::dst_fourcc_for_dxgi(dst.dxgi_format).ok_or(Why::Format)?;
    let remap = cp::remap_for(s.fourcc, dst_fourcc).map_err(|_| Why::Format)?;
    if s.width != dst.width || s.height != dst.height || plan.line_bytes > dst.pitch {
        return Err(Why::Extent);
    }
    Ok((plan, remap))
}

/// The destination: a standard buffer with system backing that fits a window, whose system copy
/// is current, that no other process has open, and that has no guest blob.
fn destination_facts(
    adapter: &AdapterContext,
    destination: PresentDestinationDesc,
    dst: DstInfo,
    presenter: usize,
) -> Result<(), Why> {
    if !matches!(destination, PresentDestinationDesc::StandardBuffer(_)) {
        return Err(Why::Destination);
    }
    let cover = helios_kmd_logic::guest_blob::cover_len(dst.pitch, dst.height, dst.alloc_size)
        .map_err(|_| Why::Destination)?;
    if !cr::fits_window(cover) || !adapter.system_backings.is_backed(dst.resource_id) {
        return Err(Why::Destination);
    }
    if adapter.system_backings.system_copy_invalid(dst.resource_id) {
        return Err(Why::Stale);
    }
    if adapter
        .with_virtio(|v| v.present_buffer_foreign_open(dst.resource_id, presenter))
        .unwrap_or(true)
    {
        return Err(Why::Foreign);
    }
    if adapter
        .system_backings
        .guest_record(dst.resource_id)
        .is_some_and(|r| !r.may_unlock())
    {
        return Err(Why::GuestBlob);
    }
    Ok(())
}

/// Whether the route holds `resource_id` (a destination record with a descriptor, or one being
/// made, drained or leaked): `GuestBlob` makes no guest blob for it then (a destination uses one
/// of the two, never both). One relaxed load while the route holds nothing.
pub(crate) fn holds(resource_id: u32) -> bool {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return false;
    }
    let mut g = STATE.lock();
    g.dst(resource_id)
        .is_some_and(|e| !matches!(e.dst.desc, Desc::Absent | Desc::Uncovered))
}

// ---- the dispatch -------------------------------------------------------------------------------

/// The HPD worker's WindowedBlt dispatch took request `(token, boundary)` (admitted, its producer
/// boundary ready, its destination taken as `KmdWriter`). `true`: the copy engine has it (the
/// completion terminalizes it, [`settle`]); `false`: the caller submits the request's Venus copy
/// exactly as it would have (no job, or a fallback, counted). Spinlocks only (called under the
/// scanout lifecycle and the Venus mutex): the job, the destination's descriptor and the producer
/// mapping were made beforehand by [`service`].
pub(crate) fn dispatch(adapter: &AdapterContext, token: u64, boundary: u64) -> bool {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return false;
    }
    let t0 = now_100ns();
    let completed = rio::poll().map(|(c, _)| c);
    let mut g = STATE.lock();
    let Some(job) = g.jobs.find(token, boundary).copied() else {
        return false;
    };
    let chan_up = rio::chan_view().up;
    let stale = adapter.system_backings.system_copy_invalid(job.dst);
    let va = g.dst(job.dst).map_or(Err(Why::Retiring), |e| e.dst.ready_va());
    let ready = match (job.prep, va, completed) {
        _ if stale => Err(Why::Stale),
        (Prep::Pending, _, _) => Err(Why::NotReady),
        (Prep::Failed(w), _, _) => Err(w),
        (_, Err(w), _) => Err(w),
        (_, _, None) => Err(Why::ChannelLost),
        _ if !chan_up => Err(Why::ChannelLost),
        (Prep::Ready { sem_va, src_va }, Ok(dst_va), Some(c)) => Ok((sem_va, src_va, dst_va, c)),
    };
    let (sem_va, src_va, dst_va, completed) = match ready {
        Ok(v) => v,
        Err(why) => {
            let _ = g.jobs.remove(token, boundary);
            refresh_active(&g);
            drop(g);
            fall(why);
            return false;
        }
    };
    let p = job.payload;
    let copy = cp::CopyRect {
        src_va: src_va.wrapping_add(p.plan.offset),
        dst_va,
        src_pitch: p.rec.record.source.pitch,
        dst_pitch: p.dst_pitch,
        line_bytes: p.plan.line_bytes,
        lines: p.lines,
        layout: p.plan.layout,
        dst_layout: SurfaceLayout::Pitch,
        remap: p.remap,
        stamp: None,
    };
    let acquire = cp::Acquire {
        va: sem_va,
        value: p.rec.record.semaphore.value,
    };
    match rio::submit(acquire, &copy) {
        Ok(value) => {
            let now = now_100ns();
            if let Some(j) = g.jobs.find_mut(token, boundary) {
                j.state = JobState::Submitted { value, t_submit: now };
            }
            if let Some(e) = g.dst(job.dst) {
                e.dst.route.on_submit(value, completed);
                // The producer's boundary was ready at the dispatch: the clock starts now.
                e.dst.route.on_producer_fired(now / 10_000, completed);
            }
            drop(g);
            infl_add();
            add_us(&SUB_US, t0);
            true
        }
        Err(fail) => {
            if fail != rio::SubmitFail::RingFull {
                if let Some(e) = g.dst(job.dst) {
                    strike_dst(&mut e.dst, cp::Why::RmError);
                }
            }
            let _ = g.jobs.remove(token, boundary);
            refresh_active(&g);
            drop(g);
            fall(Why::Submit);
            false
        }
    }
}

// ---- the worker ---------------------------------------------------------------------------------

/// The worker's wait while the route has work: the earlier of `due` and the route's poll
/// (`ce_route::POLL_DUE_100NS`) while copies are in flight or jobs need preparing. `due` unchanged
/// with the knob off or nothing to do (one relaxed load).
pub(crate) fn fold_due(optional: bool, due: Option<i64>) -> Option<i64> {
    if !optional
        || (ACTIVE.load(Ordering::Acquire) == 0 && WANT_UP.load(Ordering::Relaxed) == 0)
    {
        return due;
    }
    let busy = WANT_UP.load(Ordering::Relaxed) != 0 || {
        let g = STATE.lock();
        g.jobs.in_flight() != 0 || g.jobs.first_pending().is_some()
    };
    if !busy {
        return due;
    }
    // Relative (negative) times: the earlier is the larger.
    Some(due.map_or(cr::POLL_DUE_100NS, |d| d.max(cr::POLL_DUE_100NS)))
}

/// One HPD worker pass, before the WindowedBlt dispatch (PASSIVE, no lock held): the lazy channel
/// bring-up, the completions, a broken channel's teardown, the preparation of queued jobs and the
/// removal of jobs whose request is gone. One relaxed load with the knob off.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    if !on() {
        return;
    }
    if WANT_UP.swap(0, Ordering::AcqRel) != 0 && !STATE.lock().chan.off() {
        rio::bring_up(passive, adapter);
    }
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    settle(passive, adapter, false);
    // `poll` (inside `settle`) breaks a channel whose error notifier is set; a copy that timed
    // out broke it there too.
    if rio::chan_view().broken {
        fail_channel(passive, adapter);
    }
    prune(adapter);
    prepare(passive, adapter);
    let g = STATE.lock();
    refresh_active(&g);
}

/// After the WindowedBlt dispatch of the same pass: spin briefly for a copy just submitted
/// (`ce_route::SETTLE_SPIN_US`; a 1600x900 copy takes 0.2 to 0.35 ms), then let the worker's
/// timed wait poll the rest. One relaxed load with nothing in flight.
#[inline(never)]
pub(crate) fn settle_after_dispatch(passive: PassiveLevel, adapter: &AdapterContext) {
    if ACTIVE.load(Ordering::Acquire) == 0 || INFL.load(Ordering::Relaxed) == 0 {
        return;
    }
    settle(passive, adapter, true);
}

/// Retire every submitted job the completion value (or a timeout) decides; with `spin`, keep
/// polling up to `SETTLE_SPIN_US` while copies are in flight. A copy that timed out poisons its
/// destination for good, discharges its Present and marks the channel broken (it may be stuck on
/// that copy). No channel at all (torn down under copies in flight): every submitted job is
/// discharged.
fn settle(_passive: PassiveLevel, adapter: &AdapterContext, spin: bool) {
    let t0 = now_100ns();
    let spin_until = t0 + cr::SETTLE_SPIN_US * 10;
    loop {
        let progress = rio::poll();
        let mut done: [(u64, u64, bool, u64); cr::MAX_JOBS] = [(0, 0, false, 0); cr::MAX_JOBS];
        let mut n = 0usize;
        let mut timed_out_any = false;
        let more = {
            let mut g = STATE.lock();
            let now_ms = now_ms();
            let mut timed = [0u32; cr::MAX_DSTS];
            match progress {
                Some((completed, _)) => {
                    for (i, slot) in g.dsts.iter_mut().enumerate() {
                        let Some(e) = slot.as_mut() else { continue };
                        if e.dst.route.poll(completed, now_ms) == cp::Poll::TimedOut {
                            timed[i] = e.dst.resource_id;
                            // Never the route again; the pages stay pinned for the generation.
                            e.dst.desc = Desc::Leaked;
                            POISON.fetch_add(1, Ordering::Relaxed);
                            LEAK.fetch_add(1, Ordering::Relaxed);
                            STRIKE.fetch_add(1, Ordering::Relaxed);
                            timed_out_any = true;
                        }
                    }
                    while let Some((job, r)) =
                        g.jobs.take_settled(completed, |d| d != 0 && timed.contains(&d))
                    {
                        if let JobState::Submitted { t_submit, .. } = job.state {
                            done[n] = (job.token, job.boundary, r == Retire::Retire, t_submit);
                            n += 1;
                        }
                    }
                }
                None => {
                    // No channel: nothing will ever complete what was submitted. Its
                    // destinations may still be written by a copy RM did not cancel: poisoned.
                    while let Some(job) =
                        g.jobs.take_first(|j| matches!(j.state, JobState::Submitted { .. }))
                    {
                        if let JobState::Submitted { t_submit, .. } = job.state {
                            done[n] = (job.token, job.boundary, false, t_submit);
                            n += 1;
                        }
                        poison_leak(&mut g, job.dst, 0);
                    }
                }
            }
            if timed_out_any {
                strike_route(&mut g);
            }
            g.jobs.in_flight() != 0
        };
        for &(token, boundary, ok, t_submit) in done.iter().take(n) {
            let _ = adapter.with_virtio(|v| v.complete_ce_blt(adapter, token, boundary, ok));
            infl_sub();
            if ok {
                DONE.fetch_add(1, Ordering::Relaxed);
                let us = add_us(&DONE_US, t_submit);
                DONE_MAX.fetch_max(us, Ordering::Relaxed);
            } else {
                TIMEOUT.fetch_add(1, Ordering::Relaxed);
            }
        }
        if timed_out_any {
            rio::mark_broken();
        }
        if !spin || !more || progress.is_none() || now_100ns() >= spin_until {
            break;
        }
        core::hint::spin_loop();
    }
    add_us(&POLL_US, t0);
}

/// The channel broke (its error notifier, a copy that never completed) or its service struck
/// out: every submitted copy is discharged and its destination poisoned, the queued jobs fall
/// back at their dispatch, every descriptor is freed (or leaked), and the channel is torn down
/// (the next Present that finds it down asks for a bring-up again, until three route strikes).
#[inline(never)]
fn fail_channel(passive: PassiveLevel, adapter: &AdapterContext) {
    let completed = rio::poll().map_or(0, |(c, _)| c);
    let mut discharged: [(u64, u64); cr::MAX_JOBS] = [(0, 0); cr::MAX_JOBS];
    let mut n = 0usize;
    {
        let mut g = STATE.lock();
        let mut any = false;
        while let Some(job) = g.jobs.take_first(|j| matches!(j.state, JobState::Submitted { .. })) {
            discharged[n] = (job.token, job.boundary);
            n += 1;
            any = true;
            poison_leak(&mut g, job.dst, completed);
        }
        // The dups go with the teardown: no queued job may use what was prepared for it (its
        // dispatch takes the Venus copy).
        for j in g.jobs.iter_mut() {
            if j.state == JobState::Queued {
                j.prep = Prep::Failed(Why::ChannelLost);
            }
        }
        if any {
            strike_route(&mut g);
        }
    }
    for &(token, boundary) in discharged.iter().take(n) {
        let _ = adapter.with_virtio(|v| v.complete_ce_blt(adapter, token, boundary, false));
        infl_sub();
        TIMEOUT.fetch_add(1, Ordering::Relaxed);
    }
    free_all_descriptors(passive, adapter, 0);
    if rio::teardown_channel(passive, adapter) {
        // Its client is gone: nothing the old ring ran can still write, and the next channel
        // counts from its own start. Strikes stay; leaked destinations stay leaked.
        let mut g = STATE.lock();
        for e in g.dsts.iter_mut().flatten() {
            e.dst.route.on_channel_gone();
        }
    }
}

/// A copy into `dst` was in flight when its channel failed: poisoned (a strike), and never routed
/// again (its pages stay pinned until the generation ends: RM may not have cancelled the copy).
fn poison_leak(g: &mut State, dst: u32, completed: u64) {
    if let Some(e) = g.dst(dst) {
        e.dst.route.on_channel_failed(completed);
        e.dst.desc = Desc::Leaked;
        POISON.fetch_add(1, Ordering::Relaxed);
        LEAK.fetch_add(1, Ordering::Relaxed);
        STRIKE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Free every destination descriptor that nothing can still write (the others are leaked: their
/// pins stay until the transport generation ends). `wait_io_ms`: how long to wait for the
/// channel's I/O (0 on the worker).
fn free_all_descriptors(passive: PassiveLevel, adapter: &AdapterContext, wait_io_ms: u64) {
    let completed = rio::poll().map(|(c, _)| c);
    for i in 0..cr::MAX_DSTS {
        let plan = {
            let mut g = STATE.lock();
            let Some(e) = g.dsts[i].as_mut() else { continue };
            let plan = match completed {
                Some(c) => cr::retire_plan(&e.dst, c),
                None => match e.dst.desc {
                    Desc::Absent | Desc::Uncovered => RetirePlan::Nothing,
                    _ => RetirePlan::Leak,
                },
            };
            match (plan, e.dst.desc) {
                (RetirePlan::Free, Desc::Ready { va, len }) => {
                    e.dst.desc = Desc::Draining;
                    Some((e.dst.slot, va, len))
                }
                // A `Draining` one belongs to the paging thread retiring it.
                (RetirePlan::Wait(_) | RetirePlan::Leak, Desc::Ready { .. }) => {
                    e.dst.desc = Desc::Leaked;
                    LEAK.fetch_add(1, Ordering::Relaxed);
                    None
                }
                _ => None,
            }
        };
        if let Some((slot, va, len)) = plan {
            let freed = rio::free_dst(passive, adapter, slot, va, len, wait_io_ms, cr::FREE_MS);
            finish_free(i, freed);
        }
    }
}

/// The free of slot index `i`'s descriptor answered `freed`: `Absent` and unpinned, or leaked.
fn finish_free(i: usize, freed: bool) {
    let pin = {
        let mut g = STATE.lock();
        let Some(e) = g.dsts[i].as_mut() else { return };
        if freed {
            e.dst.desc = Desc::Absent;
            DST_DROP.fetch_add(1, Ordering::Relaxed);
            let _ = DST_LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
            e.pin.take()
        } else {
            e.dst.desc = Desc::Leaked;
            LEAK.fetch_add(1, Ordering::Relaxed);
            None
        }
    };
    // PASSIVE, outside the spinlock: may release the last owner of a lease.
    drop(pin);
}

/// Remove the queued jobs whose WindowedBlt request is gone (cancelled with its resource, its
/// stream or its scheduler epoch): nothing will ever dispatch them.
fn prune(adapter: &AdapterContext) {
    for _ in 0..cr::MAX_JOBS {
        let queued: [(u64, u64); cr::MAX_JOBS] = {
            let g = STATE.lock();
            let mut out = [(0u64, 0u64); cr::MAX_JOBS];
            for (k, j) in g.jobs.iter().filter(|j| j.state == JobState::Queued).enumerate() {
                out[k] = (j.token, j.boundary);
            }
            out
        };
        let gone = queued.iter().copied().find(|&(token, boundary)| {
            token != 0
                && adapter
                    .with_virtio(|v| v.ce_blt_request(token, boundary))
                    .ok()
                    .flatten()
                    .is_none()
        });
        let Some((token, boundary)) = gone else {
            return;
        };
        let mut g = STATE.lock();
        if g.jobs.find(token, boundary).is_some_and(|j| j.state == JobState::Queued) {
            let _ = g.jobs.remove(token, boundary);
        }
        refresh_active(&g);
    }
}

/// Prepare the queued jobs: the destination's descriptor (under the content transaction), then
/// the producer's dup and mapping. At most `MAX_JOBS` per pass; a job whose step is busy stays
/// pending for the next pass (its dispatch falls back meanwhile).
fn prepare(passive: PassiveLevel, adapter: &AdapterContext) {
    for _ in 0..cr::MAX_JOBS {
        let next = {
            let g = STATE.lock();
            g.jobs.first_pending().map(|j| (j.token, j.boundary, j.dst, j.payload.rec))
        };
        let Some((token, boundary, dst, rec)) = next else {
            return;
        };
        let t0 = now_100ns();
        let prep = match ensure_descriptor(passive, adapter, dst) {
            Err(Why::NotReady) => {
                // Busy (the content transaction or the channel's I/O): try again next pass.
                add_us(&DUP_US, t0);
                return;
            }
            Err(why) => Prep::Failed(why),
            Ok(()) => match rio::prep_producer(passive, adapter, &rec) {
                Ok(p) => Prep::Ready {
                    sem_va: p.sem_va,
                    src_va: p.src_va,
                },
                Err(f) if rio::is_busy(&f) => {
                    add_us(&DUP_US, t0);
                    return;
                }
                Err(_) => {
                    if let Some(e) = STATE.lock().dst(dst) {
                        strike_dst(&mut e.dst, cp::Why::RmError);
                    }
                    Prep::Failed(Why::Dup)
                }
            },
        };
        add_us(&DUP_US, t0);
        if let Some(j) = STATE.lock().jobs.find_mut(token, boundary) {
            j.prep = prep;
        }
    }
}

/// Make `resource_id`'s descriptor if it has none: under the content transaction (so no lease
/// changes between the snapshot and the registration), the lease pages covering
/// `[0, round_up(pitch * height))` as page runs, a pin of exactly those leases, the registration
/// and the GPU mapping. `Err(NotReady)`: busy, try again; another reason: the job falls back.
#[inline(never)]
fn ensure_descriptor(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
) -> Result<(), Why> {
    {
        let mut g = STATE.lock();
        let Some(e) = g.dst(resource_id) else {
            return Err(Why::Full);
        };
        match e.dst.desc {
            Desc::Ready { .. } => return Ok(()),
            Desc::Absent => {}
            Desc::Creating => return Err(Why::NotReady),
            Desc::Uncovered => return Err(Why::Uncovered),
            Desc::Draining => return Err(Why::Retiring),
            Desc::Leaked => return Err(Why::Poisoned),
        }
        e.dst.admits()?;
    }
    let Some(guard) = adapter.system_backings.serialize(passive) else {
        return Err(Why::NotReady);
    };
    let r = create_descriptor(passive, adapter, &guard, resource_id);
    drop(guard);
    r
}

fn set_desc(resource_id: u32, desc: Desc) {
    if let Some(e) = STATE.lock().dst(resource_id) {
        e.dst.desc = desc;
    }
}

fn create_descriptor(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
) -> Result<(), Why> {
    // Read again under the transaction: a lease change may have retired it in between.
    let (slot, cover) = {
        let mut g = STATE.lock();
        let Some(e) = g.dst(resource_id) else {
            return Err(Why::Full);
        };
        if e.dst.desc != Desc::Absent {
            return Err(Why::NotReady);
        }
        e.dst.desc = Desc::Creating;
        (e.dst.slot, e.cover)
    };
    if guard.system_copy_invalid(resource_id) {
        set_desc(resource_id, Desc::Absent);
        return Err(Why::Stale);
    }
    if adapter
        .system_backings
        .guest_record(resource_id)
        .is_some_and(|r| !r.may_unlock())
    {
        set_desc(resource_id, Desc::Absent);
        return Err(Why::GuestBlob);
    }
    let Some(snapshot) = guard.snapshot(resource_id) else {
        set_desc(resource_id, Desc::Uncovered);
        return Err(Why::Uncovered);
    };
    let mut pieces = Vec::new();
    if !snapshot.pieces(&mut pieces) {
        set_desc(resource_id, Desc::Absent);
        return Err(Why::NotReady);
    }
    let mut runs: Vec<helios_kmd_logic::guest_blob::Run> = Vec::new();
    let counted = helios_kmd_logic::guest_blob::build_runs(&pieces, cover, |_| {});
    let Ok(count) = counted else {
        set_desc(resource_id, Desc::Uncovered);
        return Err(Why::Uncovered);
    };
    let pages = (cover / 4096) as usize;
    let mut pfns: Vec<u64> = Vec::new();
    if runs.try_reserve_exact(count).is_err() || pfns.try_reserve_exact(pages).is_err() {
        set_desc(resource_id, Desc::Absent);
        return Err(Why::NotReady);
    }
    let filled = helios_kmd_logic::guest_blob::build_runs(&pieces, cover, |run| runs.push(run));
    pfns.resize(pages, 0);
    if filled != Ok(count) || cr::pfns_of_runs(&runs, &mut pfns) != Some(pages) {
        set_desc(resource_id, Desc::Uncovered);
        return Err(Why::Uncovered);
    }
    // The pin holds every lease the registration names until the descriptor is freed.
    let Some(pin) = snapshot.pin() else {
        set_desc(resource_id, Desc::Absent);
        return Err(Why::NotReady);
    };
    drop(pieces);
    match rio::create_dst(passive, adapter, slot, &pfns, cover) {
        Ok(va) => {
            let mut g = STATE.lock();
            if let Some(e) = g.dst(resource_id) {
                e.dst.desc = Desc::Ready { va, len: cover };
                e.pin = Some(pin);
                drop(g);
                DST_NEW.fetch_add(1, Ordering::Relaxed);
                DST_LIVE.fetch_add(1, Ordering::Relaxed);
                RUNS.store(count as u32, Ordering::Relaxed);
                return Ok(());
            }
            // Unreachable (the record is removed only under this transaction): never unlock
            // pages a descriptor names.
            drop(g);
            core::mem::forget(pin);
            LEAK.fetch_add(1, Ordering::Relaxed);
            Err(Why::Desc)
        }
        Err(rio::DstFail::Clean(f)) => {
            let busy = rio::is_busy(&f);
            {
                let mut g = STATE.lock();
                if let Some(e) = g.dst(resource_id) {
                    e.dst.desc = Desc::Absent;
                    if !busy {
                        strike_dst(&mut e.dst, cp::Why::RmError);
                    }
                }
            }
            drop(pin);
            Err(if busy { Why::NotReady } else { Why::Desc })
        }
        Err(rio::DstFail::Unsure(_)) => {
            // The host may hold the registration: keep the pin until the generation ends.
            let mut g = STATE.lock();
            match g.dst(resource_id) {
                Some(e) => {
                    e.dst.desc = Desc::Leaked;
                    e.pin = Some(pin);
                    strike_dst(&mut e.dst, cp::Why::RmError);
                }
                None => core::mem::forget(pin),
            }
            drop(g);
            LEAK.fetch_add(1, Ordering::Relaxed);
            Err(Why::Desc)
        }
    }
}

// ---- the paging and destroy hooks ---------------------------------------------------------------

/// Before ANY change to `resource_id`'s leases (a paging transfer, a discard), with the content
/// transaction held: stop new copies (the descriptor leaves `Ready`), wait at most
/// `ce_route::DRAIN_MS` until the completion value reaches the destination's last submitted copy,
/// free the mapping and the descriptor (waiting at most `IO_WAIT_MS` for the channel's I/O), and
/// only then drop the pin, so the caller's lease change may unlock the pages. A drain that runs
/// out, a free that is not confirmed: the destination is leaked (pinned until the generation
/// ends) and never routed again. A destination found uncovered is looked at again after the
/// change. One relaxed load while the route holds nothing (always, with the knob off). PASSIVE.
pub(crate) fn before_lease_change(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    _guard: &SystemBackingGuard<'_>,
    resource_id: u32,
) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let (index, target, va, len, slot) = {
        let mut g = STATE.lock();
        let Some(i) = g.dst_index(resource_id) else {
            return;
        };
        let Some(e) = g.dsts[i].as_mut() else {
            return;
        };
        match e.dst.desc {
            Desc::Ready { va, len } => {
                e.dst.desc = Desc::Draining;
                (i, e.dst.route.submitted(), va, len, e.dst.slot)
            }
            Desc::Uncovered => {
                e.dst.desc = Desc::Absent;
                return;
            }
            _ => return,
        }
    };
    let t0 = now_100ns();
    let mut drained = false;
    let mut waited = 0u64;
    loop {
        match rio::poll() {
            Some((completed, 0)) if completed >= target => {
                drained = true;
                break;
            }
            Some((_, 0)) => {}
            // A broken or vanished channel: nothing says the copy stopped.
            _ => break,
        }
        if waited >= cr::DRAIN_MS {
            break;
        }
        crate::virtio::ctrl::sleep_ms(passive, 1);
        waited += 1;
    }
    if drained {
        // Terminalize what completed now (the worker would on its next pass): a destroy that
        // follows finds the request's copy ring-complete instead of retaining the allocation.
        settle(passive, adapter, false);
    }
    let freed = drained
        && rio::free_dst(passive, adapter, slot, va, len, cr::IO_WAIT_MS, cr::FREE_MS);
    finish_free(index, freed);
    if !freed {
        if let Some(e) = STATE.lock().dsts[index].as_mut() {
            e.dst.route.on_failure(cp::Why::Timeout);
        }
    }
    let _ = add_us(&POLL_US, t0);
}

/// The destination is destroyed (content transaction held): retire as for a lease change, then
/// forget its record and its queued jobs (a leaked record stays, pin and all, until the
/// generation ends). One relaxed load while the route holds nothing. PASSIVE.
pub(crate) fn destination_gone(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    before_lease_change(passive, adapter, guard, resource_id);
    let removed = {
        let mut g = STATE.lock();
        let taken = match g.dst_index(resource_id) {
            // A leaked record keeps its pin; a submitted copy would have leaked it.
            Some(i)
                if g.dsts[i].as_ref().is_some_and(|e| {
                    e.dst.desc != Desc::Leaked && !g.jobs.submitted_for(resource_id)
                }) =>
            {
                g.dsts[i].take()
            }
            _ => None,
        };
        refresh_active(&g);
        taken
    };
    // PASSIVE, outside the spinlock: may release the last owner of a lease.
    drop(removed);
}

/// StopDevice (the worker joined) and a StartDevice that finds an old transport, BEFORE the
/// channel's own `retire_for_stop`: free every destination descriptor nothing can still write
/// and release the producer dups while the transport answers, on the caller's budget. What is
/// leaked keeps its pin until [`forget`] (after the transport reset). One relaxed load with
/// nothing held. PASSIVE.
pub(crate) fn retire_for_stop(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    budget: &helios_kmd_logic::sweep_budget::SweepBudget,
) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    if budget
        .call_timeout_ms(crate::adapter::foreign_scanout::now_100ns())
        .is_none()
    {
        return;
    }
    free_all_descriptors(passive, adapter, cr::IO_WAIT_MS);
    rio::release_producers(passive, adapter, *budget, cr::IO_WAIT_MS);
}

/// The transport is gone (`rm_client::forget`, after the device reset): no copy can write any
/// page any more and nothing on the host names them, so every pin goes; the jobs died with the
/// WindowedBlt FIFO. PASSIVE, no lock held.
pub(crate) fn forget() {
    WANT_UP.store(0, Ordering::Relaxed);
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut n_jobs = 0u32;
    for i in 0..cr::MAX_DSTS {
        let taken = {
            let mut g = STATE.lock();
            if i == 0 {
                g.jobs.drain(|j| {
                    if matches!(j.state, JobState::Submitted { .. }) {
                        n_jobs += 1;
                    }
                });
                g.chan = cr::Chan::new();
            }
            g.dsts[i].take()
        };
        // PASSIVE, outside the spinlock: may release the last owner of a lease.
        drop(taken);
    }
    for _ in 0..n_jobs {
        infl_sub();
    }
    DST_LIVE.store(0, Ordering::Relaxed);
    OFF.store(0, Ordering::Relaxed);
    ACTIVE.store(0, Ordering::Release);
}
