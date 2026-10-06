//! Foreign (RM-exported) resources in the KMD's tables.
//!
//! A foreign resource has an ordinary `blobs` slot (owner, context, size) and an
//! ordinary `resources` entry, made by `ctrl::alloc_blob`, so liveness checks,
//! `ATTACH_RESOURCE`, adoption by a WDDM allocation, `RELEASE_BLOB` and the
//! DestroyDevice / StopDevice sweeps all treat it as they treat any blob. What
//! this module adds is the side record in `helios_kmd_logic::foreign_resource`
//! (quotas, provenance, the host-verified size) and the two decisions that must
//! be made atomically with it: that the caller owns the DRM file and the Venus
//! context it names.
//!
//! The record's removal is driven by the blob table (`resource_tables`): every
//! path that pops a blob slot also pops the record, and adoption flips it to
//! KMD-owned. Nothing here is a second teardown path.
//!
//! Every method runs under the device spinlock: pure table work only, no
//! allocation (the record's storage is reserved at init) and nothing that waits.

use super::*;
use helios_kmd_logic::foreign_resource::{
    self as fr, AdoptPlan, AdoptRefusal, AdoptRequest, Quota, RefusalKind, Reservation,
};

/// First `device_type` of a DRM file in the `HELIOS_ESCAPE_NVRM` handle table
/// (255 is the control file, a GPU minor, 256 UVM, 257 UVM tools).
const NVRM_DEVICE_TYPE_DRM_MIN: u32 = 512;

/// What [`VirtioGpu::foreign_begin_import`] decided.
pub enum ForeignBegin {
    /// A slot and `size` bytes are held for the caller; commit or cancel it.
    Reserved(Reservation),
    /// The RM handle is not an open DRM file of the caller's.
    NotOwned,
    /// The context is not the caller's.
    BadContext,
    /// A quota is exhausted.
    Quota(Quota),
}

/// What [`VirtioGpu::foreign_commit_import`] did with the reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignCommit {
    /// Recorded.
    Recorded,
    /// The RM handle was closed while the host round trip ran, and the host may
    /// already have reused its number: the caller must tear the resource down.
    HandleClosed,
    /// The blob slot is gone (device teardown got there first), so the resource
    /// is already released: nothing was recorded, nothing is left to tear down.
    BlobGone,
    /// The table refused the record (a duplicate id). The caller must tear the
    /// resource down.
    Refused,
}

/// What [`VirtioGpu::adopt_for_allocation`] decided for one
/// `D3DKMTCreateAllocation` that names an existing resource.
pub enum AllocAdopt {
    /// Not foreign: the ordinary Venus adoption ran (`Some(size)` of the live
    /// blob, or `None` if it is dead or untracked), exactly as before.
    Legacy(Option<u64>),
    /// A foreign resource, adopted: its blob slot is KMD-owned now and its
    /// creator's quota is freed. The record (with the layout) is kept until the
    /// allocation's destroy removes it.
    Foreign(fr::Adopted),
    /// Refused; nothing changed.
    Refused(AdoptRefusal),
}

/// A point-in-time read for `QUERY_CAPS`.
#[derive(Clone, Copy)]
pub struct ForeignSnapshot {
    pub limits: fr::Limits,
    pub live_total: u32,
    pub live_owner: u32,
    pub counters: fr::Counters,
}

impl VirtioGpu {
    /// Check, atomically with the reservation, that `owner` opened `rm_handle`
    /// (as a DRM file) and created `ctx_id`, then reserve a slot and `size`
    /// bytes. One lock hold, so the answer cannot be stale by the time the
    /// reservation exists.
    pub fn foreign_begin_import(
        &mut self,
        owner: DeviceOwner,
        ctx_id: u32,
        rm_handle: u32,
        size: u64,
    ) -> ForeignBegin {
        match self.nvrm_handle_device_type(owner, rm_handle) {
            Some(t) if t >= NVRM_DEVICE_TYPE_DRM_MIN => {}
            _ => {
                self.foreign.note_refusal(RefusalKind::NotOwned);
                return ForeignBegin::NotOwned;
            }
        }
        if self.resolve_owned_ctx(Some(owner), ctx_id).is_none() {
            self.foreign.note_refusal(RefusalKind::BadContext);
            return ForeignBegin::BadContext;
        }
        match self.foreign.reserve(owner.raw() as u64, size) {
            Ok(r) => ForeignBegin::Reserved(r),
            Err(q) => ForeignBegin::Quota(q),
        }
    }

    /// The host created the resource: record it against the reservation, if the
    /// state it was made under still holds. Consumes the reservation on every
    /// path.
    pub fn foreign_commit_import(
        &mut self,
        owner: DeviceOwner,
        r: Reservation,
        resource_id: u32,
        ctx_id: u32,
        rm_handle: u32,
        gem_handle: u32,
        layout: fr::Layout,
    ) -> ForeignCommit {
        // `alloc_blob` committed the slot in an earlier hold; teardown of this
        // device may have popped it since. Recording a resource that is gone
        // would leak the record forever.
        let slot_is_ours = self
            .blobs
            .iter()
            .any(|s| s.resource_id == resource_id && s.owner == Some(owner));
        if !slot_is_ours {
            self.foreign.cancel(r);
            return ForeignCommit::BlobGone;
        }
        // The same stale-handle rule `push_nvrm_map` applies: a handle closed
        // during the round trip may name another process's file by now.
        if !self.nvrm_handle_owned(owner, rm_handle) {
            self.foreign.cancel(r);
            return ForeignCommit::HandleClosed;
        }
        match self
            .foreign
            .commit(r, resource_id, ctx_id, rm_handle, gem_handle, layout)
        {
            Ok(()) => ForeignCommit::Recorded,
            Err(_) => ForeignCommit::Refused,
        }
    }

    /// The import did not produce a resource: give the reservation back and
    /// count why.
    pub fn foreign_abandon_import(&mut self, r: Reservation) {
        self.foreign.cancel(r);
        self.foreign.note_refusal(RefusalKind::Host);
    }

    /// Adopt `resource_id` for a WDDM allocation (`DxgkDdiCreateAllocation`),
    /// foreign or not, in ONE lock hold: the decision in
    /// `ForeignTable::adopt_for_allocation`, the facts it needs read from the
    /// tables here, and the re-ownership of the blob slot that makes the
    /// allocation the resource's owner.
    ///
    /// DISPATCH-safe like the rest of this file: table work only, no allocation,
    /// nothing that waits. The caller releases the lock before any host round
    /// trip, as `build_backing` does for the legacy path.
    ///
    /// The "same device" rule. `DxgkDdiCreateAllocation` is handed no device
    /// handle (`DXGKARG_CREATEALLOCATION` has none), so the creator cannot be
    /// named by the DDI. What the KMD can check is that the allocation names
    /// the Venus context the import was made on, and that context is still the
    /// creating device's; contexts are device-owned, so presenting it is
    /// presenting something only that device was handed. This is not
    /// authentication (context ids are small integers a hostile process can
    /// guess); see the hardening list in the design note.
    pub fn adopt_for_allocation(&mut self, resource_id: u32, req: &AdoptRequest) -> AllocAdopt {
        let (ctx_ok, slot_ok) = match self
            .foreign
            .get(resource_id)
            .and_then(|e| e.creator.map(|c| (c, e.ctx_id)))
        {
            Some((creator, ctx_id)) => {
                let owner = DeviceOwner::new(creator as usize);
                (
                    owner.is_some() && self.resolve_owned_ctx(owner, ctx_id).is_some(),
                    owner.is_some()
                        && self
                            .blobs
                            .iter()
                            .any(|s| s.resource_id == resource_id && s.owner == owner),
                )
            }
            None => (false, false),
        };
        match self
            .foreign
            .adopt_for_allocation(resource_id, req, ctx_ok, slot_ok)
        {
            Ok(AdoptPlan::Legacy) => AllocAdopt::Legacy(if req.take_ownership {
                self.adopt_blob_for_allocation(resource_id)
            } else {
                self.live_blob_size(resource_id)
            }),
            Ok(AdoptPlan::Foreign(adopted)) => {
                // `slot_ok` was true in this hold, so the slot exists and was the
                // creator's. KMD-owned from here: no escape-owner reclaim path
                // reaches it, and only the allocation's destroy releases it.
                if let Some(slot) = self.blobs.iter_mut().find(|s| s.resource_id == resource_id) {
                    slot.owner = None;
                }
                AllocAdopt::Foreign(adopted)
            }
            Err(refusal) => AllocAdopt::Refused(refusal),
        }
    }

    /// The layout recorded for a foreign resource, for the KMD-driven scanout
    /// flip (`ScanoutFlip{stride, fourcc, modifier}`), the scanout copy import
    /// (`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with an explicit layout) and
    /// any importer that must not infer it from the size. `None` for a resource
    /// that is not foreign. Valid while the resource lives (its removal drops the
    /// record).
    pub fn foreign_layout(&self, resource_id: u32) -> Option<fr::Layout> {
        self.foreign.layout(resource_id)
    }

    /// The whole record the KMD's own Venus import of a foreign resource is built
    /// from: the layout and the host-verified size (`Entry::size`, never larger
    /// than the host object). `None` for a resource that is not foreign (dead, or
    /// an ordinary blob). One lookup, so the two cannot come from different
    /// instants; the KMD's copy import takes it once per import under the table
    /// lock and compares it with what the allocation carried
    /// (`helios_kmd_logic::foreign_copy::record_agrees`).
    pub fn foreign_record(&self, resource_id: u32) -> Option<(fr::Layout, u64)> {
        self.foreign.get(resource_id).map(|e| (e.layout, e.size))
    }

    /// Count a request refused before it reached any table.
    pub fn foreign_note_refusal(&mut self, kind: RefusalKind) {
        self.foreign.note_refusal(kind);
    }

    /// Limits, occupancy and counters; `owner` is the caller, whose own count
    /// is reported separately.
    pub fn foreign_snapshot(&self, owner: DeviceOwner) -> ForeignSnapshot {
        ForeignSnapshot {
            limits: self.foreign.limits(),
            live_total: self.foreign.live() as u32,
            live_owner: self.foreign.owner_live(owner.raw() as u64) as u32,
            counters: self.foreign.counters(),
        }
    }
}
