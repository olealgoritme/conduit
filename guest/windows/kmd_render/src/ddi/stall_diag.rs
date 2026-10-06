//! Stall diagnosis and the opt-in flip watchdog: the I/O half. The decisions are
//! `helios_kmd_logic::stall_diag` (host-tested); this file keeps the atomics, reads the two knobs
//! and writes the registry. Design, the counters to dump during a stall and the decision table:
//! `docs/zero-copy-present.md`, "Stall diagnosis".
//!
//! WHAT IT IS. An observed desktop stall (the Venus DWM stopped presenting after a user NVK
//! scan-out app exited; no TDR, no crash) could not be named from the counters. Every mirror was
//! event gated, and the `Vp*` dump runs from the HPD worker, so a stuck worker shows a stale dump.
//! This module leaves breadcrumbs that survive a stuck worker and publishes them from places
//! that do not depend on the worker, and adds two default-off safety valves.
//!
//! IRQL. Every `note_*` function, [`on_vsync_tick`] and [`defer_note`]'s state are atomics only
//! and legal at any IRQL (the vsync DPC at DISPATCH, `SetVidPnSourceAddress` at DIRQL, the DMA
//! lane at DISPATCH). The registry is written only by [`publish_counters`] and the knob
//! mirrors, at PASSIVE: from the escape thread ([`publish_from_escape`], only while the worker looks stuck; it does not depend on
//! the worker), from the HPD worker's periodic mirrors, and at StartDevice.
//!
//! Defaults. `FlipWdogMs` 0 and `DeferBudget` 0 are today's behaviour: the watchdog never
//! publishes, the Deferred loop never gives up. With them at their defaults the only changes are
//! the new counters and a handful of relaxed atomic stores (two clock reads per scanout
//! lifecycle operation, one per HPD worker step, one per published flip).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use helios_kmd_logic::stall_diag::{self as sd, DeferDecision, PendInput, PendState, WdAction};

use crate::adapter::{gate_active, AdapterContext};

pub(crate) use helios_kmd_logic::stall_diag::site;

// ---- HPD worker breadcrumbs ----------------------------------------------------------------

/// Worker loops (wakes) this generation, and the interrupt time (ms) of the last wake.
static HPD_LOOP_N: AtomicU32 = AtomicU32::new(0);
static HPD_LOOP_T: AtomicU32 = AtomicU32::new(0);
/// The step the worker is in or last entered (`stall_diag::site`) and when it entered it.
static HPD_SITE: AtomicU32 = AtomicU32::new(0);
static HPD_SITE_T: AtomicU32 = AtomicU32::new(0);

/// The worker entered `step`: one store of the id and one of the clock. HPD worker only (its
/// stores are not read-modify-write: one writer). Any IRQL, in practice PASSIVE.
pub(crate) fn hpd_enter(step: u32) {
    HPD_SITE.store(step, Ordering::Relaxed);
    HPD_SITE_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
}

/// The worker woke for another pass of its loop.
pub(crate) fn hpd_loop() {
    HPD_LOOP_N.store(
        HPD_LOOP_N.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
    HPD_LOOP_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
}

// ---- the scanout mutex ---------------------------------------------------------------------

/// Acquisitions (`ScLkN`) and releases (`ScLkRelN`) of the scanout mutex
/// (`with_scanout_lifecycle`), and the interrupt time (ms) of the last acquisition and of the
/// last release. The mutex is HELD NOW when the counts differ (`stall_diag::lock_held`: counts,
/// not the stamps, which cannot order two events in the same millisecond); its age is
/// `StallT - ScLkAcqT`. The one lock the worker, the DDI threads and `DestroyAllocation` queue
/// on, and a holder can sit in a host round trip for up to 30 s
/// (`retire_scanout_allocation_locked`).
static LOCK_N: AtomicU32 = AtomicU32::new(0);
static LOCK_REL_N: AtomicU32 = AtomicU32::new(0);
static LOCK_ACQ_T: AtomicU32 = AtomicU32::new(0);
static LOCK_REL_T: AtomicU32 = AtomicU32::new(0);

/// The scanout mutex was just acquired. PASSIVE. The stamp first, the count second (Release): a
/// reader that sees the count also sees this acquisition's time, never the previous one's.
pub(crate) fn note_lock_acquired() {
    LOCK_ACQ_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    LOCK_N.fetch_add(1, Ordering::Release);
}

/// The scanout mutex is about to be released. PASSIVE. Stamp, then count.
pub(crate) fn note_lock_released() {
    LOCK_REL_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    LOCK_REL_N.fetch_add(1, Ordering::Release);
}

// ---- flips issued and published ------------------------------------------------------------

/// Flips dxgkrnl issued (each `SetVidPnSourceAddress` with an argument, each DMA flip record the
/// submit took), publications of a displayed address (`publish_displayed_primary`: bound or
/// kept, any class, and the ring-1 completion DPC's own stores), and the time of the last
/// publication. Coalescing (`VpCoal`: dxgkrnl flipping faster than the worker drains) makes
/// `FlipIss - FlipPub` larger by the dropped handles, so a healthy run has `FlipIss - FlipPub
/// - VpCoal` near 0 at quiescence.
static FLIP_ISS: AtomicU32 = AtomicU32::new(0);
static FLIP_PUB: AtomicU32 = AtomicU32::new(0);
static FLIP_PUB_T: AtomicU32 = AtomicU32::new(0);

/// A flip was issued by dxgkrnl. Atomics only, any IRQL.
pub(crate) fn note_flip_issued() {
    FLIP_ISS.fetch_add(1, Ordering::Relaxed);
}

/// An address was published as the displayed one. Atomics only, any IRQL.
pub(crate) fn note_published() {
    FLIP_PUB.fetch_add(1, Ordering::Relaxed);
    FLIP_PUB_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
}

// ---- the vsync tick and the watchdog -------------------------------------------------------

/// The newest pending flip as one packed word (`stall_diag::pack_flip`), the word the watchdog
/// already published, and the flip numbering.
static FLIP_WORD: AtomicU64 = AtomicU64::new(0);
static FIRED_WORD: AtomicU64 = AtomicU64::new(0);
static FLIP_SEQ: AtomicU32 = AtomicU32::new(0);
/// The vsync DPC's state between ticks (`stall_diag::PendState`): the consecutive-pending run
/// (`VsPendN`), its maximum (`VsPendMax`), the no-publication clock and the `FlipPub` last seen.
static VS_PEND: AtomicU32 = AtomicU32::new(0);
static VS_PEND_MAX: AtomicU32 = AtomicU32::new(0);
static VS_STALL: AtomicU32 = AtomicU32::new(0);
static VS_SEEN_PUB: AtomicU32 = AtomicU32::new(0);
/// Watchdog publications (`FlipWd`), the time of the last (`FlipWdT`), and flips it could not
/// record (`FlipWdBig`).
static WD_COUNT: AtomicU32 = AtomicU32::new(0);
static WD_T: AtomicU32 = AtomicU32::new(0);
static WD_BIG: AtomicU32 = AtomicU32::new(0);

/// `FlipWdogMs` in force (clamped; 0 = off) and `DeferBudget` in force (clamped; 0 = unlimited).
static WDOG_MS: AtomicU32 = AtomicU32::new(0);
static DEFER_BUDGET: AtomicU32 = AtomicU32::new(0);

/// Record the newest pending flip's address for the watchdog: called where a flip's programming
/// gate is raised (`set_vidpn_source_address_dirql`, `arm_dma_flip_programming`), after the
/// handle paired. One packed store, no read-modify-write of shared state beyond the sequence:
/// legal at DIRQL. An address the word cannot carry (zero, or 40 bits or more) records nothing,
/// and clears the older word so the watchdog cannot publish ANOTHER flip's address for this one
/// (counted `FlipWdBig`).
pub(crate) fn note_flip_pending(address: u64) {
    let seq = FLIP_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    match sd::pack_flip(seq, address) {
        Some(word) => FLIP_WORD.store(word, Ordering::Release),
        None => {
            FLIP_WORD.store(0, Ordering::Release);
            if address != 0 {
                WD_BIG.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// One vsync tick of bookkeeping (`VsPendN`, its maximum) and, with `FlipWdogMs` set, the
/// watchdog. Called from the vsync DPC (`adapter/kobj.rs`, DISPATCH, atomics only). The tick is
/// serialized (one-shot, re-armed by its own DPC), so the state is plain loads and stores.
///
/// The watchdog publishes the newest recorded flip's address as a kept picture
/// (`AdapterContext::publish_kept_primary`, one atomic store, legal at DISPATCH) once the
/// pending run has gone `FlipWdogMs` worth of ticks without any publication, for ANY class of
/// allocation including Venus, and never twice for the same flip. The kept address names a
/// picture that is not on the screen: this is recovery, not completion, and only the opt-in knob
/// allows it. It does not lower the programming gate or touch the pending slot: the worker
/// still owns the programming and will bind or reject it as before.
pub(crate) fn on_vsync_tick(adapter: &AdapterContext, period_100ns: u64) {
    let pending = adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0
        || gate_active(adapter.vidpn_programming.load(Ordering::Acquire));
    let wdog_ms = WDOG_MS.load(Ordering::Relaxed);
    let flip_word = FLIP_WORD.load(Ordering::Acquire);
    let (next, action) = sd::pend_step(
        PendState {
            pend: VS_PEND.load(Ordering::Relaxed),
            max: VS_PEND_MAX.load(Ordering::Relaxed),
            stall: VS_STALL.load(Ordering::Relaxed),
            seen_pub: VS_SEEN_PUB.load(Ordering::Relaxed),
        },
        PendInput {
            pending,
            pub_count: FLIP_PUB.load(Ordering::Relaxed),
            flip_word,
            fired_word: FIRED_WORD.load(Ordering::Relaxed),
            limit_ticks: if wdog_ms == 0 {
                0
            } else {
                sd::ticks_for_ms(wdog_ms, period_100ns)
            },
        },
    );
    VS_PEND.store(next.pend, Ordering::Relaxed);
    VS_PEND_MAX.store(next.max, Ordering::Relaxed);
    VS_STALL.store(next.stall, Ordering::Relaxed);
    VS_SEEN_PUB.store(next.seen_pub, Ordering::Relaxed);
    if let WdAction::Publish(address) = action {
        FIRED_WORD.store(flip_word, Ordering::Relaxed);
        adapter.publish_kept_primary(address);
        WD_COUNT.fetch_add(1, Ordering::Relaxed);
        WD_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    }
}

/// Whether the watchdog knob is on (`FlipWdogMs` != 0): the direct Venus exits
/// (`display.rs`, `FkVenus`) publish kept only then.
pub(crate) fn watchdog_on() -> bool {
    WDOG_MS.load(Ordering::Relaxed) != 0
}

// ---- the Deferred retry budget -------------------------------------------------------------

/// Bookkeeping for the Deferred programming loop. Touched only by the HPD worker, under the
/// scanout mutex (like `RETRY_HANDLE`).
static DEFER_HANDLE: AtomicUsize = AtomicUsize::new(0);
static DEFER_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Charge one Deferred attempt against `handle`'s budget (`DeferBudget`; a different handle
/// starts a fresh one). With the knob at 0 (the default) it touches nothing and answers
/// `Again`: today's behaviour. On `Exhausted` the state is forgotten.
pub(crate) fn defer_note(handle: usize) -> DeferDecision {
    let budget = DEFER_BUDGET.load(Ordering::Relaxed);
    if budget == 0 {
        return DeferDecision::Again;
    }
    let attempts = sd::defer_attempts(
        DEFER_HANDLE.swap(handle, Ordering::Relaxed),
        DEFER_ATTEMPTS.load(Ordering::Relaxed),
        handle,
    );
    let decision = sd::defer_decide(attempts, budget);
    match decision {
        DeferDecision::Again => DEFER_ATTEMPTS.store(attempts, Ordering::Relaxed),
        DeferDecision::Exhausted => clear_defer_state(),
    }
    decision
}

/// Forget any Deferred attempts in progress: the primary programmed or failed for good.
pub(crate) fn clear_defer_state() {
    DEFER_HANDLE.store(0, Ordering::Relaxed);
    DEFER_ATTEMPTS.store(0, Ordering::Relaxed);
}

// ---- generations, knobs, the registry ------------------------------------------------------

/// StartDevice generations since this image was loaded (`StartN`), and the interrupt time of the
/// last one's start (`StartT`).
static START_N: AtomicU32 = AtomicU32::new(0);
static START_T: AtomicU32 = AtomicU32::new(0);

/// Read the two knobs again and mirror the values in force (0 included). StartDevice, PASSIVE:
/// they are cached in statics that outlive a `pnputil /restart-device`.
pub(crate) fn reread_knobs() {
    let wdog = sd::clamp_wdog_ms(crate::diag::read_config_dword(
        crate::diag::knobs::FLIP_WDOG_MS,
        0,
    ));
    let budget = sd::clamp_defer_budget(crate::diag::read_config_dword(
        crate::diag::knobs::DEFER_BUDGET,
        0,
    ));
    WDOG_MS.store(wdog, Ordering::Relaxed);
    DEFER_BUDGET.store(budget, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"FlWdMsEff", wdog);
    crate::diag::record_named_bytes(b"DefBudEff", budget);
}

/// A new generation (StartDevice): zero every counter of this module (never the knobs, and not
/// `StartN`, which is the generation count), bump `StartN`, stamp `StartT`, and write the block
/// once (zeros included) so values an earlier run left in the service key are never read as this
/// one's. PASSIVE.
pub(crate) fn start_generation() {
    for c in [
        &HPD_LOOP_N,
        &HPD_LOOP_T,
        &HPD_SITE,
        &HPD_SITE_T,
        &LOCK_N,
        &LOCK_REL_N,
        &LOCK_ACQ_T,
        &LOCK_REL_T,
        &FLIP_ISS,
        &FLIP_PUB,
        &FLIP_PUB_T,
        &FLIP_SEQ,
        &VS_PEND,
        &VS_PEND_MAX,
        &VS_STALL,
        &VS_SEEN_PUB,
        &WD_COUNT,
        &WD_T,
        &WD_BIG,
        &DEFER_ATTEMPTS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    FLIP_WORD.store(0, Ordering::Release);
    FIRED_WORD.store(0, Ordering::Relaxed);
    DEFER_HANDLE.store(0, Ordering::Relaxed);
    START_N.fetch_add(1, Ordering::Relaxed);
    START_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    publish_counters();
}

/// Mirror the counters to the service key. PASSIVE_LEVEL only; a few dozen microseconds of
/// registry writes, so callers rate-limit it (`publish_from_escape`) or run it from a path that
/// is already a mirror.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"StallT", AdapterContext::interrupt_time_ms());
    rec(b"HpdLoopN", HPD_LOOP_N.load(Ordering::Relaxed));
    rec(b"HpdLoopT", HPD_LOOP_T.load(Ordering::Relaxed));
    rec(b"HpdSite", HPD_SITE.load(Ordering::Relaxed));
    rec(b"HpdSiteT", HPD_SITE_T.load(Ordering::Relaxed));
    rec(b"ScLkN", LOCK_N.load(Ordering::Relaxed));
    rec(b"ScLkRelN", LOCK_REL_N.load(Ordering::Relaxed));
    rec(b"ScLkAcqT", LOCK_ACQ_T.load(Ordering::Relaxed));
    rec(b"ScLkRelT", LOCK_REL_T.load(Ordering::Relaxed));
    rec(b"FlipIss", FLIP_ISS.load(Ordering::Relaxed));
    rec(b"FlipPub", FLIP_PUB.load(Ordering::Relaxed));
    rec(b"FlipPubT", FLIP_PUB_T.load(Ordering::Relaxed));
    rec(b"VsPendN", VS_PEND.load(Ordering::Relaxed));
    rec(b"VsPendMax", VS_PEND_MAX.load(Ordering::Relaxed));
    rec(b"StartN", START_N.load(Ordering::Relaxed));
    rec(b"StartT", START_T.load(Ordering::Relaxed));
    rec(b"FlipWd", WD_COUNT.load(Ordering::Relaxed));
    rec(b"FlipWdT", WD_T.load(Ordering::Relaxed));
    rec(b"FlipWdBig", WD_BIG.load(Ordering::Relaxed));
}

/// How often the escape thread may write the block while the worker looks stuck: twice a second.
const ESCAPE_PUBLISH_MS: u32 = 500;
/// Interrupt time (ms, never 0) of the last publication from an escape.
static LAST_ESCAPE_PUBLISH: AtomicU32 = AtomicU32::new(0);

/// Whether the HPD worker looks stuck right now (`stall_diag::worker_looks_stuck`): a few
/// relaxed and acquire loads, no registry, no lock.
fn worker_looks_stuck(adapter: &AdapterContext, now: u32) -> bool {
    // Release count first, acquire count second: a mutex acquired in between reads as held, with
    // the stamp of that acquisition (stamp then count, `note_lock_acquired`).
    let released = LOCK_REL_N.load(Ordering::Acquire);
    let acquired = LOCK_N.load(Ordering::Acquire);
    sd::worker_looks_stuck(sd::StuckInput {
        now,
        site: HPD_SITE.load(Ordering::Relaxed),
        site_t: HPD_SITE_T.load(Ordering::Relaxed),
        lock_held: sd::lock_held(acquired, released),
        lock_acq_t: LOCK_ACQ_T.load(Ordering::Relaxed),
        loop_t: HPD_LOOP_T.load(Ordering::Relaxed),
        work_pending: adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0
            || gate_active(adapter.vidpn_programming.load(Ordering::Acquire)),
    })
}

/// Publish the block from the escape thread, but ONLY while the HPD worker looks stuck
/// (`stall_diag::worker_looks_stuck`: in a step other than the idle wait for more than a second,
/// or the scanout mutex held for more than a second, or work pending and no wake for two), and at
/// most every [`ESCAPE_PUBLISH_MS`]. An escape is called by user mode at PASSIVE on ITS OWN
/// thread, so this refreshes the counters even when the worker is stuck (the other mirrors run on
/// the worker, including the `Nv*` mirror an escape asks for). While the worker is healthy this
/// does NOTHING beyond one clock read, one load and the loads of the stuck test: the block is
/// about twenty registry writes (each opens the key by path, about half a millisecond in all)
/// and the worker's own mirrors keep publishing it as before.
pub(crate) fn publish_from_escape(adapter: &AdapterContext) {
    let now = AdapterContext::interrupt_time_ms().max(1);
    let last = LAST_ESCAPE_PUBLISH.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < ESCAPE_PUBLISH_MS {
        return;
    }
    if !worker_looks_stuck(adapter, now) {
        return;
    }
    // One thread publishes per interval: the others see the new stamp.
    if LAST_ESCAPE_PUBLISH
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    publish_counters();
}
