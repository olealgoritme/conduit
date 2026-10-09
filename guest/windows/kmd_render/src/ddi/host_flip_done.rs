//! Flip completion from the host's presentation feedback (`FlipDoneHost`, default 0): the I/O
//! half of `helios_kmd_logic::host_flip_done` (the rules, host-tested) and
//! `docs/independent-flip.md` section 13 (the design and the test recipe).
//!
//! Four places feed it, all atomics (any IRQL up to DISPATCH; the tick and the DPC run there):
//!
//! * every address publication (`stall_diag::note_published`, the one funnel every bound, kept
//!   or announced publication passes) records the address, its time and the `seq` floor
//!   ([`note_published`]); a kept picture says so ([`note_kept`]);
//! * every `ScanoutFlip` minted (`virtio::scanout_release::minted`) raises the floor's source
//!   ([`note_minted`]);
//! * the event-queue drain (under the virtio lock) leaves the newest `ScanoutPresented` in a
//!   one-slot mailbox ([`on_event`]); `drain_used_and_complete`, after the lock is released,
//!   consumes it ([`service`]): it records which address the host confirmed and, in `Mode::On`,
//!   delivers a CRTC_VSYNC for it at once and moves the timer's phase;
//! * the vsync tick asks which address to report and whether to deliver at all ([`tick_plan`]).
//!
//! With the knob at 0 every hook is one relaxed load and the tick reports
//! `last_primary_address` exactly as before. The feature bit is not even acked then, so the
//! host sends nothing.
//!
//! Counters (`Fdh*`, `helios_kmd_logic::host_flip_done::COUNTERS`): written as zeros at every
//! StartDevice, then from the periodic `scanout_trace` dump (PASSIVE) when one moved.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::host_flip_done::{self as hfd, Latch, Mode, Presented, Publication, TickWhy};

use crate::adapter::AdapterContext;

/// `Mode::code()` in force.
static MODE: AtomicU32 = AtomicU32::new(0);
/// The feature was acked this generation (1 / 0).
static ACK: AtomicU32 = AtomicU32::new(0);
/// Something moved since the last publication.
static DIRTY: AtomicU32 = AtomicU32::new(0);

// The publication the vsync would report without the knob.
static PUB_ADDR: AtomicU64 = AtomicU64::new(0);
static PUB_AT: AtomicU64 = AtomicU64::new(0);
static PUB_FLOOR: AtomicU64 = AtomicU64::new(0);
static PUB_KEPT: AtomicU32 = AtomicU32::new(0);
/// The highest `ScanoutFlip::seq` minted (never reset: seqs only grow).
static MAX_MINTED: AtomicU64 = AtomicU64::new(0);

// The feedback.
/// The address the host last confirmed (0 none this generation).
static LATCHED: AtomicU64 = AtomicU64::new(0);
/// Feedback is live before this time (0 never).
static ACTIVE_UNTIL: AtomicU64 = AtomicU64::new(0);
/// When the last report was serviced.
static LAST_REPORT: AtomicU64 = AtomicU64::new(0);
/// When the last CRTC_VSYNC went out, from either source (Mode::On only).
static LAST_VSYNC: AtomicU64 = AtomicU64::new(0);

// The mailbox: the newest event, written under the virtio lock, read after it.
static MBOX_GEN: AtomicU32 = AtomicU32::new(0);
static MBOX_SEEN: AtomicU32 = AtomicU32::new(0);
static MBOX_SEQ: AtomicU64 = AtomicU64::new(0);
/// `flags << 32 | host_handle`.
static MBOX_ID: AtomicU64 = AtomicU64::new(0);

// Counters.
static EV_N: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static UNASK: AtomicU32 = AtomicU32::new(0);
static LATCH: AtomicU32 = AtomicU32::new(0);
static STALE: AtomicU32 = AtomicU32::new(0);
static VSYNC: AtomicU32 = AtomicU32::new(0);
static COAL: AtomicU32 = AtomicU32::new(0);
static REPHASE: AtomicU32 = AtomicU32::new(0);
static TK_SKIP: AtomicU32 = AtomicU32::new(0);
static HELD: AtomicU32 = AtomicU32::new(0);
static TMO: AtomicU32 = AtomicU32::new(0);
static KEPT: AtomicU32 = AtomicU32::new(0);
static INACT: AtomicU32 = AtomicU32::new(0);
static LAT_US: AtomicU32 = AtomicU32::new(0);
static LAT_MAX: AtomicU32 = AtomicU32::new(0);
static LAT_SUM: AtomicU64 = AtomicU64::new(0);
static AGE_US: AtomicU32 = AtomicU32::new(0);

/// The mode in force. Any IRQL.
#[inline]
pub(crate) fn mode() -> Mode {
    Mode::from_knob(MODE.load(Ordering::Relaxed))
}

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// Whether StartDevice should ack `NVGPU_F_SCANOUT_PRESENTED` for this knob value.
pub(crate) fn wants_feature(knob: u32, display_half: bool) -> bool {
    hfd::wants_feature(Mode::from_knob(knob), display_half)
}

/// A new generation: the mode (forced off when the host did not take the ack, so nothing is held
/// for reports that will never come), every value and counter zeroed, the zero block written.
/// PASSIVE.
pub(crate) fn reset_for_start(knob: u32, acked: bool) {
    let mode = if acked {
        Mode::from_knob(knob)
    } else {
        Mode::Off
    };
    // Off first, so no hook acts on half-reset state.
    MODE.store(0, Ordering::Relaxed);
    for c in [
        &PUB_ADDR,
        &PUB_AT,
        &PUB_FLOOR,
        &LATCHED,
        &ACTIVE_UNTIL,
        &LAST_REPORT,
        &LAST_VSYNC,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    PUB_KEPT.store(0, Ordering::Relaxed);
    MBOX_SEEN.store(MBOX_GEN.load(Ordering::Relaxed), Ordering::Relaxed);
    for c in [
        &EV_N, &BAD, &UNASK, &LATCH, &STALE, &VSYNC, &COAL, &REPHASE, &TK_SKIP, &HELD, &TMO, &KEPT,
        &INACT, &LAT_US, &LAT_MAX, &AGE_US,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    LAT_SUM.store(0, Ordering::Relaxed);
    ACK.store(u32::from(acked), Ordering::Relaxed);
    MODE.store(mode.code(), Ordering::Release);
    DIRTY.store(0, Ordering::Relaxed);
    write_block(knob);
}

/// Mirror the block if something moved. PASSIVE; from `scanout_trace::dump` alone.
pub(crate) fn publish() {
    if DIRTY.swap(0, Ordering::Relaxed) == 0 {
        return;
    }
    write_block(MODE.load(Ordering::Relaxed));
}

fn write_block(knob: u32) {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    let latches = u64::from(r(&LATCH));
    let avg = if latches == 0 {
        0
    } else {
        (LAT_SUM.load(Ordering::Relaxed) / latches).min(u64::from(u32::MAX)) as u32
    };
    crate::diag::record_named_bytes(b"FdhKnob", knob);
    crate::diag::record_named_bytes(b"FdhAck", r(&ACK));
    crate::diag::record_named_bytes(b"FdhEvN", r(&EV_N));
    crate::diag::record_named_bytes(b"FdhBad", r(&BAD));
    crate::diag::record_named_bytes(b"FdhUnask", r(&UNASK));
    crate::diag::record_named_bytes(b"FdhLatch", r(&LATCH));
    crate::diag::record_named_bytes(b"FdhStale", r(&STALE));
    crate::diag::record_named_bytes(b"FdhVsync", r(&VSYNC));
    crate::diag::record_named_bytes(b"FdhCoal", r(&COAL));
    crate::diag::record_named_bytes(b"FdhRephase", r(&REPHASE));
    crate::diag::record_named_bytes(b"FdhTkSkip", r(&TK_SKIP));
    crate::diag::record_named_bytes(b"FdhHeld", r(&HELD));
    crate::diag::record_named_bytes(b"FdhTmo", r(&TMO));
    crate::diag::record_named_bytes(b"FdhKept", r(&KEPT));
    crate::diag::record_named_bytes(b"FdhInact", r(&INACT));
    crate::diag::record_named_bytes(b"FdhLatUs", r(&LAT_US));
    crate::diag::record_named_bytes(b"FdhLatMax", r(&LAT_MAX));
    crate::diag::record_named_bytes(b"FdhLatAvg", avg);
    crate::diag::record_named_bytes(b"FdhAgeUs", r(&AGE_US));
}

// ---- the hooks ------------------------------------------------------------------------------

/// A `ScanoutFlip` with this `seq` was minted. Atomics only, any IRQL.
#[inline]
pub(crate) fn note_minted(seq: u64) {
    if !mode().is_on() {
        return;
    }
    MAX_MINTED.fetch_max(seq, Ordering::Relaxed);
}

/// `address` became the published (`last_primary_address`) one. Atomics only, any IRQL. Racing
/// publications may mix their fields; the fallback bounds what that can cost (one flip held at
/// most `FALLBACK_PERIODS`).
#[inline]
pub(crate) fn note_published(address: u64) {
    if !mode().is_on() {
        return;
    }
    if PUB_ADDR.load(Ordering::Relaxed) == address {
        // A re-publication of the address already published (a re-present of the same buffer, a
        // confirmation of an announced flip): it keeps its first time and floor.
        return;
    }
    PUB_FLOOR.store(
        MAX_MINTED.load(Ordering::Relaxed).saturating_add(1),
        Ordering::Relaxed,
    );
    PUB_AT.store(
        crate::adapter::foreign_scanout::now_100ns(),
        Ordering::Relaxed,
    );
    PUB_KEPT.store(0, Ordering::Relaxed);
    PUB_ADDR.store(address, Ordering::Release);
}

/// `address` was published as a kept picture: the host is never told, so no report will confirm
/// it. Atomics only, any IRQL.
#[inline]
pub(crate) fn note_kept(address: u64) {
    if !mode().is_on() {
        return;
    }
    if PUB_ADDR.load(Ordering::Acquire) == address {
        PUB_KEPT.store(1, Ordering::Release);
    }
}

fn publication() -> Publication {
    Publication {
        address: PUB_ADDR.load(Ordering::Acquire),
        at: PUB_AT.load(Ordering::Relaxed),
        seq_floor: PUB_FLOOR.load(Ordering::Relaxed),
        kept: PUB_KEPT.load(Ordering::Acquire) != 0,
    }
}

// ---- the event queue ------------------------------------------------------------------------

/// One `ScanoutPresented` from the drain (under the virtio lock, DISPATCH): left in the mailbox
/// for [`service`]. Atomics only.
pub(crate) fn on_event(p: &Presented) {
    bump(&EV_N);
    if let Some(age) = p.host_age_us() {
        AGE_US.store(age, Ordering::Relaxed);
    }
    if p.scanout != 0 {
        bump(&STALE);
        return;
    }
    MBOX_SEQ.store(p.seq, Ordering::Relaxed);
    MBOX_ID.store(
        (u64::from(p.flags) << 32) | u64::from(p.host_handle),
        Ordering::Relaxed,
    );
    MBOX_GEN.fetch_add(1, Ordering::Release);
}

/// A short `ScanoutPresented` (dropped).
pub(crate) fn note_bad() {
    bump(&BAD);
}

/// A `ScanoutPresented` from a host that was not asked (dropped).
pub(crate) fn note_unasked() {
    bump(&UNASK);
}

/// Consume the newest report, if one is waiting: record what it confirms and, in `Mode::On`,
/// deliver its CRTC_VSYNC and move the timer's phase. Called by `drain_used_and_complete` after
/// the virtio lock is released (the DPC at DISPATCH, or the worker at PASSIVE); one relaxed load
/// when nothing is waiting. Two concurrent callers: one claims the report, the other returns.
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
    let mode = mode();
    if !mode.is_on() {
        return;
    }
    let id = MBOX_ID.load(Ordering::Relaxed);
    let p = Presented {
        scanout: 0,
        flags: (id >> 32) as u32,
        owner_handle: 0,
        host_handle: id as u32,
        seq: MBOX_SEQ.load(Ordering::Relaxed),
        present_ns: 0,
        sent_ns: 0,
    };
    let now = crate::adapter::foreign_scanout::now_100ns();
    let publ = publication();
    LAST_REPORT.store(now, Ordering::Relaxed);
    ACTIVE_UNTIL.store(now.saturating_add(hfd::HOLDOVER_100NS), Ordering::Relaxed);
    match hfd::latch(
        &p,
        &publ,
        [
            adapter.active_scanout_resource.load(Ordering::Acquire),
            adapter.host_bound_scanout_resource.load(Ordering::Acquire),
        ],
    ) {
        Latch::Address(a) => {
            if LATCHED.swap(a, Ordering::AcqRel) != a {
                bump(&LATCH);
                let lat = hfd::us_from_100ns(now.saturating_sub(publ.at));
                LAT_US.store(lat, Ordering::Relaxed);
                LAT_MAX.fetch_max(lat, Ordering::Relaxed);
                LAT_SUM.fetch_add(u64::from(lat), Ordering::Relaxed);
            } else {
                bump(&STALE);
            }
        }
        Latch::Stale => bump(&STALE),
    }
    if mode != Mode::On {
        return;
    }
    let period = adapter.vsync_period_100ns();
    let live = adapter.display_half()
        && adapter.vsync_armed.load(Ordering::Acquire) != 0
        && adapter.vsync_enabled.load(Ordering::Acquire) != 0;
    if live && hfd::host_delivers(mode, now, LAST_VSYNC.load(Ordering::Relaxed), period) {
        if let Some(dxgkrnl) = adapter.dxgkrnl_opt() {
            let (phys, _) = pick_address(
                mode,
                adapter.last_primary_address.load(Ordering::Acquire),
                now,
                period,
            );
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
                LAST_VSYNC.store(now, Ordering::Relaxed);
                bump(&VSYNC);
                // As at the end of the tick: dxgkrnl may have issued the next MMIO flip inside
                // that callback; the PASSIVE worker programs it.
                if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
                    adapter.signal_hpd();
                }
            } else {
                bump(&COAL);
            }
        }
    } else {
        bump(&COAL);
    }
    if let Some(deadline) = hfd::rephase(
        mode,
        now,
        adapter.vsync_deadline_100ns.load(Ordering::Acquire),
        period,
    ) {
        if adapter.rephase_vsync(deadline, now) {
            bump(&REPHASE);
        }
    }
}

// ---- the tick -------------------------------------------------------------------------------

/// What one vsync tick does.
pub(crate) struct TickPlan {
    /// The address its CRTC_VSYNC carries.
    pub address: u64,
    /// Whether it delivers one at all (`false`: a report's vsync was this period's).
    pub deliver: bool,
}

/// The address a vsync carries now, given the published one (`last_primary_address`). The
/// publication the hooks saw is the authority for its time and floor; the address is the one the
/// caller read (they agree unless a publication is racing, or the address was stored without
/// passing the hooks: the restart seed, which is then not held).
fn pick_address(mode: Mode, published: u64, now: u64, period: u64) -> (u64, TickWhy) {
    let mut publ = publication();
    if publ.address != published {
        publ = Publication {
            address: published,
            at: now,
            seq_floor: 0,
            kept: true,
        };
    }
    hfd::tick_address(
        mode,
        now,
        period,
        ACTIVE_UNTIL.load(Ordering::Relaxed),
        &publ,
        LATCHED.load(Ordering::Acquire),
    )
}

/// The tick's plan, given the published address and the tick's time. DISPATCH, atomics only;
/// with the knob off one relaxed load and `(published, true)`.
#[inline]
pub(crate) fn tick_plan(published: u64, now: u64, period: u64) -> TickPlan {
    let mode = mode();
    if !mode.is_on() {
        return TickPlan {
            address: published,
            deliver: true,
        };
    }
    let (address, why) = pick_address(mode, published, now, period);
    match why {
        TickWhy::Held => bump(&HELD),
        TickWhy::Timeout => bump(&TMO),
        TickWhy::Kept => bump(&KEPT),
        // Every tick while no feedback flows: counted, but it alone does not make the block
        // dirty (it would rewrite it at every dump on an idle desktop).
        TickWhy::Inactive => {
            INACT.fetch_add(1, Ordering::Relaxed);
        }
        TickWhy::Latched | TickWhy::Off => {}
    }
    let deliver = hfd::tick_delivers(mode, now, LAST_REPORT.load(Ordering::Relaxed), period);
    if !deliver {
        bump(&TK_SKIP);
    }
    TickPlan { address, deliver }
}

/// The tick delivered its CRTC_VSYNC at `now`. Atomics only.
#[inline]
pub(crate) fn note_tick_delivered(now: u64) {
    if mode() == Mode::On {
        LAST_VSYNC.store(now, Ordering::Relaxed);
    }
}
