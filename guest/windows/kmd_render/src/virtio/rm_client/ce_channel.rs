//! The KMD's own copy-engine channel (`RmCopyEngine`): the I/O half of milestone M3b. Design and
//! what differs from the user-mode tool: `docs/rm-copy-engine-present.md` section 11 (11.9 is
//! what is built). Every decision (the parameter blocks, the stage order, the undo order, the
//! service's phases and strikes, the deadlines) is `helios_kmd_logic::rm_ce_channel`; the push
//! words, GPFIFO entries and the ring of slots are `helios_kmd_logic::ce_present`. This file
//! performs them.
//!
//! A CHILD of [`super`] (`rm_client`), as `sysmem.rs` is: it drives the same I/O object ([`Io`]:
//! the forwarded RM messages as the KMD's own owner) and the same client bring-up machine.
//!
//! WHAT IT DOES. [`ensure_up`] brings the channel up once (lazily, on the first use: the self-test
//! of `RmCopyEngine` = 2 in M3b, the Present route of M3c later), in the M1 tool's order: its own
//! RM client (the device with the tool's parameters), the class list, the VA space, the engine
//! pick, the usermode doorbell and its CPU view, the control memory (error notifier, USERD) and
//! the ring memory (GPFIFO, semaphores, push slots) in RM system memory with their CPU views
//! through the RM window, the ring's GPU mapping, the channel group on `COPY(n)`, the subcontext,
//! the channel, BIND, the copy object, the work-submit token, the schedule, and a first push whose
//! release must land. [`submit`] writes one push, its GPFIFO entry, `GP_PUT` and the doorbell: no
//! RM call. Completion is POLLED ([`poll`]): the completion value the push's release writes, read
//! through the ring's CPU view (the event path is M3c's).
//!
//! TIME. Every host message is bounded: one [`cc::BRING_UP_BUDGET_MS`] deadline for the whole
//! bring-up (each message waits at most what is left, none is sent once it is spent), inside an
//! `escape_wait` bounded section so every wait primitive below obeys it too; the undo of a failed
//! bring-up has its own [`cc::UNDO_BUDGET_MS`]; StopDevice's teardown runs on the stop budget.
//! PASSIVE only, on the HPD worker (or StopDevice / StartDevice after it was joined).
//!
//! LOCKING. `STATE` is a LEAF spinlock over plain data (the service, the channel's handles and
//! views, the ring). A bring-up and a teardown copy what they need out, do their I/O with no lock
//! held, and report back. [`submit`] and [`poll`] run UNDER it: plain stores and loads into the
//! kernel views (the push, the entry, `GP_PUT`, the doorbell; the completion and the error
//! notifier), nothing allocated, nothing waited on. A teardown takes the channel out of `STATE`
//! first, so no submitter can reach a view it is about to unmap. `IO_BUSY` makes the channel's
//! RM I/O one thread's at a time.
//!
//! KNOB. `RmCopyEngine` is read at StartDevice ([`reset_for_start`]) and mirrored (`CeKnob`). At 0
//! (the default) nothing here runs but that read and mirror, one relaxed load per HPD worker pass
//! ([`service`]), one in StopDevice / StartDevice ([`retire_for_stop`]), in `retire_begin`
//! ([`drop_views`]) and in `forget`: no allocation, no RM message, no other registry write.

use super::{fail_of, Io, KMD};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::nvrm::{self, MapRefusal, Refusal};
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::ce_present::{self as cp, Gen};
use helios_kmd_logic::rm_ce_channel::{self as cc, Made, Stage, Undo};
use helios_kmd_logic::rm_client::{self as rc, Action, Client, Fail, FailKind, Out, Step, Want};
use helios_kmd_logic::rm_sysmem as rs;
use helios_kmd_logic::sweep_budget::{SweepBudget, UNITS_PER_MS};
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

/// Longest client bring-up: eleven steps plus slack (as `sysmem.rs`).
const CLIENT_STEPS: usize = 16;

// ---- counters (names at most 14 characters; `rm_ce_channel::COUNTERS`) --------------------------

const UNREAD: u32 = u32::MAX;
/// `RmCopyEngine` in force (`rm_ce_channel::knob_in_force`), or [`UNREAD`] before the first
/// StartDevice read it.
static KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static RUNLIST: AtomicU32 = AtomicU32::new(0);
static RM_ERR: AtomicU32 = AtomicU32::new(0);
static CHAN_FAIL: AtomicU32 = AtomicU32::new(0);
static CH_TRY: AtomicU32 = AtomicU32::new(0);
static CH_UP: AtomicU32 = AtomicU32::new(0);
static CH_DOWN: AtomicU32 = AtomicU32::new(0);
static CH_STAGE: AtomicU32 = AtomicU32::new(0);
static CH_FAIL: AtomicU32 = AtomicU32::new(0);
static CH_SOFT: AtomicU32 = AtomicU32::new(0);
static CH_MS: AtomicU32 = AtomicU32::new(0);
static GEN: AtomicU32 = AtomicU32::new(0);
static ENGINE: AtomicU32 = AtomicU32::new(0);
static CAPS: AtomicU32 = AtomicU32::new(0);
static TOKEN: AtomicU32 = AtomicU32::new(0);
static NOTIFY: AtomicU32 = AtomicU32::new(0);
static SUBMIT: AtomicU32 = AtomicU32::new(0);
/// `RmCeCache` in force (`CacheMode::word`: 0 cached, 1 write-combined), read at StartDevice.
static CACHE: AtomicU32 = AtomicU32::new(0);
/// The last failing RM call (`rm_call_word`), its status (`fail_word`), and for a CPU view the
/// file it was armed on (`map_node_word`).
static RM_CALL: AtomicU32 = AtomicU32::new(0);
static RM_STAT: AtomicU32 = AtomicU32::new(0);
static MAP_NODE: AtomicU32 = AtomicU32::new(0);
// The self-test's results (`ce_selftest.rs` stores them; published here with the rest).
pub(super) static SELF_TEST: AtomicU32 = AtomicU32::new(0);
pub(super) static SELF_WHY: AtomicU32 = AtomicU32::new(0);
pub(super) static SELF_US: AtomicU32 = AtomicU32::new(0);
pub(super) static SELF_WAIT_US: AtomicU32 = AtomicU32::new(0);
pub(super) static SELF_PAGES: AtomicU32 = AtomicU32::new(0);
pub(super) static SELF_MS: AtomicU32 = AtomicU32::new(0);
/// The self-test ran in this transport generation (one per generation).
static SELF_DONE: AtomicU32 = AtomicU32::new(0);

/// Nonzero while `STATE` holds a channel (up or broken): StopDevice's check is one load.
static LIVE: AtomicU32 = AtomicU32::new(0);
/// Nonzero while one thread performs the channel's RM I/O (bring-up, self-test, teardown).
static IO_BUSY: AtomicU32 = AtomicU32::new(0);

/// A new transport generation (StartDevice, PASSIVE): the knob is read again and mirrored with the
/// value in force (0 included), the counters are zeroed, the self-test may run again.
#[inline(never)]
pub(crate) fn reset_for_start() {
    for cell in [
        &RUNLIST, &RM_ERR, &CHAN_FAIL, &CH_TRY, &CH_UP, &CH_DOWN, &CH_STAGE, &CH_FAIL, &CH_SOFT,
        &CH_MS, &GEN, &ENGINE, &CAPS, &TOKEN, &NOTIFY, &SUBMIT, &SELF_TEST, &SELF_WHY, &SELF_US,
        &SELF_WAIT_US, &SELF_PAGES, &SELF_MS, &SELF_DONE, &RM_CALL, &RM_STAT, &MAP_NODE,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
    let v = cc::knob_in_force(crate::diag::read_config_dword(
        crate::diag::knobs::RM_COPY_ENGINE,
        cp::KNOB_DEFAULT,
    ));
    KNOB.store(v, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"CeKnob", v);
    // `RmCeCache` (default 0 = cached, the tool's kind), mirrored as `CeCache` with the value in
    // force. Read only when the channel can run at all.
    let cache = if v == 0 {
        cc::CacheMode::Cached
    } else {
        cc::CacheMode::from_knob(crate::diag::read_config_dword(crate::diag::knobs::RM_CE_CACHE, 0))
    };
    CACHE.store(cache.word(), Ordering::Relaxed);
    if v != 0 {
        crate::diag::record_named_bytes(b"CeCache", cache.word());
        // The dup cache's counters (M3c-1): only when the channel can run at all.
        super::ce_dup::reset_for_start();
    }
}

/// `RmCopyEngine` is 3 (the shadow mode, M3c-1): one relaxed load.
pub(crate) fn shadow_mode() -> bool {
    cc::mode(KNOB.load(Ordering::Relaxed)) == cc::Mode::Shadow
}

/// The cache attribute of the channel's own RM system memory in force.
fn cache_mode() -> cc::CacheMode {
    cc::CacheMode::from_knob(CACHE.load(Ordering::Relaxed))
}

/// The kernel views of the channel's RM system memory: the attribute the memory was made with.
pub(super) fn sysmem_view_cache() -> _MEMORY_CACHING_TYPE::Type {
    match cache_mode() {
        cc::CacheMode::Cached => _MEMORY_CACHING_TYPE::MmCached,
        cc::CacheMode::WriteCombine => _MEMORY_CACHING_TYPE::MmWriteCombined,
    }
}

/// Pseudo escape numbers of `CeRmCall` for the steps of a CPU view that are not RM escapes: the
/// `Open` of its map file, the host's `Mmap`, `MmMapIoSpace`.
const CALL_OPEN: u32 = 0xf1;
const CALL_HOST_MMAP: u32 = 0xf2;
const CALL_KERNEL_MAP: u32 = 0xf3;

/// A call of the channel failed: name it (`CeRmCall`, `CeRmStat`) so a dump says which, without
/// the backend log. Atomics only.
pub(super) fn note_call(esc: u32, what: u32, f: Fail) {
    RM_CALL.store(cc::rm_call_word(esc, what), Ordering::Relaxed);
    RM_STAT.store(cc::fail_word(f), Ordering::Relaxed);
}

/// [`note_call`] on the error of `r`.
fn noted<T>(r: Result<T, Fail>, esc: u32, what: u32) -> Result<T, Fail> {
    if let Err(f) = &r {
        note_call(esc, what, *f);
    }
    r
}

/// Mirror the counters to the service key once the channel was asked for. PASSIVE only.
pub(crate) fn publish_counters() {
    if CH_TRY.load(Ordering::Relaxed) == 0 && SELF_TEST.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (state, chan) = {
        let g = STATE.lock();
        let made = g.parts.map_or(0, |p| u32::from(p.made.bits()));
        (g.svc.state_word() | made, cc::chan_word(g.svc.phase()))
    };
    use crate::diag::record_named_bytes as rec;
    rec(b"CeKnob", KNOB.load(Ordering::Relaxed) & 0xff);
    rec(b"CeChan", chan);
    rec(b"CeRunlist", RUNLIST.load(Ordering::Relaxed));
    rec(b"CeRmErr", RM_ERR.load(Ordering::Relaxed));
    rec(b"CeChanFail", CHAN_FAIL.load(Ordering::Relaxed));
    rec(b"CeChTry", CH_TRY.load(Ordering::Relaxed));
    rec(b"CeChUp", CH_UP.load(Ordering::Relaxed));
    rec(b"CeChDown", CH_DOWN.load(Ordering::Relaxed));
    rec(b"CeChStage", CH_STAGE.load(Ordering::Relaxed));
    rec(b"CeChFail", CH_FAIL.load(Ordering::Relaxed));
    rec(b"CeChState", state);
    rec(b"CeChSoft", CH_SOFT.load(Ordering::Relaxed));
    rec(b"CeChMs", CH_MS.load(Ordering::Relaxed));
    rec(b"CeGen", GEN.load(Ordering::Relaxed));
    rec(b"CeEngine", ENGINE.load(Ordering::Relaxed));
    rec(b"CeCaps", CAPS.load(Ordering::Relaxed));
    rec(b"CeToken", TOKEN.load(Ordering::Relaxed));
    rec(b"CeNotify", NOTIFY.load(Ordering::Relaxed));
    rec(b"CeSubmit", SUBMIT.load(Ordering::Relaxed));
    rec(b"CeSelfTest", SELF_TEST.load(Ordering::Relaxed));
    rec(b"CeSelfWhy", SELF_WHY.load(Ordering::Relaxed));
    rec(b"CeSelfUs", SELF_US.load(Ordering::Relaxed));
    rec(b"CeSelfWaitUs", SELF_WAIT_US.load(Ordering::Relaxed));
    rec(b"CeSelfPages", SELF_PAGES.load(Ordering::Relaxed));
    rec(b"CeSelfMs", SELF_MS.load(Ordering::Relaxed));
    rec(b"CeCache", CACHE.load(Ordering::Relaxed));
    rec(b"CeRmCall", RM_CALL.load(Ordering::Relaxed));
    rec(b"CeRmStat", RM_STAT.load(Ordering::Relaxed));
    rec(b"CeMapNode", MAP_NODE.load(Ordering::Relaxed));
    super::ce_dup::publish_counters();
}

/// The stage about to run, written BEFORE it runs (a hang names itself).
fn note_stage(s: Stage) {
    CH_STAGE.store(s as u32, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"CeChStage", s as u32);
}

/// An undo step failed (`CeChSoft`).
pub(super) fn note_soft() {
    CH_SOFT.fetch_add(1, Ordering::Relaxed);
}

/// An RM call of the channel failed (`CeRmErr`).
pub(super) fn note_rm_error() {
    RM_ERR.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn now_100ns() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

pub(super) fn now_ms() -> u64 {
    now_100ns() / UNITS_PER_MS
}

/// A deadline of `ms` from now, no message waiting longer than [`super::TIMEOUT_MS`].
pub(super) fn budget_ms(ms: u64) -> SweepBudget {
    SweepBudget::new(now_100ns(), ms * UNITS_PER_MS, super::TIMEOUT_MS)
}

// ---- what the channel is made of ----------------------------------------------------------------

/// What the channel keeps of its RM client.
#[derive(Clone, Copy, Default)]
pub(crate) struct Handles {
    pub ctl: u32,
    pub gpu: u32,
    pub drm: u32,
    pub root: u32,
    /// The GPU's minor: the device type of a map channel, and the RM window's region.
    pub minor: u32,
}

/// One CPU view through the RM window: librmclient's channel-per-mapping protocol (a fresh GPU
/// file tied to the control file, `RM_MAP_MEMORY` armed on it, the host's `Mmap` of it), then
/// `MmMapIoSpace` of the window range, as `rm_client.rs`'s level 2 view.
#[derive(Clone, Copy, Default)]
pub(super) struct CpuView {
    pub map_ch: u32,
    pub cookie: u64,
    pub host_id: u32,
    /// The host's `Mmap` was served (its id may be 0: the RM path answers 0).
    pub host_mapped: bool,
    /// The kernel VA; 0 when not mapped.
    pub va: u64,
    pub len: u64,
    /// The device type of the map file: `rm_client::DEV_CTL` for system memory, the GPU's minor
    /// for BAR memory (`rm_ce_channel::MapNode`); also the RM window region's key.
    pub node_dev: u32,
    /// The object the RM mapping names: its parent (the device, or the subdevice for the
    /// doorbell) and the memory.
    pub parent: u32,
    pub mem: u32,
}

/// One GPU mapping: the `NV50_MEMORY_VIRTUAL` carved out of the VA space and the GPU VA.
#[derive(Clone, Copy, Default)]
pub(super) struct GpuMap {
    pub virt: u32,
    pub mem: u32,
    pub va: u64,
    pub len: u64,
}

/// Everything a bring-up made: what a teardown (or the undo of a failed bring-up) gives back.
#[derive(Clone, Copy, Default)]
struct Parts {
    h: Handles,
    gen: Option<Gen>,
    usermode_class: u32,
    engine: Option<cc::CeCaps>,
    token: u32,
    um: CpuView,
    ctl: CpuView,
    ring: CpuView,
    ring_gpu: GpuMap,
    made: Made,
}

struct State {
    svc: cc::Svc,
    /// The channel, while it is up or broken.
    parts: Option<Parts>,
    ring: cp::Ring,
}

const fn fresh_ring() -> cp::Ring {
    match cp::Ring::new(cc::GPFIFO_ENTRIES, 0) {
        Some(r) => r,
        // `GPFIFO_ENTRIES` is in `Ring::new`'s range: evaluated at compile time, never reached.
        None => unreachable!(),
    }
}

static STATE: SpinLock<State> = SpinLock::new(State {
    svc: cc::Svc::new(),
    parts: None,
    ring: fresh_ring(),
});

// ---- the worker's entry ---------------------------------------------------------------------------

/// One pass from the HPD worker's loop (PASSIVE). With `RmCopyEngine` 0 (or 1, reserved for the
/// Present route of M3c) this is one relaxed load. With 2: the self-test, once per transport
/// generation, as soon as the RM transport and the display are up. Not inlined: the worker's frame
/// must not grow by this function's locals.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    let knob = KNOB.load(Ordering::Relaxed);
    if cc::mode(knob) != cc::Mode::SelfTest || knob == UNREAD {
        return;
    }
    if SELF_DONE.load(Ordering::Relaxed) != 0 {
        return;
    }
    // The display is running: a VidPn primary is bound.
    if !adapter.display_half() || adapter.active_scanout_resource.load(Ordering::Acquire) == 0 {
        return;
    }
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return;
    };
    if epoch == 0 || adapter.hpd_stop.load(Ordering::Acquire) != 0 {
        return;
    }
    if IO_BUSY
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    SELF_DONE.store(1, Ordering::Relaxed);
    super::ce_selftest::run(passive, adapter, epoch);
    // M3b: the channel is not kept after its self-test (its teardown is part of what the
    // self-test proves); M3c keeps it for the Present route.
    teardown(passive, adapter, budget_ms(cc::UNDO_BUDGET_MS));
    IO_BUSY.store(0, Ordering::Release);
    publish_counters();
}

// ---- the bring-up ---------------------------------------------------------------------------------

/// Why a caller has no channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotUp {
    Refused(cc::Why),
    /// This call's bring-up failed: the stage and how.
    Failed(Stage, Fail),
}

/// The channel up for transport generation `epoch`, bringing it up now if it is cold (or its
/// cool-down is over). PASSIVE, no lock held, the caller holds `IO_BUSY`.
#[inline(never)]
pub(super) fn ensure_up(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
) -> Result<(), NotUp> {
    let admit = STATE.lock().svc.admit(epoch, now_ms());
    match admit {
        cc::Admit::Ready => return Ok(()),
        cc::Admit::Refuse(w) => return Err(NotUp::Refused(w)),
        cc::Admit::BringUp => {}
    }
    CH_TRY.fetch_add(1, Ordering::Relaxed);
    let started = now_100ns();
    let mut parts = Parts::default();
    let mut ring = fresh_ring();
    let result = {
        // Every wait primitive below obeys the bring-up's deadline (a thread inside a bounded
        // section sees it as an escape's), not only the messages this file sends.
        let _bounded =
            crate::ddi::escape_wait::begin_bounded(cc::BRING_UP_BUDGET_MS as u32);
        let io = Io {
            passive,
            adapter,
            epoch,
            limit: Some(budget_ms(cc::BRING_UP_BUDGET_MS)),
        };
        bring_up(&io, &mut parts, &mut ring)
    };
    let ms = (now_100ns().wrapping_sub(started) / UNITS_PER_MS).min(u64::from(u32::MAX)) as u32;
    CH_MS.store(ms, Ordering::Relaxed);
    match result {
        Ok(()) => {
            {
                let mut g = STATE.lock();
                g.parts = Some(parts);
                g.ring = ring;
                g.svc.bring_up_done(true, now_ms());
            }
            LIVE.store(1, Ordering::Release);
            CH_UP.fetch_add(1, Ordering::Relaxed);
            publish_counters();
            Ok(())
        }
        Err((stage, f)) => {
            let word = cc::pack_failure(stage as u8, f);
            CH_FAIL.store(word, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"CeChFail", word);
            if stage == Stage::FirstPush {
                CHAN_FAIL.fetch_add(1, Ordering::Relaxed);
            }
            // The undo on its own allowance: the bring-up's may be what ran out.
            let io = Io {
                passive,
                adapter,
                epoch,
                limit: Some(budget_ms(cc::UNDO_BUDGET_MS)),
            };
            undo_all(&io, &mut parts, &mut ring);
            STATE.lock().svc.bring_up_done(false, now_ms());
            publish_counters();
            Err(NotUp::Failed(stage, f))
        }
    }
}

/// The stages in order, each its own function (their buffers never share a frame).
fn bring_up(io: &Io<'_>, p: &mut Parts, ring: &mut cp::Ring) -> Result<(), (Stage, Fail)> {
    let mut b = cc::BringUp::new();
    while let Some(stage) = b.next_stage() {
        note_stage(stage);
        let result = if io.stopping() {
            // StopDevice asked the worker to go: start nothing more.
            Err(Fail::new(FailKind::Transport, 0xE2))
        } else if io.limit_spent() {
            Err(Fail::new(FailKind::Transport, 0xE1))
        } else {
            perform(io, stage, p, ring)
        };
        if result.is_err() && stage != Stage::FirstPush {
            note_rm_error();
        }
        b.finish(stage, result);
        p.made = b.made();
    }
    match b.failure() {
        Some(f) => Err(f),
        None if b.is_ready() => Ok(()),
        None => Err((Stage::Client, Fail::new(FailKind::Parse, 0xfc))),
    }
}

fn perform(io: &Io<'_>, stage: Stage, p: &mut Parts, ring: &mut cp::Ring) -> Result<(), Fail> {
    let h = p.h;
    match stage {
        Stage::Client => {
            p.h = client(io)?;
            Ok(())
        }
        Stage::ClassList => class_list(io, p),
        Stage::VaSpace => alloc(
            io,
            &h,
            rc::H_DEVICE,
            cc::H_VASPACE,
            cc::FERMI_VASPACE_A,
            &cc::vaspace_params(),
        ),
        Stage::Engines => engines(io, p),
        Stage::Usermode => {
            let params = cc::usermode_params();
            let block: &[u8] = if cc::usermode_takes_params(p.usermode_class) {
                &params
            } else {
                &[]
            };
            alloc(io, &h, rc::H_SUBDEVICE, cc::H_USERMODE, p.usermode_class, block)
        }
        Stage::UsermodeMap => {
            p.um = cpu_map(
                io,
                &h,
                rc::H_SUBDEVICE,
                cc::H_USERMODE,
                cc::MapNode::for_class(p.usermode_class),
                cp::USERMODE_BYTES,
                _MEMORY_CACHING_TYPE::MmNonCached,
            )?;
            Ok(())
        }
        Stage::Ctl => alloc_sys(io, &h, cc::H_CTL, cc::CTL_BYTES),
        Stage::CtlMap => {
            p.ctl = cpu_map(io, &h, rc::H_DEVICE, cc::H_CTL, SYSMEM, cc::CTL_BYTES, sysmem_view_cache())?;
            // The error notifier and USERD start at zero, as the tool's `memset`.
            zero(p.ctl.va, cc::CTL_BYTES);
            Ok(())
        }
        Stage::Ring => alloc_sys(io, &h, cc::H_RING, cc::RING_BYTES),
        Stage::RingMap => {
            p.ring = cpu_map(io, &h, rc::H_DEVICE, cc::H_RING, SYSMEM, cc::RING_BYTES, sysmem_view_cache())?;
            // The GPFIFO and the semaphore page; a push slot is written whole before use.
            zero(p.ring.va, cc::PUSH_OFFSET);
            Ok(())
        }
        Stage::RingGpuMap => {
            p.ring_gpu = gpu_map(io, &h, cc::H_RING_VIRT, cc::H_RING, cc::VA_RING, cc::RING_BYTES)?;
            Ok(())
        }
        Stage::Group => {
            let engine = p.engine.ok_or(Fail::new(FailKind::Parse, 0x62))?;
            alloc(
                io,
                &h,
                rc::H_DEVICE,
                cc::H_TSG,
                cc::KEPLER_CHANNEL_GROUP_A,
                &cc::channel_group_params(cc::H_VASPACE, engine.engine_type),
            )
        }
        Stage::Subcontext => alloc(
            io,
            &h,
            cc::H_TSG,
            cc::H_CTXSHARE,
            cc::FERMI_CONTEXT_SHARE_A,
            &cc::ctxshare_params(cc::H_VASPACE),
        ),
        Stage::Channel => channel(io, p),
        Stage::Bind => {
            let engine = p.engine.ok_or(Fail::new(FailKind::Parse, 0x62))?;
            let mut params = cc::bind_params(engine.engine_type);
            control(io, &h, cc::H_CHANNEL, cc::CTRL_BIND, &mut params)
        }
        Stage::CeObject => {
            let (gen, engine) = gen_engine(p)?;
            alloc(
                io,
                &h,
                cc::H_CHANNEL,
                cc::H_CE,
                gen.classes().copy,
                &cc::ce_object_params(engine.engine_type),
            )
        }
        Stage::NotifIndex => {
            let mut params = cc::token_notif_index_params();
            control(
                io,
                &h,
                cc::H_CHANNEL,
                cc::CTRL_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX,
                &mut params,
            )
        }
        Stage::Token => {
            let mut params = cc::token_params();
            control(io, &h, cc::H_CHANNEL, cc::CTRL_GET_WORK_SUBMIT_TOKEN, &mut params)?;
            let t = cc::parse_token(&params).ok_or(Fail::new(FailKind::Parse, 0x63))?;
            p.token = t.0;
            TOKEN.store(t.0, Ordering::Relaxed);
            RUNLIST.store(t.runlist(), Ordering::Relaxed);
            Ok(())
        }
        Stage::Schedule => {
            let mut params = cc::schedule_params(true);
            control(io, &h, cc::H_TSG, cc::CTRL_GPFIFO_SCHEDULE, &mut params)
        }
        Stage::FirstPush => first_push(io.passive, p, ring),
    }
}

fn gen_engine(p: &Parts) -> Result<(Gen, cc::CeCaps), Fail> {
    match (p.gen, p.engine) {
        (Some(g), Some(e)) => Ok((g, e)),
        _ => Err(Fail::new(FailKind::Parse, 0x62)),
    }
}

/// The client: the ring client's bring-up machine (`rm_client::Client`), driven to the end of its
/// eleven steps and then dropped, as `sysmem.rs` does; only the device is allocated with the tool's
/// parameters (`hClientShare`, 64 KiB big pages, `OPTIONAL_MULTIPLE_VASPACES`). A failure closes
/// what was opened, on the undo's allowance.
#[inline(never)]
fn client(io: &Io<'_>) -> Result<Handles, Fail> {
    let mut c = Client::new();
    c.sync_epoch(io.epoch);
    // A surface extent only so the machine leaves `Cold`; its surface steps are never run.
    let want = Want {
        level: 1,
        surface: Some((64, 64)),
    };
    for _ in 0..CLIENT_STEPS {
        if c.bring_up_done() {
            break;
        }
        let Action::Step(step) = c.next(want) else {
            break;
        };
        let result = if step == Step::AllocDevice {
            alloc_device(io, &c)
        } else {
            io.perform(step, &c, want)
        };
        c.finish(step, result);
        if c.is_dead() {
            break;
        }
    }
    let failed = c.failure().map(|f| f.fail);
    if failed.is_some() || !c.bring_up_done() {
        let uio = io.with_limit(Some(budget_ms(cc::UNDO_BUDGET_MS)));
        for &h in c.take_cleanup().as_slice() {
            match uio.try_close(h) {
                Ok(_) => {}
                Err(f) if f.kind == FailKind::Transport => break,
                Err(_) => {
                    CH_SOFT.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        return Err(failed.unwrap_or(Fail::new(FailKind::Parse, 0xfc)));
    }
    Ok(Handles {
        ctl: c.ctl(),
        gpu: c.gpu(),
        drm: c.drm(),
        root: c.root(),
        minor: c.minor(),
    })
}

#[inline(never)]
fn alloc_device(io: &Io<'_>, c: &Client) -> Result<Out, Fail> {
    let params = cc::device_params(c.root());
    let mut resp = [0u8; super::REPLY_MAX];
    io.rm_alloc(
        c.ctl(),
        c.root(),
        c.root(),
        rc::H_DEVICE,
        rc::NV01_DEVICE_0,
        &params,
        &mut resp,
    )
    .map(|_| Out::Unit)
}

/// `GET_CLASSLIST_V2` on the device: the generation, and its usermode class.
#[inline(never)]
fn class_list(io: &Io<'_>, p: &mut Parts) -> Result<(), Fail> {
    let mut params = heap(cc::CLASSLIST_BYTES)?;
    control(io, &p.h, rc::H_DEVICE, cc::CTRL_GET_CLASSLIST_V2, &mut params)?;
    let gen = cc::gen_from_class_list(&params).ok_or(Fail::new(FailKind::Layout, 0x60))?;
    p.gen = Some(gen);
    p.usermode_class = gen.classes().usermode;
    GEN.store(cc::gen_word(gen), Ordering::Relaxed);
    Ok(())
}

/// `GET_ENGINES_V2`, then `CE_GET_CAPS_V2` for every copy engine it lists (a query RM refuses is
/// skipped, as the tool does), then [`cc::pick_engine`].
#[inline(never)]
fn engines(io: &Io<'_>, p: &mut Parts) -> Result<(), Fail> {
    let mut list = heap(cc::ENGINES_BYTES)?;
    control(io, &p.h, rc::H_SUBDEVICE, cc::CTRL_GET_ENGINES_V2, &mut list)?;
    let mut types = [0u32; cc::MAX_CAPS_QUERIES];
    let n = cc::copy_engines(&list, &mut types);
    let mut caps = [cc::CeCaps {
        engine_type: 0,
        caps: [0, 0],
    }; cc::MAX_CAPS_QUERIES];
    let mut k = 0;
    for &t in types.get(..n).unwrap_or(&[]) {
        let mut params = cc::ce_caps_params(t);
        match control(io, &p.h, rc::H_SUBDEVICE, cc::CTRL_CE_GET_CAPS_V2, &mut params) {
            Ok(()) => {
                if let Some(c) = cc::parse_ce_caps(&params, t) {
                    caps[k] = c;
                    k += 1;
                }
            }
            Err(f) if f.kind == FailKind::Transport => return Err(f),
            Err(_) => note_rm_error(),
        }
    }
    let pick = cc::pick_engine(caps.get(..k).unwrap_or(&[])).ok_or(Fail::new(FailKind::Layout, 0x61))?;
    p.engine = Some(pick);
    ENGINE.store(pick.engine_type, Ordering::Relaxed);
    CAPS.store(pick.word(), Ordering::Relaxed);
    Ok(())
}

/// The GPFIFO channel under the TSG (376-byte parameters).
#[inline(never)]
fn channel(io: &Io<'_>, p: &Parts) -> Result<(), Fail> {
    let (gen, engine) = gen_engine(p)?;
    let params = cc::channel_params(&cc::ChannelDesc {
        h_ctl: cc::H_CTL,
        gpfifo_va: p.ring_gpu.va + cc::GPFIFO_OFFSET,
        entries: cc::GPFIFO_ENTRIES,
        h_ctxshare: cc::H_CTXSHARE,
        userd_offset: cc::USERD_OFFSET,
        engine_type: engine.engine_type,
    });
    alloc(io, &p.h, cc::H_TSG, cc::H_CHANNEL, gen.classes().gpfifo, &params)
}

/// `SET_OBJECT` of the copy class and a WFI release of the first completion value, kicked; the
/// channel is alive when the value lands within [`cc::FIRST_PUSH_MS`] and the error notifier is 0.
#[inline(never)]
fn first_push(passive: PassiveLevel, p: &Parts, ring: &mut cp::Ring) -> Result<(), Fail> {
    let (gen, _) = gen_engine(p)?;
    let mut buf = [0u32; cc::SLOT_DWORDS];
    let mut push = cp::Push::new(&mut buf);
    let value = ring.submitted() + 1;
    cp::set_object(&mut push, gen).map_err(|_| Fail::new(FailKind::Layout, 0x64))?;
    cp::release(
        &mut push,
        cp::Release {
            va: p.ring_gpu.va + cc::COMPLETION_OFFSET,
            value,
            wfi: true,
            timestamp: false,
            interrupt: false,
        },
    )
    .map_err(|_| Fail::new(FailKind::Layout, 0x64))?;
    let n = push.len();
    kick(p, ring, buf.get(..n).unwrap_or(&[])).map_err(|_| Fail::new(FailKind::Layout, 0x65))?;
    let deadline = now_100ns() + cc::FIRST_PUSH_MS * UNITS_PER_MS;
    let landed = wait_value(passive, p, value, deadline);
    let status = notifier_status(p);
    if status != 0 {
        NOTIFY.store(u32::from(status), Ordering::Relaxed);
        return Err(Fail::new(FailKind::Rm, 0x1_0000 | u32::from(status)));
    }
    if !landed {
        return Err(Fail::new(FailKind::Transport, 0xE3));
    }
    ring.observe(value);
    Ok(())
}

// ---- submission and completion ------------------------------------------------------------------

/// Why [`submit`] did not submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubmitError {
    /// No channel up (or it is broken).
    NoChannel,
    RingFull,
    Push(cp::PushError),
    Entry,
}

/// Submit one copy: acquire `producer`, copy `copy`, release the next completion value with WFI.
/// Returns that value (the completion token: the copy is in the destination once the completion
/// value reaches it, [`poll`]). Under the `STATE` lock: plain stores only (the push, the GPFIFO
/// entry, `GP_PUT`, the doorbell), no RM call; any IRQL up to DISPATCH.
pub(super) fn submit(producer: cp::Acquire, copy: &cp::CopyRect) -> Result<u64, SubmitError> {
    let mut g = STATE.lock();
    if g.svc.phase() != cc::Phase::Ready {
        return Err(SubmitError::NoChannel);
    }
    let Some(p) = g.parts else {
        return Err(SubmitError::NoChannel);
    };
    let gen = p.gen.ok_or(SubmitError::NoChannel)?;
    if g.ring.is_full() {
        return Err(SubmitError::RingFull);
    }
    let value = g.ring.submitted() + 1;
    let mut buf = [0u32; cc::SLOT_DWORDS];
    let mut push = cp::Push::new(&mut buf);
    cp::present_push(
        &mut push,
        gen,
        producer,
        copy,
        cp::Release {
            va: p.ring_gpu.va + cc::COMPLETION_OFFSET,
            value,
            wfi: true,
            timestamp: false,
            interrupt: false,
        },
    )
    .map_err(SubmitError::Push)?;
    let n = push.len();
    kick(&p, &mut g.ring, buf.get(..n).unwrap_or(&[]))
}

/// Write `words` into the next slot and kick it: the entry, a full barrier (the write-combined
/// stores drain), `GP_PUT`, a full barrier, the token to the doorbell (the tool's `kick`).
fn kick(p: &Parts, ring: &mut cp::Ring, words: &[u32]) -> Result<u64, SubmitError> {
    if words.is_empty() || words.len() > cc::SLOT_DWORDS {
        return Err(SubmitError::Entry);
    }
    if p.ring.va == 0 || p.ctl.va == 0 || p.um.va == 0 {
        return Err(SubmitError::NoChannel);
    }
    // The entry is checked before the ring counts the submission.
    let index = ring.put();
    let push_va = cc::slot_va(p.ring_gpu.va, index);
    cp::gp_entry(push_va, words.len() as u32).map_err(|_| SubmitError::Entry)?;
    let slot = ring.submit().ok_or(SubmitError::RingFull)?;
    let k = cp::kick(slot, push_va, words.len() as u32, cp::Token(p.token))
        .map_err(|_| SubmitError::Entry)?;
    let base = cc::slot_offset(slot.index);
    for (i, w) in words.iter().enumerate() {
        // SAFETY: the slot is `SLOT_BYTES` inside the ring's kernel view (`slot_offset` + at most
        // `SLOT_DWORDS` words, checked above), mapped while the channel is in `STATE`, which the
        // caller holds; the GPU reads this slot only after the entry below names it.
        unsafe { wr32(p.ring.va, base + 4 * i as u64, *w) };
    }
    // SAFETY: the entry is inside the GPFIFO at the start of the ring's view (`entries` * 8 bytes),
    // 8-aligned.
    unsafe { wr64(p.ring.va, cc::GPFIFO_OFFSET + k.entry_offset, k.entry) };
    full_barrier();
    // SAFETY: USERD at `USERD_OFFSET` of the control view, `GP_PUT` 4 bytes at 0x8c inside it.
    unsafe { wr32(p.ctl.va, cc::USERD_OFFSET + u64::from(k.userd_offset), k.put) };
    full_barrier();
    // SAFETY: the doorbell page (64 KiB, uncached), `NOTIFY_CHANNEL_PENDING` at 0x90.
    unsafe { wr32(p.um.va, u64::from(k.doorbell_offset), k.token) };
    SUBMIT.fetch_add(1, Ordering::Relaxed);
    Ok(slot.value)
}

/// What [`poll`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Progress {
    /// The completion value (the ring's watermark after this read).
    pub completed: u64,
    pub submitted: u64,
    /// The error notifier's status (0: healthy).
    pub notifier: u16,
}

/// Read the completion value and the error notifier through the kernel views; advance the ring.
/// A set notifier breaks the channel (counted once): nothing is submitted until it is torn down.
/// `None`: no channel. Under the `STATE` lock, plain loads; any IRQL up to DISPATCH.
pub(super) fn poll() -> Option<Progress> {
    let mut g = STATE.lock();
    let p = g.parts?;
    // SAFETY: the completion value is 8 bytes at `COMPLETION_OFFSET` of the ring's view, mapped
    // while the channel is in `STATE` (held).
    let value = unsafe { rd64(p.ring.va, cc::COMPLETION_OFFSET) };
    g.ring.observe(value);
    let notifier = notifier_status(&p);
    if notifier != 0 && g.svc.phase() == cc::Phase::Ready {
        g.svc.on_channel_error();
        NOTIFY.store(u32::from(notifier), Ordering::Relaxed);
        CHAN_FAIL.fetch_add(1, Ordering::Relaxed);
    }
    Some(Progress {
        completed: g.ring.completed(),
        submitted: g.ring.submitted(),
        notifier,
    })
}

/// Set the self-test's producer value (the stand-in for a producer's semaphore, in the ring's
/// semaphore page). `false`: no channel.
pub(super) fn set_producer(value: u64) -> bool {
    let g = STATE.lock();
    let Some(p) = g.parts else {
        return false;
    };
    // SAFETY: 8 bytes at `PRODUCER_OFFSET` of the ring's view, mapped while held.
    unsafe { wr64(p.ring.va, cc::PRODUCER_OFFSET, value) };
    full_barrier();
    true
}

/// The GPU VA of the self-test's producer value, and the generation (`None`: no channel).
pub(super) fn producer_va() -> Option<u64> {
    let g = STATE.lock();
    g.parts.map(|p| p.ring_gpu.va + cc::PRODUCER_OFFSET)
}

/// The channel's client, for the self-test's own RM calls.
pub(super) fn handles() -> Option<Handles> {
    STATE.lock().parts.map(|p| p.h)
}

/// The channel's class generation (`None`: no channel).
pub(super) fn gen() -> Option<Gen> {
    STATE.lock().parts.and_then(|p| p.gen)
}

fn notifier_status(p: &Parts) -> u16 {
    if p.ctl.va == 0 {
        return 0;
    }
    // SAFETY: the error notifier's `status` (2 bytes at 14) of the control view, mapped while the
    // caller holds the parts.
    unsafe { ((p.ctl.va + cc::NOTIFIER_OFFSET + cc::NOTIFIER_STATUS_AT) as *const u16).read_volatile() }
}

/// Spin (then sleep in ticks) until the completion value of `p`'s ring reaches `value` or the
/// interrupt-time `deadline`. PASSIVE (it sleeps). Used before the channel is in `STATE` (the
/// first push) and by a teardown that has taken it out.
fn wait_value(passive: PassiveLevel, p: &Parts, value: u64, deadline_100ns: u64) -> bool {
    let spin_until = now_100ns() + SPIN_100NS;
    loop {
        // SAFETY: as in `poll`; the caller owns `p` (not yet, or no longer, in `STATE`).
        if unsafe { rd64(p.ring.va, cc::COMPLETION_OFFSET) } >= value {
            return true;
        }
        let now = now_100ns();
        if now >= deadline_100ns {
            return false;
        }
        if now < spin_until {
            core::hint::spin_loop();
        } else {
            crate::virtio::ctrl::sleep_ms(passive, 1);
        }
    }
}

/// How long a wait spins before it sleeps: a copy of a 1600x900 frame is about 0.3 ms.
pub(super) const SPIN_100NS: u64 = 20 * UNITS_PER_MS;

// ---- teardown -------------------------------------------------------------------------------------

/// Tear the channel down (the tool's order: [`cc::next_undo`]), on `limit`. The channel leaves
/// `STATE` first (no submitter can reach its views any more); every acquire it could wait on is
/// released (the self-test's producer far ahead) and what was submitted is given
/// [`cc::IDLE_WAIT_MS`] to land before the memory it uses is freed. PASSIVE, `IO_BUSY` held (or
/// the worker joined).
#[inline(never)]
pub(super) fn teardown(passive: PassiveLevel, adapter: &AdapterContext, limit: SweepBudget) {
    let taken = {
        let mut g = STATE.lock();
        if !g.svc.begin_teardown() {
            None
        } else {
            let ring = g.ring;
            let epoch = g.svc.epoch();
            g.ring = fresh_ring();
            g.parts.take().map(|p| (p, ring, epoch))
        }
    };
    LIVE.store(0, Ordering::Release);
    let Some((mut parts, mut ring, epoch)) = taken else {
        // Begun with nothing in it (cannot happen: `parts` is set with `Ready`): close the phase.
        STATE.lock().svc.torn_down(now_ms());
        return;
    };
    let io = Io {
        passive,
        adapter,
        epoch,
        limit: Some(limit),
    };
    undo_all(&io, &mut parts, &mut ring);
    STATE.lock().svc.torn_down(now_ms());
    CH_DOWN.fetch_add(1, Ordering::Relaxed);
}

/// Give back everything `p.made` names, in [`cc::next_undo`]'s order. Each step runs once:
/// one that fails is counted (`CeChSoft`), never retried, and what RM may still hold goes with
/// the client's close (or the transport sweep). With StopDevice's flag up or the deadline spent
/// no message is sent, but every kernel view is still unmapped.
fn undo_all(io: &Io<'_>, p: &mut Parts, ring: &mut cp::Ring) {
    // Whether the GPU is done with everything submitted. The producer's memory dup'd for the
    // copies (`ce_dup`, M3c-1) was made after the channel, so it goes first when the GPU is idle;
    // when it is not (a copy stuck on a producer's value the KMD cannot release) it goes right
    // after the channel group, whose free stops the channel.
    let mut idle = true;
    if p.made.contains(Made::SCHEDULED) && p.ring.va != 0 {
        // Nothing may hold the GPU on an acquire, and nothing in flight may still write memory
        // that is about to go: release the producer far ahead, wait (bounded) for the last value.
        // SAFETY: the ring's view is mapped (RING_MAP is made, checked by `va != 0`).
        unsafe { wr64(p.ring.va, cc::PRODUCER_OFFSET, 1u64 << 62) };
        full_barrier();
        let wait_ms = io
            .limit
            .and_then(|b| b.call_timeout_ms(now_100ns()))
            .map_or(0, |ms| ms.min(cc::IDLE_WAIT_MS));
        let deadline = now_100ns() + wait_ms * UNITS_PER_MS;
        if ring.in_flight() != 0 && !wait_value(io.passive, p, ring.submitted(), deadline) {
            CH_SOFT.fetch_add(1, Ordering::Relaxed);
            idle = false;
        }
    }
    let h = p.h;
    if idle {
        super::ce_dup::release_all(io, &h);
    }
    while let Some(u) = cc::next_undo(p.made) {
        let send = !io.stopping() && !io.limit_spent();
        let ok = undo_one(io, p, u, send);
        if !ok {
            CH_SOFT.fetch_add(1, Ordering::Relaxed);
        }
        p.made = p.made.without(u.undoes());
        if matches!(u, Undo::FreeTsg) {
            super::ce_dup::release_all(io, &h);
        }
    }
    // Whatever is left (no channel group was made): before the client's files close (a no-op
    // when the cache is empty). A closed client takes the rest with it.
    super::ce_dup::release_all(io, &h);
}

/// One undo step; `send = false` sends nothing (the kernel view is still unmapped). Whether it
/// was confirmed.
fn undo_one(io: &Io<'_>, p: &mut Parts, u: Undo, send: bool) -> bool {
    let h = p.h;
    match u {
        Undo::UnmapRingCpu => cpu_unmap(io, &h, &mut p.ring, send),
        Undo::UnmapCtl => cpu_unmap(io, &h, &mut p.ctl, send),
        Undo::UnmapUsermode => cpu_unmap(io, &h, &mut p.um, send),
        _ if !send => false,
        Undo::ScheduleOff => {
            let mut params = cc::schedule_params(false);
            control(io, &h, cc::H_TSG, cc::CTRL_GPFIFO_SCHEDULE, &mut params).is_ok()
        }
        // Frees the subcontext, the channel and the copy object with it.
        Undo::FreeTsg => rm_free(io, &h, rc::H_DEVICE, cc::H_TSG).is_ok(),
        Undo::UnmapRingGpu => gpu_unmap(io, &h, &p.ring_gpu),
        Undo::FreeRing => rm_free(io, &h, rc::H_DEVICE, cc::H_RING).is_ok(),
        Undo::FreeCtl => rm_free(io, &h, rc::H_DEVICE, cc::H_CTL).is_ok(),
        Undo::FreeUsermode => rm_free(io, &h, rc::H_SUBDEVICE, cc::H_USERMODE).is_ok(),
        Undo::FreeVaSpace => rm_free(io, &h, rc::H_DEVICE, cc::H_VASPACE).is_ok(),
        Undo::CloseClient => {
            // The DRM and GPU files first; the control file's close frees the RM client.
            let mut ok = true;
            for f in [h.drm, h.gpu, h.ctl] {
                if f == 0 {
                    continue;
                }
                match io.try_close(f) {
                    Ok(true) => {}
                    Ok(false) => ok = false,
                    Err(e) => {
                        ok = false;
                        if e.kind == FailKind::Transport {
                            break;
                        }
                    }
                }
            }
            ok
        }
    }
}

/// StopDevice (after the worker was joined) and StartDevice (before the old transport is
/// retired): free the channel while the transport still answers, on the caller's budget. One
/// relaxed load when there is none (always, with `RmCopyEngine` 0).
#[inline(never)]
pub(crate) fn retire_for_stop(passive: PassiveLevel, adapter: &AdapterContext, budget: &SweepBudget) {
    if LIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    if IO_BUSY
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // A worker that was not joined (a start without a stop) is inside the channel's I/O: the
        // transport sweep closes the client, `drop_views` is skipped too, `forget` keeps nothing.
        CH_SOFT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    teardown(passive, adapter, *budget);
    IO_BUSY.store(0, Ordering::Release);
    publish_counters();
}

/// The transport is about to be retired (`rm_client::retire_begin`): a channel still in `STATE`
/// (StopDevice's teardown ran out of budget, or never ran) has its kernel views unmapped now, so
/// no virtual address outlives the window range the host is about to release. No message is sent:
/// the sweep closes the client. One relaxed load when there is none.
#[inline(never)]
pub(crate) fn drop_views() {
    if LIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    if IO_BUSY
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let parts = {
        let mut g = STATE.lock();
        g.svc.reset();
        g.ring = fresh_ring();
        g.parts.take()
    };
    LIVE.store(0, Ordering::Release);
    if let Some(p) = parts {
        for v in [p.ring, p.ctl, p.um] {
            kernel_unmap(v.va, v.len);
        }
    }
    IO_BUSY.store(0, Ordering::Release);
}

/// The transport is gone (`rm_client::forget`): forget everything (the sweep closed the client).
#[inline(never)]
pub(crate) fn forget() {
    drop_views();
    // The dup cache names objects of the client the sweep closed.
    super::ce_dup::forget();
    {
        let mut g = STATE.lock();
        g.svc.reset();
        g.ring = fresh_ring();
        // A channel `drop_views` could not take (a worker inside its I/O) stays mapped: leaked, not
        // unmapped under that worker.
        if g.parts.take().is_some() {
            CH_SOFT.fetch_add(1, Ordering::Relaxed);
        }
    }
    LIVE.store(0, Ordering::Release);
    SELF_DONE.store(0, Ordering::Relaxed);
}

// ---- the RM messages ------------------------------------------------------------------------------

/// A zeroed heap buffer (a parameter block too large for a worker's frame).
pub(super) fn heap(len: usize) -> Result<Vec<u8>, Fail> {
    let mut v = Vec::new();
    if v.try_reserve_exact(len).is_err() {
        return Err(Fail::new(FailKind::Os, 0x60));
    }
    v.resize(len, 0);
    Ok(v)
}

/// `NV_ESC_RM_ALLOC` of `class` as `h_new` under `parent`, with `params`.
#[inline(never)]
pub(super) fn alloc(
    io: &Io<'_>,
    h: &Handles,
    parent: u32,
    h_new: u32,
    class: u32,
    params: &[u8],
) -> Result<(), Fail> {
    let mut resp = heap(rc::REPLY_DATA + rc::NVOS64_BYTES + params.len() + 64)?;
    noted(
        io.rm_alloc(h.ctl, h.root, parent, h_new, class, params, &mut resp)
            .map(|_| ()),
        rc::ESC_RM_ALLOC,
        class,
    )
}

/// `NV_ESC_RM_ALLOC` of RM system memory (`NV01_MEMORY_SYSTEM`, `rm_sysmem::params`), as `h_mem`
/// under the device: cached by default as the tool's (`RmCeCache` 1: write-combined).
#[inline(never)]
pub(super) fn alloc_sys(io: &Io<'_>, h: &Handles, h_mem: u32, size: u64) -> Result<(), Fail> {
    let params = rs::params(h.root, cache_mode().sysmem(), size);
    alloc(io, h, rc::H_DEVICE, h_mem, cc::NV01_MEMORY_SYSTEM, &params)
}

/// `NV_ESC_RM_FREE` of `h_obj` under `parent`.
#[inline(never)]
pub(super) fn rm_free(io: &Io<'_>, h: &Handles, parent: u32, h_obj: u32) -> Result<(), Fail> {
    let block = rc::nvos00(h.root, parent, h_obj);
    let mut resp = [0u8; super::REPLY_MAX];
    let r = io
        .exchange(h.ctl, rc::nv_cmd(rc::ESC_RM_FREE, 16), &block, &[], &mut resp)
        .and_then(|n| {
            rc::rm_reply(
                resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x66))?,
                rc::NVOS00_STATUS_AT,
            )
            .map(|_| ())
            .map_err(Fail::from)
        });
    noted(r, rc::ESC_RM_FREE, h_obj)
}

/// `NV_ESC_RM_CONTROL` of `cmd` on `object` with `params` (in and out: RM's answer is copied
/// back).
#[inline(never)]
pub(super) fn control(
    io: &Io<'_>,
    h: &Handles,
    object: u32,
    cmd: u32,
    params: &mut [u8],
) -> Result<(), Fail> {
    noted(control_inner(io, h, object, cmd, params), rc::ESC_RM_CONTROL, cmd)
}

fn control_inner(
    io: &Io<'_>,
    h: &Handles,
    object: u32,
    cmd: u32,
    params: &mut [u8],
) -> Result<(), Fail> {
    let block = rc::nvos54(h.root, object, cmd, params.len() as u32);
    let mut resp = heap(rc::REPLY_DATA + rc::NVOS54_BYTES + params.len() + 64)?;
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(rc::ESC_RM_CONTROL, rc::NVOS54_BYTES as u32),
        &block,
        params,
        &mut resp,
    )?;
    let reply = rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x67))?,
        rc::NVOS54_STATUS_AT,
    )
    .map_err(Fail::from)?;
    let k = reply.nested.len().min(params.len());
    params[..k].copy_from_slice(&reply.nested[..k]);
    Ok(())
}

// ---- GPU mappings ---------------------------------------------------------------------------------

/// `crm_map_dma2` into the channel's VA space: an `NV50_MEMORY_VIRTUAL` at the fixed `va` (RM's
/// choice when RM refuses the fixed range, as the tool's `gpu_map_kind`), then `MAP_MEMORY_DMA`
/// snooped with 4 KiB pages. The whole mapping must lie below 2^40. A failure gives back what it
/// made.
#[inline(never)]
pub(super) fn gpu_map(
    io: &Io<'_>,
    h: &Handles,
    virt: u32,
    mem: u32,
    va: u64,
    len: u64,
) -> Result<GpuMap, Fail> {
    gpu_map_with(io, h, virt, mem, va, len, cc::MAP_FLAGS_SYSMEM, None)
}

/// [`gpu_map`] with the `NVOS46` flags and, when `kind` is `Some`, the PTE kind
/// (`kindOverride`; the caller sets `MAP_FLAGS_KIND_OVERRIDE` in `flags`): the dup'd producer
/// memory of M3c-1 (`ce_dup`).
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(super) fn gpu_map_with(
    io: &Io<'_>,
    h: &Handles,
    virt: u32,
    mem: u32,
    va: u64,
    len: u64,
    flags: u32,
    kind: Option<u32>,
) -> Result<GpuMap, Fail> {
    let fixed = cc::virtual_params(h.root, cc::H_VASPACE, Some(va), len);
    match alloc(io, h, rc::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &fixed) {
        Ok(()) => {}
        Err(f) if f.kind == FailKind::Rm => {
            let any = cc::virtual_params(h.root, cc::H_VASPACE, None, len);
            alloc(io, h, rc::H_DEVICE, virt, cc::NV50_MEMORY_VIRTUAL, &any)?;
        }
        Err(f) => return Err(f),
    }
    let m = cc::DmaMap {
        root: h.root,
        h_device: rc::H_DEVICE,
        h_dma: virt,
        h_memory: mem,
        length: len,
        flags,
    };
    let mapped = noted(map_dma(io, h, &m, kind), cc::ESC_RM_MAP_MEMORY_DMA, mem);
    let got = match mapped {
        Ok(got) => got,
        Err(f) => {
            if rm_free(io, h, rc::H_DEVICE, virt).is_err() {
                CH_SOFT.fetch_add(1, Ordering::Relaxed);
            }
            return Err(f);
        }
    };
    let g = GpuMap {
        virt,
        mem,
        va: got,
        len,
    };
    if got.checked_add(len).map_or(true, |end| end > cp::MAX_VA) {
        gpu_unmap(io, h, &g);
        return Err(Fail::new(FailKind::Layout, 0x68));
    }
    Ok(g)
}

#[inline(never)]
fn map_dma(io: &Io<'_>, h: &Handles, m: &cc::DmaMap, kind: Option<u32>) -> Result<u64, Fail> {
    let block = match kind {
        Some(k) => cc::nvos46_kind(m, k),
        None => cc::nvos46(m),
    };
    let mut resp = [0u8; super::REPLY_MAX];
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(cc::ESC_RM_MAP_MEMORY_DMA, cc::NVOS46_BYTES as u32),
        &block,
        &[],
        &mut resp,
    )?;
    let reply = rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x69))?,
        cc::NVOS46_STATUS_AT,
    )
    .map_err(Fail::from)?;
    cc::map_dma_va(reply.data).ok_or(Fail::new(FailKind::Parse, 0x69))
}

/// `UNMAP_MEMORY_DMA` of the whole mapping, then the free of its virtual allocation. Whether both
/// were confirmed.
#[inline(never)]
pub(super) fn gpu_unmap(io: &Io<'_>, h: &Handles, g: &GpuMap) -> bool {
    let m = cc::DmaMap {
        root: h.root,
        h_device: rc::H_DEVICE,
        h_dma: g.virt,
        h_memory: g.mem,
        length: g.len,
        flags: 0,
    };
    let block = cc::nvos47(&m, g.va);
    let mut resp = [0u8; super::REPLY_MAX];
    let unmapped = io
        .exchange(
            h.ctl,
            rc::nv_cmd(cc::ESC_RM_UNMAP_MEMORY_DMA, cc::NVOS47_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )
        .ok()
        .and_then(|n| resp.get(..n))
        .is_some_and(|r| rc::rm_reply(r, cc::NVOS47_STATUS_AT).is_ok());
    if !unmapped {
        note_call(cc::ESC_RM_UNMAP_MEMORY_DMA, g.mem, Fail::new(FailKind::Parse, 0x6d));
    }
    let freed = rm_free(io, h, rc::H_DEVICE, g.virt).is_ok();
    unmapped && freed
}

// ---- CPU views ------------------------------------------------------------------------------------

/// System memory's map kind (`MapNode::for_class(NV01_MEMORY_SYSTEM)`: a control file).
pub(super) const SYSMEM: cc::MapNode = cc::MapNode::for_class(cc::NV01_MEMORY_SYSTEM);

/// Map `len` bytes of `mem` (under `parent`) for the CPU, armed on a fresh file of kind `node`
/// (system memory: a control file; BAR memory: a GPU file, as librmclient chooses). RM's
/// `NV_ERR_INVALID_ARGUMENT` (the wrong kind) is retried once on the other kind, as librmclient
/// does. A failure gives back what it made, and is named in `CeRmCall` / `CeRmStat` / `CeMapNode`.
#[inline(never)]
pub(super) fn cpu_map(
    io: &Io<'_>,
    h: &Handles,
    parent: u32,
    mem: u32,
    node: cc::MapNode,
    len: u64,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<CpuView, Fail> {
    let mut node = node;
    let mut retried = false;
    loop {
        let mut v = CpuView {
            len,
            parent,
            mem,
            node_dev: match node {
                cc::MapNode::Ctl => rc::DEV_CTL,
                cc::MapNode::Gpu => h.minor,
            },
            ..CpuView::default()
        };
        v.map_ch = noted(io.open_file(v.node_dev), CALL_OPEN, v.node_dev)?;
        let r = cpu_map_steps(io, h, &mut v, node, cache);
        let Err(f) = r else {
            return Ok(v);
        };
        MAP_NODE.store(cc::map_node_word(node, v.map_ch), Ordering::Relaxed);
        // Partial: give back in reverse (no kernel view was made, or it failed).
        if !cpu_unmap(io, h, &mut v, true) {
            CH_SOFT.fetch_add(1, Ordering::Relaxed);
        }
        if cc::retry_other_node(f, retried) {
            retried = true;
            node = node.other();
            continue;
        }
        return Err(f);
    }
}

fn cpu_map_steps(
    io: &Io<'_>,
    h: &Handles,
    v: &mut CpuView,
    node: cc::MapNode,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<(), Fail> {
    // A GPU file is tied to the control file (`REGISTER_FD`); a control file is not
    // (librmclient registers only a GPU-minor map file).
    if node == cc::MapNode::Gpu {
        io.register_fd(v.map_ch, h.ctl)?;
    }
    // `NV_ESC_RM_MAP_MEMORY` with the map file's handle, on the client's control file.
    let block = rc::nvos33_with_fd(h.root, v.parent, v.mem, 0, v.len, v.map_ch);
    let mut resp = [0u8; super::REPLY_MAX];
    let mapped = io
        .exchange(
            h.ctl,
            rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )
        .and_then(|n| {
            let reply = rc::rm_reply(
                resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x6a))?,
                rc::NVOS33_STATUS_AT,
            )
            .map_err(Fail::from)?;
            rc::map_cookie(&reply).ok_or(Fail::new(FailKind::Parse, 0x6a))
        });
    let cookie = noted(mapped, rc::ESC_RM_MAP_MEMORY, v.mem)?;
    // A nonzero cookie marks "RM mapped it" for the undo.
    v.cookie = cookie | MAPPED;
    let Some(timeout_ms) = io.message_timeout_ms() else {
        return Err(Fail::new(FailKind::Transport, 0xE1));
    };
    let m = match nvrm::host_mmap_within(
        io.passive, io.adapter, KMD, v.map_ch, true, 0, v.len, timeout_ms,
    ) {
        Ok(m) => m,
        Err(e) => {
            let f = match e {
                MapRefusal::Host(errno) => Fail::new(FailKind::Host, errno.unsigned_abs()),
                MapRefusal::Transport(e) => fail_of(Refusal::Transport(e)),
                MapRefusal::NotOwned => Fail::new(FailKind::Refused, 2),
                MapRefusal::BadRange => Fail::new(FailKind::Refused, 5),
                MapRefusal::NoResources => Fail::new(FailKind::Refused, 3),
            };
            note_call(CALL_HOST_MMAP, v.mem, f);
            return Err(f);
        }
    };
    v.host_id = m.host_id;
    v.host_mapped = true;
    if m.size < v.len {
        return Err(Fail::new(FailKind::Layout, 0x6b));
    }
    v.va = noted(
        kernel_map(io.adapter, v.node_dev, m.offset, v.len, cache),
        CALL_KERNEL_MAP,
        v.mem,
    )?;
    Ok(())
}

/// High bit of `CpuView::cookie`: RM mapped it (the cookie itself may be 0).
const MAPPED: u64 = 1 << 63;

/// `MmMapIoSpace` of `[off, off + size)` of the RM window (the region of the map file's device
/// type: the RM window for a control file and for a GPU minor alike).
fn kernel_map(
    adapter: &AdapterContext,
    node_dev: u32,
    off: u64,
    size: u64,
    cache: _MEMORY_CACHING_TYPE::Type,
) -> Result<u64, Fail> {
    let region = nvrm::region_for(adapter, node_dev).ok_or(Fail::new(FailKind::Os, 0x60))?;
    let Some(phys) = helios_kmd_logic::window_units::place(region.base, region.len, off, size)
    else {
        return Err(Fail::new(FailKind::Layout, 0x6c));
    };
    let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
    pa.QuadPart = phys as i64;
    // SAFETY: PASSIVE; `region.base + off .. + size` lies inside the window the host just placed
    // this mapping in (checked by `place`), page aligned.
    let va = unsafe { MmMapIoSpace(pa, size, cache) };
    if va.is_null() {
        return Err(Fail::new(FailKind::Os, 0x61));
    }
    Ok(va as u64)
}

fn kernel_unmap(va: u64, len: u64) {
    if va != 0 {
        // SAFETY: `va`/`len` came from `MmMapIoSpace` in `kernel_map` and were taken out of the
        // view (zeroed by the caller or dropped with it) exactly once; PASSIVE.
        unsafe { MmUnmapIoSpace(va as *mut c_void, len) };
    }
}

/// Undo a CPU view, in reverse: the kernel view, the host's map, `RM_UNMAP_MEMORY`, the map
/// channel. `send = false` only unmaps the kernel view. Whether every message was confirmed.
pub(super) fn cpu_unmap(io: &Io<'_>, h: &Handles, v: &mut CpuView, send: bool) -> bool {
    kernel_unmap(v.va, v.len);
    v.va = 0;
    if !send {
        return v.map_ch == 0;
    }
    let mut ok = true;
    if v.host_mapped {
        let timeout = io.message_timeout_ms().unwrap_or(0).max(1);
        if nvrm::release_host_map_within(io.passive, io.adapter, v.map_ch, v.host_id, timeout).is_err() {
            ok = false;
        }
        v.host_mapped = false;
    }
    if v.cookie & MAPPED != 0 {
        let block = rc::nvos34(h.root, v.parent, v.mem, v.cookie & !MAPPED);
        let mut resp = [0u8; super::REPLY_MAX];
        let done = io
            .exchange(
                h.ctl,
                rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, rc::NVOS34_BYTES as u32),
                &block,
                &[],
                &mut resp,
            )
            .ok()
            .and_then(|n| resp.get(..n))
            .is_some_and(|r| rc::rm_reply(r, rc::NVOS34_STATUS_AT).is_ok());
        if !done {
            note_call(rc::ESC_RM_UNMAP_MEMORY, v.mem, Fail::new(FailKind::Parse, 0x6e));
        }
        ok &= done;
        v.cookie = 0;
    }
    if v.map_ch != 0 {
        ok &= io.close_file(v.map_ch);
        v.map_ch = 0;
    }
    ok
}

// ---- memory access through the kernel views ------------------------------------------------------

/// Zero `len` bytes from the start of a kernel view (8-byte stores; `len` a multiple of 8).
pub(super) fn zero(va: u64, len: u64) {
    if va == 0 {
        return;
    }
    let mut off = 0;
    while off + 8 <= len {
        // SAFETY: inside the view the caller mapped (`len` at most its length).
        unsafe { wr64(va, off, 0) };
        off += 8;
    }
    full_barrier();
}

/// # Safety
/// `base + off .. + 4` lies inside a live kernel view.
pub(super) unsafe fn wr32(base: u64, off: u64, v: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(v) };
}

/// # Safety
/// `base + off .. + 8` lies inside a live kernel view, 8-aligned.
pub(super) unsafe fn wr64(base: u64, off: u64, v: u64) {
    unsafe { ((base + off) as *mut u64).write_volatile(v) };
}

/// # Safety
/// `base + off .. + 4` lies inside a live kernel view.
pub(super) unsafe fn rd32(base: u64, off: u64) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}

/// # Safety
/// `base + off .. + 8` lies inside a live kernel view, 8-aligned.
pub(super) unsafe fn rd64(base: u64, off: u64) -> u64 {
    unsafe { ((base + off) as *const u64).read_volatile() }
}

/// A full barrier: the write-combined stores before it are globally visible before any store
/// after it (the tool's `__atomic_thread_fence(SEQ_CST)`, an `mfence`).
pub(super) fn full_barrier() {
    // SAFETY: SSE2 is baseline on x86_64.
    unsafe { core::arch::x86_64::_mm_mfence() };
}
