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

use helios_kmd_logic::hpd_wake;
use helios_kmd_logic::stall_diag::{
    self as sd, DeferDecision, DeferState, PendInput, PendState, WdAction,
};

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

/// The worker woke for another pass of its loop; `timed_out` is whether its wait ended by the
/// timeout (else an event). Counts the wake (`HpdWkEvt` / `HpdWkTmo`), takes the causes signalled
/// since the previous loop (`HpdWkSrc`) and starts the pass clock ([`hpd_pass_end`]). HPD worker
/// only (one writer for the loop counters).
pub(crate) fn hpd_loop(timed_out: bool) {
    HPD_LOOP_N.store(
        HPD_LOOP_N.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
    HPD_LOOP_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    if timed_out {
        WK_TMO.fetch_add(1, Ordering::Relaxed);
    } else {
        WK_EVT.fetch_add(1, Ordering::Relaxed);
    }
    WK_SRC_LAST.store(WK_SRC_PENDING.swap(0, Ordering::AcqRel), Ordering::Relaxed);
    PASS_START.store(crate::adapter::foreign_scanout::now_100ns(), Ordering::Relaxed);
}

// ---- the worker's wakes (T5 anomalies, docs/kmd-rm-client.md 15.18.14) ----------------------

/// Worker wakes by an event and by a timeout; the causes signalled since the previous loop (the
/// bits of `hpd_wake::cause`, accumulating) and the same as of the last loop.
static WK_EVT: AtomicU32 = AtomicU32::new(0);
static WK_TMO: AtomicU32 = AtomicU32::new(0);
static WK_SRC_PENDING: AtomicU32 = AtomicU32::new(0);
static WK_SRC_LAST: AtomicU32 = AtomicU32::new(0);
/// Signals by cause (`HpdSg*`), and windowed-Blt wakes not signalled because one was owed.
static SIGNALS: [AtomicU32; hpd_wake::cause::COUNT] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static SG_COALESCED: AtomicU32 = AtomicU32::new(0);
/// The last timed wait and the shortest one (microseconds; 0 = infinite, `u32::MAX` = none yet in
/// the minimum), and the waits by class.
static WAIT_US: AtomicU32 = AtomicU32::new(0);
static WAIT_US_MIN: AtomicU32 = AtomicU32::new(u32::MAX);
static TM_CTL: AtomicU32 = AtomicU32::new(0);
static TM_RTY: AtomicU32 = AtomicU32::new(0);
static TM_DUE: AtomicU32 = AtomicU32::new(0);
static TM_NONE: AtomicU32 = AtomicU32::new(0);
/// Microseconds awake (all passes) and the longest pass; the start of the current pass (100 ns).
static BUSY_US: AtomicU32 = AtomicU32::new(0);
static PASS_MAX_US: AtomicU32 = AtomicU32::new(0);
static PASS_START: AtomicU64 = AtomicU64::new(0);
/// Periodic dumps run, their total time (microseconds), and the loops that reached the loop
/// cadence but not the interval.
static DUMP_N: AtomicU32 = AtomicU32::new(0);
static DUMP_US: AtomicU32 = AtomicU32::new(0);
static DUMP_SKIP: AtomicU32 = AtomicU32::new(0);

/// The worker is about to wait with `timeout` (relative 100 ns, `None` = infinite) of `class`.
pub(crate) fn hpd_wait(class: hpd_wake::WaitClass, timeout: Option<i64>) {
    let counter = match class {
        hpd_wake::WaitClass::CtrlPoll => &TM_CTL,
        hpd_wake::WaitClass::Retry => &TM_RTY,
        hpd_wake::WaitClass::Due => &TM_DUE,
        hpd_wake::WaitClass::Infinite => &TM_NONE,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    let us = hpd_wake::wait_us(timeout);
    WAIT_US.store(us, Ordering::Relaxed);
    if timeout.is_some() {
        WAIT_US_MIN.fetch_min(us, Ordering::Relaxed);
    }
}

/// The worker finished its pass: add the time since [`hpd_loop`] to the busy total and the
/// longest pass.
pub(crate) fn hpd_pass_end() {
    let start = PASS_START.load(Ordering::Relaxed);
    if start == 0 {
        return;
    }
    let now = crate::adapter::foreign_scanout::now_100ns();
    let us = (now.saturating_sub(start) / 10).min(u32::MAX as u64) as u32;
    BUSY_US.fetch_add(us, Ordering::Relaxed);
    PASS_MAX_US.fetch_max(us, Ordering::Relaxed);
}

/// A signal of `cause` was sent to the worker (`AdapterContext::signal_hpd_for`). Atomics only,
/// any IRQL.
pub(crate) fn note_signal(cause: u32) {
    let slot = (cause as usize).min(hpd_wake::cause::COUNT - 1);
    SIGNALS[slot].fetch_add(1, Ordering::Relaxed);
    WK_SRC_PENDING.fetch_or(hpd_wake::cause_bit(slot as u32), Ordering::AcqRel);
}

/// A windowed-Blt wake was not signalled: the worker already owed the same pass.
pub(crate) fn note_signal_coalesced() {
    SG_COALESCED.fetch_add(1, Ordering::Relaxed);
}

/// A periodic `Vp*` dump ran and took `us` microseconds.
pub(crate) fn note_dump(us: u32) {
    DUMP_N.fetch_add(1, Ordering::Relaxed);
    DUMP_US.fetch_add(us, Ordering::Relaxed);
}

/// A wake reached the dump's loop cadence but not its interval.
pub(crate) fn note_dump_skipped() {
    DUMP_SKIP.fetch_add(1, Ordering::Relaxed);
}

/// Mirror the worker's wake block. PASSIVE.
pub(crate) fn publish_hpd_wake() {
    use crate::diag::record_named_bytes as rec;
    use hpd_wake::cause;
    rec(b"HpdWkEvt", WK_EVT.load(Ordering::Relaxed));
    rec(b"HpdWkTmo", WK_TMO.load(Ordering::Relaxed));
    rec(b"HpdWkSrc", WK_SRC_LAST.load(Ordering::Relaxed));
    rec(b"HpdWait", WAIT_US.load(Ordering::Relaxed));
    let min = WAIT_US_MIN.load(Ordering::Relaxed);
    rec(b"HpdWaitMin", if min == u32::MAX { 0 } else { min });
    rec(b"HpdTmCtl", TM_CTL.load(Ordering::Relaxed));
    rec(b"HpdTmRty", TM_RTY.load(Ordering::Relaxed));
    rec(b"HpdTmDue", TM_DUE.load(Ordering::Relaxed));
    rec(b"HpdTmNone", TM_NONE.load(Ordering::Relaxed));
    rec(b"HpdBusyUs", BUSY_US.load(Ordering::Relaxed));
    rec(b"HpdPassMaxUs", PASS_MAX_US.load(Ordering::Relaxed));
    rec(b"HpdDumpN", DUMP_N.load(Ordering::Relaxed));
    rec(b"HpdDumpUs", DUMP_US.load(Ordering::Relaxed));
    rec(b"HpdDumpSkip", DUMP_SKIP.load(Ordering::Relaxed));
    rec(b"HpdSgBlt", SIGNALS[cause::BLT as usize].load(Ordering::Relaxed));
    rec(b"HpdSgRfr", SIGNALS[cause::REFRESH as usize].load(Ordering::Relaxed));
    rec(b"HpdSgEdg", SIGNALS[cause::EDGE as usize].load(Ordering::Relaxed));
    rec(b"HpdSgFnc", SIGNALS[cause::FENCE as usize].load(Ordering::Relaxed));
    rec(b"HpdSgFs", SIGNALS[cause::FS_SET as usize].load(Ordering::Relaxed));
    rec(b"HpdSgRel", SIGNALS[cause::RELEASE as usize].load(Ordering::Relaxed));
    rec(b"HpdSgFlp", SIGNALS[cause::FLIP as usize].load(Ordering::Relaxed));
    rec(b"HpdSgOth", SIGNALS[cause::OTHER as usize].load(Ordering::Relaxed));
    rec(b"HpdSgCoal", SG_COALESCED.load(Ordering::Relaxed));
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

/// A flip was issued by dxgkrnl, naming `address`: count it, give it its number (the new
/// `FlipIss`, 24 bits on the wire of the word) and record it as the NEWEST flip for the watchdog
/// (one packed store). Atomics only, any IRQL (`SetVidPnSourceAddress` at DIRQL, the DMA lane at
/// DISPATCH). Every issued flip is recorded, whether its programming will be pending (the gate
/// is raised) or a direct publisher completes it at once (an unpaired handle, a keep record,
/// `ForeignFlip`): the watchdog only ever publishes the newest recorded flip and only if it is
/// newer than the last one done (`note_published`), so it cannot republish an address that a
/// newer flip replaced. An address the word cannot carry (zero, or 40 bits or more) clears the
/// word instead, so an OLDER flip's address is never fired for this one (`FlipWdBig`).
pub(crate) fn note_flip_issued(address: u64) {
    let seq = FLIP_ISS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
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

/// An address was published as the displayed one: `address` (any publisher: the worker's bind,
/// a kept publication of any lane, the ring-1 completion DPC, the watchdog). Counts it and, when
/// it names the newest recorded flip, marks that flip done. Atomics only, any IRQL.
pub(crate) fn note_published(address: u64) {
    FLIP_PUB.fetch_add(1, Ordering::Relaxed);
    FLIP_PUB_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    if let Some(seq) = sd::flip_done_by(
        address,
        FLIP_WORD.load(Ordering::Acquire),
        DONE_SEQ.load(Ordering::Relaxed),
    ) {
        DONE_SEQ.store(seq, Ordering::Relaxed);
    }
}

// ---- the vsync tick and the watchdog -------------------------------------------------------

/// The newest issued flip as one packed word (`stall_diag::pack_flip`: its number and address),
/// and the number of the newest flip that is DONE (published by anyone, the watchdog included).
static FLIP_WORD: AtomicU64 = AtomicU64::new(0);
static DONE_SEQ: AtomicU32 = AtomicU32::new(0);
/// The vsync DPC's state between ticks (`stall_diag::PendState`): the consecutive-pending run
/// (`VsPendN`), its maximum (`VsPendMax`), the no-publication clock and the `FlipPub` last seen.
static VS_PEND: AtomicU32 = AtomicU32::new(0);
static VS_PEND_MAX: AtomicU32 = AtomicU32::new(0);
/// Every tick of the vsync heartbeat (`VsTickN`), whether or not dxgkrnl has the CRTC_VSYNC
/// delivery gate open, and those that ran with it closed (`VsOffN`). `VpVsN` counts only the
/// ticks that were delivered, so `VsTickN - VsOffN` is `VpVsN` (and `VsTickN` against `StallT`
/// is the timer's own rate): the pair says whether a low `VpVsN` rate is a slow timer or a gate
/// that dxgkrnl keeps closed.
static VS_TICKS: AtomicU32 = AtomicU32::new(0);
static VS_OFF: AtomicU32 = AtomicU32::new(0);
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
/// `VsPowerMode`, `VsWatchdog`, `VsIdleWake` in force (v327; 0 = the KMD 325 behaviour). Read at
/// every StartDevice by [`reread_knobs`], never zeroed by [`start_generation`].
static VS_POWER_MODE: AtomicU32 = AtomicU32::new(0);
static VS_WATCHDOG: AtomicU32 = AtomicU32::new(0);
static VS_IDLE_WAKE: AtomicU32 = AtomicU32::new(0);

/// `VsPowerMode` in force (0 = any non-D0 call quiesces, 1 = adapter only).
pub(crate) fn vs_power_mode() -> u32 {
    VS_POWER_MODE.load(Ordering::Relaxed)
}

/// `VsWatchdog` in force (0 off, 1 revive, 2 revive and resume).
pub(crate) fn vs_watchdog() -> u32 {
    VS_WATCHDOG.load(Ordering::Relaxed)
}

/// `VsIdleWake` in force (1 = the worker's 4 Hz idle wake).
pub(crate) fn vs_idle_wake() -> u32 {
    VS_IDLE_WAKE.load(Ordering::Relaxed)
}

/// One vsync tick of bookkeeping (`VsPendN`, its maximum) and, with `FlipWdogMs` set, the
/// watchdog. Called from the vsync DPC (`adapter/kobj.rs`, DISPATCH, atomics only). The tick is
/// serialized (one-shot, re-armed by its own DPC), so the state is plain loads and stores.
///
/// The watchdog publishes the newest recorded flip's address as a kept picture
/// (`AdapterContext::publish_kept_primary`, one atomic store, legal at DISPATCH) once the
/// pending run has gone `FlipWdogMs` worth of ticks without any publication, for ANY class of
/// allocation including Venus, only if that flip is NEWER than the last one done (published by
/// anyone: `note_published` marks the newest recorded flip done whenever its address is
/// published), so never twice for the same flip and never the address of a flip a newer one
/// already replaced or completed. The kept address names a
/// picture that is not on the screen: this is recovery, not completion, and only the opt-in knob
/// allows it. It does not lower the programming gate or touch the pending slot: the worker
/// still owns the programming and will bind or reject it as before.
pub(crate) fn on_vsync_tick(adapter: &AdapterContext, period_100ns: u64) {
    VS_TICKS.fetch_add(1, Ordering::Relaxed);
    // The tick's own time (`VsTickT`, the watchdog's reference) and the longest silence between
    // two ticks (`VsGapMaxMs`; the arm clears the previous tick, so a quiesce is not a gap).
    let now = crate::adapter::foreign_scanout::now_100ns();
    let previous = VS_TICK_AT.swap(now, Ordering::Relaxed);
    if previous != 0 {
        let gap_ms = (now.saturating_sub(previous) / helios_kmd_logic::vsync_rate::UNITS_PER_MS)
            .min(u32::MAX as u64) as u32;
        VS_GAP_MAX_MS.fetch_max(gap_ms, Ordering::Relaxed);
    }
    VS_TICK_T.store(
        helios_kmd_logic::vsync_rate::ms_from_100ns(now),
        Ordering::Relaxed,
    );
    VS_REF_AT.store(now, Ordering::Relaxed);
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
            done_seq: DONE_SEQ.load(Ordering::Relaxed),
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
        // Done BEFORE the publication (which would mark it too): the same flip never fires twice.
        DONE_SEQ.store(sd::flip_seq(flip_word), Ordering::Relaxed);
        adapter.publish_kept_primary(address);
        WD_COUNT.fetch_add(1, Ordering::Relaxed);
        WD_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    }
}

/// Mirror the two tick counts beside `VpVsN` (`scanout_trace::dump`, the worker's periodic dump,
/// which keeps running with `ForeignFlip` on; the rest of this block rides the Venus refresh
/// mirror, which does not). PASSIVE.
pub(crate) fn publish_vsync_ticks() {
    use crate::diag::record_named_bytes as rec;
    rec(b"VsTickN", VS_TICKS.load(Ordering::Relaxed));
    rec(b"VsOffN", VS_OFF.load(Ordering::Relaxed));
    // The time of the last tick, written in the same call as the count: a count that stays put
    // while `StallT` (or `VpDmpT`) moves is a heartbeat that stopped, whatever the mirror's age.
    rec(b"VsTickT", VS_TICK_T.load(Ordering::Relaxed));
    rec(b"VsGapMaxMs", VS_GAP_MAX_MS.load(Ordering::Relaxed));
    rec(b"VsArmN", VS_ARM_N.load(Ordering::Relaxed));
    rec(b"VsDisN", VS_DIS_N.load(Ordering::Relaxed));
    rec(b"VsCanN", VS_CAN_N.load(Ordering::Relaxed));
    rec(b"VsEarlyN", VS_EARLY_N.load(Ordering::Relaxed));
    rec(b"VsExhN", VS_EXH_N.load(Ordering::Relaxed));
    rec(b"VsRevN", VS_REV_N.load(Ordering::Relaxed));
    rec(b"PwrN", PWR_N.load(Ordering::Relaxed));
    rec(b"PwrUid", PWR_UID.load(Ordering::Relaxed));
    rec(b"PwrD3N", PWR_D3_N.load(Ordering::Relaxed));
    rec(b"PwrT", PWR_T.load(Ordering::Relaxed));
    rec(b"PwrAdSt", PWR_AD_ST.load(Ordering::Relaxed));
    rec(b"PwrChSt", PWR_CH_ST.load(Ordering::Relaxed));
    rec(b"PwrStg", PWR_STG.load(Ordering::Relaxed));
    rec(b"VsCiT", VS_CI_T.load(Ordering::Relaxed));
    rec(b"VsCiSt", VS_CI_ST.load(Ordering::Relaxed));
    rec(b"LkWaitN", LK_WAIT_N.load(Ordering::Relaxed));
    rec(b"LkWaitWh", LK_WAIT_WH.load(Ordering::Relaxed));
    rec(b"LkWaitT", LK_WAIT_T.load(Ordering::Relaxed));
    rec(b"LkWaitMs", LK_WAIT_MS.load(Ordering::Relaxed));
    rec(b"StopSub", STOP_SUB.load(Ordering::Relaxed));
    rec(b"StopSubT", STOP_SUB_T.load(Ordering::Relaxed));
}

// ---- the heartbeat's life (T5 anomaly 2) -----------------------------------------------------

/// The interrupt time (ms) and the 100 ns time of the last tick, the longest silence (ms), and the
/// watchdog's reference: the newest of the last tick, the last arm and the last revive (0 = the
/// heartbeat is not meant to run).
static VS_TICK_T: AtomicU32 = AtomicU32::new(0);
static VS_TICK_AT: AtomicU64 = AtomicU64::new(0);
static VS_GAP_MAX_MS: AtomicU32 = AtomicU32::new(0);
static VS_REF_AT: AtomicU64 = AtomicU64::new(0);
/// Effective arms and disarms, cancels of the one-shot, ticks that returned before the count,
/// deadline exhaustions, revives by the watchdog.
static VS_ARM_N: AtomicU32 = AtomicU32::new(0);
static VS_DIS_N: AtomicU32 = AtomicU32::new(0);
static VS_CAN_N: AtomicU32 = AtomicU32::new(0);
static VS_EARLY_N: AtomicU32 = AtomicU32::new(0);
static VS_EXH_N: AtomicU32 = AtomicU32::new(0);
static VS_REV_N: AtomicU32 = AtomicU32::new(0);
/// `DxgkDdiSetPowerState` calls, the last `DeviceUid`, and those that left D0.
static PWR_N: AtomicU32 = AtomicU32::new(0);
static PWR_UID: AtomicU32 = AtomicU32::new(0);
static PWR_D3_N: AtomicU32 = AtomicU32::new(0);
/// v328: when the last `DxgkDdiSetPowerState` came (ms), the adapter's and the monitor child's
/// last state (1 = D0, 0 = not D0, 0xFF = none yet), how far the last call got, and the time and
/// argument of the last `ControlInterrupt(CRTC_VSYNC)` (0xFF = none yet).
static PWR_T: AtomicU32 = AtomicU32::new(0);
static PWR_AD_ST: AtomicU32 = AtomicU32::new(0xFF);
static PWR_CH_ST: AtomicU32 = AtomicU32::new(0xFF);
static PWR_STG: AtomicU32 = AtomicU32::new(0);
static VS_CI_T: AtomicU32 = AtomicU32::new(0);
static VS_CI_ST: AtomicU32 = AtomicU32::new(0xFF);
/// Waits on a mutex that outlived a 5 s slice: count, which lock, when, longest wait (ms).
static LK_WAIT_N: AtomicU32 = AtomicU32::new(0);
static LK_WAIT_WH: AtomicU32 = AtomicU32::new(0);
static LK_WAIT_T: AtomicU32 = AtomicU32::new(0);
static LK_WAIT_MS: AtomicU32 = AtomicU32::new(0);
/// The finest StopDevice / RemoveDevice step entered and when (never zeroed by a StartDevice: it
/// names where the PREVIOUS stop was if this generation is the one after a hang or a crash).
static STOP_SUB: AtomicU32 = AtomicU32::new(0);
static STOP_SUB_T: AtomicU32 = AtomicU32::new(0);
/// The interrupt time (ms) of the last [`publish_counters`] (`StallT`), whoever ran it; 0 = never.
static STALL_PUB_T: AtomicU32 = AtomicU32::new(0);

/// `DxgkDdiSetPowerState` got as far as `stage` (1 entered, 3 done).
/// Atomics only: it must survive a registry that is the thing that hangs.
pub(crate) fn power_stage(stage: u32) {
    PWR_STG.store(stage, Ordering::Relaxed);
}

/// `DxgkDdiControlInterrupt(CRTC_VSYNC, enable)` (DIRQL: atomics only).
pub(crate) fn note_control_vsync(enable: bool) {
    VS_CI_ST.store(enable as u32, Ordering::Relaxed);
    VS_CI_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
}

/// A wait on lock `which` (`stall_diag::lock`) outlived `slices` 5 s slices and goes on waiting.
/// PASSIVE, atomics only (the registry mirror is `publish_vsync_ticks`).
pub(crate) fn note_long_wait(which: u32, slices: u32) {
    LK_WAIT_N.fetch_add(1, Ordering::Relaxed);
    LK_WAIT_WH.store(which, Ordering::Relaxed);
    LK_WAIT_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    LK_WAIT_MS.fetch_max(sd::long_wait_ms(slices), Ordering::Relaxed);
}

/// StopDevice / RemoveDevice is about to run `step` (`stall_diag::stop_sub`): the atomics and two
/// registry values, written BEFORE the step so a hang names it. PASSIVE.
pub(crate) fn stop_sub(step: u32) {
    use crate::diag::record_named_bytes as rec;
    let now = AdapterContext::interrupt_time_ms();
    STOP_SUB.store(step, Ordering::Relaxed);
    STOP_SUB_T.store(now, Ordering::Relaxed);
    rec(b"StopSub", step);
    rec(b"StopSubT", now);
}

/// The heartbeat was armed at `now` (100 ns): the first tick has no predecessor, and the
/// watchdog counts its silence from here. Any IRQL.
pub(crate) fn note_vsync_armed(now: u64) {
    VS_ARM_N.fetch_add(1, Ordering::Relaxed);
    VS_TICK_AT.store(0, Ordering::Relaxed);
    VS_REF_AT.store(now, Ordering::Relaxed);
}

/// The heartbeat was disarmed (quiesce or stop): the watchdog has nothing to watch.
pub(crate) fn note_vsync_disarmed() {
    VS_DIS_N.fetch_add(1, Ordering::Relaxed);
    VS_REF_AT.store(0, Ordering::Relaxed);
    VS_TICK_AT.store(0, Ordering::Relaxed);
}

/// The one-shot was cancelled (any path).
pub(crate) fn note_vsync_cancel() {
    VS_CAN_N.fetch_add(1, Ordering::Relaxed);
}

/// A tick returned before the count: the display half is off or the heartbeat is disarmed.
pub(crate) fn note_vsync_early() {
    VS_EARLY_N.fetch_add(1, Ordering::Relaxed);
}

/// A tick found the deadline exhausted and left the heartbeat disarmed.
pub(crate) fn note_vsync_exhausted() {
    VS_EXH_N.fetch_add(1, Ordering::Relaxed);
}

/// The watchdog's reference time (100 ns; 0 = not watching).
pub(crate) fn vsync_reference() -> u64 {
    VS_REF_AT.load(Ordering::Relaxed)
}

/// The watchdog found the heartbeat dead and is re-arming it at `now`: re-base the reference.
/// `true` for exactly one of several racing callers.
pub(crate) fn note_vsync_revived(reference: u64, now: u64) -> bool {
    if VS_REF_AT
        .compare_exchange(reference, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    VS_REV_N.fetch_add(1, Ordering::Relaxed);
    true
}

/// `DxgkDdiSetPowerState` was called for `device_uid`; `d0` is whether the state is D0.
pub(crate) fn note_power(device_uid: u32, d0: bool) {
    PWR_N.fetch_add(1, Ordering::Relaxed);
    PWR_UID.store(device_uid, Ordering::Relaxed);
    if !d0 {
        PWR_D3_N.fetch_add(1, Ordering::Relaxed);
    }
    PWR_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    if device_uid == hpd_wake::DISPLAY_ADAPTER_HW_ID {
        ADAPTER_D0.store(d0 as u32, Ordering::Release);
        PWR_AD_ST.store(d0 as u32, Ordering::Relaxed);
    } else {
        PWR_CH_ST.store(d0 as u32, Ordering::Relaxed);
    }
}

/// The adapter's last power state is D0 (initially, and again at every StartDevice).
static ADAPTER_D0: AtomicU32 = AtomicU32::new(1);

/// Whether the adapter is in D0 as far as `DxgkDdiSetPowerState` has said.
pub(crate) fn adapter_d0() -> bool {
    ADAPTER_D0.load(Ordering::Acquire) != 0
}

/// A vsync tick ran with the CRTC_VSYNC delivery gate closed (`VsOffN`). Atomics only (DISPATCH).
pub(crate) fn note_gate_closed_tick() {
    VS_OFF.fetch_add(1, Ordering::Relaxed);
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
/// starts a fresh one: `kmd_logic::stall_diag::DeferState`). With the knob at 0 (the default) it
/// touches nothing and answers `Again`: today's behaviour. On `Exhausted` the state is
/// forgotten. The count is of CONSECUTIVE Deferred outcomes: every other outcome of the deferred
/// wrapper clears it ([`clear_defer_state`], from `clear_retry_state` and from the retryable
/// refusal arm).
pub(crate) fn defer_note(handle: usize) -> DeferDecision {
    let budget = DEFER_BUDGET.load(Ordering::Relaxed);
    let (state, decision) = DeferState {
        handle: DEFER_HANDLE.load(Ordering::Relaxed),
        attempts: DEFER_ATTEMPTS.load(Ordering::Relaxed),
    }
    .note(handle, budget);
    if budget != 0 {
        DEFER_HANDLE.store(state.handle, Ordering::Relaxed);
        DEFER_ATTEMPTS.store(state.attempts, Ordering::Relaxed);
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
    let pm = hpd_wake::clamp_power_mode(crate::diag::read_config_dword(
        crate::diag::knobs::VS_POWER_MODE,
        0,
    ));
    let wd = hpd_wake::clamp_watchdog(crate::diag::read_config_dword(
        crate::diag::knobs::VS_WATCHDOG,
        0,
    ));
    let idle = hpd_wake::clamp_idle_wake(crate::diag::read_config_dword(
        crate::diag::knobs::VS_IDLE_WAKE,
        0,
    ));
    VS_POWER_MODE.store(pm, Ordering::Relaxed);
    VS_WATCHDOG.store(wd, Ordering::Relaxed);
    VS_IDLE_WAKE.store(idle, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"VsPwrEff", pm);
    crate::diag::record_named_bytes(b"VsWdgEff", wd);
    crate::diag::record_named_bytes(b"VsIdlEff", idle);
}

// ---- v327 breadcrumbs: what the previous generation left, where the worker and the mode set are

/// What the statics and the adapter held at the ENTRY of this StartDevice (before anything of the
/// new generation ran): the previous generation's `ADAPTER_D0`, the watchdog reference, whether
/// the heartbeat was armed, the delivery gate, whether a worker thread was registered, the
/// previous `HpdN` and the previous heartbeat tick count. Written once per StartDevice
/// ([`note_start_entry`]), never zeroed: it names the state a failed restart inherited.
static ENT_D0: AtomicU32 = AtomicU32::new(0);
static ENT_REF: AtomicU32 = AtomicU32::new(0);
static ENT_ARM: AtomicU32 = AtomicU32::new(0);
static ENT_VS_EN: AtomicU32 = AtomicU32::new(0);
static ENT_HPD_TH: AtomicU32 = AtomicU32::new(0);
static ENT_HPD_N: AtomicU32 = AtomicU32::new(0);
static ENT_VS_TK: AtomicU32 = AtomicU32::new(0);

/// StartDevice entry, PASSIVE, BEFORE [`start_generation`] zeroes anything.
pub(crate) fn note_start_entry(adapter: &AdapterContext, hpd_indicates: u32) {
    ENT_D0.store(ADAPTER_D0.load(Ordering::Relaxed), Ordering::Relaxed);
    ENT_REF.store(
        helios_kmd_logic::vsync_rate::ms_from_100ns(VS_REF_AT.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
    ENT_ARM.store(adapter.vsync_armed.load(Ordering::Relaxed), Ordering::Relaxed);
    ENT_VS_EN.store(adapter.vsync_enabled.load(Ordering::Relaxed), Ordering::Relaxed);
    ENT_HPD_TH.store(u32::from(adapter.hpd_worker_registered()), Ordering::Relaxed);
    ENT_HPD_N.store(hpd_indicates, Ordering::Relaxed);
    ENT_VS_TK.store(VS_TICKS.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// The HPD worker's phase this generation (1 thread entered, 2 StartDevice's return seen, 3 first
/// indication done, 4 first loop reached) and when the first loop ran. Atomics only; zeroed by
/// [`start_generation`].
static HPD_PHASE: AtomicU32 = AtomicU32::new(0);
static HPD_PHASE_T: AtomicU32 = AtomicU32::new(0);
static HPD_FIRST_T: AtomicU32 = AtomicU32::new(0);

/// The worker reached `phase`.
pub(crate) fn hpd_phase(phase: u32) {
    let now = AdapterContext::interrupt_time_ms();
    HPD_PHASE.store(phase, Ordering::Relaxed);
    HPD_PHASE_T.store(now, Ordering::Relaxed);
    if phase >= 4 && HPD_FIRST_T.load(Ordering::Relaxed) == 0 {
        HPD_FIRST_T.store(now.max(1), Ordering::Relaxed);
    }
}

/// The mode-set path of this generation: the last display DDI entered (`mode_step`), when, how
/// many were entered, and the status the last one that returns one gave (`mode_result`).
/// Steps: 1 QueryChildRelations, 2 QueryChildStatus, 3 IsSupportedVidPn, 4 RecommendFunctionalVidPn,
/// 5 EnumVidPnCofuncModality, 6 SetVidPnSourceVisibility, 7 CommitVidPn entered, 8 CommitVidPn
/// returned, 9 UpdateActiveVidPnPresentPath, 10 ControlInterrupt(CRTC_VSYNC, enable).
static MODE_STG: AtomicU32 = AtomicU32::new(0);
static MODE_T: AtomicU32 = AtomicU32::new(0);
static MODE_N: AtomicU32 = AtomicU32::new(0);
static MODE_ST: AtomicU32 = AtomicU32::new(0);

/// A mode-set DDI was entered (any IRQL: atomics only). The registry mirror is [`publish_mode`].
pub(crate) fn mode_step(step: u32) {
    MODE_STG.store(step, Ordering::Relaxed);
    MODE_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    MODE_N.fetch_add(1, Ordering::Relaxed);
}

/// A mode-set DDI returned `status` (atomics only).
pub(crate) fn mode_result(step: u32, status: i32) {
    MODE_STG.store(step, Ordering::Relaxed);
    MODE_T.store(AdapterContext::interrupt_time_ms(), Ordering::Relaxed);
    MODE_ST.store(status as u32, Ordering::Relaxed);
}

/// Write the mode-set breadcrumbs to the service key. PASSIVE only (the mode-set DDIs are).
pub(crate) fn publish_mode() {
    use crate::diag::record_named_bytes as rec;
    rec(b"ModeStg", MODE_STG.load(Ordering::Relaxed));
    rec(b"ModeStgT", MODE_T.load(Ordering::Relaxed));
    rec(b"ModeN", MODE_N.load(Ordering::Relaxed));
    rec(b"ModeSt", MODE_ST.load(Ordering::Relaxed));
}

/// The entry snapshot, the worker's phase and the mode-set breadcrumbs, mirrored with the rest.
fn publish_breadcrumbs() {
    use crate::diag::record_named_bytes as rec;
    rec(b"EntD0", ENT_D0.load(Ordering::Relaxed));
    rec(b"EntRef", ENT_REF.load(Ordering::Relaxed));
    rec(b"EntArm", ENT_ARM.load(Ordering::Relaxed));
    rec(b"EntVsEn", ENT_VS_EN.load(Ordering::Relaxed));
    rec(b"EntHpdTh", ENT_HPD_TH.load(Ordering::Relaxed));
    rec(b"EntHpdN", ENT_HPD_N.load(Ordering::Relaxed));
    rec(b"EntVsTk", ENT_VS_TK.load(Ordering::Relaxed));
    rec(b"HpdPhase", HPD_PHASE.load(Ordering::Relaxed));
    rec(b"HpdPhaseT", HPD_PHASE_T.load(Ordering::Relaxed));
    rec(b"HpdFirstT", HPD_FIRST_T.load(Ordering::Relaxed));
    publish_mode();
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
        &VS_PEND,
        &VS_PEND_MAX,
        &VS_TICKS,
        &VS_OFF,
        &VS_STALL,
        &VS_SEEN_PUB,
        &WD_COUNT,
        &WD_T,
        &WD_BIG,
        &DEFER_ATTEMPTS,
        &WK_EVT,
        &WK_TMO,
        &WK_SRC_PENDING,
        &WK_SRC_LAST,
        &SG_COALESCED,
        &WAIT_US,
        &TM_CTL,
        &TM_RTY,
        &TM_DUE,
        &TM_NONE,
        &BUSY_US,
        &PASS_MAX_US,
        &DUMP_N,
        &DUMP_US,
        &DUMP_SKIP,
        &VS_TICK_T,
        &VS_GAP_MAX_MS,
        &VS_ARM_N,
        &VS_DIS_N,
        &VS_CAN_N,
        &VS_EARLY_N,
        &VS_EXH_N,
        &VS_REV_N,
        &PWR_N,
        &PWR_UID,
        &PWR_D3_N,
        &HPD_PHASE,
        &HPD_PHASE_T,
        &HPD_FIRST_T,
        &MODE_STG,
        &MODE_T,
        &MODE_N,
        &MODE_ST,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    // Every v326 static that outlives a StopDevice starts the generation in its initial state: the
    // watchdog's reference (the heartbeat is armed again after this, which sets it).
    VS_REF_AT.store(0, Ordering::Relaxed);
    for c in &SIGNALS {
        c.store(0, Ordering::Relaxed);
    }
    WAIT_US_MIN.store(u32::MAX, Ordering::Relaxed);
    ADAPTER_D0.store(1, Ordering::Release);
    PWR_AD_ST.store(1, Ordering::Relaxed);
    PWR_CH_ST.store(0xFF, Ordering::Relaxed);
    PWR_STG.store(0, Ordering::Relaxed);
    VS_CI_ST.store(0xFF, Ordering::Relaxed);
    VS_CI_T.store(0, Ordering::Relaxed);
    LK_WAIT_N.store(0, Ordering::Relaxed);
    LK_WAIT_WH.store(0, Ordering::Relaxed);
    LK_WAIT_T.store(0, Ordering::Relaxed);
    LK_WAIT_MS.store(0, Ordering::Relaxed);
    PASS_START.store(0, Ordering::Relaxed);
    VS_TICK_AT.store(0, Ordering::Relaxed);
    FLIP_WORD.store(0, Ordering::Release);
    DONE_SEQ.store(0, Ordering::Relaxed);
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
    let now = AdapterContext::interrupt_time_ms();
    STALL_PUB_T.store(now.max(1), Ordering::Relaxed);
    rec(b"StallT", now);
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
    // `VsTickN` / `VsOffN` and the rest of the heartbeat's life, then the worker's wakes.
    publish_vsync_ticks();
    publish_hpd_wake();
    publish_breadcrumbs();
    rec(b"StartN", START_N.load(Ordering::Relaxed));
    rec(b"StartT", START_T.load(Ordering::Relaxed));
    rec(b"FlipWd", WD_COUNT.load(Ordering::Relaxed));
    rec(b"FlipWdT", WD_T.load(Ordering::Relaxed));
    rec(b"FlipWdBig", WD_BIG.load(Ordering::Relaxed));
}

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
/// most every `ESCAPE_PUBLISH_MS` (`helios_kmd_logic::stall_diag`). An escape is called by user mode at PASSIVE on ITS OWN
/// thread, so this refreshes the counters even when the worker is stuck (the other mirrors run on
/// the worker, including the `Nv*` mirror an escape asks for). While the worker is healthy this
/// does NOTHING beyond one clock read, one load and the loads of the stuck test: the block is
/// about twenty registry writes (each opens the key by path, about half a millisecond in all)
/// and the worker's own mirrors keep publishing it as before.
pub(crate) fn publish_from_escape(adapter: &AdapterContext) {
    let now = AdapterContext::interrupt_time_ms().max(1);
    let last = LAST_ESCAPE_PUBLISH.load(Ordering::Relaxed);
    // Twice a second at most; then only while the worker looks stuck, or while the block is a
    // snapshot older than 5 s (a worker asleep with nothing to wake it writes no block of its
    // own, and a reader must not take its frozen `HpdSite` / `HpdLoopT` for live values).
    if !sd::escape_publish_due(
        worker_looks_stuck(adapter, now),
        now,
        last,
        STALL_PUB_T.load(Ordering::Relaxed),
    ) {
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
    // The foreign scanout block, when the DISPATCH watchdog ended a source and the (stuck)
    // worker cannot mirror its counters: one load when not due.
    crate::adapter::foreign_scanout::publish_if_due();
}
