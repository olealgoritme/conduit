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
//! THE RELEASE SEAM. A flip's `seq` is recorded ([`LAST_SEQ`] with [`LAST_GEM`]). When the
//! viewer's release event (`ScanoutReleased`, the other agent's `kmd/scanout-release`)
//! arrives it names a `seq`: the places that will want it are marked `RELEASE SEAM` below
//! and in `sysmem::released` (a GEM must not be closed while the viewer still samples
//! it: today the host's own dma-buf reference makes closing safe, and nothing waits).
//!
//! LOCKING. `TARGET` and `PRES` are leaf spinlocks over plain data, never held across I/O
//! or another lock and never held together. Everything that sends runs with no lock.

use super::sysmem::live_gem;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::foreign_scanout::{present_within, PresentRefusal};
use crate::virtio::gpu::DeviceOwner;
use crate::virtio::rm_present;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::rm_present::{Act, FlipResult, Inputs, Presenter};
use helios_kmd_logic::rm_sysmem::{self as rs, Change, Target, TargetBook};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Acts performed per worker pass (a registration is followed by the first flip).
const ACTS_PER_PASS: usize = 3;
/// How long the host gets to take one flip (the worker flips every frame and StopDevice
/// joins it for a bounded time, as `rm_present`).
const FLIP_TIMEOUT_MS: u64 = 1_000;
/// How long after the presenter gave up (three failed attempts in a row) it starts over: 5 s.
const RESTART_AFTER_GIVING_UP_100NS: u64 = 50_000_000;
/// Consecutive flips that found the source yielded before it counts as a failure.
const MAX_YIELDS: u32 = 8;

struct PState {
    epoch: u64,
    p: Presenter,
}

static PRES: SpinLock<PState> = SpinLock::new(PState {
    epoch: 0,
    p: Presenter::new(1),
});
static TARGET: SpinLock<TargetBook> = SpinLock::new(TargetBook::new());
static YIELDS: AtomicU32 = AtomicU32::new(0);
/// The `seq` of the last flip the host took and the GEM it named (the RELEASE SEAM).
pub static LAST_SEQ: AtomicU64 = AtomicU64::new(0);
pub static LAST_GEM: AtomicU32 = AtomicU32::new(0);

// Counters (names at most 14 characters): `RmSysProg` primaries programmed (each is a
// `SetVidPnSourceAddress` of an RM primary), `RmSysProgBad` refused (a layout that is not
// the mode's), `RmSysRegs` registrations the arbiter took, `RmSysWithdrawn` withdrawals,
// `RmSysGaveUp` times the presenter gave up and started over five seconds later, `RmSysFrames` flips
// shown for an edge, `RmSysReflips` flips shown for a resume, `RmSysYielded` flips that
// found the source yielded, `RmSysFlipFail` flips the host or transport refused,
// `RmSysPres` the presenter's word (bit 0 registered, bit 1 gave up, bits 8.. failures),
// `RmSysSeq` the last flip's `seq`.
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
    }
    YIELDS.store(0, Ordering::Relaxed);
    LAST_SEQ.store(0, Ordering::Relaxed);
    LAST_GEM.store(0, Ordering::Relaxed);
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
    // The first flip of a (new) primary is owed.
    rm_present::note_frame_edge(adapter);
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
        adapter.signal_hpd();
    }
}

/// The allocation `resource_id` is being destroyed (`sysmem::released`): if it is the shown
/// one, forget it; the worker withdraws the resident source in its next pass, and no flip
/// names the GEM that is about to be closed (the target is gone first).
pub(super) fn target_gone(adapter: &AdapterContext, resource_id: u32) {
    if TARGET.lock().gone(resource_id) {
        adapter.signal_hpd();
    }
}

// ---- the worker -----------------------------------------------------------------------

/// One pass of the flip service, from `rm_client::service` at level 5 (PASSIVE, the HPD
/// worker). With nothing shown and nothing registered it is two lock holds.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return;
    };
    if epoch == 0 {
        return;
    }
    rm_present::clear_wake_at();
    let (mut frame_edge, mut resume_edge) = rm_present::take_edges();
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
        let inputs = Inputs {
            now: t,
            ring_ready: ready,
            source_ok: ready,
            arbiter_has_resident: has_resident,
            foreground,
            frame_edge,
            resume_edge,
        };
        frame_edge = false;
        resume_edge = false;
        let (act, word, gave_up) = {
            let mut g = PRES.lock();
            if g.epoch != epoch {
                g.p.reset();
                g.epoch = epoch;
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
            PRES.lock().p.reset();
            SYS_GAVE_UP.fetch_add(1, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"RmSysGaveUp", SYS_GAVE_UP.load(Ordering::Relaxed));
            rm_present::set_wake_at(now().saturating_add(RESTART_AFTER_GIVING_UP_100NS));
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
    // RELEASE SEAM: when the viewer's release event exists, the previous GEM's release is
    // what a flip to ANOTHER primary would wait for; a re-flip of the same one never waits
    // (the viewer holds it for as long as it is shown).
    match present_within(passive, adapter, KMD, t.drm, t.gem, FLIP_TIMEOUT_MS) {
        Ok(seq) => {
            LAST_SEQ.store(seq, Ordering::Relaxed);
            LAST_GEM.store(t.gem, Ordering::Relaxed);
            FlipResult::Shown
        }
        Err(PresentRefusal::NoSource) => FlipResult::Yielded,
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
