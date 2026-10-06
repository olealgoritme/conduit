//! Option B for any foreign allocation (`ForeignFlip`): when dxgkrnl flips a WDDM allocation
//! that adopted an RM resource a USER-MODE device imported (DWM-on-NVK's swap-chain buffers;
//! `HELIOS_ESCAPE_FOREIGN_RESOURCE` IMPORT_RM, identity FOREIGN), the KMD shows it with its own
//! `ScanoutFlip` of that device's DRM file and GEM instead of `SET_SCANOUT_BLOB` plus a Venus
//! flush. Design, decision table, failure matrix and the hardware checklist:
//! `docs/kmd-rm-client.md` 15.18. The pure decisions are `helios_kmd_logic::foreign_flip`;
//! this file performs them. It is the level 5 flip (`rm_client/sysmem_flip.rs`) for memory
//! the KMD did not make: the same hook in `program_vidpn_source_inner`, the same resident
//! source of the arbiter and `present_within`, the same presenter with a ring of one.
//!
//! THE DIFFERENCES FROM THE LEVEL 5 FLIP, all of them on purpose:
//!
//! * The arbiter's resident source is registered under the IMPORTING device's token and its
//!   DRM file (`Target::owner`, `Target::drm`), so `present_within` proves the same ownership a
//!   user `SCANOUT_PRESENT` does (`mint`: the file is still that device's, same transport
//!   generation) and every end of that device or file ends the source (`release_handle`,
//!   `release_owner`, the suppression gate's re-check).
//! * The importer's file may be closed while the allocation is still the screen's primary (NVK
//!   closes a DRM file when the VkDevice goes), and the host may then reuse the number. The
//!   foreign table poisons every record made from a closed file (`file_closed`, hooked in
//!   `AdapterContext::foreign_scanout_release_handle` / `_release_owner`) and this arm refuses a
//!   poisoned record and drops a shown one. The wire flip carries `(owner_handle, host_handle =
//!   GEM)`, not a resource id; a resid flip would not need the file at all (docs 15.18.6).
//! * Every frame is a different allocation (a swap chain cycles), so the source is updated in
//!   place per flipped allocation: `resident_set` of the same owner keeps the generation.
//! * The memory is GPU-rendered, not written by the CPU, so there is no refresh heartbeat or
//!   tail (`rm_refresh::Refresher`): every change is a `SetVidPnSourceAddress`.
//! * Reuse is the CONSERVATIVE rule (`docs/kmd-rm-client.md` 12.4 item 5): the displayed
//!   address is published at programming, as for every flip, and nothing waits for the host's
//!   `ScanoutReleased`; the swap chain's depth is the protection. (The host's flips are entered
//!   in the release book by `present_within` all the same; see 15.18.5.)
//!
//! LOCKING (`TARGET`, `PRES` leaf spinlocks over plain data, never held across I/O or another
//! lock, never together; the `STATE` of the arbiter is taken only through the adapter
//! methods, after both are released). `program` runs at PASSIVE under the scanout lifecycle
//! lock, as `sysmem_flip::program`; it sends nothing. The flips are the HPD worker's.

use super::foreign_scanout::{present_within, PresentRefusal};
use super::gpu::DeviceOwner;
use super::rm_present;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::foreign_flip::{
    self as ff, ref_name, Book, Change, Facts, Target, Verdict, Why,
};
use helios_kmd_logic::rm_present::{Act, FlipResult, Presenter};
use helios_kmd_logic::rm_refresh as rr;
use helios_kmd_logic::rm_sysmem as rs;

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Acts performed per worker pass (a registration is followed by the first flip).
const ACTS_PER_PASS: usize = 3;
/// How long the host gets to take one flip. The worker flips every frame, StopDevice joins it for
/// a bounded time, and the SAME worker drains `pending_vidpn_allocation` (every later flip's
/// address publication) BEFORE it runs this service, so a slow or silent host used to delay every
/// later publication by up to a second per attempt (it was 1 000 ms, as the level 5 presenter's
/// own, which is untouched). A few frame periods now (`flip_completion::WORKER_FLIP_TIMEOUT_MS`,
/// host-tested bounds); the failure accounting is exactly as it was (a timeout is `Failed`, three
/// in a row give up for five seconds), and every refusal that follows completes its flip as a
/// kept picture (`flip_completion`), so a spurious timeout costs a stale picture, not a held flip.
/// Publication itself never waits for the host: `take` publishes at programming.
const FLIP_TIMEOUT_MS: u64 = helios_kmd_logic::flip_completion::WORKER_FLIP_TIMEOUT_MS;
/// Consecutive flips that found the source yielded before it counts as a failure.
const MAX_YIELDS: u32 = 8;

/// `ForeignFlip` of this transport generation, or [`KNOB_UNREAD`].
static KNOB: AtomicU32 = AtomicU32::new(KNOB_UNREAD);
const KNOB_UNREAD: u32 = u32::MAX;

static TARGET: SpinLock<Book> = SpinLock::new(Book::new());
struct PState {
    epoch: u64,
    p: Presenter,
}
static PRES: SpinLock<PState> = SpinLock::new(PState {
    epoch: 0,
    p: Presenter::new(1),
});
/// The resource the screen shows through this arm, 0 when none: one atomic load for
/// [`other_source`], [`target_gone`] and [`holds_screen`], at any IRQL.
static SHOWN_RESID: AtomicU32 = AtomicU32::new(0);
/// A frame is owed (set by [`program`], taken by the pass): the edge the shared
/// `rm_present` flags carry can be taken by the level 5 service in the one pass that stands
/// down, this one cannot be.
static OWED: AtomicU32 = AtomicU32::new(0);
static YIELDS: AtomicU32 = AtomicU32::new(0);
/// Until when (interrupt time, 100 ns) a failed registration or flip keeps [`program`] from
/// taking another allocation (the presenter's own retry pause; 0 = none).
static FAIL_UNTIL: AtomicU64 = AtomicU64::new(0);
/// When the presenter that gave up starts over (interrupt time, 100 ns; 0 = not waiting).
static RESTART_AT: AtomicU64 = AtomicU64::new(0);
/// The `seq` of the last flip the host took.
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

// Counters (service key, `Ff*`, at most 13 characters; written once the arm has seen an
// allocation: `publish_counters`). `FfProg` allocations taken, of which `FfSame` the same
// one again, `FfMoved` another of the same device, `FfReowned` of another device; `FfNoRec`
// programmed with no foreign record (a plain Venus allocation or a placeholder); `FfRef` refused to
// Venus, `FfWhy` the last reason (`Why::code`), `FfRef01`..`FfRef15` per reason; `FfRegs`
// registrations, `FfRegFail` refused registrations, `FfWithdrawn` withdrawals, `FfGaveUp` giving-ups, `FfFrames` flips for an edge,
// `FfReflips` flips for a resume, `FfYielded` flips that found the source yielded,
// `FfFlipFail` flips refused, `FfStale` flips refused because the importer's file is no longer
// its own (the shown allocation is dropped), `FfGone` shown allocations dropped (destroyed, file
// or device closed), `FfPoison` records poisoned by a close, `FfPres` the presenter word, `FfSeq`
// the last flip's `seq`, `FfEdges` frames owed.
static PROG: AtomicU32 = AtomicU32::new(0);
static SAME: AtomicU32 = AtomicU32::new(0);
static MOVED: AtomicU32 = AtomicU32::new(0);
static REOWNED: AtomicU32 = AtomicU32::new(0);
static NO_REC: AtomicU32 = AtomicU32::new(0);
static REFUSED: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static REFUSED_BY: [AtomicU32; Why::COUNT] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static REGS: AtomicU32 = AtomicU32::new(0);
static REG_FAIL: AtomicU32 = AtomicU32::new(0);
static WITHDRAWN: AtomicU32 = AtomicU32::new(0);
static GAVE_UP: AtomicU32 = AtomicU32::new(0);
static FRAMES: AtomicU32 = AtomicU32::new(0);
static REFLIPS: AtomicU32 = AtomicU32::new(0);
static YIELDED: AtomicU32 = AtomicU32::new(0);
static FLIP_FAIL: AtomicU32 = AtomicU32::new(0);
static STALE: AtomicU32 = AtomicU32::new(0);
static GONE: AtomicU32 = AtomicU32::new(0);
static POISONED: AtomicU32 = AtomicU32::new(0);
static EDGES: AtomicU32 = AtomicU32::new(0);
static PRES_WORD: AtomicU32 = AtomicU32::new(0);
/// The `Ff*` block owes the service key one full write (zeros included) for this transport
/// generation: set by [`forget`], taken by [`publish_counters`]. A value left in the registry by
/// a previous run is then never read as live, whether or not this generation sees a flip.
static MIRROR_PENDING: AtomicU32 = AtomicU32::new(1);

/// Mirror the counters to the service key. PASSIVE only. Nothing is written until the knob
/// was on and an allocation was seen, so a box with the knob off gets no new value.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let seen = PROG.load(Ordering::Relaxed)
        | NO_REC.load(Ordering::Relaxed)
        | REFUSED.load(Ordering::Relaxed)
        | POISONED.load(Ordering::Relaxed);
    // Once per generation the whole block is written even when nothing was seen (zeros), so a
    // block from an earlier run cannot be read as this one's. The knob is read (and mirrored)
    // first if this generation has not yet.
    let owed = MIRROR_PENDING.swap(0, Ordering::AcqRel) != 0;
    if seen == 0 && !owed {
        return;
    }
    if owed {
        let _ = knob_on();
    }
    rec(b"FfKnob", KNOB.load(Ordering::Relaxed).min(0xFF));
    rec(b"FfProg", PROG.load(Ordering::Relaxed));
    rec(b"FfSame", SAME.load(Ordering::Relaxed));
    rec(b"FfMoved", MOVED.load(Ordering::Relaxed));
    rec(b"FfReowned", REOWNED.load(Ordering::Relaxed));
    rec(b"FfNoRec", NO_REC.load(Ordering::Relaxed));
    rec(b"FfRef", REFUSED.load(Ordering::Relaxed));
    rec(b"FfWhy", WHY.load(Ordering::Relaxed));
    for why in Why::ALL {
        rec(
            &ref_name(why),
            REFUSED_BY[why.index()].load(Ordering::Relaxed),
        );
    }
    rec(b"FfRegs", REGS.load(Ordering::Relaxed));
    rec(b"FfRegFail", REG_FAIL.load(Ordering::Relaxed));
    rec(b"FfWithdrawn", WITHDRAWN.load(Ordering::Relaxed));
    rec(b"FfGaveUp", GAVE_UP.load(Ordering::Relaxed));
    rec(b"FfFrames", FRAMES.load(Ordering::Relaxed));
    rec(b"FfReflips", REFLIPS.load(Ordering::Relaxed));
    rec(b"FfYielded", YIELDED.load(Ordering::Relaxed));
    rec(b"FfFlipFail", FLIP_FAIL.load(Ordering::Relaxed));
    rec(b"FfStale", STALE.load(Ordering::Relaxed));
    rec(b"FfGone", GONE.load(Ordering::Relaxed));
    rec(b"FfPoison", POISONED.load(Ordering::Relaxed));
    rec(b"FfPres", PRES_WORD.load(Ordering::Relaxed));
    rec(b"FfSeq", LAST_SEQ.load(Ordering::Relaxed) as u32);
    rec(b"FfEdges", EDGES.load(Ordering::Relaxed));
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

// ---- the knob --------------------------------------------------------------------------

/// `ForeignFlip` for this transport generation: one relaxed load once read. PASSIVE (the
/// first call reads the service key).
fn knob_on() -> bool {
    let v = KNOB.load(Ordering::Relaxed);
    if v != KNOB_UNREAD {
        return v != 0;
    }
    read_knob() != 0
}

#[inline(never)]
fn read_knob() -> u32 {
    let v = crate::diag::read_config_dword(crate::diag::knobs::FOREIGN_FLIP, 0);
    KNOB.store(v, Ordering::Relaxed);
    // Mirrored on EVERY read, 0 included: "nothing is written for the default" left FfKnob = 1 in
    // the registry after the knob was set back to 0 and the device restarted (tester evidence),
    // and the whole `Ff*` block with it.
    crate::diag::record_named_bytes(b"FfKnob", v.min(0xFF));
    v
}

/// Whether `ForeignFlip` is on for this transport generation (`Present` and
/// `CreateAllocation` ask: PASSIVE, the first call reads the service key).
pub(crate) fn enabled() -> bool {
    knob_on()
}

/// Forget everything (the transport generation ended; `retire_transport`). The next
/// generation reads the knob again.
pub(crate) fn forget() {
    TARGET.lock().clear();
    {
        let mut g = PRES.lock();
        g.epoch = 0;
        g.p.reset();
    }
    SHOWN_RESID.store(0, Ordering::Release);
    OWED.store(0, Ordering::Release);
    YIELDS.store(0, Ordering::Relaxed);
    RESTART_AT.store(0, Ordering::Release);
    FAIL_UNTIL.store(0, Ordering::Release);
    LAST_SEQ.store(0, Ordering::Relaxed);
    KNOB.store(KNOB_UNREAD, Ordering::Relaxed);
    // The counters are this generation's: zero them, and owe the service key the zero block.
    for c in [
        &PROG, &SAME, &MOVED, &REOWNED, &NO_REC, &REFUSED, &WHY, &REGS, &REG_FAIL, &WITHDRAWN,
        &GAVE_UP, &FRAMES, &REFLIPS, &YIELDED, &FLIP_FAIL, &STALE, &GONE, &POISONED, &EDGES,
        &PRES_WORD, &YIELDS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for c in &REFUSED_BY {
        c.store(0, Ordering::Relaxed);
    }
    MIRROR_PENDING.store(1, Ordering::Release);
}

/// Whether this arm's allocation is what the screen shows: the level 5 service leaves the
/// shared frame and resume edges alone then (they are this arm's).
pub(crate) fn holds_screen() -> bool {
    SHOWN_RESID.load(Ordering::Acquire) != 0
}

// ---- the programming hook --------------------------------------------------------------

/// What [`program`] decided.
pub(crate) enum Programmed {
    /// Not this arm's (knob off, no foreign record, the KMD's own sysmem): the Venus path goes
    /// on, untouched.
    NotOurs,
    /// Made the shown source; the worker flips it.
    Ok,
    /// Refused with a counted reason: the Venus path goes on, after [`other_source`].
    Refused,
}

/// The facts of one programmed allocation, gathered under one hold of the virtio lock.
#[inline(never)]
fn gather(
    adapter: &AdapterContext,
    resource_id: u32,
    width: u32,
    height: u32,
    direct_scanout: bool,
) -> Facts {
    let host_import = crate::virtio::foreign::rm_import_served(adapter);
    let level = super::rm_client::level_if_read();
    let (record, epoch, owner_file) = adapter
        .with_virtio(|v| {
            let record = v.foreign_flip_record(resource_id);
            let owner_file = record.and_then(|r| {
                DeviceOwner::from_token(r.origin as usize)
                    .and_then(|o| v.nvrm_handle_device_type(o, r.rm_handle))
            });
            (record, v.nvrm_epoch(), owner_file)
        })
        .unwrap_or((None, 0, None));
    Facts {
        knob: true,
        level,
        epoch,
        display: adapter.display_half(),
        host_import,
        kmd_token: KMD.raw() as u64,
        mode: (width, height),
        record,
        owner_file,
        failing: is_failing(),
        direct_scanout,
    }
}

/// Flips are not working: the presenter gave up (and its restart is pending), or a
/// registration or a flip failed within the retry pause. Two atomics and the presenter's
/// leaf lock; the pure rule is `foreign_flip::failing`.
fn is_failing() -> bool {
    let gave_up = PRES.lock().p.gave_up();
    ff::failing(
        now(),
        RESTART_AT.load(Ordering::Acquire),
        gave_up,
        FAIL_UNTIL.load(Ordering::Acquire),
    )
}

/// A registration or a flip failed: no new allocation is taken before the presenter's own
/// retry pause is over (the screen is the Venus path's meanwhile).
fn note_failure() {
    let until = now().saturating_add(helios_kmd_logic::rm_present::RETRY_AFTER_FAIL_100NS);
    FAIL_UNTIL.fetch_max(until, Ordering::AcqRel);
}

/// `SetVidPnSourceAddress` of `resource_id` at the mode's extent `width` x `height`, from
/// `program_vidpn_source_inner` (PASSIVE, under the scanout lifecycle lock), after the level 5
/// arm declined. Sends nothing; takes no lock across I/O. With the knob off it is one relaxed
/// load and the answer is `NotOurs`.
#[inline(never)]
pub(crate) fn program(
    adapter: &AdapterContext,
    resource_id: u32,
    primary_address: u64,
    width: u32,
    height: u32,
    direct_scanout: bool,
) -> Programmed {
    if !knob_on() {
        return Programmed::NotOurs;
    }
    let facts = gather(adapter, resource_id, width, height, direct_scanout);
    match ff::decide_for(resource_id, &facts) {
        Verdict::Off | Verdict::Sysmem => Programmed::NotOurs,
        Verdict::NotForeign => {
            NO_REC.fetch_add(1, Ordering::Relaxed);
            Programmed::NotOurs
        }
        Verdict::Refuse(why) => {
            note_refusal(why);
            Programmed::Refused
        }
        Verdict::Take(target) => take(adapter, target, primary_address, width, height),
    }
}

fn note_refusal(why: Why) {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    REFUSED_BY[why.index()].fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
}

/// The allocation is the screen's source from here on: record it, publish what a bind
/// publishes, tell a registered resident source, owe a frame.
#[inline(never)]
fn take(
    adapter: &AdapterContext,
    target: Target,
    primary_address: u64,
    width: u32,
    height: u32,
) -> Programmed {
    let Some(owner) = DeviceOwner::from_token(target.owner as usize) else {
        note_refusal(Why::OwnerGone);
        return Programmed::Refused;
    };
    let change = TARGET.lock().set(target);
    SHOWN_RESID.store(target.resid, Ordering::Release);
    // Which allocation is the active scanout, and the address the vsync reports: what a bind
    // publishes (the same as the level 5 flip). The host is NOT bound to it
    // (`host_bound_scanout_resource` stays), so a Venus flush of it is refused loudly (`RfUnb`)
    // instead of being sent for a resource with no scanout.
    adapter.with_wddm_notify_lock(|_| {
        adapter
            .active_scanout_wh
            .store(((width as u64) << 32) | height as u64, Ordering::Release);
        adapter
            .active_scanout_resource
            .store(target.resid, Ordering::Release);
        adapter.publish_bound_primary(primary_address);
    });
    adapter.release_all_scanout_leases(crate::ddi::scanout_trace::LeaseEnd::Cancelled);
    // A registered resident source learns the new picture in place (the arbiter keeps its
    // generation for the same device); an unregistered one is registered by the worker.
    if PRES.lock().p.registered() && change.updates_source() {
        let _ =
            adapter.foreign_scanout_resident_set(owner, target.drm, target.epoch, target.layout);
    }
    PROG.fetch_add(1, Ordering::Relaxed);
    match change {
        Change::Same => SAME.fetch_add(1, Ordering::Relaxed),
        Change::Moved => MOVED.fetch_add(1, Ordering::Relaxed),
        Change::Reowned => REOWNED.fetch_add(1, Ordering::Relaxed),
        Change::New => 0,
    };
    // A frame is owed for every programming, also of the same allocation again.
    OWED.store(1, Ordering::Release);
    EDGES.fetch_add(1, Ordering::Relaxed);
    rm_present::note_frame_edge(adapter);
    Programmed::Ok
}

/// The screen's source is not this arm's allocation any more (a Venus allocation, or the level
/// 5 primary, was programmed): forget the target; the worker withdraws the resident source.
/// One relaxed load when nothing is shown.
pub(crate) fn other_source(adapter: &AdapterContext) {
    if SHOWN_RESID.load(Ordering::Acquire) == 0 {
        return;
    }
    TARGET.lock().clear();
    SHOWN_RESID.store(0, Ordering::Release);
    adapter.signal_hpd();
}

/// The allocation `resource_id` is being destroyed (`retire_scanout_allocation`): if it is the
/// shown one, forget it, so no flip names a GEM its importer may close next.
pub(crate) fn target_gone(adapter: &AdapterContext, resource_id: u32) {
    if SHOWN_RESID.load(Ordering::Acquire) != resource_id || resource_id == 0 {
        return;
    }
    if TARGET.lock().gone(resource_id) {
        SHOWN_RESID.store(0, Ordering::Release);
        GONE.fetch_add(1, Ordering::Relaxed);
        adapter.signal_hpd();
    }
}

/// `owner` closed DRM file `drm` (a successful forwarded `Close`, or the sweep). PASSIVE.
/// Every record made from it is poisoned, the shown allocation made from it is dropped.
pub(crate) fn file_closed(adapter: &AdapterContext, owner: DeviceOwner, drm: u32) {
    if !knob_on() {
        return;
    }
    if let Ok(n) = adapter.with_virtio(|v| v.foreign_file_closed(owner, drm)) {
        POISONED.fetch_add(n as u32, Ordering::Relaxed);
    }
    if SHOWN_RESID.load(Ordering::Acquire) != 0
        && TARGET.lock().file_closed(owner.raw() as u64, drm)
    {
        SHOWN_RESID.store(0, Ordering::Release);
        GONE.fetch_add(1, Ordering::Relaxed);
        adapter.signal_hpd();
    }
}

/// `owner`'s device is gone (DestroyDevice / StopDevice). PASSIVE.
pub(crate) fn owner_closed(adapter: &AdapterContext, owner: DeviceOwner) {
    if !knob_on() {
        return;
    }
    if let Ok(n) = adapter.with_virtio(|v| v.foreign_owner_closed(owner)) {
        POISONED.fetch_add(n as u32, Ordering::Relaxed);
    }
    if SHOWN_RESID.load(Ordering::Acquire) != 0 && TARGET.lock().owner_closed(owner.raw() as u64) {
        SHOWN_RESID.store(0, Ordering::Release);
        GONE.fetch_add(1, Ordering::Relaxed);
        adapter.signal_hpd();
    }
}

// ---- the worker ------------------------------------------------------------------------

/// One pass of the flip service, from the HPD worker's loop (PASSIVE), after the level 5 /
/// ring service. With the knob off or nothing shown or registered it is a load or two.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    // Knob off (read and zero): one load. Unread: the first `program` reads it.
    if KNOB.load(Ordering::Relaxed) == 0 {
        return;
    }
    if SHOWN_RESID.load(Ordering::Acquire) == 0 && !PRES.lock().p.registered() {
        return;
    }
    service_pass(passive, adapter);
}

#[inline(never)]
fn service_pass(passive: PassiveLevel, adapter: &AdapterContext) {
    // A presenter that gave up waits out its pause whatever wakes the worker.
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
    // The shared edges are this arm's only while it holds the screen (the level 5 service
    // leaves them alone then); otherwise it is only standing down and takes none.
    let holds = holds_screen();
    let (mut frame_edge, mut resume_edge) = if holds {
        rm_present::clear_wake_at();
        rm_present::take_edges()
    } else {
        (false, false)
    };
    if holds {
        let _ = rm_present::take_edge_count();
        frame_edge |= OWED.swap(0, Ordering::AcqRel) != 0;
    }
    let interval = rr::flip_interval_100ns(adapter.effective_refresh_mhz());
    {
        let mut g = PRES.lock();
        if g.epoch != epoch {
            g.p.reset();
            g.epoch = epoch;
        }
        g.p.set_min_interval(interval);
    }
    for _ in 0..ACTS_PER_PASS {
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            return;
        }
        let t = now();
        let target = TARGET.lock().current();
        let ready = ff::target_ready(target.as_ref(), epoch);
        // Only a source registered for a user device is this arm's.
        let (has_resident, foreground) = adapter.foreign_scanout_resident_state_of(false);
        let inputs = rs::flip_inputs(t, ready, has_resident, foreground, frame_edge, resume_edge);
        frame_edge = false;
        resume_edge = false;
        let (act, word, gave_up) = {
            let mut g = PRES.lock();
            if g.epoch != epoch {
                g.p.reset();
                g.epoch = epoch;
                g.p.set_min_interval(interval);
            }
            let act = g.p.decide(inputs);
            (act, word(&g.p), g.p.gave_up())
        };
        PRES_WORD.store(word, Ordering::Relaxed);
        if gave_up {
            // Three failures in a row: withdraw, and start over five seconds later (the
            // next allocation Windows flips is looked at afresh; nothing is frozen for good
            // because every programming goes through [`program`] again).
            if matches!(act, Act::Withdraw) {
                withdraw(adapter);
            }
            PRES.lock().p.reset();
            // The restart pause below is what `program` reads as "failing" from here on.
            GAVE_UP.fetch_add(1, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"FfGaveUp", GAVE_UP.load(Ordering::Relaxed));
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
    let ok = match target.and_then(|t| DeviceOwner::from_token(t.owner as usize).map(|o| (o, t))) {
        Some((owner, t)) => adapter
            .foreign_scanout_resident_set(owner, t.drm, epoch, t.layout)
            .is_ok(),
        None => false,
    };
    if ok {
        REGS.fetch_add(1, Ordering::Relaxed);
    } else {
        // Counted and mirrored by the throttled path (Windows may alternate between Venus and
        // foreign allocations: no registry write per registration).
        REG_FAIL.fetch_add(1, Ordering::Relaxed);
        note_failure();
    }
    {
        let mut g = PRES.lock();
        if g.epoch == epoch {
            g.p.registration(ok, now());
        }
    }
    ok
}

#[inline(never)]
fn withdraw(adapter: &AdapterContext) {
    // Only the source registered for a user device: the level 5 / ring presenters' own is
    // theirs to withdraw.
    let _ = adapter.foreign_scanout_resident_drop_of(false);
    WITHDRAWN.fetch_add(1, Ordering::Relaxed);
}

/// Flip the target's GEM of its importer's DRM file. No copy, no wait for a release. The
/// importer's file is re-proved by `present_within` (`mint`: still that device's, this
/// generation); a refusal that says it is not is the end of this target.
#[inline(never)]
fn flip(passive: PassiveLevel, adapter: &AdapterContext, target: Option<Target>) -> FlipResult {
    let Some(t) = target else {
        return FlipResult::Yielded;
    };
    let Some(owner) = DeviceOwner::from_token(t.owner as usize) else {
        return FlipResult::Failed;
    };
    // The allocation may have been destroyed, or its file closed, since the pass looked:
    // never name a GEM of a target the book no longer holds.
    if TARGET.lock().current() != Some(t) {
        return FlipResult::Yielded;
    }
    match present_within(passive, adapter, owner, t.drm, t.gem, FLIP_TIMEOUT_MS) {
        Ok(seq) => {
            LAST_SEQ.store(seq, Ordering::Relaxed);
            FlipResult::Shown
        }
        Err(PresentRefusal::NoSource) => FlipResult::Yielded,
        Err(PresentRefusal::NotOwned) | Err(PresentRefusal::Forbidden) => {
            // The importer's file is not its own any more and no hook saw it: drop the
            // allocation (the worker withdraws the source next) and refuse the record.
            STALE.fetch_add(1, Ordering::Relaxed);
            if TARGET.lock().gone(t.resid) {
                SHOWN_RESID.store(0, Ordering::Release);
                GONE.fetch_add(1, Ordering::Relaxed);
            }
            let _ = adapter.with_virtio(|v| v.foreign_file_closed(owner, t.drm));
            FlipResult::Failed
        }
        Err(_) => FlipResult::Failed,
    }
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
            if copied {
                FRAMES.fetch_add(1, Ordering::Relaxed);
            } else {
                REFLIPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        FlipResult::Yielded => {
            YIELDED.fetch_add(1, Ordering::Relaxed);
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
            FLIP_FAIL.fetch_add(1, Ordering::Relaxed);
            note_failure();
        }
    }
}
