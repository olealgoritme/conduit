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
static REC_NONE: AtomicU32 = AtomicU32::new(0);
static REC_KEY: AtomicU32 = AtomicU32::new(0);
static REC_LAST: AtomicU32 = AtomicU32::new(0);
static REC_WANT: AtomicU32 = AtomicU32::new(0);
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
static CH_FAIL: AtomicU32 = AtomicU32::new(0);
/// A copy hit its own deadline and marked the channel broken: the teardown that follows is that
/// copy's route strike (charged once, there).
static TIMED_OUT: AtomicU32 = AtomicU32::new(0);
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
/// `CeRtDirect` in force (0 or 1), read at StartDevice with the route on.
static DIRECT_KNOB: AtomicU32 = AtomicU32::new(0);
static DIR: AtomicU32 = AtomicU32::new(0);
static DIR_NO: AtomicU32 = AtomicU32::new(0);
static DIR_WHY: AtomicU32 = AtomicU32::new(0);
static DIR_US: AtomicU32 = AtomicU32::new(0);
static LAG: AtomicU32 = AtomicU32::new(0);

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
        &SEEN, &ROUTED, &DONE, &FALL, &WHY, &MASK, &REC_NONE, &REC_KEY, &REC_LAST, &REC_WANT, &DISP_FALL, &CLIENT, &STRIKE, &STRUCK,
        &CH_STRIKE, &OFF, &POISON, &LEAK, &TIMEOUT, &CH_FAIL, &TIMED_OUT, &UP, &INFL, &PEAK, &DEC_US, &DUP_US, &SUB_US,
        &DONE_US, &DONE_MAX, &POLL_US, &DST_NEW, &DST_DROP, &DST_LIVE, &RUNS, &DIR, &DIR_NO,
        &DIR_WHY, &DIR_US, &LAG,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    WANT_UP.store(0, Ordering::Relaxed);
    // `CeRtDirect`: read only with the route on (nothing is read or written with the knob off).
    let direct = on() && crate::diag::read_config_dword(crate::diag::knobs::CE_RT_DIRECT, 0) == 1;
    DIRECT_KNOB.store(u32::from(direct), Ordering::Relaxed);
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
    rec(b"CeRtRecNone", REC_NONE.load(Ordering::Relaxed));
    rec(b"CeRtRecKey", REC_KEY.load(Ordering::Relaxed));
    rec(b"CeRtRecLast", REC_LAST.load(Ordering::Relaxed));
    rec(b"CeRtRecWant", REC_WANT.load(Ordering::Relaxed));
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
    rec(b"CeRtChFail", CH_FAIL.load(Ordering::Relaxed));
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
    rec(b"CeRtDirKnob", DIRECT_KNOB.load(Ordering::Relaxed));
    rec(b"CeRtDir", DIR.load(Ordering::Relaxed));
    rec(b"CeRtDirNo", DIR_NO.load(Ordering::Relaxed));
    rec(b"CeRtDirWhy", DIR_WHY.load(Ordering::Relaxed));
    rec(b"CeRtDirUs", DIR_US.load(Ordering::Relaxed));
    rec(b"CeRtLag", LAG.load(Ordering::Relaxed));
}

/// A Present (or a dispatch) keeps the Venus copy.
/// The code of the route's last refusal (`CeRtWhy`), for a caller that falls back on its own.
pub(crate) fn last_why() -> u32 {
    WHY.load(Ordering::Relaxed)
}

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

/// A queued request fell back at its dispatch (the worker submits its Venus copy).
fn fall_at_dispatch(why: Why) {
    fall(why);
    if !(why.at_dispatch() || why == Why::Stale) {
        DISP_FALL.fetch_add(1, Ordering::Relaxed);
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
    /// The presenting process (`hKmdProcess`): the `h_client` rule is checked again with it
    /// immediately before the worker dups what the record names.
    presenter: usize,
    plan: SourcePlan,
    remap: cp::Remap,
    dst_pitch: u32,
    lines: u32,
    /// Bytes from the destination's byte 0 to where the source's (0, 0) lands (`DstInfo::x/y`).
    dst_offset: u64,
}

/// One destination: the pure record, the pin of its lease set while a descriptor may name it,
/// and what the worker needs to make the descriptor.
struct DstEntry {
    dst: Dst,
    pin: Option<GuestPin>,
    /// Pins of descriptors whose copies were discharged as bystanders of a channel failure: RM
    /// may not have cancelled those copies, so the pages stay locked until the generation ends
    /// (`forget`), while the destination may get a fresh descriptor (`pin`).
    orphan: Option<GuestPin>,
    /// The destination was destroyed while another thread was freeing its descriptor
    /// (`Draining`): `finish_free` removes the record, and only after that free answered.
    gone: bool,
    /// `[0, cover)`: `pitch * height` rounded up to pages (`guest_blob::cover_len`).
    cover: u64,
    /// The destination is a KMD RM video-memory surface (`RedirVram`, `docs/vram-redirection.md`
    /// 5.3): its "descriptor" is its mapping in the channel (`ce_vram`), it has no lease pages, no
    /// pin and no OS descriptor to free.
    vram: bool,
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
    /// Where the source's pixel (0, 0) lands in the destination (`DstRect` minus `SrcRect`): 0 for
    /// a Blt into a standard buffer (which must then be the source's size); a window's client
    /// offset for a Blt into a VRAM redirection surface, which also holds the non-client area.
    pub x: u32,
    pub y: u32,
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
    // SAFETY: the caller's contract.
    unsafe { try_route_as(passive, adapter, args, context, source, destination, dst, boundary, false) }
}

/// [`try_route`] for a `RedirVram` destination (`docs/vram-redirection.md` 5.3): the Blt of an
/// NVK-on-RM source into a KMD RM video-memory surface. Decided as a routed copy is, with the
/// destination rule of [`vram_destination_facts`] in place of the lease rule; queued as a deferred
/// WindowedBlt request whose destination is that image (its Venus copy into the imported image is
/// the fallback the worker submits whenever the copy engine does not); the copy is VRAM to VRAM.
/// `None`: nothing queued (counted), the caller decides what the Present does.
///
/// # Safety
/// As [`try_route`].
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(crate) unsafe fn try_route_vram(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    context: Option<&ContextHandleRef<'_>>,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    dst: DstInfo,
    boundary: Option<u64>,
) -> Option<u64> {
    // SAFETY: the caller's contract.
    unsafe { try_route_as(passive, adapter, args, context, source, destination, dst, boundary, true) }
}

#[allow(clippy::too_many_arguments)]
#[inline(never)]
unsafe fn try_route_as(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    args: &DXGKARG_PRESENT,
    context: Option<&ContextHandleRef<'_>>,
    source: OptimalPresentImageDesc,
    destination: PresentDestinationDesc,
    dst: DstInfo,
    boundary: Option<u64>,
    vram: bool,
) -> Option<u64> {
    if !on() {
        return None;
    }
    let t0 = now_100ns();
    SEEN.fetch_add(1, Ordering::Relaxed);
    let decided = decide_present(adapter, context, destination, dst, boundary, vram);
    add_us(&DEC_US, t0);
    let (payload, boundary) = match decided {
        Ok(v) => v,
        Err(why) => {
            fall(why);
            return None;
        }
    };
    // The latest validated record of this source image: a GDI command that later reads the same
    // NVK image on the copy engine (`ce_vram::foreign_source`, `RedirVram`) acquires its semaphore.
    remember_source_record(source.resource_id(), payload.rec, payload.presenter);
    // Queued exactly as a deferred `BltAsync` copy is (`ddi/blt_async.rs::deferred`): the Venus
    // copy prepared now is the fallback the worker submits whenever the copy engine does not.
    let no_mirror_knob = crate::ddi::blt_async::no_mirror_on();
    let queued = adapter.with_scanout_lifecycle(passive, |lock| {
        let prepared = lock.with_venus_client(|client| {
            client.prepare_present_blt_guest(adapter, source, destination)
        });
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            // A VRAM destination Venus cannot import: the copy engine is its only copy.
            Ok(Err(_)) | Err(_) if vram => {
                match crate::virtio::venus::PreparedPresentBltSubmission::none(destination) {
                    Some(p) => p,
                    None => return Err(VirtioError::DeviceError),
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(VirtioError::DeviceError),
        };
        let no_mirror = no_mirror_knob || prepared.guest_target();
        adapter
            .with_virtio(|v| {
                // `t_present` 0: not stamped (the route does not carry the DDI's entry time).
                v.queue_async_blt(adapter, source, destination, prepared, boundary, no_mirror, 0)
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
        let ok = ensure_dst_as(&mut g, dst, vram) && g.jobs.add(token, boundary, dst.resource_id, payload);
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
        // `CeRtDirect` 1: submit the copy now, the GPU acquire waiting for the producer; when it
        // cannot go without waiting, the request stays the deferred one (counted, `CeRtDirWhy`).
        if DIRECT_KNOB.load(Ordering::Relaxed) != 0 {
            try_direct(adapter, token, boundary, dst.resource_id);
        }
        // The worker prepares the job (dup, descriptor) before the request can be dispatched,
        // or polls the copy just submitted.
        adapter.signal_hpd();
    } else {
        fall(Why::Full);
    }
    Some(token)
}

/// The latest record the route validated per source image (`RedirVram`'s GDI copies out of an NVK
/// image take the producer's semaphore from it). Small, most recent first out.
const SOURCE_RECORDS: usize = 8;
static SOURCE_RECS: SpinLock<[(u32, Option<(StashedCeRecord, usize)>); SOURCE_RECORDS]> =
    SpinLock::new([(0, None); SOURCE_RECORDS]);

fn remember_source_record(resource_id: u32, rec: StashedCeRecord, presenter: usize) {
    if resource_id == 0 {
        return;
    }
    let mut t = SOURCE_RECS.lock();
    let i = t.iter().position(|e| e.0 == resource_id).unwrap_or(SOURCE_RECORDS - 1);
    // Move to the front.
    let mut k = i;
    while k > 0 {
        t[k] = t[k - 1];
        k -= 1;
    }
    t[0] = (resource_id, Some((rec, presenter)));
}

/// The latest record the route validated for the NVK image `resource_id`, if its two clients still
/// belong to the process that presented it. Spinlock only (plus the client table's lock).
pub(crate) fn source_record(adapter: &AdapterContext, resource_id: u32) -> Option<StashedCeRecord> {
    let found = SOURCE_RECS
        .lock()
        .iter()
        .find(|e| e.0 == resource_id)
        .and_then(|e| e.1);
    let (rec, presenter) = found?;
    clients_still_owned(adapter, &rec, presenter).then_some(rec)
}

/// The transport generation ended: no record names a live client any more.
pub(crate) fn forget_source_records() {
    *SOURCE_RECS.lock() = [(0, None); SOURCE_RECORDS];
}

fn dir_no(why: cr::DirNo) {
    DIR_NO.fetch_add(1, Ordering::Relaxed);
    DIR_WHY.store(why.code(), Ordering::Relaxed);
}

/// The push of one job: the producer's acquire and the copy into the destination's descriptor.
fn push_of(p: &Payload, sem_va: u64, src_va: u64, dst_va: u64) -> (cp::Acquire, cp::CopyRect) {
    (
        cp::Acquire {
            va: sem_va,
            value: p.rec.record.semaphore.value,
        },
        cp::CopyRect {
            // `ce_dup`'s source VA already includes the plan's offset (as the shadow mode uses it):
        // adding it again read `offset` bytes too far for a source with a nonzero offset.
        src_va,
            dst_va: dst_va + p.dst_offset,
            src_pitch: p.rec.record.source.pitch,
            dst_pitch: p.dst_pitch,
            line_bytes: p.plan.line_bytes,
            lines: p.lines,
            layout: p.plan.layout,
            dst_layout: SurfaceLayout::Pitch,
            remap: p.remap,
            stamp: None,
        },
    )
}

/// `CeRtDirect` 1, from the Present DDI right after the routed request was queued and its token
/// merged: submit its copy now instead of when the worker sees the producer's boundary ready.
/// The push ACQUIREs the record's own value, so the GPU holds the copy until the producer's work
/// released it. Only when nothing has to wait: the channel up with NO copy in flight (a direct
/// copy never queues behind another destination's, nor lets one queue behind its acquire), the
/// destination's descriptor made, both producer dups cached (no RM call), the channel's I/O free
/// (`try_io`: no dup is remade or given back while the cached VAs are used), the destination
/// taken as `KmdWriter` without waiting and no older request naming it. Everything else: the
/// request stays deferred (`CeRtDirNo`, `CeRtDirWhy`). The submit is spinlocks and plain stores
/// (the push, the GPFIFO entry, `GP_PUT`, the doorbell): bounded, never a wait, legal in the
/// PASSIVE Present DDI, which holds no lock here.
fn try_direct(adapter: &AdapterContext, token: u64, boundary: u64, dst: u32) {
    let t0 = now_100ns();
    let view = rio::chan_view();
    if !view.up || adapter.system_backings.system_copy_invalid(dst) {
        return dir_no(cr::DirNo::NotUp);
    }
    let (payload, dst_va) = {
        let mut g = STATE.lock();
        if g.jobs.in_flight() != 0 {
            drop(g);
            return dir_no(cr::DirNo::ChannelBusy);
        }
        let Some(payload) = g.jobs.find(token, boundary).map(|j| j.payload) else {
            drop(g);
            return dir_no(cr::DirNo::DstBusy);
        };
        match g.dst(dst).map(|e| e.dst.ready_va()) {
            Some(Ok(va)) => (payload, va),
            _ => {
                drop(g);
                return dir_no(cr::DirNo::NotPrepared);
            }
        }
    };
    if !rio::try_io() {
        return dir_no(cr::DirNo::IoBusy);
    }
    let Some(producer) = rio::cached_producer(&payload.rec) else {
        rio::end_io();
        return dir_no(cr::DirNo::NotCached);
    };
    let (acquire, copy) = push_of(&payload, producer.sem_va, producer.src_va, dst_va);
    let completed = rio::poll().map(|(c, _)| c);
    // Lock order: virtio -> the route's STATE -> the channel's STATE (nothing takes them the
    // other way round).
    let dispatched = adapter.with_virtio(|v| {
        v.ce_direct_dispatch(token, boundary, dst, || {
            let Some(completed) = completed else {
                return false;
            };
            let mut g = STATE.lock();
            if g.jobs.in_flight() != 0 || g.jobs.find(token, boundary).is_none() {
                return false;
            }
            let Ok(value) = rio::submit(acquire, &copy) else {
                return false;
            };
            if let Some(j) = g.jobs.find_mut(token, boundary) {
                j.state = JobState::Submitted { value, t_submit: now_100ns() };
                j.direct = true;
                j.seen = 0;
                j.prep = Prep::Ready { sem_va: producer.sem_va, src_va: producer.src_va };
            }
            if let Some(e) = g.dst(dst) {
                // No clock yet: it starts when the CPU sees the producer finish (`settle`).
                e.dst.route.on_submit(value, completed);
            }
            true
        })
    });
    rio::end_io();
    match dispatched {
        Ok(Ok(())) => {
            DIR.fetch_add(1, Ordering::Relaxed);
            infl_add();
            add_us(&DIR_US, t0);
        }
        Ok(Err(true)) => dir_no(cr::DirNo::Submit),
        Ok(Err(false)) | Err(_) => dir_no(cr::DirNo::DstBusy),
    }
}

/// The destination's record, made when there is none (a free slot). `false`: no room.
fn ensure_dst(g: &mut State, dst: DstInfo) -> bool {
    ensure_dst_as(g, dst, false)
}

fn ensure_dst_as(g: &mut State, dst: DstInfo, vram: bool) -> bool {
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
        orphan: None,
        gone: false,
        cover,
        vram,
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
    vram: bool,
) -> Result<(Payload, u64), Why> {
    let Some(boundary) = boundary else {
        return Err(Why::NoBoundary);
    };
    let Some(rec) = context.and_then(|c| c.take_ce_record(boundary)) else {
        // `CeRtRecNone`: no record on the context; `CeRtRecKey`: one of another boundary
        // (`CeRtRecLast`: the stashed boundary's low half, `CeRtRecWant`: the Present's).
        match context.map(|c| c.ce_record_miss(boundary)) {
            Some((2, have)) => {
                REC_KEY.fetch_add(1, Ordering::Relaxed);
                REC_LAST.store(have, Ordering::Relaxed);
                REC_WANT.store(boundary as u32, Ordering::Relaxed);
            }
            _ => {
                REC_NONE.fetch_add(1, Ordering::Relaxed);
            }
        }
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
        Some(gen) if view.up => source_facts(gen, &rec, dst, vram),
        _ => Err(Why::ChannelDown),
    };
    let destination_ok = if vram {
        vram_destination_facts(destination, dst)
    } else {
        destination_facts(adapter, destination, dst, presenter)
    };
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
            presenter,
            plan,
            remap,
            dst_pitch: dst.pitch,
            lines: rec.record.source.height,
            dst_offset: u64::from(dst.y) * u64::from(dst.pitch) + u64::from(dst.x) * 4,
        },
        boundary,
    ))
}

/// The source plan and the remap, or why the source is refused.
fn source_facts(
    gen: cp::Gen,
    rec: &StashedCeRecord,
    dst: DstInfo,
    vram: bool,
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
    // A standard buffer takes exactly the source's size at (0, 0). A VRAM window surface takes the
    // whole source at (x, y) inside it (381.1: Heaven's 1600x900 frames into its window's larger
    // surface were all `Extent`).
    let fits = if vram {
        s.width.checked_add(dst.x).is_some_and(|w| w <= dst.width)
            && s.height.checked_add(dst.y).is_some_and(|h| h <= dst.height)
            && u64::from(plan.line_bytes) + u64::from(dst.x) * 4 <= u64::from(dst.pitch)
    } else {
        dst.x == 0 && dst.y == 0 && s.width == dst.width && s.height == dst.height
            && plan.line_bytes <= dst.pitch
    };
    if !fits {
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

/// A `RedirVram` destination: a pitch-linear KMD RM video-memory surface (an optimal-image
/// destination the VRAM service made) that fits one channel window.
fn vram_destination_facts(destination: PresentDestinationDesc, dst: DstInfo) -> Result<(), Why> {
    if !matches!(destination, PresentDestinationDesc::OptimalImage(_)) {
        return Err(Why::Destination);
    }
    let Some(obj) = crate::virtio::rm_client::vidmem::lookup(dst.resource_id) else {
        return Err(Why::Destination);
    };
    let cover = helios_kmd_logic::guest_blob::cover_len(obj.pitch, obj.height, obj.size)
        .map_err(|_| Why::Destination)?;
    if !cr::fits_window(cover) || obj.pitch != dst.pitch || obj.width != dst.width || obj.height != dst.height {
        return Err(Why::Destination);
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
    // A direct copy of another destination still waits on the GPU for its producer: this copy
    // would queue behind that acquire (head-of-line), so it takes its Venus copy instead.
    let blocked = g.jobs.blocked_by_direct(job.dst);
    let stale = adapter.system_backings.system_copy_invalid(job.dst);
    let va = g.dst(job.dst).map_or(Err(Why::Retiring), |e| e.dst.ready_va());
    let ready = match (job.prep, va, completed) {
        _ if blocked => Err(Why::Blocked),
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
            fall_at_dispatch(why);
            return false;
        }
    };
    let p = job.payload;
    let copy = cp::CopyRect {
        // `ce_dup`'s source VA already includes the plan's offset (as the shadow mode uses it):
        // adding it again read `offset` bytes too far for a source with a nonzero offset.
        src_va,
        dst_va: dst_va + p.dst_offset,
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
            fall_at_dispatch(Why::Submit);
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
        // Nothing of the route's in flight, but another user of the channel (the GDI executor's
        // copies, `RedirVram`'s transfers, `ce_vram::wait` on a timeout) may have broken it: tear it
        // down now (no route job to discharge, so `fail_channel` does nothing else), so the next
        // bring-up makes a fresh one instead of the channel staying broken for the generation.
        if rio::chan_view().broken {
            fail_channel(passive, adapter);
        }
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
        observe_direct_producers(adapter);
        let progress = rio::poll();
        let mut done: [(u64, u64, bool, u64, u64); cr::MAX_JOBS] =
            [(0, 0, false, 0, 0); cr::MAX_JOBS];
        let mut n = 0usize;
        let mut timed_out_any = false;
        let more = {
            let mut g = STATE.lock();
            let now_ms = now_ms();
            let mut timed = [0u32; cr::MAX_DSTS];
            match progress {
                Some((completed, _)) => {
                    let State { jobs, dsts, .. } = &mut *g;
                    for (i, slot) in dsts.iter_mut().enumerate() {
                        let Some(e) = slot.as_mut() else { continue };
                        // A direct copy's clock starts when its producer was seen finished (or
                        // at the sanity cap); a deferred one's started at its dispatch.
                        let direct = jobs.iter().find(|j| {
                            j.dst == e.dst.resource_id
                                && j.direct
                                && matches!(j.state, JobState::Submitted { .. })
                        });
                        if let Some(j) = direct {
                            if let JobState::Submitted { t_submit, .. } = j.state {
                                let seen = (j.seen != 0).then_some(j.seen / 10_000);
                                if let Some(t) =
                                    cr::route_fire_ms(true, t_submit / 10_000, seen, now_ms)
                                {
                                    e.dst.route.on_producer_fired(t, completed);
                                }
                            }
                        }
                        if e.dst.route.poll(completed, now_ms) == cp::Poll::TimedOut {
                            // Its own deadline (`Discharge::Own`): `Route::poll` struck and
                            // poisoned it; never the route again, the pages stay pinned for the
                            // generation. The copies queued behind it are bystanders.
                            timed[i] = e.dst.resource_id;
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
                            let ok = r == Retire::Retire;
                            done[n] = (job.token, job.boundary, ok, t_submit, job.seen);
                            n += 1;
                        }
                    }
                }
                None => {
                    // No channel: nothing will ever complete what was submitted. Bystanders of
                    // the channel's end: their pages stay pinned (orphaned), no strike.
                    while let Some(job) =
                        g.jobs.take_first(|j| matches!(j.state, JobState::Submitted { .. }))
                    {
                        if let JobState::Submitted { t_submit, .. } = job.state {
                            done[n] = (job.token, job.boundary, false, t_submit, 0);
                            n += 1;
                        }
                        bystander(&mut g, job.dst);
                        // No channel: its client, and the descriptor in it, are gone with it.
                        if let Some(e) = g.dst(job.dst) {
                            if matches!(e.dst.desc, Desc::Ready { .. }) {
                                e.dst.desc = Desc::Absent;
                                let _ = DST_LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                                    v.checked_sub(1)
                                });
                            }
                        }
                    }
                }
            }
            g.jobs.in_flight() != 0
        };
        for &(token, boundary, ok, t_submit, seen) in done.iter().take(n) {
            let _ = adapter.with_virtio(|v| v.complete_ce_blt(adapter, token, boundary, ok));
            infl_sub();
            if ok {
                DONE.fetch_add(1, Ordering::Relaxed);
                let us = add_us(&DONE_US, t_submit);
                DONE_MAX.fetch_max(us, Ordering::Relaxed);
                if seen != 0 {
                    // A direct copy: the CPU's lag from seeing its producer finish to seeing it
                    // done (0 when the completion was seen first: the copy beat the event).
                    add_us(&LAG, seen);
                }
            } else if progress.is_some() {
                TIMEOUT.fetch_add(1, Ordering::Relaxed);
            } else {
                CH_FAIL.fetch_add(1, Ordering::Relaxed);
            }
        }
        if timed_out_any {
            // The channel may be stuck on that copy: torn down by the next pass, whose teardown
            // charges the one route strike of this stall.
            TIMED_OUT.store(1, Ordering::Relaxed);
            rio::mark_broken();
        }
        if !spin || !more || progress.is_none() || now_100ns() >= spin_until {
            break;
        }
        core::hint::spin_loop();
    }
    add_us(&POLL_US, t0);
}

/// The direct copies whose producer the CPU has not seen finish yet: their boundary is checked
/// (`VirtioGpu::ce_boundary_seen`, a dead stream counts as finished) and the time it was first
/// seen recorded, which starts the copy's deadline. One relaxed load with no direct copy.
fn observe_direct_producers(adapter: &AdapterContext) {
    if DIRECT_KNOB.load(Ordering::Relaxed) == 0 {
        return;
    }
    let unseen: [(u64, u64); cr::MAX_JOBS] = {
        let g = STATE.lock();
        let mut out = [(0u64, 0u64); cr::MAX_JOBS];
        for (k, j) in g
            .jobs
            .iter()
            .filter(|j| j.direct && j.seen == 0 && matches!(j.state, JobState::Submitted { .. }))
            .enumerate()
        {
            out[k] = (j.token, j.boundary);
        }
        out
    };
    for &(token, boundary) in unseen.iter().filter(|(t, _)| *t != 0) {
        let seen = adapter
            .with_virtio(|v| v.ce_boundary_seen(boundary))
            .unwrap_or(true);
        if seen {
            let now = now_100ns().max(1);
            if let Some(j) = STATE.lock().jobs.find_mut(token, boundary) {
                if j.seen == 0 {
                    j.seen = now;
                }
            }
        }
    }
}

/// The channel broke (its error notifier, a copy that never completed) or its service struck
/// out: every submitted copy is discharged and its destination poisoned, the queued jobs fall
/// back at their dispatch, every descriptor is freed (or leaked), and the channel is torn down
/// (the next Present that finds it down asks for a bring-up again, until three route strikes).
#[inline(never)]
fn fail_channel(passive: PassiveLevel, adapter: &AdapterContext) {
    // A last observation of the ring (a set notifier is counted there).
    let _ = rio::poll();
    let mut discharged: [(u64, u64); cr::MAX_JOBS] = [(0, 0); cr::MAX_JOBS];
    let mut n = 0usize;
    {
        let mut g = STATE.lock();
        let mut any = TIMED_OUT.swap(0, Ordering::Relaxed) != 0;
        while let Some(job) = g.jobs.take_first(|j| matches!(j.state, JobState::Submitted { .. })) {
            discharged[n] = (job.token, job.boundary);
            n += 1;
            any = true;
            // Queued behind a stalled copy, or in flight when the channel failed: a bystander
            // (`Discharge::Bystander`), never charged for another destination's stall.
            bystander(&mut g, job.dst);
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
        CH_FAIL.fetch_add(1, Ordering::Relaxed);
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

/// A copy into `dst` was discharged as a BYSTANDER of a channel failure (`Discharge::Bystander`):
/// no strike, no poison. RM may not have cancelled the copy, so the pin of the descriptor it
/// targets is orphaned (kept until the generation ends, `forget`), and the destination's ring
/// values are forgotten so its descriptor is freed now and a fresh one may be made over the same
/// (still locked) pages once a channel is up again.
fn bystander(g: &mut State, dst: u32) {
    debug_assert_eq!(cr::Discharge::Bystander.charges(), (false, false));
    let Some(e) = g.dst(dst) else {
        return;
    };
    if e.dst.desc == Desc::Leaked {
        return;
    }
    if let Some(pin) = e.pin.take() {
        if e.orphan.is_none() {
            e.orphan = Some(pin);
        } else {
            // Two failures under one destination in a generation: never unlock either.
            core::mem::forget(pin);
        }
    }
    e.dst.route.on_channel_gone();
    LEAK.fetch_add(1, Ordering::Relaxed);
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
                    Some((e.dst.slot, va, len, e.dst.resource_id, e.vram))
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
        if let Some((slot, va, len, resource_id, vram)) = plan {
            // A VRAM destination's mapping is `ce_vram`'s (given back with the channel): nothing
            // to free here.
            let freed = vram || rio::free_dst(passive, adapter, slot, va, len, wait_io_ms, cr::FREE_MS);
            finish_free(i, resource_id, freed);
        }
    }
}

/// The free of `resource_id`'s descriptor (slot index `i`, which a `Draining` record keeps:
/// nothing else removes or reuses it) answered `freed`: `Absent` and unpinned, or leaked. A record
/// whose destination was destroyed meanwhile (`gone`) is removed here, after the free answered;
/// a leaked one stays, pin and all, until the generation ends.
fn finish_free(i: usize, resource_id: u32, freed: bool) {
    let released = {
        let mut g = STATE.lock();
        let Some(e) = g.dsts[i].as_mut() else { return };
        if e.dst.resource_id != resource_id {
            return;
        }
        let pin = if freed {
            e.dst.desc = Desc::Absent;
            DST_DROP.fetch_add(1, Ordering::Relaxed);
            let _ = DST_LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
            e.pin.take()
        } else {
            e.dst.desc = Desc::Leaked;
            LEAK.fetch_add(1, Ordering::Relaxed);
            None
        };
        let remove = e.gone && e.dst.desc == Desc::Absent && e.orphan.is_none();
        let record = if remove { g.dsts[i].take() } else { None };
        refresh_active(&g);
        (pin, record)
    };
    // PASSIVE, outside the spinlock: may release the last owner of a lease.
    drop(released);
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
            g.jobs
                .first_pending()
                .map(|j| (j.token, j.boundary, j.dst, j.payload.rec, j.payload.presenter))
        };
        let Some((token, boundary, dst, rec, presenter)) = next else {
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
            // The `h_client` rule once more, immediately before the dup: a client freed (and its
            // number re-minted for another process) since the Present is refused here.
            Ok(()) if !clients_still_owned(adapter, &rec, presenter) => Prep::Failed(Why::Client),
            // A copy in flight may read a cached dup: only a cache hit now (no dup is remade or
            // evicted, which would unmap a slot under that copy); a miss waits for the next pass.
            Ok(()) if STATE.lock().jobs.in_flight() != 0 => {
                let hit = if rio::try_io() {
                    let p = rio::cached_producer(&rec);
                    rio::end_io();
                    p
                } else {
                    None
                };
                match hit {
                    Some(p) => Prep::Ready {
                        sem_va: p.sem_va,
                        src_va: p.src_va,
                    },
                    None => {
                        add_us(&DUP_US, t0);
                        return;
                    }
                }
            }
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

/// Whether both record clients still belong to `presenter` (`ce_record::client_check` against
/// the client table now).
fn clients_still_owned(adapter: &AdapterContext, rec: &StashedCeRecord, presenter: usize) -> bool {
    helios_kmd_logic::ce_record::both_owned(
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
    ) == ClientCheck::Owned
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
        if e.vram {
            let cover = e.cover;
            drop(g);
            return vram_descriptor(passive, adapter, resource_id, cover);
        }
    }
    // Never wait on the worker: a paging operation may hold the transaction for its whole bound.
    let Some(guard) = adapter.system_backings.try_serialize(passive) else {
        return Err(Why::NotReady);
    };
    let r = create_descriptor(passive, adapter, &guard, resource_id);
    drop(guard);
    r
}

/// A `RedirVram` destination's "descriptor": its mapping in the channel (`ce_vram::ce_surface`,
/// made on demand; no content transaction: it has no leases).
fn vram_descriptor(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    cover: u64,
) -> Result<(), Why> {
    match crate::virtio::rm_client::ce_vram::ce_surface(passive, adapter, resource_id) {
        Ok(s) => {
            let mut g = STATE.lock();
            if let Some(e) = g.dst(resource_id) {
                if e.dst.desc == Desc::Absent {
                    e.dst.desc = Desc::Ready { va: s.va, len: cover };
                    DST_NEW.fetch_add(1, Ordering::Relaxed);
                    DST_LIVE.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            } else {
                Err(Why::Full)
            }
        }
        Err(f) if rio::is_busy(&f) => Err(Why::NotReady),
        Err(_) => {
            if let Some(e) = STATE.lock().dst(resource_id) {
                strike_dst(&mut e.dst, cp::Why::RmError);
            }
            Err(Why::Desc)
        }
    }
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
    // Interrupt-time deadlines: the whole hook, and the drain inside it. Each sleep rounds up to
    // the timer quantum, so the clock (not a count of sleeps) ends every wait.
    let t0 = now_100ns();
    let hook_end = cr::deadline(t0, cr::LEASE_HOOK_MS);
    let drain_end = cr::earlier(cr::deadline(t0, cr::DRAIN_MS), hook_end);
    let mut drained = false;
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
        if cr::expired(now_100ns(), drain_end) {
            break;
        }
        crate::virtio::ctrl::sleep_ms(passive, 1);
    }
    if drained {
        // Terminalize what completed now (the worker would on its next pass): a destroy that
        // follows finds the request's copy ring-complete instead of retaining the allocation.
        settle(passive, adapter, false);
    }
    // What is left of the hook's bound for the wait for the channel's I/O and the free; none
    // left: leaked (pinned until the generation ends), never a longer stall of the paging thread.
    let left = cr::left_ms(now_100ns(), hook_end);
    let freed = drained
        && left != 0
        && rio::free_dst(
            passive,
            adapter,
            slot,
            va,
            len,
            left.min(cr::IO_WAIT_MS),
            left.min(u64::from(cr::FREE_MS)) as u32,
        );
    finish_free(index, resource_id, freed);
    if !freed {
        // Drained too late or not freed: leaked (`finish_free`), and a strike.
        if let Some(e) = STATE.lock().dsts[index].as_mut() {
            if e.dst.resource_id == resource_id {
                e.dst.route.on_failure(cp::Why::Timeout);
            }
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
        let submitted = g.jobs.submitted_for(resource_id);
        let taken = match g.dst_index(resource_id) {
            Some(i) => {
                let e = g.dsts[i].as_mut();
                match e {
                    // Another thread is freeing its descriptor: `finish_free` removes it once
                    // that free answered (the pin drops only there).
                    Some(e) if e.dst.desc == Desc::Draining => {
                        e.gone = true;
                        None
                    }
                    // A leaked record, an orphaned pin, a copy still submitted: stays until
                    // the generation ends.
                    Some(e)
                        if e.dst.desc == Desc::Leaked || e.orphan.is_some() || submitted =>
                    {
                        e.gone = true;
                        None
                    }
                    Some(_) => g.dsts[i].take(),
                    None => None,
                }
            }
            None => None,
        };
        refresh_active(&g);
        taken
    };
    // PASSIVE, outside the spinlock: may release the last owner of a lease.
    drop(removed);
}

/// A `RedirVram` destination's allocation is being destroyed (`vidmem::released`, before its memory
/// is freed): its record goes unless a copy into it is still submitted (then it stays, `gone`,
/// until the generation ends: the dup in the channel keeps the memory alive meanwhile). PASSIVE.
pub(crate) fn vram_destination_gone(resource_id: u32) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut g = STATE.lock();
    let submitted = g.jobs.submitted_for(resource_id);
    if let Some(i) = g.dst_index(resource_id) {
        let vram = g.dsts[i].as_ref().is_some_and(|e| e.vram);
        if vram {
            if submitted {
                if let Some(e) = g.dsts[i].as_mut() {
                    e.gone = true;
                    e.dst.desc = Desc::Leaked;
                }
            } else if g.dsts[i].take().is_some() {
                let _ = DST_LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
            }
        }
    }
    refresh_active(&g);
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

/// The transport is gone (`rm_client::forget`, after the device reset); the jobs died with the
/// WindowedBlt FIFO. `fate` is the transport sweep's (`nvrm::close_all_on_host`, as for user
/// pins): with every close confirmed the host let go of the channel's client and every
/// descriptor in it, so every pin goes. With any close unconfirmed the host may keep the client's
/// file across the reset and the GPU may still write pages it registered: the pin of every
/// destination a descriptor may name (ready, being made, draining, leaked) and every orphaned pin
/// is leaked on purpose (never unlock pages a GPU may still write). PASSIVE, no lock held.
pub(crate) fn forget(fate: helios_kmd_logic::sweep_budget::PinFate) {
    WANT_UP.store(0, Ordering::Relaxed);
    forget_source_records();
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
        let Some(mut e) = taken else { continue };
        // The route's pins are always described to the host (a registration carried them).
        if fate.action(true) == helios_kmd_logic::sweep_budget::PinAction::Leak {
            let named = !matches!(e.dst.desc, Desc::Absent | Desc::Uncovered);
            if named {
                if let Some(pin) = e.pin.take() {
                    core::mem::forget(pin);
                    LEAK.fetch_add(1, Ordering::Relaxed);
                }
            }
            if let Some(pin) = e.orphan.take() {
                core::mem::forget(pin);
            }
        }
        // PASSIVE, outside the spinlock: may release the last owner of a lease.
        drop(e);
    }
    for _ in 0..n_jobs {
        infl_sub();
    }
    DST_LIVE.store(0, Ordering::Relaxed);
    OFF.store(0, Ordering::Relaxed);
    ACTIVE.store(0, Ordering::Release);
}
