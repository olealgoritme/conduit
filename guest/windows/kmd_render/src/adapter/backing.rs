//! Owned system-memory backing leases for BAR allocations.
//!
//! A paging transfer lends the driver its MDL/PTE mappings only for that
//! operation. Present can happen later, so retaining numerical PFNs is not a
//! lifetime contract. Every range stored here owns a second MDL acquired with
//! `MmProbeAndLockPages`; Windows therefore keeps those pages locked until the
//! range is paged back in, replaced, discarded, or destroyed.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use wdk_sys::{PMDL, PVOID};

use helios_kmd_logic::guest_blob::{Budget, Piece, Record};

use crate::irql::PassiveLevel;
use crate::sync::FallibleArc;

/// Hard ceiling for independently locked system backing on one adapter.
///
/// These leases exist only for KMD standard surfaces that VidMm evicted from
/// Helios's BAR and that Present must continue updating. Locking without a
/// ceiling could turn arbitrary eviction traffic into unbounded nonpageable
/// memory. At the ceiling the lease is REFUSED, but the paging operation does
/// not fail: `BuildPagingBuffer` may only answer STATUS_SUCCESS (VidMm
/// bugchecks on anything else), and an eviction that stopped partway would leave
/// the system image half garbage. The eviction copies EVERY byte first; only the
/// bookkeeping is dropped (`PgSe`), so what is lost is later Present mirroring
/// into the CPU view of that surface, never the snapshot itself.
const MAX_PINNED_SYSTEM_BACKING_BYTES: u64 = 512 * 1024 * 1024;
/// Bound interval fragmentation as well as pinned bytes. A range is one
/// physically-contiguous run in the virtual-transfer path; ordinary
/// allocations normally need one or a handful, while 4096 still covers a
/// maximally fragmented 16-MiB 4-KiB mapping.
pub(crate) const MAX_SYSTEM_BACKING_RANGES: usize = 4096;

struct PinnedBackingBudget {
    bytes: AtomicU64,
}

impl PinnedBackingBudget {
    fn try_reserve(&self, bytes: u64) -> bool {
        let mut current = self.bytes.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > MAX_PINNED_SYSTEM_BACKING_BYTES {
                return false;
            }
            match self.bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, bytes: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
    }
}

extern "C" {
    /// `MmProbeAndLockPages` raises. The C shim converts that raise to NULL,
    /// maps the newly locked MDL once in KernelMode, and returns both values.
    fn helios_lock_system_buffer_seh(
        virtual_address: PVOID,
        length: u32,
        mapped_system_address: *mut PVOID,
    ) -> PMDL;
    /// Releases the persistent system mapping, unlocks the pages, and frees the
    /// owned MDL. Must run at PASSIVE_LEVEL for pageable backing.
    fn helios_unlock_system_buffer(mdl: PMDL);
}

/// An independent memory-manager lease on one exact system-backing byte range.
pub(crate) struct SystemBackingLease {
    mdl: PMDL,
    system_va: NonNull<u8>,
    size: u64,
    charged_bytes: u64,
    budget: FallibleArc<PinnedBackingBudget>,
}

// The MDL and its system mapping are kernel-global objects. The only mutation
// route requires the backing table's content guard, and final release is
// constrained to PASSIVE callers by the table's API contract.
unsafe impl Send for SystemBackingLease {}
unsafe impl Sync for SystemBackingLease {}

impl SystemBackingLease {
    /// Lock and persistently map `size` ordinary-RAM bytes beginning at `va`.
    ///
    /// # Safety
    /// `va..va+size` must be a valid system mapping of ordinary RAM for this
    /// call. The caller must be at PASSIVE_LEVEL and may hold no spinlock.
    unsafe fn acquire(
        _passive: PassiveLevel,
        va: *mut u8,
        size: u64,
        budget: &FallibleArc<PinnedBackingBudget>,
    ) -> Option<Self> {
        let length = u32::try_from(size).ok().filter(|length| *length != 0)?;
        if va.is_null() {
            return None;
        }
        let page_offset = (va as usize & 0xFFF) as u64;
        let charged_bytes = page_offset
            .checked_add(size)?
            .checked_add(0xFFF)?
            .checked_div(4096)?
            .checked_mul(4096)?;
        let budget = budget.try_clone()?;
        if !budget.try_reserve(charged_bytes) {
            return None;
        }
        let mut system_va: PVOID = core::ptr::null_mut();
        // SAFETY: the byte range and PASSIVE obligation are this function's
        // contract; the shim catches the only raising operation.
        let mdl =
            unsafe { helios_lock_system_buffer_seh(va.cast::<c_void>(), length, &mut system_va) };
        if mdl.is_null() {
            budget.release(charged_bytes);
            return None;
        }
        let Some(system_va) = NonNull::new(system_va.cast::<u8>()) else {
            // The shim promises these succeed/fail together, but retain a
            // defensive unwind if that ABI is ever changed independently.
            unsafe { helios_unlock_system_buffer(mdl) };
            budget.release(charged_bytes);
            return None;
        };
        Some(Self {
            mdl,
            system_va,
            size,
            charged_bytes,
            budget,
        })
    }

    /// Copy a subrange of this lease from the corresponding blob bytes.
    unsafe fn copy_from(&self, lease_offset: u64, size: u64, source: *const u8) -> bool {
        let Some(end) = lease_offset.checked_add(size) else {
            return false;
        };
        let Ok(lease_offset) = usize::try_from(lease_offset) else {
            return false;
        };
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        if source.is_null() || end > self.size {
            return false;
        }
        // SAFETY: the checked range lies inside the persistent MDL mapping;
        // caller proves `source` covers `size` bytes and owns the content lock.
        unsafe {
            core::ptr::copy_nonoverlapping(source, self.system_va.as_ptr().add(lease_offset), size)
        };
        true
    }
}

impl SystemBackingLease {
    /// The physical pages of this lease, from its locked MDL: `(byte offset of the first byte
    /// in the first page, PFN array)`. The PFNs stay valid while the lease lives (the MDL is
    /// locked for exactly that long).
    fn pages(&self) -> (u64, &[u64]) {
        // SAFETY: `mdl` is this lease's own locked MDL (see `acquire`); a locked MDL is
        // followed by its PFN array (`MmGetMdlPfnArray`: `(PPFN_NUMBER)(Mdl + 1)`, PFN_NUMBER
        // is pointer-sized, 8 bytes on the only target), `ADDRESS_AND_SIZE_TO_SPAN_PAGES`
        // entries long. The slice borrows `self`, which owns the MDL.
        unsafe {
            let byte_offset = u64::from((*self.mdl).ByteOffset);
            let byte_count = u64::from((*self.mdl).ByteCount);
            let pages = (byte_offset + byte_count).div_ceil(4096) as usize;
            let pfns = self.mdl.add(1).cast::<u64>().cast_const();
            (byte_offset, core::slice::from_raw_parts(pfns, pages))
        }
    }
}

impl Drop for SystemBackingLease {
    fn drop(&mut self) {
        // SAFETY: `mdl` came from helios_lock_system_buffer_seh and is released
        // exactly once. Table snapshots are dropped only by PASSIVE callbacks.
        unsafe { helios_unlock_system_buffer(self.mdl) };
        self.budget.release(self.charged_bytes);
    }
}

/// One allocation-relative range backed by an owned system-memory lease.
///
/// Each stored range owns an exact-size lease. A partial page-in replaces the
/// surviving pieces with newly probed exact leases before releasing the old
/// whole-range lease, so removed pages become pageable immediately on commit.
pub(crate) struct SystemBackingRange {
    blob_offset: u64,
    size: u64,
    lease: FallibleArc<SystemBackingLease>,
}

impl SystemBackingRange {
    /// Acquire an owned lease before the paging operation releases its view.
    ///
    /// # Safety
    /// Same byte-range and PASSIVE contract as [`SystemBackingLease::acquire`].
    unsafe fn acquire(
        passive: PassiveLevel,
        blob_offset: u64,
        size: u64,
        system_va: *mut u8,
        budget: &FallibleArc<PinnedBackingBudget>,
    ) -> Option<Self> {
        blob_offset.checked_add(size)?;
        let lease = unsafe { SystemBackingLease::acquire(passive, system_va, size, budget) }?;
        Some(Self {
            blob_offset,
            size,
            lease: FallibleArc::try_new(lease).ok()?,
        })
    }

    fn end(&self) -> Option<u64> {
        self.blob_offset.checked_add(self.size)
    }

    fn try_clone(&self) -> Option<Self> {
        Some(Self {
            blob_offset: self.blob_offset,
            size: self.size,
            lease: self.lease.try_clone()?,
        })
    }

    fn relock_slice(&self, passive: PassiveLevel, blob_offset: u64, size: u64) -> Option<Self> {
        let end = blob_offset.checked_add(size)?;
        if size == 0 || blob_offset < self.blob_offset || end > self.end()? {
            return None;
        }
        let lease_offset = blob_offset - self.blob_offset;
        let lease_offset = usize::try_from(lease_offset).ok()?;
        // Take a fresh, exact MDL lease for the surviving interval. The old
        // whole-range lease is released after the table transaction commits,
        // so a partial page-in really unlocks the removed pages instead of an
        // shared slice accidentally keeping the entire old MDL pinned.
        unsafe {
            Self::acquire(
                passive,
                blob_offset,
                size,
                self.lease.system_va.as_ptr().add(lease_offset),
                &self.lease.budget,
            )
        }
    }

    /// Copy this allocation-relative range out of `blob`.
    unsafe fn copy_from_blob(&self, blob: *const u8, blob_size: u64) -> bool {
        let Some(end) = self.end() else {
            return false;
        };
        let Ok(blob_offset) = usize::try_from(self.blob_offset) else {
            return false;
        };
        if end > blob_size || blob.is_null() {
            return false;
        }
        // SAFETY: the blob range was checked and the caller holds the backing
        // content guard for the entire snapshot copy.
        unsafe { self.lease.copy_from(0, self.size, blob.add(blob_offset)) }
    }

    unsafe fn copy_blob_intersection(
        &self,
        blob: *const u8,
        blob_size: u64,
        update_offset: u64,
        update_size: u64,
    ) -> bool {
        let Some(range_end) = self.end() else {
            return false;
        };
        let Some(update_end) = update_offset.checked_add(update_size) else {
            return false;
        };
        let start = self.blob_offset.max(update_offset);
        let end = range_end.min(update_end);
        if start >= end {
            return true;
        }
        let size = end - start;
        if end > blob_size || blob.is_null() {
            return false;
        }
        let lease_offset = start - self.blob_offset;
        let Ok(start) = usize::try_from(start) else {
            return false;
        };
        // SAFETY: the intersection lies in both the checked blob and lease
        // ranges; the caller owns the table content transaction.
        unsafe { self.lease.copy_from(lease_offset, size, blob.add(start)) }
    }
}

/// Immutable, allocation-wide snapshot used by one Present mirror.
pub(crate) struct SystemBackingSnapshot<'guard> {
    ranges: FallibleArc<Vec<SystemBackingRange>>,
    // A snapshot may hold the final MDL owner, whose Drop invokes PASSIVE-only
    // memory-manager APIs. Tie it to the serialized PASSIVE transaction so it
    // cannot escape into a caller that later drops it at arbitrary IRQL.
    _guard: PhantomData<&'guard ()>,
}

/// One destination's lease set, held by its guest blob (`GuestBlob`): while a pin lives, none
/// of the pages the host blob names can be unlocked, whatever the backing table does with its
/// own entry. Dropped (PASSIVE only: it may hold the last owner of a lease) only after the guest
/// blob was released on the host (`helios_kmd_logic::guest_blob::Record::may_unlock`).
pub(crate) struct GuestPin {
    _ranges: FallibleArc<Vec<SystemBackingRange>>,
}

impl SystemBackingSnapshot<'_> {
    /// A pin of exactly these leases. `None` only when the reference count would overflow.
    pub(crate) fn pin(&self) -> Option<GuestPin> {
        Some(GuestPin {
            _ranges: self.ranges.try_clone()?,
        })
    }

    /// The leases as `guest_blob::Piece`s (sorted by allocation offset, as stored), appended to
    /// `out`. `false` when `out` could not grow.
    pub(crate) fn pieces<'s>(&'s self, out: &mut Vec<Piece<'s>>) -> bool {
        if out.try_reserve_exact(self.ranges.len()).is_err() {
            return false;
        }
        for range in self.ranges.iter() {
            let (byte_offset, pfns) = range.lease.pages();
            out.push(Piece {
                blob_offset: range.blob_offset,
                size: range.size,
                byte_offset,
                pfns,
            });
        }
        true
    }

    /// A read-only handle on exactly these leases (`RmCopyEngine` = 3, the copy-engine shadow
    /// mode's compare): like a [`GuestPin`] it keeps every lease locked while it lives, so its
    /// pages can be read after the content transaction ended. Reads made then are not ordered
    /// against a later Present's copy into the same pages: the caller detects and reports that,
    /// it never writes. Dropped at PASSIVE only. `None` only when the reference count would
    /// overflow.
    pub(crate) fn reader(&self) -> Option<SystemBackingReader> {
        Some(SystemBackingReader {
            ranges: self.ranges.try_clone()?,
        })
    }

    pub(crate) unsafe fn copy_from_blob(&self, blob: *const u8, blob_size: u64) -> bool {
        self.ranges
            .iter()
            .all(|range| unsafe { range.copy_from_blob(blob, blob_size) })
    }

    pub(crate) unsafe fn copy_blob_range(
        &self,
        blob: *const u8,
        blob_size: u64,
        update_offset: u64,
        update_size: u64,
    ) -> bool {
        self.ranges.iter().all(|range| unsafe {
            range.copy_blob_intersection(blob, blob_size, update_offset, update_size)
        })
    }
}

/// See [`SystemBackingSnapshot::reader`].
pub(crate) struct SystemBackingReader {
    ranges: FallibleArc<Vec<SystemBackingRange>>,
}

impl SystemBackingReader {
    /// The leases, sorted by allocation offset as stored: `(allocation offset, bytes)` into
    /// `runs` and the kernel address of each lease's first byte into `vas`. `false` when the
    /// vectors could not grow.
    pub(crate) fn runs(&self, runs: &mut Vec<(u64, u64)>, vas: &mut Vec<*const u8>) -> bool {
        if runs.try_reserve_exact(self.ranges.len()).is_err()
            || vas.try_reserve_exact(self.ranges.len()).is_err()
        {
            return false;
        }
        for range in self.ranges.iter() {
            runs.push((range.blob_offset, range.size));
            vas.push(range.lease.system_va.as_ptr().cast_const());
        }
        true
    }
}

struct SystemBackingEntry {
    resource_id: u32,
    ranges: FallibleArc<Vec<SystemBackingRange>>,
}

/// Per-adapter resource-id -> owned system-backing associations.
pub(crate) struct SystemBackingTable {
    /// Serialize whole software content transactions. This is a sleeping lock:
    /// multi-megabyte copies never run under a spinlock or raised IRQL.
    content_mutex: Option<crate::sync::PassiveMutex>,
    entries: crate::sync::SpinLock<crate::sync::FixedVec<SystemBackingEntry>>,
    /// Allocations whose SYSTEM copy is invalid because a LOCAL_TO_SYSTEM
    /// eviction was skipped (answered STATUS_SUCCESS, so VidMm believes the system
    /// pages are current). A SYSTEM_TO_LOCAL page-in of such an allocation is
    /// skipped so the host blob, which is still right, is not overwritten with
    /// whatever the system pages hold. Own spinlock (a handful of ids, no
    /// allocation, no blocking), so it can be marked even when the content mutex
    /// could not be taken. See `helios_kmd_logic::paging::InvalidSet`.
    invalid: crate::sync::SpinLock<
        helios_kmd_logic::paging::InvalidSet<{ helios_kmd_logic::paging::INVALID_SET_CAPACITY }>,
    >,
    /// Shared by every lease so the bound applies before probing, including
    /// temporary relocks during a partial-range transaction.
    budget: Option<FallibleArc<PinnedBackingBudget>>,
    /// `GuestBlob`: per destination, the guest blob's state (`guest_blob::Record`) and the pin
    /// of the leases it names; and the host's live-blob/run totals. Mutated only by a holder of
    /// the content transaction (creation and every lease change hold it); read under this
    /// spinlock alone by the Present path. A pin is never dropped under the spinlock.
    guest: crate::sync::SpinLock<GuestTable>,
}

struct GuestEntry {
    record: Record,
    pin: Option<GuestPin>,
}

struct GuestTable {
    entries: crate::sync::FixedVec<GuestEntry>,
    budget: Budget,
}

impl SystemBackingTable {
    const MAX_ALLOCATIONS: usize = 128;
    /// Guest-blob records at most (one per allocation).
    pub(crate) const GUEST_RECORDS: usize = Self::MAX_ALLOCATIONS;

    pub fn new(passive: PassiveLevel) -> Self {
        Self {
            content_mutex: crate::sync::PassiveMutex::try_new(passive),
            entries: crate::sync::SpinLock::new(crate::sync::FixedVec::with_max(
                Self::MAX_ALLOCATIONS,
            )),
            invalid: crate::sync::SpinLock::new(helios_kmd_logic::paging::InvalidSet::new()),
            budget: FallibleArc::try_new(PinnedBackingBudget {
                bytes: AtomicU64::new(0),
            })
            .ok(),
            guest: crate::sync::SpinLock::new(GuestTable {
                entries: crate::sync::FixedVec::with_max(Self::MAX_ALLOCATIONS),
                budget: Budget::new(),
            }),
        }
    }

    /// Whether `resource_id` has system-backing leases at all. Spinlock only.
    pub(crate) fn is_backed(&self, resource_id: u32) -> bool {
        self.entries
            .lock()
            .as_slice()
            .iter()
            .any(|entry| entry.resource_id == resource_id)
    }

    /// `resource_id`'s guest-blob record, if it has one. Spinlock only (any IRQL <= DISPATCH).
    pub(crate) fn guest_record(&self, resource_id: u32) -> Option<Record> {
        self.guest
            .lock()
            .entries
            .as_slice()
            .iter()
            .find(|entry| entry.record.resource_id == resource_id)
            .map(|entry| entry.record)
    }

    /// Whether any guest-blob record exists. Spinlock only.
    pub(crate) fn guest_any(&self) -> bool {
        self.guest.lock().entries.len() != 0
    }

    /// The first destination whose guest blob is live (`Ready`: a copy target), if any.
    /// Spinlock only.
    pub(crate) fn guest_first_ready(&self) -> Option<u32> {
        self.guest
            .lock()
            .entries
            .as_slice()
            .iter()
            .find(|entry| entry.record.copy_target())
            .map(|entry| entry.record.resource_id)
    }

    /// The host's live guest-blob totals `(blobs, runs)`. Spinlock only.
    pub(crate) fn guest_live(&self) -> (u32, u32) {
        let table = self.guest.lock();
        (table.budget.live_blobs(), table.budget.live_runs())
    }

    /// Remember that `resource_id`'s system copy is invalid. Callable without the
    /// content mutex (the mutex failing is itself a reason to call it). Returns
    /// what the set did, so the caller can count a newly set mark or an overflow.
    pub fn mark_system_copy_invalid(&self, resource_id: u32) -> helios_kmd_logic::paging::Mark {
        self.invalid.lock().mark(resource_id)
    }

    /// `BltNoMirror`: the GPU copy of a Present is about to make the system pages VidMm holds
    /// for `resource_id` older than its blob, and the KMD does not mirror the frame into them.
    /// Marks the system copy invalid so a page-in does not copy them over the blob, but only
    /// when such pages exist (`None` otherwise: nothing could be resurrected, and the invalid
    /// set is bounded, its overflow skips every page-in). The next whole-allocation eviction
    /// (blob to system) revalidates the copy. Spinlocks only; no content transaction.
    pub(crate) fn mark_stale_if_backed(
        &self,
        resource_id: u32,
    ) -> Option<helios_kmd_logic::paging::Mark> {
        let backed = self
            .entries
            .lock()
            .as_slice()
            .iter()
            .any(|entry| entry.resource_id == resource_id);
        backed.then(|| self.mark_system_copy_invalid(resource_id))
    }

    /// Whether a page-in of `resource_id` must be skipped. When it must, the
    /// blob (which the GPU may now write) is the only current copy, so whatever
    /// eviction chunks were tallied toward clearing the mark are void.
    pub fn page_in_blocked(&self, resource_id: u32) -> bool {
        self.invalid.lock().page_in_blocked(resource_id)
    }

    /// Whether `resource_id`'s system copy is marked invalid, WITHOUT the side effect of
    /// [`Self::page_in_blocked`] (no eviction coverage is voided). `GuestBlob` only: a marked
    /// destination gets no guest blob (`helios_kmd_logic::guest_blob::eligible`). Spinlock only.
    pub(crate) fn system_copy_invalid(&self, resource_id: u32) -> bool {
        self.invalid.lock().contains(resource_id)
    }

    /// A LOCAL_TO_SYSTEM eviction chunk `[offset, offset + moved)` of the
    /// `alloc_size`-byte `resource_id` succeeded. Own spinlock, no allocation.
    pub fn evict_chunk_done(
        &self,
        resource_id: u32,
        alloc_size: u64,
        offset: u64,
        moved: u64,
    ) -> helios_kmd_logic::paging::Chunk {
        self.invalid
            .lock()
            .evict_chunk_done(resource_id, alloc_size, offset, moved)
    }

    /// Drop the mark. Returns whether it was set.
    pub fn clear_system_copy_invalid(&self, resource_id: u32) -> bool {
        self.invalid.lock().clear(resource_id)
    }

    /// Serialize a complete backing-content transaction at PASSIVE_LEVEL.
    pub fn serialize(&self, passive: PassiveLevel) -> Option<SystemBackingGuard<'_>> {
        Some(SystemBackingGuard {
            table: self,
            _mutex: self.content_mutex.as_ref()?.lock(passive)?,
        })
    }

    /// [`Self::serialize`] only if the content transaction is free now (`None` at once
    /// otherwise): the copy-engine shadow mode never makes a Present's mirror or a paging
    /// operation wait for it.
    pub(crate) fn try_serialize(&self, passive: PassiveLevel) -> Option<SystemBackingGuard<'_>> {
        Some(SystemBackingGuard {
            table: self,
            _mutex: self.content_mutex.as_ref()?.try_lock(passive)?,
        })
    }
}

/// Proof that the caller owns this exact table's PASSIVE content transaction.
pub(crate) struct SystemBackingGuard<'a> {
    table: &'a SystemBackingTable,
    _mutex: crate::sync::PassiveMutexGuard<'a>,
}

impl SystemBackingGuard<'_> {
    pub(crate) unsafe fn acquire_range(
        &self,
        passive: PassiveLevel,
        blob_offset: u64,
        size: u64,
        system_va: *mut u8,
    ) -> Option<SystemBackingRange> {
        unsafe {
            SystemBackingRange::acquire(
                passive,
                blob_offset,
                size,
                system_va,
                self.table.budget.as_ref()?,
            )
        }
    }

    pub(crate) fn snapshot(&self, resource_id: u32) -> Option<SystemBackingSnapshot<'_>> {
        self.table
            .entries
            .lock()
            .as_slice()
            .iter()
            .find(|entry| entry.resource_id == resource_id)
            .and_then(|entry| {
                Some(SystemBackingSnapshot {
                    ranges: entry.ranges.try_clone()?,
                    _guard: PhantomData,
                })
            })
    }

    /// Replace exactly `[blob_offset, blob_offset + size)` with the supplied
    /// leases. Existing ranges outside it survive, including both halves of a
    /// range split by a partial transfer.
    pub(crate) fn replace_range(
        &self,
        passive: PassiveLevel,
        resource_id: u32,
        blob_offset: u64,
        size: u64,
        mut replacements: Vec<SystemBackingRange>,
    ) -> bool {
        let Some(replace_end) = blob_offset.checked_add(size) else {
            return false;
        };
        if size == 0 {
            return false;
        }
        replacements.sort_unstable_by_key(|range| range.blob_offset);
        let mut cursor = blob_offset;
        for range in &replacements {
            if range.size == 0 || range.blob_offset != cursor {
                return false;
            }
            let Some(end) = range.end() else {
                return false;
            };
            cursor = end;
        }
        if cursor != replace_end {
            return false;
        }

        let old = self.snapshot(resource_id);
        let old_len = old.as_ref().map_or(0, |snapshot| snapshot.ranges.len());
        let Some(capacity) = old_len
            .checked_add(replacements.len())
            .and_then(|n| n.checked_add(2))
        else {
            return false;
        };
        let mut next = Vec::new();
        if next.try_reserve_exact(capacity).is_err() {
            return false;
        }
        if let Some(old) = old {
            for range in old.ranges.iter() {
                let Some(range_end) = range.end() else {
                    return false;
                };
                if range_end <= blob_offset || range.blob_offset >= replace_end {
                    let Some(range) = range.try_clone() else {
                        return false;
                    };
                    next.push(range);
                    continue;
                }
                if range.blob_offset < blob_offset {
                    let Some(left) = range.relock_slice(
                        passive,
                        range.blob_offset,
                        blob_offset - range.blob_offset,
                    ) else {
                        return false;
                    };
                    next.push(left);
                }
                if range_end > replace_end {
                    let Some(right) =
                        range.relock_slice(passive, replace_end, range_end - replace_end)
                    else {
                        return false;
                    };
                    next.push(right);
                }
            }
        }
        next.append(&mut replacements);
        next.sort_unstable_by_key(|range| range.blob_offset);
        self.store(resource_id, next)
    }

    /// Remove only the named allocation-relative range after a partial page-in.
    pub(crate) fn remove_range(
        &self,
        passive: PassiveLevel,
        resource_id: u32,
        blob_offset: u64,
        size: u64,
    ) -> bool {
        let Some(remove_end) = blob_offset.checked_add(size) else {
            return false;
        };
        if size == 0 {
            return true;
        }
        let Some(old) = self.snapshot(resource_id) else {
            return true;
        };
        let Some(capacity) = old.ranges.len().checked_add(1) else {
            return false;
        };
        let mut next = Vec::new();
        if next.try_reserve_exact(capacity).is_err() {
            return false;
        }
        for range in old.ranges.iter() {
            let Some(range_end) = range.end() else {
                return false;
            };
            if range_end <= blob_offset || range.blob_offset >= remove_end {
                let Some(range) = range.try_clone() else {
                    return false;
                };
                next.push(range);
                continue;
            }
            if range.blob_offset < blob_offset {
                let Some(left) =
                    range.relock_slice(passive, range.blob_offset, blob_offset - range.blob_offset)
                else {
                    return false;
                };
                next.push(left);
            }
            if range_end > remove_end {
                let Some(right) = range.relock_slice(passive, remove_end, range_end - remove_end)
                else {
                    return false;
                };
                next.push(right);
            }
        }
        self.store(resource_id, next)
    }

    /// See [`SystemBackingTable::mark_system_copy_invalid`].
    pub(crate) fn mark_system_copy_invalid(
        &self,
        resource_id: u32,
    ) -> helios_kmd_logic::paging::Mark {
        self.table.mark_system_copy_invalid(resource_id)
    }

    /// Whether a page-in of `resource_id` must be skipped (its last eviction was
    /// skipped, so the system pages are not its content). A skipped page-in also
    /// voids the partial-eviction coverage gathered so far: see
    /// [`SystemBackingTable::page_in_blocked`].
    pub(crate) fn page_in_blocked(&self, resource_id: u32) -> bool {
        self.table.page_in_blocked(resource_id)
    }

    /// See [`SystemBackingTable::system_copy_invalid`].
    pub(crate) fn system_copy_invalid(&self, resource_id: u32) -> bool {
        self.table.system_copy_invalid(resource_id)
    }

    /// A LOCAL_TO_SYSTEM eviction chunk of `resource_id` succeeded; clears the
    /// "invalid" mark once the successful chunks since the mark cover the whole
    /// allocation. See [`SystemBackingTable::evict_chunk_done`].
    pub(crate) fn evict_chunk_done(
        &self,
        resource_id: u32,
        alloc_size: u64,
        offset: u64,
        moved: u64,
    ) -> helios_kmd_logic::paging::Chunk {
        self.table
            .evict_chunk_done(resource_id, alloc_size, offset, moved)
    }

    /// Run `f` on `resource_id`'s guest-blob record and the live totals, creating a fresh
    /// record when `create` is set and there is none. `None`: no record (and `create` unset,
    /// or the table is full). Spinlock only; `f` must not block.
    pub(crate) fn guest_update<R>(
        &self,
        resource_id: u32,
        create: bool,
        f: impl FnOnce(&mut Record, &mut Budget) -> R,
    ) -> Option<R> {
        let mut table = self.table.guest.lock();
        let GuestTable { entries, budget } = &mut *table;
        let index = match entries
            .as_slice()
            .iter()
            .position(|entry| entry.record.resource_id == resource_id)
        {
            Some(index) => index,
            None if create => {
                let fresh = GuestEntry {
                    record: Record::new(resource_id),
                    pin: None,
                };
                // A fresh entry holds no pin, so a refused push drops nothing PASSIVE-only.
                if entries.try_push(fresh).is_err() {
                    return None;
                }
                entries.len() - 1
            }
            None => return None,
        };
        Some(f(&mut entries.as_mut_slice()[index].record, budget))
    }

    /// Hand `pin` to `resource_id`'s guest-blob record. A pin that cannot be stored (no record)
    /// is handed back, to be dropped by the caller at PASSIVE outside the spinlock.
    pub(crate) fn guest_set_pin(&self, resource_id: u32, pin: GuestPin) -> Result<(), GuestPin> {
        let old = {
            let mut table = self.table.guest.lock();
            match table
                .entries
                .as_mut_slice()
                .iter_mut()
                .find(|entry| entry.record.resource_id == resource_id)
            {
                Some(entry) => entry.pin.replace(pin),
                None => return Err(pin),
            }
        };
        drop(old);
        Ok(())
    }

    /// Take `resource_id`'s pin, ONLY when its record allows the pages to be unlocked
    /// (`Record::may_unlock`). The caller drops it (PASSIVE, outside every spinlock).
    pub(crate) fn guest_take_pin(&self, resource_id: u32) -> Option<GuestPin> {
        let mut table = self.table.guest.lock();
        table
            .entries
            .as_mut_slice()
            .iter_mut()
            .find(|entry| entry.record.resource_id == resource_id && entry.record.may_unlock())
            .and_then(|entry| entry.pin.take())
    }

    /// The destination is gone: forget its record, unless the record forbids unlocking (a
    /// failed release), in which case record and pin stay until the generation ends.
    pub(crate) fn guest_forget(&self, resource_id: u32) {
        let removed = {
            let mut table = self.table.guest.lock();
            let index = table
                .entries
                .as_slice()
                .iter()
                .position(|entry| entry.record.resource_id == resource_id && entry.record.may_unlock());
            index.map(|index| table.entries.swap_remove(index))
        };
        // PASSIVE (guard holder), outside the spinlock: may release the last lease owner.
        drop(removed);
    }

    /// The allocation's content is discarded or the allocation is gone: drop its
    /// backing ranges AND its invalid mark. Use this, not [`Self::remove`], for
    /// DISCARD_CONTENT and DestroyAllocation.
    pub(crate) fn remove_all(&self, resource_id: u32) {
        self.table.clear_system_copy_invalid(resource_id);
        self.remove(resource_id);
    }

    /// A new transport generation begins (or the old one ended): every resource
    /// id recorded here, range or invalid mark, belongs to a namespace that no
    /// longer exists, and ids restart at 1 — a surviving entry would be applied
    /// to a different live resource. Leases are released outside the spinlock.
    pub(crate) fn reset_generation(&self) {
        self.table.invalid.lock().clear_all();
        // Guest blobs first. Every caller resets the transport before this (StopDevice and
        // StartDevice drop it through `retire_transport`; `VirtioGpu::drop` writes device status
        // 0, and a reset virtio device may not access guest memory, which is all a guest blob's
        // mapping is), and the live blobs were retired before that while the host still
        // answered (`guest_blob::retire_all_for_stop`). What is left is a poisoned record (a
        // release the host did not confirm) or one a spent stop budget skipped: the reset is
        // the host's acknowledgment that nothing writes those pages any more, so every pin may
        // go, poisoned ones too. Dropping them earlier (with the transport up) would not be.
        loop {
            let taken = {
                let mut table = self.table.guest.lock();
                table.budget.clear();
                if table.entries.len() == 0 {
                    None
                } else {
                    Some(table.entries.swap_remove(0))
                }
            };
            match taken {
                Some(entry) => drop(entry),
                None => break,
            }
        }
        loop {
            let taken = {
                let mut entries = self.table.entries.lock();
                if entries.len() == 0 {
                    None
                } else {
                    Some(entries.swap_remove(0))
                }
            };
            match taken {
                // PASSIVE (guard holder): the last owner may unlock pages.
                Some(entry) => drop(entry),
                None => break,
            }
        }
    }

    /// Remove every system-backing range for one allocation. Does NOT touch the
    /// invalid mark (a failed partial transfer calls this too); see
    /// [`Self::remove_all`].
    pub(crate) fn remove(&self, resource_id: u32) {
        let removed = {
            let mut entries = self.table.entries.lock();
            entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == resource_id)
                .map(|index| entries.swap_remove(index))
        };
        // Releasing the last shared owner may unmap and unlock pages; do it after the
        // spinlock has restored the caller to PASSIVE_LEVEL.
        drop(removed);
    }

    fn store(&self, resource_id: u32, ranges: Vec<SystemBackingRange>) -> bool {
        if ranges.is_empty() {
            self.remove(resource_id);
            return true;
        }
        if ranges.len() > MAX_SYSTEM_BACKING_RANGES {
            return false;
        }
        let Some(new_bytes) = ranges.iter().try_fold(0u64, |sum, range| {
            sum.checked_add(range.lease.charged_bytes)
        }) else {
            return false;
        };
        for pair in ranges.windows(2) {
            if pair[0].end().is_none_or(|end| end > pair[1].blob_offset) {
                return false;
            }
        }
        if new_bytes > MAX_PINNED_SYSTEM_BACKING_BYTES {
            return false;
        }
        let Ok(ranges) = FallibleArc::try_new(ranges) else {
            return false;
        };
        let new_entry = SystemBackingEntry {
            resource_id,
            ranges,
        };
        let mut old = None;
        let mut rejected = None;
        let success = {
            let mut entries = self.table.entries.lock();
            match entries
                .as_slice()
                .iter()
                .position(|entry| entry.resource_id == resource_id)
            {
                Some(index) => {
                    old = Some(entries.replace_at(index, new_entry));
                    true
                }
                None => match entries.try_push(new_entry) {
                    Ok(()) => true,
                    Err(entry) => {
                        rejected = Some(entry);
                        false
                    }
                },
            }
        };
        drop(old);
        drop(rejected);
        success
    }
}
