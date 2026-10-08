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
//!    same sequence; an id no longer in the table (already executed) gets no wait. The WDDM fence
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
static PATHS: AtomicU32 = AtomicU32::new(PATH_FGN | PATH_SYS | PATH_PAIR | PATH_OVL);
const PATH_FGN: u32 = 1;
const PATH_FGN_ACQ: u32 = 2;
const PATH_SYS: u32 = 4;
const PATH_PAIR: u32 = 8;
const PATH_OVL: u32 = 16;

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
        &DST_RES, &DST_WH, &CH_UP, &CE_WHY, &RD_BACK, &SYS_CE, &SYS_REF, &SYS_FAIL, &SYS_WHY, &SYS_MSK, &SYS_CPU_K, &CH_UP_US, &SLOW_US, &SLOW_OP, &FGN_CE, &FGN_FAIL, &FGN_WHY, &FGN_WR, &OVL_N, &OVL_CE, &OVL_WHY, &JOB_N_OPS, &CHK_SEEN, &CHK_N, &CHK_BAD, &CHK_K, &CHK_GOT, &CHK_WANT,
    ] {
        c.store(0, Ordering::Relaxed);
    }
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
    w(b"GdiChUpUs", CH_UP_US.load(Ordering::Relaxed));
    w(b"GdiSlowUs", SLOW_US.load(Ordering::Relaxed));
    w(b"GdiSlowOp", SLOW_OP.load(Ordering::Relaxed));
    w(b"GdiFgnCe", FGN_CE.load(Ordering::Relaxed));
    w(b"GdiFgnFail", FGN_FAIL.load(Ordering::Relaxed));
    w(b"GdiFgnWhy", FGN_WHY.load(Ordering::Relaxed));
    w(b"GdiFgnWr", FGN_WR.load(Ordering::Relaxed));
    w(b"GdiPaths", PATHS.load(Ordering::Relaxed));
    w(b"GdiOvlN", OVL_N.load(Ordering::Relaxed));
    w(b"GdiOvlCe", OVL_CE.load(Ordering::Relaxed));
    w(b"GdiOvlWhy", OVL_WHY.load(Ordering::Relaxed));
    w(b"GdiJobMaxN", JOB_N_OPS.load(Ordering::Relaxed));
    w(b"GdiChkN", CHK_N.load(Ordering::Relaxed));
    w(b"GdiChkBad", CHK_BAD.load(Ordering::Relaxed));
    w(b"GdiChkK", CHK_K.load(Ordering::Relaxed));
    w(b"GdiChkGot", CHK_GOT.load(Ordering::Relaxed));
    w(b"GdiChkWant", CHK_WANT.load(Ordering::Relaxed));
    w(b"GdiJobT1", JOB_T[0].load(Ordering::Relaxed));
    w(b"GdiJobT1Us", JOB_T[1].load(Ordering::Relaxed));
    w(b"GdiJobT2", JOB_T[2].load(Ordering::Relaxed));
    w(b"GdiJobT2Us", JOB_T[3].load(Ordering::Relaxed));
    w(b"GdiJobT3", JOB_T[4].load(Ordering::Relaxed));
    w(b"GdiJobT3Us", JOB_T[5].load(Ordering::Relaxed));
    w(b"GdiThr", u32::from(crate::ddi::gdi_thread::running()));
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
pub(crate) fn commit(ops: Vec<Op>, ctx: usize) -> u64 {
    let mut job = Job { id: 0, ctx, seq: None, ops, running: false };
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

/// SubmitCommand without a private record, on GDI context `ctx` (DISPATCH): the context's oldest
/// unclaimed job. dxgkrnl submits a context's DMA buffers in the order it rendered them, so this
/// names the buffer being submitted; a preempted replay without its record would claim the next
/// one instead (counted with the claims, `GdiCtxClm`). `None`: nothing unclaimed.
pub(crate) fn oldest_unclaimed(ctx: usize) -> Option<u64> {
    let t = TABLE.lock();
    t.jobs.iter().filter(|j| j.ctx == ctx && j.seq.is_none()).map(|j| j.id).min()
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
        crate::ddi::gdi_accel::publish_counters();
    }
    more
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
/// There is no CPU fallback (the image has no CPU view): a failure drops the command.
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
/// (`ce_vram::foreign_write`). No CPU fallback: a failure drops the command.
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

fn execute(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    if op.engine != Engine::Drop && op.dst.is_some_and(|d| d.class == SurfaceClass::Foreign) {
        if !run_foreign_write(passive, adapter, op) {
            crate::ddi::gdi_accel::note_why(Why::CeFailed);
            crate::ddi::gdi_accel::DROP.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    if op.engine != Engine::Drop && op.srcs[0].is_some_and(|s| s.class == SurfaceClass::Foreign) {
        if !run_foreign(passive, adapter, op) {
            crate::ddi::gdi_accel::note_why(Why::CeFailed);
            crate::ddi::gdi_accel::DROP.fetch_add(1, Ordering::Relaxed);
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
    match run_cpu(passive, adapter, op) {
        Ok(()) => {
            FALL.fetch_add(1, Ordering::Relaxed);
            self_check(passive, adapter, op, 3);
        }
        Err(why) => {
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
            let per = ga::rects_per_push(glue::SLOT_DWORDS, 0, ga::COPY_RECT_DWORDS).max(1);
            for chunk in op.subs.chunks(per) {
                let v = glue::submit(|p, gen, done| {
                    for r in chunk {
                        let s = ga::bitblt_src(r, &dr, &sr);
                        ga::copy_rect(p, gen, sv, &s, dv, r, swap)?;
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
        None => true,
        Some(v) => glue::wait(passive, v, ga::CE_DEADLINE_MS),
    }
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
                ga::copy_rect(p, gen, v, s, v, d, false)?;
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
                        ga::copy_rect(p, gen, v, s, v, d, false)?;
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
                submit_and_wait(passive, op, dv, None)
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
                        submit_and_wait(passive, op, dv, Some(&sv))
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
                        submit_and_wait(passive, op, &dv, Some(sv))
                    })
                }
                (SurfaceClass::System, SurfaceClass::System) if src.resource_id == dst.resource_id => {
                    // Within one staging buffer (disjoint: an overlapping one is planned `Overlap`
                    // and never comes here): one view serves both sides.
                    let p = map_pitch(&dst, dpc);
                    glue::with_standard(passive, adapter, dst.resource_id, p, dst.width, dst.height, |v| {
                        submit_and_wait(passive, op, v, Some(v))
                    })
                }
                (SurfaceClass::System, SurfaceClass::System) if path(PATH_PAIR) => {
                    // Staging to another staging buffer: both views in one content transaction.
                    let a = (src.resource_id, map_pitch(&src, spc), src.width, src.height);
                    let b = (dst.resource_id, map_pitch(&dst, dpc), dst.width, dst.height);
                    glue::with_standard_pair(passive, adapter, a, b, |sv, dv| submit_and_wait(passive, op, dv, Some(sv)))
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
        return Err(Why::CpuFailed);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(bytes as usize).map_err(|_| Why::CpuFailed)?;
    out.resize(bytes as usize, 0);
    match s.class {
        SurfaceClass::Vram => {
            if !glue::vram_read(passive, adapter, s.resource_id, *rect, &mut out, w * 4) {
                return Err(Why::CpuFailed);
            }
        }
        SurfaceClass::System => {
            let p = pitch as u64;
            if p < (rect.right as u64) * 4 {
                return Err(Why::OutOfBounds);
            }
            let start = rect.top as u64 * p + rect.left as u64 * 4;
            let span = (h as u64 - 1) * p + w as u64 * 4;
            if span > MAX_WINDOW_BYTES * 2 {
                return Err(Why::CpuFailed);
            }
            let mut tmp = Vec::new();
            tmp.try_reserve_exact(span as usize).map_err(|_| Why::CpuFailed)?;
            tmp.resize(span as usize, 0);
            if !glue::std_read(passive, adapter, s.resource_id, start, &mut tmp) {
                return Err(Why::CpuFailed);
            }
            for y in 0..h {
                let from = y * p as usize;
                out[y * w * 4..(y + 1) * w * 4].copy_from_slice(&tmp[from..from + w * 4]);
            }
        }
        SurfaceClass::Unreachable | SurfaceClass::Foreign => return Err(Why::Unreachable),
    }
    Ok(out)
}

fn write_window(passive: PassiveLevel, adapter: &AdapterContext, s: &Surface, rect: &Rect, pitch: u32, data: &mut [u8]) -> Result<(), Why> {
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
        SurfaceClass::Unreachable | SurfaceClass::Foreign => Err(Why::Unreachable),
    }
}

/// The CPU stride of a surface for this command: a staging surface's command pitch when the
/// command carries one (Learn `DXGK_GDIARG_BITBLT` remarks), else the allocation's.
fn pitch_of(s: &Surface, cmd_pitch: u32) -> u32 {
    if s.class == SurfaceClass::System && cmd_pitch >= s.width.saturating_mul(4) && cmd_pitch != 0 {
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
        if ga::swaps_rb(&src, dst) {
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        write_window(passive, adapter, dst, &d, dpitch, &mut buf)?;
    }
    Ok(())
}

/// PATCOPY per sub-rectangle: the color written, nothing read.
fn run_cpu_fill(passive: PassiveLevel, adapter: &AdapterContext, op: &Op, dst: &Surface, color: u32) -> Result<(), Why> {
    let dpitch = pitch_of(dst, 0);
    for sub in &op.subs {
        let d = clip_to(sub, dst);
        let bytes = area(&d) * 4;
        if bytes == 0 {
            continue;
        }
        if bytes > MAX_WINDOW_BYTES {
            return Err(Why::CpuFailed);
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(bytes as usize).map_err(|_| Why::CpuFailed)?;
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
            cpu::run(&op.cmd, sub, &mut dv, sv.as_ref(), gamma_row.as_ref(), &mut done);
        }
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
