//! Guest-memory blob as the Blt copy destination (`GuestBlob`): the I/O half. The rules are
//! `helios_kmd_logic::guest_blob` (host-tested; every host-facing constant is in its
//! `contract` module, the config bit is `helios_protocol::NVGPU_CFG_GUEST_BLOB`); the Venus
//! objects are `virtio/venus/guest_blob.rs`. Design, lifecycle, failure matrix and what is not
//! verified: `docs/zero-copy-present.md` section 24.12.
//!
//! Knob (REG_DWORD in the service key, default 0 = the previous behaviour; read at every
//! StartDevice by [`reset_for_start`] and once on first use): `GuestBlob` 1 lets a Blt into a
//! KMD standard buffer whose system backing is fully leased write those pages directly.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::guest_blob::COUNTERS`;
//! atomics, written to the registry by [`publish_counters`] from `publish_nvrm_counters`):
//! `GbKnob` / `GbFeat` (knob in force, host advertises the feature), `GbMade` (guest blobs
//! created and imported), `GbHit` (Present copies submitted into one), `GbDrop` (teardowns),
//! `GbDrainUs` / `GbDrainMax` (drain time total / longest), `GbFail` (failures = strikes),
//! `GbWhy` / `GbMask` (last reason / every reason, `guest_blob::Why`), `GbRefuse` (Presents a
//! decision kept on the legacy copy), `GbStrike` (destinations disabled), `GbLeak`
//! (destinations whose pages stay pinned after a failed release), `GbRuns` / `GbBytes` (the
//! last create), `GbLive` / `GbLiveRuns` (live blobs and runs), `GbLost` (deferred copies whose
//! guest blob was retired before submission).

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::guest_blob::Why;

const UNREAD: u32 = u32::MAX;
static KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static FEAT: AtomicU32 = AtomicU32::new(0);
static MADE: AtomicU32 = AtomicU32::new(0);
static HIT: AtomicU32 = AtomicU32::new(0);
static DROP: AtomicU32 = AtomicU32::new(0);
static DRAIN_US: AtomicU32 = AtomicU32::new(0);
static DRAIN_MAX: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static MASK: AtomicU32 = AtomicU32::new(0);
static REFUSE: AtomicU32 = AtomicU32::new(0);
static STRIKE: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);
static RUNS: AtomicU32 = AtomicU32::new(0);
static BYTES: AtomicU32 = AtomicU32::new(0);
static LIVE: AtomicU32 = AtomicU32::new(0);
static LIVE_RUNS: AtomicU32 = AtomicU32::new(0);
static LOST: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn read_knob() -> bool {
    let v = crate::diag::read_config_dword(crate::diag::knobs::GUEST_BLOB, 0) != 0;
    KNOB.store(v as u32, Ordering::Relaxed);
    v
}

/// `GuestBlob` is on. One relaxed load once read; the first read is PASSIVE (registry).
pub(crate) fn knob_on() -> bool {
    match KNOB.load(Ordering::Relaxed) {
        UNREAD => read_knob(),
        v => v != 0,
    }
}

/// A new transport generation: the knob is read again and mirrored with the value in force
/// (0 included), and the counters are zeroed. PASSIVE.
pub(crate) fn reset_for_start() {
    for cell in [
        &FEAT, &MADE, &HIT, &DROP, &DRAIN_US, &DRAIN_MAX, &FAIL, &WHY, &MASK, &REFUSE, &STRIKE,
        &LEAK, &RUNS, &BYTES, &LIVE, &LIVE_RUNS, &LOST,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
    let on = read_knob();
    crate::diag::record_named_bytes(b"GbKnob", on as u32);
}

/// Mirror the counters to the service key once anything happened. PASSIVE only.
pub(crate) fn publish_counters() {
    let events = MADE.load(Ordering::Relaxed)
        | FAIL.load(Ordering::Relaxed)
        | REFUSE.load(Ordering::Relaxed)
        | DROP.load(Ordering::Relaxed);
    if events == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"GbKnob", KNOB.load(Ordering::Relaxed) & 1);
    rec(b"GbFeat", FEAT.load(Ordering::Relaxed));
    rec(b"GbMade", MADE.load(Ordering::Relaxed));
    rec(b"GbHit", HIT.load(Ordering::Relaxed));
    rec(b"GbDrop", DROP.load(Ordering::Relaxed));
    rec(b"GbDrainUs", DRAIN_US.load(Ordering::Relaxed));
    rec(b"GbDrainMax", DRAIN_MAX.load(Ordering::Relaxed));
    rec(b"GbFail", FAIL.load(Ordering::Relaxed));
    rec(b"GbWhy", WHY.load(Ordering::Relaxed));
    rec(b"GbMask", MASK.load(Ordering::Relaxed));
    rec(b"GbRefuse", REFUSE.load(Ordering::Relaxed));
    rec(b"GbStrike", STRIKE.load(Ordering::Relaxed));
    rec(b"GbLeak", LEAK.load(Ordering::Relaxed));
    rec(b"GbRuns", RUNS.load(Ordering::Relaxed));
    rec(b"GbBytes", BYTES.load(Ordering::Relaxed));
    rec(b"GbLive", LIVE.load(Ordering::Relaxed));
    rec(b"GbLiveRuns", LIVE_RUNS.load(Ordering::Relaxed));
    rec(b"GbLost", LOST.load(Ordering::Relaxed));
}

// ---- counters, callable at any IRQL (atomics only) -----------------------------------------

/// A decision kept this Present on the legacy copy (no strike).
pub(crate) fn note_refused(why: Why) {
    REFUSE.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
}

/// A failure (a strike). `disabled`: it was the destination's last.
pub(crate) fn note_failed(why: Why, disabled: bool) {
    FAIL.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if disabled {
        STRIKE.fetch_add(1, Ordering::Relaxed);
    }
    if why.poisons() {
        LEAK.fetch_add(1, Ordering::Relaxed);
    }
}

/// One Present copy was submitted into a guest blob.
pub(crate) fn note_hit() {
    HIT.fetch_add(1, Ordering::Relaxed);
}

/// A deferred copy prepared for a guest blob found it retired at submission.
pub(crate) fn note_lost() {
    LOST.fetch_add(1, Ordering::Relaxed);
}

/// The host advertises the feature (recorded once per generation, at first use).
pub(crate) fn note_feature(advertised: bool) {
    FEAT.store(advertised as u32, Ordering::Relaxed);
}
