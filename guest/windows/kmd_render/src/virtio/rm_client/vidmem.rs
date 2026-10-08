//! The KMD's own GPU-only allocations from RM VIDEO memory (`RedirVram` 1, stage V2 of
//! `docs/vram-redirection.md`): the allocation service. Every decision is
//! `helios_kmd_logic::rm_vidmem`; the slot table, bring-up and strikes are level 5's
//! (`helios_kmd_logic::rm_sysmem::Svc`), and so are the RM messages (`sysmem.rs`'s export, GEM
//! import, close, free and foreign import, shared with level 5), in a separate RM client of the
//! KMD's own owner so the two services' handles never meet.
//!
//! WHAT IT DOES. `DxgkDdiCreateAllocation` of a GPU-only GDI texture (the redirection surface
//! Windows hands out once GDI allocations need not be CPU visible) asks [`try_create`] first. On
//! the creator's thread, at PASSIVE, under one [`rs::CREATE_BUDGET_MS`] deadline:
//!
//! 1. the service's RM client, once per transport generation (level 5's bring-up machine);
//! 2. `RM_ALLOC` of `NV01_MEMORY_LOCAL_USER` (pitch-linear, 64 KiB pages: the ring surfaces'
//!    parameters), export to a file, `GEM_IMPORT_NVKMS` on the service's DRM file, close the file;
//! 3. `RESOURCE_CREATE_BLOB` of the GEM as `RM_EXPORT` WITHOUT `USE_MAPPABLE` (video memory is
//!    never CPU-mapped through the Venus window), a foreign resource of the KMD's own owner;
//! 4. the WDDM allocation ADOPTS it with a LINEAR layout: openers (DWM on NVK) get the resource id
//!    and the layout trailer and import it with `RM_RESOURCE_IMPORT` (zero copy, VRAM).
//!
//! Any failure undoes what was made, counts a strike, and the caller makes today's Venus image.
//! DestroyAllocation reaches [`released`] through `ctrl::release_allocation_resource`: the copy
//! engine forgets the object (`ce_vram::object_gone`, the route's destination record), then the
//! GEM is closed and the memory freed. [`lookup`] answers the copy engine's and the Present's
//! questions about an object.
//!
//! LOCKING. `STATE` is a leaf spinlock over plain data, as level 5's.

use super::sysmem::{self as sm, Handles};
use super::{Io, REPLY_MAX, TIMEOUT_MS};
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use crate::virtio::ctrl;
use crate::virtio::gpu::{AllocAdopt, DeviceOwner};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use helios_kmd_logic::foreign_resource::{AdoptRequest, Layout as FrLayout};
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind};
use helios_kmd_logic::rm_sysmem::{self as rs, Admit, AllocOutcome, FreeOutcome, Svc};
use helios_kmd_logic::rm_vidmem::{self as rv, VidLayout, Why};
use helios_kmd_logic::sweep_budget::UNITS_PER_MS;

const KMD: DeviceOwner = DeviceOwner::KMD_RM;
const UNREAD: u32 = u32::MAX;

/// One live object, for [`lookup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VramObject {
    pub resource_id: u32,
    /// The service's RM client and the memory's handle in it (what the channel dups).
    pub client: u32,
    pub memory: u32,
    pub size: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub epoch: u64,
}

struct State {
    svc: Svc,
    h: Handles,
    objs: [Option<VramObject>; rs::TABLE_CAP],
    bytes: u64,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    svc: Svc::new(),
    h: Handles::NONE,
    objs: [None; rs::TABLE_CAP],
    bytes: 0,
});

static KNOB: AtomicU32 = AtomicU32::new(UNREAD);
static OFF: AtomicU32 = AtomicU32::new(0);

/// Whether `RvOff` has `bit` (`rm_vidmem::off`). One relaxed load.
pub(crate) fn off(bit: u32) -> bool {
    OFF.load(Ordering::Relaxed) & bit != 0
}
static LIVE: AtomicU32 = AtomicU32::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

static TRY: AtomicU32 = AtomicU32::new(0);
static OK: AtomicU32 = AtomicU32::new(0);
static VENUS: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static STAGE: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);
static FREED: AtomicU32 = AtomicU32::new(0);
static BRING: AtomicU32 = AtomicU32::new(0);
static MS: AtomicU32 = AtomicU32::new(0);
static MS_MAX: AtomicU32 = AtomicU32::new(0);
static SOFT: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);

mod stage {
    pub const ADMIT: u32 = 1;
    pub const BRING_UP: u32 = 2;
    pub const ALLOC: u32 = 3;
    pub const OPEN_EXPORT: u32 = 4;
    pub const EXPORT: u32 = 5;
    pub const GEM_IMPORT: u32 = 6;
    pub const CLOSE_EXPORT: u32 = 7;
    pub const IMPORT: u32 = 8;
    pub const ADOPT: u32 = 11;
    pub const GEM_CLOSE: u32 = 20;
    pub const FREE: u32 = 21;
    pub const UNDO: u32 = 22;
}

/// `RedirVram` (read at StartDevice, [`reset_for_start`]). One relaxed load.
pub(crate) fn knob_on() -> bool {
    let k = KNOB.load(Ordering::Relaxed);
    k != UNREAD && rv::knob_on(k)
}

/// The raw knob value, for the private-data size decision (`GetStandardAllocationDriverData`).
pub(crate) fn knob() -> u32 {
    match KNOB.load(Ordering::Relaxed) {
        UNREAD => rv::KNOB_OFF,
        k => k,
    }
}

/// Whether any object is alive (every destroy and every Present passes the fast exit).
pub(crate) fn any_live() -> bool {
    LIVE.load(Ordering::Relaxed) != 0
}

/// StartDevice (PASSIVE): read the knob, zero the counters; with the knob on, write them.
pub(crate) fn reset_for_start() {
    let k = crate::diag::read_config_dword(crate::diag::knobs::REDIR_VRAM, rv::KNOB_OFF);
    KNOB.store(k, Ordering::Relaxed);
    let o = crate::diag::read_config_dword(crate::diag::knobs::RV_OFF, 0);
    OFF.store(o, Ordering::Relaxed);
    crate::diag::record_named_bytes(b"RvOffEff", o);
    for c in [
        &TRY, &OK, &VENUS, &WHY, &STAGE, &FAIL, &FREED, &BRING, &MS, &MS_MAX, &SOFT, &LEAK, &OPEN,
        &OPEN_FG, &OPEN_LAY,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    crate::diag::record_named_bytes(b"RvKnob", k);
    if rv::knob_on(k) {
        publish(true);
    }
}

/// Mirror the counters (PASSIVE), once the service was asked for something.
pub(crate) fn publish_counters() {
    publish(false);
}

fn publish(always: bool) {
    if !always && TRY.load(Ordering::Relaxed) == 0 {
        return;
    }
    let word = {
        let g = STATE.lock();
        state_word(&g.svc)
    };
    use crate::diag::record_named_bytes as rec;
    rec(b"RvTry", TRY.load(Ordering::Relaxed));
    rec(b"RvOk", OK.load(Ordering::Relaxed));
    rec(b"RvVenus", VENUS.load(Ordering::Relaxed));
    rec(b"RvWhy", WHY.load(Ordering::Relaxed));
    rec(b"RvStage", STAGE.load(Ordering::Relaxed));
    rec(b"RvFail", FAIL.load(Ordering::Relaxed));
    rec(b"RvState", word);
    rec(b"RvLive", LIVE.load(Ordering::Relaxed));
    rec(b"RvBytes", (BYTES.load(Ordering::Relaxed) >> 20) as u32);
    rec(b"RvFreed", FREED.load(Ordering::Relaxed));
    rec(b"RvBring", BRING.load(Ordering::Relaxed));
    rec(b"RvMs", MS.load(Ordering::Relaxed));
    rec(b"RvMsMax", MS_MAX.load(Ordering::Relaxed));
    rec(b"RvSoft", SOFT.load(Ordering::Relaxed));
    rec(b"RvLeak", LEAK.load(Ordering::Relaxed));
    super::ce_vram::publish_counters();
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
    STAGE.store(s, Ordering::Relaxed);
}

fn pack(stage: u32, f: Fail) -> u32 {
    (stage << 24) | ((f.kind as u32) << 16) | (f.code & 0xffff)
}

fn now() -> u64 {
    crate::adapter::foreign_scanout::now_100ns()
}

/// What a successful creation hands the allocation arm (the resource is ADOPTED: the
/// allocation's destroy releases it).
#[derive(Clone, Copy)]
pub(crate) struct Created {
    pub resource_id: u32,
    pub layout: FrLayout,
    pub size: u64,
}

/// Make a GPU-only GDI surface from RM video memory, or `None`: Venus makes it as today.
/// PASSIVE, the creator's thread, no lock held. With the knob off: one relaxed load.
#[inline(never)]
pub(crate) fn try_create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    kind: rs::Kind,
    width: u32,
    height: u32,
    dxgi: u32,
) -> Option<Created> {
    if rv::route(knob(), kind).is_err() {
        return None;
    }
    TRY.fetch_add(1, Ordering::Relaxed);
    note_stage(stage::ADMIT);
    let t0 = now();
    let r = create(passive, adapter, width, height, dxgi);
    let ms = (now().wrapping_sub(t0) / UNITS_PER_MS).min(u64::from(u32::MAX)) as u32;
    MS.store(ms, Ordering::Relaxed);
    MS_MAX.fetch_max(ms, Ordering::Relaxed);
    let out = match r {
        Ok(c) => {
            OK.fetch_add(1, Ordering::Relaxed);
            Some(c)
        }
        Err(why) => {
            VENUS.fetch_add(1, Ordering::Relaxed);
            WHY.store(why.code(), Ordering::Relaxed);
            None
        }
    };
    publish_counters();
    out
}

#[inline(never)]
fn create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    width: u32,
    height: u32,
    dxgi: u32,
) -> Result<Created, Why> {
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
    let lay = rv::layout(width, height, dxgi).map_err(|e| e.why())?;
    if !rv::within_budget(BYTES.load(Ordering::Relaxed), lay.surface.size) {
        return Err(Why::Budget);
    }
    let io = Io {
        passive,
        adapter,
        epoch,
        limit: Some(rs::create_budget(now(), TIMEOUT_MS)),
    };
    let slot = admit(&io)?;
    let h = STATE.lock().h;
    let mut made = Made::default();
    match build(&io, ctx, slot, &lay, &h, &mut made) {
        Ok(c) => Ok(c),
        Err((why, st, f)) => {
            FAIL.store(pack(st, f), Ordering::Relaxed);
            note_stage(stage::UNDO);
            let live = undo(&io, ctx, &h, slot, &made);
            {
                let mut g = STATE.lock();
                if live {
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

fn admit(io: &Io<'_>) -> Result<usize, Why> {
    loop {
        let a = STATE.lock().svc.admit(io.epoch);
        match a {
            Admit::Go(slot) => return Ok(slot),
            Admit::Refuse(why) => return Err(Why::from_sysmem(why)),
            Admit::Wait => {
                if io.limit_spent() {
                    return Err(Why::BringUpBusy);
                }
                ctrl::sleep_ms(io.passive, 1);
            }
            Admit::BringUp => {
                note_stage(stage::BRING_UP);
                let result = sm::bring_up(io);
                {
                    let mut g = STATE.lock();
                    g.h = match &result {
                        Ok(h) => *h,
                        Err(_) => Handles::NONE,
                    };
                    g.svc.bring_up_done(result.is_ok());
                }
                if let Err(f) = result {
                    FAIL.store(pack(stage::BRING_UP, f), Ordering::Relaxed);
                    return Err(Why::BringUp);
                }
                BRING.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Made {
    mem: AllocOutcome,
    export_ch: u32,
    gem: u32,
    resource: u32,
}

type StepErr = (Why, u32, Fail);

#[inline(never)]
fn build(
    io: &Io<'_>,
    ctx: u32,
    slot: usize,
    want: &VidLayout,
    h: &Handles,
    made: &mut Made,
) -> Result<Created, StepErr> {
    let late = |st: u32| -> Result<(), StepErr> {
        if io.limit_spent() {
            Err((Why::Slow, st, Fail::new(FailKind::Transport, 1)))
        } else {
            Ok(())
        }
    };
    let mem = Svc::handle(slot);

    late(stage::ALLOC)?;
    note_stage(stage::ALLOC);
    let lay = match alloc_vid(io, h, mem, want) {
        Ok(l) => {
            made.mem = AllocOutcome::Made;
            l
        }
        Err(e) => {
            made.mem = match e {
                AllocErr::Rm(f) => AllocOutcome::after_failure(f.kind),
                // RM made it; its answer did not hold the picture: the undo frees it.
                AllocErr::Layout(_) => AllocOutcome::Made,
            };
            let f = match e {
                AllocErr::Rm(f) => f,
                AllocErr::Layout(_) => Fail::new(FailKind::Layout, 0x30),
            };
            return Err((Why::Alloc, stage::ALLOC, f));
        }
    };
    let size = lay.surface.size;
    let fl = rv::foreign_layout(&lay)
        .ok_or((Why::Size, stage::ALLOC, Fail::new(FailKind::Layout, 0x31)))?;

    late(stage::OPEN_EXPORT)?;
    note_stage(stage::OPEN_EXPORT);
    made.export_ch = io
        .open_file(rc::DEV_CTL)
        .map_err(|f| (Why::Alloc, stage::OPEN_EXPORT, f))?;
    note_stage(stage::EXPORT);
    sm::export(io, h, mem, made.export_ch).map_err(|f| (Why::Alloc, stage::EXPORT, f))?;
    note_stage(stage::GEM_IMPORT);
    made.gem =
        sm::gem_import(io, h, size, made.export_ch).map_err(|f| (Why::Alloc, stage::GEM_IMPORT, f))?;
    note_stage(stage::CLOSE_EXPORT);
    if io.close_file(made.export_ch) {
        made.export_ch = 0;
    } else {
        return Err((Why::Alloc, stage::CLOSE_EXPORT, Fail::new(FailKind::Host, 0xC1)));
    }

    late(stage::IMPORT)?;
    note_stage(stage::IMPORT);
    made.resource = sm::import_resource_flags(io, ctx, h.drm, made.gem, &fl, size, 0)
        .map_err(|f| (Why::Import, stage::IMPORT, f))?;

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
        trailer_room: true,
        plane_room: true,
    };
    let adopted = io
        .adapter
        .with_virtio(|v| v.adopt_for_allocation(made.resource, &request));
    let (adopted_size, adopted_layout) = match adopted {
        Ok(AllocAdopt::Foreign(a)) => (a.size, a.layout),
        _ => return Err((Why::Adopt, stage::ADOPT, Fail::new(FailKind::Refused, 8))),
    };
    let resource = made.resource;
    made.resource = 0;
    {
        let mut g = STATE.lock();
        g.svc.commit(slot, resource, made.gem);
        let client = g.h.root;
        if let Some(o) = g.objs.get_mut(slot) {
            *o = Some(VramObject {
                resource_id: resource,
                client,
                memory: mem,
                size: adopted_size,
                pitch: adopted_layout.stride,
                width: adopted_layout.width,
                height: adopted_layout.height,
                fourcc: adopted_layout.fourcc,
                epoch: io.epoch,
            });
        }
        g.bytes = g.bytes.saturating_add(adopted_size);
        BYTES.store(g.bytes, Ordering::Relaxed);
    }
    mirror_live();
    Ok(Created {
        resource_id: resource,
        layout: adopted_layout,
        size: adopted_size,
    })
}

enum AllocErr {
    Rm(Fail),
    Layout(Why),
}

/// `NV_ESC_RM_ALLOC` of pitch-linear video memory (`rm_client::mem_alloc_params`), and what RM
/// made of it.
#[inline(never)]
fn alloc_vid(io: &Io<'_>, h: &Handles, mem: u32, want: &VidLayout) -> Result<VidLayout, AllocErr> {
    let params = rc::mem_alloc_params(h.root, &want.surface);
    let mut resp = [0u8; REPLY_MAX];
    let n = io
        .rm_alloc(
            h.ctl,
            h.root,
            rc::H_DEVICE,
            mem,
            rc::NV01_MEMORY_LOCAL_USER,
            &params,
            &mut resp,
        )
        .map_err(AllocErr::Rm)?;
    let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(AllocErr::Rm(Fail::new(FailKind::Parse, 0x40)))?)
        .map_err(|e| AllocErr::Rm(sm::reply_fail(e, 0x40)))?;
    rv::adopt(want, reply.nested).map_err(AllocErr::Layout)
}

/// Give back what a failed creation made (level 5's order). Whether RM may still hold the memory.
#[inline(never)]
fn undo(io: &Io<'_>, ctx: u32, h: &Handles, slot: usize, made: &Made) -> bool {
    let b = sm::undo_budget();
    let uio = io.with_limit(Some(b));
    let mut leaked = false;
    if made.export_ch != 0 && !uio.close_file(made.export_ch) {
        leaked = true;
    }
    if made.resource != 0
        && ctrl::release_blob_for_owner_within(uio.passive, uio.adapter, KMD, ctx, made.resource, Some(&b))
            .is_err()
    {
        leaked = true;
    }
    if made.gem != 0 && sm::gem_close(&uio, h, made.gem).is_err() {
        leaked = true;
    }
    let free = if made.mem == AllocOutcome::NotMade {
        FreeOutcome::NotSent
    } else {
        FreeOutcome::of(sm::free_sys(&uio, h, Svc::handle(slot)))
    };
    if free == FreeOutcome::Failed {
        leaked = true;
    }
    if leaked {
        SOFT.fetch_add(1, Ordering::Relaxed);
        LEAK.fetch_add(1, Ordering::Relaxed);
    }
    rs::mem_may_be_live(made.mem, free)
}

fn mirror_live() {
    let live = STATE.lock().svc.live();
    LIVE.store(live, Ordering::Relaxed);
}

/// The live object behind host resource `resource_id`, if the service made it. Spinlock only
/// (any IRQL up to DISPATCH); one relaxed load when nothing is alive.
pub(crate) fn lookup(resource_id: u32) -> Option<VramObject> {
    if resource_id == 0 || !any_live() {
        return None;
    }
    let g = STATE.lock();
    g.objs.iter().flatten().find(|o| o.resource_id == resource_id).copied()
}

static OPEN: AtomicU32 = AtomicU32::new(0);
static OPEN_FG: AtomicU32 = AtomicU32::new(0);
static OPEN_LAY: AtomicU32 = AtomicU32::new(0);

/// `DxgkDdiOpenAllocation` registered an open of `resource_id` (PASSIVE): if it is one of the
/// service's objects, count it (`RvOpen`), whether the open identity says FOREIGN (`RvOpenFg`: the
/// opener's UMD takes the import-by-id route) and whether the layout record reached it
/// (`RvOpenLay`), and the opener's process (`RvOpenPid`: DWM's pid means DWM opened the
/// redirection surface). One relaxed load when nothing is alive.
pub(crate) fn note_open(resource_id: u32, foreign: bool, layout: bool) {
    if lookup(resource_id).is_none() {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"RvOpen", OPEN.fetch_add(1, Ordering::Relaxed) + 1);
    if foreign {
        rec(b"RvOpenFg", OPEN_FG.fetch_add(1, Ordering::Relaxed) + 1);
    }
    if layout {
        rec(b"RvOpenLay", OPEN_LAY.fetch_add(1, Ordering::Relaxed) + 1);
    }
    rec(b"RvOpenPid", crate::virtio::nvrm_window::current_pid());
}

/// The host resource `resource_id` has been released (`ctrl::release_allocation_resource`): if the
/// service made it, the copy engine forgets it first, then its GEM is closed and its memory freed.
/// PASSIVE, no lock held; one relaxed load for every other allocation's destroy.
#[inline(never)]
pub(crate) fn released(passive: PassiveLevel, adapter: &AdapterContext, resource_id: u32) {
    if !any_live() {
        return;
    }
    let (taken, h, obj) = {
        let mut g = STATE.lock();
        let t = g.svc.take(resource_id);
        let obj = t.and_then(|t| g.objs.get_mut(t.slot).and_then(Option::take));
        if let Some(o) = obj {
            g.bytes = g.bytes.saturating_sub(o.size);
            BYTES.store(g.bytes, Ordering::Relaxed);
        }
        (t, g.h, obj)
    };
    let Some(t) = taken else {
        return;
    };
    // No copy names it from here: the route drops its destination record, the channel's mapping
    // goes stale (given back by the next pass that holds the channel's I/O).
    crate::ddi::vram_redirect::destination_gone(resource_id);
    super::ce_vram::object_gone(resource_id);
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
        note_stage(stage::GEM_CLOSE);
        if sm::gem_close(&io, &h, t.gem).is_err() {
            leaked = true;
        }
        note_stage(stage::FREE);
        // A dup of this memory in the channel's client keeps it alive until that dup goes; the
        // free here only drops the service's own handle.
        if sm::free_sys(&io, &h, Svc::handle(t.slot)).is_err() {
            leaked = true;
            freed = false;
        }
        if leaked {
            SOFT.fetch_add(1, Ordering::Relaxed);
            LEAK.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _ = obj;
    if freed {
        STATE.lock().svc.freed(t.slot);
    }
    mirror_live();
    FREED.fetch_add(1, Ordering::Relaxed);
    publish_counters();
}

/// The transport is gone (`rm_client::forget`): the sweep closed the service's client, which freed
/// every object.
pub(super) fn forget() {
    {
        let mut g = STATE.lock();
        g.svc.reset();
        g.h = Handles::NONE;
        g.objs = [None; rs::TABLE_CAP];
        g.bytes = 0;
    }
    BYTES.store(0, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed);
}
