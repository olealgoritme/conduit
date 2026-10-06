//! The KMD's own RM surfaces as foreign (Venus) resources: the KMD is the CREATOR.
//! Design and the allocation hooks it is written for: `docs/kmd-rm-client.md`
//! section 14; the foreign-resource machinery it reuses: `docs/shared-foreign-surfaces.md`
//! and `virtio/foreign.rs`.
//!
//! A surface the KMD's RM client made (`virtio/rm_client.rs`: RM memory, exported to a
//! GEM handle on the KMD's own DRI file) becomes a Venus resource through the same
//! `RESOURCE_CREATE_BLOB` with `HELIOS_BLOB_MEM_RM_EXPORT` that `IMPORT_RM` uses for a
//! user-mode NVK surface, so that
//!
//! * the resource has the ordinary `blobs` / `resources` entries and the side record
//!   with the layout and the host-verified size (`foreign_resource::ForeignTable`);
//! * a WDDM allocation of the KMD's can ADOPT it (`adopt_for_allocation`, whose
//!   creator check now knows the KMD's owner), after which `DxgkDdiOpenAllocation`
//!   by DWM yields the FOREIGN identity and the layout trailer, exactly as for an NVK
//!   game's surface (`shared-foreign-surfaces.md` section 2).
//!
//! What differs from `import_rm`: the owner is [`DeviceOwner::KMD_RM`] (the RM client's
//! DRI file is recorded under it, and no escape can present it), and the holder
//! context is the KMD's own Venus context, which is not device-owned
//! (`foreign_begin_kmd_import`). Everything else is the same code path: the quota is
//! the owner's own (64 resources, 4 GiB), the blob slot is KMD_RM's until a WDDM
//! allocation adopts it (then `None`), the transport sweep and
//! [`release_all`] reclaim what nobody adopted.
//!
//! GATE. Nothing here runs unless `KmdRmClient` is 4 and the host serves the import
//! (`foreign::rm_import_served`: config bits 13 and 10): the client asks for a
//! [`rm_client`](super::rm_client) step, the step calls [`import_surface`], and a
//! refusal gives that surface up (`share_failed`), never the client.

use core::sync::atomic::{AtomicU32, Ordering};

use super::ctrl;
use super::foreign::rm_import_served;
use super::gpu::{DeviceOwner, ForeignBegin, ForeignCommit};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use helios_kmd_logic::foreign_errno::{classify, Verdict};
use helios_kmd_logic::foreign_resource::{
    foreign_blob_id, validate_request, Layout, RefusalKind, FLAG_LAYOUT, FOURCC_XRGB8888,
    MOD_LINEAR,
};
use helios_kmd_logic::rm_client::{Client, Fail, FailKind, Out};
use helios_kmd_logic::sweep_budget::{SweepBudget, UNITS_PER_MS};
use helios_protocol::HELIOS_BLOB_MEM_RM_EXPORT;

const KMD: DeviceOwner = DeviceOwner::KMD_RM;

/// Surfaces imported (`RmFgImp`), released (`RmFgRel`), imports that failed
/// (`RmFgErr`) with the last host errno (`RmFgErrno`) and the kind of the last failure
/// (`RmFgWhy`: 1 gate closed, 2 no KMD context, 3 bad request, 4 table or quota,
/// 5 host refused, 6 transport, 7 recorded nothing / raced).
pub static RM_FG_IMPORTED: AtomicU32 = AtomicU32::new(0);
pub static RM_FG_RELEASED: AtomicU32 = AtomicU32::new(0);
pub static RM_FG_FAILED: AtomicU32 = AtomicU32::new(0);
pub static RM_FG_ERRNO: AtomicU32 = AtomicU32::new(0);
pub static RM_FG_WHY: AtomicU32 = AtomicU32::new(0);

/// Mirror the counters to the registry. PASSIVE only; nothing is written until the
/// first import was tried.
pub(crate) fn publish_counters() {
    use crate::diag::record_named_bytes as rec;
    if RM_FG_IMPORTED.load(Ordering::Relaxed) == 0 && RM_FG_FAILED.load(Ordering::Relaxed) == 0 {
        return;
    }
    rec(b"RmFgImp", RM_FG_IMPORTED.load(Ordering::Relaxed));
    rec(b"RmFgRel", RM_FG_RELEASED.load(Ordering::Relaxed));
    rec(b"RmFgErr", RM_FG_FAILED.load(Ordering::Relaxed));
    rec(b"RmFgErrno", RM_FG_ERRNO.load(Ordering::Relaxed));
    rec(b"RmFgWhy", RM_FG_WHY.load(Ordering::Relaxed));
}

/// A budget of `timeout_ms` in all, from now, shared by every host command of one call
/// (also the per-command cap): these calls run on the HPD worker, which StopDevice joins
/// for a bounded time, so a call is over when its allowance is, however many commands it
/// is made of (create + attach + unref; unmap + detach + unref).
fn budget_of(timeout_ms: u64) -> SweepBudget {
    SweepBudget::new(
        crate::adapter::foreign_scanout::now_100ns(),
        timeout_ms.saturating_mul(UNITS_PER_MS),
        timeout_ms,
    )
}

fn failed(why: u32, errno: u32) -> Fail {
    RM_FG_FAILED.fetch_add(1, Ordering::Relaxed);
    RM_FG_WHY.store(why, Ordering::Relaxed);
    RM_FG_ERRNO.store(errno, Ordering::Relaxed);
    Fail::new(FailKind::Refused, 0x40 + why)
}

/// The layout the host and every importer are told: the surface is pitch-linear
/// `XRGB8888` (`rm_client::flip_layout`'s), no plane offset.
pub fn surface_foreign_layout(l: &helios_kmd_logic::rm_client::SurfaceLayout) -> Layout {
    Layout {
        width: l.width,
        height: l.height,
        stride: l.pitch,
        offset: 0,
        fourcc: FOURCC_XRGB8888,
        modifier: MOD_LINEAR,
    }
}

/// Import the client's working-slot surface (its GEM on the KMD's DRI file) as a
/// foreign resource of owner [`DeviceOwner::KMD_RM`] on the KMD's own Venus context.
/// The resource id the KMD minted, as `Out::Resource`. PASSIVE: it waits on the control
/// queue, with no lock held, `timeout_ms` in all (create, attach and the undo of a
/// failed one share it).
#[inline(never)]
pub(super) fn import_surface(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    c: &Client,
    timeout_ms: u64,
) -> Result<Out, Fail> {
    let budget = budget_of(timeout_ms);
    let Some((layout, _mem, gem)) = c.ready_surface() else {
        return Err(Fail::new(FailKind::Parse, 0x40));
    };
    let drm = c.drm();
    if !rm_import_served(adapter) {
        return Err(failed(1, 0));
    }
    let ctx = adapter.venus_ctx_id();
    if ctx == 0 {
        return Err(failed(2, 0));
    }
    let fl = surface_foreign_layout(&layout);
    let size = layout.size;
    let Ok(fl) = validate_request(ctx, drm, gem, FLAG_LAYOUT, size, Some(fl)) else {
        let _ = adapter.with_virtio(|v| v.foreign_note_refusal(RefusalKind::BadRequest));
        return Err(failed(3, 0));
    };
    let begin = adapter
        .with_virtio(|v| v.foreign_begin_kmd_import(drm, ctx, ctx, size))
        .map_err(|_| failed(6, 0))?;
    let reservation = match begin {
        ForeignBegin::Reserved(r) => r,
        ForeignBegin::NotOwned | ForeignBegin::BadContext => return Err(failed(3, 0)),
        ForeignBegin::Quota(_) => return Err(failed(4, 0)),
    };
    let mut errno = 0u32;
    let created = ctrl::alloc_blob_errno_within(
        passive,
        adapter,
        ctx,
        HELIOS_BLOB_MEM_RM_EXPORT,
        0,
        foreign_blob_id(drm, gem),
        size,
        Some(KMD),
        Some(&mut errno),
        Some(&budget),
    );
    let resource_id = match created {
        Ok(id) => id,
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.foreign_abandon_import(reservation));
            let why = match (e, classify(errno)) {
                (VirtioError::OutOfMemory, _) | (_, Verdict::NoResources) => 4,
                (_, Verdict::NotOwned | Verdict::BadRange | Verdict::Unsupported) => 5,
                (_, Verdict::Device) => 6,
            };
            return Err(failed(why, errno));
        }
    };
    let committed = adapter
        .with_virtio(|v| v.foreign_commit_import(KMD, reservation, resource_id, ctx, drm, gem, fl));
    match committed {
        Ok(ForeignCommit::Recorded) => Ok(Out::Resource(resource_id)),
        // Teardown (or a closed DRM file) raced the round trip: the resource exists host
        // side with no record, release it through the ordinary path.
        Ok(_) | Err(_) => {
            // What is left of the allowance, and nothing when it is spent: the resource
            // is then reclaimed by `release_all` or the transport's own sweep.
            let _ = ctrl::release_blob_for_owner_within(
                passive,
                adapter,
                KMD,
                ctx,
                resource_id,
                Some(&budget),
            );
            Err(failed(7, 0))
        }
    }
}

/// Release resource `resource_id` the client imported: unmap if mapped, detach, unref,
/// drop the slot and the record. A resource a WDDM allocation adopted since is no longer
/// the client's (its slot is owned by no one): the release is then a no-op and the
/// allocation's destroy releases it, so a surface may be freed while a WDDM allocation
/// still names it (the host import holds its own reference to the memory). The host
/// commands (unmap, detach, unref) share `timeout_ms`; with it spent the answer is a
/// transport failure and the transport's sweep reclaims the rest.
#[inline(never)]
pub(super) fn release_surface(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    timeout_ms: u64,
) -> Result<Out, Fail> {
    if resource_id == 0 {
        return Ok(Out::Unit);
    }
    let ctx = adapter.venus_ctx_id();
    let budget = budget_of(timeout_ms);
    match ctrl::release_blob_for_owner_within(
        passive,
        adapter,
        KMD,
        ctx,
        resource_id,
        Some(&budget),
    ) {
        Ok(()) => {
            RM_FG_RELEASED.fetch_add(1, Ordering::Relaxed);
            Ok(Out::Unit)
        }
        Err(e) => Err(Fail::new(
            FailKind::Transport,
            if e == VirtioError::Timeout { 1 } else { 3 },
        )),
    }
}

/// Reclaim every resource the KMD's client still owns (a dead client, a retire): the
/// ones nobody adopted. PASSIVE, `timeout_ms` for the whole sweep: with it spent the
/// remaining slots are still taken out of the table, but nothing more is sent (the
/// transport's sweep reclaims the host side), so a dead client's cleanup, which runs on
/// the HPD worker, cannot outlast one step's allowance.
#[inline(never)]
pub(super) fn release_all(passive: PassiveLevel, adapter: &AdapterContext, timeout_ms: u64) {
    let budget = budget_of(timeout_ms);
    let n = ctrl::release_blobs_for_owner_within(passive, adapter, Some(KMD), Some(&budget));
    if n != 0 {
        RM_FG_RELEASED.fetch_add(n, Ordering::Relaxed);
    }
}
