//! `HELIOS_ESCAPE_FOREIGN_RESOURCE`: admit a host-exported RM memory object to the
//! KMD's resource tables as a Venus resource. The verb is in
//! `ddi/escape_foreign.rs`, the ABI in `helios_protocol::foreign`, the rules in
//! `guest/windows/docs/zero-copy-present.md`.
//!
//! The sequence, and why each step is where it is:
//!
//! 1. **Gate.** Unless the host advertises the import (`rm_import_served`: config
//!    features bit 13 with bit 10), nothing is touched: no reservation, no wire
//!    traffic.
//! 2. **Structure.** `validate_request`: ids nonzero, flags known, a whole number
//!    of pages within the per-resource cap, and a layout (mandatory) that is valid
//!    and fits the size. Pure.
//! 3. **Ownership and quota, one lock hold.** The caller must own the DRM file
//!    and the Venus context, and the reservation is taken in the same hold, so
//!    neither answer can be stale when the reservation exists.
//! 4. **Create.** `ctrl::alloc_blob` with `HELIOS_BLOB_MEM_RM_EXPORT`: the
//!    ordinary `RESOURCE_CREATE_BLOB` + `CTX_ATTACH_RESOURCE`, a blob slot owned
//!    by the caller's device, a live-resource entry. The KMD mints the id inside.
//! 5. **Commit, one lock hold.** The side record is made only if the blob slot
//!    is still the caller's and the RM handle still is (it may have been closed,
//!    and the host may have reused the number, during step 4). If either failed
//!    the reservation is returned; a closed handle also tears the resource down.
//!
//! Nothing here holds a lock across the wire round trip.

use core::sync::atomic::{AtomicU32, Ordering};

use super::ctrl;
use super::gpu::{DeviceOwner, ForeignBegin, ForeignCommit};
use super::VirtioError;
use crate::adapter::AdapterContext;
use crate::irql::PassiveLevel;
use helios_kmd_logic::foreign_resource::{foreign_blob_id, validate_request, Layout, RefusalKind};
use helios_protocol::HELIOS_BLOB_MEM_RM_EXPORT;

/// Whether the whole path is served: this KMD AND a host that creates a
/// resource from `HELIOS_BLOB_MEM_RM_EXPORT`. **NOT ADVERTISED.** The host half
/// does not exist yet (it needs the GEM-to-dma-buf import into the renderer),
/// and the wire shape below is a proposal the host has not agreed to, so this
/// stays `false` until both are real. While it is `false`, `QUERY_CAPS` leaves
/// `HELIOS_FOREIGN_CAP_RM_IMPORT` clear and `IMPORT_RM` answers `UNSUPPORTED`
/// without doing anything.
///
/// # Opening the gate: a one-line change, with preconditions
///
/// Change this to `true`, and nothing else in the KMD. Everything downstream
/// (the caps bit, `IMPORT_RM`, adoption of the resulting resid by a WDDM
/// allocation, the layout record) is already written and is dead code while
/// this is `false`. Do it only when ALL of these hold:
///
/// 1. The host serves `RESOURCE_CREATE_BLOB` with `blob_mem = 0x80000001`
///    (zero-copy-present.md H1 to H4): GEM lookup in the DRM file, dma-buf
///    import into the renderer, size check, own reference.
/// 2. The host has advertised it with a config `features` bit, and this const is
///    replaced by a read of that bit at init (the KMD reads config features
///    once, `CONDUIT_CFG_*`); a KMD must never serve the verb to a host that
///    would answer `DEVICE_ERROR` to every call.
/// 3. The host imports the object with the layout the KMD records. The record's
///    layout is NOT on the `RESOURCE_CREATE_BLOB` wire (the 56-byte command has
///    no room); the host must obtain it another way (it holds the GEM object)
///    or a layout-carrying message must be added: see "Open questions" in the
///    design note.
/// 4. The deferred review findings in the design note are fixed or accepted.
///
/// **OPENED (v310): this is no longer a constant.** [`rm_import_served`] reads the
/// device's config `features` word: the import is served only when the host
/// advertises `NVGPU_CFG_RM_IMPORT` (bit 13) together with `NVGPU_CFG_VENUS` (bit
/// 10). The host sets bit 13 only when its renderer imports dma-bufs, so an older
/// host is never sent the new blob type. The constant below is kept as the
/// compile-time kill switch.
pub const RM_IMPORT_ENABLED: bool = true;

/// Config `features` bits: `NVGPU_CFG_RM_IMPORT` and `NVGPU_CFG_VENUS`.
const CFG_RM_IMPORT: u32 = 1 << 13;
const CFG_VENUS: u32 = 1 << 10;

/// Whether this device serves `IMPORT_RM`.
pub fn rm_import_served(adapter: &AdapterContext) -> bool {
    RM_IMPORT_ENABLED
        && adapter
            .with_virtio(|v| {
                let f = v.nvrm_device_features();
                f & CFG_RM_IMPORT != 0 && f & CFG_VENUS != 0
            })
            .unwrap_or(false)
}

/// Whether an `ATTACH_RESOURCE` of a foreign resource by a caller that neither
/// created it nor holds an open of its allocation is refused (`STATUS_ACCESS_DENIED`).
///
/// `false`: counted (`FgAttUns`) and allowed, as every live resid has always been.
/// The sanctioned route to a foreign resid is `OpenAllocation` (it identifies the
/// resource and records the opener's process); flip this once a run shows
/// `FgAttUns` is 0 for every legitimate consumer (DWM, the bridge, NVK's holder
/// context). See `docs/shared-foreign-surfaces.md` section 4.
pub const ATTACH_ENFORCE: bool = false;

/// `IMPORT_RM` requests turned away because the gate is closed (`FgUns`).
pub static IMPORT_UNSUPPORTED: AtomicU32 = AtomicU32::new(0);
/// `RELEASE_BLOB`s that found nothing to release (`FgRelDup`): an unknown resource,
/// or one already released. The verb stays an idempotent success (it can race the
/// device-destroy sweeps and the Venus ICD releases defensively); this makes a
/// double free in NVK or the UMD visible without turning it into an error.
pub static RELEASE_DUP: AtomicU32 = AtomicU32::new(0);
/// `MAP_BLOB` / remap attempts on a foreign resource (`FgMapRf`). Bumped under
/// the device spinlock, hence an atomic.
pub static MAP_REFUSED: AtomicU32 = AtomicU32::new(0);

/// Why an import did not produce a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportError {
    /// The gate is closed.
    Unsupported,
    /// A field is out of range.
    BadRequest,
    /// The RM handle is not an open DRM file of the caller's.
    NotOwned,
    /// The context is not the caller's.
    BadContext,
    /// A quota is exhausted, or the KMD is out of table space or memory.
    NoResources,
    /// There is no transport.
    NoTransport,
    /// The host or the transport refused the create. The `u32` is the host's errno
    /// (0 if none was reported), already classified for the cases below.
    Device(VirtioError, u32),
    /// The host said the RM handle or the GEM handle is not usable (`EBADF` or
    /// `ENOENT`).
    HostNotOwned(u32),
    /// The host said the size or the request is out of range (`ERANGE`, `EINVAL`).
    HostBadRange(u32),
    /// The host does not serve the import (`EOPNOTSUPP`).
    HostUnsupported(u32),
}

/// Import GEM object `gem_handle` of DRM file `rm_handle` as a Venus resource of
/// `owner`, attached to `ctx_id`. Returns the id the KMD minted.
///
/// PASSIVE only: it waits on the control queue.
#[allow(clippy::too_many_arguments)]
pub fn import_rm(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    owner: DeviceOwner,
    ctx_id: u32,
    rm_handle: u32,
    gem_handle: u32,
    flags: u32,
    size: u64,
    layout: Option<Layout>,
) -> Result<u32, ImportError> {
    if !rm_import_served(adapter) {
        IMPORT_UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
        return Err(ImportError::Unsupported);
    }
    let Ok(layout) = validate_request(ctx_id, rm_handle, gem_handle, flags, size, layout) else {
        let _ = adapter.with_virtio(|v| v.foreign_note_refusal(RefusalKind::BadRequest));
        return Err(ImportError::BadRequest);
    };
    let begin = adapter
        .with_virtio(|v| v.foreign_begin_import(owner, ctx_id, rm_handle, size))
        .map_err(|_| ImportError::NoTransport)?;
    let reservation = match begin {
        ForeignBegin::Reserved(r) => r,
        ForeignBegin::NotOwned => return Err(ImportError::NotOwned),
        ForeignBegin::BadContext => return Err(ImportError::BadContext),
        ForeignBegin::Quota(_) => return Err(ImportError::NoResources),
    };

    let mut host_errno = 0u32;
    let created = ctrl::alloc_blob_errno(
        passive,
        adapter,
        ctx_id,
        HELIOS_BLOB_MEM_RM_EXPORT,
        0,
        foreign_blob_id(rm_handle, gem_handle),
        size,
        Some(owner),
        Some(&mut host_errno),
    );
    let resource_id = match created {
        Ok(id) => id,
        Err(e) => {
            let _ = adapter.with_virtio(|v| v.foreign_abandon_import(reservation));
            use helios_kmd_logic::foreign_errno::{classify, Verdict};
            return Err(match (e, classify(host_errno)) {
                (VirtioError::OutOfMemory, _) | (_, Verdict::NoResources) => {
                    ImportError::NoResources
                }
                (_, Verdict::NotOwned) => ImportError::HostNotOwned(host_errno),
                (_, Verdict::BadRange) => ImportError::HostBadRange(host_errno),
                (_, Verdict::Unsupported) => ImportError::HostUnsupported(host_errno),
                (other, Verdict::Device) => ImportError::Device(other, host_errno),
            });
        }
    };

    let committed = adapter.with_virtio(|v| {
        v.foreign_commit_import(
            owner,
            reservation,
            resource_id,
            ctx_id,
            rm_handle,
            gem_handle,
            layout,
        )
    });
    match committed {
        Ok(ForeignCommit::Recorded) => Ok(resource_id),
        // Device teardown already released the resource.
        Ok(ForeignCommit::BlobGone) => Err(ImportError::Device(VirtioError::DeviceError, 0)),
        // The resource exists host-side with no record: release it through the
        // ordinary path (blob slot, live entry, detach, unref), best effort.
        Ok(verdict) => {
            let _ = ctrl::release_blob_for_owner(passive, adapter, owner, ctx_id, resource_id);
            Err(if verdict == ForeignCommit::HandleClosed {
                ImportError::NotOwned
            } else {
                ImportError::Device(VirtioError::DeviceError, 0)
            })
        }
        Err(_) => Err(ImportError::NoTransport),
    }
}

/// Mirror the counters to the service key. PASSIVE only; the escape layer
/// throttles the calls. `owner` is the caller (the snapshot needs one for its
/// per-owner count, which is not published).
pub fn publish_counters(adapter: &AdapterContext, owner: DeviceOwner) {
    publish_snapshot(adapter.with_virtio(|v| v.foreign_snapshot(owner)).ok());
}

/// As [`publish_counters`] for the paths that have no device token (open, close,
/// attach, destroy). PASSIVE only; the callers throttle.
pub fn publish_counters_any(adapter: &AdapterContext) {
    publish_snapshot(adapter.with_virtio(|v| v.foreign_snapshot_any()).ok());
}

fn publish_snapshot(snap: Option<super::gpu::ForeignSnapshot>) {
    if let Some(snap) = snap {
        let c = snap.counters;
        // Cross-process sharing (S6). `FgOpen - FgClose` is `FgOpLive` (a count
        // that only grows is an open leaked by dxgkrnl or by us); `FgDefer` is 0
        // under dxgkrnl's contract and `FgOrphan` is 0 at rest (an allocation
        // destroyed with an opener still alive, whose release waits for it).
        crate::diag::record_named_bytes(b"FgOpen", c.opened);
        crate::diag::record_named_bytes(b"FgClose", c.closed);
        crate::diag::record_named_bytes(b"FgOpRf", c.refused_open);
        crate::diag::record_named_bytes(b"FgClsMis", c.close_missed);
        crate::diag::record_named_bytes(b"FgDefer", c.deferred);
        crate::diag::record_named_bytes(b"FgDeferRl", c.deferred_released);
        crate::diag::record_named_bytes(b"FgOpLive", snap.opens_live);
        crate::diag::record_named_bytes(b"FgOrphan", snap.orphans);
        crate::diag::record_named_bytes(b"FgAtt", c.attached);
        crate::diag::record_named_bytes(b"FgAttUns", c.attached_unsanctioned);
        crate::diag::record_named_bytes(b"FgImp", c.imported);
        crate::diag::record_named_bytes(b"FgRel", c.released);
        crate::diag::record_named_bytes(b"FgAdo", c.adopted);
        crate::diag::record_named_bytes(b"FgLive", snap.live_total);
        crate::diag::record_named_bytes(b"FgHi", c.live_high_water);
        crate::diag::record_named_bytes(b"FgRefQ", c.refused_quota);
        crate::diag::record_named_bytes(b"FgRefO", c.refused_not_owned);
        crate::diag::record_named_bytes(b"FgRefC", c.refused_context);
        crate::diag::record_named_bytes(b"FgRefR", c.refused_request);
        crate::diag::record_named_bytes(b"FgRefH", c.refused_host);
        crate::diag::record_named_bytes(b"FgRefA", c.refused_adopt);
    }
    crate::diag::record_named_bytes(b"FgUns", IMPORT_UNSUPPORTED.load(Ordering::Relaxed));
    crate::diag::record_named_bytes(b"FgMapRf", MAP_REFUSED.load(Ordering::Relaxed));
    super::rm_resource_import::publish_counters();
}
