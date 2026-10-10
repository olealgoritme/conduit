//! Immediate (tearing) flips (`ImmFlip`, default on): the I/O half of
//! `helios_kmd_logic::immediate_flip` (the rules and why, host-tested).
//!
//! * The flip arms record whether the flip they put in the pending programming slot is immediate:
//!   the DMA-buffer flip ([`note_dma_arm`]; dxgkrnl issues interval-0 flips that way) and an MMIO
//!   flip with `FlipImmediate` ([`note_mmio`]). Atomics only: the MMIO arm runs at DIRQL.
//! * The worker takes the slot's immediacy with the slot ([`begin_programming`]); `ForeignFlip`
//!   reads it when the programming owes it a frame ([`programming_is_immediate`]) and then sends
//!   that frame without the presenter's minimum interval (`helios_kmd_logic::immediate_flip::
//!   min_interval`).
//! * After the programming published the flip's address the worker reports a `CRTC_VSYNC` for it
//!   at once ([`complete_now`]), which retires the flip toward dxgkrnl without waiting for the
//!   vsync tick.
//!
//! Counters (`helios_kmd_logic::immediate_flip::COUNTERS`), written from the periodic dump
//! (PASSIVE) when one moved: `ImmKnob` (the knob in force), `ImmDma` / `ImmMmio` (immediate flips
//! armed by each lane), `ImmProg` (immediate programmings the worker took), `ImmVs` (completion
//! vsyncs reported), `ImmVsNo` (immediate programmings that did not get one: the chain was not
//! live, `FlipDoneHost` holds, or nothing was published).

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::immediate_flip as imf;

use crate::adapter::AdapterContext;

/// The knob in force (`imf::mode`).
static MODE: AtomicU32 = AtomicU32::new(imf::DEFAULT_KNOB);
/// The flip in the pending slot is immediate.
static PENDING: AtomicU32 = AtomicU32::new(0);
/// The programming the worker is performing now is immediate (set between
/// [`begin_programming`] and [`end_programming`], read by `ForeignFlip`'s `program`).
static CURRENT: AtomicU32 = AtomicU32::new(0);

static DMA: AtomicU32 = AtomicU32::new(0);
static MMIO: AtomicU32 = AtomicU32::new(0);
static PROG: AtomicU32 = AtomicU32::new(0);
static VS: AtomicU32 = AtomicU32::new(0);
static VS_NO: AtomicU32 = AtomicU32::new(0);
static DIRTY: AtomicU32 = AtomicU32::new(1);

/// The knob for this start, and zeroed counters. PASSIVE (registry read).
pub(crate) fn start_generation() {
    let knob = crate::diag::read_config_dword(crate::diag::knobs::IMM_FLIP, imf::DEFAULT_KNOB);
    MODE.store(imf::mode(knob), Ordering::Relaxed);
    PENDING.store(0, Ordering::Relaxed);
    CURRENT.store(0, Ordering::Relaxed);
    for c in [&DMA, &MMIO, &PROG, &VS, &VS_NO] {
        c.store(0, Ordering::Relaxed);
    }
    DIRTY.store(1, Ordering::Release);
}

#[inline]
fn mode() -> u32 {
    MODE.load(Ordering::Relaxed)
}

/// A DMA-buffer flip is being put in the pending slot. Any IRQL (the submit path, DISPATCH).
#[inline]
pub(crate) fn note_dma_arm() {
    if mode() == 0 {
        return;
    }
    PENDING.store(u32::from(imf::pending_after(true)), Ordering::Release);
    DMA.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// An MMIO flip with these `SetVidPnSourceAddress` flags is being put in the pending slot. Any
/// IRQL up to DIRQL (atomics only).
#[inline]
pub(crate) fn note_mmio(flags: u32) {
    if mode() == 0 {
        return;
    }
    let imm = imf::mmio_is_immediate(flags);
    PENDING.store(u32::from(imf::pending_after(imm)), Ordering::Release);
    if imm {
        MMIO.fetch_add(1, Ordering::Relaxed);
        DIRTY.store(1, Ordering::Relaxed);
    }
}

/// The worker took the pending slot: its immediacy goes with it, and is the current
/// programming's until [`end_programming`]. Returns it.
pub(crate) fn begin_programming() -> bool {
    let imm = PENDING.swap(0, Ordering::AcqRel) != 0 && mode() != 0;
    CURRENT.store(u32::from(imm), Ordering::Release);
    if imm {
        PROG.fetch_add(1, Ordering::Relaxed);
        DIRTY.store(1, Ordering::Relaxed);
    }
    imm
}

/// The worker's programming ended.
pub(crate) fn end_programming() {
    CURRENT.store(0, Ordering::Release);
}

/// The programming in progress is immediate and its host flip is not to be paced
/// (`imf::NO_PACE`). Read by `ForeignFlip`'s `program`, inside the programming.
#[inline]
pub(crate) fn programming_is_immediate() -> bool {
    CURRENT.load(Ordering::Acquire) != 0
}

/// The presenter's minimum interval for a frame owed by a programming whose immediacy was
/// `immediate`.
#[inline]
pub(crate) fn min_interval(paced_100ns: u64, immediate: bool) -> u64 {
    imf::min_interval(paced_100ns, immediate, mode())
}

/// After an immediate programming published its address: report the `CRTC_VSYNC` that retires
/// it now instead of at the next tick. The worker, after the scan-out lifecycle lock was
/// released (PASSIVE); `signal_crtc_vsync` raises to DIRQL through `DxgkCbSynchronizeExecution`
/// itself, as from the tick and from `host_flip_done::service`.
pub(crate) fn complete_now(adapter: &AdapterContext) {
    let live = adapter.display_half()
        && adapter.vsync_armed.load(Ordering::Acquire) != 0
        && adapter.vsync_enabled.load(Ordering::Acquire) != 0;
    let host_holds = crate::ddi::host_flip_done::mode().is_on();
    let address = adapter.last_primary_address.load(Ordering::Acquire);
    if imf::completes_now(mode(), true, live, host_holds, address).is_err() {
        VS_NO.fetch_add(1, Ordering::Relaxed);
        DIRTY.store(1, Ordering::Relaxed);
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        VS_NO.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SAFETY: live callback interface; callable at <= DIRQL (it synchronizes itself).
    let status = unsafe {
        crate::ddi::submit_command::signal_crtc_vsync(
            dxgkrnl,
            address as i64,
            crate::ddi::vidpn::CHILD_UID,
        )
    };
    if status != wdk_sys::STATUS_SUCCESS {
        VS_NO.fetch_add(1, Ordering::Relaxed);
        DIRTY.store(1, Ordering::Relaxed);
        return;
    }
    VS.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
    let now = crate::adapter::foreign_scanout::now_100ns();
    // `FlipLat*`: the flip with this address retired here.
    crate::ddi::flip_lat::on_delivered_tick(
        adapter,
        address,
        now,
        now,
        adapter.vsync_period_100ns(),
    );
    // dxgkrnl may have issued the next flip inside that callback.
    if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }
}

/// Mirror the counters into the service key when one moved. PASSIVE (the periodic dump).
pub(crate) fn publish_counters() {
    if DIRTY.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    let values = [
        MODE.load(Ordering::Relaxed),
        DMA.load(Ordering::Relaxed),
        MMIO.load(Ordering::Relaxed),
        PROG.load(Ordering::Relaxed),
        VS.load(Ordering::Relaxed),
        VS_NO.load(Ordering::Relaxed),
    ];
    for (name, v) in imf::COUNTERS.iter().zip(values) {
        crate::diag::record_named_bytes(name.as_bytes(), v);
    }
}
