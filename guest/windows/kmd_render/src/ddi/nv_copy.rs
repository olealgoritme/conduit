//! The Vulkan ICD's node-3 copy context (`helios_kmd_logic::nv_copy`).
//!
//! Its `SubmitCommand`s carry the work in the private data: GPFIFO entries of
//! copy-engine pushbuffer segments (pitch copies from the ICD's staging heap
//! into descriptor heaps and the shader code heap, and the semaphore releases
//! the ICD waits for). NVIDIA's KMD runs them on a copy channel of its own, and
//! so does this KMD: CreateContext of such a context builds a
//! `native_channel::CopyChannel` in the process's VA space ([`create_channel`]),
//! `SubmitCommandVirtual` (DISPATCH, [`admit`]) appends the entries and the
//! KMD's release of the submission's sequence to its ring and rings the
//! doorbell, and the submission's DMA fence waits until the channel's progress
//! semaphore reaches that sequence: [`admit`]'s value goes into the WDDM
//! pending entry's GDI gate, tagged (`nv_copy::gate`, [`gate_ready`]). The HWS
//! poll (`ddi::hws::poll_tick`, [`poll`]) watches the semaphore while work is
//! outstanding, asks for the completion DPC when it moves, and recovers a
//! faulted or hung channel (`native_hang::Watch`: report, then after the grace
//! the channel is dead and its outstanding sequences complete).
//!
//! `NvCcCpu=1` (test knob, read at CreateContext): when the channel could not be
//! built, is dead or its ring is full, the HPD worker runs the work on the CPU
//! instead ([`service`], through `native_memory::patch_gpu_bytes`).
//!
//! Counters: channel path `NvCcxMade` / `NvCcxMkFail` channels built / not
//! built, `NvCcxSub` submissions, `NvCcxDone` sequences seen complete,
//! `NvCcxBusy` ring full, `NvCcxNone` submissions with no usable channel and no
//! fallback (their work does not run), `NvCcxFault` / `NvCcxHang` reports,
//! `NvCcxForced` channels completed by the KMD; CPU fallback `NvCcAdm` jobs
//! admitted, `NvCcRun` run, `NvCcCopy` / `NvCcBytes` copies and bytes,
//! `NvCcRel` releases, `NvCcUns` / `NvCcUnsV` launches not implemented and the
//! last LAUNCH_DMA value, `NvCcIgn` methods ignored, `NvCcTrunc` segments cut
//! short, `NvCcFail` / `NvCcFailVa` failed accesses and the last VA (>> 8),
//! `NvCcFull` jobs refused (table full); both `NvCcNoRec` NVDA-context
//! submissions without a record.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::native_hang as hang;
use helios_kmd_logic::nv_copy as logic;

use crate::adapter::AdapterContext;
use crate::dxgk::*;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::native_channel::CopyChannel;

/// Jobs waiting for the worker at once.
const SLOTS: usize = 64;
/// Jobs one worker pass runs before it yields (the worker is woken again).
const JOBS_PER_PASS: u32 = 16;
/// Largest line and line count a copy may ask for.
const MAX_LINE_BYTES: u32 = 16 << 20;
const MAX_LINES: u32 = 1 << 16;
/// Copy contexts with a channel at once.
const CHANNELS_MAX: usize = 32;

#[derive(Clone, Copy)]
struct Job {
    seq: u64,
    process: usize,
    submission: logic::Submission,
}

static JOBS: SpinLock<[Option<Job>; SLOTS]> = SpinLock::new([None; SLOTS]);
static NEXT: AtomicU64 = AtomicU64::new(1);
static COMPLETED: AtomicU64 = AtomicU64::new(0);

/// One copy context's channel. `channel` is a leaked `Box<CopyChannel>`, freed
/// by [`destroy_context`] after the slot is emptied under the lock (so nothing
/// holding the lock can see it freed).
#[derive(Clone, Copy)]
struct ChannelSlot {
    context: usize,
    channel: usize,
    submitted: u64,
    seen: u64,
    /// Everything up to this completes without the GPU (a dead channel).
    forced: u64,
    watch: hang::Watch,
}

const NO_CHANNEL: ChannelSlot = ChannelSlot {
    context: 0,
    channel: 0,
    submitted: 0,
    seen: 0,
    forced: 0,
    watch: hang::Watch::new(),
};

static CHANNELS: SpinLock<[ChannelSlot; CHANNELS_MAX]> = SpinLock::new([NO_CHANNEL; CHANNELS_MAX]);
/// `NvCcCpu`: the CPU fallback is enabled.
static CPU_FALLBACK: AtomicU32 = AtomicU32::new(0);

static MADE: AtomicU32 = AtomicU32::new(0);
static MAKE_FAILED: AtomicU32 = AtomicU32::new(0);
static SUBMITTED: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static BUSY: AtomicU32 = AtomicU32::new(0);
static NONE: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
static HANGS: AtomicU32 = AtomicU32::new(0);
static FORCED: AtomicU32 = AtomicU32::new(0);

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

/// Whether a WDDM pending entry's GDI gate is satisfied: a copy-context
/// sequence once its channel's semaphore (or the CPU fallback) reached it, any
/// other value is the GDI executor's sequence. Any IRQL up to DISPATCH.
pub(crate) fn gate_ready(gate: u64) -> bool {
    let Some((slot, seq)) = logic::gate_parts(gate) else {
        return crate::ddi::gdi_exec::seq_ready(gate);
    };
    if slot == logic::GATE_CPU_SLOT {
        return COMPLETED.load(Ordering::Acquire) >= seq;
    }
    let channels = CHANNELS.lock();
    let Some(entry) = channels.get(slot).filter(|entry| entry.channel != 0) else {
        // The context is gone: nothing can complete it any more.
        return true;
    };
    // SAFETY: a slot's channel lives until `destroy_context` emptied the slot
    // under this lock.
    let channel = unsafe { &*(entry.channel as *const CopyChannel) };
    entry.forced >= seq || channel.progress() >= seq
}

/// CreateContext (PASSIVE) of an NVDA context: build the channel for the
/// Vulkan ICD's copy context (`nv_copy::is_copy_context`).
///
/// # Safety
/// `h_device` is the live hDevice of the CreateContext, `h_context` the
/// hContext it returns.
pub(crate) unsafe fn create_channel(h_device: *mut core::ffi::c_void, h_context: usize) {
    let passive = unsafe { PassiveLevel::assume() };
    let knob = crate::diag::read_config_dword(crate::diag::KnobName::new(b"NvCcCpu"), 0);
    CPU_FALLBACK.store(u32::from(knob == 1), Ordering::Relaxed);
    let made = (|| {
        let device = unsafe { crate::device::DeviceHandleRef::from_raw(h_device) }?;
        let adapter = device.adapter()?;
        let owner = crate::virtio::gpu::DeviceOwner::new(h_device as usize)?;
        let process = unsafe { device.native_process_ptr().as_mut() }?;
        CopyChannel::create(passive, adapter, process, owner).ok()
    })();
    let Some(channel) = made else {
        let n = MAKE_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvCcxMkFail", n);
        return;
    };
    let raw = Box::into_raw(Box::new(channel));
    let placed = {
        let mut channels = CHANNELS.lock();
        match channels.iter_mut().find(|entry| entry.channel == 0) {
            Some(entry) => {
                *entry = ChannelSlot {
                    context: h_context,
                    channel: raw as usize,
                    ..NO_CHANNEL
                };
                true
            }
            None => false,
        }
    };
    if !placed {
        // SAFETY: the box made above, never shared.
        let mut channel = unsafe { Box::from_raw(raw) };
        let _ = channel.destroy(passive);
        let n = MAKE_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
        crate::diag::record_named_bytes(b"NvCcxMkFail", n);
        return;
    }
    let n = MADE.fetch_add(1, Ordering::Relaxed) + 1;
    crate::diag::record_named_bytes(b"NvCcxMade", n);
}

/// DestroyContext (PASSIVE): free the context's channel, if it has one.
pub(crate) fn destroy_context(h_context: usize) {
    if h_context == 0 {
        return;
    }
    let taken = {
        let mut channels = CHANNELS.lock();
        channels
            .iter_mut()
            .find(|entry| entry.channel != 0 && entry.context == h_context)
            .map(|entry| core::mem::replace(entry, NO_CHANNEL))
    };
    let Some(entry) = taken else {
        return;
    };
    // SAFETY: DestroyContext is PASSIVE.
    let passive = unsafe { PassiveLevel::assume() };
    // SAFETY: the slot held the only reference and is empty now.
    let mut channel = unsafe { Box::from_raw(entry.channel as *mut CopyChannel) };
    let _ = channel.destroy(passive);
    publish_channel();
}

/// `SubmitCommandVirtual` (DISPATCH): a submission on an NVDA context without
/// native channel state that carries the ICD's copy record. `Some(gate)` makes
/// its DMA fence wait for the work; `None` leaves the submission as it was.
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
    let h_context = submit.hContext as usize;
    // Only the copy context's submissions are touched: a context with a channel,
    // or any NVDA context's record when the CPU fallback is on.
    let has_channel = CHANNELS
        .lock()
        .iter()
        .any(|entry| entry.channel != 0 && entry.context == h_context);
    if !has_channel && CPU_FALLBACK.load(Ordering::Relaxed) == 0 {
        return None;
    }
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
    let on_channel = {
        let mut channels = CHANNELS.lock();
        let found = channels
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.channel != 0 && entry.context == h_context);
        match found {
            Some((slot, entry)) => {
                // SAFETY: as in `gate_ready`.
                let channel = unsafe { &*(entry.channel as *const CopyChannel) };
                let seq = entry.submitted + 1;
                match channel.submit(submission.entries(), seq) {
                    Ok(()) => {
                        entry.submitted = seq;
                        Some(logic::gate(slot, seq))
                    }
                    Err(crate::virtio::native_channel::NativeError::Busy) => {
                        BUSY.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Err(_) => None,
                }
            }
            None => None,
        }
    };
    if let Some(gate) = on_channel {
        SUBMITTED.fetch_add(1, Ordering::Relaxed);
        crate::ddi::hws::kick_poll(adapter);
        return Some(gate);
    }
    if CPU_FALLBACK.load(Ordering::Relaxed) == 0 {
        NONE.fetch_add(1, Ordering::Relaxed);
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
    Some(logic::gate(logic::GATE_CPU_SLOT, seq))
}

/// One HWS poll tick (DISPATCH): `(outstanding, progressed)` over the copy
/// channels. A channel whose semaphore moved asks for the completion DPC; a
/// faulted or hung one is reported, and after the grace marked dead with its
/// outstanding sequences completed.
pub(crate) fn poll(adapter: &AdapterContext, now: u64, hang_timeout: Option<u64>) -> (bool, bool) {
    let mut outstanding = false;
    let mut progressed = false;
    {
        let mut channels = CHANNELS.lock();
        for entry in channels.iter_mut().filter(|entry| entry.channel != 0) {
            // SAFETY: as in `gate_ready`.
            let channel = unsafe { &*(entry.channel as *const CopyChannel) };
            let value = channel.progress();
            if value > entry.seen {
                DONE.fetch_add((value - entry.seen) as u32, Ordering::Relaxed);
                entry.seen = value;
                progressed = true;
            }
            let busy = entry.submitted > entry.seen.max(entry.forced);
            let sample = hang::Sample {
                now,
                outstanding: busy,
                progress: value,
                gp_get: channel.gp_get(),
                error: channel.channel_error(),
            };
            match entry.watch.tick(sample, hang_timeout) {
                hang::Action::None => {}
                hang::Action::Report(cause) => {
                    let counter = match cause {
                        hang::Cause::Fault(_) => &FAULTS,
                        hang::Cause::Hang => &HANGS,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                hang::Action::ForceComplete(_) => {
                    FORCED.fetch_add(1, Ordering::Relaxed);
                    channel.mark_dead();
                    entry.forced = entry.submitted;
                    progressed = true;
                }
            }
            outstanding |= entry.submitted > entry.seen.max(entry.forced) || entry.watch.in_grace();
        }
    }
    if progressed {
        crate::ddi::interrupt::request_wddm_completion_dpc(adapter);
    }
    (outstanding, progressed)
}

fn publish_channel() {
    let r = |c: &AtomicU32| c.load(Ordering::Relaxed);
    crate::diag::record_named_bytes(b"NvCcxMade", r(&MADE));
    crate::diag::record_named_bytes(b"NvCcxMkFail", r(&MAKE_FAILED));
    crate::diag::record_named_bytes(b"NvCcxSub", r(&SUBMITTED));
    crate::diag::record_named_bytes(b"NvCcxDone", r(&DONE));
    crate::diag::record_named_bytes(b"NvCcxBusy", r(&BUSY));
    crate::diag::record_named_bytes(b"NvCcxNone", r(&NONE));
    crate::diag::record_named_bytes(b"NvCcxFault", r(&FAULTS));
    crate::diag::record_named_bytes(b"NvCcxHang", r(&HANGS));
    crate::diag::record_named_bytes(b"NvCcxForced", r(&FORCED));
    crate::diag::record_named_bytes(b"NvCcNoRec", r(&NO_RECORD));
}

/// The HPD worker (PASSIVE): run the CPU fallback's jobs in order, each fence's
/// DPC asked for as its job completes; and mirror the channel counters.
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    // `NvCcx*` / `NvCc*` are written by the registry mirror's pass (`publish_hot`), not
    // by this worker pass (23 deferred writes per pass while the copy context ran).
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
}

/// The copy context's counters (`NvCcx*`, `NvCc*`), from the registry mirror's pass
/// (`ddi::hot_path::publish`). Nothing before the first channel or job. PASSIVE.
pub(crate) fn publish_hot() {
    if MADE.load(Ordering::Relaxed) != 0 || MAKE_FAILED.load(Ordering::Relaxed) != 0 {
        publish_channel();
    }
    if ADMITTED.load(Ordering::Relaxed) != 0 {
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
