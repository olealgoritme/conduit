//! The Vulkan ICD's node-3 copy context (`helios_kmd_logic::nv_copy`).
//!
//! Its `SubmitCommand`s carry the work in the private data: GPFIFO entries of
//! copy-engine pushbuffer segments (pitch copies from the ICD's staging heap
//! into descriptor heaps and the shader code heap, and the semaphore releases
//! the ICD waits for). NVIDIA's KMD runs them on its own copy channel. Here
//! `SubmitCommandVirtual` (DISPATCH) queues them as a job ([`admit`]) and the
//! HPD worker (PASSIVE, [`service`]) runs them on the CPU through
//! `native_memory::patch_gpu_bytes`, which reads and writes exactly the pages
//! the process's GPU VAs map (VidMm's system pages or the RM memory). The
//! submission's DMA fence waits for its job: [`admit`]'s value goes into the
//! WDDM pending entry's GDI gate, tagged [`GATE_TAG`] ([`gate_ready`]).
//!
//! Counters (`NvCc*`): `NvCcAdm` jobs admitted, `NvCcRun` run, `NvCcCopy` /
//! `NvCcBytes` copies and bytes, `NvCcRel` semaphore releases, `NvCcUns` /
//! `NvCcUnsV` launches not implemented and the last one's LAUNCH_DMA value,
//! `NvCcIgn` methods ignored, `NvCcTrunc` segments cut short, `NvCcFail` /
//! `NvCcFailVa` failed reads or writes and the last VA (>> 8), `NvCcNoRec`
//! NVDA-context submissions without a record, `NvCcFull` jobs refused (table
//! full; their fence does not wait).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::nv_copy as logic;

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;

/// Marks a WDDM pending entry's GDI gate as a copy-context job sequence.
pub(crate) const GATE_TAG: u64 = 1 << 63;

/// Jobs waiting for the worker at once.
const SLOTS: usize = 64;
/// Jobs one worker pass runs before it yields (the worker is woken again).
const JOBS_PER_PASS: u32 = 16;
/// Largest line and line count a copy may ask for.
const MAX_LINE_BYTES: u32 = 16 << 20;
const MAX_LINES: u32 = 1 << 16;

#[derive(Clone, Copy)]
struct Job {
    seq: u64,
    process: usize,
    submission: logic::Submission,
}

static JOBS: SpinLock<[Option<Job>; SLOTS]> = SpinLock::new([None; SLOTS]);
static NEXT: AtomicU64 = AtomicU64::new(1);
static COMPLETED: AtomicU64 = AtomicU64::new(0);

static ADMITTED: AtomicU32 = AtomicU32::new(0);
static RUN: AtomicU32 = AtomicU32::new(0);
static COPIES: AtomicU32 = AtomicU32::new(0);
static BYTES: AtomicU32 = AtomicU32::new(0);
static RELEASES: AtomicU32 = AtomicU32::new(0);
static UNSUPPORTED: AtomicU32 = AtomicU32::new(0);
static UNSUPPORTED_VALUE: AtomicU32 = AtomicU32::new(0);
static IGNORED: AtomicU32 = AtomicU32::new(0);
static TRUNCATED: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
static FAILED_VA: AtomicU32 = AtomicU32::new(0);
static NO_RECORD: AtomicU32 = AtomicU32::new(0);
static FULL: AtomicU32 = AtomicU32::new(0);

/// Whether a WDDM pending entry's GDI gate is satisfied: a copy-context job
/// ([`GATE_TAG`]) once the worker has run it, any other value is the GDI
/// executor's sequence.
pub(crate) fn gate_ready(gate: u64) -> bool {
    if gate & GATE_TAG != 0 {
        COMPLETED.load(Ordering::Acquire) >= gate & !GATE_TAG
    } else {
        crate::ddi::gdi_exec::seq_ready(gate)
    }
}

/// `SubmitCommandVirtual` (DISPATCH): a submission on an NVDA context without
/// native channel state that carries the ICD's copy record becomes a job.
/// `Some(gate)` makes its DMA fence wait for the job; `None` leaves the
/// submission as it was (not such a submission, or no free slot).
///
/// # Safety
/// `submit` is dxgkrnl's live argument block for this call.
pub(crate) unsafe fn admit(
    adapter: &AdapterContext,
    submit: &DXGKARG_SUBMITCOMMANDVIRTUAL,
) -> Option<u64> {
    // SAFETY: dxgkrnl passes the live hContext this driver returned.
    let context = unsafe { crate::device::ContextHandleRef::from_raw(submit.hContext) }?;
    if !context.nvda() || context.native().is_some() {
        return None;
    }
    let process = context.creator_process()?;
    if submit.pDmaBufferPrivateData.is_null() || submit.DmaBufferPrivateDataSize == 0 {
        return None;
    }
    // SAFETY: dxgkrnl's private data, DmaBufferPrivateDataSize bytes, for this call.
    let private = unsafe {
        core::slice::from_raw_parts(
            submit.pDmaBufferPrivateData as *const u8,
            submit.DmaBufferPrivateDataSize as usize,
        )
    };
    let Some(submission) = logic::find_submission(private, submit.DmaBufferSize) else {
        NO_RECORD.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if submission.count == 0 {
        return None;
    }
    let seq = {
        let mut jobs = JOBS.lock();
        match jobs.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                let seq = NEXT.fetch_add(1, Ordering::Relaxed);
                *slot = Some(Job {
                    seq,
                    process,
                    submission,
                });
                Some(seq)
            }
            None => None,
        }
    };
    let Some(seq) = seq else {
        FULL.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    ADMITTED.fetch_add(1, Ordering::Relaxed);
    adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
    Some(seq | GATE_TAG)
}

/// The HPD worker (PASSIVE): run the admitted jobs in order, each fence's DPC
/// asked for as its job completes.
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    if ADMITTED.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut ran = 0u32;
    loop {
        let want = COMPLETED.load(Ordering::Acquire) + 1;
        let job = {
            let mut jobs = JOBS.lock();
            jobs.iter_mut()
                .find(|slot| matches!(slot, Some(job) if job.seq == want))
                .and_then(|slot| slot.take())
        };
        let Some(job) = job else {
            break;
        };
        run(passive, adapter, &job);
        COMPLETED.store(job.seq, Ordering::Release);
        RUN.fetch_add(1, Ordering::Relaxed);
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
        ran += 1;
        if ran >= JOBS_PER_PASS {
            adapter.signal_hpd_for(helios_kmd_logic::hpd_wake::cause::OTHER);
            break;
        }
    }
    if ran > 0 {
        publish();
    }
}

fn publish() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvCcAdm", r(&ADMITTED));
    crate::diag::record_named_bytes(b"NvCcRun", r(&RUN));
    crate::diag::record_named_bytes(b"NvCcCopy", r(&COPIES));
    crate::diag::record_named_bytes(b"NvCcBytes", r(&BYTES));
    crate::diag::record_named_bytes(b"NvCcRel", r(&RELEASES));
    crate::diag::record_named_bytes(b"NvCcUns", r(&UNSUPPORTED));
    crate::diag::record_named_bytes(b"NvCcUnsV", r(&UNSUPPORTED_VALUE));
    crate::diag::record_named_bytes(b"NvCcIgn", r(&IGNORED));
    crate::diag::record_named_bytes(b"NvCcTrunc", r(&TRUNCATED));
    crate::diag::record_named_bytes(b"NvCcFail", r(&FAILED));
    crate::diag::record_named_bytes(b"NvCcFailVa", r(&FAILED_VA));
    crate::diag::record_named_bytes(b"NvCcNoRec", r(&NO_RECORD));
    crate::diag::record_named_bytes(b"NvCcFull", r(&FULL));
}

fn fail(va: u64) {
    FAILED.fetch_add(1, Ordering::Relaxed);
    FAILED_VA.store((va >> 8) as u32, Ordering::Relaxed);
}

fn run(passive: PassiveLevel, adapter: &AdapterContext, job: &Job) {
    let mut buf = Vec::new();
    if buf.try_reserve_exact(0x1000).is_err() {
        fail(0);
        return;
    }
    buf.resize(0x1000, 0u8);
    let mut words: Vec<u32> = Vec::new();
    for entry in job.submission.entries() {
        words.clear();
        if words.try_reserve(entry.dwords as usize).is_err() {
            fail(entry.va);
            continue;
        }
        if !read_dwords(
            passive,
            adapter,
            job.process,
            entry.va,
            entry.dwords,
            &mut words,
        ) {
            fail(entry.va);
            continue;
        }
        let stats = logic::decode(&words, |op| {
            execute(passive, adapter, job.process, op, &mut buf)
        });
        IGNORED.fetch_add(stats.ignored, Ordering::Relaxed);
        if stats.truncated {
            TRUNCATED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `dwords` method dwords at `va` of `process`, as the GPU sees them.
fn read_dwords(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    dwords: u32,
    out: &mut Vec<u32>,
) -> bool {
    let total = u64::from(dwords) * 4;
    let mut done = 0u64;
    while done < total {
        let at = va + done;
        let n = (total - done).min(0x1000 - (at & 0xfff));
        let mut read = false;
        let result = crate::virtio::native_memory::patch_gpu_bytes(
            Some(passive),
            adapter,
            process,
            at,
            n,
            |p| {
                for i in 0..(n / 4) as usize {
                    // SAFETY: `p` is valid for the `n` bytes asked for.
                    out.push(unsafe { core::ptr::read_unaligned((p as *const u32).add(i)) });
                }
                read = true;
            },
        );
        if !matches!(result, Ok(true)) || !read {
            return false;
        }
        done += n;
    }
    true
}

fn execute(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    op: logic::Op,
    buf: &mut [u8],
) {
    match op {
        logic::Op::Copy {
            src,
            dst,
            line_len,
            line_count,
            pitch_in,
            pitch_out,
        } => {
            if line_len > MAX_LINE_BYTES || line_count > MAX_LINES {
                UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            for line in 0..u64::from(line_count) {
                let from = src.wrapping_add(line * u64::from(pitch_in));
                let to = dst.wrapping_add(line * u64::from(pitch_out));
                if copy_bytes(
                    passive,
                    adapter,
                    process,
                    from,
                    to,
                    u64::from(line_len),
                    buf,
                ) {
                    BYTES.fetch_add(line_len, Ordering::Relaxed);
                }
            }
            COPIES.fetch_add(1, Ordering::Relaxed);
        }
        logic::Op::Release {
            va,
            payload,
            wide,
            four_word,
        } => {
            // A four-word release is the payload (zero-extended to 8 bytes)
            // and a 64-bit timestamp; this executor writes 0 for the time.
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&payload.to_le_bytes());
            let len = if four_word {
                16
            } else if wide {
                8
            } else {
                4
            };
            if write_bytes(passive, adapter, process, va, &bytes[..len]) {
                RELEASES.fetch_add(1, Ordering::Relaxed);
            } else {
                fail(va);
            }
        }
        logic::Op::Unsupported(value) => {
            UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
            UNSUPPORTED_VALUE.store(value, Ordering::Relaxed);
        }
    }
}

/// Copy `len` bytes from `src` to `dst` of `process`, page piece by page piece
/// through `buf` (one page).
fn copy_bytes(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    src: u64,
    dst: u64,
    len: u64,
    buf: &mut [u8],
) -> bool {
    let mut done = 0u64;
    while done < len {
        let from = src + done;
        let to = dst + done;
        let n = (len - done)
            .min(0x1000 - (from & 0xfff))
            .min(0x1000 - (to & 0xfff))
            .min(buf.len() as u64);
        let chunk = &mut buf[..n as usize];
        let mut read = false;
        let result = crate::virtio::native_memory::patch_gpu_bytes(
            Some(passive),
            adapter,
            process,
            from,
            n,
            |p| {
                // SAFETY: `p` is valid for the `n` bytes asked for; `chunk` holds `n`.
                unsafe {
                    core::ptr::copy_nonoverlapping(p as *const u8, chunk.as_mut_ptr(), n as usize)
                };
                read = true;
            },
        );
        if !matches!(result, Ok(true)) || !read {
            fail(from);
            return false;
        }
        if !write_bytes(passive, adapter, process, to, &buf[..n as usize]) {
            fail(to);
            return false;
        }
        done += n;
    }
    true
}

/// Write `bytes` at `va` of `process` as the GPU sees it, split at pages.
fn write_bytes(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    process: usize,
    va: u64,
    bytes: &[u8],
) -> bool {
    let mut done = 0usize;
    while done < bytes.len() {
        let at = va + done as u64;
        let n = (bytes.len() - done).min((0x1000 - (at & 0xfff)) as usize);
        let piece = &bytes[done..done + n];
        let mut written = false;
        let result = crate::virtio::native_memory::patch_gpu_bytes(
            Some(passive),
            adapter,
            process,
            at,
            n as u64,
            |p| {
                // SAFETY: `p` is valid for the `n` bytes asked for.
                unsafe { core::ptr::copy_nonoverlapping(piece.as_ptr(), p, n) };
                written = true;
            },
        );
        if !matches!(result, Ok(true)) || !written {
            return false;
        }
        done += n;
    }
    true
}
