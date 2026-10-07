//! The KMD's own RM client: the I/O half. Design, stages, failure matrix and the
//! hardware checklist: `docs/kmd-rm-client.md`. The decisions (which step is next,
//! what a failure does, the payloads) are `helios_kmd_logic::rm_client`; this file
//! only performs them.
//!
//! WHAT IT IS. The KMD speaks the NVIDIA RM protocol itself, over the same
//! forwarding path a user-mode RM client uses (`nvrm::forward`, i.e. the allow-list,
//! the ownership tables and the quotas), under one reserved owner,
//! [`DeviceOwner::KMD_RM`]. That owner is not any `hDevice`, so a process's sweep
//! (`close_all_for_owner`) can never touch these handles; the transport-wide sweep
//! (`close_all_on_host`, run by `retire_transport`) closes them while the host still
//! answers, exactly as it closes everybody's. Its quotas are its own (the per-owner
//! limits apply to this owner alone), and its objects are the first thing the KMD
//! owns that is NOT a Venus resource.
//!
//! WHEN IT RUNS. Never unless the `KmdRmClient` service-key DWORD is nonzero, and
//! then only on the HPD worker thread (PASSIVE, the one thread that already does the
//! display's host round trips), after the display half has bound a VidPn primary:
//! [`service`] is one line in the worker loop. It is the ONLY mutator of the client
//! while a worker runs (StopDevice joins the worker before `retire_transport`; see
//! [`retire_begin`]). Each pass performs at most [`STEPS_PER_PASS`] steps and wakes
//! the worker again for the rest, so a bring-up never holds the display's refresh
//! for more than a few round trips at a time.
//!
//! LEVEL 3 (the ring). The client builds [`rc::RING_SLOTS`] surfaces, each with its
//! CPU view, and `rm_present` (same worker, same pass) shows the desktop through
//! them. The views are then written outside the `CLIENT` lock, by a frame copy that
//! holds a LEASE (`lease_slot`, `end_lease`): `retire_begin` and `cleanup` wait for it
//! before they unmap, so a view never goes away under a write.
//!
//! LEVEL 5 (`sysmem`, `sysmem_flip`, children of this module). No ring and no client steps on the
//! worker: the VidPn primary is allocated from RM SYSTEM memory on the creator's thread (its own RM
//! client, see `sysmem.rs`) and flipped as it is (`sysmem_flip.rs`); the worker only flips.
//!
//! LOCKING. `CLIENT` is a LEAF spinlock holding plain data (`rm_client::Client`):
//! never held across a host round trip, a wait, an allocation or another lock; every
//! step copies what it needs out under the lock, does its I/O with no lock held, and
//! reports back under the lock (discarding the report if the transport generation
//! changed meanwhile). It is never taken with `virtio_lock` held, nor the other way
//! round (`with_virtio` is called only with `CLIENT` released).
//!
//! FAILING CLOSED. A failure in bring-up or in the surface path kills the client for
//! the transport generation ([`cleanup`] closes what it opened); nothing falls over,
//! because nothing consumes the client yet but the probe, and every other allocation
//! is still Venus. A failure of the CPU view or the probe only gives those up.

use super::gpu::DeviceOwner;
use super::nvrm::{self, MapRefusal, Refusal};
use super::rm_present;
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};
use helios_kmd_logic::foreign_scanout::SetError;
use helios_kmd_logic::rm_client::{
    self as rc, Action, Client, Fail, FailKind, Out, Step, Want, MAX_DRI,
};
use helios_kmd_logic::sweep_budget::SweepBudget;
use wdk_sys::ntddk::{MmMapIoSpace, MmUnmapIoSpace};
use wdk_sys::{PHYSICAL_ADDRESS, _MEMORY_CACHING_TYPE};

// Level 5 (`KmdRmClient` = 5): the KMD's own allocations from RM system memory. Children of
// this module because they drive the same `Io` and bring-up steps, which stay private.
pub(crate) mod sysmem;
pub(crate) mod sysmem_blt;
pub(crate) mod sysmem_flip;
// `RmCopyEngine`: the KMD's own copy-engine channel (its own RM client, driven by the same `Io`)
// (the hardware self-test follows). Independent of `KmdRmClient`.
pub(crate) mod ce_channel;

/// The one owner of every handle this client opens.
const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Steps performed per worker pass before the worker is woken again.
const STEPS_PER_PASS: usize = 6;
/// How long any one host message may take. Short on purpose: `stop_hpd` joins the worker
/// for 5 s (twice), so a step caught by StopDevice must be over well inside that, or the
/// worker (and with it the adapter) is leaked. Every host answer here is a few
/// milliseconds; a message that takes longer is a host that is not answering.
const TIMEOUT_MS: u64 = 2_500;
/// How long the probe picture stays on screen: the foreign scanout source's lapse,
/// after which the HPD worker gives scanout 0 back by itself (`FsLapse`).
const PROBE_LAPSE_MS: u32 = 4_000;
/// The largest `Ioctl` reply any RM step expects (`NV_ESC_RM_ALLOC` of a memory
/// object: 16 + 12 + 48 + 128), with room.
const REPLY_MAX: usize = 384;
/// The largest request a step builds on the stack (the same alloc plus headers).
const REQUEST_MAX: usize = 256;
const PAGE: u64 = 4096;

/// The one client. See the module docs for what may touch it and when.
static CLIENT: SpinLock<Client> = SpinLock::new(Client::new());

/// `KNOB_LEVEL` before the knob has been read for this transport generation.
const KNOB_UNREAD: u32 = u32::MAX;
/// The `KmdRmClient` knob (0 to 5), or [`KNOB_UNREAD`]: read once per transport
/// generation (so `reg add` + `pnputil /restart-device` applies it), by resetting it to
/// unread in [`forget`], which `retire_transport` runs for every transport it drops.
/// With the knob at 0 (the default) [`service`] is this one atomic load and nothing
/// else: no virtio lock, no registry read.
static KNOB_LEVEL: AtomicU32 = AtomicU32::new(KNOB_UNREAD);

/// Whether the ring level (3 or 4) is in force this generation: one relaxed load. Level 5
/// does not run the ring (its allocations are flipped as they are).
pub(super) fn ring_level_on() -> bool {
    let level = KNOB_LEVEL.load(Ordering::Relaxed);
    level != KNOB_UNREAD && (3..=4).contains(&level)
}

/// The `KmdRmClient` level of this transport generation if the worker has read it, `None`
/// before that (`ForeignFlip` refuses to decide on an unknown level).
pub(super) fn level_if_read() -> Option<u32> {
    let level = KNOB_LEVEL.load(Ordering::Relaxed);
    (level != KNOB_UNREAD).then_some(level)
}

/// Whether the RM system-memory level (5) is in force this generation: one relaxed load.
fn sysmem_level_on() -> bool {
    let level = KNOB_LEVEL.load(Ordering::Relaxed);
    level != KNOB_UNREAD && level >= helios_kmd_logic::rm_sysmem::LEVEL
}

/// Counters, mirrored by [`publish_counters`] (names at most 14 characters).
///
/// `RmStatus` is [`Client::status_word`] after the last pass, `RmStep` the step
/// started last (written BEFORE the step runs, so a hang names itself), `RmFail` the
/// packed failure of the last death (`step << 24 | kind << 16 | code`), `RmDead` how
/// many times the client died, `RmSteps` steps performed, `RmBringUp` bring-ups
/// finished, `RmSurf` / `RmSurfFree` surfaces made / freed, `RmGem` GEM handles
/// imported, `RmView` / `RmViewFree` kernel views made / unmapped, `RmFillMs` how long
/// the last probe fill took, `RmRdBad` probe read-back samples that did not match,
/// `RmProbe` probe flips shown, `RmBusy` probe sets that found scanout 0 held,
/// `RmClosed` handles closed by cleanup, `RmSoft` undo steps that failed, `RmRegFd`
/// `REGISTER_FD`s the host refused (harmless, as in librmclient).
pub static RM_STATUS: AtomicU32 = AtomicU32::new(0);
pub static RM_LAST_STEP: AtomicU32 = AtomicU32::new(0);
pub static RM_FAIL: AtomicU32 = AtomicU32::new(0);
pub static RM_DEAD: AtomicU32 = AtomicU32::new(0);
pub static RM_STEPS: AtomicU32 = AtomicU32::new(0);
pub static RM_BRING_UPS: AtomicU32 = AtomicU32::new(0);
pub static RM_SURFACES: AtomicU32 = AtomicU32::new(0);
pub static RM_SURFACES_FREED: AtomicU32 = AtomicU32::new(0);
pub static RM_GEMS: AtomicU32 = AtomicU32::new(0);
pub static RM_VIEWS: AtomicU32 = AtomicU32::new(0);
pub static RM_VIEWS_FREED: AtomicU32 = AtomicU32::new(0);
pub static RM_FILL_MS: AtomicU32 = AtomicU32::new(0);
pub static RM_READBACK_BAD: AtomicU32 = AtomicU32::new(0);
pub static RM_PROBES: AtomicU32 = AtomicU32::new(0);
pub static RM_BUSY: AtomicU32 = AtomicU32::new(0);
pub static RM_CLOSED: AtomicU32 = AtomicU32::new(0);
pub static RM_SOFT: AtomicU32 = AtomicU32::new(0);
pub static RM_REGFD_REFUSED: AtomicU32 = AtomicU32::new(0);
/// Times a retire stopped waiting for a frame copy to release its view lease and
/// unmapped anyway (`RmLeaseTmo`; must stay 0: the worker is joined before a retire).
pub static RM_LEASE_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the registry. PASSIVE only. Cheap to call when the knob
/// is off: nothing is written until the client has done something.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    let level = KNOB_LEVEL.load(Ordering::Relaxed);
    if level == 0 && RM_STEPS.load(Ordering::Relaxed) == 0 {
        return;
    }
    // Unread (between a transport's retirement and the worker's next pass): the
    // registry keeps what it has.
    if level != KNOB_UNREAD {
        rec(b"RmKnob", level);
    }
    // Level 5 runs no ring client: its own counters only.
    if level == helios_kmd_logic::rm_sysmem::LEVEL {
        sysmem::publish_counters();
        return;
    }
    rec(b"RmStatus", RM_STATUS.load(Ordering::Relaxed));
    rec(b"RmStep", RM_LAST_STEP.load(Ordering::Relaxed));
    rec(b"RmFail", RM_FAIL.load(Ordering::Relaxed));
    rec(b"RmDead", RM_DEAD.load(Ordering::Relaxed));
    rec(b"RmSteps", RM_STEPS.load(Ordering::Relaxed));
    rec(b"RmBringUp", RM_BRING_UPS.load(Ordering::Relaxed));
    rec(b"RmSurf", RM_SURFACES.load(Ordering::Relaxed));
    rec(b"RmSurfFree", RM_SURFACES_FREED.load(Ordering::Relaxed));
    rec(b"RmGem", RM_GEMS.load(Ordering::Relaxed));
    rec(b"RmView", RM_VIEWS.load(Ordering::Relaxed));
    rec(b"RmViewFree", RM_VIEWS_FREED.load(Ordering::Relaxed));
    rec(b"RmFillMs", RM_FILL_MS.load(Ordering::Relaxed));
    rec(b"RmRdBad", RM_READBACK_BAD.load(Ordering::Relaxed));
    rec(b"RmProbe", RM_PROBES.load(Ordering::Relaxed));
    rec(b"RmBusy", RM_BUSY.load(Ordering::Relaxed));
    rec(b"RmClosed", RM_CLOSED.load(Ordering::Relaxed));
    rec(b"RmSoft", RM_SOFT.load(Ordering::Relaxed));
    rec(b"RmRegFd", RM_REGFD_REFUSED.load(Ordering::Relaxed));
    rec(b"RmLeaseTmo", RM_LEASE_TIMEOUTS.load(Ordering::Relaxed));
    super::rm_foreign::publish_counters();
    rm_present::publish_counters();
}

// ---- the worker's entry ------------------------------------------------------------

/// The extent of the VidPn primary the display half has bound, if one is: the size the
/// scanout surface must have.
fn wanted_surface(adapter: &AdapterContext) -> Option<(u32, u32)> {
    if !adapter.display_half() {
        return None;
    }
    let wh = adapter.active_scanout_wh.load(Ordering::Acquire);
    if adapter.active_scanout_resource.load(Ordering::Acquire) == 0 || wh == 0 {
        return None;
    }
    Some(((wh >> 32) as u32, wh as u32))
}

/// The knob for this transport generation: one atomic load, and the registry read only
/// when [`forget`] reset it.
fn knob_level() -> u32 {
    let level = KNOB_LEVEL.load(Ordering::Relaxed);
    if level != KNOB_UNREAD {
        return level;
    }
    read_knob()
}

/// The registry read behind [`knob_level`], out of line: its frame is for the one pass
/// per transport generation that needs it.
#[inline(never)]
fn read_knob() -> u32 {
    let v = crate::diag::read_config_dword(crate::diag::knobs::KMD_RM_CLIENT, 0)
        .min(helios_kmd_logic::rm_sysmem::LEVEL);
    KNOB_LEVEL.store(v, Ordering::Relaxed);
    // Mirrored on EVERY read, 0 included: "nothing is written for the default" left a previous
    // run's value in the registry after the knob was set back to 0.
    crate::diag::record_named_bytes(b"RmKnob", v);
    v
}

/// Read `KmdRmClient` for this transport generation now and mirror it (StartDevice, after
/// [`forget`] reset it). PASSIVE.
pub(crate) fn reread_knob_at_start() -> u32 {
    read_knob()
}

/// One pass of the client, from the HPD worker's loop (PASSIVE). Does nothing at all
/// unless `KmdRmClient` is nonzero and a VidPn primary is bound; with the knob at 0 it
/// is one atomic load (the virtio lock is never touched). Not inlined: the worker's
/// frame must not grow by this function's locals.
#[inline(never)]
pub(crate) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    let level = knob_level();
    if level == 0 {
        return;
    }
    // Level 5 has no ring and no client steps on the worker (its service runs on the
    // creator's thread); the worker only flips what the screen shows.
    if level >= helios_kmd_logic::rm_sysmem::LEVEL {
        sysmem_flip::service(passive, adapter);
        return;
    }
    // No transport: nothing to do, and `retire_transport` already forgot the client.
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return;
    };
    if epoch == 0 {
        return;
    }
    let want = Want {
        level: level as u8,
        surface: wanted_surface(adapter),
    };

    // A new transport generation: every handle of the old one is gone. The kernel view
    // is unmapped first (retire normally did it already; this is the belt).
    {
        let mut views = rc::Views::default();
        let mut g = CLIENT.lock();
        let changed = g.epoch() != epoch;
        if changed {
            views = g.take_views();
            g.sync_epoch(epoch);
        }
        drop(g);
        if changed {
            // The ring of the old generation is gone with its views: so is the
            // presenter's memory of showing it.
            rm_present::reset();
        }
        unmap_views(&views);
    }

    let io = Io {
        passive,
        adapter,
        epoch,
        limit: None,
    };
    // Level 3: the presenter first, so a ring about to be torn down (a new extent) is
    // withdrawn from scanout before the steps close the GEM under it.
    if level >= 3 && !io.stopping() {
        rm_present::service(passive, adapter, epoch, want, false);
    }
    let mut did = 0usize;
    let mut more = false;
    for i in 0..STEPS_PER_PASS {
        // StopDevice joins this worker for a bounded time: begin no step once it asked.
        // What the client holds open stays in the NVRM tables, and the stop sweep
        // closes it.
        if io.stopping() {
            break;
        }
        let (action, snapshot) = {
            let g = CLIENT.lock();
            (g.next(want), *g)
        };
        match action {
            Action::Idle => break,
            Action::Dead => {
                // `take_cleanup` is empty once it has run: this is safe every pass.
                cleanup(&io);
                break;
            }
            Action::Step(step) => {
                RM_LAST_STEP.store(step as u32, Ordering::Relaxed);
                // Before the step, so a step that wedges the thread names itself.
                crate::diag::record_named_bytes(b"RmStep", step as u32);
                RM_STEPS.fetch_add(1, Ordering::Relaxed);
                let result = io.perform(step, &snapshot, want);
                did += 1;
                // The probe's source is held by another owner: back off to the next
                // worker pass instead of asking again at once.
                let busy = matches!(&result, Err(f) if f.kind == FailKind::Busy);
                if !apply(epoch, step, result, &snapshot) {
                    // The generation changed under the step: it is moot.
                    break;
                }
                if CLIENT.lock().is_dead() {
                    cleanup(&io);
                    break;
                }
                if busy {
                    break;
                }
                more = i + 1 == STEPS_PER_PASS;
            }
        }
    }
    if did != 0 {
        let word = CLIENT.lock().status_word();
        RM_STATUS.store(word, Ordering::Relaxed);
        publish_counters();
    }
    // Level 3: the ring is the desktop's scanout. After the client's own steps, so a
    // ring completed in this very pass is used in it; a no-op until the ring is whole.
    if level >= 3 && !io.stopping() {
        rm_present::service(passive, adapter, epoch, want, true);
    }
    if more {
        // The rest of the bring-up goes on after the worker has done its other duties.
        adapter.signal_hpd();
    }
}

/// Report a finished step. `false` if the client no longer belongs to `epoch` (the
/// result is dropped, and a kernel view it made is unmapped).
fn apply(epoch: u64, step: Step, result: Result<Out, Fail>, before: &Client) -> bool {
    let mut g = CLIENT.lock();
    if g.epoch() != epoch {
        drop(g);
        if let Ok(Out::Mapped(va, len)) = result {
            unmap_view(Some((va, len)));
        }
        return false;
    }
    let was_up = g.bring_up_done();
    let had_surface = g.ready_surface().is_some();
    if let Err(f) = &result {
        if f.kind == FailKind::Busy {
            RM_BUSY.fetch_add(1, Ordering::Relaxed);
        }
    }
    let soft_before = before.soft_errors();
    g.finish(step, result);
    let soft_after = g.soft_errors();
    if soft_after > soft_before {
        RM_SOFT.fetch_add(soft_after - soft_before, Ordering::Relaxed);
    }
    if !was_up && g.bring_up_done() {
        RM_BRING_UPS.fetch_add(1, Ordering::Relaxed);
    }
    match step {
        Step::GemImport if g.gem() != 0 => {
            RM_GEMS.fetch_add(1, Ordering::Relaxed);
        }
        Step::CloseExportCh if !had_surface && g.ready_surface().is_some() => {
            RM_SURFACES.fetch_add(1, Ordering::Relaxed);
        }
        Step::FreeMemory if g.surface().is_none() => {
            RM_SURFACES_FREED.fetch_add(1, Ordering::Relaxed);
        }
        Step::KernelMap if g.view().is_some() => {
            RM_VIEWS.fetch_add(1, Ordering::Relaxed);
        }
        Step::ForeignImport if g.foreign() != 0 => {
            super::rm_foreign::RM_FG_IMPORTED.fetch_add(1, Ordering::Relaxed);
        }
        Step::ScanoutPresent if g.probe() == rc::Probe::Shown => {
            RM_PROBES.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    let died = g.failure().map(|f| f.pack());
    // The registry write is a PASSIVE-only call: never under the spinlock.
    drop(g);
    if let Some(pack) = died {
        RM_FAIL.store(pack, Ordering::Relaxed);
        RM_DEAD.fetch_add(1, Ordering::Relaxed);
        crate::diag::record_named_bytes(b"RmFail", pack);
    }
    true
}

/// A dead client: unmap its view, then close what it opened (closing the control file
/// frees every RM client made on it; closing the DRM file drops its GEM handles).
/// Nothing is retried; the client stays dead until the next transport generation.
fn cleanup(io: &Io<'_>) {
    let (views, handles) = {
        let mut g = CLIENT.lock();
        (g.take_views(), g.take_cleanup())
    };
    wait_for_lease(io.passive);
    unmap_views(&views);
    // The foreign resources it made (level 4) are Venus blobs owned by the KMD's own
    // owner: reclaim them before their DRM file goes (the host import holds its own
    // dma-buf reference, so the order is only tidiness).
    // Skipped when StopDevice asked the worker to go (the transport sweep reclaims the
    // blobs), and bounded as a whole by one step's allowance otherwise.
    if !io.stopping() {
        super::rm_foreign::release_all(io.passive, io.adapter, TIMEOUT_MS);
    }
    for &h in handles.as_slice() {
        // Bounded like the step loop: once StopDevice asks, or the transport has
        // failed, stop sending. The handles are still in the NVRM tables, and the
        // sweep closes them (or drops them from the tables when the host is gone).
        if io.stopping() {
            break;
        }
        match io.try_close(h) {
            Ok(true) => {
                RM_CLOSED.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(f) if f.kind == FailKind::Transport => break,
            Err(_) => {}
        }
    }
    if handles.as_slice().is_empty() {
        return;
    }
    publish_counters();
}

// ---- retirement --------------------------------------------------------------------

/// The transport is about to be retired (`nvrm::close_all_on_host`, first thing, before
/// it even asks whether the host is alive): unmap the kernel view of the RM mapping, so
/// no virtual address outlives the window range the host is about to release. The
/// client's handles are in the NVRM tables under [`DeviceOwner::KMD_RM`] and are
/// closed by that same sweep.
#[inline(never)]
pub(crate) fn retire_begin(passive: PassiveLevel) {
    let views = CLIENT.lock().take_views();
    // A frame copy that leased a view before the views were taken is still writing
    // through it: wait for it (it is the worker, which StopDevice joined before this
    // in every normal path) so no virtual address outlives its window range.
    wait_for_lease(passive);
    unmap_views(&views);
    // The copy-engine channel's kernel views likewise (one load when there is none).
    ce_channel::drop_views();
}

/// The transport is gone: forget everything (the sweep closed the host side). A view
/// the worker recorded after [`retire_begin`] looked (a start without a stop, the one
/// path on which a worker can still be running) is unmapped here, so none outlives it.
#[inline(never)]
pub(crate) fn forget() {
    let views = {
        let mut g = CLIENT.lock();
        let views = g.take_views();
        g.forget();
        views
    };
    // A copy is not in flight any more (the worker was joined, or `retire_begin` waited
    // for it); a lease flag left over would stall the next generation's retire.
    VIEW_LEASED.store(0, Ordering::Release);
    unmap_views(&views);
    rm_present::reset();
    sysmem::forget();
    ce_channel::forget();
    // The next transport generation reads the knob again (once).
    KNOB_LEVEL.store(KNOB_UNREAD, Ordering::Relaxed);
}

#[inline(never)]
fn unmap_view(view: Option<(u64, u64)>) {
    if let Some((va, len)) = view {
        // SAFETY: `va`/`len` came from `MmMapIoSpace` in `kernel_map` and were taken
        // out of the client (or never recorded) exactly once, so this unmaps it
        // exactly once; PASSIVE (the worker, StopDevice or StartDevice).
        unsafe { MmUnmapIoSpace(va as *mut c_void, len) };
        RM_VIEWS_FREED.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline(never)]
fn unmap_views(views: &rc::Views) {
    for v in views.as_slice() {
        unmap_view(Some(*v));
    }
}

// ---- view leases (the presenter's copy) -----------------------------------------------

/// Nonzero while the presenter is writing a frame through one of the client's views.
/// Set under `CLIENT`'s lock (so it is ordered against `take_views`), cleared by the
/// worker when the copy ends.
static VIEW_LEASED: AtomicU32 = AtomicU32::new(0);

/// A slot of the ring, leased for a frame copy: what the presenter needs, copied out.
#[derive(Clone, Copy)]
pub(super) struct Lease {
    pub drm: u32,
    pub gem: u32,
    pub view: (u64, u64),
    pub layout: rc::SurfaceLayout,
}

/// Whether the client's ring is complete for `want` in transport generation `epoch`.
pub(super) fn ring_ready(epoch: u64, want: Want) -> bool {
    let g = CLIENT.lock();
    g.epoch() == epoch && g.presentable(want)
}

/// The DRM file and layout the resident source registers with (slot 0's: every slot of
/// a ring has the same).
pub(super) fn ring_identity(epoch: u64) -> Option<(u32, rc::SurfaceLayout)> {
    let g = CLIENT.lock();
    if g.epoch() != epoch || g.is_dead() {
        return None;
    }
    g.slot(0).map(|s| (g.drm(), s.layout))
}

/// Lease slot `slot` for a frame copy: `None` unless the ring is whole in this
/// generation. The lease keeps the retire path from unmapping the view until
/// [`end_lease`].
pub(super) fn lease_slot(epoch: u64, want: Want, slot: usize) -> Option<Lease> {
    let g = CLIENT.lock();
    if g.epoch() != epoch || !g.presentable(want) {
        return None;
    }
    let s = g.slot(slot)?;
    VIEW_LEASED.store(1, Ordering::Release);
    Some(Lease {
        drm: g.drm(),
        gem: s.gem,
        view: s.view,
        layout: s.layout,
    })
}

pub(super) fn end_lease() {
    VIEW_LEASED.store(0, Ordering::Release);
}

/// Wait (bounded) for the presenter to end a lease. PASSIVE.
#[inline(never)]
fn wait_for_lease(passive: PassiveLevel) {
    let mut waited = 0u32;
    while VIEW_LEASED.load(Ordering::Acquire) != 0 {
        if waited >= LEASE_WAIT_MS {
            RM_LEASE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        super::ctrl::sleep_ms(passive, 1);
        waited += 1;
    }
}

/// How long a retire waits for a frame copy (a whole-frame copy takes tens of ms).
const LEASE_WAIT_MS: u32 = 2_000;

// ---- the I/O -----------------------------------------------------------------------

fn page_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

/// How a refused or failed forward looks to the machine.
fn fail_of(r: Refusal) -> Fail {
    match r {
        Refusal::MsgType => Fail::new(FailKind::Refused, 1),
        Refusal::NotOwned => Fail::new(FailKind::Refused, 2),
        Refusal::NoResources => Fail::new(FailKind::Refused, 3),
        Refusal::Forbidden => Fail::new(FailKind::Refused, 4),
        Refusal::BadRange => Fail::new(FailKind::Refused, 5),
        Refusal::Transport(VirtioError::Timeout) => Fail::new(FailKind::Transport, 1),
        Refusal::Transport(VirtioError::QueueFull | VirtioError::OutOfMemory) => {
            Fail::new(FailKind::Transport, 2)
        }
        Refusal::Transport(_) => Fail::new(FailKind::Transport, 3),
    }
}

/// One step's context: the token, the adapter, and the generation the client belongs to.
struct Io<'a> {
    passive: PassiveLevel,
    adapter: &'a AdapterContext,
    epoch: u64,
    /// A deadline shared by every message this `Io` sends (level 5's creation and its undo,
    /// `sysmem`): each waits at most what is left of it, and none is sent once it is spent.
    /// `None` (the ring client): every message has [`TIMEOUT_MS`] to itself.
    limit: Option<SweepBudget>,
}

impl Io<'_> {
    /// The same I/O under another deadline (the undo of a creation whose own is spent).
    fn with_limit(&self, limit: Option<SweepBudget>) -> Io<'_> {
        Io {
            passive: self.passive,
            adapter: self.adapter,
            epoch: self.epoch,
            limit,
        }
    }

    /// How long the next message may wait: [`TIMEOUT_MS`], or what is left of the deadline
    /// (at most [`TIMEOUT_MS`]); `None` once a deadline is spent.
    fn message_timeout_ms(&self) -> Option<u64> {
        match &self.limit {
            None => Some(TIMEOUT_MS),
            Some(b) => b.call_timeout_ms(crate::adapter::foreign_scanout::now_100ns()),
        }
    }

    /// Whether a deadline was set and is spent.
    fn limit_spent(&self) -> bool {
        self.limit
            .is_some_and(|b| b.expired(crate::adapter::foreign_scanout::now_100ns()))
    }

    /// Forward one host message as the KMD's owner and return the reply length. A reply
    /// from a different transport generation than the client's is a failure: the
    /// handles it names are not this client's any more. With the deadline spent nothing is
    /// sent (a `Transport` failure, code 0xE1).
    fn send(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, Fail> {
        let Some(timeout_ms) = self.message_timeout_ms() else {
            return Err(Fail::new(FailKind::Transport, 0xE1));
        };
        let mut seen = None;
        let n = nvrm::forward(
            self.passive,
            self.adapter,
            KMD,
            req,
            resp,
            timeout_ms,
            0,
            0,
            &mut seen,
        )
        .map_err(fail_of)?;
        if seen.is_some_and(|g| g != self.epoch) {
            return Err(Fail::new(FailKind::Transport, 0xE0));
        }
        Ok(n)
    }

    /// Build and send an `Ioctl` of `cmd` on backend handle `handle`, with `data` and
    /// `nested`. The request is built on the stack when it fits [`REQUEST_MAX`], else on
    /// the heap (only `CARD_INFO` needs that).
    fn exchange(
        &self,
        handle: u32,
        cmd: u32,
        data: &[u8],
        nested: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Fail> {
        let total = rc::MSG_HDR + rc::IOCTL_REQ + data.len() + nested.len();
        if total <= REQUEST_MAX {
            let mut req = [0u8; REQUEST_MAX];
            let n = rc::build_ioctl(&mut req, handle, cmd, data, nested)
                .ok_or(Fail::new(FailKind::Parse, 1))?;
            return self.send(req.get(..n).ok_or(Fail::new(FailKind::Parse, 1))?, resp);
        }
        let mut req = Vec::<u8>::new();
        if req.try_reserve_exact(total).is_err() {
            return Err(Fail::new(FailKind::Os, 1));
        }
        req.resize(total, 0);
        let n = rc::build_ioctl(&mut req, handle, cmd, data, nested)
            .ok_or(Fail::new(FailKind::Parse, 1))?;
        self.send(req.get(..n).ok_or(Fail::new(FailKind::Parse, 1))?, resp)
    }

    /// `Open` of `device_type`; the backend handle.
    fn open_file(&self, device_type: u32) -> Result<u32, Fail> {
        let mut req = [0u8; 32];
        let n = rc::build_open(&mut req, device_type).ok_or(Fail::new(FailKind::Parse, 2))?;
        let mut resp = [0u8; 64];
        let len = self.send(
            req.get(..n).ok_or(Fail::new(FailKind::Parse, 2))?,
            &mut resp,
        )?;
        let reply = resp.get(..len).ok_or(Fail::new(FailKind::Parse, 2))?;
        if let Some(h) = rc::parse_open_reply(reply) {
            return Ok(h);
        }
        match rc::reply_status(reply) {
            Some(s) if s != 0 => Err(Fail::new(FailKind::Host, s.unsigned_abs())),
            _ => {
                // The host opened something the reply cannot name usably: the
                // transport table recorded it (any nonzero handle), so close it.
                if let Some(h) = rc::get32(reply, 4).filter(|h| *h != 0) {
                    self.close_file(h);
                }
                Err(Fail::new(FailKind::Parse, 2))
            }
        }
    }

    /// StopDevice has asked the worker to go (`stop_hpd` joins it for a bounded time):
    /// start nothing more.
    fn stopping(&self) -> bool {
        self.adapter.hpd_stop.load(Ordering::Acquire) != 0
    }

    /// `Close` of `handle`: whether the host closed it, or why nothing was asked
    /// (the forward failed: a transport failure ends a sequence of closes).
    fn try_close(&self, handle: u32) -> Result<bool, Fail> {
        let mut req = [0u8; 32];
        let Some(n) = rc::build_close(&mut req, handle) else {
            return Ok(false);
        };
        let mut resp = [0u8; 64];
        let Some(req) = req.get(..n) else {
            return Ok(false);
        };
        let len = self.send(req, &mut resp)?;
        Ok(resp
            .get(..len)
            .and_then(rc::reply_status)
            .is_some_and(|s| s == 0))
    }

    /// `Close` of `handle`; whether the host closed it.
    fn close_file(&self, handle: u32) -> bool {
        self.try_close(handle).unwrap_or(false)
    }

    /// `NV_ESC_RM_ALLOC` of `class` as `h_new` under `parent`, with `params`: the reply.
    #[allow(clippy::too_many_arguments)]
    fn rm_alloc(
        &self,
        ctl: u32,
        root: u32,
        parent: u32,
        h_new: u32,
        class: u32,
        params: &[u8],
        resp: &mut [u8],
    ) -> Result<usize, Fail> {
        let block = rc::nvos64(root, parent, h_new, class, params.len() as u32);
        let n = self.exchange(ctl, rc::nv_cmd(rc::ESC_RM_ALLOC, 48), &block, params, resp)?;
        // The status word has to be checked here for every caller: RM said no, or yes.
        rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 3))?,
            rc::NVOS64_STATUS_AT,
        )
        .map_err(Fail::from)?;
        Ok(n)
    }

    /// Perform one step. Each arm is its own function, so the stack buffers of one
    /// step never add to another's frame (this runs on a worker thread, under a deep
    /// transport call).
    fn perform(&self, step: Step, c: &Client, want: Want) -> Result<Out, Fail> {
        match step {
            Step::OpenCtl => self.open_file(rc::DEV_CTL).map(Out::Handle),
            Step::VersionQuery => self.version_query(c),
            Step::VersionStrict => self.version_strict(c),
            Step::CardInfo => self.card_info(c),
            Step::AllocRoot => self.alloc_root(c),
            Step::OpenGpu => self.open_file(c.minor()).map(Out::Handle),
            Step::RegisterGpuFd => self.register_fd(c.gpu(), c.ctl()),
            Step::AllocDevice => self.alloc_device(c),
            Step::AllocSubdevice => self.alloc_subdevice(c),
            Step::SysFiles => self.sys_files(c),
            Step::OpenDrm => self
                .open_file(rc::DEV_DRI_BASE.saturating_add(c.dri_index()))
                .map(Out::Handle),

            Step::AllocMemory => self.alloc_memory(c, want),
            Step::OpenExportCh => self.open_file(rc::DEV_CTL).map(Out::Handle),
            Step::ExportToFd => self.export_to_fd(c),
            Step::GemImport => self.gem_import(c),
            Step::CloseExportCh => self.close_checked(c.export_ch()),
            // An undo: the export file closed with the stage kept (the extent changed
            // before it was imported); a refusal is counted, not fatal.
            Step::CloseExportChUndo => self.close_checked(c.export_ch()),
            Step::GemClose => self.gem_close(c),
            Step::FreeMemory => self.free_memory(c),

            Step::OpenMapCh => self.open_file(c.minor()).map(Out::Handle),
            Step::RegisterMapFd => self.register_fd(c.map_ch(), c.ctl()),
            Step::RmMapMemory => self.rm_map_memory(c),
            Step::HostMmap => self.host_mmap(c),
            Step::KernelMap => self.kernel_map(c),
            Step::KernelUnmap => {
                // The machine still holds the view: take it back for the unmap.
                let view = CLIENT.lock().take_view();
                unmap_view(view);
                Ok(Out::Unit)
            }
            Step::HostMunmap => {
                let (id, _) = c.view_host();
                nvrm::release_host_map_within(
                    self.passive,
                    self.adapter,
                    c.map_ch(),
                    id,
                    TIMEOUT_MS,
                )
                .map(|()| Out::Unit)
                .map_err(|e| fail_of(Refusal::Transport(e)))
            }
            Step::RmUnmapMemory => self.rm_unmap_memory(c),
            Step::CloseMapCh => self.close_checked(c.map_ch()),

            Step::FillPattern => self.fill_pattern(c),
            Step::ScanoutSet => self.scanout_set(c),
            Step::ScanoutPresent => self.scanout_present(c),

            // Pure moves between the ring's slots: nothing to ask the host.
            Step::Park | Step::Unpark => Ok(Out::Unit),

            // Level 4: the surface as a foreign (Venus) resource under the KMD's owner.
            Step::ForeignImport => {
                super::rm_foreign::import_surface(self.passive, self.adapter, c, TIMEOUT_MS)
            }
            Step::ForeignRelease => super::rm_foreign::release_surface(
                self.passive,
                self.adapter,
                c.foreign(),
                TIMEOUT_MS,
            ),
        }
    }

    // ---- bring-up ------------------------------------------------------------------

    #[inline(never)]
    fn version_query(&self, c: &Client) -> Result<Out, Fail> {
        let blank = [0u8; rc::VERSION_STR_BYTES];
        let data = rc::version_params(rc::VERSION_CMD_QUERY, &blank);
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_CHECK_VERSION_STR, rc::VERSION_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        // No string is not a failure, and neither is a QUERY the host refused or
        // answered badly: librmclient ignores it too (rmclient.c), and the strict
        // check below is then skipped, as a RM that never enforced it would not
        // notice either. A transport failure or a refused forward (the `?` above)
        // still ends the client.
        Ok(Out::Version(rc::version_from_query_reply(
            resp.get(..n).unwrap_or(&[]),
        )))
    }

    #[inline(never)]
    fn version_strict(&self, c: &Client) -> Result<Out, Fail> {
        let v = c.version();
        if v[0] == 0 {
            return Ok(Out::Unit);
        }
        let data = rc::version_params(rc::VERSION_CMD_STRICT, &v);
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_CHECK_VERSION_STR, rc::VERSION_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        // RM answers a mismatch with -EINVAL, which the host reports in the header.
        rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 5))?)
            .map(|_| Out::Unit)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 5),
            })
    }

    #[inline(never)]
    fn card_info(&self, c: &Client) -> Result<Out, Fail> {
        let data = [0u8; rc::CARD_INFO_BYTES];
        let mut resp = Vec::<u8>::new();
        let cap = rc::REPLY_DATA + rc::CARD_INFO_BYTES + 64;
        if resp.try_reserve_exact(cap).is_err() {
            return Err(Fail::new(FailKind::Os, 2));
        }
        resp.resize(cap, 0);
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_CARD_INFO, rc::CARD_INFO_BYTES as u32),
            &data,
            &[],
            &mut resp,
        )?;
        let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 6))?)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 6),
            })?;
        rc::parse_card_info(reply.data)
            .map(Out::Card)
            .ok_or(Fail::new(FailKind::Parse, 7))
    }

    #[inline(never)]
    fn alloc_root(&self, c: &Client) -> Result<Out, Fail> {
        // RM chooses the client handle (`hObjectNew = 0`) and answers it.
        let block = rc::alloc_root();
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_RM_ALLOC, 48),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 8))?,
            rc::NVOS64_STATUS_AT,
        )
        .map_err(Fail::from)?;
        rc::alloc_new_handle(&reply)
            .map(Out::Handle)
            .ok_or(Fail::new(FailKind::Parse, 9))
    }

    /// `NV_ESC_REGISTER_FD`: tie `channel` to the control file. librmclient ignores its
    /// failure ("harmless: the channel still keeps the GPU open"), and so does this.
    #[inline(never)]
    fn register_fd(&self, channel: u32, ctl: u32) -> Result<Out, Fail> {
        let data = rc::register_fd_params(ctl);
        let mut resp = [0u8; REPLY_MAX];
        let cmd = rc::nv_cmd(rc::ESC_REGISTER_FD, 4);
        match self.exchange(channel, cmd, &data, &[], &mut resp) {
            Ok(n) => {
                let ok = resp
                    .get(..n)
                    .is_some_and(|r| rc::parse_ioctl_reply(r).is_ok());
                if !ok {
                    RM_REGFD_REFUSED.fetch_add(1, Ordering::Relaxed);
                }
            }
            // A transport that is gone fails the next step anyway; a host that
            // refuses is counted and ignored.
            Err(f) if f.kind == FailKind::Transport => return Err(f),
            Err(_) => {
                RM_REGFD_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Out::Unit)
    }

    #[inline(never)]
    fn alloc_device(&self, c: &Client) -> Result<Out, Fail> {
        let params = rc::device_params();
        let mut resp = [0u8; REPLY_MAX];
        self.rm_alloc(
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

    #[inline(never)]
    fn alloc_subdevice(&self, c: &Client) -> Result<Out, Fail> {
        let params = rc::subdevice_params();
        let mut resp = [0u8; REPLY_MAX];
        self.rm_alloc(
            c.ctl(),
            c.root(),
            rc::H_DEVICE,
            rc::H_SUBDEVICE,
            rc::NV20_SUBDEVICE_0,
            &params,
            &mut resp,
        )
        .map(|_| Out::Unit)
    }

    /// `GetSysFiles`: which DRI node belongs to the card. The reply is a bare stream
    /// (no `MsgHeader`) sized as librmclient and the Linux module size it.
    #[inline(never)]
    fn sys_files(&self, c: &Client) -> Result<Out, Fail> {
        let mut req = [0u8; 32];
        let n = rc::build_get_sys_files(&mut req).ok_or(Fail::new(FailKind::Parse, 10))?;
        let mut resp = Vec::<u8>::new();
        if resp.try_reserve_exact(rc::SYS_FILES_CAP).is_err() {
            return Err(Fail::new(FailKind::Os, 3));
        }
        resp.resize(rc::SYS_FILES_CAP, 0);
        let len = self.send(
            req.get(..n).ok_or(Fail::new(FailKind::Parse, 10))?,
            &mut resp,
        )?;
        let stream = resp.get(..len).ok_or(Fail::new(FailKind::Parse, 10))?;
        let mut nodes = [rc::DriNode::default(); MAX_DRI];
        let k = rc::parse_dri_section(stream, &mut nodes);
        rc::pick_dri(nodes.get(..k).unwrap_or(&[]), c.gpu_id())
            .map(Out::Dri)
            .ok_or(Fail::new(FailKind::Parse, 11))
    }

    // ---- the surface ---------------------------------------------------------------

    #[inline(never)]
    fn alloc_memory(&self, c: &Client, want: Want) -> Result<Out, Fail> {
        let (w, h) = want.surface.ok_or(Fail::new(FailKind::Layout, 1))?;
        let layout = rc::surface_layout(w, h).ok_or(Fail::new(FailKind::Layout, 2))?;
        let params = rc::mem_alloc_params(c.root(), &layout);
        let mut resp = [0u8; REPLY_MAX];
        let n = self.rm_alloc(
            c.ctl(),
            c.root(),
            rc::H_DEVICE,
            c.next_memory_handle(),
            rc::NV01_MEMORY_LOCAL_USER,
            &params,
            &mut resp,
        )?;
        // RM writes what it made back into the parameter block.
        let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 12))?)
            .map_err(|_| Fail::new(FailKind::Parse, 12))?;
        rc::adopt_alloc_reply(&layout, reply.nested)
            .map(Out::Mem)
            .map_err(|e| Fail::new(FailKind::Layout, 0x10 + e as u32))
    }

    #[inline(never)]
    fn export_to_fd(&self, c: &Client) -> Result<Out, Fail> {
        let (_, mem) = c.surface().ok_or(Fail::new(FailKind::Parse, 13))?;
        let params = rc::export_params(rc::H_DEVICE, mem, c.export_ch());
        let block = rc::nvos54(
            c.root(),
            c.root(),
            rc::NV0000_CTRL_CMD_EXPORT_OBJECT_TO_FD,
            params.len() as u32,
        );
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_RM_CONTROL, 32),
            &block,
            &params,
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 14))?,
            rc::NVOS54_STATUS_AT,
        )
        .map(|_| Out::Unit)
        .map_err(Fail::from)
    }

    #[inline(never)]
    fn gem_import(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, _) = c.surface().ok_or(Fail::new(FailKind::Parse, 15))?;
        let data = rc::gem_import_params(layout.size);
        let nested = rc::nvkms_import_params(c.export_ch());
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.drm(),
            rc::DRM_IOCTL_GEM_IMPORT_NVKMS,
            &data,
            &nested,
            &mut resp,
        )?;
        let reply = rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 16))?)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 16),
            })?;
        rc::gem_handle(&reply)
            .map(Out::Gem)
            .ok_or(Fail::new(FailKind::Parse, 17))
    }

    /// `Close` of a file the machine asked to have closed, as a step: a refusal is a
    /// failure (it leaves a handle open).
    fn close_checked(&self, handle: u32) -> Result<Out, Fail> {
        if handle == 0 {
            return Ok(Out::Unit);
        }
        if self.close_file(handle) {
            Ok(Out::Unit)
        } else {
            Err(Fail::new(FailKind::Host, 0xC1))
        }
    }

    #[inline(never)]
    fn gem_close(&self, c: &Client) -> Result<Out, Fail> {
        // The KMD's own flip source names this GEM: end it first (the desktop is
        // restored), so no flip can name a closed object. A no-op when it is not live.
        let _ = self.adapter.foreign_scanout_release(KMD, Some(c.drm()));
        let data = rc::gem_close_params(c.gem());
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(c.drm(), rc::DRM_IOCTL_GEM_CLOSE, &data, &[], &mut resp)?;
        rc::parse_ioctl_reply(resp.get(..n).ok_or(Fail::new(FailKind::Parse, 18))?)
            .map(|_| Out::Unit)
            .map_err(|e| match e {
                rc::ReplyError::Host(s) => Fail::new(FailKind::Host, s.unsigned_abs()),
                rc::ReplyError::Short => Fail::new(FailKind::Parse, 18),
            })
    }

    #[inline(never)]
    fn free_memory(&self, c: &Client) -> Result<Out, Fail> {
        let (_, mem) = c.surface().ok_or(Fail::new(FailKind::Parse, 19))?;
        let block = rc::nvos00(c.root(), rc::H_DEVICE, mem);
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_RM_FREE, 16),
            &block,
            &[],
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 20))?,
            rc::NVOS00_STATUS_AT,
        )
        .map(|_| Out::Unit)
        .map_err(Fail::from)
    }

    // ---- the CPU view --------------------------------------------------------------
    //
    // The channel-per-mapping protocol of librmclient's Windows transport
    // (`win_map_memory`): a fresh GPU channel, tied to the control file, is armed by
    // `NV_ESC_RM_MAP_MEMORY` (issued on the control file, naming the channel), and the
    // host's `Mmap` of the channel answers where in the RM window the pages are.

    #[inline(never)]
    fn rm_map_memory(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, mem) = c.surface().ok_or(Fail::new(FailKind::Parse, 21))?;
        let block = rc::nvos33_with_fd(
            c.root(),
            rc::H_DEVICE,
            mem,
            0,
            page_up(layout.size),
            c.map_ch(),
        );
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_RM_MAP_MEMORY, rc::NVOS33_FD_BYTES as u32),
            &block,
            &[],
            &mut resp,
        )?;
        let reply = rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 22))?,
            rc::NVOS33_STATUS_AT,
        )
        .map_err(Fail::from)?;
        rc::map_cookie(&reply)
            .map(Out::Cookie)
            .ok_or(Fail::new(FailKind::Parse, 23))
    }

    #[inline(never)]
    fn rm_unmap_memory(&self, c: &Client) -> Result<Out, Fail> {
        let (_, mem) = c.surface().ok_or(Fail::new(FailKind::Parse, 24))?;
        let block = rc::nvos34(c.root(), rc::H_DEVICE, mem, c.view_cookie());
        let mut resp = [0u8; REPLY_MAX];
        let n = self.exchange(
            c.ctl(),
            rc::nv_cmd(rc::ESC_RM_UNMAP_MEMORY, 32),
            &block,
            &[],
            &mut resp,
        )?;
        rc::rm_reply(
            resp.get(..n).ok_or(Fail::new(FailKind::Parse, 25))?,
            rc::NVOS34_STATUS_AT,
        )
        .map(|_| Out::Unit)
        .map_err(Fail::from)
    }

    #[inline(never)]
    fn host_mmap(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, _) = c.surface().ok_or(Fail::new(FailKind::Parse, 26))?;
        let size = page_up(layout.size);
        // Offset 0 on the freshly armed channel, as librmclient does: the mapping is
        // the channel's own.
        // Bounded like every message of a step: this runs on the worker StopDevice joins.
        match nvrm::host_mmap_within(
            self.passive,
            self.adapter,
            KMD,
            c.map_ch(),
            true,
            0,
            size,
            TIMEOUT_MS,
        ) {
            Ok(m) if m.size >= size => Ok(Out::HostMapped(m.host_id, m.offset)),
            Ok(m) => {
                // Too small: give the mapping back before failing.
                let _ = nvrm::release_host_map_within(
                    self.passive,
                    self.adapter,
                    c.map_ch(),
                    m.host_id,
                    TIMEOUT_MS,
                );
                Err(Fail::new(FailKind::Layout, 0x20))
            }
            Err(MapRefusal::Host(errno)) => Err(Fail::new(FailKind::Host, errno.unsigned_abs())),
            Err(MapRefusal::Transport(e)) => Err(fail_of(Refusal::Transport(e))),
            Err(MapRefusal::NotOwned) => Err(Fail::new(FailKind::Refused, 2)),
            Err(MapRefusal::BadRange) => Err(Fail::new(FailKind::Refused, 5)),
            Err(MapRefusal::NoResources) => Err(Fail::new(FailKind::Refused, 3)),
        }
    }

    #[inline(never)]
    fn kernel_map(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, _) = c.surface().ok_or(Fail::new(FailKind::Parse, 27))?;
        let size = page_up(layout.size);
        let (_, off) = c.view_host();
        // `minor` (<= 254) is never UVM's 256, so this is the RM window.
        let region = nvrm::region_for(self.adapter, c.minor()).ok_or(Fail::new(FailKind::Os, 4))?;
        if off.checked_add(size).is_none() {
            return Err(Fail::new(FailKind::Layout, 0x21));
        }
        // The physical address of the span, in checked 64-bit arithmetic (the window is the
        // GPU's BAR1: 32 GiB and up, above 4 GiB in guest physical space).
        let Some(phys) =
            helios_kmd_logic::window_units::place(region.base, region.len, off, size)
        else {
            return Err(Fail::new(FailKind::Layout, 0x22));
        };
        let mut pa: PHYSICAL_ADDRESS = unsafe { core::mem::zeroed() };
        pa.QuadPart = phys as i64;
        // SAFETY: PASSIVE (the HPD worker); `region.base + off .. + size` lies inside the
        // window the host just placed this mapping in (checked above), page aligned.
        // Write-combined as the user-mode map of the same memory is (the Linux module's
        // default for BAR memory); a read of it is slow by nature, see the doc.
        let va = unsafe { MmMapIoSpace(pa, size, _MEMORY_CACHING_TYPE::MmWriteCombined) };
        if va.is_null() {
            return Err(Fail::new(FailKind::Os, 5));
        }
        Ok(Out::Mapped(va as u64, size))
    }

    // ---- the probe -----------------------------------------------------------------

    /// Paint the probe picture through the kernel view, and read a sample back (like
    /// `crm_scanout_smoke`): a view that does not hold what was written is counted
    /// (`RmRdBad`), not fatal, because what is on screen is the evidence that matters.
    #[inline(never)]
    fn fill_pattern(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, _, _) = c.ready_surface().ok_or(Fail::new(FailKind::Parse, 28))?;
        let (va, len) = c.view().ok_or(Fail::new(FailKind::Parse, 29))?;
        let (w, h, pitch) = (layout.width, layout.height, layout.pitch);
        if u64::from(pitch) * u64::from(h) > len || va == 0 {
            return Err(Fail::new(FailKind::Layout, 0x23));
        }
        let started = crate::adapter::foreign_scanout::now_100ns();
        let base = va as *mut u8;
        for y in 0..h {
            // SAFETY: row `y` is `pitch` bytes at `y * pitch`, inside the mapped
            // `len` (checked above); `x < w` and `w * 4 <= pitch`; volatile because
            // the memory is device memory (a write-combined BAR mapping).
            let row = unsafe { base.add(y as usize * pitch as usize) } as *mut u32;
            for x in 0..w {
                unsafe {
                    row.add(x as usize)
                        .write_volatile(rc::pattern_pixel(x, y, w, h))
                };
            }
        }
        // Drain the write-combining buffers before anything reads or the host flips.
        // SAFETY: SSE2 is baseline on x86_64.
        unsafe { core::arch::x86_64::_mm_sfence() };
        let mut bad = 0u32;
        let mut y = 0u32;
        while y < h {
            let mut x = 0u32;
            while x < w {
                // SAFETY: as above.
                let got = unsafe {
                    ((base.add(y as usize * pitch as usize)) as *const u32)
                        .add(x as usize)
                        .read_volatile()
                };
                if got != rc::pattern_pixel(x, y, w, h) {
                    bad += 1;
                }
                x += 101;
            }
            y += 37;
        }
        let ms = crate::adapter::foreign_scanout::now_100ns().wrapping_sub(started) / 10_000;
        RM_FILL_MS.store(ms.min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
        RM_READBACK_BAD.fetch_add(bad, Ordering::Relaxed);
        Ok(Out::Unit)
    }

    /// Take scanout 0 for the KMD's own source (the foreign scanout state machine, with
    /// the KMD's owner token): from here the desktop's host flush is withheld until the
    /// source ends.
    #[inline(never)]
    fn scanout_set(&self, c: &Client) -> Result<Out, Fail> {
        let (layout, _, _) = c.ready_surface().ok_or(Fail::new(FailKind::Parse, 30))?;
        match self.adapter.foreign_scanout_set(
            KMD,
            c.drm(),
            self.epoch,
            rc::flip_layout(&layout),
            PROBE_LAPSE_MS,
        ) {
            Ok(_) => Ok(Out::Unit),
            Err(SetError::Busy) => Err(Fail::new(FailKind::Busy, 0)),
            Err(SetError::Layout(_)) => Err(Fail::new(FailKind::Layout, 0x24)),
        }
    }

    /// Send the one `ScanoutFlip`, through the same path `SCANOUT_PRESENT` uses.
    #[inline(never)]
    fn scanout_present(&self, c: &Client) -> Result<Out, Fail> {
        use crate::virtio::foreign_scanout::{present_within, PresentRefusal};
        // Direct and bounded like the ring's flips: this runs on the worker StopDevice joins,
        // and the only source that can be live here is the KMD's own (`ScanoutSet` found no
        // user source), so there is no fenced queue of a live source to stay behind.
        let result = match present_within(
            self.passive,
            self.adapter,
            KMD,
            c.drm(),
            c.gem(),
            TIMEOUT_MS,
        ) {
            Ok(_seq) => Ok(Out::Unit),
            Err(PresentRefusal::NoTransport) => Err(Fail::new(FailKind::Transport, 4)),
            Err(PresentRefusal::NotOwned) => Err(Fail::new(FailKind::Refused, 2)),
            Err(PresentRefusal::Forbidden) => Err(Fail::new(FailKind::Refused, 4)),
            Err(PresentRefusal::NoSource) => Err(Fail::new(FailKind::Refused, 6)),
            Err(PresentRefusal::Device(e)) => Err(fail_of(Refusal::Transport(e))),
            // The fenced-present refusals cannot come from an unfenced present.
            Err(
                PresentRefusal::Unsupported
                | PresentRefusal::NotFence
                | PresentRefusal::AlreadyAttached
                | PresentRefusal::QueueFull,
            ) => Err(Fail::new(FailKind::Refused, 8)),
        };
        if result.is_err() {
            // The source was set (`ScanoutSet`) and never shown: end it now, so the
            // desktop's flush is restored at once instead of after the lapse. A no-op
            // when it is not live any more.
            let _ = self.adapter.foreign_scanout_release(KMD, Some(c.drm()));
        }
        result
    }
}
