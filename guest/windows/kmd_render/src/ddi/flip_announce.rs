//! `FlipAnnounce` and `FlipEarlyWake`: one tick per flip (`docs/kmd-rm-client.md` 15.18.15). The
//! decision table, the worker-side rule and the arithmetic are `helios_kmd_logic::flip_retire`
//! (host-tested); this file is the statics and the hooks. Everything the DDI side does is
//! atomics: `SetVidPnSourceAddress` arrives at DIRQL.
//!
//! WHY. A flip dxgkrnl issues is retired by the first `CRTC_VSYNC` tick that carries its
//! address, and the address is published by the HPD worker after it programmed the flip; the
//! worker is woken only by the tick. A flip issued right after tick N is therefore published
//! after tick N+1 has read the address and retires at tick N+2: two ticks per flip, 120 flips a
//! second at 240 Hz, whatever the host does. `FlipAnnounce` publishes the address at the DDI
//! itself, so tick N+1 retires the flip, and the worker then does the real programming. The
//! early wake (`FlipEarlyWake`, and any announce mode) makes the worker start within
//! microseconds of the flip instead of at the next tick.
//!
//! THE HAZARD AND ITS BOUND. dxgkrnl hands the PREVIOUS buffer back to the compositor when a
//! flip retires. Retiring one tick early therefore lets the compositor reuse the previous
//! buffer while the host may still scan it (a bind or a foreign flip not yet done) or while a
//! copy of it is in flight. Bound: a flip is announced only if the worker is idle when its DDI
//! arrives (nothing pending, the programming gate lowered: the previous flip's bind, copy and
//! completion are all finished: [`worker_idle`], read before this flip raises the gate), so at
//! most ONE announced flip is ever unprogrammed, and the buffer a copy or a bind of flip N-1
//! reads is never released before N-1 finished. A flip that finds the worker busy retires the
//! normal way. What remains: for the one announced flip, the window between its announce and
//! the worker's bind / foreign flip (the previous buffer may be overwritten while the host still
//! scans it: a tear, not corruption; `FlipPrgLat*` measures the window).
//!
//! `FlipAnnounce`: 0 = off (today), 1 = foreign allocations `ForeignFlip` already accepted,
//! 2 = every flip. `FlipEarlyWake` 1 = the early DPC wake alone. Read at every StartDevice.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::dxgk::HANDLE;

use helios_kmd_logic::flip_retire::{self as fr, Announce, AnnounceFacts, AnnounceMode, NoAnnounce};

use crate::adapter::{gate_active, AdapterContext};

/// `FlipAnnounce` in force (`AnnounceMode::code`), `FlipEarlyWake` in force.
static MODE: AtomicU32 = AtomicU32::new(0);
static EARLY: AtomicU32 = AtomicU32::new(0);
/// `FlipAnnForeign` in force.
static FOREIGN_OK: AtomicU32 = AtomicU32::new(0);

/// The address (and handle) the newest announce waits for the worker to confirm; 0 = none.
static ANN_ADDR: AtomicU64 = AtomicU64::new(0);
static ANN_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// Resources `ForeignFlip` accepted: `(generation << 32) | resource`, valid while the generation
/// is [`GEN`]. 16 slots, replaced round robin.
const TAB_N: usize = 16;
#[allow(clippy::declare_interior_mutable_const)]
const Z64: AtomicU64 = AtomicU64::new(0);
static TAB: [AtomicU64; TAB_N] = [Z64; TAB_N];
static TAB_NEXT: AtomicU32 = AtomicU32::new(0);
static GEN: AtomicU32 = AtomicU32::new(1);

static DDI: AtomicU32 = AtomicU32::new(0);
static WORKER: AtomicU32 = AtomicU32::new(0);
static REFUSE: AtomicU32 = AtomicU32::new(0);
static LATE: AtomicU32 = AtomicU32::new(0);
static NO: AtomicU32 = AtomicU32::new(0);
static NO_WHY: AtomicU32 = AtomicU32::new(0);
static NO_BUSY: AtomicU32 = AtomicU32::new(0);
static NO_UNK: AtomicU32 = AtomicU32::new(0);
static NO_FAIL: AtomicU32 = AtomicU32::new(0);
static NO_FGN: AtomicU32 = AtomicU32::new(0);
static NO_OTHER: AtomicU32 = AtomicU32::new(0);
static EARLY_N: AtomicU32 = AtomicU32::new(0);
static MIRROR_PENDING: AtomicU32 = AtomicU32::new(1);

/// The `FlipAnnounce` mode the service key asks for (default `flip_retire::DEFAULT_KNOB`: 2).
/// PASSIVE (registry read).
pub(crate) fn configured_mode() -> AnnounceMode {
    AnnounceMode::from_knob(crate::diag::read_config_dword(
        crate::diag::knobs::FLIP_ANNOUNCE,
        fr::DEFAULT_KNOB,
    ))
}

/// `FlipAnnounce` and `FlipEarlyWake` for this start, and the zeroed counters. PASSIVE.
pub(crate) fn start_generation() {
    let mode = configured_mode();
    let early = crate::diag::read_config_dword(crate::diag::knobs::FLIP_EARLY_WAKE, 0);
    FOREIGN_OK.store(
        u32::from(crate::diag::read_config_dword(crate::diag::knobs::FLIP_ANN_FOREIGN, 0) != 0),
        Ordering::Relaxed,
    );
    MODE.store(mode.code(), Ordering::Relaxed);
    EARLY.store(u32::from(early != 0), Ordering::Relaxed);
    ANN_ADDR.store(0, Ordering::Release);
    ANN_HANDLE.store(0, Ordering::Release);
    invalidate_all();
    for c in [
        &DDI, &WORKER, &REFUSE, &LATE, &NO, &NO_WHY, &NO_BUSY, &NO_UNK, &NO_FAIL, &NO_OTHER, &NO_FGN,
        &EARLY_N,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    MIRROR_PENDING.store(1, Ordering::Release);
}

#[inline]
fn mode() -> AnnounceMode {
    AnnounceMode::from_knob(MODE.load(Ordering::Relaxed))
}

/// Whether the DDI asks for the DPC that wakes the worker at issue (an announce mode or
/// `FlipEarlyWake`). One or two relaxed loads, any IRQL.
#[inline]
pub(crate) fn wakes_early() -> bool {
    fr::wakes_early(mode(), EARLY.load(Ordering::Relaxed))
}

/// The DDI asked for the early DPC on this knob's account (counted).
pub(crate) fn note_early() {
    EARLY_N.fetch_add(1, Ordering::Relaxed);
}

// ---- the table of accepted foreign resources --------------------------------------------------

/// `ForeignFlip` accepted `resource` at a programming (PASSIVE worker, atomics only).
pub(crate) fn learn(resource: u32) {
    if resource == 0 || !mode().announces() {
        return;
    }
    let word = ((GEN.load(Ordering::Acquire) as u64) << 32) | resource as u64;
    for e in &TAB {
        if e.load(Ordering::Relaxed) == word {
            return;
        }
    }
    let i = TAB_NEXT.fetch_add(1, Ordering::Relaxed) as usize % TAB_N;
    TAB[i].store(word, Ordering::Release);
}

/// Forget every accepted resource (a file closed, a refusal, a failure, a mode change, a new
/// generation): the next flip of each is decided by the worker again. Atomics only.
pub(crate) fn invalidate_all() {
    GEN.fetch_add(1, Ordering::AcqRel);
}

fn accepted(resource: u32) -> bool {
    let word = ((GEN.load(Ordering::Acquire) as u64) << 32) | resource as u64;
    TAB.iter().any(|e| e.load(Ordering::Acquire) == word)
}

// ---- the DDI side -----------------------------------------------------------------------------

/// Whether the worker has nothing in hand: no pending handle and the programming gate lowered.
/// Read BEFORE the DDI raises the gate for its own flip. Atomics only.
pub(crate) fn worker_idle(adapter: &AdapterContext) -> bool {
    adapter.pending_vidpn_allocation.load(Ordering::Acquire) == 0
        && !gate_active(adapter.vidpn_programming.load(Ordering::Acquire))
        // The gate drops when the programming (bind, `take`) returns, but the ForeignFlip host
        // flip of that picture is the worker's LATER step (paced, windowed, asynchronous): the
        // previous buffer is still read until it is done.
        && !crate::virtio::foreign_flip::busy()
}

/// First thing in every `SetVidPnSourceAddress` with a mode on: forget an announcement the
/// worker never confirmed, so a later publication of ANOTHER address (this flip's kept picture,
/// the watchdog) is not mistaken for a regression, and read [`worker_idle`] before this flip
/// raises the gate (the answer is the `idle` of [`at_ddi`]). One relaxed load with the knob off.
#[inline]
pub(crate) fn on_ddi_entry(adapter: &AdapterContext) -> bool {
    if mode().announces() {
        forget_unconfirmed();
        return worker_idle(adapter);
    }
    false
}

/// Forget an announcement nobody confirmed. Called at every flip dxgkrnl issues (the MMIO DDI
/// above, and the DMA lane's submit and keep record, which do not announce but publish through
/// the funnel too) and when the display publication state is reset. Atomics only.
#[inline]
pub(crate) fn forget_unconfirmed() {
    if ANN_ADDR.load(Ordering::Relaxed) != 0 {
        ANN_ADDR.store(0, Ordering::Release);
        ANN_HANDLE.store(0, Ordering::Release);
    }
}

/// The paired flip `h_alloc` naming `address` reached the DDI (DIRQL; `idle` is
/// [`worker_idle`] read before the gate was raised). Announce it if the table says so: publish
/// the address toward dxgkrnl now, so the next tick retires it. Atomics only.
///
/// # Safety
/// `h_alloc` is the handle dxgkrnl passed to the DDI.
pub(crate) unsafe fn at_ddi(adapter: &AdapterContext, h_alloc: HANDLE, address: u64, idle: bool) {
    let mode = mode();
    if !mode.announces() {
        return;
    }
    // SAFETY: the same lock-free resolution `set_vidpn_primary_address` just made.
    let resource = unsafe { crate::ddi::create_allocation::allocation_resource_id(adapter, h_alloc) };
    let accepted = resource.is_some_and(accepted);
    // The KMD's own lock-free record of the allocation: a foreign or hollow one is not Venus.
    // SAFETY: as above.
    let foreign_class = unsafe { crate::ddi::create_allocation::flip_completion_info(adapter, h_alloc) }
        .is_some_and(|(source, _)| source != helios_kmd_logic::flip_completion::Source::Venus);
    let facts = AnnounceFacts {
        mode,
        address,
        resource,
        foreign_class,
        foreign_ok: FOREIGN_OK.load(Ordering::Relaxed) != 0,
        idle,
        accepted,
        failing: crate::virtio::foreign_flip::failing_atomics(),
    };
    match fr::announce_decide(&facts) {
        Announce::Yes => {
            ANN_HANDLE.store(h_alloc as usize, Ordering::Release);
            ANN_ADDR.store(address, Ordering::Release);
            adapter.publish_announced_primary(address);
            crate::ddi::flip_lat::mark_announced();
            DDI.fetch_add(1, Ordering::Relaxed);
        }
        Announce::No(why) => {
            NO.fetch_add(1, Ordering::Relaxed);
            NO_WHY.store(why.code(), Ordering::Relaxed);
            match why {
                NoAnnounce::Busy => NO_BUSY.fetch_add(1, Ordering::Relaxed),
                NoAnnounce::Unknown => NO_UNK.fetch_add(1, Ordering::Relaxed),
                NoAnnounce::Failing => NO_FAIL.fetch_add(1, Ordering::Relaxed),
                NoAnnounce::ForeignOff => NO_FGN.fetch_add(1, Ordering::Relaxed),
                _ => NO_OTHER.fetch_add(1, Ordering::Relaxed),
            };
        }
    }
}

// ---- the worker side --------------------------------------------------------------------------

/// The funnel every address publication passes (`publish_displayed_primary`): whether to store
/// `address` as the displayed one. With nothing announced it is one acquire load and `true`
/// (the default, exactly as before). The announced flip's own publication is swallowed (already
/// published; `FlipPub` counts it once), and a publication of an OLDER address after a newer
/// announce is dropped (it would hand the heartbeat a regressed address).
#[inline]
pub(crate) fn funnel(address: u64) -> bool {
    let announced = ANN_ADDR.load(Ordering::Acquire);
    if announced == 0 {
        return true;
    }
    match fr::worker_publish(announced, address) {
        fr::WorkerPublish::Store => true,
        fr::WorkerPublish::Confirm => {
            if ANN_ADDR
                .compare_exchange(announced, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                WORKER.fetch_add(1, Ordering::Relaxed);
            }
            false
        }
        fr::WorkerPublish::Late => {
            LATE.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// A ring-1 copy completion DPC is about to store `address` as the displayed one (it stores
/// through a pointer, not through `publish_displayed_primary`): the same funnel. Returns whether
/// to store. The announced flip's own copy completion confirms the announcement (the store is
/// skipped: already published), and a completion of an OLDER copy after a newer announce is
/// dropped instead of regressing the heartbeat's address.
#[inline]
pub(crate) fn funnel_dpc(address: u64) -> bool {
    funnel(address)
}

/// The worker's programming of `h_alloc` ended in a refusal. If that flip was announced the
/// flip already retired (the screen keeps the previous picture: the kept-picture semantics
/// every foreign refusal had): count it. Atomics only.
pub(crate) fn note_worker_refused(h_alloc: HANDLE) {
    if ANN_HANDLE.load(Ordering::Acquire) == h_alloc as usize && h_alloc as usize != 0 {
        REFUSE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Mirror the counters. PASSIVE only (the stall-diagnosis mirror).
pub(crate) fn publish_counters() {
    let mut mr = crate::ddi::flip_lat::Mirror::new(crate::ddi::flip_lat::ANNOUNCE_BASE);
    let owed = MIRROR_PENDING.swap(0, Ordering::AcqRel) != 0;
    let m = MODE.load(Ordering::Relaxed);
    let e = EARLY.load(Ordering::Relaxed);
    if !owed && m == 0 && e == 0 {
        return;
    }
    mr.rec(
        b"FaKnob",
        m | (e << 8) | (FOREIGN_OK.load(Ordering::Relaxed) << 16),
    );
    mr.rec(b"FaEarly", EARLY_N.load(Ordering::Relaxed));
    mr.rec(b"FaDdi", DDI.load(Ordering::Relaxed));
    mr.rec(b"FaWorker", WORKER.load(Ordering::Relaxed));
    mr.rec(b"FaRefuse", REFUSE.load(Ordering::Relaxed));
    mr.rec(b"FaLate", LATE.load(Ordering::Relaxed));
    mr.rec(b"FaTick", crate::ddi::flip_lat::announced_ticks());
    mr.rec(b"FaNo", NO.load(Ordering::Relaxed));
    mr.rec(b"FaNoWhy", NO_WHY.load(Ordering::Relaxed));
    mr.rec(b"FaNoBusy", NO_BUSY.load(Ordering::Relaxed));
    mr.rec(b"FaNoUnk", NO_UNK.load(Ordering::Relaxed));
    mr.rec(b"FaNoFail", NO_FAIL.load(Ordering::Relaxed));
    mr.rec(b"FaNoOther", NO_OTHER.load(Ordering::Relaxed));
    mr.rec(b"FaNoFgn", NO_FGN.load(Ordering::Relaxed));
}
