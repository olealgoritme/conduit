//! Independent flip, stage S-1 (`docs/independent-flip.md` section 11): the census of
//! `helios_kmd_logic::independent_flip::decide` over every flip, and the one enforced change of
//! `IndepFlip=2`.
//!
//! The knob's caps (`SupportDirectFlip`, the aperture `DirectFlip` flag, `FlipIndependent |
//! DdiPresentForIFlip`) are folded into `AdapterKnobs` when it is read and need nothing here.
//! This module only counts. Each flip is counted once, where its fate is decided:
//!
//! * in the flip worker (`program_vidpn_source_inner`), for every MMIO flip and every ARMED
//!   DMA-buffer flip, from the resolved allocation ([`worker_pre`], finished for a foreign source
//!   by [`ForeignPending::finish`] once `ForeignFlip` answered);
//! * in `DxgkDdiPresent`, for a DMA-buffer flip it answers without arming the worker (the skip of
//!   a foreign source, a hollow source, the `PBFlip` 0xE6 row): [`count_dma_unarmed`].
//!
//! Atomics only on the counting side (`SetVidPnSourceAddress` can run at DIRQL). The mode is a
//! static set at StartDevice so no counting site needs the adapter's knob snapshot. Event-gated
//! like `Ff*`: the whole block is written as zeros once per StartDevice, then from the periodic
//! `scanout_trace::dump` (PASSIVE) only when a count moved.

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::independent_flip::{
    self as idf, Class, DmaFacts, Mode, Route, Verdict, Why, WorkerFacts,
};

/// `Mode::code()` of the generation in force (0 off).
static MODE: AtomicU32 = AtomicU32::new(0);
/// `SupportDirectFlip` as advertised in this generation (1 / 0).
static CAPS: AtomicU32 = AtomicU32::new(0);
/// Something moved since the last publication.
static DIRTY: AtomicU32 = AtomicU32::new(0);

static SEEN: AtomicU32 = AtomicU32::new(0);
static DIRECT: AtomicU32 = AtomicU32::new(0);
static DIR_FOR: AtomicU32 = AtomicU32::new(0);
static DIR_VEN: AtomicU32 = AtomicU32::new(0);
static COPY: AtomicU32 = AtomicU32::new(0);
static KEEP: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static ARM_MMIO: AtomicU32 = AtomicU32::new(0);
static ARM_DMA: AtomicU32 = AtomicU32::new(0);
static UNTAGGED: AtomicU32 = AtomicU32::new(0);
static ENF_KEEP: AtomicU32 = AtomicU32::new(0);
/// `IdfRedirSkip` in force (1 / 0; only with the mode on).
static REDIR_SKIP: AtomicU32 = AtomicU32::new(0);
static RED_OK: AtomicU32 = AtomicU32::new(0);
static RED_ERR: AtomicU32 = AtomicU32::new(0);
static RED_ST: AtomicU32 = AtomicU32::new(0);
static RED_SD: AtomicU32 = AtomicU32::new(0);
static RED_SKIP: AtomicU32 = AtomicU32::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);
static REFS: [AtomicU32; Why::COUNT] = [ZERO; Why::COUNT];

/// The mode in force. Any IRQL.
pub(crate) fn mode() -> Mode {
    Mode::from_knob(MODE.load(Ordering::Relaxed))
}

fn caps() -> bool {
    CAPS.load(Ordering::Relaxed) != 0
}

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
}

/// A new generation: take the mode from the knob snapshot StartDevice runs with, zero the
/// counters and write the zero block (with `IdfKnob`, 0 included). PASSIVE.
pub(crate) fn reset_for_start(knobs: &crate::adapter::AdapterKnobs) {
    let mode = knobs.indep_flip_mode();
    MODE.store(mode.code(), Ordering::Relaxed);
    CAPS.store(u32::from(knobs.direct_flip), Ordering::Relaxed);
    let skip =
        mode.is_on() && crate::diag::read_config_dword(crate::diag::knobs::IDF_REDIR_SKIP, 0) != 0;
    REDIR_SKIP.store(u32::from(skip), Ordering::Relaxed);
    for c in [
        &SEEN, &DIRECT, &DIR_FOR, &DIR_VEN, &COPY, &KEEP, &WHY, &ARM_MMIO, &ARM_DMA, &UNTAGGED,
        &ENF_KEEP, &RED_OK, &RED_ERR, &RED_ST, &RED_SD, &RED_SKIP,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for c in REFS.iter() {
        c.store(0, Ordering::Relaxed);
    }
    DIRTY.store(0, Ordering::Relaxed);
    write_block();
}

/// Mirror the block if a count moved. PASSIVE; called from `scanout_trace::dump` alone.
pub(crate) fn publish() {
    if !mode().is_on() || DIRTY.swap(0, Ordering::Relaxed) == 0 {
        return;
    }
    write_block();
}

fn write_block() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"IdfKnob", MODE.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"IdfSeen", r(&SEEN));
    crate::diag::record_named_bytes(b"IdfDirect", r(&DIRECT));
    crate::diag::record_named_bytes(b"IdfDirFor", r(&DIR_FOR));
    crate::diag::record_named_bytes(b"IdfDirVen", r(&DIR_VEN));
    crate::diag::record_named_bytes(b"IdfCopy", r(&COPY));
    crate::diag::record_named_bytes(b"IdfKeep", r(&KEEP));
    crate::diag::record_named_bytes(b"IdfWhy", r(&WHY));
    crate::diag::record_named_bytes(b"IdfArmMmio", r(&ARM_MMIO));
    crate::diag::record_named_bytes(b"IdfArmDma", r(&ARM_DMA));
    crate::diag::record_named_bytes(b"IdfUntagged", r(&UNTAGGED));
    crate::diag::record_named_bytes(b"IdfEnfKeep", r(&ENF_KEEP));
    crate::diag::record_named_bytes(b"IdfRedOk", r(&RED_OK));
    crate::diag::record_named_bytes(b"IdfRedErr", r(&RED_ERR));
    crate::diag::record_named_bytes(b"IdfRedSt", r(&RED_ST));
    crate::diag::record_named_bytes(b"IdfRedSD", r(&RED_SD));
    crate::diag::record_named_bytes(b"IdfRedSkip", r(&RED_SKIP));
    for why in Why::ALL {
        crate::diag::record_named_bytes(&idf::ref_name(why), r(&REFS[why.index()]));
    }
}

/// Count one verdict. Any IRQL, atomics only. `Off` counts nothing.
fn count(v: Verdict, primary_tagged: bool) {
    match v {
        Verdict::Off => return,
        Verdict::Direct(route) => {
            bump(&DIRECT);
            bump(match route {
                Route::Foreign => &DIR_FOR,
                Route::VenusBind => &DIR_VEN,
            });
            if !primary_tagged {
                bump(&UNTAGGED);
            }
        }
        Verdict::Copy => bump(&COPY),
        Verdict::Keep(why) => {
            bump(&KEEP);
            WHY.store(why.code(), Ordering::Relaxed);
            bump(&REFS[why.index()]);
        }
    }
    bump(&SEEN);
}

/// A `SetVidPnSourceAddress` call (the MMIO flip contract). Any IRQL, atomics only.
pub(crate) fn note_arm_mmio() {
    if mode().is_on() {
        bump(&ARM_MMIO);
    }
}

/// A flip on the DMA-buffer contract (`DxgkDdiPresent` with a DMA buffer). Atomics only.
pub(crate) fn note_arm_dma() {
    if mode().is_on() {
        bump(&ARM_DMA);
    }
}

/// What one `DxgkDdiPresent` that carried `RedirectedFlip` returned (`flags` its
/// `DXGK_PRESENTFLAGS`, `src`/`dst` its allocation counts). Counted only with the mode on, and
/// only for such presents: the question is whether dxgkrnl's independent-flip candidate presents
/// fail or succeed here. Atomics only.
#[inline]
pub(crate) fn note_present_result(flags: u32, src: u32, dst: u32, ok: bool, status: u32) {
    if !mode().is_on() || !helios_kmd_logic::flip_flags::present_is_redirected(flags) {
        return;
    }
    if ok {
        bump(&RED_OK);
    } else {
        RED_ST.store(status, Ordering::Relaxed);
        bump(&RED_ERR);
    }
    RED_SD.store((src << 16) | (dst & 0xFFFF), Ordering::Relaxed);
}

/// `IdfRedirSkip`: whether this Blt-arm Present (flags `flags`) completes with no copy. Counts
/// the ones it skips. Atomics only.
#[inline]
pub(crate) fn skip_redirected_blt(flags: u32) -> bool {
    if REDIR_SKIP.load(Ordering::Relaxed) == 0 || !idf::redirected_blt(flags) {
        return false;
    }
    bump(&RED_SKIP);
    true
}

/// `IndepFlip=2` completed a `PBFlip` 0xE6 flip as a kept picture. Atomics only.
pub(crate) fn note_enforced_keep() {
    bump(&ENF_KEEP);
}

/// A foreign source's verdict, provisional until `ForeignFlip` (or the level-5 arm) answered.
#[must_use]
pub(crate) struct ForeignPending {
    pre: Verdict,
    primary_tagged: bool,
}

impl ForeignPending {
    /// Count it, now that the arm took it (`took`) or refused it.
    pub(crate) fn finish(self, took: bool) {
        count(idf::finish_foreign(self.pre, took), self.primary_tagged);
    }
}

/// The worker's census for one resolved flip source, before any arm runs. A final verdict is
/// counted at once; a foreign source's provisional one comes back to be finished. `extent` is
/// the extent the worker compares (the mode's when the allocation recorded none). PASSIVE.
pub(crate) fn worker_pre(
    source: &crate::ddi::create_allocation::WindowsPrimary,
    extent: (u32, u32),
    mode_wh: (u32, u32),
) -> Option<ForeignPending> {
    let mode = mode();
    if !mode.is_on() {
        return None;
    }
    let w = WorkerFacts {
        caps: caps(),
        // The worker runs only with the display half on.
        display: true,
        class: Class::of(source.flip_source, source.direct_scanout),
        address: source.primary_address,
        primary_tagged: source.primary_tagged,
        mode: mode_wh,
        extent,
        dxgi_format: source.dxgi_format,
        pitch: source.pitch,
        plane_offset: source.plane_offset,
        alloc_size: source.venus_alloc_size,
    };
    let v = idf::census_worker(mode, &w);
    if v == Verdict::Direct(Route::Foreign) {
        return Some(ForeignPending {
            pre: v,
            primary_tagged: source.primary_tagged,
        });
    }
    count(v, source.primary_tagged);
    None
}

/// The census of a DMA-buffer flip `DxgkDdiPresent` answers without arming the worker. PASSIVE.
pub(crate) fn count_dma_unarmed(
    source: &crate::ddi::create_allocation::PresentAllocInfo,
    registered: bool,
    address: u64,
    display: bool,
    mode_wh: (u32, u32),
) {
    let mode = mode();
    if !mode.is_on() {
        return;
    }
    let flip_source = helios_kmd_logic::flip_completion::classify(
        &helios_kmd_logic::flip_completion::SourceFacts {
            resource_id: source.resource_id,
            foreign: source.foreign_identity,
            direct_scanout: source.direct_scanout,
            width: source.width,
            height: source.height,
            venus_identity: source.venus_alloc_size != 0,
        },
    );
    let d = DmaFacts {
        caps: caps(),
        display,
        class: Class::of(flip_source, source.direct_scanout),
        address,
        registered,
        mode: mode_wh,
        extent: (source.width, source.height),
        dxgi_format: source.resolved_dxgi_format().unwrap_or(0),
    };
    count(idf::census_dma_unarmed(mode, &d), false);
}
