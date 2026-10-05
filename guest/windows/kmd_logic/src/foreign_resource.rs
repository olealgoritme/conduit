//! Foreign scanout resources: Venus resources whose backing memory the KMD did
//! not create, today the RM memory an NVK-on-RM client rendered into, imported
//! on the host as a blob (see `guest/windows/docs/zero-copy-present.md`).
//!
//! # What this table is, and is not
//!
//! The KMD's `resources` and `blobs` tables already decide everything about a
//! resource id: liveness (attach, adopt, open, scanout flush), the owning
//! device (reclaim at DestroyDevice, RELEASE_BLOB) and the size. A foreign
//! resource has a normal entry in both. This table is the *side* record that
//! says "this one came from the host's RM export, not from a Venus allocation",
//! and carries what only those need:
//!
//! * quotas (a foreign resource pins host VRAM until it is released, so the
//!   limits are tighter and counted in bytes);
//! * provenance (which RM handle and GEM object it was made from), for tracing;
//! * the KMD-recorded size, which the host has verified (the host refuses an
//!   import whose claimed size exceeds the object), so unlike a size a UMD
//!   claims in allocation private data it is a bound the KMD can trust;
//! * a marker that makes the CPU paths refuse it (`MAP_BLOB`): there is no CPU
//!   view of a foreign resource by construction.
//!
//! Everything here is a function of its arguments. Storage is reserved by
//! [`ForeignTable::new`], and no operation allocates afterwards, so the KMD can
//! call it under its device spinlock (the capacity checks run before every
//! push, so a push never exceeds the reservation).
//!
//! # Lifetime (the rules the tests pin)
//!
//! ```text
//!  reserve ──► commit ──► (creator = Some(device))
//!     │                        │  adopt (a WDDM allocation takes the resource)
//!     └─ cancel                ▼
//!                        (creator = None, KMD-owned)
//!                              │
//!  remove  ◄── RELEASE_BLOB / DestroyDevice / StopDevice / allocation destroy
//! ```
//!
//! * Per-owner quotas count only `creator == Some(owner)` entries: once an
//!   allocation has adopted the resource, VidMm charges it and the creating
//!   process has no say in it. The global cap counts every entry.
//! * `remove` is idempotent: the three teardown paths can race, and only the
//!   first gets an entry back.
//! * A reservation is a promise of one slot and `size` bytes to one owner; it
//!   is consumed exactly once, by `commit` or `cancel`.

extern crate alloc;
use alloc::vec::Vec;

/// Most foreign resources across every process, adopted ones included.
pub const MAX_FOREIGN_TOTAL: usize = 512;
/// Most one device may hold that it created and no allocation has adopted.
pub const MAX_FOREIGN_PER_OWNER: usize = 64;
/// Largest single resource (a 16384x16384 BGRA8 image is exactly 1 GiB).
pub const MAX_FOREIGN_RESOURCE_BYTES: u64 = 1 << 30;
/// Most bytes one device may hold that it created and no allocation adopted.
pub const MAX_FOREIGN_BYTES_PER_OWNER: u64 = 4 << 30;

const PAGE: u64 = 4096;

/// The `RESOURCE_CREATE_BLOB.blob_id` that names the host object to import:
/// the backend handle of the DRM file in the high half, the GEM handle that
/// file's `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` returned in the low half.
///
/// The pair is the whole identity. The KMD has checked that the calling device
/// opened the DRM file; the host checks that the GEM handle exists in it.
pub const fn foreign_blob_id(rm_handle: u32, gem_handle: u32) -> u64 {
    ((rm_handle as u64) << 32) | gem_handle as u64
}

/// Why an import request is refused before any table is consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// `ctx_id`, `rm_handle` or `gem_handle` is 0 (none of them can be).
    ZeroId,
    /// `flags` is not 0: a bit this KMD does not know.
    Flags,
    /// `size` is 0 or not a whole number of pages.
    Size,
    /// `size` is over [`MAX_FOREIGN_RESOURCE_BYTES`].
    TooLarge,
}

/// Structural checks of an import request. Pure; ownership and quotas are
/// checked later, under the lock that makes them atomic with the reservation.
pub const fn validate_request(
    ctx_id: u32,
    rm_handle: u32,
    gem_handle: u32,
    flags: u32,
    size: u64,
) -> Result<(), RequestError> {
    if ctx_id == 0 || rm_handle == 0 || gem_handle == 0 {
        return Err(RequestError::ZeroId);
    }
    if flags != 0 {
        return Err(RequestError::Flags);
    }
    if size == 0 || size % PAGE != 0 {
        return Err(RequestError::Size);
    }
    if size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(RequestError::TooLarge);
    }
    Ok(())
}

/// Which quota refused a reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quota {
    /// The table is full ([`Limits::total`]).
    Table,
    /// The owner holds [`Limits::per_owner`] resources already.
    OwnerCount,
    /// The owner would hold more than [`Limits::bytes_per_owner`] bytes.
    OwnerBytes,
}

/// Why [`ForeignTable::commit`] could not record an entry. The reservation is
/// consumed either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// No reservation of this owner and size is outstanding.
    NoReservation,
    /// The resource id is already recorded (ids are unique by construction, so
    /// this is a bug upstream, refused rather than shadowed).
    Duplicate,
}

/// Table limits; [`Limits::DEFAULT`] is what the KMD uses, tests shrink them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub total: usize,
    pub per_owner: usize,
    pub bytes_per_owner: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        total: MAX_FOREIGN_TOTAL,
        per_owner: MAX_FOREIGN_PER_OWNER,
        bytes_per_owner: MAX_FOREIGN_BYTES_PER_OWNER,
    };
}

/// A promise of one slot and `size` bytes to `owner`. Made only by
/// [`ForeignTable::reserve`], consumed by `commit` or `cancel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    owner: u64,
    size: u64,
}

impl Reservation {
    pub const fn owner(&self) -> u64 {
        self.owner
    }

    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// One foreign resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub resource_id: u32,
    /// The device token that created it; `None` once a WDDM allocation adopted
    /// it (KMD-owned from then on).
    pub creator: Option<u64>,
    /// The Venus context the resource was attached to at import.
    pub ctx_id: u32,
    pub rm_handle: u32,
    pub gem_handle: u32,
    /// The size the host verified. Never larger than the host object.
    pub size: u64,
}

/// Why a request that never became a resource was turned away, for
/// [`ForeignTable::note_refusal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    /// The RM handle is not the caller's, or is not a DRM file.
    NotOwned,
    /// The context is not the caller's.
    BadContext,
    /// [`validate_request`] refused.
    BadRequest,
    /// The host or the transport refused the import.
    Host,
}

/// Counters, read under the same lock as the table and published by the escape
/// layer at PASSIVE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub imported: u32,
    pub released: u32,
    pub adopted: u32,
    pub refused_quota: u32,
    pub refused_not_owned: u32,
    pub refused_context: u32,
    pub refused_request: u32,
    pub refused_host: u32,
    pub live_high_water: u32,
}

impl Counters {
    /// Every refusal, whatever the reason.
    pub const fn refused(&self) -> u32 {
        self.refused_quota
            .saturating_add(self.refused_not_owned)
            .saturating_add(self.refused_context)
            .saturating_add(self.refused_request)
            .saturating_add(self.refused_host)
    }
}

pub struct ForeignTable {
    limits: Limits,
    entries: Vec<Entry>,
    reserved: Vec<Reservation>,
    counters: Counters,
}

impl ForeignTable {
    /// The KMD's table: [`Limits::DEFAULT`], storage reserved up front.
    pub fn new() -> Self {
        Self::with_limits(Limits::DEFAULT)
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            entries: Vec::with_capacity(limits.total),
            reserved: Vec::with_capacity(limits.total),
            counters: Counters::default(),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Entries, adopted ones included.
    pub fn live(&self) -> usize {
        self.entries.len()
    }

    /// Resources `owner` created and no allocation has adopted.
    pub fn owner_live(&self, owner: u64) -> usize {
        self.entries
            .iter()
            .filter(|e| e.creator == Some(owner))
            .count()
    }

    /// Bytes of those.
    pub fn owner_bytes(&self, owner: u64) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.creator == Some(owner))
            .fold(0u64, |a, e| a.saturating_add(e.size))
    }

    pub fn contains(&self, resource_id: u32) -> bool {
        self.entries.iter().any(|e| e.resource_id == resource_id)
    }

    pub fn get(&self, resource_id: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.resource_id == resource_id)
    }

    /// Reserve one slot and `size` bytes for `owner`, or say which quota is out.
    /// `size` must already have passed [`validate_request`].
    pub fn reserve(&mut self, owner: u64, size: u64) -> Result<Reservation, Quota> {
        let verdict = self.check_quota(owner, size);
        if let Err(q) = verdict {
            self.counters.refused_quota = self.counters.refused_quota.saturating_add(1);
            return Err(q);
        }
        let r = Reservation { owner, size };
        // `check_quota` proved entries + reserved < total, and both vectors
        // were reserved at `total`: this push cannot grow either.
        self.reserved.push(r);
        Ok(r)
    }

    fn check_quota(&self, owner: u64, size: u64) -> Result<(), Quota> {
        if self.entries.len() + self.reserved.len() >= self.limits.total {
            return Err(Quota::Table);
        }
        let mine =
            self.owner_live(owner) + self.reserved.iter().filter(|r| r.owner == owner).count();
        if mine >= self.limits.per_owner {
            return Err(Quota::OwnerCount);
        }
        let held = self.owner_bytes(owner).saturating_add(
            self.reserved
                .iter()
                .filter(|r| r.owner == owner)
                .fold(0u64, |a, r| a.saturating_add(r.size)),
        );
        match held.checked_add(size) {
            Some(total) if total <= self.limits.bytes_per_owner => Ok(()),
            _ => Err(Quota::OwnerBytes),
        }
    }

    fn take_reservation(&mut self, r: &Reservation) -> bool {
        match self.reserved.iter().position(|x| x == r) {
            Some(idx) => {
                self.reserved.swap_remove(idx);
                true
            }
            None => false,
        }
    }

    /// The import succeeded: record the resource against the reservation.
    pub fn commit(
        &mut self,
        r: Reservation,
        resource_id: u32,
        ctx_id: u32,
        rm_handle: u32,
        gem_handle: u32,
    ) -> Result<(), CommitError> {
        if !self.take_reservation(&r) {
            return Err(CommitError::NoReservation);
        }
        if self.contains(resource_id) {
            return Err(CommitError::Duplicate);
        }
        // The reservation just released guaranteed room for this one.
        self.entries.push(Entry {
            resource_id,
            creator: Some(r.owner),
            ctx_id,
            rm_handle,
            gem_handle,
            size: r.size,
        });
        self.counters.imported = self.counters.imported.saturating_add(1);
        let live = self.entries.len() as u32;
        if live > self.counters.live_high_water {
            self.counters.live_high_water = live;
        }
        Ok(())
    }

    /// The import failed or was abandoned: give the reservation back.
    pub fn cancel(&mut self, r: Reservation) {
        let _ = self.take_reservation(&r);
    }

    /// A WDDM allocation took the resource: it is KMD-owned, and no longer
    /// counts against its creator. `false` if it is not recorded or was
    /// already adopted.
    pub fn adopt(&mut self, resource_id: u32) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|e| e.resource_id == resource_id && e.creator.is_some())
        {
            Some(e) => {
                e.creator = None;
                self.counters.adopted = self.counters.adopted.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// The resource is gone (released, reclaimed or its allocation destroyed).
    /// Only the first caller gets the entry.
    pub fn remove(&mut self, resource_id: u32) -> Option<Entry> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.resource_id == resource_id)?;
        self.counters.released = self.counters.released.saturating_add(1);
        Some(self.entries.swap_remove(idx))
    }

    /// Count a request that never produced a reservation or a resource.
    pub fn note_refusal(&mut self, kind: RefusalKind) {
        let c = match kind {
            RefusalKind::NotOwned => &mut self.counters.refused_not_owned,
            RefusalKind::BadContext => &mut self.counters.refused_context,
            RefusalKind::BadRequest => &mut self.counters.refused_request,
            RefusalKind::Host => &mut self.counters.refused_host,
        };
        *c = c.saturating_add(1);
    }
}

impl Default for ForeignTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn small() -> ForeignTable {
        ForeignTable::with_limits(Limits {
            total: 4,
            per_owner: 2,
            bytes_per_owner: 64 * MIB,
        })
    }

    #[test]
    fn blob_id_is_rm_handle_high_gem_handle_low() {
        assert_eq!(foreign_blob_id(0x1234, 0x5678), 0x0000_1234_0000_5678);
        assert_eq!(foreign_blob_id(u32::MAX, 1), 0xFFFF_FFFF_0000_0001);
        assert_eq!(foreign_blob_id(1, u32::MAX), 0x0000_0001_FFFF_FFFF);
    }

    #[test]
    fn request_validation() {
        let ok = validate_request(1, 2, 3, 0, 8 * MIB);
        assert_eq!(ok, Ok(()));
        assert_eq!(
            validate_request(0, 2, 3, 0, 8 * MIB),
            Err(RequestError::ZeroId)
        );
        assert_eq!(
            validate_request(1, 0, 3, 0, 8 * MIB),
            Err(RequestError::ZeroId)
        );
        assert_eq!(
            validate_request(1, 2, 0, 0, 8 * MIB),
            Err(RequestError::ZeroId)
        );
        assert_eq!(
            validate_request(1, 2, 3, 1, 8 * MIB),
            Err(RequestError::Flags)
        );
        assert_eq!(validate_request(1, 2, 3, 0, 0), Err(RequestError::Size));
        assert_eq!(validate_request(1, 2, 3, 0, 4097), Err(RequestError::Size));
        assert_eq!(
            validate_request(1, 2, 3, 0, MAX_FOREIGN_RESOURCE_BYTES),
            Ok(())
        );
        assert_eq!(
            validate_request(1, 2, 3, 0, MAX_FOREIGN_RESOURCE_BYTES + PAGE),
            Err(RequestError::TooLarge)
        );
        // The cap itself is a page multiple, so TooLarge is reachable.
        assert_eq!(MAX_FOREIGN_RESOURCE_BYTES % PAGE, 0);
    }

    #[test]
    fn reserve_commit_records_the_owner_and_counts() {
        let mut t = small();
        let r = t.reserve(10, 8 * MIB).unwrap();
        assert_eq!((r.owner(), r.size()), (10, 8 * MIB));
        // A reservation counts before it is committed.
        assert_eq!(t.live(), 0);
        t.commit(r, 100, 7, 3, 9).unwrap();
        assert_eq!(t.live(), 1);
        assert_eq!(t.owner_live(10), 1);
        assert_eq!(t.owner_bytes(10), 8 * MIB);
        let e = t.get(100).unwrap();
        assert_eq!(e.creator, Some(10));
        assert_eq!((e.ctx_id, e.rm_handle, e.gem_handle), (7, 3, 9));
        assert_eq!(e.size, 8 * MIB);
        assert_eq!(t.counters().imported, 1);
        assert_eq!(t.counters().live_high_water, 1);
    }

    #[test]
    fn reservations_count_against_quotas_until_cancelled() {
        let mut t = small();
        let a = t.reserve(1, MIB).unwrap();
        let b = t.reserve(1, MIB).unwrap();
        // Two outstanding reservations fill the per-owner count.
        assert_eq!(t.reserve(1, MIB), Err(Quota::OwnerCount));
        // Another owner is unaffected.
        let c = t.reserve(2, MIB).unwrap();
        t.cancel(a);
        assert!(t.reserve(1, MIB).is_ok());
        t.cancel(b);
        t.cancel(c);
        assert_eq!(t.counters().refused_quota, 1);
    }

    #[test]
    fn per_owner_count_quota() {
        let mut t = small();
        for id in 1..=2 {
            let r = t.reserve(1, MIB).unwrap();
            t.commit(r, id, 1, 1, id).unwrap();
        }
        assert_eq!(t.reserve(1, MIB), Err(Quota::OwnerCount));
        assert!(t.reserve(2, MIB).is_ok());
    }

    #[test]
    fn per_owner_byte_quota_counts_reservations_and_entries() {
        let mut t = ForeignTable::with_limits(Limits {
            total: 8,
            per_owner: 8,
            bytes_per_owner: 64 * MIB,
        });
        let r = t.reserve(1, 40 * MIB).unwrap();
        t.commit(r, 1, 1, 1, 1).unwrap();
        // 40 held + 24 pending is exactly the cap.
        let pending = t.reserve(1, 24 * MIB).unwrap();
        assert_eq!(t.reserve(1, PAGE), Err(Quota::OwnerBytes));
        // Another owner has its own budget.
        assert!(t.reserve(2, 64 * MIB).is_ok());
        t.cancel(pending);
        assert!(t.reserve(1, 24 * MIB).is_ok());
    }

    #[test]
    fn byte_arithmetic_cannot_overflow_the_quota() {
        let mut t = ForeignTable::with_limits(Limits {
            total: 4,
            per_owner: 4,
            bytes_per_owner: u64::MAX,
        });
        let r = t.reserve(1, u64::MAX - PAGE).unwrap();
        t.commit(r, 1, 1, 1, 1).unwrap();
        // Would wrap; must be refused, not admitted.
        assert_eq!(t.reserve(1, 2 * PAGE), Err(Quota::OwnerBytes));
    }

    #[test]
    fn global_cap_counts_every_owner_and_every_reservation() {
        let mut t = small();
        let mut held = [None; 4];
        for (i, slot) in held.iter_mut().enumerate() {
            *slot = Some(t.reserve(i as u64 + 1, MIB).unwrap());
        }
        assert_eq!(t.reserve(99, MIB), Err(Quota::Table));
        t.cancel(held[0].take().unwrap());
        assert!(t.reserve(99, MIB).is_ok());
    }

    #[test]
    fn adoption_moves_the_resource_out_of_its_creators_quota_only() {
        let mut t = small();
        for id in 1..=2 {
            let r = t.reserve(1, 4 * MIB).unwrap();
            t.commit(r, id, 1, 1, id).unwrap();
        }
        assert_eq!(t.reserve(1, MIB), Err(Quota::OwnerCount));
        assert!(t.adopt(1));
        assert_eq!(t.get(1).unwrap().creator, None);
        // The creator may import again; the global count still holds both.
        assert_eq!(t.owner_live(1), 1);
        assert_eq!(t.owner_bytes(1), 4 * MIB);
        assert_eq!(t.live(), 2);
        assert!(t.reserve(1, MIB).is_ok());
        // Adopting twice, or something never recorded, reports false.
        assert!(!t.adopt(1));
        assert!(!t.adopt(77));
        assert_eq!(t.counters().adopted, 1);
    }

    #[test]
    fn remove_is_idempotent_and_frees_the_slot() {
        let mut t = small();
        let r = t.reserve(1, 4 * MIB).unwrap();
        t.commit(r, 5, 1, 1, 1).unwrap();
        let gone = t.remove(5).unwrap();
        assert_eq!((gone.resource_id, gone.size), (5, 4 * MIB));
        assert_eq!(t.remove(5), None);
        assert!(!t.contains(5));
        assert_eq!(t.counters().released, 1);
        assert_eq!(t.owner_live(1), 0);
        assert!(t.reserve(1, MIB).is_ok());
    }

    #[test]
    fn remove_after_adoption_returns_a_kmd_owned_entry() {
        let mut t = small();
        let r = t.reserve(1, MIB).unwrap();
        t.commit(r, 5, 1, 1, 1).unwrap();
        assert!(t.adopt(5));
        let e = t.remove(5).unwrap();
        assert_eq!(e.creator, None);
    }

    #[test]
    fn commit_without_a_matching_reservation_is_refused() {
        let mut t = small();
        let r = t.reserve(1, MIB).unwrap();
        t.cancel(r);
        // The reservation was already given back: a second use must not mint
        // an entry out of thin air.
        assert_eq!(t.commit(r, 5, 1, 1, 1), Err(CommitError::NoReservation));
        assert_eq!(t.live(), 0);
    }

    #[test]
    fn duplicate_resource_id_is_refused_and_the_reservation_released() {
        let mut t = small();
        let a = t.reserve(1, MIB).unwrap();
        t.commit(a, 5, 1, 1, 1).unwrap();
        let b = t.reserve(2, MIB).unwrap();
        assert_eq!(t.commit(b, 5, 1, 1, 1), Err(CommitError::Duplicate));
        assert_eq!(t.live(), 1);
        // The failed commit consumed the reservation: nothing is left pending.
        assert_eq!(t.owner_live(2), 0);
        for id in 6..=8 {
            let r = t.reserve(id as u64, MIB).unwrap();
            t.commit(r, id, 1, 1, 1).unwrap();
        }
        assert_eq!(t.live(), 4);
    }

    #[test]
    fn identical_reservations_are_interchangeable_but_counted_once_each() {
        let mut t = small();
        let a = t.reserve(1, MIB).unwrap();
        let b = t.reserve(1, MIB).unwrap();
        assert_eq!(a, b);
        t.commit(a, 1, 1, 1, 1).unwrap();
        t.commit(b, 2, 1, 1, 2).unwrap();
        // Both were consumed; a third commit has nothing to draw on.
        assert_eq!(t.commit(a, 3, 1, 1, 3), Err(CommitError::NoReservation));
        assert_eq!(t.live(), 2);
    }

    #[test]
    fn storage_is_reserved_once_and_never_grows() {
        let mut t = ForeignTable::new();
        let (ec, rc) = (t.entries.capacity(), t.reserved.capacity());
        assert!(ec >= MAX_FOREIGN_TOTAL && rc >= MAX_FOREIGN_TOTAL);
        // Fill to the global cap with many owners, then churn.
        for i in 0..MAX_FOREIGN_TOTAL as u32 {
            let r = t.reserve(i as u64 / 8 + 1, PAGE).unwrap();
            t.commit(r, i + 1, 1, 1, i + 1).unwrap();
        }
        assert_eq!(t.reserve(1000, PAGE), Err(Quota::Table));
        for i in 0..MAX_FOREIGN_TOTAL as u32 {
            assert!(t.remove(i + 1).is_some());
        }
        assert_eq!(t.live(), 0);
        assert_eq!((t.entries.capacity(), t.reserved.capacity()), (ec, rc));
    }

    #[test]
    fn default_limits_are_consistent() {
        // A process at its own limits must not be able to starve the table.
        assert!(MAX_FOREIGN_PER_OWNER <= MAX_FOREIGN_TOTAL);
        assert!(MAX_FOREIGN_RESOURCE_BYTES <= MAX_FOREIGN_BYTES_PER_OWNER);
        assert_eq!(Limits::DEFAULT.total, MAX_FOREIGN_TOTAL);
    }

    #[test]
    fn refusals_are_counted_by_reason() {
        let mut t = small();
        t.note_refusal(RefusalKind::NotOwned);
        t.note_refusal(RefusalKind::BadContext);
        t.note_refusal(RefusalKind::BadRequest);
        t.note_refusal(RefusalKind::BadRequest);
        t.note_refusal(RefusalKind::Host);
        let _ = t.reserve(1, 128 * MIB); // over the byte quota
        let c = t.counters();
        assert_eq!(
            (
                c.refused_not_owned,
                c.refused_context,
                c.refused_request,
                c.refused_host,
                c.refused_quota
            ),
            (1, 1, 2, 1, 1)
        );
        assert_eq!(c.refused(), 6);
    }

    #[test]
    fn high_water_tracks_the_peak_not_the_current_count() {
        let mut t = small();
        for id in 1..=3u32 {
            let r = t.reserve(id as u64, MIB).unwrap();
            t.commit(r, id, 1, 1, id).unwrap();
        }
        t.remove(1);
        t.remove(2);
        assert_eq!(t.counters().live_high_water, 3);
        assert_eq!(t.live(), 1);
    }

    #[test]
    fn owner_isolation() {
        let mut t = small();
        let r = t.reserve(1, MIB).unwrap();
        t.commit(r, 1, 1, 1, 1).unwrap();
        assert_eq!(t.owner_live(2), 0);
        assert_eq!(t.owner_bytes(2), 0);
        assert_eq!(t.owner_live(1), 1);
    }
}
