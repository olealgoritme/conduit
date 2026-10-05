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
use helios_kmd_logic::foreign_resource::{self as fr, Quota, RefusalKind, Reservation};

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
            .commit(r, resource_id, ctx_id, rm_handle, gem_handle)
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
