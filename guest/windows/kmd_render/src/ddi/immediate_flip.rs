//! Immediate (tearing) flips (`ImmFlip`, default on): the I/O half of
//! `helios_kmd_logic::immediate_flip` (the rules and why, host-tested).
//!
//! * Each flip records on its allocation whether it waits for a retrace: the MMIO arm its own
//!   `SetVidPnSourceAddress` flags (`FlipImmediate`), the DMA-buffer arm the immediacy of its
//!   Present's `FlipInterval` (0 or `IMMEDIATE_ALLOW_TEARING`, carried in the flip record). Per
//!   allocation, so the coalescing pending slot and its retries keep the right answer. The arms
//!   only count here ([`note_dma_arm`], [`note_mmio`]; atomics, the MMIO arm runs at DIRQL).
//! * The worker reads the record back with the handle ([`begin_programming`]); `ForeignFlip`
//!   reads it when the programming owes it a frame ([`programming_is_immediate`]) and then sends
//!   that frame without the presenter's minimum interval ([`min_interval`]).
//! * When the programming published a NEW address the worker reports a `CRTC_VSYNC` for it at
//!   once ([`complete_now`]), which retires the flip toward dxgkrnl without waiting for the vsync
//!   tick. It is not fed to the `FlipLat` statistics (they model the tick).
//!
//! Counters (`helios_kmd_logic::immediate_flip::COUNTERS`), written from the periodic dump
//! (PASSIVE) when one moved: `ImmKnob` (the knob in force), `ImmDma` / `ImmMmio` (immediate flips
//! armed by each lane), `ImmProg` (immediate programmings the worker took), `ImmVs` (completion
//! vsyncs reported), `ImmVsNo` (immediate programmings that did not get one: nothing new was
//! published, the chain was not live, or `FlipDoneHost` holds addresses).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::immediate_flip as imf;

use crate::adapter::AdapterContext;

/// The knob in force (`imf::mode`).
static MODE: AtomicU32 = AtomicU32::new(imf::DEFAULT_KNOB);
/// The programming the worker is performing now is immediate (set between
/// [`begin_programming`] and [`end_programming`], read by `ForeignFlip`'s `program`).
static CURRENT: AtomicU32 = AtomicU32::new(0);

static DMA: AtomicU32 = AtomicU32::new(0);
static MMIO: AtomicU32 = AtomicU32::new(0);
static PROG: AtomicU32 = AtomicU32::new(0);
static VS: AtomicU32 = AtomicU32::new(0);
static VS_NO: AtomicU32 = AtomicU32::new(0);
static DIRTY: AtomicU32 = AtomicU32::new(1);
/// The address the last completion vsync carried.
static LAST_DONE: AtomicU64 = AtomicU64::new(0);

/// The knob for this start, and zeroed counters. PASSIVE (registry read).
pub(crate) fn start_generation() {
    let knob = crate::diag::read_config_dword(crate::diag::knobs::IMM_FLIP, imf::DEFAULT_KNOB);
    MODE.store(imf::mode(knob), Ordering::Relaxed);
    CURRENT.store(0, Ordering::Relaxed);
    LAST_DONE.store(0, Ordering::Relaxed);
    for c in [&DMA, &MMIO, &PROG, &VS, &VS_NO] {
        c.store(0, Ordering::Relaxed);
    }
    DIRTY.store(1, Ordering::Release);
}

#[inline]
fn mode() -> u32 {
    MODE.load(Ordering::Relaxed)
}

#[inline]
fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// A DMA-buffer flip was armed (`immediate`: its Present's interval). Any IRQL up to DISPATCH.
#[inline]
pub(crate) fn note_dma_arm(immediate: bool) {
    if mode() != 0 && immediate {
        bump(&DMA);
    }
}

/// An MMIO flip with these `SetVidPnSourceAddress` flags was armed. Any IRQL up to DIRQL
/// (atomics only).
#[inline]
pub(crate) fn note_mmio(flags: u32) {
    if mode() != 0 && imf::mmio_is_immediate(flags) {
        bump(&MMIO);
    }
}

/// The worker took an allocation off the pending slot whose newest flip recorded `flags`: its
/// immediacy is the current programming's until [`end_programming`]. Returns it.
pub(crate) fn begin_programming(flags: u32) -> bool {
    let imm = imf::programming_is_immediate(mode(), flags);
    CURRENT.store(u32::from(imm), Ordering::Release);
    if imm {
        bump(&PROG);
    }
    imm
}

/// The worker's programming ended.
pub(crate) fn end_programming() {
    CURRENT.store(0, Ordering::Release);
}

/// The programming in progress is immediate. Read by `ForeignFlip`'s `program`, inside the
/// programming.
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

/// After an immediate programming: if the flip's own address (`flip_address`) is the published
/// one and no completion carried it yet, report the `CRTC_VSYNC` that retires it now instead of
/// at the next tick (`imf::completion_address`). The worker, after the scan-out lifecycle lock was released (PASSIVE);
/// `signal_crtc_vsync` raises to DIRQL through `DxgkCbSynchronizeExecution` itself, as from the
/// tick and from `host_flip_done::service`.
pub(crate) fn complete_now(adapter: &AdapterContext, flip_address: u64) {
    let live = adapter.display_half()
        && adapter.vsync_armed.load(Ordering::Acquire) != 0
        && adapter.vsync_enabled.load(Ordering::Acquire) != 0;
    let host_holds = crate::ddi::host_flip_done::mode().is_on();
    let published = adapter.last_primary_address.load(Ordering::Acquire);
    let address =
        imf::completion_address(published, flip_address, LAST_DONE.load(Ordering::Relaxed))
            .unwrap_or(0);
    if imf::completes_now(mode(), true, live, host_holds, address).is_err() {
        bump(&VS_NO);
        return;
    }
    let Some(dxgkrnl) = adapter.dxgkrnl_opt() else {
        bump(&VS_NO);
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
        bump(&VS_NO);
        return;
    }
    LAST_DONE.store(address, Ordering::Relaxed);
    bump(&VS);
    // dxgkrnl may have issued the next flip inside that callback.
    if adapter.pending_vidpn_allocation.load(Ordering::Acquire) != 0 {
        adapter.signal_hpd();
    }
}

/// The vsync tick delivered a CRTC_VSYNC carrying `address`: the next completion compares with
/// it, so an immediate flip back to an address only the tick retired still completes at once.
#[inline]
pub(crate) fn note_tick_delivered(address: u64) {
    LAST_DONE.store(address, Ordering::Relaxed);
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
