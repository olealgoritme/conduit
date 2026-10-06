//! Option B: the KMD's own `ScanoutFlip` of an RM system-memory primary
//! (`KmdRmClient` = 5). Design: `docs/kmd-rm-client.md` sections 14.3 and 15. The decisions
//! are `helios_kmd_logic::rm_sysmem` (which allocation is shown) and the level 3 presenter's
//! state machine (`helios_kmd_logic::rm_present::Presenter`, ring of one, whose "copy" is
//! nothing: the memory IS the primary and the CPU keeps writing it); this file performs
//! them.
//!
//! THE FLOW. `SetVidPnSourceAddress` of an allocation that adopted RM system memory reaches
//! [`program`] from `program_vidpn_source_inner`, which makes it the shown target (no
//! `SET_SCANOUT_BLOB`, no GPU copy, no Venus image: there is none) and wakes the HPD
//! worker. The worker's pass ([`service`], from `rm_client::service`) registers the KMD's
//! RESIDENT source with the arbiter and flips the target's GEM; every desktop refresh the
//! arbiter's suppression gate withholds afterwards is a frame edge and flips it again
//! (paced to 60 Hz: a flip is a header-only message that tells the viewer the memory
//! changed). A user-mode source preempts the resident one and, when it ends, the resume
//! edge flips the primary again, exactly as at level 3. The hook leaves the Venus path
//! alone for every other allocation, and withdraws the resident source when the screen's
//! source stops being an RM primary ([`other_source`]).
//!
//! WHEN IT FLIPS AGAIN (`docs/kmd-rm-client.md` 15.16, decided). The viewer commits only when it
//! is sent a `ScanoutFlip`, so every change of the primary (GDI and DWM write it through the CPU
//! aperture mapping) has to end in a flip of the same GEM. [`primary_changed`] is the one door
//! for the events that report a change (`rm_refresh::Edge`: the present blit, the windowed blit's
//! completion, a paging write; the programming and the refresh gate raise the same frame edge);
//! `rm_refresh::Refresher` covers the change nobody reports (a short decaying tail after the last
//! reported one, and the opt-in `KmdRmSysPollMs` heartbeat); the presenter's minimum interval is
//! the mode's refresh period (`rm_refresh::flip_interval_100ns`), so a flood of edges costs at
//! most one flip per refresh and the last of them is always shown (a trailing flip).
//!
//! THE RELEASE SEAM (`docs/kmd-rm-client.md` 15.7, decided). The KMD's flips are entered in the
//! host-release book by `present_within` like every other flip (`virtio/scanout_release.rs`).
//! A flip here NEVER waits for a release: the buffer it shows is the one already on the
//! scanout (a re-flip, the whole life of a ring of one: the host never releases the buffer on
//! the scanout, so a wait could only last its limit, every frame) or another primary (a mode
//! change), whose flip is what makes the host release the previous one. The presenter is
//! given `release_tracked = false` for that reason (`rm_sysmem::flip_inputs`). What CAN wait
//! is giving the memory back: [`log_flip`] keeps which buffer the newest flip replaced, and
//! `sysmem::released` holds the GEM close of a REPLACED primary until the host released it,
//! for at most 500 ms from the replacing flip (`rm_sysmem::close_gate`).
//!
//! GIVING UP. Three failures in a row (a refused registration or flip, eight yielded flips)
//! withdraw the source and reset the presenter, and `RESTART_AT` is set five seconds ahead:
//! until then [`service`] returns at its first line (and asks for a wake at that time), whatever
//! wakes the worker. The gate is needed because a reset presenter registers at its very next
//! look, and every desktop frame edge wakes the worker; the decision is
//! `rm_sysmem::restart_pause`. So a host that hangs or refuses costs the worker three flips
//! (1 s each at most, 100 ms apart) per five seconds, not a loop.
//!
//! LOCKING. `TARGET`, `PRES` and `FLIPS` are leaf spinlocks over plain data, never held across
//! I/O or another lock and never held together. Everything that sends runs with no lock.

use super::sysmem::live_gem;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::foreign_scanout::{present_within, PresentRefusal};
use crate::virtio::gpu::DeviceOwner;
use crate::virtio::rm_present;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::rm_present::{Act, FlipResult, Presenter};
use helios_kmd_logic::rm_refresh::{self as rr, Edge, Refresher, Synthetic, Verdict};
use helios_kmd_logic::rm_sysmem::{self as rs, Change, CloseWait, FlipLog, Target, TargetBook};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Acts performed per worker pass (a registration is followed by the first flip).
const ACTS_PER_PASS: usize = 3;
/// How long the host gets to take one flip (the worker flips every frame and StopDevice
/// joins it for a bounded time, as `rm_present`).
const FLIP_TIMEOUT_MS: u64 = 1_000;
/// Consecutive flips that found the source yielded before it counts as a failure.
const MAX_YIELDS: u32 = 8;

struct PState {
    epoch: u64,
    p: Presenter,
    /// The dirty-unknown window (tail and heartbeat) and the edge census.
    r: Refresher,
}

static PRES: SpinLock<PState> = SpinLock::new(PState {
    epoch: 0,
    p: Presenter::new(1),
    r: Refresher::new(),
});
/// The resource the screen shows when it is an RM primary, 0 when not: what the edge sources
/// that name an allocation (the present blit, a paging write) compare against with one atomic
/// load, at any IRQL. Written by [`program`], [`other_source`], [`target_gone`] and [`reset`].
static SHOWN_RESID: AtomicU32 = AtomicU32::new(0);
/// Reported edges that were a change of the shown primary, by `rm_refresh::Edge::index`
/// (the refresh gate's are the rest of the total: `RmSysEdRef`).
static EDGE_KIND: [AtomicU32; rr::EDGE_KINDS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
/// Events that named another allocation than the shown primary (not a change of the screen).
static EDGE_OTHER: AtomicU32 = AtomicU32::new(0);
/// The flip interval the last pass used (100 ns): the mode's refresh period.
static INTERVAL: AtomicU64 = AtomicU64::new(0);
/// `KmdRmSysPollMs` of this transport generation, or [`POLL_UNREAD`].
static POLL_MS: AtomicU32 = AtomicU32::new(POLL_UNREAD);
const POLL_UNREAD: u32 = u32::MAX;
static TARGET: SpinLock<TargetBook> = SpinLock::new(TargetBook::new());
static YIELDS: AtomicU32 = AtomicU32::new(0);
/// When the presenter that gave up starts over (interrupt time, 100 ns; 0 = it is not
/// waiting). A freshly reset presenter registers at its very next look, and every desktop
/// frame edge wakes the worker, so the pause is this gate at the top of [`service`]
/// (`rs::restart_pause`), not the presenter's.
static RESTART_AT: AtomicU64 = AtomicU64::new(0);
/// What the host was last told to show and the buffer that replaced (the release seam: a
/// leaf spinlock over plain data, held for a few stores, never across I/O).
static FLIPS: SpinLock<FlipLog> = SpinLock::new(FlipLog::new());
/// The `seq` of the last flip the host took.
pub static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

// Counters (names at most 14 characters): `RmSysProg` primaries programmed (each is a
// `SetVidPnSourceAddress` of an RM primary), `RmSysProgBad` refused (a layout that is not
// the mode's), `RmSysRegs` registrations the arbiter took, `RmSysWithdrawn` withdrawals,
// `RmSysGaveUp` times the presenter gave up and started over five seconds later, `RmSysFrames` flips
// shown for an edge, `RmSysReflips` flips shown for a resume, `RmSysYielded` flips that
// found the source yielded, `RmSysFlipFail` flips the host or transport refused,
// `RmSysPres` the presenter's word (bit 0 registered, bit 1 gave up, bits 8.. failures),
// `RmSysSeq` the last flip's `seq`. The refresh census (15.16): `RmSysEdges` frame edges raised
// (all kinds), then by kind `RmSysEdProg` (programmed), `RmSysEdBlt` (present blit),
// `RmSysEdWBlt` (windowed blit done), `RmSysEdPag` (paging write), `RmSysEdRef` (the refresh
// gate: markers, restore), `RmSysEdOther` events that named another allocation;
// `RmSysCoal` edges that got no flip of their own (folded into one flip per refresh),
// `RmSysTail` / `RmSysPoll` flips of the tail and of the heartbeat, `RmSysIvl` the flip
// interval in 100 ns (the mode's refresh period), `RmSysPollMs` the heartbeat knob.
pub static SYS_PROG: AtomicU32 = AtomicU32::new(0);
pub static SYS_PROG_BAD: AtomicU32 = AtomicU32::new(0);
pub static SYS_REGS: AtomicU32 = AtomicU32::new(0);
pub static SYS_WITHDRAWN: AtomicU32 = AtomicU32::new(0);
pub static SYS_GAVE_UP: AtomicU32 = AtomicU32::new(0);
pub static SYS_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static SYS_REFLIPS: AtomicU32 = AtomicU32::new(0);
pub static SYS_YIELDED: AtomicU32 = AtomicU32::new(0);
pub static SYS_FLIP_FAIL: AtomicU32 = AtomicU32::new(0);
pub static SYS_PRES: AtomicU32 = AtomicU32::new(0);

pub(super) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if SYS_PROG.load(Ordering::Relaxed) == 0 && SYS_PROG_BAD.load(Ordering::Relaxed) == 0 {
        return;
    }
    rec(b"RmSysProg", SYS_PROG.load(Ordering::Relaxed));
    rec(b"RmSysProgBad", SYS_PROG_BAD.load(Ordering::Relaxed));
    rec(b"RmSysRegs", SYS_REGS.load(Ordering::Relaxed));
    rec(b"RmSysWithdrawn", SYS_WITHDRAWN.load(Ordering::Relaxed));
    rec(b"RmSysGaveUp", SYS_GAVE_UP.load(Ordering::Relaxed));
    rec(b"RmSysFrames", SYS_FRAMES.load(Ordering::Relaxed));
    rec(b"RmSysReflips", SYS_REFLIPS.load(Ordering::Relaxed));
    rec(b"RmSysYielded", SYS_YIELDED.load(Ordering::Relaxed));
    rec(b"RmSysFlipFail", SYS_FLIP_FAIL.load(Ordering::Relaxed));
    rec(b"RmSysPres", SYS_PRES.load(Ordering::Relaxed));
    rec(b"RmSysSeq", LAST_SEQ.load(Ordering::Relaxed) as u32);
    let st = PRES.lock().r.stats();
    let kind = |e: Edge| EDGE_KIND[e.index()].load(Ordering::Relaxed);
    let reported = kind(Edge::Programmed)
        .saturating_add(kind(Edge::PresentBlt))
        .saturating_add(kind(Edge::WindowedBlt))
        .saturating_add(kind(Edge::Paging));
    rec(b"RmSysEdges", st.edges);
    rec(b"RmSysEdProg", kind(Edge::Programmed));
    rec(b"RmSysEdBlt", kind(Edge::PresentBlt));
    rec(b"RmSysEdWBlt", kind(Edge::WindowedBlt));
    rec(b"RmSysEdPag", kind(Edge::Paging));
    rec(b"RmSysEdRef", st.edges.saturating_sub(reported));
    rec(b"RmSysEdOther", EDGE_OTHER.load(Ordering::Relaxed));
    rec(b"RmSysCoal", st.coalesced());
    rec(b"RmSysTail", st.tail_flips);
    rec(b"RmSysPoll", st.poll_flips);
    rec(b"RmSysIvl", INTERVAL.load(Ordering::Relaxed) as u32);
    let poll = POLL_MS.load(Ordering::Relaxed);
    rec(b"RmSysPollMs", if poll == POLL_UNREAD { 0 } else { poll });
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// Forget everything (the transport generation ended).
pub(super) fn reset() {
    TARGET.lock().clear();
    {
        let mut g = PRES.lock();
        g.epoch = 0;
        g.p.reset();
        g.r.reset();
    }
    SHOWN_RESID.store(0, Ordering::Release);
    POLL_MS.store(POLL_UNREAD, Ordering::Relaxed);
    YIELDS.store(0, Ordering::Relaxed);
    RESTART_AT.store(0, Ordering::Release);
    LAST_SEQ.store(0, Ordering::Relaxed);
    FLIPS.lock().clear();
    rm_present::clear_wake_at();
}

// ---- the programming hook ------------------------------------------------------------

/// What [`program`] decided.
pub(crate) enum Programmed {
    /// Not an RM primary (or the level is below 5): the Venus path goes on, untouched.
    NotOurs,
    /// Made the shown source; the worker flips it.
    Ok,
    /// The allocation's layout is not the mode's: refused (`ScanoutReject::Layout`).
    BadLayout,
    /// Not now (no transport, the service forgot it): the caller retries.
    Retry,
}

/// `SetVidPnSourceAddress` of `resource_id` at the mode's extent `width` x `height`, from
/// `program_vidpn_source_inner` (PASSIVE, under the scanout lifecycle lock). Takes no lock
/// across I/O and sends nothing: it records which allocation the screen shows, publishes
/// what a bind publishes (the displayed primary, the active resource, the end of the
/// leases: nothing reads this memory through a lease, the flip is not one) and wakes the
/// worker.
#[inline(never)]
pub(crate) fn program(
    adapter: &AdapterContext,
    resource_id: u32,
    primary_address: u64,
    width: u32,
    height: u32,
) -> Programmed {
    if !super::sysmem_level_on() {
        return Programmed::NotOurs;
    }
    let Ok(Some((drm, gem, fl, _size))) =
        adapter.with_virtio(|v| v.foreign_sysmem_source(resource_id))
    else {
        return Programmed::NotOurs;
    };
    let flip = rs::flip_layout(&fl);
    if fl.width != width || fl.height != height || flip.validate().is_err() {
        SYS_PROG_BAD.fetch_add(1, Ordering::Relaxed);
        return Programmed::BadLayout;
    }
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return Programmed::Retry;
    };
    // The service must still know it (a new transport generation forgot it).
    match live_gem(resource_id) {
        Some((g, e)) if g == gem && e == epoch && epoch != 0 => {}
        _ => return Programmed::Retry,
    }
    let target = Target {
        resid: resource_id,
        drm,
        gem,
        epoch,
        layout: flip,
    };
    let change = TARGET.lock().set(target);
    // The edge sources that name an allocation compare against this one word.
    SHOWN_RESID.store(resource_id, Ordering::Release);
    // Which allocation is the active scanout, and the address the vsync reports: what a bind
    // publishes. The host is NOT bound to it (`host_bound_scanout_resource` stays), so a
    // Venus flush of it, if the resident source were ever withdrawn, is refused loudly
    // (`RfUnb`) instead of being sent for a resource with no scanout.
    adapter.with_wddm_notify_lock(|_| {
        adapter
            .active_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Release);
        adapter
            .active_scanout_resource
            .store(resource_id, Ordering::Release);
        adapter.publish_bound_primary(primary_address);
    });
    adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Cancelled);
    // A registered resident source learns the new picture in place (the arbiter keeps its
    // generation); an unregistered one is registered by the worker.
    if PRES.lock().p.registered() && change != Change::Same {
        let _ = adapter.foreign_scanout_resident_set(KMD, drm, epoch, flip);
    }
    SYS_PROG.fetch_add(1, Ordering::Relaxed);
    // The first flip of a (new) primary is owed; so is one for every programming of it, also
    // of the same allocation again (every flip of the DMA / MMIO present contract ends here).
    primary_changed(adapter, Edge::Programmed, resource_id);
    Programmed::Ok
}

/// The screen's source is not an RM primary any more (a Venus allocation was programmed):
/// forget the target, and the worker withdraws the resident source so the Venus desktop
/// flush comes back. A relaxed read when nothing is shown.
pub(crate) fn other_source(adapter: &AdapterContext) {
    if !super::sysmem_level_on() {
        return;
    }
    let had = {
        let mut g = TARGET.lock();
        let had = g.current().is_some();
        g.clear();
        had
    };
    if had {
        SHOWN_RESID.store(0, Ordering::Release);
        adapter.signal_hpd();
    }
}

/// The allocation `resource_id` is being destroyed (`sysmem::released`): if it is the shown
/// one, forget it; the worker withdraws the resident source in its next pass, and no flip
/// names the GEM that is about to be closed (the target is gone first).
pub(super) fn target_gone(adapter: &AdapterContext, resource_id: u32) {
    if TARGET.lock().gone(resource_id) {
        SHOWN_RESID.store(0, Ordering::Release);
        adapter.signal_hpd();
    }
}

// ---- the edges ------------------------------------------------------------------------------

/// A change of the primary was reported: `edge` named allocation `resid` (0 when the edge names
/// none). If it is the shown RM primary a frame is owed and the worker is woken; the flip comes
/// at the mode's rate (`rm_refresh::flip_interval_100ns`), after the presenter's pacing.
///
/// Atomics and `KeSetEvent(Wait = FALSE)` only, so it is legal wherever an edge can come from:
/// PASSIVE (the present DDI, `BuildPagingBuffer`, the HPD worker's own windowed-blit service)
/// and up to DISPATCH (a completion DPC). It takes no lock and does no I/O. With the knob below
/// 5 it is one relaxed load.
pub(crate) fn primary_changed(adapter: &AdapterContext, edge: Edge, resid: u32) {
    let shown = SHOWN_RESID.load(Ordering::Acquire);
    match rr::judge(super::sysmem_level_on(), shown, edge, resid) {
        Verdict::Flip => {
            EDGE_KIND[edge.index()].fetch_add(1, Ordering::Relaxed);
            rm_present::note_frame_edge(adapter);
        }
        Verdict::NotShown => {
            EDGE_OTHER.fetch_add(1, Ordering::Relaxed);
        }
        Verdict::Off => {}
    }
}

/// `KmdRmSysPollMs` (the opt-in heartbeat, 0 = off), read once per transport generation.
fn poll_ms() -> u32 {
    let v = POLL_MS.load(Ordering::Relaxed);
    if v != POLL_UNREAD {
        return v;
    }
    read_poll_ms()
}

#[inline(never)]
fn read_poll_ms() -> u32 {
    let raw = crate::diag::read_config_dword(crate::diag::knobs::KMD_RM_SYS_POLL_MS, 0);
    let v = if raw == 0 {
        0
    } else {
        raw.clamp(rr::MIN_POLL_MS, rr::MAX_POLL_MS)
    };
    POLL_MS.store(v, Ordering::Relaxed);
    if v != 0 {
        crate::diag::record_named_bytes(b"RmSysPollMs", v);
    }
    v
}

// ---- the worker -----------------------------------------------------------------------

/// One pass of the flip service, from `rm_client::service` at level 5 (PASSIVE, the HPD
/// worker). With nothing shown and nothing registered it is two lock holds. After the pass the
/// refresher's next moment (the tail, the heartbeat) is added to the worker's timed wake: the
/// earliest deadline wins, and an idle desktop has none.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    service_pass(passive, adapter);
    let due = PRES.lock().r.next_due(now());
    if let Some(at) = due {
        rm_present::set_wake_at_min(at);
    }
}

#[inline(never)]
fn service_pass(passive: PassiveLevel, adapter: &AdapterContext) {
    // A presenter that gave up waits out its pause whatever wakes the worker (frame edges
    // come at the display's rate): nothing is registered or flipped, the edges stay
    // owed, and the next wake is the end of the pause.
    let restart_at = RESTART_AT.load(Ordering::Acquire);
    if let Some(wake) = rs::restart_pause(restart_at, now()) {
        rm_present::set_wake_at(wake);
        return;
    }
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return;
    };
    if epoch == 0 {
        return;
    }
    rm_present::clear_wake_at();
    let (mut frame_edge, mut resume_edge) = rm_present::take_edges();
    // The cap on flips is the mode's: one per refresh period, whatever rate the edges come at.
    let interval = rr::flip_interval_100ns(adapter.effective_refresh_mhz());
    INTERVAL.store(interval, Ordering::Relaxed);
    let poll = poll_ms();
    {
        let mut g = PRES.lock();
        if g.epoch != epoch {
            g.p.reset();
            g.r.reset();
            g.epoch = epoch;
        }
        g.p.set_min_interval(interval);
        g.r.set_poll_ms(poll);
        g.r.edges(rm_present::take_edge_count(), frame_edge);
    }
    for _ in 0..ACTS_PER_PASS {
        // StopDevice is joining the worker: start nothing. The transport reset that
        // follows ends the resident source.
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            return;
        }
        let t = now();
        let target = TARGET.lock().current();
        let ready = rs::target_ready(target.as_ref(), epoch);
        let (has_resident, foreground) = adapter.foreign_scanout_resident_state();
        // A ring of one: no release is ever waited for (`rm_sysmem::flip_inputs`).
        let mut inputs =
            rs::flip_inputs(t, ready, has_resident, foreground, frame_edge, resume_edge);
        frame_edge = false;
        resume_edge = false;
        let (act, word, gave_up) = {
            let mut g = PRES.lock();
            if g.epoch != epoch {
                g.p.reset();
                g.r.reset();
                g.epoch = epoch;
                g.p.set_min_interval(interval);
            }
            // A frame nobody reported: the next step of the tail after the last reported
            // change, or the opt-in heartbeat. Never while a reported change is owed a flip.
            if g.r.synthetic(t, ready) != Synthetic::None {
                inputs.frame_edge = true;
            }
            let act = g.p.decide(inputs);
            (act, word(&g.p), g.p.gave_up())
        };
        SYS_PRES.store(word, Ordering::Relaxed);
        if gave_up {
            // Three failures in a row. At level 3 this hands the screen back to Venus; here
            // Venus has nothing to show (the primary has no Venus image), so withdrawing for
            // good would leave the screen frozen on its last flip. The source is withdrawn
            // (the arbiter owes the desktop its flush), counted, and the presenter starts over
            // five seconds later: a host that comes back is used again.
            if matches!(act, Act::Withdraw) {
                withdraw(adapter);
            }
            {
                let mut g = PRES.lock();
                g.p.reset();
                g.r.reset();
            }
            SYS_GAVE_UP.fetch_add(1, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"RmSysGaveUp", SYS_GAVE_UP.load(Ordering::Relaxed));
            let at = now().saturating_add(rs::RESTART_AFTER_GIVING_UP_100NS);
            RESTART_AT.store(at, Ordering::Release);
            rm_present::set_wake_at(at);
            return;
        }
        match act {
            Act::Idle => return,
            Act::WaitUntil(at) => {
                rm_present::set_wake_at(at);
                return;
            }
            Act::Register => {
                if !register(adapter, epoch, target) {
                    rm_present::set_wake_at(
                        now().saturating_add(helios_kmd_logic::rm_present::RETRY_AFTER_FAIL_100NS),
                    );
                    return;
                }
            }
            Act::Withdraw => {
                withdraw(adapter);
                return;
            }
            Act::CopyFlip { slot } | Act::Reflip { slot } => {
                let copied = matches!(act, Act::CopyFlip { .. });
                let result = flip(passive, adapter, target);
                finish(epoch, slot, copied, result);
                if result != FlipResult::Shown {
                    rm_present::set_wake_at(
                        now().saturating_add(helios_kmd_logic::rm_present::RETRY_AFTER_FAIL_100NS),
                    );
                    return;
                }
            }
        }
    }
}

fn word(p: &Presenter) -> u32 {
    u32::from(p.registered()) | (u32::from(p.gave_up()) << 1) | (u32::from(p.fails()) << 8)
}

#[inline(never)]
fn register(adapter: &AdapterContext, epoch: u64, target: Option<Target>) -> bool {
    let ok = match target {
        Some(t) => adapter
            .foreign_scanout_resident_set(KMD, t.drm, epoch, t.layout)
            .is_ok(),
        None => false,
    };
    if ok {
        SYS_REGS.fetch_add(1, Ordering::Relaxed);
    }
    {
        let mut g = PRES.lock();
        if g.epoch == epoch {
            g.p.registration(ok, now());
        }
    }
    crate::diag::record_named_bytes(b"RmSysReg", u32::from(ok));
    ok
}

#[inline(never)]
fn withdraw(adapter: &AdapterContext) {
    let _ = adapter.foreign_scanout_resident_drop();
    SYS_WITHDRAWN.fetch_add(1, Ordering::Relaxed);
}

/// Flip the target's GEM. No copy: the primary is the memory. Whatever write-combined
/// stores of this core are still in its buffers are drained first (a cached primary needs
/// no more: the compositor samples snooped memory).
#[inline(never)]
fn flip(passive: PassiveLevel, adapter: &AdapterContext, target: Option<Target>) -> FlipResult {
    let Some(t) = target else {
        return FlipResult::Yielded;
    };
    // The allocation may have been destroyed since the pass looked: never name a GEM the
    // service no longer holds.
    if !matches!(live_gem(t.resid), Some((g, _)) if g == t.gem) {
        return FlipResult::Yielded;
    }
    // SAFETY: SSE2 is baseline on x86_64.
    unsafe { core::arch::x86_64::_mm_sfence() };
    // No wait for a release here, by design (module docs): a re-flip of the shown buffer is
    // never released, and a flip to another primary is what releases the previous one.
    match present_within(passive, adapter, KMD, t.drm, t.gem, FLIP_TIMEOUT_MS) {
        Ok(seq) => {
            LAST_SEQ.store(seq, Ordering::Relaxed);
            log_flip(t.gem, seq);
            FlipResult::Shown
        }
        Err(PresentRefusal::NoSource) => FlipResult::Yielded,
        Err(_) => FlipResult::Failed,
    }
}

/// The host took the flip of `gem` as `seq`: remember it for the close of a replaced primary.
fn log_flip(gem: u32, seq: u64) {
    FLIPS.lock().flipped(gem, seq, now());
}

/// What closing `gem` has to wait for (`sysmem::released`, PASSIVE, no lock held).
pub(super) fn close_wait(gem: u32) -> CloseWait {
    FLIPS.lock().closing(gem)
}

/// `gem` was closed: nothing is remembered of it.
pub(super) fn gem_closed(gem: u32) {
    FLIPS.lock().forget(gem);
}

fn finish(epoch: u64, slot: u8, copied: bool, result: FlipResult) {
    let t = now();
    {
        let mut g = PRES.lock();
        if g.epoch == epoch {
            g.p.flipped(slot, copied, result, t);
        }
    }
    match result {
        FlipResult::Shown => {
            YIELDS.store(0, Ordering::Relaxed);
            {
                let mut g = PRES.lock();
                if g.epoch == epoch {
                    g.r.shown(t, copied);
                }
            }
            if copied {
                SYS_FRAMES.fetch_add(1, Ordering::Relaxed);
            } else {
                SYS_REFLIPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        FlipResult::Yielded => {
            SYS_YIELDED.fetch_add(1, Ordering::Relaxed);
            // A source that keeps finding scanout taken with no user source behind it is a
            // registration that does not work: a failure after a few.
            if YIELDS.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_YIELDS {
                YIELDS.store(0, Ordering::Relaxed);
                let mut g = PRES.lock();
                if g.epoch == epoch {
                    g.p.flipped(slot, copied, FlipResult::Failed, t);
                }
            }
        }
        FlipResult::Failed | FlipResult::SourceFailed => {
            SYS_FLIP_FAIL.fetch_add(1, Ordering::Relaxed);
        }
    }
}
