//! The paging copy channel (`NvPgGpu`, `helios_kmd_logic::pg_gpu`).
//!
//! One KMD-owned GPFIFO channel on an asynchronous copy engine (its own TSG, the copy
//! engine `rm_ce_channel::pick_engine` prefers), in the shared native-memory RM client and the
//! paging VA space `native_memory::gpu_paging` keeps for it. Built lazily on the first GPU
//! paging operation, in the order of the M1 tool and `rm_client::ce_channel` (the proven
//! async-CE bring-up): control memory and USERD, the usermode doorbell, the channel's block
//! (GPFIFO, semaphore, push slots: `pg_gpu::ring`) in cached RM system memory mapped into the
//! paging space, the TSG, the subcontext, the channel, BIND, the copy object, the work-submit
//! token, the schedule, and a first push whose release must land.
//!
//! Every use runs under [`OpGuard`] (one PASSIVE mutex: paging operations are one at a time,
//! and the paging space's windows are theirs while they hold it). [`run`] writes one push, its
//! GPFIFO entry, `GP_PUT` and the doorbell, and waits for the push's release (it spins, then
//! sleeps in timer ticks). A release that does not land within `pg_gpu::deadline_ms`, or an
//! error notifier, breaks the channel: it is taken off the runlist and freed (the copy engine
//! touches nothing of the operation any more when [`run`] returns), and a new one is built
//! after the cool-down (`rm_ce_channel::Svc`: three failures in a row turn it off for the
//! transport generation).
//!
//! A transport reset closes the client on the host: [`forget`] drops the kernel views only.

use super::super::native_memory::NativeClient;
use super::{
    cpu_map, cpu_unmap, full_barrier, rm_error_code, write32, zero, CpuView, Io, NativeError,
    ObjectHandles,
};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::{PassiveMutex, PassiveMutexGuard, SpinLock};
use alloc::boxed::Box;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::ce_present::{self as cp, Gen};
use helios_kmd_logic::nvda as nd;
use helios_kmd_logic::pg_gpu::{self as pg, ring};
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client as rc;
use wdk_sys::ntddk::MmUnmapIoSpace;
use wdk_sys::_MEMORY_CACHING_TYPE;

/// Handles of the channel's objects (`ObjectHandles::at`).
const H_TSG: u32 = 0;
const H_CTXSHARE: u32 = 1;
const H_CHANNEL: u32 = 2;
const H_CE: u32 = 3;
const H_CTL: u32 = 4;
const H_USERMODE: u32 = 5;
const H_BLOCK: u32 = 6;
const H_BLOCK_VIRT: u32 = 7;

/// `Chan::made` bits: the objects allocated (by handle index).
const fn bit(index: u32) -> u32 {
    1 << index
}

#[derive(Clone, Copy)]
struct Chan {
    epoch: u64,
    handles: ObjectHandles,
    made: u32,
    ctl: CpuView,
    usermode: CpuView,
    block: CpuView,
    block_gpu: u64,
    engine: u32,
    token: u32,
    put: u32,
    /// The last sequence submitted; the first push carries `SET_OBJECT`.
    seq: u64,
}

static SVC: SpinLock<cc::Svc> = SpinLock::new(cc::Svc::new());
static CHAN: SpinLock<Option<Chan>> = SpinLock::new(None);
static OP_MUTEX: SpinLock<Option<&'static PassiveMutex>> = SpinLock::new(None);

/// `PgGpuChErr`: 0 built, else the bring-up step (high byte) and the error.
static CH_ERR: AtomicU32 = AtomicU32::new(0);
static ENGINE: AtomicU32 = AtomicU32::new(0);
static TOKEN: AtomicU32 = AtomicU32::new(0);
/// Releases that did not land (`PgGpuHung`): 0x1_0000 | notifier status for an RC error.
static HUNG: AtomicU32 = AtomicU32::new(0);
static HUNG_WHY: AtomicU32 = AtomicU32::new(0);
/// Microseconds spent waiting for releases (`PgGpuWaitUs`).
pub(crate) static WAIT_US: AtomicU64 = AtomicU64::new(0);

/// The paging operations' lock: the channel and the paging space's windows are the holder's.
pub(crate) struct OpGuard {
    _guard: PassiveMutexGuard<'static>,
}

/// Take the operations' lock, waiting at most `max_ms`. PASSIVE.
pub(crate) fn lock(passive: PassiveLevel, max_ms: u64) -> Option<OpGuard> {
    let mutex = {
        let existing = *OP_MUTEX.lock();
        match existing {
            Some(m) => m,
            None => {
                let made: &'static PassiveMutex =
                    Box::leak(Box::new(PassiveMutex::try_new(passive)?));
                let mut slot = OP_MUTEX.lock();
                match *slot {
                    Some(m) => m,
                    None => {
                        *slot = Some(made);
                        made
                    }
                }
            }
        }
    };
    mutex
        .lock_within(passive, max_ms)
        .map(|guard| OpGuard { _guard: guard })
}

/// The service word (`PgGpuChState`): `rm_ce_channel::Svc::state_word`.
pub(crate) fn state_word() -> u32 {
    SVC.lock().state_word()
}

/// `(PgGpuChErr, PgGpuEngine, PgGpuToken, PgGpuHung, last hang reason)`.
pub(crate) fn counters() -> (u32, u32, u32, u32, u32) {
    (
        CH_ERR.load(Ordering::Relaxed),
        ENGINE.load(Ordering::Relaxed),
        TOKEN.load(Ordering::Relaxed),
        HUNG.load(Ordering::Relaxed),
        HUNG_WHY.load(Ordering::Relaxed),
    )
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// Why the channel is not there (`NativeError`): the `Svc` refusal maps to `Busy` (cooling
/// down, another thread) or `Unsupported` (struck out).
fn refused(why: cc::Why) -> NativeError {
    match why {
        cc::Why::Disabled => NativeError::Unsupported,
        _ => NativeError::Busy,
    }
}

/// Make sure the channel is up in `space` of `client`: bring it up on the first use (or after a
/// cool-down). PASSIVE, under `_op`.
pub(crate) fn ensure(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    client: &NativeClient,
    space: u32,
    _op: &OpGuard,
) -> Result<(), NativeError> {
    let now_ms = now() / 10_000;
    let admit = SVC.lock().admit(client.epoch, now_ms);
    match admit {
        cc::Admit::Ready => {
            let live = CHAN.lock().is_some_and(|c| c.epoch == client.epoch);
            if live {
                return Ok(());
            }
            // The service is up for this epoch but holds no channel (a reset raced): rebuild.
            SVC.lock().reset();
            let again = SVC.lock().admit(client.epoch, now_ms);
            if again != cc::Admit::BringUp {
                return Err(NativeError::Busy);
            }
        }
        cc::Admit::BringUp => {}
        cc::Admit::Refuse(why) => return Err(refused(why)),
    }
    let io = Io {
        passive,
        adapter,
        epoch: client.epoch,
    };
    let mut chan = Chan {
        epoch: client.epoch,
        handles: ObjectHandles::mint()?,
        made: 0,
        ctl: CpuView::default(),
        usermode: CpuView::default(),
        block: CpuView::default(),
        block_gpu: 0,
        engine: 0,
        token: 0,
        put: 0,
        seq: 0,
    };
    let mut step = 0u32;
    let built = build(io, client, space, &mut chan, &mut step);
    match built {
        Ok(()) => {
            CH_ERR.store(0, Ordering::Relaxed);
            *CHAN.lock() = Some(chan);
            SVC.lock().bring_up_done(true, now() / 10_000);
            Ok(())
        }
        Err(error) => {
            CH_ERR.store((step << 24) | error as u32 + 1, Ordering::Relaxed);
            destroy(io, client, &mut chan);
            SVC.lock().bring_up_done(false, now() / 10_000);
            Err(error)
        }
    }
}

/// The copy engine for the channel: `GET_ENGINES_V2`, `CE_GET_CAPS_V2` of each copy engine,
/// `rm_ce_channel::pick_engine` (an async one that writes system memory).
pub(super) fn pick_engine(io: Io<'_>, client: &NativeClient) -> Result<u32, NativeError> {
    let mut list = [0u8; cc::ENGINES_BYTES];
    io.control(client, nd::H_SUBDEVICE, cc::CTRL_GET_ENGINES_V2, &mut list)?;
    let mut types = [0u32; cc::MAX_CAPS_QUERIES];
    let n = cc::copy_engines(&list, &mut types);
    let mut caps = [cc::CeCaps {
        engine_type: 0,
        caps: [0, 0],
    }; cc::MAX_CAPS_QUERIES];
    let mut k = 0;
    for &t in types.get(..n).unwrap_or(&[]) {
        let mut params = cc::ce_caps_params(t);
        match io.control(
            client,
            nd::H_SUBDEVICE,
            cc::CTRL_CE_GET_CAPS_V2,
            &mut params,
        ) {
            Ok(()) => {
                if let Some(c) = cc::parse_ce_caps(&params, t) {
                    caps[k] = c;
                    k += 1;
                }
            }
            Err(NativeError::Transport) => return Err(NativeError::Transport),
            Err(_) => {}
        }
    }
    cc::pick_engine(caps.get(..k).unwrap_or(&[]))
        .map(|c| c.engine_type)
        .ok_or(NativeError::Unsupported)
}

fn build(
    io: Io<'_>,
    client: &NativeClient,
    space: u32,
    c: &mut Chan,
    step: &mut u32,
) -> Result<(), NativeError> {
    let h = c.handles;
    let classes = Gen::Gb202.classes();
    *step = 1;
    c.engine = pick_engine(io, client)?;
    ENGINE.store(c.engine, Ordering::Relaxed);

    *step = 2;
    io.alloc(
        client,
        nd::H_DEVICE,
        h.at(H_CTL),
        cc::NV01_MEMORY_SYSTEM,
        &helios_kmd_logic::rm_sysmem::params(
            client.root,
            helios_kmd_logic::rm_sysmem::Cache::Cached,
            cc::CTL_BYTES,
        ),
    )?;
    c.made |= bit(H_CTL);
    c.ctl = cpu_map(
        io,
        client,
        nd::H_DEVICE,
        h.at(H_CTL),
        cc::MapNode::Ctl,
        cc::CTL_BYTES,
        _MEMORY_CACHING_TYPE::MmCached,
    )?;
    zero(c.ctl.va, cc::CTL_BYTES);

    *step = 3;
    io.alloc(
        client,
        nd::H_SUBDEVICE,
        h.at(H_USERMODE),
        classes.usermode,
        &cc::usermode_params(),
    )?;
    c.made |= bit(H_USERMODE);
    c.usermode = cpu_map(
        io,
        client,
        nd::H_SUBDEVICE,
        h.at(H_USERMODE),
        cc::MapNode::Gpu,
        cp::USERMODE_BYTES,
        _MEMORY_CACHING_TYPE::MmNonCached,
    )?;

    *step = 4;
    io.alloc(
        client,
        nd::H_DEVICE,
        h.at(H_BLOCK),
        cc::NV01_MEMORY_SYSTEM,
        &helios_kmd_logic::rm_sysmem::params(
            client.root,
            helios_kmd_logic::rm_sysmem::Cache::Cached,
            ring::BYTES,
        ),
    )?;
    c.made |= bit(H_BLOCK);
    c.block = cpu_map(
        io,
        client,
        nd::H_DEVICE,
        h.at(H_BLOCK),
        cc::MapNode::Ctl,
        ring::BYTES,
        _MEMORY_CACHING_TYPE::MmCached,
    )?;
    zero(c.block.va, ring::BYTES);

    *step = 5;
    // A 4-KiB-paged virtual allocation RM places itself in the paging space (away from the
    // windows), the block mapped into it with system-memory PTEs.
    io.alloc(
        client,
        nd::H_DEVICE,
        h.at(H_BLOCK_VIRT),
        cc::NV50_MEMORY_VIRTUAL,
        &cc::virtual_params_4k(client.root, space, None, ring::BYTES),
    )?;
    c.made |= bit(H_BLOCK_VIRT);
    let block = cc::nvos46(&cc::DmaMap {
        root: client.root,
        h_device: nd::H_DEVICE,
        h_dma: h.at(H_BLOCK_VIRT),
        h_memory: h.at(H_BLOCK),
        length: ring::BYTES,
        flags: cc::MAP_FLAGS_SYSMEM,
    });
    let mut resp = [0u8; 256];
    let n = io.exchange(
        client.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut resp,
    )?;
    let reply = rc::rm_reply(
        resp.get(..n).ok_or(NativeError::Transport)?,
        cc::NVOS46_STATUS_AT,
    )
    .map_err(|error| {
        CH_ERR.store(
            (5 << 24) | (rm_error_code(error) & 0xff_ffff),
            Ordering::Relaxed,
        );
        NativeError::Rm
    })?;
    c.block_gpu = cc::map_dma_va(reply.data).ok_or(NativeError::Transport)?;
    if c.block_gpu == 0 || c.block_gpu % 0x1000 != 0 || c.block_gpu + ring::BYTES > cp::MAX_VA {
        return Err(NativeError::Invalid);
    }

    *step = 6;
    io.alloc(
        client,
        nd::H_DEVICE,
        h.at(H_TSG),
        cc::KEPLER_CHANNEL_GROUP_A,
        &cc::channel_group_params(space, c.engine),
    )?;
    c.made |= bit(H_TSG);
    *step = 7;
    io.alloc(
        client,
        h.at(H_TSG),
        h.at(H_CTXSHARE),
        cc::FERMI_CONTEXT_SHARE_A,
        &cc::ctxshare_params(space),
    )?;
    c.made |= bit(H_CTXSHARE);
    *step = 8;
    let params = cc::channel_params(&cc::ChannelDesc {
        h_ctl: h.at(H_CTL),
        gpfifo_va: c.block_gpu + ring::GPFIFO_AT,
        entries: ring::ENTRIES,
        h_ctxshare: h.at(H_CTXSHARE),
        userd_offset: cc::USERD_OFFSET,
        engine_type: c.engine,
        flags: 0,
    });
    io.alloc(
        client,
        h.at(H_TSG),
        h.at(H_CHANNEL),
        classes.gpfifo,
        &params,
    )?;
    c.made |= bit(H_CHANNEL);
    *step = 9;
    let mut bind = cc::bind_params(c.engine);
    io.control(client, h.at(H_CHANNEL), cc::CTRL_BIND, &mut bind)?;
    *step = 10;
    io.alloc(
        client,
        h.at(H_CHANNEL),
        h.at(H_CE),
        classes.copy,
        &cc::ce_object_params(c.engine),
    )?;
    c.made |= bit(H_CE);
    *step = 11;
    let mut index = cc::token_notif_index_params();
    io.control(
        client,
        h.at(H_CHANNEL),
        cc::CTRL_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX,
        &mut index,
    )?;
    let mut token = cc::token_params();
    io.control(
        client,
        h.at(H_CHANNEL),
        cc::CTRL_GET_WORK_SUBMIT_TOKEN,
        &mut token,
    )?;
    c.token = cc::parse_token(&token)
        .filter(|t| t.0 != 0)
        .ok_or(NativeError::Transport)?
        .0;
    TOKEN.store(c.token, Ordering::Relaxed);
    *step = 12;
    let mut schedule = cc::schedule_params(true);
    io.control(client, h.at(H_TSG), cc::CTRL_GPFIFO_SCHEDULE, &mut schedule)?;

    *step = 13;
    // The first push: SET_OBJECT and a release, which must land.
    let seq = 1;
    let words = write_push(c, seq, |p, first, sem| {
        pg::release_push(p, Gen::Gb202, first, sem, seq)
    })?;
    kick(c, words)?;
    c.seq = seq;
    match wait(io.passive, c, seq, pg::FIRST_PUSH_MS, 0) {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Write the push of `seq` into its slot; `Ok(dwords)`.
fn write_push(
    c: &Chan,
    seq: u64,
    f: impl FnOnce(&mut cp::Push<'_>, bool, u64) -> Result<(), cp::PushError>,
) -> Result<u32, NativeError> {
    let mut buf = [0u32; (ring::PUSH_SLOT_BYTES / 4) as usize];
    let mut push = cp::Push::new(&mut buf);
    f(&mut push, c.seq == 0, c.block_gpu + ring::SEMAPHORE_AT).map_err(|_| NativeError::Invalid)?;
    let words = push.words();
    let at = c.block.va + ring::push_offset(seq);
    for (i, w) in words.iter().enumerate() {
        // SAFETY: inside the block's kernel view (`push_offset` + one slot).
        unsafe { write32(at + 4 * i as u64, *w) };
    }
    Ok(words.len() as u32)
}

/// Append the GPFIFO entry of the push in `c.seq + 1`'s slot (`dwords` long), `GP_PUT`, the
/// doorbell.
fn kick(c: &mut Chan, dwords: u32) -> Result<(), NativeError> {
    let seq = c.seq + 1;
    let entry = cp::gp_entry(c.block_gpu + ring::push_offset(seq), dwords)
        .map_err(|_| NativeError::Invalid)?;
    let at = c.block.va + ring::GPFIFO_AT + u64::from(c.put) * 8;
    // SAFETY: `put` < ENTRIES: inside the ring at the block's start.
    unsafe {
        write32(at, entry as u32);
        write32(at + 4, (entry >> 32) as u32);
    }
    let put = ring::next(c.put);
    full_barrier();
    // SAFETY: USERD GP_PUT in the control view, then the doorbell (as `CopyChannel::submit`).
    unsafe {
        write32(
            c.ctl.va + cc::USERD_OFFSET + u64::from(cp::USERD_GP_PUT),
            put,
        )
    };
    full_barrier();
    unsafe { write32(c.usermode.va + u64::from(cp::DOORBELL_OFFSET), c.token) };
    c.put = put;
    Ok(())
}

fn semaphore(c: &Chan) -> u64 {
    // SAFETY: the live kernel view of the block; 8-aligned.
    unsafe { ((c.block.va + ring::SEMAPHORE_AT) as *const u64).read_volatile() }
}

fn notifier_status(c: &Chan) -> u16 {
    // SAFETY: the live kernel view of the control memory; the error notifier at its start.
    unsafe {
        ((c.ctl.va + cc::NOTIFIER_OFFSET + cc::NOTIFIER_STATUS_AT) as *const u16).read_volatile()
    }
}

/// Wait until the release of `seq` landed, at most `max_ms`; spin `spin` (100 ns units)
/// first. `Ok(waited in 100 ns)`, `Err(Dead)` for an RC error or a release that did not land.
fn wait(
    passive: PassiveLevel,
    c: &Chan,
    seq: u64,
    max_ms: u64,
    spin: u64,
) -> Result<u64, NativeError> {
    let start = now();
    let deadline = start + max_ms * 10_000;
    let spin_until = start + spin;
    loop {
        if semaphore(c) >= seq {
            return Ok(now().saturating_sub(start));
        }
        let status = notifier_status(c);
        if status != 0 {
            HUNG_WHY.store(0x1_0000 | u32::from(status), Ordering::Relaxed);
            return Err(NativeError::Dead);
        }
        let t = now();
        if t >= deadline {
            HUNG_WHY.store(0x2_0000 | (seq as u32 & 0xffff), Ordering::Relaxed);
            return Err(NativeError::Dead);
        }
        if t < spin_until {
            core::hint::spin_loop();
        } else {
            super::super::ctrl::sleep_ms(passive, 1);
        }
    }
}

/// Run one push on the channel and wait for it: `f` writes the push of the given sequence
/// (`first`: the channel's first push, which carries `SET_OBJECT`; the semaphore VA its release
/// writes). `bytes` sizes the wait. A release that does not land (or an RC error) breaks the
/// channel, which is taken off the runlist and freed before this returns `Err(Dead)`.
/// PASSIVE, under `_op`.
pub(crate) fn run(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    client: &NativeClient,
    _op: &OpGuard,
    bytes: u64,
    f: impl FnOnce(&mut cp::Push<'_>, bool, u64, u64) -> Result<(), cp::PushError>,
) -> Result<u64, NativeError> {
    let Some(mut c) = *CHAN.lock() else {
        return Err(NativeError::Transport);
    };
    let epoch = adapter.nvrm_epoch_fast();
    if epoch == 0 || epoch != c.epoch || client.epoch != c.epoch {
        return Err(NativeError::Transport);
    }
    let seq = c.seq + 1;
    let dwords = write_push(&c, seq, |p, first, sem| f(p, first, sem, seq))?;
    kick(&mut c, dwords)?;
    c.seq = seq;
    *CHAN.lock() = Some(c);
    match wait(
        passive,
        &c,
        seq,
        pg::deadline_ms(bytes),
        pg::spin_100ns(bytes),
    ) {
        Ok(waited) => {
            WAIT_US.fetch_add(waited / 10, Ordering::Relaxed);
            Ok(waited)
        }
        Err(error) => {
            HUNG.fetch_add(1, Ordering::Relaxed);
            let taken = CHAN.lock().take();
            SVC.lock().on_channel_error();
            if SVC.lock().begin_teardown() {
                if let Some(mut c) = taken {
                    let io = Io {
                        passive,
                        adapter,
                        epoch: c.epoch,
                    };
                    destroy(io, client, &mut c);
                }
                SVC.lock().torn_down(now() / 10_000);
            }
            Err(error)
        }
    }
}

/// Take the channel off the runlist and free what was made (children with their TSG). What RM
/// refuses to free stays to the client's teardown (the transport's sweep). PASSIVE.
fn destroy(io: Io<'_>, client: &NativeClient, c: &mut Chan) {
    let h = c.handles;
    if c.made & bit(H_TSG) != 0 {
        let mut schedule = cc::schedule_params(false);
        let _ = io.control(client, h.at(H_TSG), cc::CTRL_GPFIFO_SCHEDULE, &mut schedule);
        if io.free(client, nd::H_DEVICE, h.at(H_TSG)).is_err() {
            // The channel may still name the block and the control memory: leave them.
            drop_views(c);
            return;
        }
        c.made &= !(bit(H_TSG) | bit(H_CTXSHARE) | bit(H_CHANNEL) | bit(H_CE));
    }
    for view in [&mut c.usermode, &mut c.ctl, &mut c.block] {
        if view.va != 0 && cpu_unmap(io, client, view).is_err() {
            // `cpu_unmap` removed the kernel view first; the host half goes with the client.
            view.va = 0;
        }
    }
    for (index, parent) in [
        (H_BLOCK_VIRT, nd::H_DEVICE),
        (H_BLOCK, nd::H_DEVICE),
        (H_USERMODE, nd::H_SUBDEVICE),
        (H_CTL, nd::H_DEVICE),
    ] {
        if c.made & bit(index) != 0 && io.free(client, parent, h.at(index)).is_ok() {
            c.made &= !bit(index);
        }
    }
}

fn drop_views(c: &mut Chan) {
    for view in [&mut c.usermode, &mut c.ctl, &mut c.block] {
        if view.va != 0 {
            // SAFETY: this exact MmMapIoSpace view, removed once.
            unsafe { MmUnmapIoSpace(view.va as *mut c_void, view.len) };
            view.va = 0;
        }
    }
}

/// The transport generation ended (`native_memory::forget`): the host closed the client with
/// every object; drop the kernel views and start cold. Under the operations' lock when one can
/// be had, so no operation is using the views.
pub(crate) fn forget(passive: PassiveLevel) {
    let op = lock(passive, 5_000);
    let taken = CHAN.lock().take();
    if let Some(mut c) = taken {
        if op.is_some() {
            drop_views(&mut c);
        }
        // Without the lock an operation may still read the views: they are leaked (a few
        // pages of kernel VA per reset that raced a stuck operation).
    }
    SVC.lock().reset();
    drop(op);
}
