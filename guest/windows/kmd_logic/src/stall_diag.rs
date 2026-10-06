//! Stall diagnosis and the opt-in flip watchdog: the pure half. Counters, site ids, the vsync tick
//! bookkeeping, the watchdog decision and the Deferred retry budget, all functions of their
//! arguments (no clock, no memory, no registry). The I/O half is
//! `kmd_render/src/ddi/stall_diag.rs`; the arms are in `ddi/hpd.rs`, `ddi/display.rs`,
//! `adapter/kobj.rs` (the vsync DPC), `adapter/mod.rs` and `virtio/gpu/mod.rs`. Design, the
//! counter list to dump and the decision table: `docs/zero-copy-present.md`, "Stall diagnosis".
//!
//! WHY. A desktop stall after a user NVK scanout app exited (the Venus DWM stopped presenting, no
//! TDR, no crash) could not be named from the counters: the registry mirrors are event gated and
//! the `Vp*` dump only runs every 128th HPD worker wake, so a stuck worker shows a stale dump.
//! The candidates were (1) the HPD worker, or the Venus programming, blocked in the KMD (a lock
//! held across a host round trip, or a Deferred programming retrying with no budget), (2) a flip
//! whose completion is withheld (`flip_completion::decide` answers `None` for a Venus source), (3)
//! a wait the KMD cannot see. This module is the instrument for telling them apart (breadcrumbs
//! the worker leaves as it goes, a flip issued/published pair, a tick count of "pending") plus two
//! default-off safety valves (`DeferBudget`, `FlipWdogMs`).
//!
//! Everything here defaults to the behaviour before it existed: the knobs are 0, and the
//! counters are new passive registry values.

/// Where the HPD worker is, as the `HpdSite` counter. The worker stores the id on entering each
/// service or step of its loop (`ddi/hpd.rs`), so a worker that stops answering shows WHICH step
/// it stopped in: with `HpdSiteT` (the interrupt time it entered it) and the time of the dump
/// (`StallT`) the age in the step is a subtraction. `WAIT` is "asleep on the wake event", which is
/// healthy and says nothing about a stall. Append, do not renumber: the ids are owner-readable
/// ABI in the service key.
pub mod site {
    /// The worker never ran (or the counters were just reset by a StartDevice).
    pub const NONE: u32 = 0;
    /// Asleep in `KeWaitForSingleObject(hpd_event)`: idle, not stuck.
    pub const WAIT: u32 = 1;
    /// The prologue wait for StartDevice to return.
    pub const START_WAIT: u32 = 2;
    /// `indicate_child_status` (`DxgkCbIndicateChildStatus`).
    pub const INDICATE: u32 = 3;
    /// `drain_used_and_complete` (the used-ring drain, holds `virtio_lock`).
    pub const DRAIN_USED: u32 = 4;
    /// `foreign_scanout_service` (a foreign scan-out source lapsing).
    pub const FOREIGN_SCANOUT: u32 = 5;
    /// `foreign_fence_service` (fenced presents: flips whose fences fired, fence closes).
    pub const FOREIGN_FENCE: u32 = 6;
    /// `process_deferred_vidpn_source_address`: takes the scanout mutex and programs the primary
    /// (`SET_SCANOUT_BLOB` round trips, the Venus copy). The usual suspect.
    pub const DEFERRED_VIDPN: u32 = 7;
    /// `service_windowed_blt`.
    pub const WINDOWED_BLT: u32 = 8;
    /// `rm_client::service`: the level 5 service (`KmdRmClient`).
    pub const RM_CLIENT: u32 = 9;
    /// `foreign_flip::service` (`ForeignFlip`).
    pub const FOREIGN_FLIP: u32 = 10;
    /// `nvrm_publish_service` (the `Nv*` registry mirror).
    pub const NVRM_PUBLISH: u32 = 11;
    /// `scanout_trace::dump_periodic` (the `Vp*` dump, about 120 registry writes).
    pub const DUMP: u32 = 12;
    /// The one-shot Present probe (a fence wait and a host map round trip).
    pub const PROBE: u32 = 13;
    /// `queue_active_scanout_refresh` (takes the scanout mutex, enqueues `RESOURCE_FLUSH`).
    pub const REFRESH: u32 = 14;
    /// The worker is terminating.
    pub const EXITED: u32 = 15;
    /// Inside `process_deferred_vidpn_source_address` WITH the scanout mutex held (id 7 is
    /// waiting for it): the programming itself, `SET_SCANOUT_BLOB` and the Venus copy.
    pub const DEFERRED_LOCKED: u32 = 16;
    /// Inside `queue_active_scanout_refresh` with the scanout mutex held (id 14 is waiting for it).
    pub const REFRESH_LOCKED: u32 = 17;

    /// Every id with its name, for the doc and the host tests.
    pub const ALL: [(u32, &str); 18] = [
        (NONE, "none"),
        (WAIT, "wait"),
        (START_WAIT, "start_wait"),
        (INDICATE, "indicate_child"),
        (DRAIN_USED, "drain_used"),
        (FOREIGN_SCANOUT, "foreign_scanout_service"),
        (FOREIGN_FENCE, "foreign_fence_service"),
        (DEFERRED_VIDPN, "process_deferred_vidpn_source_address"),
        (WINDOWED_BLT, "service_windowed_blt"),
        (RM_CLIENT, "rm_client::service"),
        (FOREIGN_FLIP, "foreign_flip::service"),
        (NVRM_PUBLISH, "nvrm_publish_service"),
        (DUMP, "dump_periodic"),
        (PROBE, "present_probe"),
        (REFRESH, "queue_active_scanout_refresh"),
        (EXITED, "exited"),
        (
            DEFERRED_LOCKED,
            "process_deferred_vidpn_source_address (mutex held)",
        ),
        (REFRESH_LOCKED, "queue_active_scanout_refresh (mutex held)"),
    ];
}

/// The service-key counter names the I/O half (`ddi/stall_diag.rs`) writes, nothing else does: at
/// most 14 characters (`record_named_bytes` clamps there), none equal to any other counter in
/// `kmd_render` or `kmd_logic` (host-tested by scanning both trees). The two `Fk` names,
/// `FkDefBud` and `FkVenus`, are in `flip_completion::COUNTERS` and written by `ddi/flip_keep.rs`.
///
/// * `HpdLoopN`, `HpdLoopT`: HPD worker loops, interrupt time (ms) of the last one's wake.
/// * `HpdSite`, `HpdSiteT`: [`site`] the worker is in or last entered, and when it entered it.
/// * `FlipIss`: flips dxgkrnl issued (each `SetVidPnSourceAddress`, each DMA flip taken).
///   `FlipPub`: publications of a displayed address (bound or kept, any class). `FlipPubT`: when.
/// * `VsPendN`, `VsPendMax`: consecutive vsync ticks with a pending programming (handle in the
///   slot, or the programming gate raised), and the longest run this generation.
/// * `ScLkN`, `ScLkRelN`, `ScLkAcqT`, `ScLkRelT`: acquisitions and releases of the scanout mutex
///   (held now when they differ, [`lock_held`]) and the interrupt time of the last acquisition
///   and release.
/// * `StartN`, `StartT`: StartDevice generation count (since the image loaded) and its time.
/// * `FlipWd`, `FlipWdT`, `FlipWdBig`: watchdog publications, the time of the last, and flips it
///   could not record (an address above 2^40).
/// * `StallT`: interrupt time (ms) of the publication of this block: the "now" of every age.
/// * `FlWdMsEff`, `DefBudEff`: the `FlipWdogMs` and `DeferBudget` knobs in force (clamped, 0
///   included), written at every StartDevice.
pub const COUNTERS: [&str; 21] = [
    "HpdLoopN",
    "HpdLoopT",
    "HpdSite",
    "HpdSiteT",
    "ScLkN",
    "ScLkRelN",
    "ScLkAcqT",
    "ScLkRelT",
    "FlipIss",
    "FlipPub",
    "FlipPubT",
    "VsPendN",
    "VsPendMax",
    "StartN",
    "StartT",
    "FlipWd",
    "FlipWdT",
    "FlipWdBig",
    "StallT",
    "FlWdMsEff",
    "DefBudEff",
];

// ---- the scanout mutex -----------------------------------------------------------------------

/// Whether the scanout mutex is held, from the acquisition and release COUNTS (`ScLkN`,
/// `ScLkRelN`). The mutex serializes its holders, so the counts alternate: they are equal when it
/// is free and differ by one while held. Counts, not the millisecond stamps: an acquisition and a
/// release in the same millisecond are indistinguishable by time, and the wrapping 32-bit counts
/// compare exactly.
pub const fn lock_held(acquired: u32, released: u32) -> bool {
    acquired != released
}

// ---- knobs ---------------------------------------------------------------------------------

/// Smallest nonzero `FlipWdogMs`. A flip's programming legitimately takes a few vsync ticks (the
/// PASSIVE worker wakes on the tick, a host round trip follows), and the watchdog publishes an
/// address that names a picture not on screen; a value below this would fire on ordinary load.
pub const WDOG_MIN_MS: u32 = 50;
/// Largest `FlipWdogMs` (a minute): above it the valve would not be one.
pub const WDOG_MAX_MS: u32 = 60_000;

/// `FlipWdogMs` as the driver uses it: 0 stays 0 (off), anything else is clamped into
/// `[WDOG_MIN_MS, WDOG_MAX_MS]`.
pub const fn clamp_wdog_ms(raw: u32) -> u32 {
    if raw == 0 {
        0
    } else if raw < WDOG_MIN_MS {
        WDOG_MIN_MS
    } else if raw > WDOG_MAX_MS {
        WDOG_MAX_MS
    } else {
        raw
    }
}

/// Smallest nonzero `DeferBudget`: a Deferred programming waits for a producer boundary or a
/// busy publication to clear, and the worker retries it on every wake (about one per vsync tick,
/// more when completions also wake it); a handful of attempts is a few frames, not a stall.
pub const DEFER_BUDGET_MIN: u32 = 16;
/// Largest `DeferBudget` (about 19 hours of ticks at 60 Hz): above it the budget is moot.
pub const DEFER_BUDGET_MAX: u32 = 4_000_000;
/// The value to try on a diagnosis run: 240 attempts, about four seconds at the vsync rate. NOT
/// the default (see the doc: the budget cannot be proven never to cut a flow that is working, a
/// producer boundary that retires after the budget is a legitimate, if slow, completion).
pub const DEFER_BUDGET_SUGGESTED: u32 = 240;

/// `DeferBudget` as the driver uses it: 0 stays 0 (unlimited, today's behaviour), anything else
/// is clamped into `[DEFER_BUDGET_MIN, DEFER_BUDGET_MAX]`.
pub const fn clamp_defer_budget(raw: u32) -> u32 {
    if raw == 0 {
        0
    } else if raw < DEFER_BUDGET_MIN {
        DEFER_BUDGET_MIN
    } else if raw > DEFER_BUDGET_MAX {
        DEFER_BUDGET_MAX
    } else {
        raw
    }
}

// ---- ticks and time ------------------------------------------------------------------------

/// `ms` milliseconds as a whole number of vsync ticks of `period_100ns` (rounded UP, so the
/// watchdog never fires earlier than asked). 0 means "off": for `ms` 0, and for a zero period
/// (nothing to count ticks with). Saturates at `u32::MAX`.
pub const fn ticks_for_ms(ms: u32, period_100ns: u64) -> u32 {
    if ms == 0 || period_100ns == 0 {
        return 0;
    }
    let total_100ns = ms as u64 * 10_000;
    let ticks = (total_100ns + period_100ns - 1) / period_100ns;
    if ticks > u32::MAX as u64 {
        u32::MAX
    } else if ticks == 0 {
        1
    } else {
        ticks as u32
    }
}

// ---- the watchdog's record of the newest flip ----------------------------------------------

/// Address bits a recorded flip carries (a 1 TiB segment space; the Helios segments are BAR
/// apertures far below it).
pub const FLIP_ADDR_BITS: u32 = 40;
const FLIP_ADDR_MASK: u64 = (1u64 << FLIP_ADDR_BITS) - 1;
const FLIP_SEQ_MASK: u32 = 0x00FF_FFFF;

/// The watchdog's record of the newest pending flip as ONE word, so the vsync DPC never reads an
/// address of one flip with the identity of another: `(seq & 0xFFFFFF) << 40 | address`. `seq` is
/// the running flip number (never 0 for a real flip, the caller starts at 1); `None` for a zero
/// address (nothing assigned: nothing to publish) or one that does not fit 40 bits. The word is
/// never 0 for a `Some`, because the address is not.
pub const fn pack_flip(seq: u32, address: u64) -> Option<u64> {
    if address == 0 || address > FLIP_ADDR_MASK {
        return None;
    }
    Some((((seq & FLIP_SEQ_MASK) as u64) << FLIP_ADDR_BITS) | address)
}

/// The address a packed flip word carries (0 for an empty word).
pub const fn flip_address(word: u64) -> u64 {
    word & FLIP_ADDR_MASK
}

// ---- the vsync tick ------------------------------------------------------------------------

/// What the vsync DPC remembers between ticks (a handful of atomics in the driver).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendState {
    /// Consecutive ticks with a pending programming (`VsPendN`).
    pub pend: u32,
    /// The longest such run (`VsPendMax`).
    pub max: u32,
    /// Consecutive pending ticks with no publication in between (the watchdog's clock).
    pub stall: u32,
    /// `FlipPub` as of the previous tick.
    pub seen_pub: u32,
}

/// What one tick can see.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendInput {
    /// `pending_vidpn_allocation != 0` or the programming gate raised.
    pub pending: bool,
    /// `FlipPub` now.
    pub pub_count: u32,
    /// The newest recorded flip ([`pack_flip`]), 0 for none.
    pub flip_word: u64,
    /// The flip word the watchdog already published, 0 for none.
    pub fired_word: u64,
    /// The watchdog interval in ticks ([`ticks_for_ms`]), 0 = off.
    pub limit_ticks: u32,
}

/// What the tick must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdAction {
    /// Nothing.
    None,
    /// Publish this address as the displayed one (a kept picture), and remember the flip word
    /// as fired. Once per flip word.
    Publish(u64),
}

/// One vsync tick of bookkeeping: the pending run, its maximum, the no-publication clock, and the
/// watchdog decision. Total, atomics-sized, no allocation: it runs in the DPC.
///
/// The watchdog fires when it is on (`limit_ticks != 0`), a flip is recorded and not yet fired,
/// and the pending run has gone MORE than `limit_ticks` ticks with no publication since (every
/// publication restarts the clock: a stream of flips that each publish is progress, however long
/// the gate stays raised). After a publication by the watchdog the clock restarts, and the same
/// flip is never published again.
pub const fn pend_step(s: PendState, i: PendInput) -> (PendState, WdAction) {
    if !i.pending {
        return (
            PendState {
                pend: 0,
                max: s.max,
                stall: 0,
                seen_pub: i.pub_count,
            },
            WdAction::None,
        );
    }
    let pend = s.pend.saturating_add(1);
    let max = if pend > s.max { pend } else { s.max };
    let stall = if i.pub_count != s.seen_pub {
        0
    } else {
        s.stall.saturating_add(1)
    };
    if i.limit_ticks != 0
        && stall > i.limit_ticks
        && i.flip_word != 0
        && i.flip_word != i.fired_word
    {
        let address = flip_address(i.flip_word);
        if address != 0 {
            return (
                PendState {
                    pend,
                    max,
                    stall: 0,
                    seen_pub: i.pub_count,
                },
                WdAction::Publish(address),
            );
        }
    }
    (
        PendState {
            pend,
            max,
            stall,
            seen_pub: i.pub_count,
        },
        WdAction::None,
    )
}

// ---- the Deferred retry budget -------------------------------------------------------------

/// The Deferred attempt number for `handle`: one more than the previous when it is the same
/// handle, 1 for a different one (a new primary is not the old one's retry).
pub const fn defer_attempts(prev_handle: usize, prev_attempts: u32, handle: usize) -> u32 {
    if prev_handle == handle {
        prev_attempts.saturating_add(1)
    } else {
        1
    }
}

/// What to do with a Deferred programming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferDecision {
    /// Re-arm the handle and keep the gate raised (today's behaviour).
    Again,
    /// Budget spent: publish the flip's address kept, lower the gate, stop retrying.
    Exhausted,
}

/// The budget decision, with the same convention as the refusal retry (`attempts > budget` gives
/// up). `budget` 0 is unlimited and never exhausts.
pub const fn defer_decide(attempts: u32, budget: u32) -> DeferDecision {
    if budget != 0 && attempts > budget {
        DeferDecision::Exhausted
    } else {
        DeferDecision::Again
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    const PERIOD_60: u64 = 166_667;

    // ---- knobs and time ------------------------------------------------------------------

    #[test]
    fn knobs_zero_is_off_and_nonzero_is_clamped() {
        assert_eq!(clamp_wdog_ms(0), 0);
        assert_eq!(clamp_wdog_ms(1), WDOG_MIN_MS);
        assert_eq!(clamp_wdog_ms(WDOG_MIN_MS - 1), WDOG_MIN_MS);
        assert_eq!(clamp_wdog_ms(250), 250);
        assert_eq!(clamp_wdog_ms(WDOG_MAX_MS), WDOG_MAX_MS);
        assert_eq!(clamp_wdog_ms(u32::MAX), WDOG_MAX_MS);
        assert_eq!(clamp_defer_budget(0), 0);
        assert_eq!(clamp_defer_budget(1), DEFER_BUDGET_MIN);
        assert_eq!(clamp_defer_budget(240), 240);
        assert_eq!(clamp_defer_budget(u32::MAX), DEFER_BUDGET_MAX);
        // The suggested value is itself a legal one.
        assert_eq!(
            clamp_defer_budget(DEFER_BUDGET_SUGGESTED),
            DEFER_BUDGET_SUGGESTED
        );
    }

    #[test]
    fn ticks_for_ms_rounds_up_and_zero_is_off() {
        assert_eq!(ticks_for_ms(0, PERIOD_60), 0);
        assert_eq!(ticks_for_ms(500, 0), 0);
        // 500 ms at 60 Hz: 30 ticks of 16.6667 ms is 500.001 ms, so exactly 30.
        assert_eq!(ticks_for_ms(500, PERIOD_60), 30);
        // 1000 ms: 60 ticks (59.9999 rounds up to 60).
        assert_eq!(ticks_for_ms(1000, PERIOD_60), 60);
        // Never earlier than asked: ticks * period >= ms.
        for ms in [50u32, 51, 99, 100, 250, 333, 500, 1000, 4000, 60_000] {
            for period in [PERIOD_60, 83_333, 69_444, 100_000, 400_000] {
                let t = ticks_for_ms(ms, period) as u64;
                assert!(t * period >= ms as u64 * 10_000, "{ms} ms at {period}");
                assert!(
                    (t - 1) * period < ms as u64 * 10_000,
                    "{ms} ms at {period}: too many"
                );
            }
        }
        // A period longer than the interval is still one tick, never zero (zero means off).
        assert_eq!(ticks_for_ms(50, 10_000_000), 1);
        assert_eq!(ticks_for_ms(u32::MAX, 1), u32::MAX);
    }

    // ---- the flip word -------------------------------------------------------------------

    #[test]
    fn flip_word_roundtrips_and_is_never_zero() {
        assert_eq!(pack_flip(1, 0), None);
        assert_eq!(pack_flip(1, 1 << 40), None);
        assert_eq!(pack_flip(1, u64::MAX), None);
        for (seq, addr) in [
            (1u32, 1u64),
            (1, 0x1000),
            (7, 0x1234_5000),
            (0xFF_FFFF, (1 << 40) - 1),
        ] {
            let w = pack_flip(seq, addr).unwrap();
            assert_ne!(w, 0);
            assert_eq!(flip_address(w), addr);
        }
        // Two different flips of the same address are different words (until the 24-bit wrap).
        assert_ne!(pack_flip(1, 0x1000), pack_flip(2, 0x1000));
        assert_eq!(flip_address(0), 0);
        // The sequence wraps in 24 bits and still makes a nonzero word.
        assert_eq!(pack_flip(0x100_0001, 0x1000), pack_flip(1, 0x1000));
    }

    // ---- the tick ------------------------------------------------------------------------

    fn input(pending: bool, pub_count: u32) -> PendInput {
        PendInput {
            pending,
            pub_count,
            ..PendInput::default()
        }
    }

    #[test]
    fn pending_run_counts_resets_and_keeps_its_maximum() {
        let mut s = PendState::default();
        for n in 1..=5u32 {
            let (next, act) = pend_step(s, input(true, 0));
            assert_eq!(act, WdAction::None);
            assert_eq!(next.pend, n);
            assert_eq!(next.max, n);
            s = next;
        }
        // An idle tick ends the run, keeps the maximum.
        let (s2, _) = pend_step(s, input(false, 0));
        assert_eq!((s2.pend, s2.max, s2.stall), (0, 5, 0));
        // A shorter run does not lower it; a longer one raises it.
        let mut t = s2;
        for _ in 0..3 {
            t = pend_step(t, input(true, 0)).0;
        }
        assert_eq!((t.pend, t.max), (3, 5));
        for _ in 0..4 {
            t = pend_step(t, input(true, 0)).0;
        }
        assert_eq!((t.pend, t.max), (7, 7));
    }

    #[test]
    fn counters_saturate() {
        let s = PendState {
            pend: u32::MAX,
            max: u32::MAX,
            stall: u32::MAX,
            seen_pub: 3,
        };
        let (n, _) = pend_step(s, input(true, 3));
        assert_eq!((n.pend, n.max, n.stall), (u32::MAX, u32::MAX, u32::MAX));
    }

    #[test]
    fn a_publication_restarts_the_no_publish_clock_but_not_the_pending_run() {
        let mut s = PendState::default();
        for _ in 0..10 {
            s = pend_step(s, input(true, 4)).0;
        }
        // First tick seeded seen_pub from 0 to 4: that tick counts as a publication.
        assert_eq!(s.pend, 10);
        assert_eq!(s.stall, 9);
        let (s2, _) = pend_step(s, input(true, 5));
        assert_eq!((s2.pend, s2.stall, s2.seen_pub), (11, 0, 5));
    }

    fn word(seq: u32, addr: u64) -> u64 {
        pack_flip(seq, addr).unwrap()
    }

    /// Drive `ticks` pending ticks with a fixed publication count; the first action, if any.
    fn run(
        mut s: PendState,
        ticks: u32,
        mk: impl Fn(u32) -> PendInput,
    ) -> (PendState, Vec<(u32, WdAction)>) {
        let mut acts = Vec::new();
        for t in 1..=ticks {
            let (n, a) = pend_step(s, mk(t));
            s = n;
            if a != WdAction::None {
                acts.push((t, a));
            }
        }
        (s, acts)
    }

    #[test]
    fn watchdog_off_never_fires() {
        let w = word(1, 0x4000);
        let (_, acts) = run(PendState::default(), 100_000, |_| PendInput {
            pending: true,
            flip_word: w,
            limit_ticks: 0,
            ..PendInput::default()
        });
        assert!(acts.is_empty());
    }

    #[test]
    fn watchdog_fires_once_per_flip_after_the_interval() {
        let w = word(1, 0x4000);
        let limit = ticks_for_ms(500, PERIOD_60);
        assert_eq!(limit, 30);
        let (s, acts) = run(PendState::default(), 500, |_| PendInput {
            pending: true,
            flip_word: w,
            // The driver stores the fired word once it published.
            fired_word: 0,
            limit_ticks: limit,
            ..PendInput::default()
        });
        // Fires on the tick that EXCEEDS the interval (the clock reads limit + 1), and, because
        // the test never stores the fired word, again each limit + 1 ticks: the driver's store of
        // the fired word is what makes it once.
        assert_eq!(acts[0], (limit + 1, WdAction::Publish(0x4000)));
        assert_eq!(acts[1].0, 2 * (limit + 1));
        assert!(s.pend == 500);
        // With the fired word stored (what the driver does) it never repeats.
        let (_, acts) = run(PendState::default(), 5_000, |_| PendInput {
            pending: true,
            flip_word: w,
            fired_word: w,
            limit_ticks: limit,
            ..PendInput::default()
        });
        assert!(acts.is_empty(), "the same flip is never published twice");
    }

    #[test]
    fn watchdog_waits_exactly_the_interval() {
        let w = word(3, 0x8000);
        for limit in [1u32, 2, 30, 240] {
            let (_, acts) = run(PendState::default(), limit + 5, |_| PendInput {
                pending: true,
                flip_word: w,
                limit_ticks: limit,
                ..PendInput::default()
            });
            // The first tick seeds the publication count and counts as clock 0 or 1; the point
            // is that it never fires before `limit` ticks of pending and fires by `limit + 2`.
            let first = acts[0].0;
            assert!(first > limit, "fired at {first} with limit {limit}");
            assert!(first <= limit + 2, "fired at {first} with limit {limit}");
        }
    }

    #[test]
    fn a_stream_of_publishing_flips_is_progress_not_a_stall() {
        // The gate stays raised for ten seconds because each flip re-raises it, but every few
        // ticks something publishes: the watchdog must stay quiet.
        let limit = ticks_for_ms(500, PERIOD_60);
        let (s, acts) = run(PendState::default(), 600, |t| PendInput {
            pending: true,
            pub_count: t / 5,
            flip_word: word(t / 5 + 1, 0x4000 + (t as u64 / 5) * 0x1000),
            limit_ticks: limit,
            ..PendInput::default()
        });
        assert!(acts.is_empty());
        assert_eq!(s.pend, 600);
        assert_eq!(s.max, 600);
    }

    #[test]
    fn idle_ticks_reset_the_clock_and_a_new_flip_fires_again() {
        let limit = 10;
        let w1 = word(1, 0x1000);
        let w2 = word(2, 0x2000);
        let mut s = PendState::default();
        // Nine pending ticks (below the interval), an idle tick, nine more: no fire.
        for _ in 0..9 {
            s = pend_step(
                s,
                PendInput {
                    pending: true,
                    flip_word: w1,
                    limit_ticks: limit,
                    ..Default::default()
                },
            )
            .0;
        }
        s = pend_step(s, input(false, 0)).0;
        for _ in 0..9 {
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    flip_word: w1,
                    limit_ticks: limit,
                    ..Default::default()
                },
            );
            assert_eq!(a, WdAction::None);
            s = n;
        }
        // Stuck on w1 until it fires, then the driver stores it as fired.
        let mut fired = 0u64;
        let mut fires = Vec::new();
        for _ in 0..40 {
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    flip_word: w1,
                    fired_word: fired,
                    limit_ticks: limit,
                    ..Default::default()
                },
            );
            s = n;
            if let WdAction::Publish(addr) = a {
                fired = w1;
                fires.push(addr);
            }
        }
        assert_eq!(fires, std::vec![0x1000]);
        // A newer flip, stuck as well: fires for it, once.
        for _ in 0..40 {
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    flip_word: w2,
                    fired_word: fired,
                    limit_ticks: limit,
                    ..Default::default()
                },
            );
            s = n;
            if let WdAction::Publish(addr) = a {
                fired = w2;
                fires.push(addr);
            }
        }
        assert_eq!(fires, std::vec![0x1000, 0x2000]);
    }

    #[test]
    fn watchdog_needs_a_recorded_flip() {
        let (_, acts) = run(PendState::default(), 1000, |_| PendInput {
            pending: true,
            flip_word: 0,
            limit_ticks: 5,
            ..PendInput::default()
        });
        assert!(acts.is_empty());
    }

    #[test]
    fn watchdog_does_not_fire_while_idle() {
        let (_, acts) = run(PendState::default(), 1000, |_| PendInput {
            pending: false,
            flip_word: word(1, 0x1000),
            limit_ticks: 1,
            ..PendInput::default()
        });
        assert!(acts.is_empty());
    }

    #[test]
    fn the_firing_publication_itself_restarts_the_clock() {
        // After it fires the driver's own publication moves FlipPub: the next tick sees it and
        // restarts the clock. A flip that is still stuck after another full interval fires for
        // its own word; nothing fires twice for one.
        let limit = 10u32;
        let w1 = word(1, 0x1000);
        let mut s = PendState::default();
        let mut pubs = 0u32;
        let mut fired = 0u64;
        let mut fire_ticks = Vec::new();
        for t in 1..=200u32 {
            let flip = if t < 100 { w1 } else { word(2, 0x2000) };
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    pub_count: pubs,
                    flip_word: flip,
                    fired_word: fired,
                    limit_ticks: limit,
                },
            );
            s = n;
            if let WdAction::Publish(_) = a {
                fired = flip;
                pubs += 1;
                fire_ticks.push(t);
            }
        }
        assert_eq!(fire_ticks.len(), 2, "{fire_ticks:?}");
        assert!(fire_ticks[0] <= limit + 3);
        // The pipeline stayed stuck with no publication but the watchdog's own, so the clock had
        // long exceeded the interval when the newer flip appeared: that one fires at once.
        assert_eq!(fire_ticks[1], 100);
    }

    // ---- the Deferred budget -------------------------------------------------------------

    #[test]
    fn defer_attempts_follow_the_handle() {
        assert_eq!(defer_attempts(0, 0, 0x10), 1);
        assert_eq!(defer_attempts(0x10, 1, 0x10), 2);
        assert_eq!(defer_attempts(0x10, 7, 0x20), 1);
        assert_eq!(defer_attempts(0x10, u32::MAX, 0x10), u32::MAX);
    }

    #[test]
    fn defer_budget_zero_is_unlimited() {
        for attempts in [1u32, 4, 240, 241, 1_000_000, u32::MAX] {
            assert_eq!(defer_decide(attempts, 0), DeferDecision::Again);
        }
    }

    #[test]
    fn defer_budget_exhausts_after_exactly_the_budget() {
        for budget in [16u32, 240, 1000] {
            assert_eq!(defer_decide(budget, budget), DeferDecision::Again);
            assert_eq!(defer_decide(budget + 1, budget), DeferDecision::Exhausted);
        }
        // Simulated worker: the same handle deferred over and over.
        let budget = 240;
        let (mut handle, mut attempts) = (0usize, 0u32);
        let mut again = 0;
        loop {
            attempts = defer_attempts(handle, attempts, 0x55);
            handle = 0x55;
            match defer_decide(attempts, budget) {
                DeferDecision::Again => again += 1,
                DeferDecision::Exhausted => break,
            }
        }
        assert_eq!(again, budget);
    }

    // ---- sites and counter names ---------------------------------------------------------

    #[test]
    fn the_lock_is_held_when_the_counts_differ_whatever_the_clock_says() {
        assert!(!lock_held(0, 0));
        assert!(lock_held(1, 0));
        assert!(!lock_held(1, 1));
        assert!(lock_held(1_000_001, 1_000_000));
        // The counts wrap: free again after the 2^32th pair, held with the acquire already wrapped.
        assert!(!lock_held(0, 0));
        assert!(lock_held(0, u32::MAX));
        assert!(!lock_held(u32::MAX, u32::MAX));
    }

    #[test]
    fn site_ids_are_dense_unique_and_named() {
        for (i, (id, name)) in site::ALL.iter().enumerate() {
            assert_eq!(*id as usize, i, "{name}");
            assert!(!name.is_empty());
        }
        let mut names: Vec<&str> = site::ALL.iter().map(|(_, n)| *n).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    /// Every `b"..."` literal in the Rust files under `root` (name, file).
    fn byte_literals(root: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
        let mut out = Vec::new();
        let mut stack = std::vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    let mut rest = text.as_str();
                    while let Some(i) = rest.find("b\"") {
                        // Not `rb"` / an identifier ending in b: the byte string must start a token.
                        let before = rest[..i].chars().last();
                        let tail = &rest[i + 2..];
                        let Some(end) = tail.find('"') else { break };
                        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            out.push((tail[..end].into(), p.clone()));
                        }
                        rest = &tail[end + 1..];
                    }
                }
            }
        }
        out
    }

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let mut names: Vec<String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for n in &names {
            // `record_named_bytes` clamps to 14 characters; a longer name would be truncated and
            // could merge with another.
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
        // Not in the other lists of this crate.
        for other in crate::foreign_flip::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        for other in crate::flip_completion::COUNTERS {
            assert!(!COUNTERS.contains(&other), "{other} collides");
        }
        // The two Fk names are listed where the Fk rules live.
        for fk in ["FkDefBud", "FkVenus"] {
            assert!(
                crate::flip_completion::COUNTERS.contains(&fk),
                "{fk} is not listed"
            );
        }
        let all_mine: Vec<&str> = COUNTERS
            .iter()
            .copied()
            .chain(["FkDefBud", "FkVenus"])
            .collect();

        // The sibling trees, when present (a copy of this crate without them scans nothing).
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let render = manifest.join("../kmd_render/src");
        let mut scanned = 0;
        if render.exists() {
            let lits = byte_literals(&render);
            assert!(lits.len() > 500, "scan found {} literals", lits.len());
            for (lit, file) in &lits {
                let in_writer = file.file_name().is_some_and(|n| n == "stall_diag.rs");
                let in_fk_writer = file.file_name().is_some_and(|n| n == "flip_keep.rs");
                for mine in &all_mine {
                    let fk = mine.starts_with("Fk");
                    if lit == mine {
                        assert!(
                            (fk && in_fk_writer) || (!fk && in_writer),
                            "{mine} is also written by {}",
                            file.display()
                        );
                    }
                    // A longer literal that the 14-character clamp truncates onto one of mine.
                    if lit.len() > 14 && lit[..14] == **mine {
                        panic!("{lit} in {} truncates onto {mine}", file.display());
                    }
                    // Mine are not a truncation of anything else either way round.
                    assert!(!(mine.len() > 14), "{mine} would be truncated");
                }
                scanned += 1;
            }
            // What the writer file spells is exactly the list.
            let mut spelled: Vec<String> = lits
                .iter()
                .filter(|(_, f)| f.file_name().is_some_and(|n| n == "stall_diag.rs"))
                .map(|(l, _)| l.clone())
                .collect();
            spelled.sort();
            spelled.dedup();
            let mut listed: Vec<String> = COUNTERS.iter().map(|s| (*s).into()).collect();
            listed.sort();
            assert_eq!(
                spelled, listed,
                "ddi/stall_diag.rs writes a different set than COUNTERS"
            );
        }
        // In this crate the names are quoted strings; only the lists above may spell them.
        let logic = manifest.join("src");
        let mut stack = std::vec![logic];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let file = p.file_name().unwrap().to_string_lossy().into_owned();
                    if file == "stall_diag.rs" || file == "flip_completion.rs" {
                        continue;
                    }
                    let text = std::fs::read_to_string(&p).unwrap();
                    for mine in COUNTERS {
                        assert!(
                            !text.contains(&std::format!("\"{mine}\"")),
                            "{file} spells the counter {mine}"
                        );
                    }
                    scanned += 1;
                }
            }
        }
        assert!(scanned > 20);
    }

    #[test]
    fn existing_counters_do_not_collide_with_mine() {
        // Every other literal in kmd_render, as the scan above, but the other way round: the
        // names this module adds are not equal to any dynamic or histogram name stem either.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let render = manifest.join("../kmd_render/src");
        if !render.exists() {
            return;
        }
        // Histogram dumps spell `<2 chars>R<d>`, `C<d>`, `Tot`, `Ovf`; the `Vp<hex>A..D` ring.
        let dynamic = |name: &str| -> bool {
            let b = name.as_bytes();
            let hist = b.len() >= 4
                && matches!(
                    &b[..2],
                    b"Vs" | b"Ff" | b"Fs" | b"Mk" | b"Df" | b"Pb" | b"Fl" | b"Fi"
                )
                && ((matches!(b[2], b'R' | b'C') && b.len() == 4 && b[3].is_ascii_digit())
                    || &b[2..] == b"Tot"
                    || &b[2..] == b"Ovf");
            let ring = b.len() == 4
                && &b[..2] == b"Vp"
                && b[2].is_ascii_hexdigit()
                && matches!(b[3], b'A'..=b'D');
            hist || ring
        };
        for mine in COUNTERS {
            assert!(
                !dynamic(mine),
                "{mine} looks like a dumped histogram or ring name"
            );
        }
    }
}
