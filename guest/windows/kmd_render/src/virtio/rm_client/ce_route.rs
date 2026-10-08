//! The copy-engine Present route's RM I/O (`RmCopyEngine` = 1, M3c-2): a CHILD of `rm_client`, as
//! `ce_channel.rs` is, so it drives the channel's own client (`Io`, `ce_channel::Handles`). The
//! route itself (decision, dispatch, completion, counters, the destination table) is
//! `ddi/ce_present_route.rs`; the pure rules are `helios_kmd_logic::ce_route`; the design is
//! `docs/rm-copy-engine-present.md` section 15.
//!
//! What is here, every call PASSIVE and bounded:
//!
//! * [`bring_up`]: the channel's lazy bring-up for the route (`ce_channel::ensure_up`), from the
//!   HPD worker only, never inside a DDI.
//! * [`prep_producer`]: the producer's dup and GPU mapping (`ce_dup::dup_map_record`).
//! * [`create_dst`] / [`free_dst`]: the destination's `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over the
//!   page runs of its lease pages (the KMD-owned registration, `nvrm::forward_kmd_registration`,
//!   the same page-run block a user PIN carries), GPU-mapped snooped in the channel's VA space,
//!   and its release.
//! * [`submit`], [`poll`], [`chan_view`], [`mark_broken`]: thin doors to the channel (spinlock
//!   only, any IRQL up to DISPATCH).
//! * [`release_producers`], [`teardown_channel`]: the channel's teardown for the route.
//!
//! `IO_BUSY` (`ce_channel::try_io`) makes the channel's RM I/O one thread's at a time. The worker
//! never waits for it; the paging path waits at most `ce_route::IO_WAIT_MS` and then leaves the
//! descriptor to the channel's teardown (its pages stay pinned until then). Lock order: the
//! content transaction (when held) -> `IO_BUSY` -> the virtio lock (inside each message).

use super::ce_channel::{self as ch, GpuMap, Handles};
use super::{ce_dup, fail_of, Io, REPLY_MAX};
use crate::adapter::AdapterContext;
use crate::device::StashedCeRecord;
use crate::irql::PassiveLevel;
use crate::virtio::nvrm::{self, Refusal};
use crate::virtio::VirtioError;
use helios_kmd_logic::ce_present as cp;
use helios_kmd_logic::ce_route as cr;
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind};
use helios_kmd_logic::sweep_budget::SweepBudget;

pub(crate) use super::ce_dup::Producer;

/// Another thread has the channel's RM I/O (try again later; not a strike).
pub(crate) const BUSY: Fail = Fail::new(FailKind::Transport, 0xE4);
/// No channel up.
pub(crate) const NO_CHANNEL: Fail = Fail::new(FailKind::Transport, 0xE5);

/// Whether `f` only says "not now" ([`BUSY`]).
pub(crate) fn is_busy(f: &Fail) -> bool {
    f.kind == BUSY.kind && f.code == BUSY.code
}

/// What the route needs to know about the channel.
#[derive(Clone, Copy)]
pub(crate) struct ChanView {
    /// Up: submissions go.
    pub up: bool,
    /// The channel's service struck out for the generation.
    pub disabled: bool,
    /// The channel failed while up (its error notifier, or a copy of the route that never
    /// completed): nothing is submitted until it is torn down.
    pub broken: bool,
    /// Cold, or its cool-down over: a bring-up may be asked for.
    pub may_bring_up: bool,
    pub gen: Option<cp::Gen>,
}

/// The channel's state. Spinlock only.
pub(crate) fn chan_view() -> ChanView {
    let (phase, gen) = ch::route_view();
    ChanView {
        up: phase == cc::Phase::Ready && gen.is_some(),
        disabled: phase == cc::Phase::Disabled,
        broken: phase == cc::Phase::Broken,
        may_bring_up: matches!(phase, cc::Phase::Cold | cc::Phase::CoolDown),
        gen,
    }
}

fn epoch(adapter: &AdapterContext) -> Option<u64> {
    adapter
        .with_virtio(|v| v.nvrm_epoch())
        .ok()
        .filter(|e| *e != 0)
}

fn io<'a>(passive: PassiveLevel, adapter: &'a AdapterContext, epoch: u64, ms: u64) -> Io<'a> {
    Io {
        passive,
        adapter,
        epoch,
        limit: Some(ch::budget_ms(ms)),
    }
}

/// Bring the channel up for the route if it is cold (or its cool-down is over). From the HPD
/// worker only (PASSIVE, no lock held): `ce_channel::ensure_up` runs on its own 6 s budget inside
/// a bounded section. Whether it is up afterwards. Never waits for `IO_BUSY`.
#[inline(never)]
pub(crate) fn bring_up(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    let Some(epoch) = epoch(adapter) else {
        return false;
    };
    if adapter
        .hpd_stop
        .load(core::sync::atomic::Ordering::Acquire)
        != 0
    {
        return false;
    }
    if !ch::try_io() {
        return false;
    }
    let up = ch::ensure_up(passive, adapter, epoch).is_ok();
    ch::end_io();
    up
}

/// The producer's semaphore entry and image base in the channel's VA space (`ce_dup`, cached per
/// `(h_client, h_memory)` there). PASSIVE, no lock held; [`BUSY`] when another thread has the
/// channel's I/O. At most `ce_route::PREP_MS`.
#[inline(never)]
pub(crate) fn prep_producer(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    rec: &StashedCeRecord,
) -> Result<Producer, Fail> {
    let view = chan_view();
    let (true, Some(gen), Some(h), Some(epoch)) = (view.up, view.gen, ch::handles(), epoch(adapter))
    else {
        return Err(NO_CHANNEL);
    };
    if !ch::try_io() {
        return Err(BUSY);
    }
    let r = {
        let _bounded = crate::ddi::escape_wait::begin_bounded(cr::PREP_MS);
        let io = io(passive, adapter, epoch, u64::from(cr::PREP_MS));
        ce_dup::dup_map_record(&io, &h, rec, gen)
    };
    ch::end_io();
    r
}

/// The record's producer VAs when both dups are cached under the client table's current
/// generation (no RM call; `ce_dup::cached_producer`). The caller holds the channel's I/O
/// ([`try_io`]). Spinlock only.
pub(crate) fn cached_producer(rec: &StashedCeRecord) -> Option<Producer> {
    let gen = chan_view().gen?;
    ce_dup::cached_producer(rec, gen, crate::virtio::nvrm_harden::client_generation())
}

/// Take the channel's RM I/O without waiting (`ce_channel::try_io`): while held, no dup is made,
/// remade or given back. `false`: another thread has it.
pub(crate) fn try_io() -> bool {
    ch::try_io()
}

/// Give back what [`try_io`] took.
pub(crate) fn end_io() {
    ch::end_io();
}

/// Why [`create_dst`] made no descriptor.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DstFail {
    /// Nothing is left on the host: the pages may be unpinned.
    Clean(Fail),
    /// The host may hold the registration (a timeout, or a free after a failed map that was not
    /// confirmed): the pages must stay pinned until the channel's client (or the transport) goes.
    Unsure(Fail),
}

/// Register `pfns` (the destination's `[0, cover)`, whole pages in allocation order) as an
/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` of slot `slot` in the channel's client and GPU-map it
/// snooped with 4 KiB pages at the slot's window (RM's choice of VA if the fixed range is
/// refused, below 2^40 checked). The caller holds the content transaction and the pin of the
/// leases `pfns` came from, and keeps the pin for as long as the descriptor may exist. PASSIVE,
/// at most `ce_route::PREP_MS`. Returns the GPU VA.
#[inline(never)]
pub(crate) fn create_dst(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    slot: u8,
    pfns: &[u64],
    cover: u64,
) -> Result<u64, DstFail> {
    let (Some(h), Some(epoch)) = (ch::handles(), epoch(adapter)) else {
        return Err(DstFail::Clean(NO_CHANNEL));
    };
    if !chan_view().up {
        return Err(DstFail::Clean(NO_CHANNEL));
    }
    if pfns.is_empty() || pfns.len() as u64 * 4096 != cover || !cr::fits_window(cover) {
        return Err(DstFail::Clean(Fail::new(FailKind::Layout, 0x70)));
    }
    if !ch::try_io() {
        return Err(DstFail::Clean(BUSY));
    }
    let r = {
        let _bounded = crate::ddi::escape_wait::begin_bounded(cr::PREP_MS);
        let io = io(passive, adapter, epoch, u64::from(cr::PREP_MS));
        create_dst_io(&io, &h, slot, pfns, cover)
    };
    ch::end_io();
    r
}

fn create_dst_io(
    io: &Io<'_>,
    h: &Handles,
    slot: u8,
    pfns: &[u64],
    cover: u64,
) -> Result<u64, DstFail> {
    create_osdesc_io(io, h, cr::dst_handles(slot), cr::dst_va(slot), pfns, cover)
}

/// [`create_dst`]'s registration and mapping with the caller's handles and window (`ce_sysmem`:
/// the CE view of a CPU-visible standard buffer's system pages). The caller holds the channel's
/// I/O, the content transaction and the pin of the leases `pfns` came from.
pub(crate) fn create_osdesc_io(
    io: &Io<'_>,
    h: &Handles,
    handles: (u32, u32),
    va: u64,
    pfns: &[u64],
    cover: u64,
) -> Result<u64, DstFail> {
    if pfns.is_empty() || pfns.len() as u64 * 4096 != cover || !cr::fits_window(cover) {
        return Err(DstFail::Clean(Fail::new(FailKind::Layout, 0x70)));
    }
    let (h_mem, h_virt) = handles;
    let (kind, deep, big) = nvrm::page_run_table(io.passive, pfns)
        .map_err(|_| DstFail::Clean(Fail::new(FailKind::Os, 0x71)))?;
    // `pMemory` is only logged by the host (it substitutes its alias of the runs): the first
    // page's guest-physical address says which registration a host log line is.
    let block = cr::nvos02_osdesc(h.root, rc::H_DEVICE, h_mem, pfns[0].wrapping_mul(4096), cover);
    let mut req = [0u8; rc::MSG_HDR + rc::IOCTL_REQ + cr::NVOS02_FD_BYTES];
    let cmd = rc::nv_cmd(cr::ESC_RM_ALLOC_MEMORY, cr::NVOS02_FD_BYTES as u32);
    // The registration goes on the client's GPU file, as librmclient's `crm_alloc_os_descriptor`.
    let n = rc::build_ioctl(&mut req, h.gpu, cmd, &block, &[])
        .ok_or(DstFail::Clean(Fail::new(FailKind::Parse, 0x72)))?;
    let Some(timeout_ms) = io.message_timeout_ms() else {
        return Err(DstFail::Clean(Fail::new(FailKind::Transport, 0xE1)));
    };
    let mut resp = [0u8; REPLY_MAX];
    let sent = nvrm::forward_kmd_registration(
        io.passive,
        io.adapter,
        req.get(..n).unwrap_or(&[]),
        kind,
        &deep,
        &mut resp,
        timeout_ms,
    );
    let reply = match sent {
        Ok(len) => resp.get(..len).unwrap_or(&[]),
        Err(Refusal::Transport(VirtioError::Timeout)) => {
            // The host may still read an indirect table and register the pages: neither the
            // table nor the pages may go (the caller keeps the pin).
            core::mem::forget(big);
            return Err(DstFail::Unsure(fail_of(Refusal::Transport(VirtioError::Timeout))));
        }
        Err(r) => return Err(DstFail::Clean(fail_of(r))),
    };
    // Answered: the host has copied the table out (an indirect one is read whole before use).
    drop(big);
    if let Err(e) = rc::rm_reply(reply, cr::NVOS02_STATUS_AT) {
        return Err(DstFail::Clean(Fail::from(e)));
    }
    match ch::gpu_map(io, h, h_virt, h_mem, va, cover) {
        Ok(g) => Ok(g.va),
        Err(f) => {
            // The descriptor exists: free it, or the pages stay pinned with it.
            if ch::rm_free(io, h, rc::H_DEVICE, h_mem).is_ok() {
                Err(DstFail::Clean(f))
            } else {
                Err(DstFail::Unsure(f))
            }
        }
    }
}

/// Unmap and free slot `slot`'s descriptor (`va`, `len` as [`create_dst`] made it). Whether both
/// were confirmed (only then may the pages be unpinned). Waits at most `wait_io_ms` for the
/// channel's I/O (0: the worker, which never waits); each message within `limit_ms`. PASSIVE.
#[inline(never)]
pub(crate) fn free_dst(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    slot: u8,
    va: u64,
    len: u64,
    wait_io_ms: u64,
    limit_ms: u32,
) -> bool {
    let (Some(h), Some(epoch)) = (ch::handles(), epoch(adapter)) else {
        return false;
    };
    if !take_io(passive, wait_io_ms) {
        return false;
    }
    let ok = {
        let _bounded = crate::ddi::escape_wait::begin_bounded(limit_ms);
        let io = io(passive, adapter, epoch, u64::from(limit_ms));
        let (h_mem, h_virt) = cr::dst_handles(slot);
        let g = GpuMap {
            virt: h_virt,
            mem: h_mem,
            va,
            len,
        };
        let unmapped = ch::gpu_unmap(&io, &h, &g);
        let freed = unmapped && ch::rm_free(&io, &h, rc::H_DEVICE, h_mem).is_ok();
        unmapped && freed
    };
    ch::end_io();
    ok
}

/// Unmap and free an OS descriptor [`create_osdesc_io`] made. Whether both were confirmed (only
/// then may its pages be unpinned). The caller holds the channel's I/O.
pub(crate) fn free_osdesc_io(io: &Io<'_>, h: &Handles, handles: (u32, u32), va: u64, len: u64) -> bool {
    let (h_mem, h_virt) = handles;
    let g = GpuMap {
        virt: h_virt,
        mem: h_mem,
        va,
        len,
    };
    let unmapped = ch::gpu_unmap(io, h, &g);
    unmapped && ch::rm_free(io, h, rc::H_DEVICE, h_mem).is_ok()
}

/// Run `f` with the channel's I/O (waiting at most `wait_io_ms` for it) and an `Io` on the
/// channel's client bounded by `limit_ms`. `None`: no channel, or the I/O was not free in time.
pub(crate) fn with_channel_io<T>(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    wait_io_ms: u64,
    limit_ms: u32,
    f: impl FnOnce(&Io<'_>, &Handles) -> T,
) -> Option<T> {
    let (Some(h), Some(epoch)) = (ch::handles(), epoch(adapter)) else {
        return None;
    };
    if !take_io(passive, wait_io_ms) {
        return None;
    }
    let r = {
        let _bounded = crate::ddi::escape_wait::begin_bounded(limit_ms);
        let io = io(passive, adapter, epoch, u64::from(limit_ms));
        f(&io, &h)
    };
    ch::end_io();
    Some(r)
}

/// `IO_BUSY`, waiting at most `ms` of interrupt time (each sleep rounds up to the timer
/// quantum, so the clock, not a count of sleeps, ends the wait).
fn take_io(passive: PassiveLevel, ms: u64) -> bool {
    let end = cr::deadline(ch::now_100ns(), ms);
    loop {
        if ch::try_io() {
            return true;
        }
        if cr::expired(ch::now_100ns(), end) {
            return false;
        }
        crate::virtio::ctrl::sleep_ms(passive, 1);
    }
}

/// Release every producer dup (the channel's teardown, while its client still answers). Waits at
/// most `wait_io_ms` for the channel's I/O. PASSIVE.
#[inline(never)]
pub(crate) fn release_producers(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    budget: SweepBudget,
    wait_io_ms: u64,
) {
    let (Some(h), Some(epoch)) = (ch::handles(), epoch(adapter)) else {
        return;
    };
    if !take_io(passive, wait_io_ms) {
        return;
    }
    let io = Io {
        passive,
        adapter,
        epoch,
        limit: Some(budget),
    };
    ce_dup::release_all(&io, &h);
    super::ce_vram::release_all(&io, &h);
    ch::end_io();
}

/// Tear the channel down for the route (it broke, or a copy never completed): the producer dups
/// first, then the channel in its own order. The caller freed (or leaked) every destination
/// descriptor before. From the HPD worker (never waits for `IO_BUSY`). PASSIVE.
#[inline(never)]
pub(crate) fn teardown_channel(passive: PassiveLevel, adapter: &AdapterContext) -> bool {
    if !ch::try_io() {
        return false;
    }
    if let (Some(h), Some(epoch)) = (ch::handles(), epoch(adapter)) {
        let io = io(passive, adapter, epoch, cc::UNDO_BUDGET_MS);
        ce_dup::release_all(&io, &h);
    super::ce_vram::release_all(&io, &h);
    }
    ch::teardown(passive, adapter, ch::budget_ms(cc::UNDO_BUDGET_MS));
    ch::end_io();
    true
}

/// Why [`submit`] did not submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitFail {
    NoChannel,
    RingFull,
    /// The push could not be built (a shape the builder refuses) or its entry not encoded.
    Push,
}

/// Submit one copy (`ce_channel::submit`: plain stores under the channel's leaf lock, no RM
/// call, any IRQL up to DISPATCH). The completion value the push releases.
pub(crate) fn submit(producer: cp::Acquire, copy: &cp::CopyRect) -> Result<u64, SubmitFail> {
    ch::submit(producer, copy).map_err(|e| match e {
        ch::SubmitError::NoChannel => SubmitFail::NoChannel,
        ch::SubmitError::RingFull => SubmitFail::RingFull,
        ch::SubmitError::Push(_) | ch::SubmitError::Entry => SubmitFail::Push,
    })
}

/// The channel's completion watermark and its error notifier, read through its kernel views
/// (`ce_channel::poll`; a set notifier breaks the channel). `None`: no channel.
pub(crate) fn poll() -> Option<(u64, u16)> {
    ch::poll().map(|p| (p.completed, p.notifier))
}

/// A copy never completed: no more submissions until the channel is torn down.
pub(crate) fn mark_broken() {
    ch::mark_broken();
}
