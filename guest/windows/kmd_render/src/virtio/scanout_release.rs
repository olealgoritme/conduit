//! The KMD's use of the host's buffer-release event (`NVGPU_F_SCANOUT_RELEASE`,
//! `MsgType::ScanoutReleased` = 28): the one state cell, the interrupt-side consumer and
//! the counters. Rules, book and wait state machine: `helios_kmd_logic::scanout_release`;
//! the ABI the release is shown to user mode through, and the host contract:
//! `docs/foreign-scanout.md` ("Buffer release").
//!
//! WHO FEEDS IT. Every flip the KMD mints and sends (`virtio/foreign_scanout.rs`: user
//! `SCANOUT_PRESENT`, fenced or not, and the RM ring presenter) is entered in the BOOK
//! when it is minted (`minted`), marked on the host when the host took it (`sent`), and
//! retired without an event when it never reaches the host (`gone`: skipped for a newer
//! frame, dropped with its source, refused). The host's `ScanoutReleased` retires the
//! rest (`on_released`, from the DPC).
//!
//! WHO READS IT. The RM ring presenter asks [`is_done`] before it writes a surface again
//! (`virtio/rm_present.rs`); `SCANOUT_STATUS` answers [`floor`] to user mode
//! (`ddi/escape_foreign_scanout.rs`); a matched release signals the owner's
//! `SCANOUT_RELEASED` events (user mode) or wakes the HPD worker (the ring presenter).
//!
//! A Venus resource release (`RESOURCE` flag) is counted and nothing more. The desktop's
//! read ledger (`adapter/read_ledger.rs`) retires a read at the host's FLUSH reply, not at
//! replacement, and a desktop that flushes one resource over and over never gets its
//! release; making the ledger wait for it would pin the front buffer for good. See the
//! doc for what a future per-resource use would need.
//!
//! LOCKING AND IRQL. `BOOK` is a LEAF spinlock over plain data (no allocation, no other
//! lock, no wait inside it). [`on_released`] runs in the interrupt DPC under the virtio
//! lock (order: virtio lock -> `BOOK`); everything else takes `BOOK` alone, at any IRQL up
//! to DISPATCH, and never takes another lock while holding it. [`publish_counters`] is
//! PASSIVE only.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use helios_kmd_logic::scanout_release::{Release, ReleaseBook, Released};

use crate::adapter::AdapterContext;
use crate::sync::SpinLock;
use crate::virtio::gpu::DeviceOwner;

static BOOK: SpinLock<ReleaseBook> = SpinLock::new(ReleaseBook::new());

/// The device acked the release event and the KMD uses it (set by `StartDevice` after the
/// transport is up, cleared by every reset of the display publication state). Off: every
/// function here is a no-op that answers "nothing to wait for".
static TRACKING: AtomicBool = AtomicBool::new(false);
/// Tracking was on at some point this boot (the counters are only mirrored then).
static EVER: AtomicBool = AtomicBool::new(false);

// Counters (registry mirror: `publish_counters`; names at most 14 characters):
// `RelRecv` events parsed; `RelMatch` of those that retired a flip the KMD minted;
// `RelDrop` that named no flip of ours (a forwarded flip, a buffer already forgotten, a
// scanout other than 0); `RelRes` Venus resources released; `RelNotShown` / `RelForced`
// with those host flags; `RelBad` short or malformed `ScanoutReleased`; `RelUnasked`
// ones that arrived without the feature (counted by `NvEvOther` as well); `RelSig` user
// events signalled; `RelTrack` flips entered; `RelGone` flips retired without an event;
// `RelEvict` live book entries overwritten; `RelRWaits` / `RelRTimeouts` ring presenter
// waits begun / overruled at the limit.
pub static REL_RECV: AtomicU32 = AtomicU32::new(0);
pub static REL_MATCHED: AtomicU32 = AtomicU32::new(0);
pub static REL_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static REL_RESOURCE: AtomicU32 = AtomicU32::new(0);
pub static REL_NOT_SHOWN: AtomicU32 = AtomicU32::new(0);
pub static REL_FORCED: AtomicU32 = AtomicU32::new(0);
pub static REL_BAD: AtomicU32 = AtomicU32::new(0);
pub static REL_UNASKED: AtomicU32 = AtomicU32::new(0);
pub static REL_SIGNALS: AtomicU32 = AtomicU32::new(0);
pub static REL_TRACKED: AtomicU32 = AtomicU32::new(0);
pub static REL_GONE: AtomicU32 = AtomicU32::new(0);
pub static REL_EVICTED: AtomicU32 = AtomicU32::new(0);
pub static REL_RING_WAITS: AtomicU32 = AtomicU32::new(0);
pub static REL_RING_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the registry. PASSIVE only. Nothing is written for a boot that
/// never had releases on.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if !EVER.load(Ordering::Relaxed) {
        return;
    }
    rec(b"RelRecv", REL_RECV.load(Ordering::Relaxed));
    rec(b"RelMatch", REL_MATCHED.load(Ordering::Relaxed));
    rec(b"RelDrop", REL_DROPPED.load(Ordering::Relaxed));
    rec(b"RelRes", REL_RESOURCE.load(Ordering::Relaxed));
    rec(b"RelNotShown", REL_NOT_SHOWN.load(Ordering::Relaxed));
    rec(b"RelForced", REL_FORCED.load(Ordering::Relaxed));
    rec(b"RelBad", REL_BAD.load(Ordering::Relaxed));
    rec(b"RelUnasked", REL_UNASKED.load(Ordering::Relaxed));
    rec(b"RelSig", REL_SIGNALS.load(Ordering::Relaxed));
    rec(b"RelTrack", REL_TRACKED.load(Ordering::Relaxed));
    rec(b"RelGone", REL_GONE.load(Ordering::Relaxed));
    rec(b"RelEvict", REL_EVICTED.load(Ordering::Relaxed));
    rec(b"RelRWaits", REL_RING_WAITS.load(Ordering::Relaxed));
    rec(b"RelRTimeouts", REL_RING_TIMEOUTS.load(Ordering::Relaxed));
}

/// Turn tracking on or off for this transport generation (`StartDevice`, after the
/// display state was reset). The book starts empty either way.
pub(crate) fn set_tracking(on: bool) {
    BOOK.lock().reset();
    TRACKING.store(on, Ordering::Release);
    if on {
        EVER.store(true, Ordering::Relaxed);
    }
}

/// Whether the host's releases are being tracked.
pub(crate) fn tracking() -> bool {
    TRACKING.load(Ordering::Acquire)
}

/// The transport generation ended (reset / StopDevice): nothing of it is tracked any
/// more, and tracking is off until the next transport says otherwise.
pub(crate) fn reset() {
    TRACKING.store(false, Ordering::Release);
    BOOK.lock().reset();
}

// ---- the flips -----------------------------------------------------------------------

/// A flip of `(owner, handle, gem)` was minted as `seq`.
pub(crate) fn minted(seq: u64, owner: usize, handle: u32, gem: u32) {
    // `FlipDoneHost`: the newest `seq` minted is the floor of the next publication (every mint
    // passes here, tracked or not).
    crate::ddi::host_flip_done::note_minted(seq);
    if !tracking() {
        return;
    }
    let m = BOOK.lock().minted(seq, owner, handle, gem);
    REL_TRACKED.fetch_add(1, Ordering::Relaxed);
    if m.evicted_live {
        REL_EVICTED.fetch_add(1, Ordering::Relaxed);
    }
}

/// The host took flip `seq`, at `now` (100 ns). Returns the owner to wake when this
/// finished older flips of the same buffer (the floor may have moved: the host's release
/// of the buffer it replaced can have arrived before this call, and found them live).
pub(crate) fn sent(seq: u64, now: u64) -> Option<usize> {
    if !tracking() {
        return None;
    }
    let s = BOOK.lock().sent(seq, now)?;
    (s.superseded != 0).then_some(s.owner)
}

/// Wake whoever waits on `owner`'s flips (`DeviceOwner::raw`): the HPD worker for the
/// KMD's own ring presenter, the registered `SCANOUT_RELEASED` events for a user process.
/// PASSIVE (takes the virtio lock); the DPC does the same inline.
pub(crate) fn wake(adapter: &AdapterContext, owner: usize) {
    if owner == DeviceOwner::KMD_RM.raw() {
        adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::RELEASE);
        return;
    }
    let _ = adapter.with_virtio(|v| v.signal_scanout_released(owner));
}

/// Flip `seq` never reached (or was refused by) the host. Returns the owner to wake when
/// that changed anything.
pub(crate) fn gone(seq: u64) -> Option<usize> {
    if !tracking() {
        return None;
    }
    let owner = BOOK.lock().gone(seq);
    if owner.is_some() {
        REL_GONE.fetch_add(1, Ordering::Relaxed);
    }
    owner
}

/// Whether nothing reads flip `seq` any more (`true` when tracking is off or `seq` is
/// not known: nothing to wait for).
pub(crate) fn is_done(seq: u64, now: u64) -> bool {
    !tracking() || BOOK.lock().is_done(seq, now)
}

/// `(released floor, newest seq)` of the source on DRM handle `handle`.
pub(crate) fn floor(handle: u32, now: u64) -> (u64, u64) {
    BOOK.lock().floor(handle, now)
}

/// The file `handle` was closed: its buffers are forgotten by the host with no event.
pub(crate) fn forget_handle(handle: u32) {
    BOOK.lock().forget_handle(handle);
}

/// The device `owner` is gone.
pub(crate) fn forget_owner(owner: usize) {
    BOOK.lock().forget_owner(owner);
}

// ---- the event -----------------------------------------------------------------------

/// What one `ScanoutReleased` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It retired a flip of `owner`'s (`owner` is the `DeviceOwner::raw()` it was minted for).
    Matched { owner: usize },
    /// Counted, nothing to do.
    Ignored,
}

/// Account for one `ScanoutReleased`. DPC, under the virtio lock (leaf `BOOK`): no
/// allocation, no wait.
pub(crate) fn on_released(r: &Released) -> Outcome {
    REL_RECV.fetch_add(1, Ordering::Relaxed);
    if r.not_shown() {
        REL_NOT_SHOWN.fetch_add(1, Ordering::Relaxed);
    }
    if r.forced() {
        REL_FORCED.fetch_add(1, Ordering::Relaxed);
    }
    if r.is_resource() {
        // A Venus scanout resource: counted only (see the module docs).
        REL_RESOURCE.fetch_add(1, Ordering::Relaxed);
        return Outcome::Ignored;
    }
    // Only scanout 0 exists; a release for another one names nothing of ours.
    if r.scanout != 0 || !tracking() {
        REL_DROPPED.fetch_add(1, Ordering::Relaxed);
        return Outcome::Ignored;
    }
    match BOOK.lock().released(r.owner_handle, r.host_handle, r.seq) {
        Release::Matched { owner, .. } => {
            REL_MATCHED.fetch_add(1, Ordering::Relaxed);
            Outcome::Matched { owner }
        }
        Release::Unmatched => {
            REL_DROPPED.fetch_add(1, Ordering::Relaxed);
            Outcome::Ignored
        }
    }
}
