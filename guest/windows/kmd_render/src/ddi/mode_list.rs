//! The host's mode list and the committed scanout size (`kmd_logic::mode_list`,
//! docs/SCANOUT.md "Mode list").
//!
//! The event-queue DPC publishes the newest `DisplayModeList` here (one writer:
//! `drain_nvrm_events` runs under the virtio lock); the VidPN DDIs read it at
//! PASSIVE through a bounded seqlock and offer Windows its modes. Without a list
//! (an older host, or none arrived yet) they offer native and the standard modes
//! up to it. `CommitVidPn` records the committed source size, which the scanout
//! path compares a flip's extent against instead of native.
//!
//! Plain atomics only: nothing on the flip path takes a lock for this. One
//! adapter per driver image, as for `host_flip_done`.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use helios_kmd_logic::mode_list::{self, HostList, ModeList, MODE_LIST_MAX};

/// Seqlock sequence over `LEN`/`MHZ`/`MODES`: odd while the DPC writes.
static SEQ: AtomicU32 = AtomicU32::new(0);
/// Modes in the published host list; 0 = none (offer the standard list).
static LEN: AtomicU32 = AtomicU32::new(0);
static MHZ: AtomicU32 = AtomicU32::new(0);
static MODES: [AtomicU32; MODE_LIST_MAX] = [const { AtomicU32::new(0) }; MODE_LIST_MAX];
/// A new list was published and the HPD worker has not told Windows yet.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// The committed source size, packed; 0 = none (native).
static COMMITTED: AtomicU32 = AtomicU32::new(0);
/// Lists received this start (diag `MlN`).
static RECEIVED: AtomicU32 = AtomicU32::new(0);

/// StartDevice, before the transport comes up: no list, nothing committed.
pub(crate) fn reset_for_start() {
    SEQ.fetch_add(1, Ordering::AcqRel);
    LEN.store(0, Ordering::Release);
    SEQ.fetch_add(1, Ordering::AcqRel);
    CHANGED.store(false, Ordering::Release);
    COMMITTED.store(0, Ordering::Release);
    RECEIVED.store(0, Ordering::Relaxed);
    MONITOR_SYNCED.store(0, Ordering::Relaxed);
}

/// The list last added to the monitor's source mode set (a hash, 0 = none), so a
/// cofunc call adds modes only when the list changed (`vidpn::sync_monitor_modes`).
static MONITOR_SYNCED: AtomicU32 = AtomicU32::new(0);

pub(crate) fn monitor_synced() -> u32 {
    MONITOR_SYNCED.load(Ordering::Relaxed)
}

pub(crate) fn set_monitor_synced(hash: u32) {
    MONITOR_SYNCED.store(hash, Ordering::Relaxed);
}

/// The event-queue DPC got a `DisplayModeList`. Returns whether it differs from
/// the published one (the caller then wakes the HPD worker).
pub(crate) fn on_host_list(l: &HostList) -> bool {
    RECEIVED.fetch_add(1, Ordering::Relaxed);
    if host_list().is_some_and(|cur| cur == *l) {
        return false;
    }
    SEQ.fetch_add(1, Ordering::AcqRel);
    // The odd sequence is visible before any field store below.
    core::sync::atomic::fence(Ordering::Release);
    for (i, p) in l.packed().iter().enumerate() {
        MODES[i].store(*p, Ordering::Relaxed);
    }
    MHZ.store(l.refresh_mhz, Ordering::Relaxed);
    LEN.store(l.len() as u32, Ordering::Relaxed);
    SEQ.fetch_add(1, Ordering::AcqRel);
    CHANGED.store(true, Ordering::Release);
    true
}

/// The published host list, if any (and readable within the bounded attempts).
pub(crate) fn host_list() -> Option<HostList> {
    for _ in 0..helios_kmd_logic::SEQ_READ_ATTEMPTS {
        let before = SEQ.load(Ordering::Acquire);
        let len = (LEN.load(Ordering::Relaxed) as usize).min(MODE_LIST_MAX);
        let mhz = MHZ.load(Ordering::Relaxed);
        let mut packed = [0u32; MODE_LIST_MAX];
        for (i, slot) in packed.iter_mut().enumerate().take(len) {
            *slot = MODES[i].load(Ordering::Relaxed);
        }
        core::sync::atomic::fence(Ordering::Acquire);
        let after = SEQ.load(Ordering::Relaxed);
        if helios_kmd_logic::seq_read(before, after) == helios_kmd_logic::SeqRead::Stable {
            if len == 0 {
                return None;
            }
            return Some(HostList::from_packed(&packed[..len], mhz));
        }
    }
    None
}

/// The modes offered for this native size: the host's list, else the standard
/// one. Never empty for a usable native.
pub(crate) fn offered(native: (u32, u32)) -> ModeList {
    let l = match host_list() {
        Some(h) => ModeList::from_host(native.0, native.1, &h),
        None => ModeList::standard(native.0, native.1),
    };
    crate::diag::record_named_bytes(b"MlLen", l.len() as u32);
    l
}

/// Consumed by the HPD worker: whether a new list arrived since last asked.
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// `CommitVidPn` pinned this source size (packed `(w << 16) | h`). A size the
/// driver could never have offered clears it (native) instead.
pub(crate) fn set_committed(packed: u32) {
    let (w, h) = mode_list::unpack(packed);
    let v = if mode_list::usable(w, h) { packed } else { 0 };
    COMMITTED.store(v, Ordering::Release);
}

/// The committed source size, packed; 0 = none.
pub(crate) fn committed() -> u32 {
    COMMITTED.load(Ordering::Acquire)
}

/// Lists received this start.
pub(crate) fn received() -> u32 {
    RECEIVED.load(Ordering::Relaxed)
}

/// The size a flip must have to be shown: the committed one, else native.
#[inline]
pub(crate) fn scanout_extent(native: (u32, u32)) -> (u32, u32) {
    mode_list::scanout_extent(committed(), native)
}
