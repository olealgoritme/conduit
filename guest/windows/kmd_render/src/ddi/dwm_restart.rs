//! DWM restart and a stale Explorer: the I/O half of the `Dw*` breadcrumb block. The names, the
//! census verdicts and the pure helpers are `helios_kmd_logic::dwm_restart` (host-tested); this
//! file holds the atomics, the hooks and the mirror. Analysis, counters and the hardware
//! checklist: `docs/zero-copy-present.md`, "DWM restart and stale Explorer".
//!
//! IRQL. Every hook is atomics only, so it is legal wherever its caller is: `note_flip` runs in
//! `SetVidPnSourceAddress`, which dxgkrnl can call at DIRQL; the Present hooks run at PASSIVE.
//! The registry is written only by `publish` (PASSIVE: the end of `DestroyDevice`, the HPD
//! worker's `service`, `publish_nvrm_counters`, StartDevice's zero block). A hook that wants the
//! block mirrored soon calls [`request_publish`], one atomic and at most one event signal.
//!
//! The block is WINDOWED: `begin_destroy` (the entry of `DxgkDdiDestroyDevice`) zeroes the
//! window words, so they count what happened since the last device died. Read them right after
//! the DWM restart with nothing else starting or stopping; `DwDevDel` says if another device died.
//!
//! Only words that changed since the last mirror are written, so an idle driver adds no registry
//! traffic.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::dwm_restart::{self as dr, Ctr};

use crate::adapter::foreign_scanout::now_100ns;
use crate::adapter::AdapterContext;
use crate::sync::SpinLock;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);
static CTRS: [AtomicU32; dr::COUNT] = [ZERO; dr::COUNT];
/// What the registry holds for each word (0 until written: a zero is never written first, the
/// StartDevice zero block owns that).
static PUBLISHED: [AtomicU32; dr::COUNT] = [ZERO; dr::COUNT];

static RING: SpinLock<dr::DestroyedRing> = SpinLock::new(dr::DestroyedRing::new());

/// When the last `DestroyDevice` began (interrupt time, 100 ns); 0 = none yet.
static DESTROY_AT: AtomicU64 = AtomicU64::new(0);
/// `scanout_refresh_count` and `SC_UNAVAILABLE` at that moment, for the derived words.
static SNAP_REFRESH: AtomicU32 = AtomicU32::new(0);
static SNAP_UNAVAILABLE: AtomicU32 = AtomicU32::new(0);
/// The newest `scanout_refresh_count` the worker saw (the adapter is not at hand in
/// `publish_nvrm_counters`).
static LATEST_REFRESH: AtomicU32 = AtomicU32::new(0);
/// The verdict history (`push_verdict`) and the follow-up schedule.
static VERDICTS: AtomicU32 = AtomicU32::new(0);
static FOLLOWUPS_LEFT: AtomicU32 = AtomicU32::new(0);
static NEXT_FOLLOWUP_AT: AtomicU64 = AtomicU64::new(0);

#[inline]
fn bump(c: Ctr) -> u32 {
    CTRS[c as usize]
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

#[inline]
fn set(c: Ctr, v: u32) {
    CTRS[c as usize].store(v, Ordering::Relaxed);
}

#[inline]
fn get(c: Ctr) -> u32 {
    CTRS[c as usize].load(Ordering::Relaxed)
}

/// The first event of a kind since the window opened: record `marker` once. `true` if this call
/// was the first.
#[inline]
fn first(c: Ctr, marker: u32) -> bool {
    CTRS[c as usize]
        .compare_exchange(0, marker, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Milliseconds since the last destroy began, plus one (0 stays "never").
#[inline]
fn since_destroy_marker() -> u32 {
    dr::ms_marker(now_100ns(), DESTROY_AT.load(Ordering::Relaxed))
}

/// Ask the HPD worker to mirror the block (and the rest of the `Nv*` set) soon. Cheap; PASSIVE or
/// DISPATCH (one atomic, one `KeSetEvent(Wait = FALSE)` at most).
pub(crate) fn request_publish(adapter: &AdapterContext) {
    super::escape::request_nvrm_publish(adapter);
}

// ---- the device lifecycle (PASSIVE) -------------------------------------------------------

/// `DxgkDdiCreateDevice` made a device at `addr`.
pub(crate) fn note_device_created(addr: usize) {
    bump(Ctr::DevNew);
    if RING.lock().take_reuse(addr) {
        bump(Ctr::Reuse);
    }
}

/// The ENTRY of `DxgkDdiDestroyDevice`: the window opens. Returns the start time for
/// [`end_destroy`]. The refresh and unavailable counts are snapshotted so the derived words
/// (`DwRfPost`, `DwUnavPost`) are "since this device died".
pub(crate) fn begin_destroy(adapter: &AdapterContext) -> u64 {
    let start = now_100ns();
    for c in dr::WINDOW {
        set(*c, 0);
    }
    DESTROY_AT.store(start, Ordering::Relaxed);
    VERDICTS.store(0, Ordering::Relaxed);
    SNAP_REFRESH.store(
        adapter.scanout_refresh_count.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    SNAP_UNAVAILABLE.store(
        crate::ddi::display::SC_UNAVAILABLE.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    LATEST_REFRESH.store(
        adapter.scanout_refresh_count.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    start
}

/// The END of `DxgkDdiDestroyDevice` (everything the sweeps did is done, the device box is not
/// yet freed): durations, what was reclaimed, the first census, the schedule of the follow-ups.
/// PASSIVE.
pub(crate) fn end_destroy(
    adapter: &AdapterContext,
    addr: usize,
    start_100ns: u64,
    blobs: u32,
    contexts: u32,
    streams: u32,
) {
    let end = now_100ns();
    let ms = dr::ms_between(end, start_100ns);
    bump(Ctr::DevDel);
    set(Ctr::DelMs, ms);
    CTRS[Ctr::DelMsMax as usize].fetch_max(ms, Ordering::Relaxed);
    set(Ctr::DelBlobs, blobs);
    set(Ctr::DelCtxs, contexts);
    set(Ctr::DelStrms, streams);
    set(
        Ctr::ActRes,
        adapter.active_scanout_resource.load(Ordering::Relaxed),
    );
    RING.lock().note_destroyed(addr);
    take_census(adapter);
    FOLLOWUPS_LEFT.store(dr::FOLLOWUPS, Ordering::Relaxed);
    NEXT_FOLLOWUP_AT.store(
        dr::next_followup(dr::FOLLOWUPS, end).unwrap_or(0),
        Ordering::Relaxed,
    );
    publish();
}

/// One census: what the dead device left in the present machinery. PASSIVE.
fn take_census(adapter: &AdapterContext) {
    let Ok(census) = adapter.with_virtio(|v| v.dwm_census()) else {
        // No transport (StopDevice raced): nothing to count.
        return;
    };
    set(Ctr::PbExt, census.buffers_external);
    set(Ctr::PbCons, census.buffers_consumer);
    set(Ctr::PbConsDd, census.buffers_consumer_dead);
    set(Ctr::PbWr, census.buffers_kmd);
    set(Ctr::PsLive, census.streams_live);
    set(Ctr::PsClose, census.streams_closing);
    set(Ctr::WbPend, census.blt_pending);
    set(Ctr::WbReady, census.blt_ready);
    set(
        Ctr::WbHead,
        census.head.map_or(0, dr::HeadBlock::code),
    );
    let verdicts = dr::push_verdict(VERDICTS.load(Ordering::Relaxed), dr::wedge(&census).code());
    VERDICTS.store(verdicts, Ordering::Relaxed);
    set(Ctr::Wedge, verdicts);
    bump(Ctr::CenN);
}

/// The HPD worker's half (PASSIVE, once per pass): the follow-up censuses, about 2 s apart, that
/// tell a transient pin from a wedge. One atomic load when none is owed.
pub(crate) fn service(adapter: &AdapterContext) {
    LATEST_REFRESH.store(
        adapter.scanout_refresh_count.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let left = FOLLOWUPS_LEFT.load(Ordering::Relaxed);
    if left == 0 {
        return;
    }
    let now = now_100ns();
    if now < NEXT_FOLLOWUP_AT.load(Ordering::Relaxed) {
        return;
    }
    FOLLOWUPS_LEFT.store(left - 1, Ordering::Relaxed);
    NEXT_FOLLOWUP_AT.store(
        dr::next_followup(left - 1, now).unwrap_or(0),
        Ordering::Relaxed,
    );
    take_census(adapter);
    publish();
}

/// Whether a follow-up census is owed (the worker then wakes at least every 250 ms).
pub(crate) fn pending() -> bool {
    FOLLOWUPS_LEFT.load(Ordering::Relaxed) != 0
}

/// `CTX_DESTROY` did not confirm (the context is gone from the table, its closing stream slots
/// stay pinned: nothing retries).
pub(crate) fn note_ctx_destroy_failed() {
    bump(Ctr::CtxFail);
}

/// Closing stream slots a successful `CTX_DESTROY` finalized.
pub(crate) fn note_streams_finalized(n: u32) {
    CTRS[Ctr::CtxFin as usize].fetch_add(n, Ordering::Relaxed);
}

/// A destroyed allocation imported the adapter-owned scanout target: its host unbind was skipped
/// (`helios_kmd_logic::dwm_restart::importer_retire_id`).
pub(crate) fn note_importer_destroyed() {
    bump(Ctr::ImpDest);
}

// ---- the Present window (PASSIVE) ----------------------------------------------------------

/// One `DxgkDdiPresent` returned.
#[inline]
pub(crate) fn note_present(success: bool) {
    bump(Ctr::PrN);
    if !success {
        bump(Ctr::PrFail);
    }
}

/// A Blt was copied or queued: the run of skipped Blts ends.
pub(crate) fn note_blt_done(adapter: Option<&AdapterContext>) {
    bump(Ctr::PrOk);
    set(Ctr::RunNow, 0);
    if DESTROY_AT.load(Ordering::Relaxed) != 0 && first(Ctr::OkMs, since_destroy_marker()) {
        if let Some(adapter) = adapter {
            request_publish(adapter);
        }
    }
}

/// A Blt completed without a copy (any reason: `present_blt_skipped`).
pub(crate) fn note_blt_skipped(adapter: Option<&AdapterContext>) {
    let n = bump(Ctr::PrSkip);
    let run = bump(Ctr::RunNow);
    CTRS[Ctr::RunMax as usize].fetch_max(run, Ordering::Relaxed);
    // The first and every 64th: the skips repeat at the frame rate.
    if n == 1 || n % 64 == 0 {
        if let Some(adapter) = adapter {
            request_publish(adapter);
        }
    }
}

/// A Blt could not resolve something (`present_foreign::unresolved_skip`): `source` and
/// `destination` are `HandleCause` numbers (0 resolved), `adapter_unresolved` the adapter, and
/// `color_fill` a no-source fill.
pub(crate) fn note_unresolved(
    source: u32,
    destination: u32,
    adapter_unresolved: bool,
    color_fill: bool,
) {
    if color_fill {
        bump(Ctr::PrCol);
    } else {
        bump(Ctr::PrUnr);
    }
    if adapter_unresolved {
        bump(Ctr::UnrAdp);
    }
    for (word, cause) in [(Ctr::UnrSrc, source), (Ctr::UnrDst, destination)] {
        if let Some(slot) = dr::cause_slot(cause) {
            // A read-modify-write that tolerates a racing bump (the Present path is serialized
            // per context, not globally): a lost increment is acceptable for a histogram.
            let cell = &CTRS[word as usize];
            let current = cell.load(Ordering::Relaxed);
            cell.store(dr::hist_bump(current, slot), Ordering::Relaxed);
        }
    }
    if DESTROY_AT.load(Ordering::Relaxed) != 0 {
        first(Ctr::UnrMs, since_destroy_marker());
    }
}

// ---- flips (DIRQL-safe) ---------------------------------------------------------------------

/// `SetVidPnSourceAddress` was called. Atomics only: legal at DIRQL.
#[inline]
pub(crate) fn note_flip() {
    bump(Ctr::FlipN);
    if DESTROY_AT.load(Ordering::Relaxed) != 0 {
        first(Ctr::FlipMs, since_destroy_marker());
    }
}

/// ... and its handle paired with nothing (`STATUS_INVALID_PARAMETER`).
#[inline]
pub(crate) fn note_flip_unpaired() {
    bump(Ctr::FlipBad);
}

// ---- opens (PASSIVE) -------------------------------------------------------------------------

/// `DxgkDdiOpenAllocation` refused an open whose resource is no longer alive (the C1 gate): a new
/// DWM that cannot open what Explorer shares fails here.
pub(crate) fn note_open_refused(adapter: &AdapterContext, resource_id: u32) {
    bump(Ctr::OpFail);
    set(Ctr::OpFailId, resource_id);
    request_publish(adapter);
}

/// An open recorded no identity (`DwOpNoIdSz`: the private data size of the entry in the low half,
/// of the call in the high half).
pub(crate) fn note_open_no_identity(entry_size: u32, call_size: u32) {
    bump(Ctr::OpNoId);
    set(
        Ctr::OpNoIdSz,
        (entry_size & 0xFFFF) | (call_size.min(0xFFFF) << 16),
    );
}

// ---- the mirror (PASSIVE) --------------------------------------------------------------------

/// The value a word reads at publish time: the stored one, or the derived/foreign one.
fn value_of(c: Ctr) -> u32 {
    use crate::virtio::counters as vc;
    match c {
        Ctr::RfPost => LATEST_REFRESH
            .load(Ordering::Relaxed)
            .wrapping_sub(SNAP_REFRESH.load(Ordering::Relaxed)),
        Ctr::UnavPost => crate::ddi::display::SC_UNAVAILABLE
            .load(Ordering::Relaxed)
            .wrapping_sub(SNAP_UNAVAILABLE.load(Ordering::Relaxed)),
        Ctr::WrBusy => vc::PRESENT_BUFFER_WRITE_BUSY.load(Ordering::Relaxed),
        Ctr::RdBusy => vc::PRESENT_BUFFER_READ_BUSY.load(Ordering::Relaxed),
        Ctr::SyncRej => vc::PRESENT_BUFFER_SYNC_REJECTS.load(Ordering::Relaxed),
        Ctr::RdClaim => vc::PRESENT_BUFFER_READ_CLAIMS.load(Ordering::Relaxed),
        other => get(other),
    }
}

/// Mirror the words that changed since the last mirror. PASSIVE_LEVEL only.
fn publish() {
    for c in dr::ALL {
        let v = value_of(c);
        if PUBLISHED[c as usize].swap(v, Ordering::Relaxed) != v {
            crate::diag::record_named_bytes(c.name().as_bytes(), v);
        }
    }
}

/// The `publish_nvrm_counters` entry (PASSIVE). The derived words are as fresh as the HPD worker's
/// last pass (`service` refreshes `LATEST_REFRESH`).
pub(crate) fn publish_counters() {
    publish();
}

/// A new generation (StartDevice): zero the words and write the zero block, so a value an earlier
/// run left in the service key is never read as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for c in dr::ALL {
        set(c, 0);
        PUBLISHED[c as usize].store(0, Ordering::Relaxed);
        crate::diag::record_named_bytes(c.name().as_bytes(), 0);
    }
    DESTROY_AT.store(0, Ordering::Relaxed);
    VERDICTS.store(0, Ordering::Relaxed);
    FOLLOWUPS_LEFT.store(0, Ordering::Relaxed);
    NEXT_FOLLOWUP_AT.store(0, Ordering::Relaxed);
    SNAP_REFRESH.store(0, Ordering::Relaxed);
    SNAP_UNAVAILABLE.store(0, Ordering::Relaxed);
    LATEST_REFRESH.store(0, Ordering::Relaxed);
}
