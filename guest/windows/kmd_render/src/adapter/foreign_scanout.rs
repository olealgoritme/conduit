//! The foreign scanout source, driver side (`HELIOS_NVRM_OP_SCANOUT_*`,
//! `helios_protocol::nvrm_scanout`). The decisions are
//! `helios_kmd_logic::foreign_scanout`; this file holds the one state cell and the
//! edges around it:
//!
//! * the DESKTOP SUPPRESSION GATE the scanout refresh path asks
//!   ([`AdapterContext::foreign_scanout_suppresses`]). It only withholds the host
//!   `RESOURCE_FLUSH`, the one command that makes the host show a Venus resource.
//!   Binds, the present path, WDDM fences, vsync and the read ledger are untouched,
//!   so a suppressed desktop present completes exactly as before; the refresh that
//!   is dropped is cancelled the way the ownership-gate drops are (publication
//!   transaction cancelled, leases ended), so nothing waits for a read that will
//!   never be issued;
//! * the RESTORE: when the source ends, the desktop owes one fresh flush
//!   ([`AdapterContext::request_scanout_refresh`]); the worker reports it queued
//!   through [`AdapterContext::foreign_scanout_desktop_flushed`];
//! * teardown hooks (`Close` of the DRM file, `close_all_for_owner`, transport
//!   reset) and the lapse timer the HPD worker polls;
//! * the KMD's RESIDENT source (`KmdRmClient` = 3, `virtio/rm_present.rs`): it
//!   suppresses the desktop exactly as a user source does, every flush it withholds
//!   is a frame for the presenter to copy and flip, a user source preempts it, and
//!   when that one ends the restore is a re-flip of the resident surface
//!   ([`AdapterContext::foreign_scanout_restore_desktop`]) instead of a Venus flush.
//!
//! LOCKING. `STATE` is a LEAF spinlock: its holders call nothing but the pure
//! state machine (no allocation, no other lock, no wait). It is taken at PASSIVE
//! (escape, HPD worker, under `scanout_mutex`) and may be taken at DISPATCH; the
//! atomics-and-event edges after it (`request_scanout_refresh`,
//! `release_all_scanout_leases`) are legal at any IRQL up to DISPATCH.
//! `with_virtio` is never called with `STATE` held, and `STATE` is never taken
//! under `virtio_lock`.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::foreign_scanout::{
    Flip, ForeignScanout, Layout, Poll, PresentError, ReleaseOutcome, ResidentDrop,
    ResidentOutcome, SetError, SetKind, SetOutcome,
};
use helios_kmd_logic::rm_fence_present::{Attach, QEntry, ScanoutQueue};

use super::AdapterContext;
use crate::ddi::scanout_trace::LeaseEnd;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::gpu::{DeviceOwner, FenceClaim, FenceRefusal};
use wdk_sys::ntddk::KeSetEvent;

static STATE: SpinLock<ForeignScanout> = SpinLock::new(ForeignScanout::new());

/// The `SCANOUT_PRESENT`s waiting for their fences (`docs/rm-fence-marker.md`,
/// carrier (a)). LOCK ORDER: `virtio_lock` -> `FENCES`; `FENCES` is a leaf (its
/// holders call only the pure queue and `VirtioGpu` methods already under
/// `virtio_lock`). `STATE` is never taken under `FENCES`.
static FENCES: SpinLock<ScanoutQueue> = SpinLock::new(ScanoutQueue::new());
/// The pump (queue -> host flips) is one at a time, so flips leave in order. A
/// caller that finds it busy leaves `PUMP_AGAIN` for the holder to see.
static PUMP_BUSY: AtomicU32 = AtomicU32::new(0);
static PUMP_AGAIN: AtomicU32 = AtomicU32::new(0);

/// Become the pump, or leave a note for whoever is. `true`: this caller pumps.
///
/// The holder's exit is `PUMP_BUSY = 0` then `swap(PUMP_AGAIN, 0)`. A caller that
/// failed the compare-exchange before that store and stores its note after that swap
/// would leave the note set with nobody pumping (a lost wakeup), so after the note it
/// tries once more: either the holder has released (we take over and pump, which
/// covers what the note was for), or it has not yet, and then its swap comes after
/// our note and sees it. All four operations are `SeqCst`: this is a store-then-load
/// pattern on two locations, which release/acquire does not order. No `signal_hpd`
/// here: when the failing caller IS the worker, a wake per failed attempt would spin
/// it for as long as an escape thread holds the pump in a host round trip; the
/// worker's own service pass already looks at `PUMP_AGAIN`.
fn pump_acquire() -> bool {
    if PUMP_BUSY
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // Everything queued so far is seen by the pass that starts now.
        PUMP_AGAIN.store(0, Ordering::SeqCst);
        return true;
    }
    PUMP_AGAIN.store(1, Ordering::SeqCst);
    if PUMP_BUSY
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        PUMP_AGAIN.store(0, Ordering::SeqCst);
        return true;
    }
    false
}

/// Sources started (`FsSet`), flips sent (`FsPres`), sources ended by an explicit
/// `RELEASE` (`FsRel`), by the lapse (`FsLapse`), by teardown or an invalid handle
/// or epoch (`FsEnd`); another owner's source replaced after its lapse (`FsTake`);
/// desktop refreshes withheld (`FsSupp`) and desktop restores completed (`FsRest`);
/// requests refused (`FsRef`) and flips the host or transport did not take
/// (`FsErr`). `FsSet - FsRel - FsLapse - FsEnd - FsTake` is what is live (0 or 1);
/// a nonzero `FsSupp` with no source live is a bug.
pub static FS_SETS: AtomicU32 = AtomicU32::new(0);
pub static FS_PRESENTS: AtomicU32 = AtomicU32::new(0);
pub static FS_RELEASES: AtomicU32 = AtomicU32::new(0);
pub static FS_LAPSES: AtomicU32 = AtomicU32::new(0);
pub static FS_ENDED: AtomicU32 = AtomicU32::new(0);
pub static FS_TAKEOVERS: AtomicU32 = AtomicU32::new(0);
pub static FS_SUPPRESSED: AtomicU32 = AtomicU32::new(0);
pub static FS_RESTORES: AtomicU32 = AtomicU32::new(0);
pub static FS_REFUSED: AtomicU32 = AtomicU32::new(0);
pub static FS_SEND_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Fenced presents (`rm-fence-marker.md`): entries queued (`FsFQue`), flips sent from
/// the queue (`FsFSent`), fences fired (`FsFFire`), of those with an error status
/// (`FsFErr`), queued already fired (`FsFEarly`), ready entries superseded by a newer
/// one (`FsFSkip`), entries dropped unsent because their source ended (`FsFDrop`),
/// requests refused (`FsFRef`) and refused for a full queue (`FsFFull`).
/// Waiting now = `FsFQue - FsFSent - FsFSkip - FsFDrop`; it returns to 0 when idle.
pub static FS_FENCE_QUEUED: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_SENT: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_FIRED: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_ERRORS: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_EARLY: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_SKIPPED: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_REFUSED: AtomicU32 = AtomicU32::new(0);
pub static FS_FENCE_FULL: AtomicU32 = AtomicU32::new(0);

/// A fence a queued present waits on fired (from the DPC: atomics only).
pub(crate) fn note_fence_fired(status: i32) {
    FS_FENCE_FIRED.fetch_add(1, Ordering::Relaxed);
    if status != 0 {
        FS_FENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Why a fenced `SCANOUT_PRESENT` could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueRefusal {
    NoTransport,
    Full,
    NotOwned,
    NotFence,
    AlreadyAttached,
}

/// Monotonic time in 100 ns units. A scalar read, legal through DISPATCH.
pub(crate) fn now_100ns() -> u64 {
    let mut qpc_timestamp = 0u64;
    // SAFETY: `KeQueryInterruptTimePrecise` only reads the clock and writes the
    // out-parameter we own.
    unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&mut qpc_timestamp) }
}

/// Mirror the counters to the registry. PASSIVE only.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    rec(b"FsSet", FS_SETS.load(Ordering::Relaxed));
    rec(b"FsPres", FS_PRESENTS.load(Ordering::Relaxed));
    rec(b"FsRel", FS_RELEASES.load(Ordering::Relaxed));
    rec(b"FsLapse", FS_LAPSES.load(Ordering::Relaxed));
    rec(b"FsEnd", FS_ENDED.load(Ordering::Relaxed));
    rec(b"FsTake", FS_TAKEOVERS.load(Ordering::Relaxed));
    rec(b"FsSupp", FS_SUPPRESSED.load(Ordering::Relaxed));
    rec(b"FsRest", FS_RESTORES.load(Ordering::Relaxed));
    rec(b"FsRef", FS_REFUSED.load(Ordering::Relaxed));
    rec(b"FsErr", FS_SEND_ERRORS.load(Ordering::Relaxed));
    rec(b"FsFQue", FS_FENCE_QUEUED.load(Ordering::Relaxed));
    rec(b"FsFSent", FS_FENCE_SENT.load(Ordering::Relaxed));
    rec(b"FsFFire", FS_FENCE_FIRED.load(Ordering::Relaxed));
    rec(b"FsFErr", FS_FENCE_ERRORS.load(Ordering::Relaxed));
    rec(b"FsFEarly", FS_FENCE_EARLY.load(Ordering::Relaxed));
    rec(b"FsFSkip", FS_FENCE_SKIPPED.load(Ordering::Relaxed));
    rec(b"FsFDrop", FS_FENCE_DROPPED.load(Ordering::Relaxed));
    rec(b"FsFRef", FS_FENCE_REFUSED.load(Ordering::Relaxed));
    rec(b"FsFFull", FS_FENCE_FULL.load(Ordering::Relaxed));
    rec(
        b"FnCloseErr",
        crate::virtio::nvrm::FENCE_CLOSE_ERRORS.load(Ordering::Relaxed),
    );
    crate::virtio::gpu::publish_rm_gate_counters();
    // The host's buffer releases (`Rel*`), written only on a boot that had them on.
    crate::virtio::scanout_release::publish_counters();
}

impl AdapterContext {
    /// The source ended: the host still shows the app's last frame, and the
    /// desktop owes one fresh flush. Atomics and `KeSetEvent(Wait = FALSE)` only.
    ///
    /// The leases go with it, which also switches the epoch ownership gate off
    /// (`scanout_epoch_tracked`), so that gate cannot drop the very refresh this
    /// requests. The same call the `Unavailable` / `RfUnb` arms make: no host read
    /// of the desktop is outstanding for the lease to wait on (this driver's
    /// reuse protection is the read ledger, which retires per token, not this).
    fn foreign_scanout_restore_desktop(&self) {
        self.release_all_scanout_leases(LeaseEnd::Cancelled);
        // A user source ended and the KMD's own resident source has scanout 0 again:
        // the screen is owed a re-flip of ITS surface, not a Venus flush (which the
        // gate would withhold anyway). Atomics and an event: legal where this is.
        if STATE.lock().take_resume_owed() {
            crate::virtio::rm_present::note_resume_edge(self);
            return;
        }
        self.request_scanout_refresh();
        // The source ended: its queued fenced flips are dropped (and their fences
        // closed) by the worker's next pass.
        self.signal_hpd();
    }

    /// `SCANOUT_SET`.
    pub(crate) fn foreign_scanout_set(
        &self,
        owner: DeviceOwner,
        handle: u32,
        epoch: u64,
        layout: Layout,
        lapse_ms: u32,
    ) -> Result<SetOutcome, SetError> {
        let now = now_100ns();
        let result = STATE
            .lock()
            .set(owner.raw() as u64, handle, epoch, layout, lapse_ms, now);
        match &result {
            Ok(o) => {
                // The HPD worker arms the lapse deadline when it loops: wake a
                // worker parked in an untimed wait so it sees this (new or shorter)
                // deadline, or a hung owner on an idle desktop is never timed out.
                // SAFETY: hpd_event is an embedded, in-place initialized KEVENT;
                // KeSetEvent(Wait = FALSE) is legal through DISPATCH_LEVEL.
                unsafe { KeSetEvent(self.hpd_event.get(), 0, 0) };
                FS_SETS.fetch_add(1, Ordering::Relaxed);
                if o.kind == SetKind::TookOver {
                    FS_TAKEOVERS.fetch_add(1, Ordering::Relaxed);
                }
                if o.kind == SetKind::Preempted {
                    crate::virtio::rm_present::note_preempted();
                }
            }
            Err(_) => {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    /// `SCANOUT_PRESENT`, first half: mint the flip. A lapsed source is released
    /// here (and the desktop restored), so the refusal is also the cleanup.
    pub(crate) fn foreign_scanout_mint_flip(
        &self,
        owner: DeviceOwner,
        handle: u32,
    ) -> Result<Flip, PresentError> {
        let now = now_100ns();
        let result = STATE.lock().present(owner.raw() as u64, handle, now);
        match result {
            Ok(_) => {}
            Err(PresentError::Lapsed) => {
                FS_LAPSES.fetch_add(1, Ordering::Relaxed);
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                self.foreign_scanout_restore_desktop();
            }
            Err(PresentError::NoSource) => {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    /// `SCANOUT_PRESENT`, second half, after the host took (or refused) the flip
    /// for `generation`. If the source ended while the flip was in flight, the
    /// flip may have landed AFTER the desktop restore was requested and be the
    /// frame left on screen: ask for another desktop flush, which is ordered after
    /// it. Cheap, and only on the race.
    pub(crate) fn foreign_scanout_flip_done(&self, generation: u32, sent: bool) {
        if sent {
            FS_PRESENTS.fetch_add(1, Ordering::Relaxed);
        } else {
            FS_SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
        let live = {
            let mut g = STATE.lock();
            let now = now_100ns();
            // A flip the host took keeps the source alive from NOW (the lapse
            // runs from acceptance, not from when it was minted); a failed one
            // does not.
            if sent {
                g.extend(generation, now);
            }
            g.suppress_desktop(now)
                .is_some_and(|a| a.generation == generation)
        };
        if sent && !live {
            self.foreign_scanout_restore_desktop();
        }
    }

    /// `SCANOUT_RELEASE`.
    pub(crate) fn foreign_scanout_release(
        &self,
        owner: DeviceOwner,
        handle: Option<u32>,
    ) -> ReleaseOutcome {
        let (result, was_resident) = {
            let mut g = STATE.lock();
            let was = g.resident_foreground();
            (g.release(owner.raw() as u64, handle), was)
        };
        match result {
            ReleaseOutcome::Released { .. } => {
                // The KMD's own resident source ended by its client (`GemClose` of the
                // surface it shows) is `RmResEnd`, as every other end of it: `FsSet`
                // never counted it, so `FsRel` must not either.
                if was_resident {
                    crate::virtio::rm_present::RM_RES_ENDED.fetch_add(1, Ordering::Relaxed);
                } else {
                    FS_RELEASES.fetch_add(1, Ordering::Relaxed);
                }
                self.foreign_scanout_restore_desktop();
            }
            ReleaseOutcome::NotOwner => {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
            ReleaseOutcome::NotActive => {}
        }
        result
    }

    /// Device teardown (`DestroyDevice`, `StopDevice`): the owner is gone.
    pub(crate) fn foreign_scanout_release_owner(&self, owner: DeviceOwner) {
        // The device is gone: nothing of its flips is waited for, and the host sends no
        // release for the buffers of the files it closed.
        crate::virtio::scanout_release::forget_owner(owner.raw());
        // `ForeignFlip`: what the device imported is not flippable any more (its token may be
        // handed to a new device), and the shown allocation is dropped BEFORE the arbiter
        // ends the source, so the worker finds no target to flip rather than a refused flip
        // (a presenter strike), as `foreign_scanout_release_handle` does. PASSIVE, no lock.
        crate::virtio::foreign_flip::owner_closed(self, owner);
        let (ended, was_resident) = {
            let mut g = STATE.lock();
            let was = g.resident_foreground();
            (g.release_owner(owner.raw() as u64), was)
        };
        if ended {
            self.count_end(was_resident);
            self.foreign_scanout_restore_desktop();
        }
    }

    /// The owner closed `handle` (a successful forwarded `Close`).
    pub(crate) fn foreign_scanout_release_handle(&self, owner: DeviceOwner, handle: u32) {
        // The file is closed: the host forgets its buffers with no release event.
        crate::virtio::scanout_release::forget_handle(handle);
        // `ForeignFlip`: records made from this file are poisoned and a shown allocation of
        // it is dropped (the host may reuse the file number). PASSIVE, no lock held.
        crate::virtio::foreign_flip::file_closed(self, owner, handle);
        let (ended, was_resident) = {
            let mut g = STATE.lock();
            let was = g.resident_foreground();
            (g.release_handle(owner.raw() as u64, handle), was)
        };
        if ended {
            self.count_end(was_resident);
            self.foreign_scanout_restore_desktop();
        }
    }

    /// A source ended by teardown or an invalid handle: the resident source has its own
    /// counter (`RmResEnd`), so `FsSet - FsRel - FsLapse - FsEnd - FsTake` stays the
    /// number of USER sources live.
    fn count_end(&self, was_resident: bool) {
        if was_resident {
            crate::virtio::rm_present::RM_RES_ENDED.fetch_add(1, Ordering::Relaxed);
        } else {
            FS_ENDED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Transport reset / StopDevice: the host's handles, and the desktop's binding
    /// with them, are gone. Called from `reset_display_publication_state`, which
    /// rebuilds the display state from scratch, so no restore is owed.
    pub(crate) fn foreign_scanout_reset(&self) {
        let (ended, was_resident) = {
            let mut g = STATE.lock();
            let was = g.resident_foreground();
            (g.reset(), was)
        };
        if ended {
            self.count_end(was_resident);
        }
        // Queued fenced flips die with the transport; their fence handles are
        // closed by the transport sweep.
        let dropped = FENCES.lock().clear();
        FS_FENCE_DROPPED.fetch_add(dropped, Ordering::Relaxed);
        // The flips the host's release events would have retired: gone with the
        // generation, and so is the tracking (`StartDevice` turns it on again if the new
        // transport acked the feature).
        crate::virtio::scanout_release::reset();
    }

    /// Whether a FORWARDed `ScanoutFlip` from `owner` must be refused because
    /// another device holds the scanout.
    pub(crate) fn foreign_scanout_blocks_flip(&self, owner: DeviceOwner) -> bool {
        let now = now_100ns();
        STATE
            .lock()
            .suppress_desktop(now)
            .is_some_and(|a| a.owner != owner.raw() as u64)
    }

    /// The gate the scanout refresh path asks, at PASSIVE under `scanout_mutex`:
    /// is the desktop's host flush withheld right now? True only while a source is
    /// live AND its DRM file is still the owner's in this transport generation; a
    /// source that fails that check is ended here (so a dead or closed owner can
    /// never hold the desktop, whichever teardown hook missed it).
    pub(crate) fn foreign_scanout_suppresses(&self) -> bool {
        let snapshot = {
            let g = STATE.lock();
            g.suppress_desktop(now_100ns())
        };
        let Some(src) = snapshot else {
            return false;
        };
        // `from_token`, not `new`: the owner may be the KMD's own RM client
        // (`DeviceOwner::KMD_RM`), which `new` refuses on purpose.
        let valid = DeviceOwner::from_token(src.owner as usize).is_some_and(|owner| {
            self.with_virtio(|v| {
                v.nvrm_epoch() == src.epoch
                    && v.nvrm_handle_device_type(owner, src.handle)
                        .is_some_and(|t| t >= 512)
            })
            .unwrap_or(false)
        });
        if !valid {
            // Its file is no longer the owner's (closed, or another generation).
            crate::virtio::scanout_release::forget_handle(src.handle);
            if STATE.lock().invalidate(src.generation) {
                self.count_end(src.resident);
                self.foreign_scanout_restore_desktop();
            }
            return false;
        }
        FS_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
        if src.resident {
            // The desktop wanted a flush and the KMD's own source is what is on screen:
            // that flush is a frame to copy and flip, which the worker does next.
            crate::virtio::rm_present::note_frame_edge(self);
        } else {
            // A user source is on screen: a parked resident source is out of date, and
            // owes a fresh frame when the user source ends.
            crate::virtio::rm_present::note_desktop_changed();
        }
        true
    }

    /// The worker queued a desktop refresh (or found nothing bound): a pending
    /// restore is done.
    pub(crate) fn foreign_scanout_desktop_flushed(&self) {
        if STATE.lock().desktop_restored() {
            FS_RESTORES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// HPD worker, once per wake: expire a source whose owner went silent. PASSIVE.
    pub(crate) fn foreign_scanout_service(&self) {
        let now = now_100ns();
        let polled = STATE.lock().poll(now);
        if let Poll::Lapsed { .. } = polled {
            FS_LAPSES.fetch_add(1, Ordering::Relaxed);
            self.foreign_scanout_restore_desktop();
            publish_counters();
        }
    }

    /// For the HPD worker's wait: how long, as a negative relative `KeWait` timeout
    /// in 100 ns units, until the live source lapses or the presenter's next paced
    /// frame is due; `None` when neither waits.
    pub(crate) fn foreign_scanout_wait_100ns(&self) -> Option<i64> {
        let deadline = STATE.lock().next_deadline();
        let frame_at = crate::virtio::rm_present::wake_at();
        if deadline.is_none() && frame_at == 0 {
            // The common case (no source, no paced frame): no clock read.
            return None;
        }
        let now = now_100ns();
        let lapse = deadline.map(|deadline| {
            // At least 1 ms (a due deadline must not spin the worker), at most an hour.
            let remaining = deadline.saturating_sub(now).clamp(10_000, 36_000_000_000);
            -(remaining as i64)
        });
        let frame = (frame_at != 0).then(|| {
            // At least 1 ms, at most a second.
            let remaining = frame_at.saturating_sub(now).clamp(10_000, 10_000_000);
            -(remaining as i64)
        });
        // Both are relative (negative) 100 ns units: the earlier is the larger.
        match (lapse, frame) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Register the KMD's resident source (`KmdRmClient` = 3): see
    /// `helios_kmd_logic::foreign_scanout::ForeignScanout::resident_set`. PASSIVE.
    pub(crate) fn foreign_scanout_resident_set(
        &self,
        owner: DeviceOwner,
        handle: u32,
        epoch: u64,
        layout: Layout,
    ) -> Result<ResidentOutcome, SetError> {
        let now = now_100ns();
        let result = STATE
            .lock()
            .resident_set(owner.raw() as u64, handle, epoch, layout, now);
        if result.is_err() {
            FS_REFUSED.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// The KMD withdraws its resident source: when it was on screen, the desktop is
    /// owed one flush. PASSIVE.
    pub(crate) fn foreign_scanout_resident_drop(&self) -> ResidentDrop {
        let result = STATE.lock().resident_drop();
        if result == ResidentDrop::Ended {
            self.foreign_scanout_restore_desktop();
        }
        result
    }

    /// `(a resident source is registered, it is the foreground source)`.
    pub(crate) fn foreign_scanout_resident_state(&self) -> (bool, bool) {
        let g = STATE.lock();
        (g.resident().is_some(), g.resident_foreground())
    }

    /// As [`Self::foreign_scanout_resident_state`] for one class of resident source: the
    /// KMD's own (`kmd_class`, the RM client's ring and primary) or a user device's
    /// (`ForeignFlip`). Each flip service asks only about its own class.
    pub(crate) fn foreign_scanout_resident_state_of(&self, kmd_class: bool) -> (bool, bool) {
        STATE
            .lock()
            .resident_state_of(DeviceOwner::KMD_RM.raw() as u64, kmd_class)
    }

    /// As [`Self::foreign_scanout_resident_drop`] for one class only: a resident source of
    /// the other class is left alone.
    pub(crate) fn foreign_scanout_resident_drop_of(&self, kmd_class: bool) -> ResidentDrop {
        let result = STATE
            .lock()
            .resident_drop_of(DeviceOwner::KMD_RM.raw() as u64, kmd_class);
        if result == ResidentDrop::Ended {
            self.foreign_scanout_restore_desktop();
        }
        result
    }

    // ---- fenced presents (`docs/rm-fence-marker.md`, carrier (a)) -----------------

    /// Whether any flip waits in the fenced queue (an unfenced present then queues
    /// behind it, to keep the order).
    pub(crate) fn foreign_fence_queue_busy(&self) -> bool {
        !FENCES.lock().is_empty() || PUMP_BUSY.load(Ordering::Acquire) != 0
    }

    /// Queue one flip. With `fence != 0` the fence is taken over for the KMD in the
    /// same lock hold (`FenceClaim::Owner`: it must be the source owner's own fence).
    /// Returns whether the fence had already fired. Nothing changes on a refusal.
    pub(crate) fn foreign_fence_enqueue(
        &self,
        owner: DeviceOwner,
        flip: Flip,
        gem: u32,
        fence: u32,
    ) -> Result<bool, EnqueueRefusal> {
        let outcome = self
            .with_virtio(|v| {
                let mut queue = FENCES.lock();
                if queue.is_full() {
                    return Err(EnqueueRefusal::Full);
                }
                let early = if fence != 0 {
                    match v.fence_attach(FenceClaim::Owner(owner), fence, Attach::Scanout) {
                        Ok(early) => early,
                        Err(FenceRefusal::NotOwned) => return Err(EnqueueRefusal::NotOwned),
                        Err(FenceRefusal::NotFence) => return Err(EnqueueRefusal::NotFence),
                        Err(FenceRefusal::AlreadyAttached) => {
                            return Err(EnqueueRefusal::AlreadyAttached)
                        }
                        // The KMD holds as many fences as it may: the same answer
                        // as a full queue (present without a fence, or retry).
                        Err(FenceRefusal::Quota) => return Err(EnqueueRefusal::Full),
                    }
                } else {
                    None
                };
                // Room was checked under this same hold, so this cannot fail; if it
                // somehow does, hand the fence back rather than strand it.
                if queue.push(QEntry { flip, gem, fence }).is_err() {
                    if fence != 0 {
                        v.fence_unattach(owner, fence);
                    }
                    return Err(EnqueueRefusal::Full);
                }
                Ok(early)
            })
            .map_err(|_| EnqueueRefusal::NoTransport)?;
        match outcome {
            Ok(early) => {
                FS_FENCE_QUEUED.fetch_add(1, Ordering::Relaxed);
                if fence != 0 {
                    // PASSIVE (the escape), outside every lock: what the creator
                    // registered on a handle that is now the KMD's goes with it.
                    crate::virtio::nvrm::release_events_of_taken_fence(self, fence);
                }
                if let Some(status) = early {
                    FS_FENCE_EARLY.fetch_add(1, Ordering::Relaxed);
                    note_fence_fired(status);
                }
                Ok(early.is_some())
            }
            Err(e) => {
                FS_FENCE_REFUSED.fetch_add(1, Ordering::Relaxed);
                if e == EnqueueRefusal::Full {
                    FS_FENCE_FULL.fetch_add(1, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }

    /// Send every flip that is ready, in order, and close the fences that are done
    /// (sent, superseded or dropped). One pump at a time: a caller that finds it
    /// busy leaves a note and the holder goes round again, so a flip queued while
    /// another was being sent is never left waiting for the next wake. PASSIVE: it
    /// does the host round trips.
    ///
    /// Bounded for the HPD worker (which `stop_hpd` joins for 5 s, twice): a flip waits
    /// at most 2.5 s for the host and the pass ends at the first timeout (the host is
    /// not answering) and as soon as `hpd_stop` is set; the flips not reached stay
    /// queued for the next pass or for the teardown that drops them.
    pub(crate) fn foreign_fence_pump(&self, passive: PassiveLevel) {
        if !pump_acquire() {
            return;
        }
        loop {
            let mut host_slow = false;
            loop {
                if self.hpd_stop.load(Ordering::Acquire) != 0 {
                    // StopDevice is joining the worker: stop sending; the sweep of
                    // the transport closes the fences and drops the queue.
                    host_slow = true;
                    break;
                }
                let now = now_100ns();
                let live = STATE
                    .lock()
                    .suppress_desktop(now)
                    .map(|a| (a.generation, a.epoch));
                let drained = self.with_virtio(|v| {
                    // A source of an earlier transport generation names handles that
                    // no longer exist.
                    let live = live.filter(|&(_, epoch)| epoch == v.nvrm_epoch());
                    let drained = FENCES.lock().drain(live, |fence| v.fence_fired(fence));
                    for &fence in drained.closes() {
                        v.fence_want_close(fence);
                    }
                    drained
                });
                let Ok(drained) = drained else {
                    // No transport: nothing can be sent, and nothing will fire.
                    let dropped = FENCES.lock().clear();
                    FS_FENCE_DROPPED.fetch_add(dropped, Ordering::Relaxed);
                    break;
                };
                FS_FENCE_SKIPPED.fetch_add(drained.skipped, Ordering::Relaxed);
                FS_FENCE_DROPPED.fetch_add(drained.dropped, Ordering::Relaxed);
                // Those flips never reach the host: its release event will never name
                // them, so they are done now (a client waiting for one is woken).
                for &seq in drained.gone_seqs() {
                    if let Some(owner) = crate::virtio::scanout_release::gone(seq) {
                        crate::virtio::scanout_release::wake(self, owner);
                    }
                }
                let Some(entry) = drained.send else {
                    break;
                };
                FS_FENCE_SENT.fetch_add(1, Ordering::Relaxed);
                if !crate::virtio::foreign_scanout::send_queued(
                    passive, self, entry.flip, entry.gem,
                ) {
                    host_slow = true;
                    break;
                }
            }
            if !host_slow {
                // (A slow host would cost the next wait as well; `close_owed_fences`
                // has its own bound and stop rules, and the debt flag survives.)
                crate::virtio::nvrm::close_owed_fences(passive, self);
            }
            // SeqCst on the release / note pair (see `pump_acquire`).
            PUMP_BUSY.store(0, Ordering::SeqCst);
            if host_slow || PUMP_AGAIN.swap(0, Ordering::SeqCst) == 0 {
                break;
            }
            if !pump_acquire() {
                break;
            }
        }
    }

    /// HPD worker, once per wake: send what fired, close what is owed. Cheap (two
    /// loads and one short lock) when there is nothing.
    pub(crate) fn foreign_fence_service(&self, passive: PassiveLevel) {
        if FENCES.lock().is_empty()
            && crate::virtio::nvrm::FENCE_CLOSE_OWED.load(Ordering::Acquire) == 0
            && PUMP_AGAIN.load(Ordering::Acquire) == 0
        {
            return;
        }
        self.foreign_fence_pump(passive);
    }
}
