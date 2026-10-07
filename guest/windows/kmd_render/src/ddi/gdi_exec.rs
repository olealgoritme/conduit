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
pub(crate) fn reset_for_start(_on: bool) {
    for c in [
        &BLT_N, &FILL_N, &FALL, &JOB_N, &AGAIN, &DONE, &ORPH, &CE_SUB, &US, &US_MAX, &RECTS, &CLS,
        &DST_RES, &DST_WH,
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
}

fn class_bit(c: SurfaceClass) -> u32 {
    match c {
        SurfaceClass::Vram => 1,
        SurfaceClass::System => 2,
        SurfaceClass::Unreachable => 4,
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
            adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
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

/// Jobs executed per worker pass before it yields (and re-signals itself).
const JOBS_PER_PASS: usize = 32;

/// The HPD worker (PASSIVE): execute admitted jobs in sequence order. With the knob off, or
/// nothing admitted: one relaxed load.
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    if !crate::ddi::gdi_accel::on() || PENDING.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut ran = 0usize;
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
        let t0 = now_100ns();
        for op in &ops {
            execute(passive, adapter, op);
        }
        let us = (now_100ns().wrapping_sub(t0) / 10).min(u64::from(u32::MAX)) as u32;
        US.fetch_add(us, Ordering::Relaxed);
        US_MAX.fetch_max(us, Ordering::Relaxed);
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
        if ran >= JOBS_PER_PASS {
            adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
            break;
        }
    }
    if ran > 0 {
        crate::ddi::gdi_accel::publish_counters();
    }
}

fn execute(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
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
            } else {
                crate::ddi::gdi_accel::note_why(Why::CeFailed);
                run_cpu_counted(passive, adapter, op);
            }
        }
        Engine::Cpu => run_cpu_counted(passive, adapter, op),
    }
}

fn run_cpu_counted(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) {
    match run_cpu(passive, adapter, op) {
        Ok(()) => {
            FALL.fetch_add(1, Ordering::Relaxed);
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
    let Some(dv) = glue::ce_surface(passive, adapter, dst.resource_id) else {
        return false;
    };
    let mut last = None;
    match op.cmd {
        Cmd::ColorFill { color, .. } => {
            let per = ga::rects_per_push(glue::SLOT_DWORDS, ga::FILL_STATE_DWORDS, ga::FILL_RECT_DWORDS).max(1);
            for chunk in op.subs.chunks(per) {
                let v = glue::submit(|p, _gen, done| {
                    ga::fill(p, color)?;
                    for r in chunk {
                        ga::fill_rect(p, dv.va, dv.pitch, r)?;
                    }
                    cp::release(p, done)
                });
                let Some(v) = v else { return false };
                CE_SUB.fetch_add(1, Ordering::Relaxed);
                last = Some(v);
            }
        }
        Cmd::BitBlt { src: sr, dst: dr, .. } => {
            let Some(src) = op.srcs[0] else {
                return false;
            };
            let Some(sv) = glue::ce_surface(passive, adapter, src.resource_id) else {
                return false;
            };
            let per = ga::rects_per_push(glue::SLOT_DWORDS, 0, ga::COPY_RECT_DWORDS).max(1);
            for chunk in op.subs.chunks(per) {
                let v = glue::submit(|p, gen, done| {
                    for r in chunk {
                        let s = ga::bitblt_src(r, &dr, &sr);
                        ga::copy_rect(p, gen, &sv, &s, &dv, r)?;
                    }
                    cp::release(p, done)
                });
                let Some(v) = v else { return false };
                CE_SUB.fetch_add(1, Ordering::Relaxed);
                last = Some(v);
            }
        }
        _ => return false,
    }
    match last {
        None => true,
        Some(v) => {
            if glue::wait(passive, v, ga::CE_DEADLINE_MS) {
                true
            } else {
                // The copies may still land; the CPU redo writes the same pixels.
                false
            }
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
        SurfaceClass::Unreachable => return Err(Why::Unreachable),
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
        SurfaceClass::Unreachable => Err(Why::Unreachable),
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

fn run_cpu(passive: PassiveLevel, adapter: &AdapterContext, op: &Op) -> Result<(), Why> {
    let dst = op.dst.ok_or(Why::BadIndex)?;
    if op.subs.is_empty() {
        return Ok(());
    }
    let (dst_pitch_cmd, src_pitch_cmd) = match op.cmd {
        Cmd::BitBlt { dst_pitch, src_pitch, .. } => (dst_pitch, src_pitch),
        Cmd::AlphaBlend { src_pitch, .. }
        | Cmd::StretchBlt { src_pitch, .. }
        | Cmd::TransparentBlt { src_pitch, .. } => (0, src_pitch),
        Cmd::ClearTypeBlend { alpha_pitch, .. } => (0, alpha_pitch),
        _ => (0, 0),
    };
    let dwin = clip_to(&bound(&op.subs), &dst);
    if dwin.is_empty() {
        return Ok(());
    }
    let dpitch = pitch_of(&dst, dst_pitch_cmd);
    let mut dbuf = read_window(passive, adapter, &dst, &dwin, dpitch)?;

    // The source window (or the alpha surface's for ClearType).
    let mut sbuf: Option<(Vec<u8>, Rect)> = None;
    if let Some(src) = op.srcs[0] {
        let mut swin = Rect::default();
        for sub in &op.subs {
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
        for sub in &op.subs {
            cpu::run(&op.cmd, sub, &mut dv, sv.as_ref(), gamma_row.as_ref(), &mut done);
        }
    }
    write_window(passive, adapter, &dst, &dwin, dpitch, &mut dbuf)
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
