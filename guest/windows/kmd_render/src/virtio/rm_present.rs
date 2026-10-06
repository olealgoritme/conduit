//! The KMD's RM scanout presenter (`KmdRmClient` = 3): the I/O half. Design, the CPU
//! access decision, performance and the hardware checklist: `docs/kmd-rm-client.md`
//! section 13. Every decision (which surface is written, when a frame is due, when
//! the source yields to a user-mode source, when to give up) is
//! `helios_kmd_logic::rm_present` and `helios_kmd_logic::foreign_scanout`; this file
//! performs them.
//!
//! WHAT IT DOES. While the VidPn primary that is bound to scanout 0 is the adapter's
//! LINEAR scanout picture (a Venus blob the display worker keeps current), the
//! desktop is shown through a ring of RM video-memory surfaces instead of Venus'
//! `RESOURCE_FLUSH`: every time the desktop would have been flushed, the gate in
//! `adapter/foreign_scanout.rs` withholds the flush and raises [`note_frame_edge`];
//! this worker pass then copies the primary's rows into the ring surface that is NOT
//! on screen (a write-combined store loop through the surface's RM-window view) and
//! sends a `ScanoutFlip` for it, through the same `present` path user mode uses. A
//! user-mode source (an NVK game) that takes scanout 0 preempts it at once; when that
//! one ends, [`note_resume_edge`] makes this pass flip the front surface again.
//!
//! The GDI bytes themselves stay where dxgkrnl's CPU aperture put them (the Venus
//! window head): see the doc for why RM memory cannot be what GDI writes, and what
//! replaces the source once Venus is gone.
//!
//! WHEN IT RUNS. Only on the HPD worker thread (PASSIVE), from
//! [`super::rm_client::service`], and only at level 3 with a complete ring. At any
//! other level, and with the knob at 0, nothing here runs.
//!
//! LOCKING. `PRESENTER` is a leaf spinlock over plain data, never held across I/O or
//! another lock. The surface views belong to `rm_client::CLIENT`; the copy takes a
//! LEASE on a slot (under that lock) and holds no lock while it copies: the retire
//! path waits for the lease before it unmaps the views.
//!
//! FAILING CLOSED. A registration, a copy or a flip that fails three times in a row
//! ends the presenter for the transport generation: the resident source is withdrawn
//! (the arbiter asks for one Venus desktop flush) and Venus has the screen again.
//! A frame that cannot be shown costs that frame, never the desktop.

use super::foreign_scanout::{present_within, PresentRefusal};
use super::gpu::{DeviceOwner, OwnerFilter};
use super::rm_client::{self, end_lease, lease_slot};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::rm_client::{flip_layout, Want, RING_SLOTS};
use helios_kmd_logic::rm_present::{
    source_layout, Act, CopyPlan, FlipResult, Inputs, Presenter, SourceLayout, SourceRefusal,
};
use helios_kmd_logic::sweep_budget::{SweepBudget, UNITS_PER_MS};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::PHYSICAL_ADDRESS;

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Acts performed per worker pass: a registration is followed by the first frame in
/// the same pass, nothing needs more.
const ACTS_PER_PASS: usize = 3;
/// Frames between two counter mirrors (about ten seconds at 60 frames a second); the
/// events (registration, withdrawal, the first frame, a failure, giving up) mirror at
/// once.
const MIRROR_EVERY_FRAMES: u32 = 600;
/// How long the host gets to take one frame's flip. The reply is a bare header the
/// backend sends without waiting for the viewer, so this is generous; it is short
/// because the HPD worker flips every frame and StopDevice joins it for a bounded time.
const FLIP_TIMEOUT_MS: u64 = 1_000;
/// How long the host gets to map the primary's blob (`RESOURCE_MAP_BLOB`, the first
/// frame of a blob; later frames find the mapping and send nothing), for the same reason.
const MAP_TIMEOUT_MS: u64 = 1_000;
/// How long after a frame that could not be shown the worker is woken to try again
/// (100 ms): the failure counter, and so giving up, moves only if it does. The
/// presenter itself refuses to act earlier (`helios_kmd_logic::rm_present::
/// RETRY_AFTER_FAIL_100NS`, the same 100 ms), so this is only when the worker is woken.
const RETRY_AFTER_100NS: u64 = helios_kmd_logic::rm_present::RETRY_AFTER_FAIL_100NS;
/// Consecutive flips that found the source yielded before it counts as a failure (a
/// registration the arbiter keeps refusing to let us show, with no user source).
pub const MAX_YIELDS: u32 = 8;

// ---- state -----------------------------------------------------------------------

struct PState {
    /// The transport generation `p` belongs to (0 = none).
    epoch: u64,
    p: Presenter,
}

static PRESENTER: SpinLock<PState> = SpinLock::new(PState {
    epoch: 0,
    p: Presenter::new(RING_SLOTS as u8),
});

/// The desktop wanted a flush since the worker last looked (raised at the suppression
/// gate, any thread up to DISPATCH).
static FRAME_EDGE: AtomicU32 = AtomicU32::new(0);
/// A user source ended and the resident one took the screen back.
static RESUME_EDGE: AtomicU32 = AtomicU32::new(0);
/// Absolute time (100 ns) the worker must wake at for a paced frame, 0 = none.
static WAKE_AT: AtomicU64 = AtomicU64::new(0);
/// Consecutive yielded flips.
static YIELDS: AtomicU32 = AtomicU32::new(0);

// Counters, mirrored by `publish_counters` (names at most 14 characters). `RmPStage` is
// the stage the presenter started last, written BEFORE it runs (readable by symbol
// even if the registry write never happens): 1 register, 2 withdraw, 3 map source,
// 4 copy, 5 flip, 6 re-flip. `RmPres` is the presenter's word: bit 0 registered, bit 1
// gave up, bits 8.. consecutive failures, bits 16.. front surface + 1.
pub static RM_PSTAGE: AtomicU32 = AtomicU32::new(0);
pub static RM_PRES: AtomicU32 = AtomicU32::new(0);
/// Registrations the arbiter took / withdrawals / times the presenter gave up.
pub static RM_REGS: AtomicU32 = AtomicU32::new(0);
pub static RM_WITHDRAWN: AtomicU32 = AtomicU32::new(0);
/// Times the arbiter ended the resident source under the presenter (its DRM file was
/// closed, or its generation or epoch went invalid).
pub static RM_RES_ENDED: AtomicU32 = AtomicU32::new(0);
pub static RM_GAVE_UP: AtomicU32 = AtomicU32::new(0);
/// Frames copied and flipped / surfaces re-flipped for a resume.
pub static RM_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static RM_REFLIPS: AtomicU32 = AtomicU32::new(0);
/// Flips that found the source yielded to a user-mode source, user sources that
/// preempted the resident one, resume edges answered.
pub static RM_YIELDED: AtomicU32 = AtomicU32::new(0);
pub static RM_PREEMPTED: AtomicU32 = AtomicU32::new(0);
pub static RM_RESUMES: AtomicU32 = AtomicU32::new(0);
/// Flips the host or the transport refused, source reads that failed (map or plan).
pub static RM_FLIP_FAIL: AtomicU32 = AtomicU32::new(0);
pub static RM_SRC_BAD: AtomicU32 = AtomicU32::new(0);
/// The last copy's duration in ms and the longest; megabytes copied in all.
pub static RM_COPY_MS: AtomicU32 = AtomicU32::new(0);
pub static RM_COPY_MAX_MS: AtomicU32 = AtomicU32::new(0);
pub static RM_COPY_MB: AtomicU32 = AtomicU32::new(0);
/// Why the source was last refused (`SourceRefusal` + 1; 0 = accepted): the answer to
/// "level 3 is up and nothing shows".
pub static RM_SRC_WHY: AtomicU32 = AtomicU32::new(0);
/// Shown frames so far, for the throttled counter mirror.
static MIRROR_TICK: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the registry. PASSIVE only. Nothing is written until the
/// presenter has done something.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if RM_PSTAGE.load(Ordering::Relaxed) == 0 {
        return;
    }
    rec(b"RmPStage", RM_PSTAGE.load(Ordering::Relaxed));
    rec(b"RmPres", RM_PRES.load(Ordering::Relaxed));
    rec(b"RmRegs", RM_REGS.load(Ordering::Relaxed));
    rec(b"RmWithdrawn", RM_WITHDRAWN.load(Ordering::Relaxed));
    rec(b"RmResEnd", RM_RES_ENDED.load(Ordering::Relaxed));
    rec(b"RmGaveUp", RM_GAVE_UP.load(Ordering::Relaxed));
    rec(b"RmFrames", RM_FRAMES.load(Ordering::Relaxed));
    rec(b"RmReflips", RM_REFLIPS.load(Ordering::Relaxed));
    rec(b"RmYielded", RM_YIELDED.load(Ordering::Relaxed));
    rec(b"RmPreempted", RM_PREEMPTED.load(Ordering::Relaxed));
    rec(b"RmResumes", RM_RESUMES.load(Ordering::Relaxed));
    rec(b"RmFlipFail", RM_FLIP_FAIL.load(Ordering::Relaxed));
    rec(b"RmSrcBad", RM_SRC_BAD.load(Ordering::Relaxed));
    rec(b"RmCopyMs", RM_COPY_MS.load(Ordering::Relaxed));
    rec(b"RmCopyMaxMs", RM_COPY_MAX_MS.load(Ordering::Relaxed));
    rec(b"RmCopyMB", RM_COPY_MB.load(Ordering::Relaxed));
    rec(b"RmSrcWhy", RM_SRC_WHY.load(Ordering::Relaxed));
}

/// Whether this generation's giving up has been counted (`RmGaveUp`).
static GAVE_UP_COUNTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Time of the last throttled counter mirror / source-verdict record, 100 ns.
static LAST_MIRROR: AtomicU64 = AtomicU64::new(0);
/// At most one throttled registry mirror a second: a source verdict or a registration
/// that flaps (a UMD image bound and unbound) must not make the worker a registry
/// writer.
const MIRROR_MIN_INTERVAL_100NS: u64 = 10_000_000;

/// [`publish_counters`], at most once per [`MIRROR_MIN_INTERVAL_100NS`]. PASSIVE.
fn publish_counters_throttled() {
    if mirror_due() {
        publish_counters();
    }
}

/// Whether a throttled registry write may happen now (and claims the slot).
fn mirror_due() -> bool {
    let now = now_100ns();
    let last = LAST_MIRROR.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < MIRROR_MIN_INTERVAL_100NS {
        return false;
    }
    LAST_MIRROR.store(now, Ordering::Relaxed);
    true
}

// ---- edges -------------------------------------------------------------------------

/// The desktop wanted a host flush and the gate withheld it (the resident source is on
/// screen): a frame is due. Atomics and `KeSetEvent(Wait = FALSE)` only: legal at
/// any IRQL up to DISPATCH.
pub(crate) fn note_frame_edge(adapter: &AdapterContext) {
    FRAME_EDGE.store(1, Ordering::Release);
    adapter.signal_hpd();
}

/// A user source ended and the resident one has scanout 0 again: re-flip it. Same
/// context rules as [`note_frame_edge`].
pub(crate) fn note_resume_edge(adapter: &AdapterContext) {
    RESUME_EDGE.store(1, Ordering::Release);
    adapter.signal_hpd();
}

/// The desktop wanted a flush while a USER source holds scanout 0 (the gate withheld
/// it): the KMD's parked resident source, if there is one, is out of date. Only a flag:
/// the worker is not woken (the user source owns the screen), and the presenter folds
/// it into the frame it owes for the moment the user source ends. A flag nobody reads
/// is harmless; it is not raised unless the ring level is on.
pub(crate) fn note_desktop_changed() {
    if rm_client::ring_level_on() {
        FRAME_EDGE.store(1, Ordering::Release);
    }
}

/// A user source preempted the resident one (counted by the arbiter's `set`).
pub(crate) fn note_preempted() {
    RM_PREEMPTED.fetch_add(1, Ordering::Relaxed);
}

/// For the HPD worker's wait: the absolute time (100 ns) a paced frame is due, or 0
/// when none waits.
pub(crate) fn wake_at() -> u64 {
    WAKE_AT.load(Ordering::Acquire)
}

/// Level 5 (`rm_client::sysmem_flip`) shares the edges and the worker's timed wake with the
/// presenter above (the two never run in the same generation): take the frame and resume
/// edges raised since the last look, `(frame, resume)`.
pub(crate) fn take_edges() -> (bool, bool) {
    (
        FRAME_EDGE.swap(0, Ordering::AcqRel) != 0,
        RESUME_EDGE.swap(0, Ordering::AcqRel) != 0,
    )
}

/// Ask the worker to wake at `at` (100 ns, absolute) for a paced flip.
pub(crate) fn set_wake_at(at: u64) {
    WAKE_AT.store(at, Ordering::Release);
}

/// Forget a wake nobody is owed any more.
pub(crate) fn clear_wake_at() {
    WAKE_AT.store(0, Ordering::Release);
}

/// Forget the presenter (the transport generation ended, or is being retired). The
/// kernel views are the client's to unmap; this holds none.
pub(crate) fn reset() {
    {
        let mut g = PRESENTER.lock();
        g.epoch = 0;
        g.p.reset();
    }
    FRAME_EDGE.store(0, Ordering::Release);
    RESUME_EDGE.store(0, Ordering::Release);
    WAKE_AT.store(0, Ordering::Release);
    YIELDS.store(0, Ordering::Release);
    GAVE_UP_COUNTED.store(false, Ordering::Release);
}

// ---- the pass ----------------------------------------------------------------------

fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// The primary bound to scanout 0, judged for a copy, read coherently (the
/// `primary_scanout_*` seqlock): `Ok((layout, resource id))`.
fn read_source(
    adapter: &AdapterContext,
    ring: (u32, u32),
) -> Result<(SourceLayout, u32), SourceRefusal> {
    for _ in 0..4 {
        let s1 = adapter.primary_scanout_seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            continue;
        }
        let active = adapter.active_scanout_resource.load(Ordering::Acquire);
        let primary = adapter.primary_scanout_resource.load(Ordering::Acquire);
        let wh = adapter.primary_scanout_wh.load(Ordering::Relaxed);
        let layout = adapter.primary_scanout_layout.load(Ordering::Relaxed);
        let size = adapter.primary_scanout_alloc_size.load(Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Acquire);
        if adapter.primary_scanout_seq.load(Ordering::Acquire) != s1 {
            continue;
        }
        return source_layout(active, primary, wh, layout, size, ring).map(|l| (l, primary));
    }
    // A publisher kept the set incoherent for four reads: not now.
    Err(SourceRefusal::NoSource)
}

/// One pass of the presenter, from [`rm_client::service`] (PASSIVE, the HPD worker),
/// at level 3. `want` is what the client was driven toward this pass.
///
/// `service` calls this twice per worker pass (before the client's steps, so a ring about
/// to be torn down is withdrawn first, and after them, so a ring completed in this pass is
/// used in it). `again` is the second call: it leaves the pass alone while a wake the
/// first call asked for (a paced frame, the pause after a failure) is still in the
/// future, instead of acting on a decision the first call already made. (The presenter
/// enforces its own pauses too, `Presenter::decide`; this keeps the second call from
/// even looking.)
#[inline(never)]
pub(crate) fn service(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
    want: Want,
    again: bool,
) {
    if again && WAKE_AT.load(Ordering::Acquire) > now_100ns() {
        return;
    }
    WAKE_AT.store(0, Ordering::Release);
    let mut frame_edge = FRAME_EDGE.swap(0, Ordering::AcqRel) != 0;
    let mut resume_edge = RESUME_EDGE.swap(0, Ordering::AcqRel) != 0;
    if resume_edge {
        RM_RESUMES.fetch_add(1, Ordering::Relaxed);
    }
    for _ in 0..ACTS_PER_PASS {
        // The worker is being stopped: start nothing. What is registered is ended by
        // the transport reset that follows.
        if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
            return;
        }
        let now = now_100ns();
        let ring_ready = rm_client::ring_ready(epoch, want);
        let source = match want.surface {
            Some(wh) => read_source(adapter, wh),
            None => Err(SourceRefusal::NoSource),
        };
        let why = source.as_ref().err().map_or(0, |e| *e as u32 + 1);
        if RM_SRC_WHY.swap(why, Ordering::Relaxed) != why && mirror_due() {
            // A change of verdict is an event: name it in the registry (throttled; the
            // value is also in every counter mirror).
            crate::diag::record_named_bytes(b"RmSrcWhy", why);
        }
        let (has_resident, foreground) = adapter.foreign_scanout_resident_state();
        let inputs = Inputs {
            now,
            ring_ready,
            source_ok: source.is_ok(),
            arbiter_has_resident: has_resident,
            foreground,
            frame_edge,
            resume_edge,
        };
        // Folded into the presenter now; a second act of this pass sees none.
        frame_edge = false;
        resume_edge = false;
        let act = {
            let mut g = PRESENTER.lock();
            if g.epoch != epoch {
                g.p.reset();
                g.epoch = epoch;
            }
            let act = g.p.decide(inputs);
            let g_word = word(&g.p);
            let gave_up = g.p.gave_up();
            drop(g);
            if gave_up && !GAVE_UP_COUNTED.swap(true, Ordering::Relaxed) {
                RM_GAVE_UP.fetch_add(1, Ordering::Relaxed);
                crate::diag::record_named_bytes(b"RmGaveUp", RM_GAVE_UP.load(Ordering::Relaxed));
            }
            RM_PRES.store(g_word, Ordering::Relaxed);
            act
        };
        match act {
            Act::Idle => return,
            Act::WaitUntil(t) => {
                WAKE_AT.store(t, Ordering::Release);
                return;
            }
            Act::Register => {
                if !register(adapter, epoch) {
                    // Refused: the presenter pauses before the next attempt and the
                    // worker is woken for it (nothing else would, on an idle desktop).
                    WAKE_AT.store(
                        now_100ns().saturating_add(RETRY_AFTER_100NS),
                        Ordering::Release,
                    );
                    return;
                }
            }
            Act::Withdraw => {
                withdraw(adapter);
                return;
            }
            Act::CopyFlip { slot } => {
                let result = match source {
                    Ok((src, resid)) => copy_flip(passive, adapter, epoch, want, slot, &src, resid),
                    // The source went away between the verdict above and now.
                    Err(_) => FlipResult::Yielded,
                };
                finish_flip(epoch, slot, true, result);
                if result != FlipResult::Shown {
                    // The frame is still owed and nothing else will wake the worker
                    // on an idle desktop: ask for a retry.
                    WAKE_AT.store(
                        now_100ns().saturating_add(RETRY_AFTER_100NS),
                        Ordering::Release,
                    );
                    return;
                }
            }
            Act::Reflip { slot } => {
                let result = reflip(passive, adapter, epoch, want, slot);
                finish_flip(epoch, slot, false, result);
                if result != FlipResult::Shown {
                    WAKE_AT.store(
                        now_100ns().saturating_add(RETRY_AFTER_100NS),
                        Ordering::Release,
                    );
                }
                return;
            }
        }
    }
}

/// The presenter's word for `RmPres`.
fn word(p: &Presenter) -> u32 {
    u32::from(p.registered())
        | (u32::from(p.gave_up()) << 1)
        | (u32::from(p.fails()) << 8)
        | (p.front().map_or(0, |f| u32::from(f) + 1) << 16)
}

/// Register the resident source with the arbiter. `false` when it was refused.
#[inline(never)]
fn register(adapter: &AdapterContext, epoch: u64) -> bool {
    RM_PSTAGE.store(1, Ordering::Relaxed);
    // A registration behind a user source is not a failure: it waits, parked.
    let ok = match rm_client::ring_identity(epoch) {
        Some((drm, layout)) => adapter
            .foreign_scanout_resident_set(KMD, drm, epoch, flip_layout(&layout))
            .is_ok(),
        None => false,
    };
    if ok {
        RM_REGS.fetch_add(1, Ordering::Relaxed);
    }
    {
        let mut g = PRESENTER.lock();
        if g.epoch == epoch {
            g.p.registration(ok, now_100ns());
        }
    }
    crate::diag::record_named_bytes(b"RmReg", u32::from(ok));
    publish_counters_throttled();
    ok
}

/// Withdraw the resident source: the arbiter asks for one Venus desktop flush.
#[inline(never)]
fn withdraw(adapter: &AdapterContext) {
    RM_PSTAGE.store(2, Ordering::Relaxed);
    let _ = adapter.foreign_scanout_resident_drop();
    RM_WITHDRAWN.fetch_add(1, Ordering::Relaxed);
    FRAME_EDGE.store(0, Ordering::Release);
    publish_counters_throttled();
}

/// Report a flip to the presenter and count it.
fn finish_flip(epoch: u64, slot: u8, copied: bool, result: FlipResult) {
    let now = now_100ns();
    let (first, word_now) = {
        let mut g = PRESENTER.lock();
        let first = g.p.front().is_none();
        if g.epoch == epoch {
            g.p.flipped(slot, copied, result, now);
        }
        let w = word(&g.p);
        (first, w)
    };
    RM_PRES.store(word_now, Ordering::Relaxed);
    match result {
        FlipResult::Shown => {
            YIELDS.store(0, Ordering::Relaxed);
            if copied {
                RM_FRAMES.fetch_add(1, Ordering::Relaxed);
            } else {
                RM_REFLIPS.fetch_add(1, Ordering::Relaxed);
            }
            let tick = MIRROR_TICK.fetch_add(1, Ordering::Relaxed);
            if first || tick % MIRROR_EVERY_FRAMES == MIRROR_EVERY_FRAMES - 1 {
                publish_counters();
            }
        }
        FlipResult::Yielded => {
            RM_YIELDED.fetch_add(1, Ordering::Relaxed);
            // A resident source that keeps finding scanout taken with no user source
            // behind it is a registration that does not work: count it as a failure
            // after a few, so it cannot spin the worker.
            if YIELDS.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_YIELDS {
                YIELDS.store(0, Ordering::Relaxed);
                let mut g = PRESENTER.lock();
                if g.epoch == epoch {
                    g.p.flipped(slot, copied, FlipResult::Failed, now);
                }
            }
        }
        FlipResult::Failed => {
            RM_FLIP_FAIL.fetch_add(1, Ordering::Relaxed);
            publish_counters();
        }
        FlipResult::SourceFailed => {
            RM_SRC_BAD.fetch_add(1, Ordering::Relaxed);
            publish_counters();
        }
    }
}

/// How a `present` ended, for the presenter.
fn judge(r: Result<u64, PresentRefusal>) -> FlipResult {
    match r {
        Ok(_) => FlipResult::Shown,
        // Another source holds scanout 0, or ours lapsed with the generation: not ours
        // to show now.
        Err(PresentRefusal::NoSource) => FlipResult::Yielded,
        Err(
            PresentRefusal::NoTransport
            | PresentRefusal::NotOwned
            | PresentRefusal::Forbidden
            | PresentRefusal::Device(_)
            // Never answered to an unfenced, unqueued `present_within`; named so a new
            // refusal is a compile error here and not a silent `Failed`.
            | PresentRefusal::Unsupported
            | PresentRefusal::NotFence
            | PresentRefusal::AlreadyAttached
            | PresentRefusal::QueueFull,
        ) => FlipResult::Failed,
    }
}

/// Copy the primary into ring surface `slot` and flip it.
///
/// The source is mapped FIRST (host round trips, up to seconds); the view lease is
/// taken only for the row loop, so the retire path never waits on, or times out
/// against, a host call.
#[inline(never)]
fn copy_flip(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
    want: Want,
    slot: u8,
    src: &SourceLayout,
    resid: u32,
) -> FlipResult {
    RM_PSTAGE.store(3, Ordering::Relaxed);
    // A blob that is already gone has nothing to read.
    if !adapter
        .with_virtio(|v| v.resource_is_live(resid))
        .unwrap_or(false)
    {
        return FlipResult::SourceFailed;
    }
    // StopDevice is joining the worker: no host round trip is started (a source that is
    // not read is not a failure: the generation is ending).
    if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
        return FlipResult::Yielded;
    }
    let budget = SweepBudget::new(
        now_100ns(),
        MAP_TIMEOUT_MS.saturating_mul(UNITS_PER_MS),
        MAP_TIMEOUT_MS,
    );
    let Ok(prep) = crate::virtio::ctrl::map_blob_prepare_within(
        passive,
        adapter,
        OwnerFilter::Any,
        resid,
        Some(&budget),
    ) else {
        return FlipResult::SourceFailed;
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = prep.gpa as i64;
    // The cache attribute MUST be the host's (`MAP_INFO`): an alias with another one
    // is architecturally invalid, and reading a WC view is slow but correct.
    let cache = crate::ddi::map_cache_to_mm(prep.map_cache);
    // SAFETY: PASSIVE (the HPD worker); the range was RESOURCE_MAP_BLOB'd into the
    // host-visible window by `map_blob_prepare`, so the pages are backed. Unmapped
    // below, on every path.
    let src_va = unsafe { MmMapIoSpace(pa, prep.size, cache) } as *const u8;
    if src_va.is_null() {
        return FlipResult::SourceFailed;
    }
    // Nothing below may start a host round trip until the lease ends.
    let copied = copy_leased(adapter, epoch, want, slot, src, src_va, prep.size);
    // SAFETY: the mapping made above, exact size.
    unsafe { MmUnmapIoSpace(src_va as *mut core::ffi::c_void, prep.size) };
    let lease = match copied {
        Ok(lease) => lease,
        Err(r) => return r,
    };
    RM_PSTAGE.store(5, Ordering::Relaxed);
    judge(present_within(
        passive,
        adapter,
        KMD,
        lease.drm,
        lease.gem,
        FLIP_TIMEOUT_MS,
    ))
}

/// With the source mapped: lease the slot, copy the rows, end the lease. No host call
/// and nothing that can block between the lease and its end.
#[inline(never)]
fn copy_leased(
    adapter: &AdapterContext,
    epoch: u64,
    want: Want,
    slot: u8,
    src: &SourceLayout,
    src_va: *const u8,
    src_len: u64,
) -> Result<rm_client::Lease, FlipResult> {
    // The worker is being stopped: the retire path is on its way and must not find a
    // lease it has to wait for.
    if adapter.hpd_stop.load(Ordering::Acquire) != 0 {
        return Err(FlipResult::Yielded);
    }
    let Some(lease) = lease_slot(epoch, want, usize::from(slot)) else {
        // The ring went away under the pass (a retire, a mode change): not a failure.
        return Err(FlipResult::Yielded);
    };
    let (dst_va, dst_len) = lease.view;
    let plan = if dst_va == 0 {
        None
    } else {
        CopyPlan::new(src, src_len, lease.layout.pitch, dst_len, 0, src.height)
    };
    let Some(plan) = plan else {
        end_lease();
        return Err(FlipResult::SourceFailed);
    };
    RM_PSTAGE.store(4, Ordering::Relaxed);
    let started = now_100ns();
    // SAFETY: `plan` proved every row of both mappings in range (`src_len` = the blob
    // mapping the caller holds, `dst_len` = the leased view); the view stays mapped
    // while the lease is held (the retire path waits for it), the source until the
    // caller unmaps it, after this returns.
    unsafe { copy_frame(&plan, src_va, dst_va as *mut u8) };
    end_lease();
    let ms = (now_100ns().wrapping_sub(started) / 10_000).min(u64::from(u32::MAX)) as u32;
    RM_COPY_MS.store(ms, Ordering::Relaxed);
    RM_COPY_MAX_MS.fetch_max(ms, Ordering::Relaxed);
    let mb_before = COPIED_BYTES.fetch_add(plan.bytes(), Ordering::Relaxed);
    RM_COPY_MB.store(((mb_before + plan.bytes()) >> 20) as u32, Ordering::Relaxed);
    Ok(lease)
}

/// Flip ring surface `slot` again, with no copy.
#[inline(never)]
fn reflip(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
    want: Want,
    slot: u8,
) -> FlipResult {
    // The lease is only for the identity here; nothing is written, so it ends at once.
    let Some(lease) = lease_slot(epoch, want, usize::from(slot)) else {
        return FlipResult::Yielded;
    };
    end_lease();
    RM_PSTAGE.store(6, Ordering::Relaxed);
    judge(present_within(
        passive,
        adapter,
        KMD,
        lease.drm,
        lease.gem,
        FLIP_TIMEOUT_MS,
    ))
}

/// Bytes copied since boot, for `RmCopyMB`.
static COPIED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Copy `plan`'s rows from `src` to the write-combined `dst`, with non-temporal
/// stores where the destination row is 16-byte aligned (it is: the surface pitch is a
/// multiple of 256 and the view starts on a page), then drain the write-combining
/// buffers so the host reads the whole frame.
///
/// # Safety
/// `src` and `dst` must be mapped for every row `plan` names (the plan proved the
/// bounds against their lengths).
unsafe fn copy_frame(plan: &CopyPlan, src: *const u8, dst: *mut u8) {
    for y in plan.y0..plan.y1 {
        let Some((so, d)) = plan.row(y) else {
            return;
        };
        // SAFETY: in range per the contract.
        unsafe {
            copy_row(
                src.add(so as usize),
                dst.add(d as usize),
                plan.row_bytes as usize,
            )
        };
    }
    // SAFETY: SSE2 is baseline on x86_64.
    unsafe { core::arch::x86_64::_mm_sfence() };
}

/// One row: 64 bytes at a time through the streaming path when `dst` is 16-aligned,
/// the tail (and an unaligned row) with plain copies.
///
/// # Safety
/// `n` bytes readable at `src` and writable at `dst`, not overlapping.
unsafe fn copy_row(src: *const u8, dst: *mut u8, n: usize) {
    use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_stream_si128};
    let mut i = 0usize;
    if (dst as usize) & 15 == 0 {
        while i + 64 <= n {
            // SAFETY: `i + 64 <= n`; unaligned loads, aligned streaming stores.
            unsafe {
                let a = _mm_loadu_si128(src.add(i) as *const __m128i);
                let b = _mm_loadu_si128(src.add(i + 16) as *const __m128i);
                let c = _mm_loadu_si128(src.add(i + 32) as *const __m128i);
                let d = _mm_loadu_si128(src.add(i + 48) as *const __m128i);
                _mm_stream_si128(dst.add(i) as *mut __m128i, a);
                _mm_stream_si128(dst.add(i + 16) as *mut __m128i, b);
                _mm_stream_si128(dst.add(i + 32) as *mut __m128i, c);
                _mm_stream_si128(dst.add(i + 48) as *mut __m128i, d);
            }
            i += 64;
        }
        while i + 16 <= n {
            // SAFETY: `i + 16 <= n`.
            unsafe {
                let a = _mm_loadu_si128(src.add(i) as *const __m128i);
                _mm_stream_si128(dst.add(i) as *mut __m128i, a);
            }
            i += 16;
        }
    }
    if i < n {
        // SAFETY: the remaining `n - i` bytes, in range.
        unsafe { core::ptr::copy_nonoverlapping(src.add(i), dst.add(i), n - i) };
    }
}
