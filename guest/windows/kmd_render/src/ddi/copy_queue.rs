//! The transfer-only queue for the windowed Present copies (`CopyQueue`): the knob and the
//! counters. The rules (family choice, the device-creation fallback, the per-copy route, the
//! ownership family, the queue-switch rule) are `helios_kmd_logic::copy_queue` (host-tested); the
//! Venus half (the queue-family query, the second queue, the per-record command pools and
//! barriers, the ring of each submission) is `virtio/venus`. `docs/zero-copy-present.md` 24.13.
//!
//! Knob (REG_DWORD in the service key, default 0 = the previous behaviour; read at every
//! StartDevice by [`reset_for_start`], before the Venus bring-up that uses it): `CopyQueue` 1.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::copy_queue::COUNTERS`):
//! bring-up, written once by [`note_bringup`]: `CqKnob` (knob in force), `CqFamN` (queue families
//! the host reported; 0 when the knob was 0 and nothing was asked), `CqFam` (the transfer family
//! chosen, `0xFFFFFFFF` none), `CqGran` (its `minImageTransferGranularity`, one byte per axis),
//! `CqReady` (1: the device has the transfer queue and its ring), `CqDevFall` (1: `vkCreateDevice`
//! refused the two-queue device and the old one-queue device was made), `CqPrio` (1: the family-0
//! queue was created at high global priority, `CopyQueue` 2), `CqPrioFall` (1: 2 was asked and
//! the device refused the priority at every tier). Per copy, atomics written
//! to the registry by [`publish_counters`] from `publish_nvrm_counters` once the knob is on:
//! `CqMain` / `CqXfer` (Present copies submitted on family 0 / on the transfer family), `CqFall`
//! (of `CqMain`, copies the knob wanted on the transfer queue), `CqWhy` / `CqMask` (last reason /
//! every reason, `copy_queue::Why`), `CqSwitch` (submissions that waited for a copy of another
//! queue sharing a resource), `CqSwitchTo` (of those, waits that gave up after
//! `copy_queue::SWITCH_WAIT_MS`).

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::copy_queue::{self as cq, Choice, Knob, Route, Why};

const UNREAD: u32 = u32::MAX;
static KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static MAIN: AtomicU32 = AtomicU32::new(0);
static XFER: AtomicU32 = AtomicU32::new(0);
static FALL: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static MASK: AtomicU32 = AtomicU32::new(0);
static SWITCH: AtomicU32 = AtomicU32::new(0);
static SWITCH_TO: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn read_knob() -> Knob {
    let k = Knob::from_raw(crate::diag::read_config_dword(
        crate::diag::knobs::COPY_QUEUE,
        0,
    ));
    KNOB.store(k.raw(), Ordering::Relaxed);
    k
}

/// The knob in force. PASSIVE on first use (a registry read), one relaxed load after.
pub(crate) fn knob() -> Knob {
    match KNOB.load(Ordering::Relaxed) {
        UNREAD => read_knob(),
        raw => Knob::from_raw(raw),
    }
}

/// StartDevice, before the Venus bring-up: read the knob again, zero the counters, and write the
/// bring-up block's "nothing yet" values so a previous boot's cannot be read as this one's.
pub(crate) fn reset_for_start() {
    for cell in [&MAIN, &XFER, &FALL, &WHY, &MASK, &SWITCH, &SWITCH_TO] {
        cell.store(0, Ordering::Relaxed);
    }
    let k = read_knob();
    use crate::diag::record_named_bytes as rec;
    rec(b"CqKnob", k.raw());
    rec(b"CqFamN", 0);
    rec(b"CqFam", cq::NO_FAMILY);
    rec(b"CqGran", 0);
    rec(b"CqReady", 0);
    rec(b"CqDevFall", 0);
    rec(b"CqPrio", 0);
    rec(b"CqPrioFall", 0);
}

/// What the Venus bring-up found and made. PASSIVE (StartDevice).
pub(crate) fn note_bringup(
    families: u32,
    choice: Option<Choice>,
    ready: bool,
    dev_fall: bool,
    priority: bool,
    priority_fall: bool,
) {
    use crate::diag::record_named_bytes as rec;
    rec(b"CqFamN", families);
    rec(b"CqFam", choice.map_or(cq::NO_FAMILY, |c| c.index));
    rec(
        b"CqGran",
        choice.map_or(0, |c| cq::pack_granularity(c.granularity)),
    );
    rec(b"CqReady", ready as u32);
    rec(b"CqDevFall", dev_fall as u32);
    rec(b"CqPrio", priority as u32);
    rec(b"CqPrioFall", priority_fall as u32);
}

/// One Present copy submitted on `route`; `why` is set when the knob wanted the transfer queue
/// and the copy's record was made for family 0. Any IRQL (atomics).
pub(crate) fn note_submit(route: Route, why: Option<Why>) {
    match route {
        Route::Main => MAIN.fetch_add(1, Ordering::Relaxed),
        Route::Transfer => XFER.fetch_add(1, Ordering::Relaxed),
    };
    if let Some(why) = why {
        FALL.fetch_add(1, Ordering::Relaxed);
        WHY.store(why.code(), Ordering::Relaxed);
        MASK.fetch_or(why.bit(), Ordering::Relaxed);
    }
}

/// A submission waited for a copy of the other queue (`timed_out`: it gave up and submitted).
pub(crate) fn note_switch(timed_out: bool) {
    SWITCH.fetch_add(1, Ordering::Relaxed);
    if timed_out {
        SWITCH_TO.fetch_add(1, Ordering::Relaxed);
    }
}

/// Mirror the per-copy counters to the service key. PASSIVE only. Nothing with the knob at 0 (the
/// bring-up block, `CqKnob` included, is written at StartDevice either way).
pub(crate) fn publish_counters() {
    if !Knob::from_raw(KNOB.load(Ordering::Relaxed)).transfer() {
        return;
    }
    let events = MAIN.load(Ordering::Relaxed) | XFER.load(Ordering::Relaxed);
    if events == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"CqMain", MAIN.load(Ordering::Relaxed));
    rec(b"CqXfer", XFER.load(Ordering::Relaxed));
    rec(b"CqFall", FALL.load(Ordering::Relaxed));
    rec(b"CqWhy", WHY.load(Ordering::Relaxed));
    rec(b"CqMask", MASK.load(Ordering::Relaxed));
    rec(b"CqSwitch", SWITCH.load(Ordering::Relaxed));
    rec(b"CqSwitchTo", SWITCH_TO.load(Ordering::Relaxed));
}
