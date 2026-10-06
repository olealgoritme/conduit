//! The KMD's own allocations from RM SYSTEM memory (`KmdRmClient` = 5): the service.
//! Design, the decision to create synchronously, the failure matrix and the hardware
//! checklist: `docs/kmd-rm-client.md` section 15. Every decision (which kinds, sizes,
//! cache attribute, the state of the service) is `helios_kmd_logic::rm_sysmem`; this
//! file performs them.
//!
//! A CHILD of [`super`] (`rm_client`) on purpose: it drives the same I/O object ([`Io`]:
//! the forwarded RM messages as the KMD's own owner) and the same bring-up steps, which
//! are private to that file, without widening anything there.
//!
//! WHAT IT DOES. `DxgkDdiCreateAllocation` of the VidPn primary asks [`try_create_primary`]
//! for RM memory before it asks Venus. On the creator's thread, at PASSIVE, it
//!
//! 1. brings the service's own RM client up once per transport generation (eleven
//!    messages; the ring client's machine, `rm_client::Client`, driven to the end of
//!    its bring-up and then dropped: the service keeps only the handles);
//! 2. `RM_ALLOC` of `NV01_MEMORY_SYSTEM`, exports it to a file, `GEM_IMPORT_NVKMS`
//!    on the service's DRM file, closes the export file;
//! 3. creates the Venus-side resource (`RESOURCE_CREATE_BLOB`, `RM_EXPORT`, `USE_MAPPABLE`)
//!    as a foreign resource of the KMD's own owner and marks it sysmem (the one class of
//!    foreign resource the KMD may map);
//! 4. maps it once into the host window and unmaps it again (the TRIAL: the host must
//!    serve the map, and its `map_info` must be the cache attribute the memory was made
//!    with), then lets the WDDM allocation ADOPT the resource.
//!
//! Any failure undoes what was made, counts a strike, and the caller allocates from Venus
//! as it always did.
//!
//! TIME. The creation runs on dxgkrnl's CreateAllocation thread, so everything that waits is
//! bounded by a deadline on the interrupt-time clock, never by a count of sleeps (a
//! `sleep_ms(1)` lasts a timer tick, about 15.6 ms): ONE [`rs::CREATE_BUDGET_MS`] (6 s)
//! deadline covers the wait for another thread's bring-up, the bring-up, and every step
//! after it (each message waits at most what is left, [`TIMEOUT_MS`] at most, and none is
//! sent once it is spent: `Why::Slow`). The undo of a failed creation has its own
//! [`rs::UNDO_BUDGET_MS`] (3 s), so a creation that ran out of time can still give back
//! what it made; a failed creation is therefore over in about 9 s.
//!
//! DestroyAllocation reaches [`released`] through `ctrl::release_allocation_resource` (the
//! one place both release triggers meet): it closes the GEM and frees the RM memory, in
//! that order, after the host resource is gone.
//!
//! LOCKING. `STATE` is a LEAF spinlock over plain data: never held across a host round
//! trip, a wait, an allocation or another lock; every step copies what it needs out and
//! reports back. Creations run concurrently (each owns a reserved slot); only the bring-up
//! is exclusive, and the others wait for it with `sleep_ms`, on the creation's own deadline.
//! Nothing here is called with the scanout lifecycle lock held, so it cannot invert with the
//! programming path (`sysmem_flip::program` takes no lock of this file across I/O either).

use super::{Io, REPLY_MAX, TIMEOUT_MS};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::ctrl;
use crate::virtio::gpu::{AllocAdopt, DeviceOwner, ForeignBegin, ForeignCommit, OwnerFilter};
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::foreign_errno::{classify, Verdict};
use helios_kmd_logic::foreign_resource::{
    foreign_blob_id, validate_request, AdoptRequest, Layout as FrLayout, RefusalKind, FLAG_LAYOUT,
};
use helios_kmd_logic::rm_client::{self as rc, Action, Client, Fail, FailKind, Want};
use helios_kmd_logic::rm_sysmem::{
    self as rs, Admit, AllocOutcome, Cache, CloseWait, FreeOutcome, Kind, PrimaryCache, Svc, Why,
};
use helios_kmd_logic::scanout_release::Wait;
use helios_kmd_logic::sweep_budget::{SweepBudget, UNITS_PER_MS};
use helios_protocol::{HELIOS_BLOB_MEM_RM_EXPORT, VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE};

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

// The pure half mirrors the wire's cache nibble; the two must agree.
const _: () = assert!(rs::MAP_CACHE_CACHED == helios_protocol::VIRTIO_GPU_MAP_CACHE_CACHED);
const _: () = assert!(rs::MAP_CACHE_UNCACHED == helios_protocol::VIRTIO_GPU_MAP_CACHE_UNCACHED);
const _: () = assert!(rs::MAP_CACHE_WC == helios_protocol::VIRTIO_GPU_MAP_CACHE_WC);

/// Longest bring-up: eleven steps plus slack.
const BRING_UP_STEPS: usize = 16;

/// What the service keeps of its RM client after bring-up.
#[derive(Clone, Copy)]
struct Handles {
    ctl: u32,
    gpu: u32,
    drm: u32,
    root: u32,
}

impl Handles {
    const NONE: Handles = Handles {
        ctl: 0,
        gpu: 0,
        drm: 0,
        root: 0,
    };
}

struct State {
    svc: Svc,
    h: Handles,
    cache: PrimaryCache,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    svc: Svc::new(),
    h: Handles::NONE,
    cache: PrimaryCache::WriteCombine,
});

/// Allocations alive, mirrored out of `STATE` so [`released`] costs one load when the
/// service has nothing (every Venus allocation's destroy passes through it).
static LIVE: AtomicU32 = AtomicU32::new(0);

// ---- counters (names at most 14 characters) -------------------------------------------
//
// `RmSysTry` creations asked for, `RmSysOk` made, `RmSysVenus` asked for and given to Venus,
// `RmSysWhy` the last reason (`Why::code`), `RmSysStage` the stage started last (written
// BEFORE it runs, so a hang names itself; the numbers are in `stage`), `RmSysFail` the
// last failure (`stage << 24 | kind << 16 | code`), `RmSysState` the service word
// (`phase << 28 | strikes << 24 | live`), `RmSysLive` / `RmSysFreed` allocations alive /
// released, `RmSysBring` bring-ups done, `RmSysMs` / `RmSysMsMax` the last and longest
// creation (ms), `RmSysTrial` / `RmSysTrialFail` trial maps that worked / failed,
// `RmSysCache` the last `map_info` nibble the host reported, `RmSysMis` creations refused
// because it was not the attribute asked for, `RmSysAlias` creations whose memory and
// dxgkrnl's view of it differ in cache attribute (0 by default: see `PrimaryCache`),
// `RmSysSoft` undo steps that failed, `RmSysLeak` allocations whose RM objects were left to
// the transport sweep, `RmSysRelWait` / `RmSysRelTmo` GEM closes of a REPLACED primary that
// waited for the host's release / went ahead at the 500 ms limit (the release seam, 15.7).
pub static SYS_TRY: AtomicU32 = AtomicU32::new(0);
pub static SYS_OK: AtomicU32 = AtomicU32::new(0);
pub static SYS_VENUS: AtomicU32 = AtomicU32::new(0);
pub static SYS_WHY: AtomicU32 = AtomicU32::new(0);
pub static SYS_STAGE: AtomicU32 = AtomicU32::new(0);
pub static SYS_FAIL: AtomicU32 = AtomicU32::new(0);
pub static SYS_FREED: AtomicU32 = AtomicU32::new(0);
pub static SYS_BRING_UPS: AtomicU32 = AtomicU32::new(0);
pub static SYS_MS: AtomicU32 = AtomicU32::new(0);
pub static SYS_MS_MAX: AtomicU32 = AtomicU32::new(0);
pub static SYS_TRIAL: AtomicU32 = AtomicU32::new(0);
pub static SYS_TRIAL_FAIL: AtomicU32 = AtomicU32::new(0);
pub static SYS_CACHE: AtomicU32 = AtomicU32::new(0);
pub static SYS_MIS: AtomicU32 = AtomicU32::new(0);
pub static SYS_ALIAS: AtomicU32 = AtomicU32::new(0);
pub static SYS_SOFT: AtomicU32 = AtomicU32::new(0);
pub static SYS_LEAK: AtomicU32 = AtomicU32::new(0);
pub static SYS_REL_WAIT: AtomicU32 = AtomicU32::new(0);
pub static SYS_REL_TMO: AtomicU32 = AtomicU32::new(0);

/// The stages, as `RmSysStage` / the first byte of `RmSysFail` name them.
mod stage {
    pub const ADMIT: u32 = 1;
    pub const BRING_UP: u32 = 2;
    pub const ALLOC: u32 = 3;
    pub const OPEN_EXPORT: u32 = 4;
    pub const EXPORT: u32 = 5;
    pub const GEM_IMPORT: u32 = 6;
    pub const CLOSE_EXPORT: u32 = 7;
    pub const IMPORT: u32 = 8;
    pub const MARK: u32 = 9;
    pub const TRIAL: u32 = 10;
    pub const ADOPT: u32 = 11;
    pub const GEM_CLOSE: u32 = 20;
    pub const FREE: u32 = 21;
    pub const UNDO: u32 = 22;
    pub const REL_WAIT: u32 = 23;
}

/// Mirror the counters to the registry. PASSIVE only; nothing is written until the service
/// was asked for something.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if SYS_TRY.load(Ordering::Relaxed) == 0 {
        return;
    }
    let word = {
        let g = STATE.lock();
        state_word(&g.svc)
    };
    rec(b"RmSysTry", SYS_TRY.load(Ordering::Relaxed));
    rec(b"RmSysOk", SYS_OK.load(Ordering::Relaxed));
    rec(b"RmSysVenus", SYS_VENUS.load(Ordering::Relaxed));
    rec(b"RmSysWhy", SYS_WHY.load(Ordering::Relaxed));
    rec(b"RmSysStage", SYS_STAGE.load(Ordering::Relaxed));
    rec(b"RmSysFail", SYS_FAIL.load(Ordering::Relaxed));
    rec(b"RmSysState", word);
    rec(b"RmSysLive", LIVE.load(Ordering::Relaxed));
    rec(b"RmSysFreed", SYS_FREED.load(Ordering::Relaxed));
    rec(b"RmSysBring", SYS_BRING_UPS.load(Ordering::Relaxed));
    rec(b"RmSysMs", SYS_MS.load(Ordering::Relaxed));
    rec(b"RmSysMsMax", SYS_MS_MAX.load(Ordering::Relaxed));
    rec(b"RmSysTrial", SYS_TRIAL.load(Ordering::Relaxed));
    rec(b"RmSysTrialF", SYS_TRIAL_FAIL.load(Ordering::Relaxed));
    rec(b"RmSysCache", SYS_CACHE.load(Ordering::Relaxed));
    rec(b"RmSysMis", SYS_MIS.load(Ordering::Relaxed));
    rec(b"RmSysAlias", SYS_ALIAS.load(Ordering::Relaxed));
    rec(b"RmSysSoft", SYS_SOFT.load(Ordering::Relaxed));
    rec(b"RmSysLeak", SYS_LEAK.load(Ordering::Relaxed));
    rec(b"RmSysRelWait", SYS_REL_WAIT.load(Ordering::Relaxed));
    rec(b"RmSysRelTmo", SYS_REL_TMO.load(Ordering::Relaxed));
    super::sysmem_flip::publish_counters();
    // The Blt CPU-copy fallback (`RmSysBlt*`; 15.17): written once it has run.
    super::sysmem_blt::publish_counters();
}

fn state_word(svc: &Svc) -> u32 {
    use rs::Phase;
    let phase = match svc.phase() {
        Phase::Cold => 0u32,
        Phase::BringingUp => 1,
        Phase::Up => 2,
        Phase::NoNew => 3,
        Phase::Dead => 4,
    };
    (phase << 28) | (u32::from(svc.strikes()) << 24) | svc.live().min(0xff_ffff)
}

fn note_stage(s: u32) {
    SYS_STAGE.store(s, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"RmSysStage", s);
}

fn pack(stage: u32, f: Fail) -> u32 {
    (stage << 24) | ((f.kind as u32) << 16) | (f.code & 0xffff)
}

/// Count and breadcrumb a creation that went to Venus.
fn venus(why: Why) {
    SYS_VENUS.fetch_add(1, Ordering::Relaxed);
    SYS_WHY.store(why.code(), Ordering::Relaxed);
    crate::diag::record_named_bytes(b"RmSysWhy", why.code());
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// The deadline the step in progress shares with the rest of its creation (the undo and the
/// failed-commit cleanup take their own).
fn step_budget(io: &Io<'_>) -> SweepBudget {
    io.limit
        .unwrap_or_else(|| rs::create_budget(now(), TIMEOUT_MS))
}

/// The undo's own allowance, starting now.
fn undo_budget() -> SweepBudget {
    rs::undo_budget(now(), TIMEOUT_MS)
}

// ---- the entry --------------------------------------------------------------------

/// What a successful creation hands the allocation arm: the resource the WDDM allocation
/// has ADOPTED (it is the allocation's, and its destroy releases it), the layout recorded
/// for it and the size recorded (the size VidMm, the aperture check and the blob mapping
/// all use).
#[derive(Clone, Copy)]
pub(crate) struct Created {
    pub resource_id: u32,
    pub layout: FrLayout,
    pub size: u64,
}

/// Whether the primary `resource_id` is created with the `Cached` flag (`KmdRmSysCache` = 2,
/// an opt-in; the default write-combined memory never asks): only for a primary this service
/// made (a UMD-adopted foreign primary is none of its business), read at bring-up, so it is
/// only meaningful once a creation has run. One lock hold.
pub(crate) fn primary_cached_flag(resource_id: u32) -> bool {
    let g = STATE.lock();
    g.cache.cached_flag() && g.svc.find(resource_id).is_some()
}

/// Make the VidPn primary from RM system memory, or say (`None`) that Venus must make it.
/// PASSIVE, on the creator's thread, no lock held. With the knob below 5 this is one atomic
/// load and nothing else.
#[inline(never)]
pub(crate) fn try_create_primary(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    dxgi: u32,
) -> Option<Created> {
    let level = super::knob_level();
    if rs::route(level, Kind::LinearPrimary).is_err() {
        return None;
    }
    SYS_TRY.fetch_add(1, Ordering::Relaxed);
    note_stage(stage::ADMIT);
    let started = now();
    let result = create_primary(passive, adapter, width, height, dxgi);
    let ms = (now().wrapping_sub(started) / UNITS_PER_MS).min(u64::from(u32::MAX)) as u32;
    SYS_MS.store(ms, Ordering::Relaxed);
    SYS_MS_MAX.fetch_max(ms, Ordering::Relaxed);
    let out = match result {
        Ok(c) => {
            SYS_OK.fetch_add(1, Ordering::Relaxed);
            Some(c)
        }
        Err(why) => {
            venus(why);
            None
        }
    };
    publish_counters();
    out
}

#[inline(never)]
fn create_primary(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    dxgi: u32,
) -> Result<Created, Why> {
    if !adapter.display_half() {
        return Err(Why::NoDisplay);
    }
    let epoch = adapter
        .with_virtio(|v| v.nvrm_epoch())
        .map_err(|_| Why::NoTransport)?;
    if epoch == 0 {
        return Err(Why::NoTransport);
    }
    let ctx = adapter.venus_ctx_id();
    if ctx == 0 {
        return Err(Why::NoContext);
    }
    let lay = rs::layout(width, height, dxgi).map_err(|e| e.why())?;
    // One deadline for the whole forward part, the wait for another thread's bring-up
    // and the bring-up included.
    let io = Io {
        passive,
        adapter,
        epoch,
        limit: Some(rs::create_budget(now(), TIMEOUT_MS)),
    };
    let slot = admit(&io)?;
    // From here the slot is ours: every exit commits it or aborts it.
    match build(&io, ctx, slot, &lay) {
        Ok(created) => Ok(created),
        Err((why, memory_may_be_live)) => {
            {
                let mut g = STATE.lock();
                if memory_may_be_live {
                    // RM may still hold the slot's handle: nothing may take the slot again.
                    g.svc.abort_leaked(slot);
                } else {
                    g.svc.abort(slot);
                }
            }
            mirror_live();
            Err(why)
        }
    }
}

/// Take a slot: bring the client up first if this is the first creation of the generation,
/// wait for another thread's bring-up until the creation's deadline.
fn admit(io: &Io<'_>) -> Result<usize, Why> {
    loop {
        let a = STATE.lock().svc.admit(io.epoch);
        match a {
            Admit::Go(slot) => return Ok(slot),
            Admit::Refuse(why) => return Err(why),
            Admit::Wait => {
                // The deadline is the clock's: a sleep lasts a timer tick, not a millisecond.
                if io.limit_spent() {
                    return Err(Why::BringUpBusy);
                }
                ctrl::sleep_ms(io.passive, 1);
            }
            Admit::BringUp => {
                note_stage(stage::BRING_UP);
                let cache = PrimaryCache::from_knob(crate::diag::read_config_dword(
                    crate::diag::knobs::KMD_RM_SYS_CACHE,
                    0,
                ));
                let result = bring_up(io);
                {
                    let mut g = STATE.lock();
                    match &result {
                        Ok(h) => {
                            g.h = *h;
                            g.cache = cache;
                        }
                        Err(_) => g.h = Handles::NONE,
                    }
                    g.svc.bring_up_done(result.is_ok());
                }
                if let Err(f) = result {
                    SYS_FAIL.store(pack(stage::BRING_UP, f), Ordering::Relaxed);
                    crate::diag::record_named_bytes(b"RmSysFail", pack(stage::BRING_UP, f));
                    return Err(Why::BringUp);
                }
                SYS_BRING_UPS.fetch_add(1, Ordering::Relaxed);
                // Ask again: now for a slot.
            }
        }
    }
}

/// The ring client's own bring-up machine, driven to the end of its eleven steps. A
/// failure closes what was opened (nothing depends on it yet) and is the end of the service
/// for the generation.
#[inline(never)]
fn bring_up(io: &Io<'_>) -> Result<Handles, Fail> {
    let mut c = Client::new();
    c.sync_epoch(io.epoch);
    // A surface extent only so the machine leaves `Cold`; its surface steps are never run.
    let want = Want {
        level: 1,
        surface: Some((64, 64)),
    };
    for _ in 0..BRING_UP_STEPS {
        if c.bring_up_done() {
            break;
        }
        let Action::Step(step) = c.next(want) else {
            break;
        };
        let result = io.perform(step, &c, want);
        c.finish(step, result);
        if c.is_dead() {
            break;
        }
    }
    // The files a failed bring-up opened are closed on the undo's allowance: the deadline
    // may be what ended it.
    if let Some(f) = c.failure() {
        close_all(&io.with_limit(Some(undo_budget())), &mut c);
        return Err(f.fail);
    }
    if !c.bring_up_done() {
        close_all(&io.with_limit(Some(undo_budget())), &mut c);
        return Err(Fail::new(FailKind::Parse, 0xfc));
    }
    Ok(Handles {
        ctl: c.ctl(),
        gpu: c.gpu(),
        drm: c.drm(),
        root: c.root(),
    })
}

/// Close the files a dead bring-up opened (the control file's close frees the RM client).
fn close_all(io: &Io<'_>, c: &mut Client) {
    for &h in c.take_cleanup().as_slice() {
        match io.try_close(h) {
            Ok(_) => {}
            // The transport is gone: the sweep closes what is left.
            Err(f) if f.kind == FailKind::Transport => break,
            Err(_) => {
                SYS_SOFT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

// ---- one allocation ---------------------------------------------------------------

/// What a creation has made so far (so an undo knows what to give back).
#[derive(Clone, Copy, Default)]
struct Made {
    /// What is known of the memory's `RM_ALLOC`: made, refused, or asked and not answered.
    mem: AllocOutcome,
    export_ch: u32,
    gem: u32,
    resource: u32,
}

/// `Err((why, memory_may_be_live))`: whether the undo could not make sure RM freed the memory,
/// so the slot's handle may still be RM's ([`rs::mem_may_be_live`]).
#[inline(never)]
fn build(io: &Io<'_>, ctx: u32, slot: usize, lay: &rs::SysLayout) -> Result<Created, (Why, bool)> {
    let (h, cache) = {
        let g = STATE.lock();
        (g.h, g.cache.sysmem())
    };
    let alias = STATE.lock().cache.aliases(adapter_alloc_cached(io.adapter));
    let mut made = Made::default();
    let r = build_steps(io, ctx, slot, lay, &h, cache, &mut made);
    match r {
        Ok(c) => {
            if alias {
                SYS_ALIAS.fetch_add(1, Ordering::Relaxed);
            }
            Ok(c)
        }
        Err((why, st, f)) => {
            let word = pack(st, f);
            SYS_FAIL.store(word, Ordering::Relaxed);
            crate::diag::record_named_bytes(b"RmSysFail", word);
            note_stage(stage::UNDO);
            let live = undo(io, ctx, &h, slot, &made);
            Err((why, live))
        }
    }
}

fn adapter_alloc_cached(adapter: &AdapterContext) -> bool {
    adapter.alloc_cached()
}

type StepErr = (Why, u32, Fail);

fn err(why: Why, st: u32, f: Fail) -> StepErr {
    (why, st, f)
}

#[allow(clippy::too_many_arguments)]
fn build_steps(
    io: &Io<'_>,
    ctx: u32,
    slot: usize,
    lay: &rs::SysLayout,
    h: &Handles,
    cache: Cache,
    made: &mut Made,
) -> Result<Created, StepErr> {
    // The creation's deadline (admit and bring-up have used part of it).
    let late = |st: u32| -> Result<(), StepErr> {
        if io.limit_spent() {
            Err(err(Why::Slow, st, Fail::new(FailKind::Transport, 1)))
        } else {
            Ok(())
        }
    };
    let mem = Svc::handle(slot);

    // 3. the memory. With the deadline spent nothing is sent: the request that is never
    // sent leaves no object, and the undo has nothing to free.
    late(stage::ALLOC)?;
    note_stage(stage::ALLOC);
    let reported = match alloc_sys(io, h, mem, cache, lay.size) {
        Ok(reported) => {
            made.mem = AllocOutcome::Made;
            reported
        }
        Err(f) => {
            // A timeout or an unreadable reply: the host may have allocated it all the
            // same, and the undo must find out (or quarantine the slot).
            made.mem = AllocOutcome::after_failure(f.kind);
            return Err(err(Why::Alloc, stage::ALLOC, f));
        }
    };
    // THE ONE SIZE. The host checks the declared size against the dma-buf's `lseek(SEEK_END)`
    // (larger: ERANGE), and a size RM rounded up (to 64 KiB) passes only if the GEM import's
    // `mem_size` is that same rounded value: this variable goes to `gem_import`, to the
    // layout check, to the import request and reservation and to the resource creation
    // below, and the record keeps it (`docs/kmd-rm-client.md` 15.16).
    let size = rs::adopt_size(lay.size, reported)
        .map_err(|w| err(w, stage::ALLOC, Fail::new(FailKind::Layout, 0x30)))?;
    let fl = rs::foreign_layout(lay, size)
        .ok_or_else(|| err(Why::Size, stage::ALLOC, Fail::new(FailKind::Layout, 0x31)))?;

    // 4-7. export it to a file and import that file as a GEM on the service's DRM file
    late(stage::OPEN_EXPORT)?;
    note_stage(stage::OPEN_EXPORT);
    made.export_ch = io
        .open_file(rc::DEV_CTL)
        .map_err(|f| err(Why::Alloc, stage::OPEN_EXPORT, f))?;
    note_stage(stage::EXPORT);
    export(io, h, mem, made.export_ch).map_err(|f| err(Why::Alloc, stage::EXPORT, f))?;
    note_stage(stage::GEM_IMPORT);
    made.gem = gem_import(io, h, size, made.export_ch)
        .map_err(|f| err(Why::Alloc, stage::GEM_IMPORT, f))?;
    note_stage(stage::CLOSE_EXPORT);
    let ch = made.export_ch;
    // The GEM holds its own reference now; a close the host refuses leaves a file behind
    // and is an undo's business (the file is in `made` until it closes).
    if io.close_file(ch) {
        made.export_ch = 0;
    } else {
        return Err(err(
            Why::Alloc,
            stage::CLOSE_EXPORT,
            Fail::new(FailKind::Host, 0xC1),
        ));
    }

    // 8-9. the Venus resource, marked as RM system memory
    late(stage::IMPORT)?;
    note_stage(stage::IMPORT);
    made.resource = import_resource(io, ctx, h.drm, made.gem, &fl, size)
        .map_err(|f| err(Why::Import, stage::IMPORT, f))?;
    note_stage(stage::MARK);
    let marked = io
        .adapter
        .with_virtio(|v| v.foreign_mark_sysmem(made.resource))
        .unwrap_or(false);
    if !marked {
        return Err(err(
            Why::Import,
            stage::MARK,
            Fail::new(FailKind::Refused, 7),
        ));
    }

    // 10. the trial map
    late(stage::TRIAL)?;
    note_stage(stage::TRIAL);
    trial_map(io, made.resource, cache).map_err(|(why, f)| err(why, stage::TRIAL, f))?;

    // 11. adopt: the WDDM allocation owns the resource from here
    note_stage(stage::ADOPT);
    let request = AdoptRequest {
        declares_foreign: true,
        take_ownership: true,
        ctx_id: ctx,
        width: fl.width,
        height: fl.height,
        pitch: fl.stride,
        plane_offset: 0,
        claimed_alloc_size: 0,
        supplied_layout: Some(fl),
        // The KMD is the creator: there is no creator to promise a trailer to. The
        // trailer is written where the buffer has room (the open path says so when it
        // has none: `FgOpNoRm`).
        trailer_room: true,
        plane_room: true,
    };
    let adopted = io
        .adapter
        .with_virtio(|v| v.adopt_for_allocation(made.resource, &request));
    let (adopted_size, adopted_layout) = match adopted {
        Ok(AllocAdopt::Foreign(a)) => (a.size, a.layout),
        _ => {
            return Err(err(
                Why::Adopt,
                stage::ADOPT,
                Fail::new(FailKind::Refused, 8),
            ))
        }
    };
    // Adopted: from here the resource is the allocation's; the undo path must not release
    // it as the KMD's own.
    let resource = made.resource;
    made.resource = 0;
    {
        let mut g = STATE.lock();
        g.svc.commit(slot, resource, made.gem);
    }
    mirror_live();
    // The mapping and the layout the allocation records are the adopted ones.
    Ok(Created {
        resource_id: resource,
        layout: adopted_layout,
        size: adopted_size,
    })
}

/// Give back what a failed creation made, in the order that keeps every reference valid:
/// the export file, the Venus resource (it names the GEM), the GEM, the memory. A step
/// that fails is counted, never retried; what could not be closed is left to the
/// transport's sweep (it closes the whole KMD owner). It runs on its own allowance
/// ([`rs::UNDO_BUDGET_MS`]), not on what is left of the creation's.
///
/// Returns whether RM may still hold the memory's object ([`rs::mem_may_be_live`]): the free
/// failed, or the allocation's answer never came and the free was not answered either. The
/// caller must then keep the slot, and with it the RM handle `Svc::handle(slot)`, out of
/// circulation (`Svc::abort_leaked`): the next creation on that number would be refused as
/// a duplicate.
#[inline(never)]
fn undo(io: &Io<'_>, ctx: u32, h: &Handles, slot: usize, made: &Made) -> bool {
    let b = undo_budget();
    let uio = io.with_limit(Some(b));
    let mut leaked = false;
    if made.export_ch != 0 && !uio.close_file(made.export_ch) {
        SYS_SOFT.fetch_add(1, Ordering::Relaxed);
        leaked = true;
    }
    if made.resource != 0
        && ctrl::release_blob_for_owner_within(
            uio.passive,
            uio.adapter,
            KMD,
            ctx,
            made.resource,
            Some(&b),
        )
        .is_err()
    {
        SYS_SOFT.fetch_add(1, Ordering::Relaxed);
        leaked = true;
    }
    if made.gem != 0 && gem_close(&uio, h, made.gem).is_err() {
        SYS_SOFT.fetch_add(1, Ordering::Relaxed);
        leaked = true;
    }
    let free = if made.mem == AllocOutcome::NotMade {
        FreeOutcome::NotSent
    } else {
        FreeOutcome::of(free_sys(&uio, h, Svc::handle(slot)))
    };
    if free == FreeOutcome::Failed {
        SYS_SOFT.fetch_add(1, Ordering::Relaxed);
        leaked = true;
    }
    if leaked {
        SYS_LEAK.fetch_add(1, Ordering::Relaxed);
    }
    rs::mem_may_be_live(made.mem, free)
}

// ---- the RM messages ----------------------------------------------------------------

fn reply_fail(e: rc::ReplyError, code: u32) -> Fail {
    match e {
        rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
        rc::ReplyError::Short => Fail::new(FailKind::Parse, code),
    }
}

/// `NV_ESC_RM_ALLOC` of system memory (`NV01_MEMORY_SYSTEM`, `NVOS64`: what `crm_alloc` sends,
/// and what the backend reads the memory's kind back from): the size RM reports.
#[inline(never)]
fn alloc_sys(io: &Io<'_>, h: &Handles, mem: u32, cache: Cache, size: u64) -> Result<u64, Fail> {
    let params = rs::params(h.root, cache, size);
    let mut resp = [0u8; REPLY_MAX];
    let n = io.rm_alloc(
        h.ctl,
        h.root,
        rc::H_DEVICE,
        mem,
        rs::NV01_MEMORY_SYSTEM,
        &params,
        &mut resp,
    )?;
    let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x40))?)
        .map_err(|e| reply_fail(e, 0x40))?;
    Ok(rs::reply_size(reply.nested))
}

#[inline(never)]
fn export(io: &Io<'_>, h: &Handles, mem: u32, export_ch: u32) -> Result<(), Fail> {
    let params = rc::export_params(rc::H_DEVICE, mem, export_ch);
    let block = rc::nvos54(
        h.root,
        h.root,
        rc::NV0000_CTRL_CMD_EXPORT_OBJECT_TO_FD,
        params.len() as u32,
    );
    let mut resp = [0u8; REPLY_MAX];
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(rc::ESC_RM_CONTROL, 32),
        &block,
        &params,
        &mut resp,
    )?;
    rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x41))?,
        rc::NVOS54_STATUS_AT,
    )
    .map(|_| ())
    .map_err(Fail::from)
}

#[inline(never)]
fn gem_import(io: &Io<'_>, h: &Handles, size: u64, export_ch: u32) -> Result<u32, Fail> {
    let data = rc::gem_import_params(size);
    let nested = rc::nvkms_import_params(export_ch);
    let mut resp = [0u8; REPLY_MAX];
    let n = io.exchange(
        h.drm,
        rc::DRM_IOCTL_GEM_IMPORT_NVKMS,
        &data,
        &nested,
        &mut resp,
    )?;
    let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x42))?)
        .map_err(|e| reply_fail(e, 0x42))?;
    rc::gem_handle(&reply).ok_or(Fail::new(FailKind::Parse, 0x43))
}

#[inline(never)]
fn gem_close(io: &Io<'_>, h: &Handles, gem: u32) -> Result<(), Fail> {
    let data = rc::gem_close_params(gem);
    let mut resp = [0u8; REPLY_MAX];
    let n = io.exchange(h.drm, rc::DRM_IOCTL_GEM_CLOSE, &data, &[], &mut resp)?;
    rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x44))?)
        .map(|_| ())
        .map_err(|e| reply_fail(e, 0x44))
}

#[inline(never)]
fn free_sys(io: &Io<'_>, h: &Handles, mem: u32) -> Result<(), Fail> {
    let block = rc::nvos00(h.root, rc::H_DEVICE, mem);
    let mut resp = [0u8; REPLY_MAX];
    let n = io.exchange(
        h.ctl,
        rc::nv_cmd(rc::ESC_RM_FREE, 16),
        &block,
        &[],
        &mut resp,
    )?;
    rc::rm_reply(
        resp.get(..n).ok_or(Fail::new(FailKind::Parse, 0x45))?,
        rc::NVOS00_STATUS_AT,
    )
    .map(|_| ())
    .map_err(Fail::from)
}

// ---- the Venus resource ---------------------------------------------------------------

/// `RESOURCE_CREATE_BLOB` of the GEM as `RM_EXPORT`, under the KMD's own owner and context,
/// with `USE_MAPPABLE`: the backend accepts that flag only for memory it saw RM allocate as
/// system memory (anything else is `EOPNOTSUPP`). The user-mode `IMPORT_RM` sends no flags
/// and cannot ask for it. Mirrors `rm_foreign::import_surface` (the same reservation,
/// create and commit), which is why a failure leaves nothing recorded.
#[inline(never)]
fn import_resource(
    io: &Io<'_>,
    ctx: u32,
    drm: u32,
    gem: u32,
    fl: &FrLayout,
    size: u64,
) -> Result<u32, Fail> {
    let adapter = io.adapter;
    let b = step_budget(io);
    if !crate::virtio::foreign::rm_import_served(adapter) {
        return Err(Fail::new(FailKind::Refused, 0x41));
    }
    let Ok(fl) = validate_request(ctx, drm, gem, FLAG_LAYOUT, size, Some(*fl)) else {
        let _ = adapter.with_virtio(|v| v.foreign_note_refusal(RefusalKind::BadRequest));
        return Err(Fail::new(FailKind::Refused, 0x43));
    };
    let begin = adapter
        .with_virtio(|v| v.foreign_begin_kmd_import(drm, ctx, ctx, size))
        .map_err(|_| Fail::new(FailKind::Refused, 0x46))?;
    let reservation = match begin {
        ForeignBegin::Reserved(r) => r,
        ForeignBegin::NotOwned | ForeignBegin::BadContext => {
            return Err(Fail::new(FailKind::Refused, 0x43))
        }
        ForeignBegin::Quota(_) => return Err(Fail::new(FailKind::Refused, 0x44)),
    };
    let mut errno = 0u32;
    let created = ctrl::alloc_blob_errno_within(
        io.passive,
        adapter,
        ctx,
        HELIOS_BLOB_MEM_RM_EXPORT,
        VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE,
        foreign_blob_id(drm, gem),
        size,
        Some(KMD),
        Some(&mut errno),
        Some(&b),
    );
    let resource = match created {
        Ok(id) => id,
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.foreign_abandon_import(reservation));
            let code = match (e, classify(errno)) {
                (crate::virtio::VirtioError::OutOfMemory, _) | (_, Verdict::NoResources) => 0x44,
                (_, Verdict::NotOwned | Verdict::BadRange | Verdict::Unsupported) => 0x45,
                (_, Verdict::Device) => 0x46,
            };
            return Err(Fail::new(FailKind::Host, ((errno & 0xff) << 8) | code));
        }
    };
    let committed = adapter
        .with_virtio(|v| v.foreign_commit_import(KMD, reservation, resource, ctx, drm, gem, fl));
    match committed {
        Ok(ForeignCommit::Recorded) => Ok(resource),
        _ => {
            // Teardown (or a closed DRM file) raced the round trip: the resource exists
            // host side with no record; release it through the ordinary path, on the
            // undo's allowance (the creation's may be spent).
            let ub = undo_budget();
            let _ = ctrl::release_blob_for_owner_within(
                io.passive,
                adapter,
                KMD,
                ctx,
                resource,
                Some(&ub),
            );
            Err(Fail::new(FailKind::Refused, 0x47))
        }
    }
}

/// Map the resource once into the host window and unmap it again. The host must serve the
/// map (a host that does not is found here, at the first allocation, not when dxgkrnl first
/// needs a CPU view), and its `map_info` must be the attribute the memory was made with:
/// anything else is another attribute than RM's own CPU mappings use, and the allocation is
/// given up rather than aliased.
///
/// The unmap is safe: nothing else knows the resource yet, so nothing can read the range
/// (the host swaps in zeros when it unmaps).
///
/// A range is returned to the window only once the host confirmed the unmap. If the unmap
/// fails the host may still have the range mapped, and a map that reused it would overlap:
/// the failure ends the creation with the blob still recorded as mapped, and the undo's
/// `release_blob_for_owner_within` (which sees `mapped`) unmaps again and only then frees the
/// range.
#[inline(never)]
fn trial_map(io: &Io<'_>, resource: u32, made: Cache) -> Result<(), (Why, Fail)> {
    let adapter = io.adapter;
    let b = step_budget(io);
    let prep = match ctrl::map_blob_prepare_within(
        io.passive,
        adapter,
        OwnerFilter::Any,
        resource,
        Some(&b),
    ) {
        Ok(p) => p,
        Err(_) => {
            SYS_TRIAL_FAIL.fetch_add(1, Ordering::Relaxed);
            return Err((Why::Trial, Fail::new(FailKind::Host, 0x50)));
        }
    };
    SYS_CACHE.store(prep.map_cache, Ordering::Relaxed);
    // Unmap first, judge after: the mapping must not stay behind a refusal.
    let unmapped = ctrl::resource_unmap_blob_budgeted(io.passive, adapter, resource, &b).is_ok();
    if !unmapped {
        // Not confirmed: neither the mapped flag nor the window range is touched here.
        SYS_SOFT.fetch_add(1, Ordering::Relaxed);
        SYS_TRIAL_FAIL.fetch_add(1, Ordering::Relaxed);
        return Err((Why::Trial, Fail::new(FailKind::Host, 0x51)));
    }
    let _ = adapter.with_virtio(|v| {
        v.blob_note_unmapped(resource);
        if let Some(w) = v.host_visible() {
            v.free_window_range_pub(prep.gpa.wrapping_sub(w.base), prep.size);
        }
    });
    if !rs::host_cache_ok(made, prep.map_cache) {
        SYS_MIS.fetch_add(1, Ordering::Relaxed);
        return Err((Why::Cache, Fail::new(FailKind::Layout, prep.map_cache)));
    }
    SYS_TRIAL.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

// ---- release ------------------------------------------------------------------------

fn mirror_live() {
    let live = STATE.lock().svc.live();
    LIVE.store(live, Ordering::Relaxed);
}

/// The host resource `resource_id` has been released (`ctrl::release_allocation_resource`:
/// DestroyAllocation, or the last close of a destroyed allocation that was still open):
/// if the service made it, end the screen's use of it, close its GEM and free its memory.
/// PASSIVE, no lock held. One atomic load for every Venus allocation's destroy.
///
/// The host resource is already unreferenced, so nothing the host holds names the GEM; the
/// host's own dma-buf reference keeps the pages valid for a viewer that still samples them.
#[inline(never)]
pub(crate) fn released(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    if LIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (taken, h) = {
        let mut g = STATE.lock();
        (g.svc.take(resource_id), g.h)
    };
    let Some(t) = taken else {
        return;
    };
    // The screen stops using it first (no flip names a GEM about to be closed).
    super::sysmem_flip::target_gone(adapter, resource_id);
    let epoch_now = adapter.with_virtio(|v| v.nvrm_epoch()).unwrap_or(0);
    let mut freed = true;
    if epoch_now == t.epoch && epoch_now != 0 {
        let io = Io {
            passive,
            adapter,
            epoch: t.epoch,
            limit: None,
        };
        let mut leaked = false;
        // A primary the screen has moved off is closed once the host released it (the
        // release seam, 15.7): bounded, and never for the one on the scanout.
        wait_released(&io, super::sysmem_flip::close_wait(t.gem));
        note_stage(stage::GEM_CLOSE);
        if gem_close(&io, &h, t.gem).is_err() {
            SYS_SOFT.fetch_add(1, Ordering::Relaxed);
            leaked = true;
        }
        note_stage(stage::FREE);
        if free_sys(&io, &h, Svc::handle(t.slot)).is_err() {
            SYS_SOFT.fetch_add(1, Ordering::Relaxed);
            leaked = true;
            freed = false;
        }
        if leaked {
            SYS_LEAK.fetch_add(1, Ordering::Relaxed);
        }
    }
    super::sysmem_flip::gem_closed(t.gem);
    // A slot whose RM object could not be freed stays taken (Closing): its handle is still
    // RM's, and reusing the number would make the next `RM_ALLOC` fail as a duplicate. The
    // sweep of the transport closes the client and the generation reset frees the slot.
    if freed {
        STATE.lock().svc.freed(t.slot);
    }
    mirror_live();
    SYS_FREED.fetch_add(1, Ordering::Relaxed);
    publish_counters();
}

/// Hold the GEM close of a primary the host was told to stop showing until the host says it is
/// done with it (`ScanoutReleased`, tracked in `scanout_release`), or 500 ms after the flip
/// that replaced it: the level 3 ring's rule (`rm_sysmem::close_gate`). Nothing to wait for
/// (`Free`: the shown buffer, which the host never releases; one never flipped; release events
/// not tracked) is two loads. PASSIVE, no lock held, polled every 2 ms (the interrupt side's
/// release signals the HPD worker, not this thread); StopDevice ends it.
#[inline(never)]
fn wait_released(io: &Io<'_>, w: CloseWait) {
    let CloseWait::Replaced { seq, .. } = w else {
        return;
    };
    let mut began = false;
    loop {
        let t = now();
        let done = crate::virtio::scanout_release::is_done(seq, t);
        match rs::close_gate(w, done, t) {
            Wait::Go => return,
            Wait::TimedOut => {
                SYS_REL_TMO.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Wait::Hold { .. } => {
                if io.stopping() {
                    return;
                }
                if !began {
                    began = true;
                    SYS_REL_WAIT.fetch_add(1, Ordering::Relaxed);
                    note_stage(stage::REL_WAIT);
                }
                ctrl::sleep_ms(io.passive, 2);
            }
        }
    }
}

/// The live slot of `resource_id`, for a flip: `(gem, epoch)`.
pub(super) fn live_gem(resource_id: u32) -> Option<(u32, u64)> {
    STATE
        .lock()
        .svc
        .find(resource_id)
        .map(|(_, gem, epoch)| (gem, epoch))
}

/// The transport is gone (`rm_client::forget`): forget every slot. The sweep closed the
/// control and DRM files, which freed the RM objects and dropped the GEMs.
pub(super) fn forget() {
    {
        let mut g = STATE.lock();
        g.svc.reset();
        g.h = Handles::NONE;
        // The next generation reads the knob again at its bring-up; until then nothing asks
        // dxgkrnl for `Cached`.
        g.cache = PrimaryCache::WriteCombine;
    }
    LIVE.store(0, Ordering::Relaxed);
    super::sysmem_flip::reset();
}
