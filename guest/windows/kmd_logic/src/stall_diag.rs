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
    /// `process_deferred_vidpn_source_address` AFTER the scanout mutex was released: the `VpDSt`
    /// registry write.
    pub const DEFERRED_POST: u32 = 18;
    /// `queue_active_scanout_refresh` AFTER the scanout mutex was released: the pacing snapshot
    /// (about 40 registry writes).
    pub const REFRESH_POST: u32 = 19;

    /// Every id with its name, for the doc and the host tests.
    pub const ALL: [(u32, &str); 20] = [
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
        (
            DEFERRED_POST,
            "process_deferred_vidpn_source_address (mutex released)",
        ),
        (
            REFRESH_POST,
            "queue_active_scanout_refresh (mutex released)",
        ),
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
/// * `VsTickN`, `VsOffN`: every vsync heartbeat tick, and those that ran with the CRTC_VSYNC
///   delivery gate closed (`ControlInterrupt` disable); `VsTickN - VsOffN` is `VpVsN`.
/// * `ScLkN`, `ScLkRelN`, `ScLkAcqT`, `ScLkRelT`: acquisitions and releases of the scanout mutex
///   (held now when they differ, [`lock_held`]) and the interrupt time of the last acquisition
///   and release.
/// * `StartN`, `StartT`: StartDevice generation count (since the image loaded) and its time.
/// * `FlipWd`, `FlipWdT`, `FlipWdBig`: watchdog publications, the time of the last, and flips it
///   could not record (an address above 2^40).
/// * `StallT`: interrupt time (ms) of the publication of this block: the "now" of every age.
/// * `FlWdMsEff`, `DefBudEff`: the `FlipWdogMs` and `DeferBudget` knobs in force (clamped, 0
///   included), written at every StartDevice.
/// * `HpdWkEvt`, `HpdWkTmo`: worker loops woken by an event / by a timeout; `HpdWkSrc`: the
///   `hpd_wake::cause` bits signalled since the previous loop, as of the last loop.
///   `HpdSgBlt`, `HpdSgRfr`, `HpdSgEdg`, `HpdSgFnc`, `HpdSgFs`, `HpdSgRel`, `HpdSgFlp`,
///   `HpdSgOth`: signals by cause (`HpdSgOth` is every caller not named); `HpdSgCoal`: windowed-
///   Blt wakes not signalled because one was already owed.
/// * `HpdWait`, `HpdWaitMin`: the last timed wait and the shortest one, in microseconds (0 =
///   infinite / none yet); `HpdTmCtl`, `HpdTmRty`, `HpdTmDue`, `HpdTmNone`: waits by class
///   (control poll, refresh retry, a due time, none).
/// * `HpdBusyUs`, `HpdPassMaxUs`: microseconds the worker spent awake (all passes) and the longest
///   pass. `HpdDumpN`, `HpdDumpUs`, `HpdDumpSkip`: periodic `Vp*` dumps run, their total time in
///   microseconds, and the wake counts that reached the loop cadence but not the 1 s one.
/// * `VsTickT`: interrupt time (ms) of the last heartbeat tick, written beside `VsTickN` (a
///   frozen count with a moving `StallT` and a stale `VsTickT` is a dead chain, not a stale
///   mirror); `VsGapMaxMs`: the longest silence between two ticks, ms; `VsArmN`, `VsDisN`,
///   `VsCanN`: effective arms, effective disarms, and cancels of the one-shot; `VsEarlyN`: ticks
///   that returned before the count (disarmed or no display half); `VsExhN`: the deadline
///   exhausted; `VsRevN`: heartbeats the watchdog found dead and re-armed.
/// * `PwrN`, `PwrUid`, `PwrD3N`: `DxgkDdiSetPowerState` calls, the last one's `DeviceUid`
///   (0xFFFFFFFF = the adapter), and those that were not D0.
/// * `PwrT`: interrupt time (ms) of the last `DxgkDdiSetPowerState`; `PwrAdSt`, `PwrChSt`: the
///   adapter's and the monitor child's last state (1 = D0, 0 = not D0, 0xFF = no call yet);
///   `PwrStg`: how far the last power call got (1 entered, 3 done).
///   `VsCiT`, `VsCiSt`: time and argument (1 enable, 0 disable) of the last
///   `ControlInterrupt(CRTC_VSYNC)`. A child at 0 with `VsCiSt` 0 is the monitor asleep: DWM
///   presents nothing, no vsync is wanted and the worker idles; that is not a stall.
/// * `LkWaitN`, `LkWaitWh`, `LkWaitT`, `LkWaitMs`: waits on the venus mutex (1), the scanout mutex
///   (2) or the content mutex (3) that outlived one 5 s slice (they still wait, unchanged):
///   count, which one last, when, and the longest wait seen so far, ms. All 0 on a healthy run.
/// * `StopSub`, `StopSubT`: the finest step `DxgkDdiStopDevice` / `RemoveDevice` reached
///   ([`stop_sub`]) and when; written BEFORE the step runs, so a hang names the step it is in.
/// * Every value of the block written by `publish_counters` (`HpdLoopN`, `HpdSite`, `StallT` ...)
///   is a SNAPSHOT as of `StallT`; only what the periodic dump writes itself (`VpDmpT`, `VsTickT`,
///   `HpdWk*`, `HpdWait`, ...) is live as of `VpDmpT`. See [`snapshot_is_stale`].
/// * `HpdLongSite`, `HpdLongUs`, `HpdLongT`, `HpdLongInfl`: the STEP (a [`site`] id) of the worker
///   that held it longest in one go, for how many microseconds, when it ended (interrupt ms),
///   and the DDIs in flight then (`device_lost` ids 0..32 as a bitmask). `HpdStep100N`: steps of
///   100 ms or more. `HpdPass100N`, `HpdPass500N`: whole passes of 100 ms / 500 ms or more.
/// * `VsGap100N`, `VsGap1000N`: silences of the vsync heartbeat of 100 ms / 1 s or more.
///   `VsGapT`, `VsGapSite`, `VsGapFlg`, `VsGapInfl`: for the longest (`VsGapMaxMs`): when it
///   ended, the worker's `HpdSite` then, flags (bit 0 scanout mutex held, 1 Venus mutex held, 2
///   worker idle in its wait, 3 programming pending), the DDIs in flight (ids 0..32).
/// * `VsSnapA`, `VsSnapB` (REG_QWORD, 15.18.16): the heartbeat's tick count, resp. the period slots
///   it moved over, in the high 32 bits and the interrupt time (ms) of the tick that made them in
///   the low 32: one registry value each, so a reader gets a count with ITS time, from the one
///   seqlock'd sample the mirror took (`vsync_snap`). `VsSlotN`, `VsSkipN`, `VsCatchN`: the slots
///   moved over (nominal rate), those dropped because a callback ran a period or more late, and
///   those served by a catch-up tick (`VsCatchUp`, `VsCatchEff` in force); `VsSnapMiss`: samples
///   that met a write in flight. `VsCbMaxUs`, `VsCbOvN`: the longest tick callback and those that
///   took a whole period; `VsExTm`: 1 = the high-resolution Ex timer drives the heartbeat.
///   `HpdOv4N`, `HpdOv4Mask`: worker steps over 4 ms and the step ids (bit = `site` id) that had
///   one; `HpdDumpDef`: inline dumps held back by a flip in the worker's hands.
/// * `VsLiveT`: interrupt time (ms) the heartbeat block (`VsTickN` ... `VsWd*`) was last written. Every
///   value of that block is a snapshot as of `VsLiveT`: compare `VsTickT` with `VsLiveT`, and
///   `VsLiveT` with the uptime, before calling a heartbeat dead. The watchdog timer asks the worker
///   to rewrite ten of its values every 2 s (the rest only after it acted, at most once per 2 s).
/// * `VsCbIn`, `VsCbOut`: tick callbacks entered and returned (never zeroed; `VsCbIn` above
///   `VsCbOut` for longer than a tick is a blocked callback). `VsCbSyncB`, `VsCbSyncOk`,
///   `VsCbSyncSt`, `VsCbSyncT`: `DxgkCbSynchronizeExecution` calls the tick began and returned,
///   the last status, and when the last began (ms).
/// * `VsWdTkN`, `VsWdTkT`, `VsWdAgeMs`, `VsWdFixN`, `VsWdHungN`, `VsWdPubN`, `VsWdOn`,
///   `VsWdNoTm`, `VsWdTmEff`: the independent watchdog timer: its ticks and the time of the last,
///   the heartbeat's silence it saw at the last, re-arms it did, ticks that found a blocked
///   callback, mirror refreshes it asked for, armed (1), no timer could be allocated, the
///   `VsWdTimer` knob in force. `VsWdSAt`, `VsWdSArm`, `VsWdSRef`, `VsWdSDl`, `VsWdSAge`,
///   `VsWdSCbI`, `VsWdSCbO`, `VsWdSSyT`: what it saw the last time it acted (when, armed, the
///   reference and deadline in ms, the silence, the callback counts, when the last synchronized
///   call began).
pub const COUNTERS: [&str; 146] = [
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
    "VsTickN",
    "VsOffN",
    "StartN",
    "StartT",
    "FlipWd",
    "FlipWdT",
    "FlipWdBig",
    "StallT",
    "FlWdMsEff",
    "DefBudEff",
    // T5 anomalies (`hpd_wake`, docs/kmd-rm-client.md 15.18.14): the worker's wakes, its wait,
    // its periodic dump, the signals by cause, and the vsync heartbeat's life.
    "HpdWkEvt",
    "HpdWkTmo",
    "HpdWkSrc",
    "HpdWait",
    "HpdWaitMin",
    "HpdTmCtl",
    "HpdTmRty",
    "HpdTmDue",
    "HpdTmNone",
    "HpdBusyUs",
    "HpdPassMaxUs",
    "HpdDumpN",
    "HpdDumpUs",
    "HpdDumpSkip",
    "HpdSgBlt",
    "HpdSgRfr",
    "HpdSgEdg",
    "HpdSgFnc",
    "HpdSgFs",
    "HpdSgRel",
    "HpdSgFlp",
    "HpdSgOth",
    "HpdSgCoal",
    "VsTickT",
    "VsGapMaxMs",
    "VsArmN",
    "VsDisN",
    "VsCanN",
    "VsEarlyN",
    "VsExhN",
    "VsRevN",
    "PwrN",
    "PwrUid",
    "PwrD3N",
    // v327 (docs/zero-copy-present.md, the "mode lost after a device restart" incident): the
    // knobs in force, what the previous generation left at StartDevice entry, the worker's phase
    // and the mode-set path's last step.
    "VsPwrEff",
    "VsWdgEff",
    "VsIdlEff",
    "EntD0",
    "EntRef",
    "EntArm",
    "EntVsEn",
    "EntHpdTh",
    "EntHpdN",
    "EntVsTk",
    "HpdPhase",
    "HpdPhaseT",
    "HpdFirstT",
    "ModeStg",
    "ModeStgT",
    "ModeN",
    "ModeSt",
    // v328 (docs/zero-copy-present.md, "The v327 incident"): the display's power history, which
    // counters of the block are snapshots, the waits that outlived their slice, and how far a
    // power transition or a stop got.
    "PwrT",
    "PwrAdSt",
    "PwrChSt",
    "VsCiT",
    "VsCiSt",
    "LkWaitN",
    "LkWaitWh",
    "LkWaitT",
    "LkWaitMs",
    "StopSub",
    "StopSubT",
    "PwrStg",
    // The "Adapter-wide device removed" incident (docs/zero-copy-present.md): the longest worker
    // STEP and the longest vsync silence, each with the context it ended in.
    "HpdLongSite",
    "HpdLongUs",
    "HpdLongT",
    "HpdLongInfl",
    "HpdStep100N",
    "HpdPass100N",
    "HpdPass500N",
    "VsGap100N",
    "VsGap1000N",
    "VsGapT",
    "VsGapSite",
    "VsGapFlg",
    "VsGapInfl",
    // v329 (docs/zero-copy-present.md, "Heartbeat stops after (re)start"): when the heartbeat
    // block was written, the tick callback breadcrumbs, the independent watchdog timer.
    "VsLiveT",
    "VsCbIn",
    "VsCbOut",
    "VsCbSyncB",
    "VsCbSyncOk",
    "VsCbSyncSt",
    "VsCbSyncT",
    "VsWdTkN",
    "VsWdTkT",
    "VsWdAgeMs",
    "VsWdFixN",
    "VsWdHungN",
    "VsWdPubN",
    "VsWdOn",
    "VsWdNoTm",
    "VsWdSAt",
    "VsWdSArm",
    "VsWdSRef",
    "VsWdSDl",
    "VsWdSAge",
    "VsWdSCbI",
    "VsWdSCbO",
    "VsWdSSyT",
    "VsWdTmEff",
    // Flip retirement across a device restart (`restart_flip`, docs/zero-copy-present.md "DWM
    // after a device restart"): the programming state found at the two edges, the heartbeat's
    // address at StopDevice entry and at StartDevice exit, the newest address dxgkrnl issued,
    // their high bytes, and the worker wake StartDevice owed.
    "ScRestPend",
    "ScRestAdr0",
    "ScRestAddr",
    "ScRestIss",
    "ScRestHi",
    "ScRestSig",
    // The restart seed that survives an image reload (`restart_flip::choose_seed`): the knob in
    // force, the persisted address as read at StartDevice (low and high dword), and why it was or
    // was not used. The persisted words themselves (`RestIssLo`, ...) are state, not counters:
    // they are spelled once, in `restart_flip`.
    "RestSeedEff",
    "RestSeedLo",
    "RestSeedHi",
    "RestSeedUse",
    // 15.18.16 (docs/kmd-rm-client.md): the heartbeat's exact (count, time) pair and the slots it
    // moved over, what the tick callback itself costs, which timer drives it, the worker steps over
    // 4 ms, and the inline dumps held back by a flip.
    "VsSnapA",
    "VsSnapB",
    "VsSnapMiss",
    "VsSlotN",
    "VsSkipN",
    "VsCatchN",
    "VsCatchEff",
    "VsCbMaxUs",
    "VsCbOvN",
    "VsExTm",
    "HpdOv4N",
    "HpdOv4Mask",
    "HpdDumpDef",
];

/// A worker step longer than this (one 240 Hz period, microseconds) is over budget: a flip that
/// arrives while the worker is inside it waits a whole tick or more.
pub const STEP_BUDGET_US: u32 = 4_000;

/// Whether a worker step that lasted `us` microseconds is over [`STEP_BUDGET_US`].
pub const fn step_over_budget(us: u32) -> bool {
    us >= STEP_BUDGET_US
}

/// The bit of `HpdOv4Mask` for step id `site` (ids above 31 share bit 31).
pub const fn step_mask_bit(site: u32) -> u32 {
    1u32 << if site > 31 { 31 } else { site }
}

// ---- the scanout mutex -----------------------------------------------------------------------

/// Whether the scanout mutex is held, from the acquisition and release COUNTS (`ScLkN`,
/// `ScLkRelN`). The mutex serializes its holders, so the counts alternate: they are equal when it
/// is free and differ by one while held. Counts, not the millisecond stamps: an acquisition and a
/// release in the same millisecond are indistinguishable by time, and the wrapping 32-bit counts
/// compare exactly.
pub const fn lock_held(acquired: u32, released: u32) -> bool {
    acquired != released
}

// ---- does the worker look stuck? ----------------------------------------------------------

/// A step the worker has been in for longer than this (and that is not the idle wait) looks stuck.
pub const STUCK_SITE_MS: u32 = 1_000;
/// A scanout mutex held for longer than this looks stuck.
pub const STUCK_LOCK_MS: u32 = 1_000;
/// A worker that has not woken for longer than this while work is pending looks stuck (a pending
/// programming wakes it on every vsync tick, so a healthy worker's last wake is a few ticks old).
pub const STUCK_LOOP_MS: u32 = 2_000;

/// Milliseconds from `then` to `now` on the wrapping 32-bit interrupt-time clock. A `then` that
/// is AHEAD of `now` (a stamp from before a reset, or read a hair after `now`) is age 0, never a
/// 49-day age.
pub const fn age_ms(now: u32, then: u32) -> u32 {
    let d = now.wrapping_sub(then);
    if d >= 0x8000_0000 {
        0
    } else {
        d
    }
}

/// What the escape thread can see of the worker, all from atomics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StuckInput {
    /// Interrupt time now, ms.
    pub now: u32,
    /// `HpdSite` and `HpdSiteT`.
    pub site: u32,
    pub site_t: u32,
    /// The scanout mutex is held ([`lock_held`]) and `ScLkAcqT`.
    pub lock_held: bool,
    pub lock_acq_t: u32,
    /// `HpdLoopT`.
    pub loop_t: u32,
    /// A programming is pending (`pending_vidpn_allocation != 0` or the gate is raised).
    pub work_pending: bool,
}

/// Whether the HPD worker LOOKS stuck, so that the escape thread should write the stall block
/// (and otherwise write nothing: the registry is not free). Any of:
/// * it is in a step other than the idle wait (and not "never ran" or "exited") for more than
///   [`STUCK_SITE_MS`];
/// * the scanout mutex has been held for more than [`STUCK_LOCK_MS`] (by anyone);
/// * work is pending and it has not woken for more than [`STUCK_LOOP_MS`].
///
/// A healthy idle worker (asleep in `WAIT`, nothing pending, the mutex free) is never stuck, however
/// old its stamps are. A false positive costs one block write per interval; a false negative is
/// the instrument missing the stall, so the thresholds are short.
pub const fn worker_looks_stuck(i: StuckInput) -> bool {
    let in_step = i.site != site::WAIT && i.site != site::NONE && i.site != site::EXITED;
    if in_step && age_ms(i.now, i.site_t) > STUCK_SITE_MS {
        return true;
    }
    if i.lock_held && age_ms(i.now, i.lock_acq_t) > STUCK_LOCK_MS {
        return true;
    }
    i.work_pending && age_ms(i.now, i.loop_t) > STUCK_LOOP_MS
}

/// The stall block is refreshed from the escape thread at least this often (ms) even when the
/// worker does not look stuck, so a reader never meets a snapshot older than this plus one escape.
pub const STALE_PUBLISH_MS: u32 = 5_000;
/// The least gap between two escape-thread publications.
pub const ESCAPE_PUBLISH_MS: u32 = 500;

/// Whether the escape thread writes the stall block now: never twice within
/// [`ESCAPE_PUBLISH_MS`] (`last_escape`, 0 = never), and then either the worker looks stuck or
/// the block's last write by anyone (`last_publish`, `StallT`; 0 = never) is at least
/// [`STALE_PUBLISH_MS`] old. The second arm keeps `HpdSite` / `HpdLoopT` of an idle worker from
/// being read as live (the v327 incident: `HpdSite` 11 and `HpdLoopT` frozen at a snapshot
/// taken by the worker itself, the worker asleep for the next 300 s).
pub const fn escape_publish_due(stuck: bool, now: u32, last_escape: u32, last_publish: u32) -> bool {
    if last_escape != 0 && age_ms(now, last_escape) < ESCAPE_PUBLISH_MS {
        return false;
    }
    stuck || last_publish == 0 || age_ms(now, last_publish) >= STALE_PUBLISH_MS
}

/// Whether `HpdLoopT` / `HpdSite` must be read as a snapshot: the block's `StallT` is older than
/// the live source of the same read (`VpDmpT` or `VsTickT`, `reading_ms`) by more than
/// [`STALE_PUBLISH_MS`].
pub const fn snapshot_is_stale(stall_t: u32, reading_ms: u32) -> bool {
    reading_ms.wrapping_sub(stall_t) as i32 > STALE_PUBLISH_MS as i32
}

/// A wait that used to be infinite is made of slices this long (100 ns, relative); after each
/// slice that expired the waiter counts it and waits again, so the semantics are unchanged and a
/// holder that never lets go becomes visible (`LkWait*`) instead of silent.
pub const LONG_WAIT_SLICE_100NS: i64 = -50_000_000;
/// The same slice in milliseconds, for the `LkWaitMs` arithmetic.
pub const LONG_WAIT_SLICE_MS: u32 = 5_000;

/// Which lock a long wait was on (`LkWaitWh`).
pub mod lock {
    pub const VENUS: u32 = 1;
    pub const SCANOUT: u32 = 2;
    pub const CONTENT: u32 = 3;
}

/// The wait time after `slices` expired slices, ms, saturating.
pub const fn long_wait_ms(slices: u32) -> u32 {
    slices.saturating_mul(LONG_WAIT_SLICE_MS)
}

/// Finest steps of `DxgkDdiStopDevice` and `RemoveDevice` (`StopSub`), entered in this order.
pub mod stop_sub {
    pub const ENTER: u32 = 1;
    pub const FLUSH_FIRST: u32 = 2;
    pub const ISR_CLEARED: u32 = 3;
    pub const VSYNC_STOP: u32 = 4;
    pub const HPD_STOP: u32 = 5;
    pub const HPD_STOPPED: u32 = 6;
    pub const FINAL_PUBLISH: u32 = 7;
    pub const RESET_PUBLICATION: u32 = 8;
    pub const VENUS_CLIENT_DROP: u32 = 9;
    pub const BLOB_SWEEP: u32 = 10;
    pub const CTX_DESTROY: u32 = 11;
    pub const REAP_PARKED: u32 = 12;
    pub const HOST_SWEEP: u32 = 13;
    pub const RETIRE_TRANSPORT: u32 = 14;
    pub const SYSTEM_BACKINGS: u32 = 15;
    pub const FLUSH_LAST: u32 = 16;
    pub const DONE: u32 = 17;
    pub const REMOVE_ENTER: u32 = 20;
    pub const REMOVE_DROP: u32 = 21;
    pub const REMOVE_DONE: u32 = 22;
    /// Written INSIDE `REMOVE_DROP`..`REMOVE_DONE` (so it follows 21 and precedes 22 in time), just
    /// before `ExDeleteTimer(wait)` of the heartbeat and watchdog timers: a callback blocked in
    /// `DxgkCbSynchronizeExecution` hangs that wait, and `StopSub` 23 names it.
    pub const REMOVE_TIMER: u32 = 23;
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

/// The 24-bit flip number a packed word carries.
pub const fn flip_seq(word: u64) -> u32 {
    ((word >> FLIP_ADDR_BITS) as u32) & FLIP_SEQ_MASK
}

/// Whether flip number `a` is NEWER than `b` on the wrapping 24-bit numbering: strictly ahead by
/// less than half the range. Equal is not newer.
pub const fn seq_newer(a: u32, b: u32) -> bool {
    let d = a.wrapping_sub(b) & FLIP_SEQ_MASK;
    d != 0 && d < (FLIP_SEQ_MASK + 1) / 2
}

/// A publication of `published` happened: does it complete the recorded flip? When it names the
/// newest recorded flip's address and that flip is newer than the last one done, the flip is done:
/// returns its number, for the driver to store. Any publisher goes through this (the worker's
/// bind, a kept publication of any lane, the ring-1 completion), so the watchdog can never later
/// publish the address of a flip that something newer already replaced or completed. A
/// publication of some other address (an older flip's programming finishing while a newer flip is
/// recorded) completes nothing.
pub const fn flip_done_by(published: u64, flip_word: u64, done_seq: u32) -> Option<u32> {
    if flip_word == 0 || (published & FLIP_ADDR_MASK) != flip_address(flip_word) {
        return None;
    }
    let seq = flip_seq(flip_word);
    if seq_newer(seq, done_seq) {
        Some(seq)
    } else {
        None
    }
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
    /// The number of the newest flip that is DONE: published by anyone (the watchdog included).
    /// The watchdog only ever publishes a flip newer than this, so it never publishes an older
    /// address than what was displayed last, and never the same flip twice.
    pub done_seq: u32,
    /// The watchdog interval in ticks ([`ticks_for_ms`]), 0 = off.
    pub limit_ticks: u32,
}

/// What the tick must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdAction {
    /// Nothing.
    None,
    /// Publish this address as the displayed one (a kept picture) and mark the flip done
    /// ([`flip_seq`] of the word that was read). Once per flip.
    Publish(u64),
}

/// One vsync tick of bookkeeping: the pending run, its maximum, the no-publication clock, and the
/// watchdog decision. Total, atomics-sized, no allocation: it runs in the DPC.
///
/// The watchdog fires when it is on (`limit_ticks != 0`), a flip is recorded and NEWER than the
/// last one done (`done_seq`), and the pending run has gone MORE than `limit_ticks` ticks with no publication since (every
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
        && seq_newer(flip_seq(i.flip_word), i.done_seq)
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

/// The Deferred budget's whole state: the handle being deferred and how many consecutive
/// Deferred outcomes it has had. `EMPTY` (also `default()`) is "no count in progress".
///
/// The driver keeps these two words in atomics (`ddi/stall_diag.rs`) and calls [`Self::note`]
/// for every Deferred outcome and clears the state at EVERY other outcome of the deferred
/// wrapper (programmed, copy queued, superseded, a retryable refusal re-armed or given up, a
/// permanent reject): the count is of CONSECUTIVE Deferred outcomes of one handle, so a later
/// Deferred of the same handle never continues an old count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeferState {
    pub handle: usize,
    pub attempts: u32,
}

impl DeferState {
    /// No count in progress.
    pub const EMPTY: Self = Self {
        handle: 0,
        attempts: 0,
    };

    /// One more Deferred outcome for `handle` under `budget`. `budget` 0 (unlimited) touches
    /// nothing. A different handle starts at 1; an `Exhausted` answer forgets the state.
    pub const fn note(self, handle: usize, budget: u32) -> (Self, DeferDecision) {
        if budget == 0 {
            return (self, DeferDecision::Again);
        }
        let attempts = defer_attempts(self.handle, self.attempts, handle);
        match defer_decide(attempts, budget) {
            DeferDecision::Again => (Self { handle, attempts }, DeferDecision::Again),
            DeferDecision::Exhausted => (Self::EMPTY, DeferDecision::Exhausted),
        }
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
            // The driver stores the flip as done once it published.
            done_seq: 0,
            limit_ticks: limit,
            ..PendInput::default()
        });
        // Fires on the tick that EXCEEDS the interval (the clock reads limit + 1), and, because
        // the test never stores the done number, again each limit + 1 ticks: the driver's store of
        // it is what makes it once.
        assert_eq!(acts[0], (limit + 1, WdAction::Publish(0x4000)));
        assert_eq!(acts[1].0, 2 * (limit + 1));
        assert!(s.pend == 500);
        // With the done number stored (what the driver does) it never repeats.
        let (_, acts) = run(PendState::default(), 5_000, |_| PendInput {
            pending: true,
            flip_word: w,
            done_seq: flip_seq(w),
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
        let mut fired = 0u32;
        let mut fires = Vec::new();
        for _ in 0..40 {
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    flip_word: w1,
                    done_seq: fired,
                    limit_ticks: limit,
                    ..Default::default()
                },
            );
            s = n;
            if let WdAction::Publish(addr) = a {
                fired = flip_seq(w1);
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
                    done_seq: fired,
                    limit_ticks: limit,
                    ..Default::default()
                },
            );
            s = n;
            if let WdAction::Publish(addr) = a {
                fired = flip_seq(w2);
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
        let mut fired = 0u32;
        let mut fire_ticks = Vec::new();
        for t in 1..=200u32 {
            let flip = if t < 100 { w1 } else { word(2, 0x2000) };
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    pub_count: pubs,
                    flip_word: flip,
                    done_seq: fired,
                    limit_ticks: limit,
                },
            );
            s = n;
            if let WdAction::Publish(_) = a {
                fired = flip_seq(flip);
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

    // ---- done numbers: a watchdog never publishes an older address ---------------------------

    #[test]
    fn sequence_order_wraps_in_24_bits() {
        assert!(seq_newer(1, 0));
        assert!(!seq_newer(0, 0));
        assert!(!seq_newer(0, 1));
        assert!(seq_newer(0, 0xFF_FFFF), "0 follows 0xFFFFFF");
        assert!(!seq_newer(0xFF_FFFF, 0));
        assert!(seq_newer(0x7F_FFFF, 0));
        assert!(
            !seq_newer(0x80_0000, 0),
            "half the range ahead is not newer"
        );
        assert_eq!(flip_seq(word(0x12_3456, 0x1000)), 0x12_3456);
        assert_eq!(flip_seq(0), 0);
    }

    #[test]
    fn a_publication_of_the_recorded_address_completes_that_flip_only() {
        let w = word(5, 0x4000);
        assert_eq!(flip_done_by(0x4000, w, 4), Some(5));
        assert_eq!(flip_done_by(0x4000, w, 5), None, "already done");
        assert_eq!(flip_done_by(0x4000, w, 9), None, "something newer is done");
        // Another address (an older flip's programming finishing): completes nothing.
        assert_eq!(flip_done_by(0x3000, w, 0), None);
        // Nothing recorded.
        assert_eq!(flip_done_by(0x4000, 0, 0), None);
        // A published address wider than the word carries compares on its low 40 bits only.
        assert_eq!(flip_done_by((1 << 41) | 0x4000, w, 4), Some(5));
    }

    #[test]
    fn the_watchdog_never_publishes_an_older_flip_than_one_already_done() {
        // Flip 5 (address A) is stuck pending. Flip 6 (address B) is then issued and completed
        // by a direct publisher (a keep: FkDdi, the DMA keep record, ForeignFlip): the word now
        // names 6 and the publication marked it done. The pipeline stays stuck on flip 5's gate.
        let limit = 10;
        let w5 = word(5, 0xA000);
        let w6 = word(6, 0xB000);
        let mut done = 4u32;
        let mut s = PendState::default();
        let mut published = std::vec::Vec::new();
        for t in 1..=200u32 {
            let flip = if t < 5 { w5 } else { w6 };
            if t == 5 {
                // flip 6 published directly
                done = flip_done_by(0xB000, w6, done).unwrap();
                published.push(0xB000u64);
            }
            let (n, a) = pend_step(
                s,
                PendInput {
                    pending: true,
                    pub_count: if t < 5 { 0 } else { 1 },
                    flip_word: flip,
                    done_seq: done,
                    limit_ticks: limit,
                },
            );
            s = n;
            if let WdAction::Publish(addr) = a {
                published.push(addr);
            }
        }
        assert_eq!(
            published,
            std::vec![0xB000],
            "flip 5's address must never be published after flip 6's"
        );
    }

    #[test]
    fn an_older_flip_recorded_after_a_newer_one_done_is_never_published() {
        // The numbers race: the older flip's record lands last (two issuing contexts).
        let (w6, w5) = (word(6, 0xB000), word(5, 0xA000));
        let (_, acts) = run(PendState::default(), 500, |_| PendInput {
            pending: true,
            flip_word: w5,
            done_seq: flip_seq(w6),
            limit_ticks: 5,
            ..PendInput::default()
        });
        assert!(acts.is_empty());
    }

    #[test]
    fn the_watchdog_fires_for_a_newer_flip_across_the_24_bit_wrap() {
        let w = word(0, 0x4000); // 0 follows 0xFFFFFF
        let (_, acts) = run(PendState::default(), 50, |_| PendInput {
            pending: true,
            flip_word: w,
            done_seq: 0xFF_FFFF,
            limit_ticks: 5,
            ..PendInput::default()
        });
        assert_eq!(acts.len() >= 1, true);
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

    /// What the deferred wrapper does with the state, as a model: Deferred notes it, every
    /// other outcome clears it.
    #[derive(Clone, Copy)]
    enum Outcome {
        Deferred(usize),
        /// Programmed, copy queued, superseded, a retryable refusal (re-armed or given up), a
        /// permanent reject: anything but Deferred.
        Other,
    }

    fn drive(budget: u32, outcomes: &[Outcome]) -> Vec<DeferDecision> {
        let mut st = DeferState::EMPTY;
        let mut out = Vec::new();
        for o in outcomes {
            match *o {
                Outcome::Deferred(h) => {
                    let (n, d) = st.note(h, budget);
                    st = n;
                    out.push(d);
                }
                Outcome::Other => st = DeferState::EMPTY,
            }
        }
        out
    }

    #[test]
    fn a_later_deferred_of_the_same_handle_does_not_continue_an_old_count() {
        let budget = 20;
        // 15 Deferred, a refusal in between (re-armed or given up), 15 more: no exhaustion; with
        // the count continuing it would have run out at the 21st.
        let mut seq = std::vec::Vec::new();
        seq.extend(std::iter::repeat(Outcome::Deferred(7)).take(15));
        seq.push(Outcome::Other);
        seq.extend(std::iter::repeat(Outcome::Deferred(7)).take(15));
        assert!(drive(budget, &seq)
            .iter()
            .all(|d| *d == DeferDecision::Again));
        // Without the clear (the bug the follow-up fixes) the same sequence exhausts.
        let mut st = DeferState::EMPTY;
        let mut exhausted = false;
        for _ in 0..30 {
            let (n, d) = st.note(7, budget);
            st = n;
            exhausted |= d == DeferDecision::Exhausted;
        }
        assert!(exhausted);
        // Consecutive Deferred of one handle exhaust after exactly the budget.
        let run = drive(budget, &std::vec![Outcome::Deferred(7); 25]);
        assert!(run[..20].iter().all(|d| *d == DeferDecision::Again));
        assert_eq!(run[20], DeferDecision::Exhausted);
        // ... and after exhaustion the state is empty: the next Deferred starts from 1.
        assert_eq!(run[21], DeferDecision::Again);
    }

    #[test]
    fn a_handle_change_restarts_the_count_and_the_old_one_does_not_come_back() {
        let budget = 16;
        let mut seq = std::vec::Vec::new();
        seq.extend(std::iter::repeat(Outcome::Deferred(1)).take(10));
        seq.extend(std::iter::repeat(Outcome::Deferred(2)).take(10));
        // Back to handle 1: 10 more. Neither handle ever has 16 in a row.
        seq.extend(std::iter::repeat(Outcome::Deferred(1)).take(10));
        assert!(drive(budget, &seq)
            .iter()
            .all(|d| *d == DeferDecision::Again));
    }

    #[test]
    fn budget_zero_never_changes_the_state() {
        let st = DeferState {
            handle: 9,
            attempts: 5,
        };
        assert_eq!(st.note(9, 0), (st, DeferDecision::Again));
        assert_eq!(st.note(3, 0), (st, DeferDecision::Again));
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

    // ---- does the worker look stuck ------------------------------------------------------

    fn healthy() -> StuckInput {
        StuckInput {
            now: 1_000_000,
            site: site::WAIT,
            site_t: 1_000_000 - 50_000,
            lock_held: false,
            lock_acq_t: 1_000_000 - 50_000,
            loop_t: 1_000_000 - 50_000,
            work_pending: false,
        }
    }

    #[test]
    fn a_healthy_idle_worker_is_never_stuck_however_old_its_stamps() {
        assert!(!worker_looks_stuck(healthy()));
        // Hours idle: still healthy.
        let mut i = healthy();
        i.site_t = 0;
        i.loop_t = 0;
        i.lock_acq_t = 0;
        i.now = 40_000_000;
        assert!(!worker_looks_stuck(i));
        // Never ran / exited: not stuck either, at any age.
        for s in [site::NONE, site::EXITED] {
            let mut i = healthy();
            i.site = s;
            i.site_t = 0;
            assert!(!worker_looks_stuck(i), "site {s}");
        }
    }

    #[test]
    fn busy_but_moving_is_not_stuck() {
        // In a step for 40 ms, woke 16 ms ago, work pending, mutex held for 30 ms.
        let i = StuckInput {
            now: 5_000,
            site: site::DEFERRED_LOCKED,
            site_t: 4_960,
            lock_held: true,
            lock_acq_t: 4_970,
            loop_t: 4_984,
            work_pending: true,
        };
        assert!(!worker_looks_stuck(i));
    }

    #[test]
    fn stuck_in_any_step_after_a_second() {
        for (id, name) in site::ALL {
            if id == site::WAIT || id == site::NONE || id == site::EXITED {
                continue;
            }
            let mut i = healthy();
            i.site = id;
            i.site_t = i.now - STUCK_SITE_MS;
            assert!(
                !worker_looks_stuck(i),
                "{name}: exactly the threshold is not yet"
            );
            i.site_t = i.now - STUCK_SITE_MS - 1;
            assert!(worker_looks_stuck(i), "{name}");
        }
    }

    #[test]
    fn a_held_mutex_is_stuck_after_a_second_and_a_free_one_never() {
        let mut i = healthy();
        i.lock_held = true;
        i.lock_acq_t = i.now - STUCK_LOCK_MS;
        assert!(!worker_looks_stuck(i));
        i.lock_acq_t = i.now - STUCK_LOCK_MS - 1;
        assert!(worker_looks_stuck(i));
        // Free, however old the acquisition stamp: not stuck.
        i.lock_held = false;
        assert!(!worker_looks_stuck(i));
    }

    #[test]
    fn an_old_loop_stamp_is_stuck_only_with_work_pending() {
        let mut i = healthy();
        i.loop_t = i.now - STUCK_LOOP_MS - 1;
        assert!(
            !worker_looks_stuck(i),
            "nothing pending: an idle worker has an old loop stamp"
        );
        i.work_pending = true;
        assert!(worker_looks_stuck(i));
        i.loop_t = i.now - STUCK_LOOP_MS;
        assert!(!worker_looks_stuck(i));
    }

    #[test]
    fn ages_survive_the_clock_wrapping() {
        // now just after the 2^32 ms wrap, stamps just before it.
        let mut i = healthy();
        i.now = 500;
        i.site = site::FOREIGN_FENCE;
        i.site_t = u32::MAX - 999; // 1 500 ms ago
        assert_eq!(age_ms(i.now, i.site_t), 1_500);
        assert!(worker_looks_stuck(i));
        i.site_t = u32::MAX - 100; // 601 ms ago
        assert!(!worker_looks_stuck(i));
        // A stamp ahead of now (stale after a reset, or racing the clock read) is age 0.
        assert_eq!(age_ms(10, 20), 0);
        assert_eq!(age_ms(10, 10), 0);
        i.site_t = i.now + 5;
        assert!(!worker_looks_stuck(i));
        // The loop and lock stamps wrap the same way.
        let mut j = healthy();
        j.now = 100;
        j.work_pending = true;
        j.loop_t = u32::MAX - 2_999;
        assert!(worker_looks_stuck(j));
        j.loop_t = 0; // woke at the epoch of the clock, 100 ms ago
        assert!(!worker_looks_stuck(j));
        j.lock_held = true;
        j.lock_acq_t = u32::MAX - 1_999;
        assert!(worker_looks_stuck(j));
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
    fn escape_publish_is_rate_limited_and_refreshes_a_stale_block() {
        assert!(escape_publish_due(true, 10_000, 0, 9_999));
        assert!(!escape_publish_due(true, 10_000, 9_800, 9_999));
        assert!(escape_publish_due(true, 10_000, 9_400, 9_999));
        assert!(!escape_publish_due(false, 10_000, 0, 9_000));
        assert!(escape_publish_due(false, 10_000, 0, 5_000));
        assert!(!escape_publish_due(false, 10_000, 9_800, 5_000));
        assert!(escape_publish_due(false, 10_000, 0, 0));
        // The incident: a block written at 824619, an escape at 1129213.
        assert!(escape_publish_due(false, 1_129_213, 0, 824_619));
        assert!(snapshot_is_stale(824_619, 1_129_213));
        assert!(!snapshot_is_stale(1_129_000, 1_129_213));
        // A stamp ahead of the reading is never stale.
        assert!(!snapshot_is_stale(1_129_300, 1_129_213));
        // Wrapping clock.
        assert!(escape_publish_due(false, 3, 0, u32::MAX - 9_000));
        assert!(!escape_publish_due(false, 3, 0, u32::MAX - 100));
    }

    #[test]
    fn long_wait_slices() {
        assert_eq!(LONG_WAIT_SLICE_100NS, -(LONG_WAIT_SLICE_MS as i64) * 10_000);
        assert_eq!(long_wait_ms(0), 0);
        assert_eq!(long_wait_ms(3), 15_000);
        assert_eq!(long_wait_ms(u32::MAX), u32::MAX);
    }

    #[test]
    fn stop_sub_steps_are_distinct_and_ordered() {
        use stop_sub::*;
        let seq = [
            ENTER, FLUSH_FIRST, ISR_CLEARED, VSYNC_STOP, HPD_STOP, HPD_STOPPED, FINAL_PUBLISH,
            RESET_PUBLICATION, VENUS_CLIENT_DROP, BLOB_SWEEP, CTX_DESTROY, REAP_PARKED,
            HOST_SWEEP, RETIRE_TRANSPORT, SYSTEM_BACKINGS, FLUSH_LAST, DONE,
        ];
        for w in seq.windows(2) {
            assert!(w[0] < w[1]);
        }
        assert!(REMOVE_ENTER > DONE && REMOVE_DROP > REMOVE_ENTER && REMOVE_DONE > REMOVE_DROP);
        assert!(REMOVE_TIMER > REMOVE_DONE);
    }

    #[test]
    fn the_four_millisecond_budget_marks_steps_by_id() {
        assert!(!step_over_budget(0));
        assert!(!step_over_budget(STEP_BUDGET_US - 1));
        assert!(step_over_budget(STEP_BUDGET_US));
        assert!(step_over_budget(u32::MAX));
        // every worker step id has its own bit; the mask of two is the union
        let mut seen = 0u32;
        for (id, _) in site::ALL {
            let bit = step_mask_bit(id);
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(seen & bit, 0, "site {id} shares a bit");
            seen |= bit;
        }
        assert_eq!(step_mask_bit(site::DUMP), 1 << 12);
        assert_eq!(step_mask_bit(site::NVRM_PUBLISH) | step_mask_bit(site::DUMP), 0x1800);
        // an id past the mask shares the last bit instead of overflowing the shift
        assert_eq!(step_mask_bit(32), 1 << 31);
        assert_eq!(step_mask_bit(u32::MAX), 1 << 31);
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
