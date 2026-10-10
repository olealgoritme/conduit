//! The executor of GDI acceleration (`GdiAccel` = 1, stage G1): the job table RenderKm fills, the
//! admission at SubmitCommand, the gate on the WDDM fence, and the execution on the HPD worker.
//! Pure rules: `helios_kmd_logic::gdi_accel`; design: `docs/vram-redirection.md` section 10.
//!
//! LIFE OF A JOB.
//! 1. `DxgkDdiRenderKm` (PASSIVE) parses the command buffer and [`commit`]s a job: its commands
//!    with their surfaces resolved, sub-rectangles materialised and clipped, and the engine of each
//!    planned. The job id goes into the DMA buffer's private data (`gdi_accel::Private`).
//! 2. `DxgkDdiSubmitCommand` (DISPATCH) reads the id and [`admit`]s it: the first submission gives
//!    it the next sequence of the [`Timeline`] and wakes the worker; a preempted replay gets the
//!    same sequence; an id no longer in the table (already executed) gets no wait. A submission
//!    without the record (RenderGdi's `SubmitCommandVirtual`) admits the job RenderGdi rendered
//!    into the submitted DMA buffer, found by the buffer's GPU VA, and waits for the context's
//!    newest unfinished job ([`admit_unclaimed`]), which also keeps a preempted buffer's
//!    resubmission waiting for its job. The WDDM fence
//!    of that submission is gated on `completed >= seq` (`WddmPending::gdi_seq`, `virtio/gpu`),
//!    which [`seq_ready`] answers with one atomic load.
//! 3. The HPD worker ([`service`], PASSIVE) executes admitted jobs in sequence order: copies and
//!    fills between VRAM surfaces on the copy engine (one push per batch of rectangles, waited for
//!    at most `CE_DEADLINE_MS`), everything else on the CPU through the surfaces' CPU views
//!    (bounce buffer for VRAM, the standard buffer's authoritative view for staging). Then the
//!    watermark advances and a completion DPC is requested, which retires the fence.
//!
//! WHY DXGKRNL CANNOT HANG ON A JOB. Every admitted job completes: an operation that fails on the
//! copy engine is redone on the CPU, one that fails there is dropped (counted), a copy that does
//! not complete in time is abandoned (`Why::Timeout`). StopDevice discharges every sequence
//! ([`discharge_all`]). Unclaimed jobs (a buffer rendered and never submitted) are dropped above
//! `MAX_UNCLAIMED`, oldest first (`GdiOrph`); they gate nothing.
//!
//! LOCKING. `TABLE` is a leaf spinlock over the jobs and the timeline: no I/O and no other lock
//! under it; a job's commands are moved out of it before execution. `COMPLETED` mirrors the
//! timeline's watermark for the lock-free fence gate.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use alloc::vec::Vec;

use helios_kmd_logic::ce_present as cp;
use helios_kmd_logic::gdi_accel::{self as ga, cpu, CeView, Cmd, Engine, Rect, Surface, SurfaceClass, Timeline, Why};

use crate::adapter::AdapterContext;
use crate::ddi::gdi_ce_glue as glue;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;

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
/// Channel bring-ups the executor asked for, and why the last copy-engine attempt failed (1
/// channel not up, 2 the destination's mapping, 3 the source's mapping, 4 the submission, 5 the
/// wait; 16 + `channel_state` when the channel could not be brought up).
static CH_UP: AtomicU32 = AtomicU32::new(0);
static CE_WHY: AtomicU32 = AtomicU32::new(0);
/// Copies from a VRAM surface into a standard buffer (a GDI readback: a screen or window read).
static RD_BACK: AtomicU32 = AtomicU32::new(0);
/// Copies and fills with a staging buffer on one side done on the copy engine over its system
/// pages; refused (CPU instead: not system-resident, partial leases, busy, staging to staging);
/// failed after the mapping (submit or wait; CPU instead).
static SYS_CE: AtomicU32 = AtomicU32::new(0);
static SYS_REF: AtomicU32 = AtomicU32::new(0);
static SYS_FAIL: AtomicU32 = AtomicU32::new(0);
/// Why the last staging copy was not done on the copy engine (`GdiSysWhy`): 1 staging to staging,
/// 2 the VRAM side's mapping, 3 the channel down, else the `fail_word` of `ce_sysmem`'s refusal;
/// and every class seen (`GdiSysMsk`: 1 staging to staging, 2 VRAM side, 4 not system-resident, 8
/// uncovered, 16 busy or no channel, 32 RM unsure, 64 other, 128 channel down).
static SYS_WHY: AtomicU32 = AtomicU32::new(0);
static SYS_MSK: AtomicU32 = AtomicU32::new(0);
/// The channel bring-up's own time (µs, outside any job's `GdiUs`), the slowest command's time and
/// its signature (opcode | engine << 4 | dst class bit << 8 | src class bit << 12 | sub-rects << 16,
/// at most 255; bit 24: over 1 MPixel).
static CH_UP_US: AtomicU32 = AtomicU32::new(0);
static SLOW_US: AtomicU32 = AtomicU32::new(0);
static SLOW_OP: AtomicU32 = AtomicU32::new(0);
/// The slowest command's reason and raster operation ([`rop_key`]).
static SLOW_ROP: AtomicU32 = AtomicU32::new(0);
/// Commands the CPU executor ran: the reasons seen (bit per `Why` code, as `GdiMask`) and the last
/// one's [`rop_key`].
static CPU_MSK: AtomicU32 = AtomicU32::new(0);
static CPU_ROP: AtomicU32 = AtomicU32::new(0);
/// The slowest staging copy-engine command (`run_ce_sys`): its total µs, the µs to its views
/// (`ce_sysmem` resolve), to its last submit, in the wait, and its pixels.
static SYS_US: AtomicU32 = AtomicU32::new(0);
static SYS_VW_US: AtomicU32 = AtomicU32::new(0);
static SYS_SUB_US: AtomicU32 = AtomicU32::new(0);
static SYS_WT_US: AtomicU32 = AtomicU32::new(0);
static SYS_PX: AtomicU32 = AtomicU32::new(0);
/// Its surfaces: kinds (source `kind_bits` low 16 | destination's << 16), extents (width << 16 |
/// height, source and destination), resource ids (source low 16 | destination << 16), and shape
/// (sub-rectangles, at most 0xffff | same buffer << 16 | covers the whole destination << 17 |
/// source and destination the same extent << 18).
static SYS_K: AtomicU32 = AtomicU32::new(0);
/// Windows of foreign NVK images the CPU executor read and wrote (`Why::Foreign`, read-modify-
/// write through `ce_vram::foreign_transfer`), and transfers that failed.
static FGN_RMW_RD: AtomicU32 = AtomicU32::new(0);
static FGN_RMW_WR: AtomicU32 = AtomicU32::new(0);
static FGN_RMW_FAIL: AtomicU32 = AtomicU32::new(0);
static SYS_SWH: AtomicU32 = AtomicU32::new(0);
static SYS_DWH: AtomicU32 = AtomicU32::new(0);
static SYS_RES: AtomicU32 = AtomicU32::new(0);
static SYS_SHAPE: AtomicU32 = AtomicU32::new(0);
/// The last `wait_last`'s µs (read by `run_ce_sys` for its split; executor thread only).
static LAST_WAIT_US: AtomicU32 = AtomicU32::new(0);
/// Copies from a foreign NVK image done on the copy engine; failed (dropped); the last failing step.
static FGN_CE: AtomicU32 = AtomicU32::new(0);
static FGN_FAIL: AtomicU32 = AtomicU32::new(0);
static FGN_WHY: AtomicU32 = AtomicU32::new(0);
/// Copies INTO a foreign NVK image done on the copy engine (failures share `GdiFgnFail`, with
/// `GdiFgnWhy` 16 + the step).
static FGN_WR: AtomicU32 = AtomicU32::new(0);
/// Overlapping copies inside one surface (scrolls) seen, done on the copy engine as ordered
/// non-overlapping bands, and the last refusal (1 not VRAM/staging, 2 view refused, 3 submit,
/// 4 wait, 5 GdiOvl 0, 6 channel down).
/// The slowest job's breakdown: its command count and its three slowest commands (signature as
/// `GdiSlowOp`, time in µs).
static JOB_N_OPS: AtomicU32 = AtomicU32::new(0);
static JOB_T: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
/// The pixel self-check: after a fill or copy that reported success, one destination pixel is read
/// back and compared with what the command should have written (the fill's color, the copy's
/// source pixel; the low 24 bits). Sampled: the first 16 checkable commands, then every 32nd.
/// Checks made, mismatches, the last mismatch's command (`GdiSlowOp` signature | path << 28: 1 CE,
/// 2 staging CE view, 3 CPU, 4 scroll bands), the pixel read and the pixel wanted.
static CHK_SEEN: AtomicU32 = AtomicU32::new(0);
static CHK_N: AtomicU32 = AtomicU32::new(0);
static CHK_BAD: AtomicU32 = AtomicU32::new(0);
static CHK_K: AtomicU32 = AtomicU32::new(0);
static CHK_GOT: AtomicU32 = AtomicU32::new(0);
static CHK_WANT: AtomicU32 = AtomicU32::new(0);
/// Of the checked pixels in GPU surfaces (VRAM, foreign): alpha byte 0, alpha byte 0xff, and the
/// last such pixel read (all 32 bits). Commands that wrote an opaque alpha (`ga::opaque_alpha`).
static CHK_A0: AtomicU32 = AtomicU32::new(0);
static CHK_AFF: AtomicU32 = AtomicU32::new(0);
static CHK_GPU_PX: AtomicU32 = AtomicU32::new(0);
static OPAQ_N: AtomicU32 = AtomicU32::new(0);
/// The last GPU-destination write's formats: source D3DDDIFORMAT (low 16) | destination's << 16.
static FMT_K: AtomicU32 = AtomicU32::new(0);
static PITCH_MIS: AtomicU32 = AtomicU32::new(0);
static PITCH_CMD: AtomicU32 = AtomicU32::new(0);
static PITCH_AL: AtomicU32 = AtomicU32::new(0);
/// The destinations commands were executed into (any engine): up to 8 distinct resource ids, each
/// `resource id << 12 | executed commands (saturating at 4095)` (`GdiRes0`..`GdiRes7`), first come.
static RES_SLOTS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

fn note_dst_written(op: &Op) {
    let Some(d) = op.dst else { return };
    let id = d.resource_id & 0xf_ffff;
    if id == 0 {
        return;
    }
    for s in RES_SLOTS.iter() {
        let v = s.load(Ordering::Relaxed);
        if v == 0 {
            if s.compare_exchange(0, id << 12 | 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                return;
            }
            continue;
        }
        if v >> 12 == id {
            if v & 0xfff != 0xfff {
                s.store(v + 1, Ordering::Relaxed);
            }
            return;
        }
    }
}
/// Destinations SEEN by the parser, whatever became of the command (up to 16, first come), and
/// how many distinct ones there were: resource id (low 16) << 16 | class bit << 12 | GDI surface
/// type << 8 | commands (max 255). Unlike `GdiRes*` (written), a destination whose commands were
/// dropped or failed is listed too.
static SEEN_SLOTS: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
static SEEN_N: AtomicU32 = AtomicU32::new(0);

pub(crate) fn note_dst_seen(d: &Surface) {
    let id = d.resource_id & 0xffff;
    if id == 0 {
        return;
    }
    let head = id << 16 | class_bit(d.class) << 12 | (d.kind_bits & 0xf) << 8;
    for s in SEEN_SLOTS.iter() {
        let v = s.load(Ordering::Relaxed);
        if v == 0 {
            if s.compare_exchange(0, head | 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                SEEN_N.fetch_add(1, Ordering::Relaxed);
                return;
            }
            continue;
        }
        if v >> 16 == id {
            if v & 0xff != 0xff {
                s.store(v + 1, Ordering::Relaxed);
            }
            return;
        }
    }
    // All 16 slots taken by others: count it as distinct-unlisted once per command.
    SEEN_N.fetch_add(0x1_0000, Ordering::Relaxed);
}

static OVL_N: AtomicU32 = AtomicU32::new(0);
static OVL_CE: AtomicU32 = AtomicU32::new(0);
static OVL_WHY: AtomicU32 = AtomicU32::new(0);

fn sys_refused(why: u32, bit: u32) {
    SYS_WHY.store(why, Ordering::Relaxed);
    SYS_MSK.fetch_or(bit, Ordering::Relaxed);
}

/// The last copy or fill planned for a staging copy-engine path that ran on the CPU instead
/// (`GdiSysCpuK`): opcode | src class bit << 4 | dst class bit << 8 | same buffer << 12 |
/// `GdiPaths` << 16 | (where it left: 1 not tried, the path off; 2 run_ce_sys refused; 3 failed
/// after the mapping) << 24.
static SYS_CPU_K: AtomicU32 = AtomicU32::new(0);
/// The same command's surfaces (`Surface::kind_bits`: std type << 4 | GDI type | RM-backed << 8):
/// source in the low 16 bits, destination in the high 16 (`GdiSysCpuT`).
static SYS_CPU_T: AtomicU32 = AtomicU32::new(0);

fn note_sys_cpu(op: &Op, stage: u32) {
    let src = op.srcs[0];
    let same = matches!((src, op.dst), (Some(a), Some(b)) if a.resource_id == b.resource_id);
    SYS_CPU_K.store(
        op.cmd.opcode()
            | class_bit_of(src) << 4
            | class_bit_of(op.dst) << 8
            | u32::from(same) << 12
            | (PATHS.load(Ordering::Relaxed) & 0xff) << 16
            | stage << 24,
        Ordering::Relaxed,
    );
    SYS_CPU_T.store(
        src.map_or(0, |s| s.kind_bits & 0xffff) | op.dst.map_or(0, |d| d.kind_bits & 0xffff) << 16,
        Ordering::Relaxed,
    );
}

/// The timeline's completed watermark, for [`seq_ready`].
static COMPLETED: AtomicU64 = AtomicU64::new(0);
/// Nonzero while the table holds an admitted, unexecuted job (the worker's fast exit).
static PENDING: AtomicU32 = AtomicU32::new(0);

/// One resolved command.
pub(crate) struct Op {
    pub cmd: Cmd,
    pub dst: Option<Surface>,
    pub srcs: [Option<Surface>; 2],
    pub engine: Engine,
    pub why: Option<Why>,
    /// The destination sub-rectangles, clipped to the destination surface and to `DstRect`.
    pub subs: Vec<Rect>,
}

struct Job {
    id: u64,
    /// The `hContext` RenderKm/RenderGdi ran on (SubmitCommand's by-context claim).
    ctx: usize,
    /// The DMA buffer's GPU VA RenderGdi wrote this job's marker into (0 from RenderKm): how a
    /// record-less submission finds its own job (`gdi_accel::recordless_admit`).
    dma_va: u64,
    /// `None` until SubmitCommand admits it.
    seq: Option<u64>,
    /// Taken by the worker while it executes (the entry stays, so a replay still waits).
    ops: Vec<Op>,
    running: bool,
}

struct Table {
    jobs: Vec<Job>,
    next_id: u64,
    tl: Timeline,
}

static TABLE: SpinLock<Table> = SpinLock::new(Table {
    jobs: Vec::new(),
    next_id: 1,
    tl: Timeline { submitted: 0, completed: 0 },
});

/// StartDevice (PASSIVE): zero everything, forget the jobs of the previous generation (their
/// fences went with it).
/// The bisect switches (`GdiFgn`, `GdiFgnAcq`, `GdiSysCe`, `GdiPair`), bits 0..3 of `PATHS`.
static PATHS: AtomicU32 = AtomicU32::new(PATH_FGN | PATH_SYS | PATH_PAIR | PATH_OVL | PATH_FGN_WR);
const PATH_FGN: u32 = 1;
const PATH_FGN_ACQ: u32 = 2;
const PATH_SYS: u32 = 4;
const PATH_PAIR: u32 = 8;
const PATH_OVL: u32 = 16;
/// Copies INTO a foreign NVK image (`GdiOff` 0x20 turns them off).
const PATH_FGN_WR: u32 = 32;
/// Every GDI write into a GPU surface sets alpha 0xff (`GdiOff` 0x40 opts in; `ga::opaque_alpha`).
const PATH_OPAQUE: u32 = 64;
/// Execute each job synchronously inside RenderGdi/RenderKm, before its fence exists (`GdiOff`
/// 0x80 opts in; the timing test of the RGB-0 sources).
const PATH_SYNC: u32 = 128;
/// The readback diagnostics: the pixel self-check (`GdiChk*`, alpha census) and the content probes
/// (`GdiPrb*`, `GdiPre*`, `GdiSrcScan*`). Each costs bounce-buffer readbacks per sampled command,
/// so they are off unless `GdiOff` 0x100 opts in (they found 384.1-390.1's empty staging source).
const PATH_DIAG: u32 = 256;

static SYNC_N: AtomicU32 = AtomicU32::new(0);
static SYNC_OPS: AtomicU32 = AtomicU32::new(0);

/// The last command the CPU executor dropped (`run_cpu` failed), so a lost command names itself:
/// its signature (`GdiSlowOp` encoding), the failing step | the `Why` << 8 | the standard
/// buffer's failing step (`build_paging_buffer::last_std_fail`) << 16, the surfaces' kinds
/// (`kind_bits`, source low 16 | destination << 16), D3DDDIFORMATs, authored pitches and extents
/// (w << 16 | h), the command's pitches (source | destination << 16), and its first
/// sub-rectangle (left << 16 | top, w << 16 | h).
static DROP_K: AtomicU32 = AtomicU32::new(0);
static DROP_S: AtomicU32 = AtomicU32::new(0);
static DROP_T: AtomicU32 = AtomicU32::new(0);
static DROP_F: AtomicU32 = AtomicU32::new(0);
static DROP_P: AtomicU32 = AtomicU32::new(0);
static DROP_C: AtomicU32 = AtomicU32::new(0);
static DROP_SWH: AtomicU32 = AtomicU32::new(0);
static DROP_DWH: AtomicU32 = AtomicU32::new(0);
static DROP_O: AtomicU32 = AtomicU32::new(0);
static DROP_R: AtomicU32 = AtomicU32::new(0);
/// Commands into a GDI lookup table (`LOOKUPTABLE`, the ClearType gamma table CDD fills once
/// with a BitBlt): seen | executed without a drop << 16, and the last one's surface kinds
/// (source | destination << 16), formats, command pitches (source | destination << 16), first
/// sub-rectangle (w << 16 | h) and source extent (w << 16 | h).
static LUT_N: AtomicU32 = AtomicU32::new(0);
static LUT_T: AtomicU32 = AtomicU32::new(0);
static LUT_F: AtomicU32 = AtomicU32::new(0);
static LUT_C: AtomicU32 = AtomicU32::new(0);
static LUT_R: AtomicU32 = AtomicU32::new(0);
static LUT_SWH: AtomicU32 = AtomicU32::new(0);
/// The aperture history (`aperture_pages::history`) of the last gamma-table command's source
/// (low 8 bits) and destination (<< 8), and their resource ids (source | destination << 16).
static LUT_AP: AtomicU32 = AtomicU32::new(0);
static LUT_ID: AtomicU32 = AtomicU32::new(0);
/// Command pitches the CPU executor ignored (the surface is not `STAGING_CPUVISIBLE` /
/// `EXISTINGSYSMEM`, Learn `DXGK_GDIARG_BITBLT` remarks), and the last one ignored.
static PITCH_IGN: AtomicU32 = AtomicU32::new(0);
static PITCH_IGN_V: AtomicU32 = AtomicU32::new(0);

fn note_lut(op: &Op) {
    let lo16 = |v: u32| v & 0xffff;
    let (src, dst) = (op.srcs[0], op.dst);
    let (dpc, spc) = cmd_pitches(&op.cmd);
    LUT_N.fetch_add(1, Ordering::Relaxed);
    LUT_T.store(src.map_or(0, |s| lo16(s.kind_bits)) | dst.map_or(0, |d| lo16(d.kind_bits)) << 16, Ordering::Relaxed);
    LUT_F.store(src.map_or(0, |s| lo16(s.format)) | dst.map_or(0, |d| lo16(d.format)) << 16, Ordering::Relaxed);
    LUT_C.store(lo16(spc) | lo16(dpc) << 16, Ordering::Relaxed);
    let r = op.subs.first().copied().unwrap_or_default();
    LUT_R.store(lo16(r.width()) << 16 | lo16(r.height()), Ordering::Relaxed);
    LUT_SWH.store(src.map_or(0, |s| lo16(s.width) << 16 | lo16(s.height)), Ordering::Relaxed);
    let hist = |x: Option<Surface>| x.map_or(0, |x| crate::ddi::aperture_pages::history(x.resource_id) & 0xff);
    LUT_AP.store(hist(src) | hist(dst) << 8, Ordering::Relaxed);
    LUT_ID.store(src.map_or(0, |s| lo16(s.resource_id)) | dst.map_or(0, |d| lo16(d.resource_id)) << 16, Ordering::Relaxed);
}

/// The CPU executor's failing step (`GdiDropS` low byte): 1 source window size, 2 source rows
/// past its pitch, 3 source span, 4 source read, 5 destination write, 6 window size, 7 memory.
static CPU_STEP: AtomicU32 = AtomicU32::new(0);

fn cpu_step(n: u32) {
    CPU_STEP.store(n, Ordering::Relaxed);
}

fn note_drop(op: &Op, why: Why) {
    let lo16 = |v: u32| v & 0xffff;
    let wh = |s: Option<Surface>| s.map_or(0, |s| lo16(s.width) << 16 | lo16(s.height));
    let (src, dst) = (op.srcs[0], op.dst);
    let (dpc, spc) = cmd_pitches(&op.cmd);
    DROP_K.store(op_signature(op), Ordering::Relaxed);
    DROP_S.store(
        (CPU_STEP.load(Ordering::Relaxed) & 0xff)
            | why.code() << 8
            | (crate::ddi::build_paging_buffer::last_std_fail() & 0xff) << 16,
        Ordering::Relaxed,
    );
    DROP_T.store(src.map_or(0, |s| lo16(s.kind_bits)) | dst.map_or(0, |d| lo16(d.kind_bits)) << 16, Ordering::Relaxed);
    DROP_F.store(src.map_or(0, |s| lo16(s.format)) | dst.map_or(0, |d| lo16(d.format)) << 16, Ordering::Relaxed);
    DROP_P.store(src.map_or(0, |s| lo16(s.pitch)) | dst.map_or(0, |d| lo16(d.pitch)) << 16, Ordering::Relaxed);
    DROP_C.store(lo16(spc) | lo16(dpc) << 16, Ordering::Relaxed);
    DROP_SWH.store(wh(src), Ordering::Relaxed);
    DROP_DWH.store(wh(dst), Ordering::Relaxed);
    let r = op.subs.first().copied().unwrap_or_default();
    DROP_O.store(lo16(r.left as u32) << 16 | lo16(r.top as u32), Ordering::Relaxed);
    DROP_R.store(lo16(r.width()) << 16 | lo16(r.height()), Ordering::Relaxed);
}

/// Whether `translate` runs the commands itself (`GdiOff` 0x80).
pub(crate) fn sync_mode() -> bool {
    path(PATH_SYNC)
}

/// The commands of one buffer, executed now on the rendering thread (`GdiOff` 0x80): the staging
/// sources are read as win32k left them when it built the buffer. The caller then commits no job,
/// so SubmitCommand gates nothing. PASSIVE.
pub(crate) fn run_now(passive: PassiveLevel, adapter: &AdapterContext, ops: &[Op]) {
    let mut channel: Option<bool> = None;
    for op in ops {
        if channel.is_none() && needs_channel(op) {
            channel = Some(ensure_channel(passive, adapter));
        }
        execute(passive, adapter, op);
    }
    SYNC_N.fetch_add(1, Ordering::Relaxed);
    SYNC_OPS.fetch_add(ops.len() as u32, Ordering::Relaxed);
    DIRTY.store(1, Ordering::Relaxed);
    publish_if_due(false);
}

/// Whether `op` writes an opaque alpha byte (`ga::opaque_alpha`), counted in `GdiOpaqN`.
fn opaque(op: &Op) -> bool {
    let Some(d) = op.dst else { return false };
    ga::opaque_alpha(&op.cmd, op.srcs[0].as_ref(), &d, path(PATH_OPAQUE))
}

/// Sets byte 3 of every pixel of `subs` (surface coordinates) inside a packed window `win`.
fn force_alpha(buf: &mut [u8], win: &Rect, subs: &[Rect]) {
    let w = win.width().max(0) as usize;
    for sub in subs {
        let r = sub.intersect(win);
        if r.is_empty() {
            continue;
        }
        for y in r.top..r.bottom {
            let row = (y - win.top) as usize * w * 4;
            for x in r.left..r.right {
                if let Some(a) = buf.get_mut(row + (x - win.left) as usize * 4 + 3) {
                    *a = 0xff;
                }
            }
        }
    }
}

fn path(bit: u32) -> bool {
    PATHS.load(Ordering::Relaxed) & bit != 0
}

pub(crate) fn reset_for_start(on: bool) {
    if on {
        let off = crate::diag::read_config_dword(crate::diag::knobs::GDI_OFF, 0);
        PATHS.store(ga::paths_from_off(off), Ordering::Relaxed);
    }
    for c in [
        &BLT_N, &FILL_N, &FALL, &JOB_N, &AGAIN, &DONE, &ORPH, &CE_SUB, &US, &US_MAX, &RECTS, &CLS,
        &DST_RES, &DST_WH, &CH_UP, &CE_WHY, &RD_BACK, &SYS_CE, &SYS_REF, &SYS_FAIL, &SYS_WHY, &SYS_MSK, &SYS_CPU_K, &SYS_CPU_T, &CH_UP_US, &SLOW_US, &SLOW_OP, &SLOW_ROP, &CPU_MSK, &CPU_ROP, &SYS_US, &SYS_VW_US, &SYS_SUB_US, &SYS_WT_US, &SYS_PX, &FGN_RMW_RD, &FGN_RMW_WR, &FGN_RMW_FAIL, &SYS_K, &SYS_SWH, &SYS_DWH, &SYS_RES, &SYS_SHAPE, &FGN_CE, &FGN_FAIL, &FGN_WHY, &FGN_WR, &OVL_N, &OVL_CE, &OVL_WHY, &JOB_N_OPS, &CHK_SEEN, &CHK_N, &CHK_BAD, &CHK_K, &CHK_GOT, &CHK_WANT, &CHK_A0, &CHK_AFF, &CHK_GPU_PX, &OPAQ_N, &FMT_K, &PRB_K, &PRB_S_K, &SYNC_N, &SYNC_OPS, &SRC_SCAN, &SRC_SCAN_K, &PITCH_MIS, &PITCH_CMD, &PITCH_AL, &DROP_K, &DROP_S, &DROP_T, &DROP_F, &DROP_P, &DROP_C, &DROP_SWH, &DROP_DWH, &DROP_O, &DROP_R, &CPU_STEP, &LUT_N, &LUT_T, &LUT_F, &LUT_C, &LUT_R, &LUT_SWH, &LUT_AP, &LUT_ID, &CLAIM_MULTI, &CLAIM_MAX, &REGATE, &FREE_WAIT, &FREE_TO, &FREE_UNS, &FREE_US, &PITCH_IGN, &PITCH_IGN_V,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for s in RES_SLOTS.iter().chain(SEEN_SLOTS.iter()).chain(PRB.iter()).chain(PRB_SEEN.iter()).chain(PRB_S.iter()).chain(PRB_S_SEEN.iter()).chain(PRE.iter()) {
        s.store(0, Ordering::Relaxed);
    }
    SEEN_N.store(0, Ordering::Relaxed);
    let old = {
        let mut t = TABLE.lock();
        t.tl.discharge_all();
        COMPLETED.store(t.tl.completed, Ordering::Release);
        PENDING.store(0, Ordering::Release);
        core::mem::take(&mut t.jobs)
    };
    drop(old);
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
    w(b"GdiChUp", CH_UP.load(Ordering::Relaxed));
    w(b"GdiCeWhy", CE_WHY.load(Ordering::Relaxed));
    w(b"GdiRdBk", RD_BACK.load(Ordering::Relaxed));
    w(b"GdiSysCe", SYS_CE.load(Ordering::Relaxed));
    w(b"GdiSysRef", SYS_REF.load(Ordering::Relaxed));
    w(b"GdiSysFail", SYS_FAIL.load(Ordering::Relaxed));
    w(b"GdiSysWhy", SYS_WHY.load(Ordering::Relaxed));
    w(b"GdiSysMsk", SYS_MSK.load(Ordering::Relaxed));
    w(b"GdiSysCpuK", SYS_CPU_K.load(Ordering::Relaxed));
    w(b"GdiSysCpuT", SYS_CPU_T.load(Ordering::Relaxed));
    w(b"GdiChUpUs", CH_UP_US.load(Ordering::Relaxed));
    w(b"GdiSlowUs", SLOW_US.load(Ordering::Relaxed));
    w(b"GdiSlowOp", SLOW_OP.load(Ordering::Relaxed));
    w(b"GdiSlowRop", SLOW_ROP.load(Ordering::Relaxed));
    w(b"GdiCpuMsk", CPU_MSK.load(Ordering::Relaxed));
    w(b"GdiCpuRop", CPU_ROP.load(Ordering::Relaxed));
    w(b"GdiSysUs", SYS_US.load(Ordering::Relaxed));
    w(b"GdiSysVwUs", SYS_VW_US.load(Ordering::Relaxed));
    w(b"GdiSysSubUs", SYS_SUB_US.load(Ordering::Relaxed));
    w(b"GdiSysWtUs", SYS_WT_US.load(Ordering::Relaxed));
    w(b"GdiSysPx", SYS_PX.load(Ordering::Relaxed));
    w(b"GdiSysK", SYS_K.load(Ordering::Relaxed));
    w(b"GdiFgnRd", FGN_RMW_RD.load(Ordering::Relaxed));
    w(b"GdiFgnWb", FGN_RMW_WR.load(Ordering::Relaxed));
    w(b"GdiFgnRwF", FGN_RMW_FAIL.load(Ordering::Relaxed));
    w(b"GdiSysSWH", SYS_SWH.load(Ordering::Relaxed));
    w(b"GdiSysDWH", SYS_DWH.load(Ordering::Relaxed));
    w(b"GdiSysRes", SYS_RES.load(Ordering::Relaxed));
    w(b"GdiSysShape", SYS_SHAPE.load(Ordering::Relaxed));
    w(b"GdiFgnCe", FGN_CE.load(Ordering::Relaxed));
    w(b"GdiFgnFail", FGN_FAIL.load(Ordering::Relaxed));
    w(b"GdiFgnWhy", FGN_WHY.load(Ordering::Relaxed));
    w(b"GdiFgnWr", FGN_WR.load(Ordering::Relaxed));
    w(b"GdiPaths", PATHS.load(Ordering::Relaxed));
    w(b"GdiOvlN", OVL_N.load(Ordering::Relaxed));
    w(b"GdiOvlCe", OVL_CE.load(Ordering::Relaxed));
    w(b"GdiOvlWhy", OVL_WHY.load(Ordering::Relaxed));
    w(b"GdiJobMaxN", JOB_N_OPS.load(Ordering::Relaxed));
    for (i, s) in RES_SLOTS.iter().enumerate() {
        let name: &[u8] = match i {
            0 => b"GdiRes0",
            1 => b"GdiRes1",
            2 => b"GdiRes2",
            3 => b"GdiRes3",
            4 => b"GdiRes4",
            5 => b"GdiRes5",
            6 => b"GdiRes6",
            _ => b"GdiRes7",
        };
        w(name, s.load(Ordering::Relaxed));
    }
    const SEEN_NAMES: [&[u8]; 16] = [
        b"GdiSeen0", b"GdiSeen1", b"GdiSeen2", b"GdiSeen3", b"GdiSeen4", b"GdiSeen5", b"GdiSeen6",
        b"GdiSeen7", b"GdiSeen8", b"GdiSeen9", b"GdiSeen10", b"GdiSeen11", b"GdiSeen12", b"GdiSeen13",
        b"GdiSeen14", b"GdiSeen15",
    ];
    for (name, s) in SEEN_NAMES.iter().zip(SEEN_SLOTS.iter()) {
        w(name, s.load(Ordering::Relaxed));
    }
    w(b"GdiSeenN", SEEN_N.load(Ordering::Relaxed));
    w(b"GdiChkN", CHK_N.load(Ordering::Relaxed));
    w(b"GdiChkBad", CHK_BAD.load(Ordering::Relaxed));
    w(b"GdiChkK", CHK_K.load(Ordering::Relaxed));
    w(b"GdiChkGot", CHK_GOT.load(Ordering::Relaxed));
    w(b"GdiChkA0", CHK_A0.load(Ordering::Relaxed));
    w(b"GdiChkAFF", CHK_AFF.load(Ordering::Relaxed));
    w(b"GdiChkGpuPx", CHK_GPU_PX.load(Ordering::Relaxed));
    w(b"GdiOpaqN", OPAQ_N.load(Ordering::Relaxed));
    w(b"GdiFmtK", FMT_K.load(Ordering::Relaxed));
    const PRB_NAMES: [&[u8]; 7] = [b"GdiPrb1", b"GdiPrb2", b"GdiPrb3", b"GdiPrb4", b"GdiPrb5", b"GdiPrb6", b"GdiPrb7"];
    for (name, s) in PRB_NAMES.iter().zip(PRB.iter()) {
        w(name, s.load(Ordering::Relaxed));
    }
    w(b"GdiPrbK", PRB_K.load(Ordering::Relaxed));
    const PRB_S_NAMES: [&[u8]; 7] = [b"GdiPrbS1", b"GdiPrbS2", b"GdiPrbS3", b"GdiPrbS4", b"GdiPrbS5", b"GdiPrbS6", b"GdiPrbS7"];
    for (name, s) in PRB_S_NAMES.iter().zip(PRB_S.iter()) {
        w(name, s.load(Ordering::Relaxed));
    }
    w(b"GdiPrbSK", PRB_S_K.load(Ordering::Relaxed));
    w(b"GdiSyncN", SYNC_N.load(Ordering::Relaxed));
    w(b"GdiSyncOps", SYNC_OPS.load(Ordering::Relaxed));
    w(b"GdiSrcScan", SRC_SCAN.load(Ordering::Relaxed));
    w(b"GdiSrcScanK", SRC_SCAN_K.load(Ordering::Relaxed));
    const PRE_NAMES: [&[u8]; 4] = [b"GdiPre0", b"GdiPre1", b"GdiPre2", b"GdiPre3"];
    for (name, s) in PRE_NAMES.iter().zip(PRE.iter()) {
        w(name, s.load(Ordering::Relaxed));
    }
    w(b"GdiChkWant", CHK_WANT.load(Ordering::Relaxed));
    w(b"GdiPitchMis", PITCH_MIS.load(Ordering::Relaxed));
    w(b"GdiPitchCmd", PITCH_CMD.load(Ordering::Relaxed));
    w(b"GdiPitchAl", PITCH_AL.load(Ordering::Relaxed));
    w(b"GdiJobT1", JOB_T[0].load(Ordering::Relaxed));
    w(b"GdiJobT1Us", JOB_T[1].load(Ordering::Relaxed));
    w(b"GdiJobT2", JOB_T[2].load(Ordering::Relaxed));
    w(b"GdiJobT2Us", JOB_T[3].load(Ordering::Relaxed));
    w(b"GdiJobT3", JOB_T[4].load(Ordering::Relaxed));
    w(b"GdiJobT3Us", JOB_T[5].load(Ordering::Relaxed));
    w(b"GdiThr", u32::from(crate::ddi::gdi_thread::running()));
    w(b"GdiDropK", DROP_K.load(Ordering::Relaxed));
    w(b"GdiDropS", DROP_S.load(Ordering::Relaxed));
    w(b"GdiDropT", DROP_T.load(Ordering::Relaxed));
    w(b"GdiDropF", DROP_F.load(Ordering::Relaxed));
    w(b"GdiDropP", DROP_P.load(Ordering::Relaxed));
    w(b"GdiDropC", DROP_C.load(Ordering::Relaxed));
    w(b"GdiDropSWH", DROP_SWH.load(Ordering::Relaxed));
    w(b"GdiDropDWH", DROP_DWH.load(Ordering::Relaxed));
    w(b"GdiDropO", DROP_O.load(Ordering::Relaxed));
    w(b"GdiDropR", DROP_R.load(Ordering::Relaxed));
    w(b"GdiLutN", LUT_N.load(Ordering::Relaxed));
    w(b"GdiLutT", LUT_T.load(Ordering::Relaxed));
    w(b"GdiLutF", LUT_F.load(Ordering::Relaxed));
    w(b"GdiLutC", LUT_C.load(Ordering::Relaxed));
    w(b"GdiLutR", LUT_R.load(Ordering::Relaxed));
    w(b"GdiLutSWH", LUT_SWH.load(Ordering::Relaxed));
    w(b"GdiLutAp", LUT_AP.load(Ordering::Relaxed));
    w(b"GdiLutId", LUT_ID.load(Ordering::Relaxed));
    w(b"GdiClmMul", CLAIM_MULTI.load(Ordering::Relaxed));
    w(b"GdiClmMax", CLAIM_MAX.load(Ordering::Relaxed));
    w(b"GdiReGate", REGATE.load(Ordering::Relaxed));
    w(b"GdiVaHit", VA_HIT.load(Ordering::Relaxed));
    w(b"GdiVaNone", VA_NONE.load(Ordering::Relaxed));
    w(b"GdiVaOrph", VA_ORPH.load(Ordering::Relaxed));
    w(b"GdiFreeWait", FREE_WAIT.load(Ordering::Relaxed));
    w(b"GdiFreeTo", FREE_TO.load(Ordering::Relaxed));
    w(b"GdiFreeUns", FREE_UNS.load(Ordering::Relaxed));
    w(b"GdiFreeUs", FREE_US.load(Ordering::Relaxed));
    w(b"GdiPitchIgn", PITCH_IGN.load(Ordering::Relaxed));
    w(b"GdiPitchIgnV", PITCH_IGN_V.load(Ordering::Relaxed));
}

/// The channel up for a job that needs it (a VRAM surface on either side): bring it up when it is
/// cold. HPD worker only.
fn ensure_channel(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    match glue::channel_state() {
        0 => true,
        1 => {
            CH_UP.fetch_add(1, Ordering::Relaxed);
            let t0 = now_100ns();
            let up = glue::bring_up(passive, adapter);
            CH_UP_US.fetch_add((now_100ns().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
            up || {
                CE_WHY.store(16 + glue::channel_state(), Ordering::Relaxed);
                false
            }
        }
        s => {
            CE_WHY.store(16 + s, Ordering::Relaxed);
            false
        }
    }
}

/// The executor thread's first act: the channel up before any job asks (`RedirVram` on and the
/// channel cold). PASSIVE.
pub(crate) fn warm_up(passive: PassiveLevel, adapter: &AdapterContext) {
    if crate::ddi::gdi_accel::on() && glue::vram_knob_on() && glue::channel_state() == 1 {
        let _ = ensure_channel(passive, adapter);
    }
}

fn needs_channel(op: &Op) -> bool {
    op.engine == Engine::Ce
        || op.why == Some(Why::SystemSurface)
        || op.why == Some(Why::Overlap)
        || [op.dst, op.srcs[0], op.srcs[1]]
            .iter()
            .flatten()
            .any(|s| matches!(s.class, SurfaceClass::Vram | SurfaceClass::Foreign))
}

fn class_bit(c: SurfaceClass) -> u32 {
    match c {
        SurfaceClass::Vram => 1,
        SurfaceClass::System => 2,
        SurfaceClass::Unreachable => 4,
        SurfaceClass::Foreign => 8,
    }
}

/// One parsed command's census (RenderKm, PASSIVE).
pub(crate) fn note_census(cmd: &Cmd, dst: Option<&Surface>, srcs: [Option<&Surface>; 2]) {
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

/// Is sequence `seq` executed (the WDDM fence gate). Lock-free, any IRQL.
#[inline]
pub(crate) fn seq_ready(seq: u64) -> bool {
    COMPLETED.load(Ordering::Acquire) >= seq
}

/// Insert a job (RenderKm, PASSIVE). Returns its id, or 0 when it could not be stored (the
/// buffer then carries no job and its fence does not wait: its commands are dropped, counted).
pub(crate) fn commit(ops: Vec<Op>, ctx: usize, dma_va: u64) -> u64 {
    let mut job = Job { id: 0, ctx, dma_va, seq: None, ops, running: false };
    let mut orphans: Vec<Job> = Vec::new();
    let id = {
        let mut t = TABLE.lock();
        if t.jobs.try_reserve(1).is_err() {
            return 0;
        }
        let id = t.next_id;
        t.next_id += 1;
        job.id = id;
        t.jobs.push(job);
        // Orphans: unclaimed jobs above the bound, oldest first.
        let unclaimed = t.jobs.iter().filter(|j| j.seq.is_none()).count();
        if unclaimed > ga::MAX_UNCLAIMED {
            if let Some(pos) = t.jobs.iter().position(|j| j.seq.is_none()) {
                let j = t.jobs.remove(pos);
                if orphans.try_reserve(1).is_ok() {
                    orphans.push(j);
                } else {
                    // Dropped under the lock: plain pool memory, no PASSIVE-only destructor.
                    drop(j);
                }
            }
        }
        id
    };
    if !orphans.is_empty() {
        ORPH.fetch_add(orphans.len() as u32, Ordering::Relaxed);
    }
    drop(orphans);
    id
}

/// SubmitCommand (DISPATCH): admit job `id`. `Some(seq)`: gate the fence on [`seq_ready`]`(seq)`.
pub(crate) fn admit(adapter: &AdapterContext, id: u64) -> Option<u64> {
    let r = {
        let mut t = TABLE.lock();
        let completed = t.tl.completed;
        let found = t.jobs.iter().position(|j| j.id == id);
        match found {
            None => ga::Admit::NoWait,
            Some(i) => match t.jobs[i].seq {
                Some(seq) if seq <= completed => ga::Admit::NoWait,
                Some(seq) => ga::Admit::Again { seq },
                None => {
                    let seq = t.tl.next();
                    t.jobs[i].seq = Some(seq);
                    ga::Admit::Queue { seq }
                }
            },
        }
    };
    match r {
        ga::Admit::Queue { seq } => {
            JOB_N.fetch_add(1, Ordering::Relaxed);
            PENDING.store(1, Ordering::Release);
            kick(adapter);
            Some(seq)
        }
        ga::Admit::Again { seq } => {
            AGAIN.fetch_add(1, Ordering::Relaxed);
            Some(seq)
        }
        ga::Admit::NoWait => None,
    }
}

/// Submissions on a GDI context without a private record that admitted more than one job, and
/// the most one admitted (`GdiClmMul`, `GdiClmMax`).
static CLAIM_MULTI: AtomicU32 = AtomicU32::new(0);
static CLAIM_MAX: AtomicU32 = AtomicU32::new(0);
/// Submissions on a GDI context without a private record that admitted no job and were gated on
/// the context's outstanding jobs (`GdiReGate`): resubmissions after a preemption, or a buffer
/// whose job an earlier submission admitted.
static REGATE: AtomicU32 = AtomicU32::new(0);
/// Record-less submissions that found their buffer's job by its DMA VA (`GdiVaHit`), ones that
/// found none once the pairing held (`GdiVaNone`: replays, buffers without a job), and earlier
/// renders of a submitted buffer dropped as never submitted (`GdiVaOrph`).
static VA_HIT: AtomicU32 = AtomicU32::new(0);
static VA_NONE: AtomicU32 = AtomicU32::new(0);
static VA_ORPH: AtomicU32 = AtomicU32::new(0);
/// Some record-less submission on this boot found its job by the DMA VA RenderGdi recorded, i.e.
/// RenderGdi's `DmaBufferGpuVirtualAddress` and SubmitCommandVirtual's `DmaBufferVirtualAddress`
/// agree here. Until then the record-less rule stays "every unclaimed job of the context".
static VA_PAIRED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// SubmitCommand without a private record, on GDI context `ctx` (DISPATCH), for the DMA buffer at
/// GPU VA `va` of `size` bytes (0: not known): admit the job RenderGdi rendered into that buffer
/// (`gdi_accel::recordless_admit`), and gate this submission's fence on the newest outstanding job
/// of the context (`gdi_accel::context_gate`): the one admitted here, or, when the buffer's job
/// was admitted already, the jobs earlier submissions admitted that the executor has not finished.
///
/// The second case is a preempted buffer coming back: `DxgkDdiPreemptCommand` drops the pending
/// fence and dxgkrnl resubmits the same DMA buffer under a new fence id. Its job was admitted by
/// the first submission; gated on nothing, the replay's fence retired before the executor ran the
/// job (CDD read stale readback pixels and reused staging buffers the job still read: half-drawn
/// rows). The WDDM fence FIFO is head-of-line and the context in-order, so waiting for every
/// outstanding job of the context covers the replay's own.
///
/// Rendering runs ahead of submission, so the table can hold jobs of buffers dxgkrnl has not
/// submitted yet. They stay unclaimed until their own submission: admitted here they ran before
/// dxgkrnl had done the waits it does ahead of their buffer, and this buffer's fence waited for
/// work that was not its own. Claiming only the oldest unclaimed job went wrong the other way
/// whenever a rendered buffer was never submitted (each later fence was gated on the job before
/// its own; CDD destroyed a copy's source while the job still had to read it, `GdiLutAp` 0x0D).
/// Until a submission has matched a job by its VA on this boot (`VA_PAIRED`), and for a
/// submission without a VA, every unclaimed job of the context is admitted as before.
///
/// Returns the gate (`None`: nothing of the context outstanding) and whether a job was admitted.
pub(crate) fn admit_unclaimed(adapter: &AdapterContext, ctx: usize, va: u64, size: u64) -> (Option<u64>, bool) {
    let mut orphans = 0u32;
    let (gate, n, exact) = {
        let mut t = TABLE.lock();
        let mut n = 0u32;
        let pick = ga::recordless_admit(
            t.jobs.iter().map(|j| ga::JobView { id: j.id, ctx: j.ctx, claimed: j.seq.is_some(), dma_va: j.dma_va }),
            ctx,
            va,
            size,
            VA_PAIRED.load(Ordering::Relaxed),
        );
        match pick {
            ga::Recordless::Exact { id } => {
                // Earlier renders of this buffer were never submitted: out of the table, so a
                // later replay of the buffer cannot pick one up. Dropped under the lock (plain
                // nonpaged pool memory, no PASSIVE-only destructor), like `commit`'s orphans.
                let before = t.jobs.len();
                t.jobs.retain(|j| {
                    let view = ga::JobView { id: j.id, ctx: j.ctx, claimed: j.seq.is_some(), dma_va: j.dma_va };
                    !ga::orphaned_by(&view, id, ctx, va, size)
                });
                orphans = (before - t.jobs.len()) as u32;
                if let Some(i) = t.jobs.iter().position(|j| j.id == id) {
                    let seq = t.tl.next();
                    t.jobs[i].seq = Some(seq);
                    n = 1;
                }
            }
            ga::Recordless::Nothing => {}
            ga::Recordless::AllUnclaimed => loop {
                let Some(i) = t
                    .jobs
                    .iter()
                    .enumerate()
                    .filter(|(_, j)| j.ctx == ctx && j.seq.is_none())
                    .min_by_key(|(_, j)| j.id)
                    .map(|(i, _)| i)
                else {
                    break;
                };
                let seq = t.tl.next();
                t.jobs[i].seq = Some(seq);
                n += 1;
            },
        }
        // A job the executor is running stays in the table with its sequence until it completes.
        let completed = t.tl.completed;
        let gate = ga::context_gate(t.jobs.iter().filter(|j| j.ctx == ctx).map(|j| j.seq), completed);
        (gate, n, pick)
    };
    match exact {
        ga::Recordless::Exact { .. } => {
            VA_PAIRED.store(true, Ordering::Relaxed);
            VA_HIT.fetch_add(1, Ordering::Relaxed);
        }
        ga::Recordless::Nothing => {
            VA_NONE.fetch_add(1, Ordering::Relaxed);
        }
        ga::Recordless::AllUnclaimed => {}
    }
    if orphans != 0 {
        VA_ORPH.fetch_add(orphans, Ordering::Relaxed);
        ORPH.fetch_add(orphans, Ordering::Relaxed);
    }
    if n == 0 {
        if gate.is_some() {
            REGATE.fetch_add(1, Ordering::Relaxed);
        }
        return (gate, false);
    }
    JOB_N.fetch_add(n, Ordering::Relaxed);
    if n > 1 {
        CLAIM_MULTI.fetch_add(1, Ordering::Relaxed);
    }
    CLAIM_MAX.fetch_max(n, Ordering::Relaxed);
    PENDING.store(1, Ordering::Release);
    kick(adapter);
    (gate, true)
}

/// Allocation destroys that waited for queued GDI jobs naming the allocation (`GdiFreeWait`), the
/// waits that ran out (`GdiFreeTo`), destroys of an allocation named by a job not yet submitted
/// (`GdiFreeUns`), and the longest wait in µs (`GdiFreeUs`).
static FREE_WAIT: AtomicU32 = AtomicU32::new(0);
static FREE_TO: AtomicU32 = AtomicU32::new(0);
static FREE_UNS: AtomicU32 = AtomicU32::new(0);
static FREE_US: AtomicU32 = AtomicU32::new(0);
/// How long a destroy waits for the executor at most (wall clock).
const FREE_WAIT_MS: u64 = 500;

fn names(op: &Op, resource_id: u32) -> bool {
    [op.dst, op.srcs[0], op.srcs[1]].iter().flatten().any(|s| s.resource_id == resource_id)
}

/// DestroyAllocation of `resource_id` (PASSIVE): first let the executor finish every admitted job
/// that names it (and the job it is running, whose commands are out of the table).
///
/// dxgkrnl destroys an allocation once it holds no reference to it; it does not wait for the
/// fence of a GDI DMA buffer that named it. CDD frees the staging buffer of the ClearType gamma
/// table right after submitting the table's initialising BitBlt, before the executor ran it: the
/// copy then read a freed allocation and the table stayed zero (405.31: `GdiLutAp` 0x0D, the
/// source created, registered and forgotten before the job ran). The executor only reads and
/// writes the allocation through its content views (the aperture pages, the blob), which go with
/// the destroy, so the destroy waits, as one on hardware waits for the GPU to be done with it.
pub(crate) fn drain_for(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    if resource_id == 0 || PENDING.load(Ordering::Acquire) == 0 && !any_running() {
        return;
    }
    let target = {
        let t = TABLE.lock();
        let mut target: Option<u64> = None;
        let mut unsubmitted = false;
        for j in t.jobs.iter() {
            let named = j.running || j.ops.iter().any(|op| names(op, resource_id));
            if !named {
                continue;
            }
            match j.seq {
                Some(seq) if seq > t.tl.completed => target = Some(target.map_or(seq, |x| x.max(seq))),
                Some(_) => {}
                None => unsubmitted |= !j.running,
            }
        }
        if unsubmitted {
            FREE_UNS.fetch_add(1, Ordering::Relaxed);
        }
        target
    };
    let Some(target) = target else { return };
    FREE_WAIT.fetch_add(1, Ordering::Relaxed);
    let t0 = now_100ns();
    // Wall clock: `sleep_ms(1)` rounds up to the timer tick (~15.6 ms), so counting sleeps would
    // stretch the cap up to ~16x.
    while !seq_ready(target) {
        if now_100ns().wrapping_sub(t0) >= FREE_WAIT_MS * 10_000 {
            FREE_TO.fetch_add(1, Ordering::Relaxed);
            break;
        }
        kick(adapter);
        crate::virtio::ctrl::sleep_ms(passive, 1);
    }
    FREE_US.fetch_max(us_since(t0), Ordering::Relaxed);
}

fn any_running() -> bool {
    TABLE.lock().jobs.iter().any(|j| j.running)
}

/// DestroyContext: its unclaimed jobs can never be submitted (dropped, counted as orphans).
pub(crate) fn forget_context(ctx: usize) {
    let gone: Vec<Job> = {
        let mut t = TABLE.lock();
        let mut keep = Vec::new();
        let mut gone = Vec::new();
        if keep.try_reserve(t.jobs.len()).is_err() || gone.try_reserve(t.jobs.len()).is_err() {
            return;
        }
        for j in core::mem::take(&mut t.jobs) {
            if j.ctx == ctx && j.seq.is_none() {
                gone.push(j);
            } else {
                keep.push(j);
            }
        }
        t.jobs = keep;
        gone
    };
    if !gone.is_empty() {
        ORPH.fetch_add(gone.len() as u32, Ordering::Relaxed);
    }
}

/// StopDevice: every admitted sequence is complete (their fences retire), the jobs are dropped.
pub(crate) fn discharge_all(adapter: &AdapterContext) {
    let old = {
        let mut t = TABLE.lock();
        t.tl.discharge_all();
        COMPLETED.store(t.tl.completed, Ordering::Release);
        PENDING.store(0, Ordering::Release);
        core::mem::take(&mut t.jobs)
    };
    drop(old);
    crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
}

fn now_100ns() -> u64 {
    crate::ddi::blt_async::now_100ns()
}

/// Jobs executed per worker pass before it yields (and re-signals itself), and the time after
/// which a pass yields early: the worker also serves flips and Present copies, so one GDI burst
/// must not hold it for long (one job is still atomic).
const JOBS_PER_PASS: usize = 32;
const PASS_BUDGET_100NS: u64 = 20_000;

/// Wake whoever runs the executor: its own thread, or the HPD worker without one. Any IRQL up to
/// DISPATCH.
fn kick(adapter: &AdapterContext) {
    if crate::ddi::gdi_thread::running() {
        crate::ddi::gdi_thread::kick();
    } else {
        adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
    }
}

/// Execute admitted jobs in sequence order (PASSIVE): from the executor's thread (`on_thread`), or
/// from the HPD worker while that thread does not run. With the knob off, nothing admitted, or
/// the other runner in charge: one or two relaxed loads. Returns whether work is left (the pass
/// yielded on its budget).
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext, on_thread: bool) -> bool {
    if !crate::ddi::gdi_accel::on()
        || PENDING.load(Ordering::Acquire) == 0
        || on_thread != crate::ddi::gdi_thread::running()
    {
        return false;
    }
    let mut more = false;
    let mut ran = 0usize;
    let pass_t0 = now_100ns();
    let mut channel: Option<bool> = None;
    loop {
        // Take the next sequence's commands out, leaving its entry (a replay still waits on it).
        let taken = {
            let mut t = TABLE.lock();
            let want = t.tl.completed + 1;
            if want > t.tl.submitted {
                PENDING.store(0, Ordering::Release);
                None
            } else {
                match t.jobs.iter_mut().find(|j| j.seq == Some(want)) {
                    Some(j) if !j.running => {
                        j.running = true;
                        Some((want, core::mem::take(&mut j.ops)))
                    }
                    Some(_) => None,
                    None => {
                        // Not in the table (StartDevice raced it away): nothing to run.
                        t.tl.complete(want);
                        COMPLETED.store(t.tl.completed, Ordering::Release);
                        Some((want, Vec::new()))
                    }
                }
            }
        };
        let Some((seq, ops)) = taken else {
            break;
        };
        // A cold channel comes up before the job's clock starts (`GdiChUpUs`, not `GdiUs`).
        if channel.is_none() && ops.iter().any(needs_channel) {
            channel = Some(ensure_channel(passive, adapter));
        }
        let t0 = now_100ns();
        // The three slowest commands of this job: (µs, signature).
        let mut top = [(0u32, 0u32); 3];
        for op in &ops {
            if crate::ddi::gdi_thread::stopping() {
                // StopDevice discharges the job; the rest of its commands are not run.
                break;
            }
            if channel.is_none() && needs_channel(op) {
                channel = Some(ensure_channel(passive, adapter));
            }
            let o0 = now_100ns();
            execute(passive, adapter, op);
            let ous = (now_100ns().wrapping_sub(o0) / 10).min(u64::from(u32::MAX)) as u32;
            if ous > SLOW_US.load(Ordering::Relaxed) {
                SLOW_US.store(ous, Ordering::Relaxed);
                SLOW_OP.store(op_signature(op), Ordering::Relaxed);
                SLOW_ROP.store(rop_key(op), Ordering::Relaxed);
            }
            if ous > top[2].0 {
                top[2] = (ous, op_signature(op));
                top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            }
        }
        let us = (now_100ns().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32;
        US.fetch_add(us, Ordering::Relaxed);
        if us > US_MAX.fetch_max(us, Ordering::Relaxed) {
            JOB_N_OPS.store(ops.len() as u32, Ordering::Relaxed);
            for (i, (t, sig)) in top.iter().enumerate() {
                JOB_T[2 * i].store(*sig, Ordering::Relaxed);
                JOB_T[2 * i + 1].store(*t, Ordering::Relaxed);
            }
        }
        let finished = {
            let mut t = TABLE.lock();
            t.tl.complete(seq);
            COMPLETED.store(t.tl.completed, Ordering::Release);
            t.jobs.iter().position(|j| j.seq == Some(seq)).map(|i| t.jobs.remove(i))
        };
        drop(finished);
        drop(ops);
        DONE.fetch_add(1, Ordering::Relaxed);
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
        ran += 1;
        if ran >= JOBS_PER_PASS || now_100ns().wrapping_sub(pass_t0) >= PASS_BUDGET_100NS {
            if on_thread {
                more = true;
            } else {
                adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
            }
            break;
        }
    }
    if ran > 0 {
        DIRTY.store(1, Ordering::Relaxed);
        publish_if_due(false);
    }
    more
}

/// Executed jobs whose counters are not mirrored yet.
static DIRTY: AtomicU32 = AtomicU32::new(0);
/// When the counters were last mirrored (100 ns).
static LAST_PUBLISH: AtomicU64 = AtomicU64::new(0);
/// The counters are mirrored at most this often while jobs run: one publish is ~100 registry
/// writes (~100 µs each), which after every worker pass was most of a short job's cost.
const PUBLISH_EVERY_100NS: u64 = 250 * 10_000;

/// Mirror the counters when some changed and the last publish is `PUBLISH_EVERY_100NS` old (or
/// `force`). The executor thread calls it after each pass and when it idles (`gdi_thread`), so the
/// last burst's counters land within one idle period. PASSIVE.
pub(crate) fn publish_if_due(force: bool) {
    if DIRTY.load(Ordering::Relaxed) == 0 {
        return;
    }
    let now = now_100ns();
    if !force && now.wrapping_sub(LAST_PUBLISH.load(Ordering::Relaxed)) < PUBLISH_EVERY_100NS {
        return;
    }
    DIRTY.store(0, Ordering::Relaxed);
    LAST_PUBLISH.store(now, Ordering::Relaxed);
    crate::ddi::gdi_accel::publish_counters();
}

/// One pixel of a surface (VRAM through the bounce buffer, a staging buffer through its CPU view).
fn read_px(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, x: i32, y: i32, cmd_pitch: u32) -> Option<u32> {
    if x < 0 || y < 0 || x as u32 >= s.width || y as u32 >= s.height {
        return None;
    }
    let mut b = [0u8; 4];
    let ok = match s.class {
        SurfaceClass::Vram => glue::vram_read(passive, adapter, s.resource_id, Rect::new(x, y, x + 1, y + 1), &mut b, 4),
        SurfaceClass::System => {
            let p = pitch_of(s, cmd_pitch) as u64;
            glue::std_read(passive, adapter, s.resource_id, y as u64 * p + x as u64 * 4, &mut b)
        }
        _ => false,
    };
    ok.then(|| u32::from_le_bytes(b))
}

/// The sampled pixel self-check of a command that reported success (`path`: 1 CE, 2 staging CE
/// view, 3 CPU, 4 scroll bands). PASSIVE, no lock held.
fn self_check(passive: PassiveLevel, adapter: &AdapterContext, op: &Op, path: u32) {
    if !self::path(PATH_DIAG) {
        return;
    }
    let (Some(dst), Some(sub)) = (op.dst, op.subs.first()) else {
        return;
    };
    let want = match op.cmd {
        Cmd::ColorFill { rop, color, .. } if rop == ga::cfrop::PATCOPY => Some(color),
        Cmd::BitBlt { rop, src: sr, dst: dr, .. } if rop == ga::rop::SRCCOPY && op.why != Some(Why::Overlap) => None
            .or_else(|| {
                let src = op.srcs[0]?;
                let s = ga::bitblt_src(sub, &dr, &sr);
                let (_, spc) = cmd_pitches(&op.cmd);
                let v = read_px(passive, adapter, &src, s.left, s.top, spc)?;
                Some(if ga::swaps_rb(&src, &dst) { (v & 0xff00_ff00) | (v >> 16 & 0xff) | (v & 0xff) << 16 } else { v })
            }),
        _ => return,
    };
    let n = CHK_SEEN.fetch_add(1, Ordering::Relaxed);
    if n >= 16 && n % 32 != 0 {
        return;
    }
    let Some(want) = want else {
        return;
    };
    let (dpc, _) = cmd_pitches(&op.cmd);
    let Some(got) = read_px(passive, adapter, &dst, sub.left, sub.top, dpc) else {
        return;
    };
    CHK_N.fetch_add(1, Ordering::Relaxed);
    if matches!(dst.class, SurfaceClass::Vram | SurfaceClass::Foreign) {
        match got >> 24 {
            0 => CHK_A0.fetch_add(1, Ordering::Relaxed),
            0xff => CHK_AFF.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        CHK_GPU_PX.store(got, Ordering::Relaxed);
    }
    if (got ^ want) & 0x00ff_ffff != 0 {
        CHK_BAD.fetch_add(1, Ordering::Relaxed);
        CHK_K.store(op_signature(op) | path << 28, Ordering::Relaxed);
        CHK_GOT.store(got, Ordering::Relaxed);
        CHK_WANT.store(want, Ordering::Relaxed);
    }
}

fn class_bit_of(s: Option<Surface>) -> u32 {
    s.map_or(0, |s| class_bit(s.class))
}

/// `GdiSlowRop` / `GdiCpuRop`: the plan's reason (`Why` code, 0 none) | the DXGK rop enum << 8 |
/// the ROP3 code << 16 (a BitBlt's or ColorFill's; 0 for the other commands).
fn rop_key(op: &Op) -> u32 {
    let (r, r3) = match op.cmd {
        Cmd::BitBlt { rop, rop3, .. } | Cmd::ColorFill { rop, rop3, .. } => (u32::from(rop), u32::from(rop3 & 0xff)),
        _ => (0, 0),
    };
    op.why.map_or(0, |w| w.code()) | (r & 0xff) << 8 | r3 << 16
}

/// `GdiSlowOp`: opcode | engine << 4 (0 CE, 1 CPU, 2 drop) | dst class << 8 | src class << 12 |
/// sub-rectangles (at most 255) << 16 | 1 << 24 when the op's destination rectangle is over 1 MPixel.
fn op_signature(op: &Op) -> u32 {
    let eng = match op.engine {
        Engine::Ce => 0,
        Engine::Cpu => 1,
        Engine::Drop => 2,
    };
    let big = op.subs.iter().map(|r| r.width() as u64 * r.height() as u64).sum::<u64>() > 1 << 20;
    op.cmd.opcode()
        | eng << 4
        | class_bit_of(op.dst) << 8
        | class_bit_of(op.srcs[0]) << 12
        | (op.subs.len().min(255) as u32) << 16
        | u32::from(big) << 24
}

/// A SRCCOPY BitBlt from a foreign NVK image into VRAM or a staging buffer, on the copy engine.
/// `false` (counted in `GdiFgnFail`, the step in `GdiFgnWhy`): the caller runs the CPU path.
fn run_foreign(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> bool {
    let (Some(dst), Some(src), Cmd::BitBlt { src: sr, dst: dr, .. }) = (op.dst, op.srcs[0], op.cmd) else {
        return false;
    };
    let fail = |why: u32| {
        FGN_FAIL.fetch_add(1, Ordering::Relaxed);
        FGN_WHY.store(why, Ordering::Relaxed);
        false
    };
    if !path(PATH_FGN) {
        return fail(9);
    }
    if glue::channel_state() != 0 {
        return fail(6);
    }
    let Some(fs) = glue::foreign_source(passive, adapter, src.resource_id, path(PATH_FGN_ACQ)) else {
        return fail(1);
    };
    let mut pairs: Vec<(Rect, Rect)> = Vec::new();
    if pairs.try_reserve_exact(op.subs.len()).is_err() {
        return fail(7);
    }
    for r in &op.subs {
        let s = clip_to(&ga::bitblt_src(r, &dr, &sr), &src);
        let d = ga::bitblt_src(&s, &sr, &dr);
        if !s.is_empty() && s.width() == d.width() && s.height() == d.height() {
            pairs.push((s, d));
        }
    }
    let fourcc = glue::fourcc_of(dst.format);
    let step = match dst.class {
        SurfaceClass::Vram => glue::foreign_to_vram(passive, adapter, &fs, dst.resource_id, &pairs, fourcc),
        SurfaceClass::System => {
            let (dpc, _) = cmd_pitches(&op.cmd);
            glue::foreign_to_standard(
                passive, adapter, &fs, dst.resource_id, map_pitch(&dst, dpc), dst.width, dst.height, &pairs, fourcc,
            )
        }
        _ => 8,
    };
    if step != 0 {
        return fail(step);
    }
    FGN_CE.fetch_add(1, Ordering::Relaxed);
    BLT_N.fetch_add(1, Ordering::Relaxed);
    true
}

/// A SRCCOPY BitBlt INTO a foreign NVK image from VRAM or a staging buffer, on the copy engine
/// (`ce_vram::foreign_write`). `false`: the caller runs the CPU path.
fn run_foreign_write(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> bool {
    let (Some(dst), Some(src), Cmd::BitBlt { src: sr, dst: dr, .. }) = (op.dst, op.srcs[0], op.cmd) else {
        return false;
    };
    let fail = |why: u32| {
        FGN_FAIL.fetch_add(1, Ordering::Relaxed);
        FGN_WHY.store(16 + why, Ordering::Relaxed);
        false
    };
    if !path(PATH_FGN) {
        return fail(9);
    }
    if !path(PATH_FGN_WR) {
        return fail(10);
    }
    if glue::channel_state() != 0 {
        return fail(6);
    }
    let Some(fd) = glue::foreign_source(passive, adapter, dst.resource_id, path(PATH_FGN_ACQ)) else {
        return fail(1);
    };
    let mut pairs: Vec<(Rect, Rect)> = Vec::new();
    if pairs.try_reserve_exact(op.subs.len()).is_err() {
        return fail(7);
    }
    for r in &op.subs {
        let d = clip_to(r, &dst);
        let s = clip_to(&ga::bitblt_src(&d, &dr, &sr), &src);
        let d = ga::bitblt_src(&s, &sr, &dr);
        if !s.is_empty() && s.width() == d.width() && s.height() == d.height() {
            pairs.push((s, d));
        }
    }
    let fourcc = glue::fourcc_of(src.format);
    let step = match src.class {
        SurfaceClass::Vram => glue::vram_to_foreign(passive, adapter, src.resource_id, fourcc, &fd, &pairs),
        SurfaceClass::System => {
            let (_, spc) = cmd_pitches(&op.cmd);
            glue::standard_to_foreign(
                passive, adapter, src.resource_id, map_pitch(&src, spc), src.width, src.height, fourcc, &fd, &pairs,
            )
        }
        _ => 8,
    };
    if step != 0 {
        return fail(step);
    }
    FGN_WR.fetch_add(1, Ordering::Relaxed);
    BLT_N.fetch_add(1, Ordering::Relaxed);
    true
}

// ── content probes (384.1: Explorer's textures and the wallpaper end up RGB 0) ─────────────────

/// Per opcode 1..7 (index opcode - 1): probes made | destination row all RGB 0 after the command
/// << 10 | source row all RGB 0 << 20 (each saturating at 1023). Only commands into GPU surfaces.
static PRB: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];
static PRB_SEEN: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];
/// The last probe whose destination row came out all zero: opcode | source class bit << 4 | source
/// GDI type << 8 | destination GDI type << 12 | source all zero << 16 | engine << 18 (0 CE, 1 CPU,
/// 2 drop) | plan reason << 20 | DXGK rop enum << 26.
static PRB_K: AtomicU32 = AtomicU32::new(0);
/// The same probes for commands into STAGING destinations (`GdiPrbS1..7`, `GdiPrbSK`): whether
/// what the executor writes into a staging buffer reads back through its CPU view.
static PRB_S: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];
static PRB_S_SEEN: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];
static PRB_S_K: AtomicU32 = AtomicU32::new(0);
/// The first command into each of the first 4 GPU destinations, BEFORE it runs: resource id << 16
/// | magenta pixels (the `RvOff` 0x2000 clear) << 8 | RGB-0 pixels, of a row of up to 64 pixels.
static PRE: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
/// When a probed source row is all RGB 0: the whole source surface, 16 evenly spaced rows across
/// its full width. Sources scanned | sources with any non-zero pixel << 10 (each to 1023), and the
/// last scan: resource id << 16 | non-zero rows (of 16) << 8 | source GDI type. Non-zero elsewhere
/// means the command's rectangle points at an empty part (offset or pitch); all zero means the
/// surface holds nothing when the command runs.
static SRC_SCAN: AtomicU32 = AtomicU32::new(0);
static SRC_SCAN_K: AtomicU32 = AtomicU32::new(0);

fn scan_source(passive: PassiveLevel, adapter: &AdapterContext, src: &Surface, cmd_pitch: u32) {
    if src.width == 0 || src.height == 0 {
        return;
    }
    let mut rows_nonzero = 0u32;
    for k in 0..16u32 {
        let y = (src.height as u64 * k as u64 / 16) as i32;
        let w = src.width.min(4096) as i32;
        let Ok(row) = read_window(passive, adapter, src, &Rect::new(0, y, w, y + 1), pitch_of(src, cmd_pitch)) else {
            continue;
        };
        if !all_rgb_zero(&row) {
            rows_nonzero += 1;
        }
    }
    let _ = SRC_SCAN.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        let n = ((v & 0x3ff) + 1).min(0x3ff);
        let nz = ((v >> 10 & 0x3ff) + u32::from(rows_nonzero != 0)).min(0x3ff);
        Some(n | nz << 10)
    });
    SRC_SCAN_K.store((src.resource_id & 0xffff) << 16 | rows_nonzero << 8 | (src.kind_bits & 0xf), Ordering::Relaxed);
}

/// One row of up to 64 pixels through the middle of `sub` on `s` (surface coordinates).
fn probe_row(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, sub: &Rect, cmd_pitch: u32) -> Option<Vec<u8>> {
    let r = clip_to(sub, s);
    if r.is_empty() {
        return None;
    }
    let y = r.top + (r.height() / 2) as i32;
    let w = r.width().min(64);
    let x = r.left + ((r.width() - w) / 2) as i32;
    read_window(passive, adapter, s, &Rect::new(x, y, x + w as i32, y + 1), pitch_of(s, cmd_pitch)).ok()
}

fn rgb_zero(px: &[u8]) -> bool {
    px[0] == 0 && px[1] == 0 && px[2] == 0
}

fn all_rgb_zero(row: &[u8]) -> bool {
    row.chunks_exact(4).all(rgb_zero)
}

fn gpu_dst(op: &Op) -> Option<Surface> {
    op.dst.filter(|d| matches!(d.class, SurfaceClass::Vram | SurfaceClass::Foreign))
}

/// Before the command: the first touch of a GPU destination (did the magenta clear reach it?).
fn probe_before(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    if !path(PATH_DIAG) {
        return;
    }
    let (Some(d), Some(sub)) = (gpu_dst(op), op.subs.first()) else { return };
    let id = d.resource_id & 0xffff;
    if id == 0 || PRE.iter().any(|s| s.load(Ordering::Relaxed) >> 16 == id) {
        return;
    }
    let Some(slot) = PRE.iter().find(|s| s.load(Ordering::Relaxed) == 0) else { return };
    let Some(row) = probe_row(passive, adapter, &d, sub, cmd_pitches(&op.cmd).0) else { return };
    let magenta = row.chunks_exact(4).filter(|p| p[0] == 0xff && p[1] == 0 && p[2] == 0xff).count().min(255) as u32;
    let zero = row.chunks_exact(4).filter(|p| rgb_zero(p)).count().min(255) as u32;
    slot.store(id << 16 | magenta << 8 | zero, Ordering::Relaxed);
}

/// After the command: is the destination's row, and the source's matching row, all RGB 0?
fn probe_after(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    if !path(PATH_DIAG) {
        return;
    }
    let Some(sub) = op.subs.first() else { return };
    let (d, prb, seen, key) = match op.dst {
        Some(d) if matches!(d.class, SurfaceClass::Vram | SurfaceClass::Foreign) => (d, &PRB, &PRB_SEEN, &PRB_K),
        Some(d) if d.class == SurfaceClass::System => (d, &PRB_S, &PRB_S_SEEN, &PRB_S_K),
        _ => return,
    };
    let i = (op.cmd.opcode() as usize).wrapping_sub(1);
    if i >= prb.len() {
        return;
    }
    let n = seen[i].fetch_add(1, Ordering::Relaxed);
    if n >= 32 && n % 16 != 0 {
        return;
    }
    let (dpc, spc) = cmd_pitches(&op.cmd);
    let Some(drow) = probe_row(passive, adapter, &d, sub, dpc) else { return };
    let dz = all_rgb_zero(&drow);
    // The source window the command read for the same sub-rectangle (ClearType: its alpha
    // surface, where all zero means no glyph coverage).
    let sz = match (op.srcs[0], cpu::src_window(&op.cmd, sub)) {
        (Some(src), Some(sw)) => probe_row(passive, adapter, &src, &sw, spc).is_some_and(|r| all_rgb_zero(&r)),
        _ => false,
    };
    if sz && SRC_SCAN.load(Ordering::Relaxed) & 0x3ff < 64 {
        if let Some(src) = op.srcs[0] {
            scan_source(passive, adapter, &src, spc);
        }
    }
    let add = 1 | u32::from(dz) << 10 | u32::from(sz) << 20;
    let _ = prb[i].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        let f = |shift: u32| ((v >> shift) & 0x3ff) + ((add >> shift) & 0x3ff);
        let c = |x: u32| x.min(0x3ff);
        Some(c(f(0)) | c(f(10)) << 10 | c(f(20)) << 20)
    });
    if dz {
        let eng = match op.engine {
            Engine::Ce => 0,
            Engine::Cpu => 1,
            Engine::Drop => 2,
        };
        let rop = match op.cmd {
            Cmd::BitBlt { rop, .. } | Cmd::ColorFill { rop, .. } => u32::from(rop) & 0x7,
            _ => 0,
        };
        key.store(
            op.cmd.opcode()
                | class_bit_of(op.srcs[0]) << 4
                | op.srcs[0].map_or(0, |s| s.kind_bits & 0xf) << 8
                | (d.kind_bits & 0xf) << 12
                | u32::from(sz) << 16
                | eng << 18
                | op.why.map_or(0, |w| w.code()) << 20
                | rop << 26,
            Ordering::Relaxed,
        );
    }
}

fn execute(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    probe_before(passive, adapter, op);
    if opaque(op) {
        OPAQ_N.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(d) = op.dst.filter(|d| matches!(d.class, SurfaceClass::Vram | SurfaceClass::Foreign)) {
        FMT_K.store(op.srcs[0].map_or(0, |s| s.format & 0xffff) | (d.format & 0xffff) << 16, Ordering::Relaxed);
    }
    let lut = op.dst.is_some_and(|d| ga::is_lookup_table(d.kind_bits));
    if lut {
        note_lut(op);
    }
    let drops = crate::ddi::gdi_accel::DROP.load(Ordering::Relaxed);
    execute_inner(passive, adapter, op);
    if crate::ddi::gdi_accel::DROP.load(Ordering::Relaxed) == drops {
        note_dst_written(op);
        if lut {
            LUT_N.fetch_add(1 << 16, Ordering::Relaxed);
        }
    }
    probe_after(passive, adapter, op);
}

fn execute_inner(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    let fgn_copy = ga::is_foreign_copy(&op.cmd, op.dst.as_ref(), op.srcs[0].as_ref());
    // A plain copy to or from a foreign NVK image: one copy-engine copy between the image and the
    // other surface. When the copy engine cannot reach the other surface (a staging buffer outside
    // guest system pages has no copy-engine view), the CPU path does the copy: it moves the image's
    // side through the bounce buffer (`read_window` / `write_window`).
    if fgn_copy && op.engine != Engine::Drop && op.dst.is_some_and(|d| d.class == SurfaceClass::Foreign) {
        if !run_foreign_write(passive, adapter, op) {
            crate::ddi::gdi_accel::note_why(Why::CeFailed);
            run_cpu_counted(passive, adapter, op);
        }
        return;
    }
    if fgn_copy && op.engine != Engine::Drop && op.srcs[0].is_some_and(|s| s.class == SurfaceClass::Foreign) {
        if !run_foreign(passive, adapter, op) {
            crate::ddi::gdi_accel::note_why(Why::CeFailed);
            run_cpu_counted(passive, adapter, op);
        }
        return;
    }
    match op.engine {
        Engine::Drop => {
            if !matches!(op.cmd, Cmd::Escape) {
                crate::ddi::gdi_accel::DROP.fetch_add(1, Ordering::Relaxed);
            }
        }
        Engine::Ce => {
            if run_ce(passive, adapter, op) {
                match op.cmd {
                    Cmd::ColorFill { .. } => FILL_N.fetch_add(1, Ordering::Relaxed),
                    _ => BLT_N.fetch_add(1, Ordering::Relaxed),
                };
                self_check(passive, adapter, op, 1);
            } else {
                crate::ddi::gdi_accel::note_why(Why::CeFailed);
                run_cpu_counted(passive, adapter, op);
            }
        }
        Engine::Cpu if op.why == Some(Why::Overlap) => {
            // A scroll inside one surface: ordered non-overlapping copy-engine bands.
            if run_overlap(passive, adapter, op) {
                OVL_CE.fetch_add(1, Ordering::Relaxed);
                BLT_N.fetch_add(1, Ordering::Relaxed);
                self_check(passive, adapter, op, 4);
            } else {
                run_cpu_counted(passive, adapter, op);
            }
        }
        Engine::Cpu => {
            // A copy or fill that touches a staging buffer: the copy engine over its system pages
            // first, the CPU only when that is refused.
            let stage = if op.why != Some(Why::SystemSurface) {
                0
            } else if !path(PATH_SYS) {
                1
            } else {
                let before = SYS_FAIL.load(Ordering::Relaxed);
                if run_ce_sys(passive, adapter, op) {
                    4
                } else if SYS_FAIL.load(Ordering::Relaxed) != before {
                    3
                } else {
                    2
                }
            };
            if stage == 4 {
                SYS_CE.fetch_add(1, Ordering::Relaxed);
                match op.cmd {
                    Cmd::ColorFill { .. } => FILL_N.fetch_add(1, Ordering::Relaxed),
                    _ => BLT_N.fetch_add(1, Ordering::Relaxed),
                };
                self_check(passive, adapter, op, 2);
            } else {
                if stage != 0 {
                    note_sys_cpu(op, stage);
                }
                run_cpu_counted(passive, adapter, op);
            }
        }
    }
}

fn run_cpu_counted(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    CPU_MSK.fetch_or(op.why.map_or(1, |w| w.bit()), Ordering::Relaxed);
    CPU_ROP.store(rop_key(op), Ordering::Relaxed);
    CPU_STEP.store(0, Ordering::Relaxed);
    crate::ddi::build_paging_buffer::clear_std_fail();
    match run_cpu(passive, adapter, op) {
        Ok(()) => {
            FALL.fetch_add(1, Ordering::Relaxed);
            self_check(passive, adapter, op, 3);
        }
        Err(why) => {
            note_drop(op, why);
            crate::ddi::gdi_accel::note_why(why);
            crate::ddi::gdi_accel::DROP.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ── the copy engine ────────────────────────────────────────────────────────────────────────────

fn run_ce(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> bool {
    let Some(dst) = op.dst else {
        return false;
    };
    if glue::channel_state() != 0 {
        CE_WHY.store(1, Ordering::Relaxed);
        return false;
    }
    let Some(dv) = glue::ce_surface(passive, adapter, dst.resource_id) else {
        CE_WHY.store(2, Ordering::Relaxed);
        return false;
    };
    let sv = match op.cmd {
        Cmd::BitBlt { .. } => {
            let Some(src) = op.srcs[0] else {
                return false;
            };
            let Some(sv) = glue::ce_surface(passive, adapter, src.resource_id) else {
                CE_WHY.store(3, Ordering::Relaxed);
                return false;
            };
            Some(sv)
        }
        _ => None,
    };
    submit_and_wait(passive, op, &dv, sv.as_ref())
}

/// The copy-engine pushes of a SRCCOPY BitBlt (`sv` the source view) or a PATCOPY ColorFill into
/// `dv`, waited for. Spinlocks and polling only: callable inside `ce_sysmem::with_standard`.
fn submit_and_wait(passive: PassiveLevel, op: &Op, dv: &CeView, sv: Option<&CeView>) -> bool {
    let mut last = None;
    match (op.cmd, sv) {
        (Cmd::ColorFill { color, .. }, _) => {
            let color = if opaque(op) { color | 0xff00_0000 } else { color };
            let per = ga::rects_per_push(glue::SLOT_DWORDS, ga::FILL_STATE_DWORDS, ga::FILL_RECT_DWORDS).max(1);
            for chunk in op.subs.chunks(per) {
                let v = glue::submit(|p, _gen, done| {
                    ga::fill(p, color)?;
                    for r in chunk {
                        ga::fill_rect(p, dv.va, dv.pitch, r)?;
                    }
                    cp::release(p, done)
                });
                let Some(v) = v else {
                    CE_WHY.store(4, Ordering::Relaxed);
                    let _ = wait_last(passive, last);
                    return false;
                };
                CE_SUB.fetch_add(1, Ordering::Relaxed);
                last = Some(v);
            }
        }
        (Cmd::BitBlt { src: sr, dst: dr, .. }, Some(sv)) => {
            let swap = match (op.srcs[0], op.dst) {
                (Some(a), Some(b)) => ga::swaps_rb(&a, &b),
                _ => false,
            };
            let opq = opaque(op);
            let per = ga::rects_per_push(glue::SLOT_DWORDS, 0, ga::COPY_RECT_DWORDS).max(1);
            for chunk in op.subs.chunks(per) {
                let v = glue::submit(|p, gen, done| {
                    for r in chunk {
                        let s = ga::bitblt_src(r, &dr, &sr);
                        ga::copy_rect(p, gen, sv, &s, dv, r, ga::copy_remap(swap, opq))?;
                    }
                    cp::release(p, done)
                });
                let Some(v) = v else {
                    CE_WHY.store(4, Ordering::Relaxed);
                    let _ = wait_last(passive, last);
                    return false;
                };
                CE_SUB.fetch_add(1, Ordering::Relaxed);
                last = Some(v);
            }
        }
        _ => return false,
    }
    if wait_last(passive, last) {
        true
    } else {
        CE_WHY.store(5, Ordering::Relaxed);
        // The copies may still land; the CPU redo writes the same pixels.
        false
    }
}

/// Wait for the last submitted value (none: nothing to wait for). A push refused part-way still
/// waits for the earlier ones: nothing may be in flight once a caller's transaction ends.
fn wait_last(passive: PassiveLevel, last: Option<u64>) -> bool {
    match last {
        None => {
            LAST_WAIT_US.store(0, Ordering::Relaxed);
            true
        }
        Some(v) => {
            let t0 = now_100ns();
            let ok = glue::wait(passive, v, ga::CE_DEADLINE_MS);
            LAST_WAIT_US.store(us_since(t0), Ordering::Relaxed);
            ok
        }
    }
}

fn us_since(t0: u64) -> u32 {
    (now_100ns().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32
}

/// The view a staging command addresses its rectangles with: the mapping (made with the authored
/// pitch, one cache key per buffer) with the COMMAND's pitch when it has one (Learn
/// `DXGK_GDIARG_BITBLT`: the pitch of a `STAGING_CPUVISIBLE` surface is the command's), provided the
/// rows still fit the mapping. Counts a disagreement (`GdiPitchMis`, the last pair in
/// `GdiPitchCmd`/`GdiPitchAl`).
fn addr_view(v: &CeView, s: &Surface, cmd_pitch: u32) -> CeView {
    let mut out = *v;
    if cmd_pitch != 0 && cmd_pitch != v.pitch && ga::cmd_pitch_applies(s.kind_bits) {
        PITCH_MIS.fetch_add(1, Ordering::Relaxed);
        PITCH_CMD.store(cmd_pitch, Ordering::Relaxed);
        PITCH_AL.store(v.pitch, Ordering::Relaxed);
        if cmd_pitch >= s.width.saturating_mul(4) && cmd_pitch <= v.pitch {
            out.pitch = cmd_pitch;
        }
    }
    out
}

/// The pitch a staging buffer's copy-engine view is made with: the allocation's authored pitch
/// whenever it has one, so every call names the same descriptor (`ce_sysmem` caches per resource,
/// pitch and extent); the command's pitch only for a buffer without one.
fn map_pitch(s: &Surface, cmd_pitch: u32) -> u32 {
    if s.pitch != 0 {
        s.pitch
    } else {
        pitch_of(s, cmd_pitch)
    }
}

/// Submits `(src, dst)` band copies of one surface view in order (a ring-full submit waits for the
/// previous push and tries once more), then waits for the last. `0` done, 3 submit, 4 wait.
fn submit_bands(passive: PassiveLevel, v: &CeView, bands: &[(Rect, Rect)]) -> u32 {
    let per = ga::rects_per_push(glue::SLOT_DWORDS, 0, ga::COPY_RECT_DWORDS).max(1);
    let mut last: Option<u64> = None;
    for chunk in bands.chunks(per) {
        let push = |p: &mut cp::Push<'_>, gen: cp::Gen, done: cp::Release| {
            for (s, d) in chunk {
                ga::copy_rect(p, gen, v, s, v, d, cp::Remap::None)?;
            }
            cp::release(p, done)
        };
        let v1 = match glue::submit(push) {
            Some(x) => Some(x),
            None => {
                // Ring full (or a transient refusal): drain what is queued, then once more.
                if !wait_last(passive, last) {
                    return 4;
                }
                glue::submit(|p, gen, done| {
                    for (s, d) in chunk {
                        ga::copy_rect(p, gen, v, s, v, d, cp::Remap::None)?;
                    }
                    cp::release(p, done)
                })
            }
        };
        let Some(x) = v1 else {
            let _ = wait_last(passive, last);
            return 3;
        };
        CE_SUB.fetch_add(1, Ordering::Relaxed);
        last = Some(x);
    }
    if wait_last(passive, last) {
        0
    } else {
        4
    }
}

/// A SRCCOPY BitBlt whose source and destination overlap in one VRAM surface or staging buffer
/// (a scroll; CDD sends these although the caps ask it not to, 365.1 `GdiSlowOp` 0x12211): ordered
/// bands of `|dy|` rows (or `|dx|` columns) on the copy engine (`gdi_accel::split_overlap`).
fn run_overlap(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> bool {
    OVL_N.fetch_add(1, Ordering::Relaxed);
    let fail = |why: u32| {
        OVL_WHY.store(why, Ordering::Relaxed);
        false
    };
    let (Some(dst), Cmd::BitBlt { src: sr, dst: dr, rop, .. }) = (op.dst, op.cmd) else {
        return fail(1);
    };
    if rop != ga::rop::SRCCOPY || op.srcs[0].map(|s| s.resource_id) != Some(dst.resource_id) {
        return fail(1);
    }
    if !path(PATH_OVL) {
        return fail(5);
    }
    if glue::channel_state() != 0 {
        return fail(6);
    }
    let mut bands: Vec<(Rect, Rect)> = Vec::new();
    for sub in &op.subs {
        let d = clip_to(sub, &dst);
        let s = clip_to(&ga::bitblt_src(&d, &dr, &sr), &dst);
        if d.is_empty() || s.width() != d.width() || s.height() != d.height() {
            continue;
        }
        let mut ok = true;
        let need = (d.height().max(d.width())) as usize + 1;
        if bands.try_reserve(need).is_err() {
            return fail(1);
        }
        ga::split_overlap(&s, &d, |a, b| {
            if bands.try_reserve(1).is_ok() {
                bands.push((a, b));
            } else {
                ok = false;
            }
        });
        if !ok {
            return fail(1);
        }
    }
    if bands.is_empty() {
        return true;
    }
    let step = match dst.class {
        SurfaceClass::Vram => match glue::ce_surface(passive, adapter, dst.resource_id) {
            Some(v) => submit_bands(passive, &v, &bands),
            None => 2,
        },
        SurfaceClass::System => {
            let (dpc, _) = cmd_pitches(&op.cmd);
            match glue::with_standard(passive, adapter, dst.resource_id, map_pitch(&dst, dpc), dst.width, dst.height, |v| {
                submit_bands(passive, v, &bands)
            }) {
                Ok(s) => s,
                Err(_) => 2,
            }
        }
        _ => 1,
    };
    if step == 0 {
        true
    } else {
        fail(step)
    }
}

/// A SRCCOPY BitBlt or PATCOPY ColorFill with a KMD standard buffer (staging) on one side: one
/// copy-engine copy (or fill) over the buffer's system pages (`ce_sysmem::with_standard`), the
/// other side VRAM (or the fill's color). `false`: refused (not system-resident, partial leases,
/// channel busy, a staging-to-staging copy), the caller takes the CPU path.
fn run_ce_sys(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> bool {
    let t0 = now_100ns();
    let view_us = core::cell::Cell::new(0u32);
    let ok = run_ce_sys_inner(passive, adapter, op, t0, &view_us);
    let total = us_since(t0);
    if ok && total > SYS_US.load(Ordering::Relaxed) {
        let wt = LAST_WAIT_US.load(Ordering::Relaxed);
        SYS_US.store(total, Ordering::Relaxed);
        SYS_VW_US.store(view_us.get(), Ordering::Relaxed);
        SYS_WT_US.store(wt, Ordering::Relaxed);
        SYS_SUB_US.store(total.saturating_sub(wt).saturating_sub(view_us.get()), Ordering::Relaxed);
        SYS_PX.store(op.subs.iter().map(|r| area(r)).sum::<u64>().min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
        let wh = |s: Option<Surface>| s.map_or(0, |s| (s.width & 0xffff) << 16 | (s.height & 0xffff));
        let (src, dst) = (op.srcs[0], op.dst);
        SYS_K.store(src.map_or(0, |s| s.kind_bits & 0xffff) | dst.map_or(0, |d| d.kind_bits & 0xffff) << 16, Ordering::Relaxed);
        SYS_SWH.store(wh(src), Ordering::Relaxed);
        SYS_DWH.store(wh(dst), Ordering::Relaxed);
        SYS_RES.store(src.map_or(0, |s| s.resource_id & 0xffff) | dst.map_or(0, |d| d.resource_id & 0xffff) << 16, Ordering::Relaxed);
        let same = matches!((src, dst), (Some(a), Some(b)) if a.resource_id == b.resource_id);
        let whole = dst.is_some_and(|d| op.subs.iter().map(|r| area(&clip_to(r, &d))).sum::<u64>() >= u64::from(d.width) * u64::from(d.height));
        let same_wh = wh(src) == wh(dst) && src.is_some();
        SYS_SHAPE.store(
            (op.subs.len().min(0xffff) as u32) | u32::from(same) << 16 | u32::from(whole) << 17 | u32::from(same_wh) << 18,
            Ordering::Relaxed,
        );
    }
    ok
}

fn run_ce_sys_inner(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    op: &Op,
    t0: u64,
    view_us: &core::cell::Cell<u32>,
) -> bool {
    let mark = || view_us.set((now_100ns().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32);
    let Some(dst) = op.dst else {
        return false;
    };
    if glue::channel_state() != 0 {
        sys_refused(3, 128);
        SYS_REF.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let (dpc, spc) = cmd_pitches(&op.cmd);
    let r = match op.cmd {
        Cmd::ColorFill { rop, .. } if rop == ga::cfrop::PATCOPY && dst.class == SurfaceClass::System => {
            let dp = map_pitch(&dst, dpc);
            glue::with_standard(passive, adapter, dst.resource_id, dp, dst.width, dst.height, |dv| {
                mark();
                submit_and_wait(passive, op, &addr_view(dv, &dst, dpc), None)
            })
        }
        Cmd::BitBlt { rop, .. } if rop == ga::rop::SRCCOPY => {
            let Some(src) = op.srcs[0] else {
                return false;
            };
            match (src.class, dst.class) {
                (SurfaceClass::Vram, SurfaceClass::System) => {
                    // The VRAM side first: it takes the channel's I/O itself.
                    let Some(sv) = glue::ce_surface(passive, adapter, src.resource_id) else {
                        sys_refused(2, 2);
                        SYS_REF.fetch_add(1, Ordering::Relaxed);
                        return false;
                    };
                    let dp = map_pitch(&dst, dpc);
                    glue::with_standard(passive, adapter, dst.resource_id, dp, dst.width, dst.height, |dv| {
                        mark();
                        submit_and_wait(passive, op, &addr_view(dv, &dst, dpc), Some(&sv))
                    })
                }
                (SurfaceClass::System, SurfaceClass::Vram) => {
                    let Some(dv) = glue::ce_surface(passive, adapter, dst.resource_id) else {
                        sys_refused(2, 2);
                        SYS_REF.fetch_add(1, Ordering::Relaxed);
                        return false;
                    };
                    let sp = map_pitch(&src, spc);
                    glue::with_standard(passive, adapter, src.resource_id, sp, src.width, src.height, |sv| {
                        mark();
                        submit_and_wait(passive, op, &dv, Some(&addr_view(sv, &src, spc)))
                    })
                }
                (SurfaceClass::System, SurfaceClass::System) if src.resource_id == dst.resource_id => {
                    // Within one staging buffer (disjoint: an overlapping one is planned `Overlap`
                    // and never comes here): one view serves both sides.
                    let p = map_pitch(&dst, dpc);
                    glue::with_standard(passive, adapter, dst.resource_id, p, dst.width, dst.height, |v| {
                        mark();
                        submit_and_wait(passive, op, &addr_view(v, &dst, dpc), Some(&addr_view(v, &src, spc)))
                    })
                }
                (SurfaceClass::System, SurfaceClass::System) if path(PATH_PAIR) => {
                    // Staging to another staging buffer: both views in one content transaction.
                    let a = (src.resource_id, map_pitch(&src, spc), src.width, src.height);
                    let b = (dst.resource_id, map_pitch(&dst, dpc), dst.width, dst.height);
                    glue::with_standard_pair(passive, adapter, a, b, |sv, dv| {
                        mark();
                        submit_and_wait(passive, op, &addr_view(dv, &dst, dpc), Some(&addr_view(sv, &src, spc)))
                    })
                }
                _ => {
                    sys_refused(1, 1);
                    SYS_REF.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
        }
        _ => return false,
    };
    match r {
        Ok(true) => {
            if matches!((op.srcs[0].map(|s| s.class), dst.class), (Some(SurfaceClass::Vram), SurfaceClass::System)) {
                RD_BACK.fetch_add(1, Ordering::Relaxed);
            }
            true
        }
        Ok(false) => {
            SYS_FAIL.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(e) => {
            let bit = match e.class {
                glue::SysClass::NotSystem => 4,
                glue::SysClass::Uncovered => 8,
                glue::SysClass::Busy => 16,
                glue::SysClass::Unsure => 32,
                glue::SysClass::Other => 64,
            };
            sys_refused(e.word, bit);
            SYS_REF.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

// ── the CPU ────────────────────────────────────────────────────────────────────────────────────

/// The largest window the CPU path reads (a 4K surface is 33 MB).
const MAX_WINDOW_BYTES: u64 = 64 << 20;

fn bound(subs: &[Rect]) -> Rect {
    subs.iter().fold(Rect::default(), |a, r| a.union(r))
}

fn clip_to(r: &Rect, s: &Surface) -> Rect {
    r.intersect(&Rect::new(0, 0, s.width as i32, s.height as i32))
}

/// Read `rect` (inside `s`) packed (`w * 4` per row). `pitch` is the byte stride of a system
/// surface's CPU view.
fn read_window(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, rect: &Rect, pitch: u32) -> Result<Vec<u8>, Why> {
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let bytes = (w as u64) * 4 * h as u64;
    if bytes == 0 || bytes > MAX_WINDOW_BYTES {
        cpu_step(1);
        return Err(Why::CpuFailed);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(bytes as usize).map_err(|_| {
        cpu_step(7);
        Why::CpuFailed
    })?;
    out.resize(bytes as usize, 0);
    match s.class {
        SurfaceClass::Vram => {
            if !glue::vram_read(passive, adapter, s.resource_id, *rect, &mut out, w * 4) {
                cpu_step(4);
                return Err(Why::CpuFailed);
            }
        }
        SurfaceClass::System => {
            let p = pitch as u64;
            if p < (rect.right as u64) * 4 {
                cpu_step(2);
                return Err(Why::OutOfBounds);
            }
            let start = rect.top as u64 * p + rect.left as u64 * 4;
            let span = (h as u64 - 1) * p + w as u64 * 4;
            if span > MAX_WINDOW_BYTES * 2 {
                cpu_step(3);
                return Err(Why::CpuFailed);
            }
            let mut tmp = Vec::new();
            tmp.try_reserve_exact(span as usize).map_err(|_| {
                cpu_step(7);
                Why::CpuFailed
            })?;
            tmp.resize(span as usize, 0);
            if !glue::std_read(passive, adapter, s.resource_id, start, &mut tmp) {
                cpu_step(4);
                return Err(Why::CpuFailed);
            }
            for y in 0..h {
                let from = y * p as usize;
                out[y * w * 4..(y + 1) * w * 4].copy_from_slice(&tmp[from..from + w * 4]);
            }
        }
        SurfaceClass::Foreign => {
            if !path(PATH_FGN) {
                return Err(Why::Unreachable);
            }
            if !glue::foreign_read(passive, adapter, s.resource_id, *rect, &mut out, w * 4) {
                cpu_step(4);
                FGN_RMW_FAIL.fetch_add(1, Ordering::Relaxed);
                return Err(Why::CpuFailed);
            }
            FGN_RMW_RD.fetch_add(1, Ordering::Relaxed);
        }
        SurfaceClass::Unreachable => return Err(Why::Unreachable),
    }
    Ok(out)
}

fn write_window(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, rect: &Rect, pitch: u32, data: &mut [u8]) -> Result<(), Why> {
    let r = write_window_inner(passive, adapter, s, rect, pitch, data);
    if r.is_err() {
        cpu_step(5);
    }
    r
}

fn write_window_inner(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, rect: &Rect, pitch: u32, data: &mut [u8]) -> Result<(), Why> {
    let w = rect.width();
    match s.class {
        SurfaceClass::Vram => {
            if glue::vram_write(passive, adapter, s.resource_id, *rect, data, w as usize * 4) {
                Ok(())
            } else {
                Err(Why::CpuFailed)
            }
        }
        SurfaceClass::System => {
            let start = rect.top as u64 * pitch as u64 + rect.left as u64 * 4;
            if glue::std_write(passive, adapter, s.resource_id, start, pitch, w * 4, rect.height(), data) {
                Ok(())
            } else {
                Err(Why::CpuFailed)
            }
        }
        SurfaceClass::Foreign => {
            if !path(PATH_FGN) || !path(PATH_FGN_WR) {
                return Err(Why::Unreachable);
            }
            if glue::foreign_upload(passive, adapter, s.resource_id, *rect, data, w as usize * 4) {
                FGN_RMW_WR.fetch_add(1, Ordering::Relaxed);
                Ok(())
            } else {
                FGN_RMW_FAIL.fetch_add(1, Ordering::Relaxed);
                Err(Why::CpuFailed)
            }
        }
        SurfaceClass::Unreachable => Err(Why::Unreachable),
    }
}

/// The CPU stride of a surface for this command: the command's pitch for a `STAGING_CPUVISIBLE`
/// or `EXISTINGSYSMEM` surface when it carries one, else the allocation's. Learn
/// `DXGK_GDIARG_BITBLT` remarks: the pitches locate the rectangles for those two GDI surface
/// types only and "should be ignored for other allocation types" (CDD's staging and lookup-table
/// surfaces, shadows).
fn pitch_of(s: &Surface, cmd_pitch: u32) -> u32 {
    let applies = ga::cmd_pitch_applies(s.kind_bits);
    if s.class == SurfaceClass::System && cmd_pitch != 0 && !applies && cmd_pitch != s.pitch {
        PITCH_IGN.fetch_add(1, Ordering::Relaxed);
        PITCH_IGN_V.store(cmd_pitch, Ordering::Relaxed);
    }
    if s.class == SurfaceClass::System && applies && cmd_pitch >= s.width.saturating_mul(4) && cmd_pitch != 0 {
        cmd_pitch
    } else if s.pitch != 0 {
        s.pitch
    } else {
        s.width.saturating_mul(4)
    }
}

fn cmd_pitches(cmd: &Cmd) -> (u32, u32) {
    match *cmd {
        Cmd::BitBlt { dst_pitch, src_pitch, .. } => (dst_pitch, src_pitch),
        Cmd::AlphaBlend { src_pitch, .. }
        | Cmd::StretchBlt { src_pitch, .. }
        | Cmd::TransparentBlt { src_pitch, .. } => (0, src_pitch),
        Cmd::ClearTypeBlend { alpha_pitch, .. } => (0, alpha_pitch),
        _ => (0, 0),
    }
}

fn area(r: &Rect) -> u64 {
    r.width() as u64 * r.height() as u64
}

/// The CPU path. A plain copy (SRCCOPY BitBlt, not overlapping its own source) or a plain fill
/// (PATCOPY) writes each destination sub-rectangle straight from the source (or the color): no
/// read of the destination, one transfer per side per sub-rectangle. Every other operation reads,
/// computes and writes back; one window per sub-rectangle when the sub-rectangles cover less than
/// half of their bounding box, else the bounding box once.
fn run_cpu(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> Result<(), Why> {
    let dst = op.dst.ok_or(Why::BadIndex)?;
    if op.subs.is_empty() {
        return Ok(());
    }
    match op.cmd {
        Cmd::BitBlt { rop, .. } if rop == ga::rop::SRCCOPY && op.why != Some(Why::Overlap) => {
            return run_cpu_copy(passive, adapter, op, &dst);
        }
        Cmd::ColorFill { rop, color, .. } if rop == ga::cfrop::PATCOPY => {
            return run_cpu_fill(passive, adapter, op, &dst, color);
        }
        _ => {}
    }
    let bbox = clip_to(&bound(&op.subs), &dst);
    let covered: u64 = op.subs.iter().map(area).sum();
    if op.subs.len() > 1 && covered * 2 < area(&bbox) {
        for sub in &op.subs {
            run_cpu_window(passive, adapter, op, &dst, core::slice::from_ref(sub))?;
        }
        Ok(())
    } else {
        run_cpu_window(passive, adapter, op, &dst, &op.subs)
    }
}

/// SRCCOPY per sub-rectangle: read the source rectangle, write it to the destination rectangle.
fn run_cpu_copy(passive: PassiveLevel, adapter: &AdapterContext, op: &Op, dst: &Surface) -> Result<(), Why> {
    let Cmd::BitBlt { src: sr, dst: dr, .. } = op.cmd else {
        return Err(Why::BadIndex);
    };
    let src = op.srcs[0].ok_or(Why::BadIndex)?;
    let opq = opaque(op);
    let (dpc, spc) = cmd_pitches(&op.cmd);
    let (dpitch, spitch) = (pitch_of(dst, dpc), pitch_of(&src, spc));
    if src.class == SurfaceClass::Vram && dst.class == SurfaceClass::System {
        RD_BACK.fetch_add(1, Ordering::Relaxed);
    }
    for sub in &op.subs {
        let d = clip_to(sub, dst);
        let s = clip_to(&ga::bitblt_src(&d, &dr, &sr), &src);
        if d.is_empty() || s.width() != d.width() || s.height() != d.height() {
            continue;
        }
        let mut buf = read_window(passive, adapter, &src, &s, spitch)?;
        // A foreign image's window is already B G R A (`glue::foreign_read` / `foreign_upload`).
        let bgra = |x: &Surface| if x.class == SurfaceClass::Foreign { Surface { format: 21, ..*x } } else { *x };
        if ga::swaps_rb(&bgra(&src), &bgra(dst)) {
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        if opq {
            for px in buf.chunks_exact_mut(4) {
                px[3] = 0xff;
            }
        }
        write_window(passive, adapter, dst, &d, dpitch, &mut buf)?;
    }
    Ok(())
}

/// PATCOPY per sub-rectangle: the color written, nothing read.
fn run_cpu_fill(passive: PassiveLevel, adapter: &AdapterContext, op: &Op, dst: &Surface, color: u32) -> Result<(), Why> {
    let color = if opaque(op) { color | 0xff00_0000 } else { color };
    let dpitch = pitch_of(dst, 0);
    for sub in &op.subs {
        let d = clip_to(sub, dst);
        let bytes = area(&d) * 4;
        if bytes == 0 {
            continue;
        }
        if bytes > MAX_WINDOW_BYTES {
            cpu_step(6);
            return Err(Why::CpuFailed);
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(bytes as usize).map_err(|_| {
            cpu_step(7);
            Why::CpuFailed
        })?;
        let px = color.to_le_bytes();
        for _ in 0..area(&d) {
            buf.extend_from_slice(&px);
        }
        write_window(passive, adapter, dst, &d, dpitch, &mut buf)?;
    }
    Ok(())
}

/// Read, compute, write back the window bounding `subs`.
fn run_cpu_window(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    op: &Op,
    dst: &Surface,
    subs: &[Rect],
) -> Result<(), Why> {
    let (dst_pitch_cmd, src_pitch_cmd) = cmd_pitches(&op.cmd);
    let dwin = clip_to(&bound(subs), dst);
    if dwin.is_empty() {
        return Ok(());
    }
    let dpitch = pitch_of(dst, dst_pitch_cmd);
    let mut dbuf = read_window(passive, adapter, dst, &dwin, dpitch)?;

    // The source window (or the alpha surface's for ClearType): only what these sub-rectangles read.
    let mut sbuf: Option<(Vec<u8>, Rect)> = None;
    if let Some(src) = op.srcs[0] {
        let mut swin = Rect::default();
        for sub in subs {
            if let Some(r) = cpu::src_window(&op.cmd, sub) {
                swin = swin.union(&r);
            }
        }
        let swin = clip_to(&swin, &src);
        if !swin.is_empty() {
            let spitch = pitch_of(&src, src_pitch_cmd);
            sbuf = Some((read_window(passive, adapter, &src, &swin, spitch)?, swin));
        }
    }
    // ClearType's gamma row (8 bpp, 512 entries) from the gamma surface.
    let mut gamma_row: Option<[u8; 512]> = None;
    if let (Cmd::ClearTypeBlend { gamma, .. }, Some(g)) = (op.cmd, op.srcs[1]) {
        if gamma != ga::INVALID_GAMMA && gamma < 16 && g.class == SurfaceClass::System {
            let pitch = if g.pitch != 0 { g.pitch } else { 512 };
            let mut row = [0u8; 512];
            if glue::std_read(passive, adapter, g.resource_id, gamma as u64 * pitch as u64, &mut row) {
                gamma_row = Some(row);
            }
        }
    }

    let mut done = cpu::Done::default();
    {
        let w = dwin.width();
        let h = dwin.height();
        let mut dv = cpu::ViewMut { data: &mut dbuf, pitch: w as usize * 4, x0: dwin.left, y0: dwin.top, w, h };
        let sv = sbuf.as_ref().map(|(b, r)| cpu::View {
            data: b,
            pitch: r.width() as usize * 4,
            x0: r.left,
            y0: r.top,
            w: r.width(),
            h: r.height(),
        });
        for sub in subs {
            // An unscaled BitBlt (any ROP) row by row; the per-pixel reference otherwise.
            let rows = matches!(op.cmd, Cmd::BitBlt { .. })
                && sv.as_ref().is_some_and(|s| cpu::bitblt_rows(&op.cmd, sub, &mut dv, s, &mut done));
            if !rows {
                cpu::run(&op.cmd, sub, &mut dv, sv.as_ref(), gamma_row.as_ref(), &mut done);
            }
        }
    }
    if opaque(op) {
        force_alpha(&mut dbuf, &dwin, subs);
    }
    write_window(passive, adapter, dst, &dwin, dpitch, &mut dbuf)
}

/// Clip `r` to the destination surface and to `DstRect`.
pub(crate) fn clip_sub(r: &Rect, dst_rect: &Rect, dst: Option<&Surface>) -> Rect {
    let mut c = r.intersect(dst_rect);
    if let Some(s) = dst {
        c = clip_to(&c, s);
    }
    c
}

/// A CE view of a cached VRAM mapping, for tests of the plan against live surfaces (unused when
/// the glue has no VRAM module).
#[allow(dead_code)]
pub(crate) fn view_of(s: &Surface, va: u64) -> CeView {
    CeView { va, pitch: s.pitch, width: s.width, height: s.height }
}
