//! The flip-completion invariant for foreign and hollow primaries: the I/O half. The decision is
//! `helios_kmd_logic::flip_completion` (host-tested); this file publishes the kept picture and
//! counts it. Design, unknowns and the hardware checklist: `docs/zero-copy-present.md`, "Flip
//! completion invariant for foreign primaries".
//!
//! A flip of a foreign or hollow allocation (`flip_completion::Source`: an adopted NVK-on-RM
//! resource, or one the Venus path can never show, such as the host-less shared placeholder) the
//! KMD could not show (the `ForeignFlip` arm declined or refused and the Venus path cannot bind,
//! the extent is not the mode's, the DMA Present skipped it, a queued copy's completion failed,
//! the handle paired with nothing) is COMPLETED toward dxgkrnl anyway: its address is published as
//! the displayed one (`ProgrammedPrimary::kept_picture`) while the screen keeps the previous
//! picture. Before this, the only publisher was a programming that BOUND the allocation, so every
//! one of those exits left the flip held and the compositor blocked after a couple of presents.
//!
//! IRQL. [`keep`] is atomics only (one store of the address, a few counters), so it is legal at
//! DIRQL (`SetVidPnSourceAddress` itself), DISPATCH (the DMA lane's submit, the ring-1 completion
//! DPC) and PASSIVE. The registry
//! is written only by [`keep_passive`] (the first and every 64th) and by [`publish_counters`]
//! (PASSIVE, from `publish_nvrm_counters`).
//!
//! Counters (`Fk` prefix, at most 14 characters, the list is
//! `helios_kmd_logic::flip_completion::COUNTERS`): `FkKeep` kept publications, split by who made
//! them into `FkWorker` (the PASSIVE programming worker, every MMIO flip and every armed DMA
//! flip), `FkDma` (the DMA lane, at submit), `FkAsync` (the ring-1 copy-completion DPC), `FkDdi`
//! (`SetVidPnSourceAddress` for an unpaired handle); `FkWhy` the last reason (`KeepWhy::code`);
//! `FkKeep01`..`FkKeep08` per reason; `FkDmaRec` the DMA
//! Presents that wrote a keep record (the Present side of `FkDma`).

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::flip_completion::{self as fc, KeepWhy};

use crate::adapter::AdapterContext;

/// Who completed the flip, for the split counters.
#[derive(Clone, Copy)]
pub(crate) enum Lane {
    /// The PASSIVE programming worker.
    Worker,
    /// The DMA flip lane, at `DxgkDdiSubmitCommand` (DISPATCH).
    Dma,
    /// The ring-1 copy-completion DPC (DISPATCH).
    Async,
    /// `SetVidPnSourceAddress` itself (any IRQL), for a handle that paired with nothing.
    Ddi,
}

static KEPT: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static BY_WHY: [AtomicU32; KeepWhy::COUNT] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static WORKER: AtomicU32 = AtomicU32::new(0);
static DMA: AtomicU32 = AtomicU32::new(0);
static ASYNC: AtomicU32 = AtomicU32::new(0);
static DDI: AtomicU32 = AtomicU32::new(0);
/// DMA Presents that wrote a keep record (the Present side of the DMA lane).
static DMA_RECORDS: AtomicU32 = AtomicU32::new(0);
/// Flips of a host-less shared placeholder the Present completed instead of failing (both
/// contracts; `FkPhFlip`).
static PH_FLIPS: AtomicU32 = AtomicU32::new(0);

/// Publish `address` as a kept picture and count it: atomics only, any IRQL. Returns the running
/// count when this was the first or a 64th (a registry write is due; a PASSIVE caller makes it),
/// `None` otherwise, and `None` without publishing for a zero address (nothing was assigned).
pub(crate) fn keep(
    adapter: &AdapterContext,
    address: u64,
    why: KeepWhy,
    lane: Lane,
) -> Option<u32> {
    let address = fc::keep_address(address)?;
    adapter.publish_kept_primary(address);
    count(why, lane)
}

/// Count one kept publication whose address store the caller made itself (the ring-1
/// completion DPC stores through the pointers its notify block carries): atomics only. Returns
/// the running count when a registry write is due, as [`keep`] does.
pub(crate) fn count(why: KeepWhy, lane: Lane) -> Option<u32> {
    let n = KEPT.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    BY_WHY[why.index()].fetch_add(1, Ordering::Relaxed);
    match lane {
        Lane::Worker => WORKER.fetch_add(1, Ordering::Relaxed),
        Lane::Dma => DMA.fetch_add(1, Ordering::Relaxed),
        Lane::Async => ASYNC.fetch_add(1, Ordering::Relaxed),
        Lane::Ddi => DDI.fetch_add(1, Ordering::Relaxed),
    };
    fc::mirror_due(n).then_some(n)
}

/// [`keep`] for a PASSIVE caller: also the first-and-every-64th registry write.
pub(crate) fn keep_passive(adapter: &AdapterContext, address: u64, why: KeepWhy, lane: Lane) {
    if let Some(n) = keep(adapter, address, why, lane) {
        crate::diag::record_named_bytes(b"FkWhy", why.code());
        crate::diag::record_named_bytes(b"FkKeep", n);
    }
}

/// A DMA Present (PASSIVE) wrote a keep record for a flip it skipped: counted, the first and
/// every 64th written at once.
pub(crate) fn note_dma_record() {
    let n = DMA_RECORDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if fc::mirror_due(n) {
        crate::diag::record_named_bytes(b"FkDmaRec", n);
    }
}

/// A Present (PASSIVE) completed a flip of a host-less shared placeholder (`FkPhFlip`): the first
/// and every 64th written at once.
pub(crate) fn note_placeholder_flip() {
    let n = PH_FLIPS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if fc::mirror_due(n) {
        crate::diag::record_named_bytes(b"FkPhFlip", n);
    }
}

/// The `Fk*` block owes the service key one full write (zeros included) for this StartDevice.
static MIRROR_PENDING: AtomicU32 = AtomicU32::new(1);

/// A new generation (StartDevice): zero the counters and owe the zero block, so values from an
/// earlier run in the service key are never read as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for c in [&KEPT, &LAST_WHY, &WORKER, &DMA, &ASYNC, &DDI, &DMA_RECORDS, &PH_FLIPS] {
        c.store(0, Ordering::Relaxed);
    }
    for c in &BY_WHY {
        c.store(0, Ordering::Relaxed);
    }
    MIRROR_PENDING.store(1, Ordering::Release);
}

/// Mirror the counters to the service key. PASSIVE_LEVEL only; nothing is written until a flip was
/// completed this way (or a keep record was written), so a box that never meets a foreign primary
/// gets no new value.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let kept = KEPT.load(Ordering::Relaxed);
    let records = DMA_RECORDS.load(Ordering::Relaxed);
    let placeholders = PH_FLIPS.load(Ordering::Relaxed);
    let owed = MIRROR_PENDING.swap(0, Ordering::AcqRel) != 0;
    if kept == 0 && records == 0 && placeholders == 0 && !owed {
        return;
    }
    rec(b"FkPhFlip", placeholders);
    rec(b"FkKeep", kept);
    rec(b"FkWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"FkWorker", WORKER.load(Ordering::Relaxed));
    rec(b"FkDma", DMA.load(Ordering::Relaxed));
    rec(b"FkAsync", ASYNC.load(Ordering::Relaxed));
    rec(b"FkDdi", DDI.load(Ordering::Relaxed));
    rec(b"FkDmaRec", records);
    for why in KeepWhy::ALL {
        rec(
            &fc::why_name(why),
            BY_WHY[why.index()].load(Ordering::Relaxed),
        );
    }
}
