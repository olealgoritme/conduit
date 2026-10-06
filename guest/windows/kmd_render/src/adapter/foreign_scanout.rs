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
//!   reset) and the lapse timer the HPD worker polls.
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
    Flip, ForeignScanout, Layout, Poll, PresentError, ReleaseOutcome, SetError, SetKind, SetOutcome,
};

use super::AdapterContext;
use crate::ddi::scanout_trace::LeaseEnd;
use crate::sync::SpinLock;
use crate::virtio::gpu::DeviceOwner;

static STATE: SpinLock<ForeignScanout> = SpinLock::new(ForeignScanout::new());

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
        self.request_scanout_refresh();
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
                FS_SETS.fetch_add(1, Ordering::Relaxed);
                if o.kind == SetKind::TookOver {
                    FS_TAKEOVERS.fetch_add(1, Ordering::Relaxed);
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
            let g = STATE.lock();
            g.suppress_desktop(now_100ns())
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
        let result = STATE.lock().release(owner.raw() as u64, handle);
        match result {
            ReleaseOutcome::Released { .. } => {
                FS_RELEASES.fetch_add(1, Ordering::Relaxed);
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
        if STATE.lock().release_owner(owner.raw() as u64) {
            FS_ENDED.fetch_add(1, Ordering::Relaxed);
            self.foreign_scanout_restore_desktop();
        }
    }

    /// The owner closed `handle` (a successful forwarded `Close`).
    pub(crate) fn foreign_scanout_release_handle(&self, owner: DeviceOwner, handle: u32) {
        if STATE.lock().release_handle(owner.raw() as u64, handle) {
            FS_ENDED.fetch_add(1, Ordering::Relaxed);
            self.foreign_scanout_restore_desktop();
        }
    }

    /// Transport reset / StopDevice: the host's handles, and the desktop's binding
    /// with them, are gone. Called from `reset_display_publication_state`, which
    /// rebuilds the display state from scratch, so no restore is owed.
    pub(crate) fn foreign_scanout_reset(&self) {
        if STATE.lock().reset() {
            FS_ENDED.fetch_add(1, Ordering::Relaxed);
        }
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
        let valid = DeviceOwner::new(src.owner as usize).is_some_and(|owner| {
            self.with_virtio(|v| {
                v.nvrm_epoch() == src.epoch
                    && v.nvrm_handle_device_type(owner, src.handle)
                        .is_some_and(|t| t >= 512)
            })
            .unwrap_or(false)
        });
        if !valid {
            if STATE.lock().invalidate(src.generation) {
                FS_ENDED.fetch_add(1, Ordering::Relaxed);
                self.foreign_scanout_restore_desktop();
            }
            return false;
        }
        FS_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
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
    /// in 100 ns units, until the live source lapses; `None` with no live source.
    pub(crate) fn foreign_scanout_wait_100ns(&self) -> Option<i64> {
        let deadline = STATE.lock().next_deadline()?;
        let remaining = deadline.saturating_sub(now_100ns());
        // At least 1 ms (a due deadline must not spin the worker), at most an hour.
        let remaining = remaining.clamp(10_000, 36_000_000_000);
        Some(-(remaining as i64))
    }
}
