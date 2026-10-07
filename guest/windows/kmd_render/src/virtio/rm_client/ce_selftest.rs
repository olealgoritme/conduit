//! The copy-engine channel's hardware self-test (`RmCopyEngine` = 2, milestone M3b). The rules
//! (geometry, pattern, the two copies, the verdict words) are `helios_kmd_logic::rm_ce_channel::
//! selftest`; the procedure and the expected counters are in `docs/rm-copy-engine-present.md`
//! 11.10. This file performs it, through `ce_channel.rs`.
//!
//! ONE per transport generation, from the HPD worker at PASSIVE ([`super::ce_channel::service`]),
//! never inside a DDI, with the channel's `IO_BUSY` held:
//!
//! 1. bring the channel up ([`ce::ensure_up`]);
//! 2. a source and a destination of 1600x900x4 bytes in the KMD client's own RM system memory
//!    (`NV01_MEMORY_SYSTEM`, cached by default, `RmCeCache` 1 write-combined), each with a CPU view
//!    through the RM window and a GPU mapping in the channel's VA space;
//! 3. the source filled with a position-dependent, salted pattern;
//! 4. the **ready** copy: the producer value set first, then the push; kick to completion seen is
//!    `CeSelfUs`; the destination compared word for word;
//! 5. the **wait** copy: the push acquires a value the producer does not have yet; after
//!    `HOLD_MS` the completion must NOT have landed (the GPU waits), then the KMD sets the
//!    producer; producer set to completion seen is `CeSelfWaitUs`; the destination (the source one
//!    word further on) compared word for word;
//! 6. everything freed: the buffers here (after the GPU is idle), the channel by the caller.
//!
//! Every wait is bounded (the copies by `COPY_WAIT_MS`, the whole by `BUDGET_MS`, each RM message
//! by the RM client's per-message cap), a failure at any stage is counted in `CeSelfTest` /
//! `CeSelfWhy` and is never fatal, and nothing here touches the Present path.

use super::ce_channel::{self as ce, CpuView, GpuMap, Handles, NotUp, SubmitError};
use super::Io;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use core::sync::atomic::Ordering;
use helios_kmd_logic::ce_present as cp;
use helios_kmd_logic::rm_ce_channel::selftest::{self as st, Copy as Which, Stage};
use helios_kmd_logic::rm_ce_channel::{self as cc};
use helios_kmd_logic::rm_client::Fail;
use helios_kmd_logic::sweep_budget::UNITS_PER_MS;

type StageErr = (Stage, Option<Fail>);

/// One self-test buffer: its RM memory, CPU view and GPU mapping, as far as they were made.
#[derive(Clone, Copy, Default)]
struct Buf {
    mem: u32,
    allocated: bool,
    cpu: CpuView,
    gpu: Option<GpuMap>,
}

/// Run the self-test and publish its verdict. The caller holds `IO_BUSY` and tears the channel
/// down afterwards.
#[inline(never)]
pub(super) fn run(passive: PassiveLevel, adapter: &AdapterContext, epoch: u64) {
    let started = ce::now_100ns();
    ce::SELF_PAGES.store(st::PAGES, Ordering::Relaxed);
    match test(passive, adapter, epoch) {
        Ok((us, wait_us)) => {
            ce::SELF_US.store(us, Ordering::Relaxed);
            ce::SELF_WAIT_US.store(wait_us, Ordering::Relaxed);
            ce::SELF_WHY.store(0, Ordering::Relaxed);
            ce::SELF_TEST.store(st::PASS, Ordering::Relaxed);
        }
        Err((stage, f)) => {
            ce::SELF_WHY.store(st::why_word(f), Ordering::Relaxed);
            ce::SELF_TEST.store(st::fail_word(stage), Ordering::Relaxed);
        }
    }
    let ms = (ce::now_100ns().wrapping_sub(started) / UNITS_PER_MS).min(u64::from(u32::MAX)) as u32;
    ce::SELF_MS.store(ms, Ordering::Relaxed);
}

#[inline(never)]
fn test(passive: PassiveLevel, adapter: &AdapterContext, epoch: u64) -> Result<(u32, u32), StageErr> {
    match ce::ensure_up(passive, adapter, epoch) {
        Ok(()) => {}
        Err(NotUp::Failed(_, f)) => return Err((Stage::BringUp, Some(f))),
        Err(NotUp::Refused(_)) => return Err((Stage::BringUp, None)),
    }
    let h = ce::handles().ok_or((Stage::BringUp, None))?;
    let mut src = Buf {
        mem: cc::H_SELF_SRC,
        ..Buf::default()
    };
    let mut dst = Buf {
        mem: cc::H_SELF_DST,
        ..Buf::default()
    };
    let result = {
        // Every wait primitive below obeys the self-test's deadline too; the section ends before
        // the frees, which have their own.
        let _bounded = crate::ddi::escape_wait::begin_bounded(st::BUDGET_MS as u32);
        let io = Io {
            passive,
            adapter,
            epoch,
            limit: Some(ce::budget_ms(st::BUDGET_MS)),
        };
        copies(&io, &h, &mut src, &mut dst)
    };
    // The buffers go on their own allowance (the test's may be what ran out), once the GPU can no
    // longer touch them.
    let idle = gpu_idle(passive);
    let uio = Io {
        passive,
        adapter,
        epoch,
        limit: Some(ce::budget_ms(cc::UNDO_BUDGET_MS)),
    };
    free(&uio, &h, &mut dst, idle);
    free(&uio, &h, &mut src, idle);
    result
}

/// The two copies.
fn copies(io: &Io<'_>, h: &Handles, src: &mut Buf, dst: &mut Buf) -> Result<(u32, u32), StageErr> {
    make(io, h, src, cc::H_SELF_SRC_VIRT, cc::VA_SELF_SRC, st::SRC_BYTES)
        .map_err(|f| (Stage::Source, Some(f)))?;
    make(io, h, dst, cc::H_SELF_DST_VIRT, cc::VA_SELF_DST, st::DST_BYTES)
        .map_err(|f| (Stage::Destination, Some(f)))?;
    let (Some(sg), Some(dg)) = (src.gpu, dst.gpu) else {
        return Err((Stage::Source, None));
    };
    let producer_va = ce::producer_va().ok_or((Stage::ChannelError, None))?;
    let salt = (ce::now_100ns() as u32) | 1;

    // The source: COPY_WORDS + 1 words (the wait copy reads one word further on).
    in_time(io)?;
    for i in 0..=st::COPY_WORDS {
        // SAFETY: word `i` is inside the source's view (`SRC_BYTES` >= `COPY_BYTES + 4`), mapped
        // until `free`.
        unsafe { ce::wr32(src.cpu.va, 4 * u64::from(i), st::pattern(i, salt)) };
    }
    ce::full_barrier();

    // 1. ready: the producer has the value before the kick.
    in_time(io)?;
    if !ce::set_producer(st::PRODUCER_READY) {
        return Err((Stage::ChannelError, None));
    }
    let kicked = ce::now_100ns();
    let v1 = submit(producer_va, st::PRODUCER_READY, Which::Ready, sg.va, dg.va)?;
    let done = wait_for(io.passive, v1, Stage::ReadyTimeout)?;
    let us = st::us_between(kicked, done);
    verify(Which::Ready, salt, dst.cpu.va).map_err(|_| (Stage::ReadyBad, None))?;

    // 2. wait: the push waits on a value the producer does not have yet.
    in_time(io)?;
    let v2 = submit(producer_va, st::PRODUCER_WAIT, Which::Wait, sg.va, dg.va)?;
    crate::virtio::ctrl::sleep_ms(io.passive, st::HOLD_MS);
    let held = ce::poll().ok_or((Stage::ChannelError, None))?;
    if held.notifier != 0 {
        return Err((Stage::ChannelError, None));
    }
    if held.completed >= v2 {
        return Err((Stage::NotHeld, None));
    }
    let released = ce::now_100ns();
    if !ce::set_producer(st::PRODUCER_WAIT) {
        return Err((Stage::ChannelError, None));
    }
    let done = wait_for(io.passive, v2, Stage::WaitTimeout)?;
    let wait_us = st::us_between(released, done);
    verify(Which::Wait, salt, dst.cpu.va).map_err(|_| (Stage::WaitBad, None))?;
    Ok((us, wait_us))
}

fn in_time(io: &Io<'_>) -> Result<(), StageErr> {
    if io.limit_spent() || io.stopping() {
        Err((Stage::NoTime, None))
    } else {
        Ok(())
    }
}

/// RM system memory, its CPU view, its GPU mapping at the fixed `va`; what was made is recorded in
/// `b` for [`free`].
fn make(io: &Io<'_>, h: &Handles, b: &mut Buf, virt: u32, va: u64, size: u64) -> Result<(), Fail> {
    ce::alloc_sys(io, h, b.mem, size).inspect_err(|_| ce::note_rm_error())?;
    b.allocated = true;
    b.cpu = ce::cpu_map(io, h, rc_device(), b.mem, ce::SYSMEM, size, ce::sysmem_view_cache())
        .inspect_err(|_| ce::note_rm_error())?;
    b.gpu = Some(ce::gpu_map(io, h, virt, b.mem, va, size).inspect_err(|_| ce::note_rm_error())?);
    Ok(())
}

const fn rc_device() -> u32 {
    helios_kmd_logic::rm_client::H_DEVICE
}

fn submit(producer_va: u64, value: u64, which: Which, src_va: u64, dst_va: u64) -> Result<u64, StageErr> {
    let acquire = cp::Acquire {
        va: producer_va,
        value,
    };
    ce::submit(acquire, &st::copy_rect(which, src_va, dst_va)).map_err(|e| match e {
        SubmitError::NoChannel => (Stage::ChannelError, None),
        SubmitError::RingFull => (Stage::RingFull, None),
        SubmitError::Push(_) | SubmitError::Entry => (Stage::Push, None),
    })
}

/// Poll the completion until it reaches `value` (spinning first, then in ticks) or
/// `COPY_WAIT_MS` passed: the time it was seen. A set error notifier ends it.
fn wait_for(passive: PassiveLevel, value: u64, timeout: Stage) -> Result<u64, StageErr> {
    let start = ce::now_100ns();
    let deadline = start + st::COPY_WAIT_MS * UNITS_PER_MS;
    loop {
        let p = ce::poll().ok_or((Stage::ChannelError, None))?;
        let now = ce::now_100ns();
        if p.notifier != 0 {
            return Err((Stage::ChannelError, None));
        }
        if p.completed >= value {
            return Ok(now);
        }
        if now >= deadline {
            return Err((timeout, None));
        }
        if now < start + ce::SPIN_100NS {
            core::hint::spin_loop();
        } else {
            crate::virtio::ctrl::sleep_ms(passive, 1);
        }
    }
}

fn verify(which: Which, salt: u32, va: u64) -> Result<(), (u32, u32)> {
    st::verify(which, salt, st::COPY_WORDS, |i| {
        // SAFETY: word `i < COPY_WORDS` is inside the destination's view (`DST_BYTES` >=
        // `COPY_BYTES`), mapped until `free`.
        unsafe { ce::rd32(va, 4 * u64::from(i)) }
    })
}

/// Whether the GPU is done with everything submitted: if not, every acquire is released (the
/// producer far ahead) and it gets `IDLE_WAIT_MS`.
fn gpu_idle(passive: PassiveLevel) -> bool {
    let Some(p) = ce::poll() else {
        return true;
    };
    if p.completed >= p.submitted {
        return true;
    }
    if !ce::set_producer(1u64 << 62) {
        return false;
    }
    let deadline = ce::now_100ns() + cc::IDLE_WAIT_MS * UNITS_PER_MS;
    loop {
        match ce::poll() {
            Some(q) if q.completed >= q.submitted => return true,
            Some(_) if ce::now_100ns() < deadline => crate::virtio::ctrl::sleep_ms(passive, 1),
            _ => return false,
        }
    }
}

/// Give a buffer back in reverse: the GPU mapping and the memory only when the GPU is idle (else
/// they go with the channel's teardown: the TSG first, then the client), the CPU view always.
fn free(io: &Io<'_>, h: &Handles, b: &mut Buf, idle: bool) {
    let mut ok = true;
    if let Some(g) = b.gpu.take() {
        if idle {
            ok &= ce::gpu_unmap(io, h, &g);
        } else {
            ok = false;
        }
    }
    ok &= ce::cpu_unmap(io, h, &mut b.cpu, true);
    if b.allocated && idle {
        ok &= ce::rm_free(io, h, rc_device(), b.mem).is_ok();
        b.allocated = false;
    }
    if !ok {
        ce::note_soft();
    }
}
