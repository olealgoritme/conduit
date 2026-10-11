//! The host's vblank as the guest's vsync (`HostVblank`, default 1): the I/O half of
//! `helios_kmd_logic::host_vblank`'s per-vblank mode (the rules, host-tested; docs/SCANOUT.md
//! "Host vblank").
//!
//! The host backend sends one `HostVblank` per host display vblank while a display client's
//! presentation reports keep its vblank lattice fitted. Here:
//!
//! * the event-queue drain (under the virtio lock) leaves the newest event in a one-slot mailbox
//!   with its arrival time ([`on_event`]);
//! * `drain_used_and_complete`, after the lock is released, consumes it ([`service`]): it delivers
//!   a `CRTC_VSYNC` now (unless one went out less than half a period ago) and keeps the timer half
//!   a period off the host's phase. The timer keeps ticking (its other periodic duties, the
//!   watchdogs' heartbeat), delivers nothing between host events, and fills in for a missing one;
//! * the vsync tick asks whether to deliver ([`tick_delivers`]: not within three quarters of a
//!   period of a host event, and only by claiming the vsync slot, as an event does).
//! * events whose refresh is not the mode's (within 2%) drive nothing (`HvRateOff`).
//!
//! With the knob at 0 the feature is not acked, the host sends nothing, and every hook is one
//! relaxed load. `FlipDoneHost` = 1 delivers its own vsyncs from presentation reports; then this
//! module stands aside (it counts, delivers nothing, moves no timer).
//!
//! Counters (`helios_kmd_logic::host_vblank::PACE_COUNTERS`): zeros at every StartDevice, then from
//! the periodic dump (PASSIVE) when one moved.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::host_vblank as hv;

use crate::adapter::AdapterContext;

/// The feature is live this generation (acked, event queue up, knob on).
static ON: AtomicU32 = AtomicU32::new(0);
static KNOB: AtomicU32 = AtomicU32::new(hv::KNOB_DEFAULT);
static DIRTY: AtomicU32 = AtomicU32::new(0);

// The mailbox: the newest event, written under the virtio lock, read after it.
static MBOX_GEN: AtomicU32 = AtomicU32::new(0);
static MBOX_SEEN: AtomicU32 = AtomicU32::new(0);
static MBOX_AT: AtomicU64 = AtomicU64::new(0);
/// `flags << 32 | refresh_mhz`.
static MBOX_WORD: AtomicU64 = AtomicU64::new(0);

/// When the last host event was serviced (0: none, or the host said `IDLE`).
static LAST_HOST: AtomicU64 = AtomicU64::new(0);
/// When the last CRTC_VSYNC went out, from either source.
static LAST_VSYNC: AtomicU64 = AtomicU64::new(0);
/// When the last host-delivered vsync went out (the jitter histogram's reference).
static LAST_HOST_VS: AtomicU64 = AtomicU64::new(0);

static RECV: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static IDLE: AtomicU32 = AtomicU32::new(0);
static VS: AtomicU32 = AtomicU32::new(0);
static VS_SKIP: AtomicU32 = AtomicU32::new(0);
static TK_SKIP: AtomicU32 = AtomicU32::new(0);
static REPHASE: AtomicU32 = AtomicU32::new(0);
static GAP2: AtomicU32 = AtomicU32::new(0);
static RATE_OFF: AtomicU32 = AtomicU32::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const Z32: AtomicU32 = AtomicU32::new(0);
static JIT: [AtomicU32; 6] = [Z32; 6];

#[inline]
fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// Whether to ack `NVGPU_F_HOST_VBLANK` at this start (the knob, read here at PASSIVE, and the
/// display half).
pub(crate) fn wants_feature(display_half: bool) -> bool {
    let knob = crate::diag::read_config_dword(crate::diag::knobs::HOST_VBLANK, hv::KNOB_DEFAULT);
    KNOB.store(knob, Ordering::Relaxed);
    display_half && knob != 0
}

/// This generation's state: live iff the host took the ack and the event queue is up. Zeroes the
/// counters and writes `HvKnob` / `HvAck`. PASSIVE.
pub(crate) fn reset_for_start(acked: bool) {
    let knob = KNOB.load(Ordering::Relaxed);
    ON.store(u32::from(acked && knob != 0), Ordering::Release);
    MBOX_SEEN.store(MBOX_GEN.load(Ordering::Acquire), Ordering::Release);
    for t in [&LAST_HOST, &LAST_VSYNC, &LAST_HOST_VS] {
        t.store(0, Ordering::Relaxed);
    }
    for c in [
        &RECV, &BAD, &IDLE, &VS, &VS_SKIP, &TK_SKIP, &REPHASE, &GAP2, &RATE_OFF,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for c in &JIT {
        c.store(0, Ordering::Relaxed);
    }
    crate::diag::record_named_bytes(b"HvKnob", knob);
    crate::diag::record_named_bytes(b"HvAck", u32::from(acked));
    DIRTY.store(1, Ordering::Release);
}

#[inline]
fn on() -> bool {
    ON.load(Ordering::Acquire) != 0
}

/// A `HostVblank` the parser refused (short, version, count). Any IRQL.
pub(crate) fn note_bad() {
    bump(&BAD);
}

/// A `HostVblank` from a host that was not asked (dropped).
pub(crate) fn note_unasked() {
    bump(&BAD);
}

/// The drain took a `HostVblank`: leave it in the mailbox with its arrival time. Under the
/// virtio lock (DISPATCH); atomics only.
pub(crate) fn on_event(e: &hv::Event) {
    let at = crate::adapter::foreign_scanout::now_100ns();
    MBOX_AT.store(at, Ordering::Relaxed);
    MBOX_WORD.store(
        (u64::from(e.flags) << 32) | u64::from(e.refresh_mhz),
        Ordering::Relaxed,
    );
    MBOX_GEN.fetch_add(1, Ordering::Release);
    RECV.fetch_add(1, Ordering::Relaxed);
}

/// Consume the newest event, if one waits: deliver its CRTC_VSYNC and push the timer. Called by
/// `drain_used_and_complete` after the virtio lock is released (DISPATCH from the DPC, or the
/// worker at PASSIVE); one relaxed load when nothing waits. Two concurrent callers: one claims.
pub(crate) fn service(adapter: &AdapterContext) {
    let gen = MBOX_GEN.load(Ordering::Acquire);
    let seen = MBOX_SEEN.load(Ordering::Relaxed);
    if gen == seen {
        return;
    }
    if MBOX_SEEN
        .compare_exchange(seen, gen, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    DIRTY.store(1, Ordering::Relaxed);
    if !on() || crate::ddi::host_flip_done::mode() == helios_kmd_logic::host_flip_done::Mode::On {
        return;
    }
    let word = MBOX_WORD.load(Ordering::Relaxed);
    let flags = (word >> 32) as u32;
    if flags & hv::F_IDLE != 0 {
        // The host stops: the timer alone from the next tick on.
        LAST_HOST.store(0, Ordering::Relaxed);
        LAST_HOST_VS.store(0, Ordering::Relaxed);
        bump(&IDLE);
        return;
    }
    let now = crate::adapter::foreign_scanout::now_100ns();
    // One period for the event path and the tick: the mode's. A host at another rate does not
    // drive the vsync at all (the timer alone paces at the mode's rate).
    let period = adapter.vsync_period_100ns();
    if !hv::rate_matches(period, word as u32) {
        LAST_HOST.store(0, Ordering::Relaxed);
        LAST_HOST_VS.store(0, Ordering::Relaxed);
        bump(&RATE_OFF);
        return;
    }
    LAST_HOST.store(now, Ordering::Relaxed);
    let live = adapter.display_half()
        && adapter.vsync_armed.load(Ordering::Acquire) != 0
        && adapter.vsync_enabled.load(Ordering::Acquire) != 0;
    if live && claim_vsync(now, period) {
        if let Some(dxgkrnl) = adapter.dxgkrnl_opt() {
            let phys = crate::ddi::host_flip_done::tick_plan(
                adapter.last_primary_address.load(Ordering::Acquire),
                now,
                period,
            )
            .address;
            // SAFETY: live callback interface; `signal_crtc_vsync` raises to DIRQL through
            // `DxgkCbSynchronizeExecution` itself and is callable at <= DIRQL, as from the tick.
            let status = unsafe {
                crate::ddi::submit_command::signal_crtc_vsync(
                    dxgkrnl,
                    phys as i64,
                    crate::ddi::vidpn::CHILD_UID,
                )
            };
            if status == wdk_sys::STATUS_SUCCESS {
                let prev = LAST_HOST_VS.swap(now, Ordering::Relaxed);
                if prev != 0 && now > prev {
                    match hv::jitter_bucket(now - prev, period) {
                        Some(b) => bump(&JIT[b]),
                        None => bump(&GAP2),
                    }
                }
                bump(&VS);
            } else {
                bump(&VS_SKIP);
            }
        }
    } else if live {
        bump(&VS_SKIP);
    }
    // dxgkrnl may have issued the next flip inside that callback.
    if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }
    if let Some(deadline) = hv::rephase(
        now,
        adapter.vsync_deadline_100ns.load(Ordering::Acquire),
        period,
    ) {
        if adapter.rephase_vsync(deadline, now) {
            REPHASE.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Claim the vsync slot at `now`: `true` (and `LAST_VSYNC` = `now`) unless a vsync went out
/// less than half a period ago. A compare-exchange, so the tick (timer DPC) and an event
/// (interrupt DPC) on two CPUs cannot both deliver for one vblank.
fn claim_vsync(now: u64, period: u64) -> bool {
    let mut last = LAST_VSYNC.load(Ordering::Acquire);
    loop {
        if !hv::delivers(now, last, period) {
            return false;
        }
        match LAST_VSYNC.compare_exchange_weak(last, now, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(seen) => last = seen,
        }
    }
}

/// Whether the tick at `now` delivers its CRTC_VSYNC, claiming the slot when it does. DISPATCH,
/// atomics only; with the feature off one relaxed load and `true`.
#[inline]
pub(crate) fn tick_delivers(now: u64, period: u64) -> bool {
    if !on() {
        return true;
    }
    let d = hv::tick_delivers(now, LAST_HOST.load(Ordering::Relaxed), period)
        && claim_vsync(now, period);
    if !d {
        bump(&TK_SKIP);
    }
    d
}

/// Mirror the counters into the service key when one moved. PASSIVE (the periodic dump).
pub(crate) fn publish() {
    if DIRTY.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    let mut values = [0u32; hv::PACE_COUNTERS.len()];
    let fixed = [
        KNOB.load(Ordering::Relaxed),
        ON.load(Ordering::Relaxed),
        RECV.load(Ordering::Relaxed),
        BAD.load(Ordering::Relaxed),
        IDLE.load(Ordering::Relaxed),
        VS.load(Ordering::Relaxed),
        VS_SKIP.load(Ordering::Relaxed),
        TK_SKIP.load(Ordering::Relaxed),
        REPHASE.load(Ordering::Relaxed),
        GAP2.load(Ordering::Relaxed),
        RATE_OFF.load(Ordering::Relaxed),
    ];
    values[..fixed.len()].copy_from_slice(&fixed);
    for (i, c) in JIT.iter().enumerate() {
        values[fixed.len() + i] = c.load(Ordering::Relaxed);
    }
    for (name, v) in hv::PACE_COUNTERS.iter().zip(values) {
        crate::diag::record_named_bytes(name.as_bytes(), v);
    }
}
