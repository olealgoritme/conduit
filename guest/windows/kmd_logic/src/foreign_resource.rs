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
//!   view of a foreign resource by construction;
//! * its layout ([`Layout`]: extent, pitch, offset, fourcc, DRM modifier),
//!   mandatory and validated against the size at import, so a scanout flip or an
//!   importer reads what the producer chose instead of inferring it;
//! * the adoption decision ([`ForeignTable::adopt_for_allocation`]) when a WDDM
//!   allocation takes the resource.
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

/// `IMPORT_RM.flags` bit: the request carries a layout tail
/// (`HeliosForeignImportRmLayout`). Mandatory: see [`validate_request`].
pub const FLAG_LAYOUT: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// Surface layout
// ---------------------------------------------------------------------------
//
// A foreign resource is RM memory the KMD never allocated and cannot inspect, so
// the one place its layout can come from is the process that made it. The record
// carries it (mandatory, validated once at import) so that a KMD-driven scanout
// flip, a `venus/scanout.rs` import and DWM's opener read what NVK actually
// chose instead of inferring it from the size, which is wrong for heights that
// are a whole number of blocks.
//
// The rules mirror `foreign_scanout::Layout::validate` (branch
// `kmd/foreign-scanout`, 66c714f): the same four 32-bit RGB formats, the same
// stride bound. Differences, on purpose: the extent floor is 1 (a foreign
// resource is any adopted allocation, not only a mode-sized scanout image) and
// the modifier set is closed to what the host's NVIDIA Vulkan driver was shown to
// import with the exact layout (host spike c6fab91): LINEAR and the NVIDIA
// block-linear family NVK builds for B8G8R8A8 / R8G8B8A8.

/// `DRM_FORMAT_*` accepted: the four 32-bit RGB formats.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;
pub const FOURCC_ARGB8888: u32 = 0x3432_5241;
pub const FOURCC_XBGR8888: u32 = 0x3432_4258;
pub const FOURCC_ABGR8888: u32 = 0x3432_4241;
/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0, s=1, g=2, k=0x06, h=0)`: the
/// family NVK advertises, `base | h` with `h` the log2 block height in GOBs.
pub const MOD_NVIDIA_BLOCK_LINEAR_BASE: u64 = 0x0300_0000_0060_6010;
/// Largest accepted `h` (32 GOBs = 256 rows per block).
pub const MAX_BLOCK_HEIGHT_LOG2: u32 = 5;
/// Rows in one GOB.
pub const GOB_ROWS: u64 = 8;
/// Smallest and largest extent, and the largest pitch.
pub const MIN_DIM: u32 = 1;
pub const MAX_DIM: u32 = 16_384;
pub const MAX_STRIDE: u32 = 1 << 20;

/// The picture inside a foreign resource: everything a scanout flip or an
/// importer needs besides which object it is. Plane 0 only (all accepted formats
/// are single plane).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Plane 0 pitch in bytes (`rowPitch` of the explicit-modifier image).
    pub stride: u32,
    /// Plane 0 offset in bytes from the start of the object.
    pub offset: u32,
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`: [`MOD_LINEAR`] or `MOD_NVIDIA_BLOCK_LINEAR_BASE | h`.
    pub modifier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Width or height outside `MIN_DIM..=MAX_DIM`.
    Dimensions,
    /// A fourcc this KMD does not forward.
    Format,
    /// Stride under `width * 4`, not a multiple of 4, or over [`MAX_STRIDE`].
    Stride,
    /// Not LINEAR and not `MOD_NVIDIA_BLOCK_LINEAR_BASE | h`, `h <= 5`.
    Modifier,
    /// The layout needs more bytes than the resource has.
    TooLarge,
}

impl Layout {
    pub const fn validate(&self) -> Result<(), LayoutError> {
        if self.width < MIN_DIM
            || self.width > MAX_DIM
            || self.height < MIN_DIM
            || self.height > MAX_DIM
        {
            return Err(LayoutError::Dimensions);
        }
        if !matches!(
            self.fourcc,
            FOURCC_XRGB8888 | FOURCC_ARGB8888 | FOURCC_XBGR8888 | FOURCC_ABGR8888
        ) {
            return Err(LayoutError::Format);
        }
        if (self.stride as u64) < (self.width as u64) * 4
            || self.stride > MAX_STRIDE
            || self.stride % 4 != 0
        {
            return Err(LayoutError::Stride);
        }
        if self.modifier != MOD_LINEAR && self.block_height_log2().is_none() {
            return Err(LayoutError::Modifier);
        }
        Ok(())
    }

    /// `h` of an NVIDIA block-linear modifier, or `None` for LINEAR and for a
    /// modifier outside the accepted family.
    pub const fn block_height_log2(&self) -> Option<u32> {
        let m = self.modifier;
        if m >= MOD_NVIDIA_BLOCK_LINEAR_BASE
            && m <= MOD_NVIDIA_BLOCK_LINEAR_BASE + MAX_BLOCK_HEIGHT_LOG2 as u64
        {
            Some((m - MOD_NVIDIA_BLOCK_LINEAR_BASE) as u32)
        } else {
            None
        }
    }

    /// A LOWER BOUND on the bytes the image occupies from the start of the
    /// object: `offset + stride * rows`, `rows` being `height` for LINEAR and
    /// `height` rounded up to the block for block-linear. It is a bound and not
    /// the exact size: RM rounds allocations up (a 1080p linear image is
    /// 0x7e9000 in a 0x7f0000 object), so the check is `min_bytes() <= size`,
    /// never equality. Meaningful for a validated layout (no overflow: every
    /// factor is bounded).
    pub const fn min_bytes(&self) -> u64 {
        let rows = match self.block_height_log2() {
            None => self.height as u64,
            Some(h) => {
                let block = GOB_ROWS << h;
                ((self.height as u64) + block - 1) / block * block
            }
        };
        self.offset as u64 + (self.stride as u64) * rows
    }

    /// The layout is valid and fits an object of `size` bytes.
    pub const fn validate_for(&self, size: u64) -> Result<(), LayoutError> {
        if let Err(e) = self.validate() {
            return Err(e);
        }
        if self.min_bytes() > size {
            return Err(LayoutError::TooLarge);
        }
        Ok(())
    }
}

/// Why an import request is refused before any table is consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// `ctx_id`, `rm_handle` or `gem_handle` is 0 (none of them can be).
    ZeroId,
    /// `flags` has a bit this KMD does not know.
    Flags,
    /// `size` is 0 or not a whole number of pages.
    Size,
    /// `size` is over [`MAX_FOREIGN_RESOURCE_BYTES`].
    TooLarge,
    /// No layout was supplied ([`FLAG_LAYOUT`] clear). Not optional.
    LayoutRequired,
    /// The layout is invalid, or does not fit `size`.
    Layout(LayoutError),
}

/// Structural checks of an import request. Pure; ownership and quotas are
/// checked later, under the lock that makes them atomic with the reservation.
/// Returns the layout to record.
///
/// `layout` is what the escape layer decoded from the tail when
/// [`FLAG_LAYOUT`] is set, else `None`.
pub fn validate_request(
    ctx_id: u32,
    rm_handle: u32,
    gem_handle: u32,
    flags: u32,
    size: u64,
    layout: Option<Layout>,
) -> Result<Layout, RequestError> {
    if ctx_id == 0 || rm_handle == 0 || gem_handle == 0 {
        return Err(RequestError::ZeroId);
    }
    if flags & !FLAG_LAYOUT != 0 {
        return Err(RequestError::Flags);
    }
    if size == 0 || size % PAGE != 0 {
        return Err(RequestError::Size);
    }
    if size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(RequestError::TooLarge);
    }
    let layout = match layout {
        Some(l) if flags & FLAG_LAYOUT != 0 => l,
        _ => return Err(RequestError::LayoutRequired),
    };
    layout.validate_for(size).map_err(RequestError::Layout)?;
    Ok(layout)
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
    /// What is in it, validated against `size` at import.
    pub layout: Layout,
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
    /// WDDM allocations that named a foreign resource and were turned away
    /// ([`AdoptRefusal`]).
    pub refused_adopt: u32,
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
            .saturating_add(self.refused_adopt)
    }
}

/// What `D3DKMTCreateAllocation` says about the resource it names, reduced to
/// the facts the adoption decision needs. Built by the driver from the private
/// data; every field is a claim by the caller except `take_ownership` and
/// `trailer_room`, which are the driver's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdoptRequest {
    /// `blob_mem == HELIOS_BLOB_MEM_RM_EXPORT` (and not the typed-tracker shape,
    /// where that word is a cookie): the caller says the resource is foreign.
    pub declares_foreign: bool,
    /// The allocation kind adopts the blob's lifetime (DEVICE_MEMORY). Only such
    /// an allocation may take a foreign resource.
    pub take_ownership: bool,
    /// `HeliosWddmAllocPrivate.ctx_id`: must be the holder context the resource
    /// was imported on.
    pub ctx_id: u32,
    /// `HeliosWddmAllocMeta` geometry, which must repeat the record's layout.
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub plane_offset: u64,
    /// `HeliosWddmAllocMeta.venus_alloc_size` (0 = unspecified). May not exceed
    /// the recorded size; the identity reports the recorded size either way.
    pub claimed_alloc_size: u64,
    /// The optional `HeliosWddmAllocLayout` trailer the caller supplied, with the
    /// meta's width and height filled in: must equal the record's layout.
    pub supplied_layout: Option<Layout>,
    /// The allocation's private-data buffer can take the layout trailer the KMD
    /// writes back for openers.
    pub trailer_room: bool,
}

/// What a foreign adoption yields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adopted {
    /// The size the host verified at import.
    pub size: u64,
    pub layout: Layout,
}

/// The outcome of [`ForeignTable::adopt_for_allocation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptPlan {
    /// Not a foreign resource and not declared one: the ordinary Venus adoption
    /// applies, unchanged.
    Legacy,
    /// Adopted: the creator's quota is freed; the driver must re-own the blob
    /// slot (same lock hold) and record `layout` in the allocation.
    Foreign(Adopted),
}

impl AdoptRefusal {
    /// A stable nonzero code for the registry trace (`FgAdRf`).
    pub const fn code(self) -> u32 {
        match self {
            Self::NotForeign => 1,
            Self::Undeclared => 2,
            Self::NotDeviceMemory => 3,
            Self::AlreadyAdopted => 4,
            Self::ContextMismatch => 5,
            Self::ContextGone => 6,
            Self::SlotNotCreators => 7,
            Self::NoTrailerRoom => 8,
            Self::GeometryMismatch => 9,
            Self::LayoutMismatch => 10,
            Self::ClaimTooLarge => 11,
        }
    }
}

/// Why a foreign adoption was refused. Nothing was changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptRefusal {
    /// Declared foreign, but the id has no foreign record (dead, or an
    /// ordinary Venus resource).
    NotForeign,
    /// A foreign resource named without the declaration.
    Undeclared,
    /// The allocation kind does not take the blob's lifetime.
    NotDeviceMemory,
    /// A WDDM allocation already adopted it (it would be released twice).
    AlreadyAdopted,
    /// `ctx_id` is not the context the resource was imported on.
    ContextMismatch,
    /// That context is no longer its creator's (destroyed).
    ContextGone,
    /// The blob slot is not the creator's any more (teardown got there first).
    SlotNotCreators,
    /// The private data cannot take the layout trailer.
    NoTrailerRoom,
    /// Width, height, pitch or plane offset differ from the record's layout.
    GeometryMismatch,
    /// The supplied trailer differs from the record's layout.
    LayoutMismatch,
    /// `claimed_alloc_size` is over the recorded size.
    ClaimTooLarge,
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
        layout: Layout,
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
            layout,
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

    /// The layout recorded for `resource_id`, for a scanout flip or an importer.
    pub fn layout(&self, resource_id: u32) -> Option<Layout> {
        self.get(resource_id).map(|e| e.layout)
    }

    /// Decide, and on success perform, the adoption of `resource_id` by a WDDM
    /// allocation. One call so the decision and the state change cannot be
    /// separated by another thread: the driver holds its device lock across it
    /// and the slot re-ownership that follows.
    ///
    /// `ctx_owned_by_creator` and `slot_owned_by_creator` are facts only the
    /// driver's tables know; they are read in the same lock hold, about the
    /// record's creator ([`Entry::creator`]).
    ///
    /// ```text
    /// no record, not declared ........................ Legacy (nothing here)
    /// no record, declared ............................ NotForeign
    /// record, not declared ........................... Undeclared
    /// record, kind does not own the blob ............. NotDeviceMemory
    /// record, creator None ........................... AlreadyAdopted
    /// ctx != record ctx / ctx not creator's .......... ContextMismatch / ContextGone
    /// slot not creator's ............................. SlotNotCreators
    /// no room / geometry / format differ ............. NoTrailerRoom / GeometryMismatch / LayoutMismatch
    /// claim over recorded size ....................... ClaimTooLarge
    /// otherwise ...................................... creator := None; Foreign(..)
    /// ```
    ///
    /// The record stays: MAP refusal and teardown rules still apply, and the
    /// allocation's destroy (`forget_allocation_blob`) removes it.
    pub fn adopt_for_allocation(
        &mut self,
        resource_id: u32,
        req: &AdoptRequest,
        ctx_owned_by_creator: bool,
        slot_owned_by_creator: bool,
    ) -> Result<AdoptPlan, AdoptRefusal> {
        let Some(e) = self.get(resource_id).copied() else {
            return if req.declares_foreign {
                Err(self.refuse_adopt(AdoptRefusal::NotForeign))
            } else {
                Ok(AdoptPlan::Legacy)
            };
        };
        let verdict = if !req.declares_foreign {
            Some(AdoptRefusal::Undeclared)
        } else if !req.take_ownership {
            Some(AdoptRefusal::NotDeviceMemory)
        } else if e.creator.is_none() {
            Some(AdoptRefusal::AlreadyAdopted)
        } else if req.ctx_id != e.ctx_id {
            Some(AdoptRefusal::ContextMismatch)
        } else if !ctx_owned_by_creator {
            Some(AdoptRefusal::ContextGone)
        } else if !slot_owned_by_creator {
            Some(AdoptRefusal::SlotNotCreators)
        } else if !req.trailer_room {
            Some(AdoptRefusal::NoTrailerRoom)
        } else if req.width != e.layout.width
            || req.height != e.layout.height
            || req.pitch != e.layout.stride
            || req.plane_offset != u64::from(e.layout.offset)
        {
            Some(AdoptRefusal::GeometryMismatch)
        } else if req.supplied_layout.is_some_and(|l| l != e.layout) {
            Some(AdoptRefusal::LayoutMismatch)
        } else if req.claimed_alloc_size > e.size {
            Some(AdoptRefusal::ClaimTooLarge)
        } else {
            None
        };
        if let Some(r) = verdict {
            return Err(self.refuse_adopt(r));
        }
        // Every refusal is behind us and `e.creator` is `Some`, so this cannot
        // fail; the check keeps the invariant local.
        if !self.adopt(resource_id) {
            return Err(self.refuse_adopt(AdoptRefusal::AlreadyAdopted));
        }
        Ok(AdoptPlan::Foreign(Adopted {
            size: e.size,
            layout: e.layout,
        }))
    }

    fn refuse_adopt(&mut self, r: AdoptRefusal) -> AdoptRefusal {
        self.counters.refused_adopt = self.counters.refused_adopt.saturating_add(1);
        r
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

    /// 1080p XRGB8888, LINEAR, rowPitch 7680: 0x7e9000 bytes of image.
    fn lay() -> Layout {
        Layout {
            width: 1920,
            height: 1080,
            stride: 7680,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: MOD_LINEAR,
        }
    }

    fn bl(h: u64) -> Layout {
        Layout {
            modifier: MOD_NVIDIA_BLOCK_LINEAR_BASE | h,
            ..lay()
        }
    }

    fn validate(flags: u32, size: u64, layout: Option<Layout>) -> Result<Layout, RequestError> {
        validate_request(1, 2, 3, flags, size, layout)
    }

    #[test]
    fn request_validation() {
        let ok = validate(FLAG_LAYOUT, 8 * MIB, Some(lay()));
        assert_eq!(ok, Ok(lay()));
        assert_eq!(
            validate_request(0, 2, 3, FLAG_LAYOUT, 8 * MIB, Some(lay())),
            Err(RequestError::ZeroId)
        );
        assert_eq!(
            validate_request(1, 0, 3, FLAG_LAYOUT, 8 * MIB, Some(lay())),
            Err(RequestError::ZeroId)
        );
        assert_eq!(
            validate_request(1, 2, 0, FLAG_LAYOUT, 8 * MIB, Some(lay())),
            Err(RequestError::ZeroId)
        );
        // An unknown flag bit is refused, with or without the layout bit.
        assert_eq!(
            validate(FLAG_LAYOUT | 2, 8 * MIB, Some(lay())),
            Err(RequestError::Flags)
        );
        assert_eq!(validate(2, 8 * MIB, None), Err(RequestError::Flags));
        assert_eq!(
            validate(FLAG_LAYOUT, 0, Some(lay())),
            Err(RequestError::Size)
        );
        assert_eq!(
            validate(FLAG_LAYOUT, 4097, Some(lay())),
            Err(RequestError::Size)
        );
        assert_eq!(
            validate(FLAG_LAYOUT, MAX_FOREIGN_RESOURCE_BYTES, Some(lay())),
            Ok(lay())
        );
        assert_eq!(
            validate(FLAG_LAYOUT, MAX_FOREIGN_RESOURCE_BYTES + PAGE, Some(lay())),
            Err(RequestError::TooLarge)
        );
        // The cap itself is a page multiple, so TooLarge is reachable.
        assert_eq!(MAX_FOREIGN_RESOURCE_BYTES % PAGE, 0);
    }

    #[test]
    fn the_layout_is_not_optional() {
        // The old 72-byte request (flags 0) and a flag with no tail both lack it.
        assert_eq!(
            validate(0, 8 * MIB, None),
            Err(RequestError::LayoutRequired)
        );
        assert_eq!(
            validate(FLAG_LAYOUT, 8 * MIB, None),
            Err(RequestError::LayoutRequired)
        );
        // A decoded layout without the flag is not trusted either.
        assert_eq!(
            validate(0, 8 * MIB, Some(lay())),
            Err(RequestError::LayoutRequired)
        );
    }

    #[test]
    fn layout_formats() {
        for f in [
            FOURCC_XRGB8888,
            FOURCC_ARGB8888,
            FOURCC_XBGR8888,
            FOURCC_ABGR8888,
        ] {
            assert_eq!(Layout { fourcc: f, ..lay() }.validate(), Ok(()));
        }
        // 'RG16' (RGB565), 'AB4H', 0: not forwarded.
        for f in [0x3631_4752, 0x4834_4241, 0] {
            assert_eq!(
                Layout { fourcc: f, ..lay() }.validate(),
                Err(LayoutError::Format)
            );
        }
        // The constants spell the DRM fourccs.
        let cc = |a: u8, b: u8, c: u8, d: u8| {
            u32::from(a) | u32::from(b) << 8 | u32::from(c) << 16 | u32::from(d) << 24
        };
        assert_eq!(FOURCC_XRGB8888, cc(b'X', b'R', b'2', b'4'));
        assert_eq!(FOURCC_ARGB8888, cc(b'A', b'R', b'2', b'4'));
        assert_eq!(FOURCC_XBGR8888, cc(b'X', b'B', b'2', b'4'));
        assert_eq!(FOURCC_ABGR8888, cc(b'A', b'B', b'2', b'4'));
    }

    #[test]
    fn layout_extent_and_stride() {
        let l = lay();
        assert_eq!(
            Layout { width: 0, ..l }.validate(),
            Err(LayoutError::Dimensions)
        );
        assert_eq!(
            Layout { height: 0, ..l }.validate(),
            Err(LayoutError::Dimensions)
        );
        assert_eq!(
            Layout {
                width: MAX_DIM + 1,
                stride: MAX_STRIDE,
                ..l
            }
            .validate(),
            Err(LayoutError::Dimensions)
        );
        assert_eq!(
            Layout {
                height: MAX_DIM + 1,
                ..l
            }
            .validate(),
            Err(LayoutError::Dimensions)
        );
        // A 1x1 image is a legal foreign resource (not only mode-sized ones).
        assert_eq!(
            Layout {
                width: 1,
                height: 1,
                stride: 4,
                ..l
            }
            .validate(),
            Ok(())
        );
        // rowPitch under width * 4, off a 4-byte multiple, over 1 MiB.
        assert_eq!(
            Layout { stride: 7676, ..l }.validate(),
            Err(LayoutError::Stride)
        );
        assert_eq!(
            Layout { stride: 7682, ..l }.validate(),
            Err(LayoutError::Stride)
        );
        assert_eq!(
            Layout {
                width: 16384,
                stride: MAX_STRIDE + 4,
                ..l
            }
            .validate(),
            Err(LayoutError::Stride)
        );
        // Padding beyond width * 4 is fine.
        assert_eq!(Layout { stride: 8192, ..l }.validate(), Ok(()));
        // 16384 * 4 is exactly the cap.
        assert_eq!(
            Layout {
                width: 16384,
                height: 16,
                stride: 65536,
                ..l
            }
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn layout_modifiers() {
        assert_eq!(lay().block_height_log2(), None);
        for h in 0..=5u64 {
            assert_eq!(bl(h).validate(), Ok(()));
            assert_eq!(bl(h).block_height_log2(), Some(h as u32));
        }
        // The values NVK advertises for B8G8R8A8: ...6010 up to ...6015.
        assert_eq!(MOD_NVIDIA_BLOCK_LINEAR_BASE, 0x0300_0000_0060_6010);
        assert_eq!(bl(5).modifier, 0x0300_0000_0060_6015);
        // h = 6, one below the family, DRM_FORMAT_MOD_INVALID, another vendor,
        // another kind: all refused.
        for m in [
            MOD_NVIDIA_BLOCK_LINEAR_BASE | 6,
            MOD_NVIDIA_BLOCK_LINEAR_BASE - 1,
            0x00ff_ffff_ffff_ffff,
            0x0100_0000_0000_0001,
            0x0300_0000_0060_6110,
            1,
        ] {
            assert_eq!(
                Layout {
                    modifier: m,
                    ..lay()
                }
                .validate(),
                Err(LayoutError::Modifier),
                "{m:#x}"
            );
        }
    }

    #[test]
    fn layout_size_is_a_lower_bound_not_an_equality() {
        let l = lay();
        // The spike's numbers: the image is 0x7e9000 bytes inside a 0x7f0000
        // object, because RM rounds to 64 KiB.
        assert_eq!(l.min_bytes(), 0x7e_9000);
        assert_eq!(l.validate_for(0x7f_0000), Ok(()));
        assert_eq!(l.validate_for(0x7e_9000), Ok(()));
        assert_eq!(l.validate_for(0x7e_8000), Err(LayoutError::TooLarge));
        // The plane offset counts.
        let off = Layout {
            offset: 0x1_0000,
            ..l
        };
        assert_eq!(off.min_bytes(), 0x7e_9000 + 0x1_0000);
        assert_eq!(off.validate_for(0x7f_0000), Err(LayoutError::TooLarge));
        // Block-linear rounds the height up to the block: 1080 rows in
        // 256-row blocks is 1280 rows; h = 0 (8-row blocks) is 1080 exactly.
        assert_eq!(bl(0).min_bytes(), 7680 * 1080);
        assert_eq!(bl(4).min_bytes(), 7680 * 1152); // 128-row blocks
        assert_eq!(bl(5).min_bytes(), 7680 * 1280);
        assert_eq!(bl(5).validate_for(7680 * 1279), Err(LayoutError::TooLarge));
        assert_eq!(bl(5).validate_for(7680 * 1280), Ok(()));
        // validate_for reports the layout's own fault first.
        assert_eq!(
            Layout { stride: 4, ..l }.validate_for(u64::MAX),
            Err(LayoutError::Stride)
        );
    }

    #[test]
    fn a_request_whose_layout_does_not_fit_is_refused() {
        // Layout needs 0x7e9000; the object is 4 MiB.
        assert_eq!(
            validate(FLAG_LAYOUT, 4 * MIB, Some(lay())),
            Err(RequestError::Layout(LayoutError::TooLarge))
        );
        assert_eq!(
            validate(FLAG_LAYOUT, 8 * MIB, Some(Layout { stride: 1, ..lay() })),
            Err(RequestError::Layout(LayoutError::Stride))
        );
    }

    #[test]
    fn reserve_commit_records_the_owner_and_counts() {
        let mut t = small();
        let r = t.reserve(10, 8 * MIB).unwrap();
        assert_eq!((r.owner(), r.size()), (10, 8 * MIB));
        // A reservation counts before it is committed.
        assert_eq!(t.live(), 0);
        t.commit(r, 100, 7, 3, 9, lay()).unwrap();
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
            t.commit(r, id, 1, 1, id, lay()).unwrap();
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
        t.commit(r, 1, 1, 1, 1, lay()).unwrap();
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
        t.commit(r, 1, 1, 1, 1, lay()).unwrap();
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
            t.commit(r, id, 1, 1, id, lay()).unwrap();
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
        t.commit(r, 5, 1, 1, 1, lay()).unwrap();
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
        t.commit(r, 5, 1, 1, 1, lay()).unwrap();
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
        assert_eq!(
            t.commit(r, 5, 1, 1, 1, lay()),
            Err(CommitError::NoReservation)
        );
        assert_eq!(t.live(), 0);
    }

    #[test]
    fn duplicate_resource_id_is_refused_and_the_reservation_released() {
        let mut t = small();
        let a = t.reserve(1, MIB).unwrap();
        t.commit(a, 5, 1, 1, 1, lay()).unwrap();
        let b = t.reserve(2, MIB).unwrap();
        assert_eq!(t.commit(b, 5, 1, 1, 1, lay()), Err(CommitError::Duplicate));
        assert_eq!(t.live(), 1);
        // The failed commit consumed the reservation: nothing is left pending.
        assert_eq!(t.owner_live(2), 0);
        for id in 6..=8 {
            let r = t.reserve(id as u64, MIB).unwrap();
            t.commit(r, id, 1, 1, 1, lay()).unwrap();
        }
        assert_eq!(t.live(), 4);
    }

    #[test]
    fn identical_reservations_are_interchangeable_but_counted_once_each() {
        let mut t = small();
        let a = t.reserve(1, MIB).unwrap();
        let b = t.reserve(1, MIB).unwrap();
        assert_eq!(a, b);
        t.commit(a, 1, 1, 1, 1, lay()).unwrap();
        t.commit(b, 2, 1, 1, 2, lay()).unwrap();
        // Both were consumed; a third commit has nothing to draw on.
        assert_eq!(
            t.commit(a, 3, 1, 1, 3, lay()),
            Err(CommitError::NoReservation)
        );
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
            t.commit(r, i + 1, 1, 1, i + 1, lay()).unwrap();
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
            t.commit(r, id, 1, 1, id, lay()).unwrap();
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
        t.commit(r, 1, 1, 1, 1, lay()).unwrap();
        assert_eq!(t.owner_live(2), 0);
        assert_eq!(t.owner_bytes(2), 0);
        assert_eq!(t.owner_live(1), 1);
    }

    // ---- adoption by a WDDM allocation -------------------------------------

    /// One device (owner 1) imported resource 50 on ctx 7 with `lay()`.
    fn with_import() -> ForeignTable {
        let mut t = small();
        let r = t.reserve(1, 8 * MIB).unwrap();
        t.commit(r, 50, 7, 3, 9, lay()).unwrap();
        t
    }

    fn req() -> AdoptRequest {
        AdoptRequest {
            declares_foreign: true,
            take_ownership: true,
            ctx_id: 7,
            width: 1920,
            height: 1080,
            pitch: 7680,
            plane_offset: 0,
            claimed_alloc_size: 0,
            supplied_layout: None,
            trailer_room: true,
        }
    }

    fn adopt(t: &mut ForeignTable, r: &AdoptRequest) -> Result<AdoptPlan, AdoptRefusal> {
        t.adopt_for_allocation(50, r, true, true)
    }

    #[test]
    fn adoption_frees_the_quota_keeps_the_record_and_returns_the_layout() {
        let mut t = with_import();
        assert_eq!(t.owner_live(1), 1);
        assert_eq!(
            adopt(&mut t, &req()),
            Ok(AdoptPlan::Foreign(Adopted {
                size: 8 * MIB,
                layout: lay()
            }))
        );
        // Creator's share freed; record kept (MAP refusal and teardown apply).
        assert_eq!(t.owner_live(1), 0);
        assert_eq!(t.owner_bytes(1), 0);
        assert!(t.contains(50));
        assert_eq!(t.get(50).unwrap().creator, None);
        assert_eq!(t.layout(50), Some(lay()));
        assert_eq!(t.counters().adopted, 1);
        assert_eq!(t.counters().refused_adopt, 0);
        // Teardown removes it exactly once.
        assert!(t.remove(50).is_some());
        assert!(t.remove(50).is_none());
        assert_eq!(t.layout(50), None);
    }

    #[test]
    fn the_second_adoption_is_refused() {
        let mut t = with_import();
        assert!(adopt(&mut t, &req()).is_ok());
        // Two allocations on one resource would release it twice.
        assert_eq!(adopt(&mut t, &req()), Err(AdoptRefusal::AlreadyAdopted));
        assert_eq!(t.counters().adopted, 1);
        assert_eq!(t.counters().refused_adopt, 1);
    }

    #[test]
    fn an_ordinary_venus_resource_is_left_to_the_legacy_path() {
        let mut t = with_import();
        let mut r = req();
        r.declares_foreign = false;
        // Resource 99 has no record: nothing here applies, nothing is counted.
        assert_eq!(
            t.adopt_for_allocation(99, &r, true, true),
            Ok(AdoptPlan::Legacy)
        );
        assert_eq!(t.counters().refused_adopt, 0);
        assert_eq!(t.counters().adopted, 0);
    }

    #[test]
    fn the_declaration_and_the_record_must_agree() {
        let mut t = with_import();
        // Declared foreign, no record (dead, or a plain Venus blob).
        assert_eq!(
            t.adopt_for_allocation(99, &req(), true, true),
            Err(AdoptRefusal::NotForeign)
        );
        // A record, but the caller did not say foreign.
        let mut r = req();
        r.declares_foreign = false;
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::Undeclared));
        // Nothing moved.
        assert_eq!(t.get(50).unwrap().creator, Some(1));
        assert_eq!(t.counters().refused_adopt, 2);
    }

    #[test]
    fn only_a_blob_owning_kind_adopts() {
        let mut t = with_import();
        let mut r = req();
        r.take_ownership = false; // a STANDARD allocation naming a foreign resid
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::NotDeviceMemory));
        assert_eq!(t.get(50).unwrap().creator, Some(1));
    }

    #[test]
    fn the_holder_context_must_be_the_imports_and_still_the_creators() {
        let mut t = with_import();
        let mut r = req();
        r.ctx_id = 8;
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::ContextMismatch));
        assert_eq!(
            t.adopt_for_allocation(50, &req(), false, true),
            Err(AdoptRefusal::ContextGone)
        );
        assert_eq!(
            t.adopt_for_allocation(50, &req(), true, false),
            Err(AdoptRefusal::SlotNotCreators)
        );
        assert_eq!(t.get(50).unwrap().creator, Some(1));
        // And then the correct request still works: refusals changed nothing.
        assert!(adopt(&mut t, &req()).is_ok());
    }

    #[test]
    fn the_allocation_must_repeat_the_recorded_layout() {
        let mut t = with_import();
        for mutate in [
            |r: &mut AdoptRequest| r.width = 1919,
            |r: &mut AdoptRequest| r.height = 1081,
            |r: &mut AdoptRequest| r.pitch = 8192,
            |r: &mut AdoptRequest| r.plane_offset = 4096,
            // An unspecified (0) field is not "don't care": it must be repeated.
            |r: &mut AdoptRequest| r.pitch = 0,
        ] {
            let mut r = req();
            mutate(&mut r);
            assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::GeometryMismatch));
        }
        let mut r = req();
        r.supplied_layout = Some(Layout {
            fourcc: FOURCC_ARGB8888,
            ..lay()
        });
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::LayoutMismatch));
        r.supplied_layout = Some(bl(5));
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::LayoutMismatch));
        // The trailer's own stride and offset are part of the comparison.
        r.supplied_layout = Some(Layout {
            stride: 8192,
            ..lay()
        });
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::LayoutMismatch));
        r.supplied_layout = Some(Layout {
            offset: 4096,
            ..lay()
        });
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::LayoutMismatch));
        // A matching trailer is fine, and so is none.
        r.supplied_layout = Some(lay());
        let mut t2 = with_import();
        assert!(adopt(&mut t2, &r).is_ok());
        assert_eq!(t.get(50).unwrap().creator, Some(1));
        assert!(adopt(&mut t, &req()).is_ok());
    }

    #[test]
    fn the_private_data_must_have_room_for_the_trailer() {
        let mut t = with_import();
        let mut r = req();
        r.trailer_room = false;
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::NoTrailerRoom));
        assert_eq!(t.get(50).unwrap().creator, Some(1));
    }

    #[test]
    fn a_size_claim_over_the_recorded_size_is_refused() {
        let mut t = with_import();
        let mut r = req();
        r.claimed_alloc_size = 8 * MIB + 1;
        assert_eq!(adopt(&mut t, &r), Err(AdoptRefusal::ClaimTooLarge));
        // At or under is fine (the identity reports the recorded size).
        r.claimed_alloc_size = 8 * MIB;
        assert!(adopt(&mut t, &r).is_ok());
    }

    #[test]
    fn the_adopted_state_machine_end_to_end() {
        // import -> adopt (quota freed) -> import again by the same device is
        // allowed up to its own limit -> destroy removes once.
        let mut t = small();
        for id in [50u32, 51] {
            let r = t.reserve(1, 8 * MIB).unwrap();
            t.commit(r, id, 7, 3, id, lay()).unwrap();
        }
        assert_eq!(t.reserve(1, MIB), Err(Quota::OwnerCount));
        assert!(t.adopt_for_allocation(50, &req(), true, true).is_ok());
        assert!(t.reserve(1, MIB).is_ok());
        assert_eq!(t.live(), 2);
        assert!(t.remove(50).is_some());
        assert_eq!(t.counters().released, 1);
        // 51 is still the creator's: its own adoption is independent.
        assert_eq!(t.get(51).unwrap().creator, Some(1));
    }

    #[test]
    fn refusal_codes_are_distinct_and_nonzero() {
        let all = [
            AdoptRefusal::NotForeign,
            AdoptRefusal::Undeclared,
            AdoptRefusal::NotDeviceMemory,
            AdoptRefusal::AlreadyAdopted,
            AdoptRefusal::ContextMismatch,
            AdoptRefusal::ContextGone,
            AdoptRefusal::SlotNotCreators,
            AdoptRefusal::NoTrailerRoom,
            AdoptRefusal::GeometryMismatch,
            AdoptRefusal::LayoutMismatch,
            AdoptRefusal::ClaimTooLarge,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_ne!(a.code(), 0);
            for b in &all[i + 1..] {
                assert_ne!(a.code(), b.code());
            }
        }
    }

    #[test]
    fn refusals_of_adoption_are_in_the_total() {
        let mut t = with_import();
        let _ = t.adopt_for_allocation(99, &req(), true, true);
        assert_eq!(t.counters().refused(), 1);
    }
}
