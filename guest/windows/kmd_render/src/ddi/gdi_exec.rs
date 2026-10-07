//! The executor of GDI acceleration (`GdiAccel`): the census of stage G0. The copy-engine and
//! CPU execution of stage G1 lands here; design in `docs/vram-redirection.md` section 9.
//!
//! Written only from PASSIVE (`DxgkDdiRenderKm`, StartDevice); every entry point is inert with the
//! knob off (`gdi_accel::on()` false: nothing is counted or written).

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::gdi_accel::{Cmd, Engine, Surface, SurfaceClass};

static BLT_N: AtomicU32 = AtomicU32::new(0);
static FILL_N: AtomicU32 = AtomicU32::new(0);
static FALL: AtomicU32 = AtomicU32::new(0);
static JOB_N: AtomicU32 = AtomicU32::new(0);
static AGAIN: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static ORPH: AtomicU32 = AtomicU32::new(0);
static CE_SUB: AtomicU32 = AtomicU32::new(0);
static US: AtomicU32 = AtomicU32::new(0);
static US_MAX: AtomicU32 = AtomicU32::new(0);
static RECTS: AtomicU32 = AtomicU32::new(0);
static CLS: AtomicU32 = AtomicU32::new(0);
static DST_RES: AtomicU32 = AtomicU32::new(0);
static DST_WH: AtomicU32 = AtomicU32::new(0);

/// StartDevice (PASSIVE): zero everything.
pub(crate) fn reset_for_start(_on: bool) {
    for c in [
        &BLT_N, &FILL_N, &FALL, &JOB_N, &AGAIN, &DONE, &ORPH, &CE_SUB, &US, &US_MAX, &RECTS, &CLS,
        &DST_RES, &DST_WH,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

/// PASSIVE; called by `gdi_accel::publish_counters` with the knob on.
pub(crate) fn publish_counters() {
    let w = crate::diag::record_named_bytes;
    w(b"GdiBltN", BLT_N.load(Ordering::Relaxed));
    w(b"GdiFillN", FILL_N.load(Ordering::Relaxed));
    w(b"GdiFall", FALL.load(Ordering::Relaxed));
    w(b"GdiJobN", JOB_N.load(Ordering::Relaxed));
    w(b"GdiAgain", AGAIN.load(Ordering::Relaxed));
    w(b"GdiDone", DONE.load(Ordering::Relaxed));
    w(b"GdiOrph", ORPH.load(Ordering::Relaxed));
    w(b"GdiCeSub", CE_SUB.load(Ordering::Relaxed));
    w(b"GdiUs", US.load(Ordering::Relaxed));
    w(b"GdiUsMax", US_MAX.load(Ordering::Relaxed));
    w(b"GdiRects", RECTS.load(Ordering::Relaxed));
    w(b"GdiCls", CLS.load(Ordering::Relaxed));
    w(b"GdiDstRes", DST_RES.load(Ordering::Relaxed));
    w(b"GdiDstWH", DST_WH.load(Ordering::Relaxed));
}

fn class_bit(c: SurfaceClass) -> u32 {
    match c {
        SurfaceClass::Vram => 1,
        SurfaceClass::System => 2,
        SurfaceClass::Unreachable => 4,
    }
}

/// One parsed command's census (RenderKm, PASSIVE).
pub(crate) fn note_census(cmd: &Cmd, _engine: Engine, dst: Option<&Surface>, srcs: [Option<&Surface>; 2]) {
    RECTS.fetch_add(cmd.subs().count().max(1), Ordering::Relaxed);
    if let Some(d) = dst {
        CLS.fetch_or(class_bit(d.class), Ordering::Relaxed);
        DST_RES.store(d.resource_id, Ordering::Relaxed);
        DST_WH.store((d.width.min(0xffff) << 16) | d.height.min(0xffff), Ordering::Relaxed);
    }
    for s in srcs.into_iter().flatten() {
        CLS.fetch_or(class_bit(s.class) << 4, Ordering::Relaxed);
    }
}
