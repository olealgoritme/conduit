//! RM system memory for the KMD's own CPU-written allocations (`KmdRmClient` = 5): the
//! pure half. Design, the synchronous-creation decision, the failure matrix and the
//! hardware checklist: `docs/kmd-rm-client.md` section 15.
//!
//! Level 5 allocates the VidPn primary (`KmdLinearPrimary`) from RM SYSTEM memory
//! (`NV01_MEMORY_SYSTEM`) instead of a Venus blob, makes it a foreign (RM-export)
//! resource that the WDDM allocation adopts, lets the host map it into the Venus
//! window at the offset dxgkrnl chose (`RESOURCE_MAP_BLOB`), and shows it with the
//! KMD's own `ScanoutFlip` (Option B). Everything the driver DECIDES lives here and is
//! tested on the host:
//!
//! * which allocation kinds go to RM ([`route`]) and why the others do not;
//! * the geometry and size of the allocation ([`layout`], [`adopt_size`]) and the
//!   `NV_MEMORY_ALLOCATION_PARAMS` block ([`params`]);
//! * the cache attribute: what RM is asked for, what dxgkrnl will map, and what the
//!   host's `map_info` must say ([`Cache`], [`PrimaryCache`], [`host_cache_ok`]);
//! * the service's state: bring-up once, a bounded table of live allocations, and the
//!   fail-closed fallback to Venus ([`Svc`]);
//! * which allocation the screen shows ([`TargetBook`]). The flip itself is the level 3
//!   presenter's state machine ([`crate::rm_present::Presenter`] with a ring of one,
//!   whose "copy" is nothing): pacing, retries, giving up and the resume after a user
//!   source are exactly the ones already tested.
//!
//! Nothing here touches memory, the transport or the clock.

use crate::foreign_resource::{
    Layout as FrLayout, FOURCC_ABGR8888, FOURCC_ARGB8888, FOURCC_XRGB8888,
    MAX_FOREIGN_RESOURCE_BYTES, MOD_LINEAR,
};
use crate::foreign_scanout::{Layout as FlipLayout, MAX_DIM, MAX_STRIDE, MIN_DIM};
use crate::rm_client::{Fail, FailKind, MAX_SURFACE_BYTES, MEM_ALLOC_BYTES};
use crate::rm_present::Inputs;
use crate::scanout_release::{ring_wait, Wait};
use crate::sweep_budget::{SweepBudget, UNITS_PER_MS};

/// The `KmdRmClient` value that turns this on.
pub const LEVEL: u32 = 5;
/// `NV01_MEMORY_SYSTEM`.
pub const NV01_MEMORY_SYSTEM: u32 = 0x3e;

// ---- the RM attributes ----------------------------------------------------------------

// `nvos.h` (610.57.04): `NVOS32_ATTR_LOCATION` 26:25, `NVOS32_ATTR_PHYSICALITY` 28:27,
// `NVOS32_ATTR_COHERENCY` 31:29.
const ATTR_LOCATION_SHIFT: u32 = 25;
const ATTR_LOCATION_PCI: u32 = 1;
const ATTR_PHYSICALITY_SHIFT: u32 = 27;
const ATTR_PHYSICALITY_ALLOW_NONCONTIG: u32 = 3;
const ATTR_COHERENCY_SHIFT: u32 = 29;
const ATTR_COHERENCY_CACHED: u32 = 1;
const ATTR_COHERENCY_WRITE_COMBINE: u32 = 2;
const ATTR2_ZBC_PREFER_NO_ZBC: u32 = 1;
const NVOS32_TYPE_IMAGE: u32 = 0;
const NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE: u32 = 0x100;

const ATTR_BASE: u32 = (ATTR_LOCATION_PCI << ATTR_LOCATION_SHIFT)
    | (ATTR_PHYSICALITY_ALLOW_NONCONTIG << ATTR_PHYSICALITY_SHIFT);
/// `attr` of cached system memory: PCI, cached, not necessarily contiguous
/// (`rm_sysmem_flip.c`'s `sysmem` kind).
pub const ATTR_CACHED: u32 = ATTR_BASE | (ATTR_COHERENCY_CACHED << ATTR_COHERENCY_SHIFT);
/// `attr` of write-combined system memory (`sysmem-wc`).
pub const ATTR_WC: u32 = ATTR_BASE | (ATTR_COHERENCY_WRITE_COMBINE << ATTR_COHERENCY_SHIFT);
/// `attr2`: no ZBC (the flip source is never compressed).
pub const ATTR2: u32 = ATTR2_ZBC_PREFER_NO_ZBC;
/// RM system memory is page-granular.
pub const PAGE: u64 = 4096;
/// `alignment` asked of RM.
pub const ALIGNMENT: u64 = PAGE;

const _: () = assert!(ATTR_CACHED == 0x3a00_0000);
const _: () = assert!(ATTR_WC == 0x5a00_0000);

/// RM object handles of the service's allocations (its own client: the namespace is
/// not the ring client's). Slot `i` is `H_BASE + i`, so a handle is never reused while
/// its slot is in use.
pub const H_BASE: u32 = 0x4b4d_2000;

/// `virtio_gpu` `map_info` caching nibble values (`helios_protocol::VIRTIO_GPU_MAP_CACHE_*`;
/// the driver asserts the two agree at compile time).
pub const MAP_CACHE_CACHED: u32 = 0x01;
pub const MAP_CACHE_UNCACHED: u32 = 0x02;
pub const MAP_CACHE_WC: u32 = 0x03;

// ---- which allocations ----------------------------------------------------------------

/// The KMD's allocation classes (`helios_protocol::AllocationBacking`, minus the ones
/// that are not the KMD's own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `KmdLinearPrimary`: the VidPn primary, CPU-written by GDI.
    LinearPrimary,
    /// `KmdStandardBuffer`: `primary` is the ABI-defensive primary flag; a buffer that
    /// is not one is the shadow / staging / GDI staging / external Present buffer.
    StandardBuffer { primary: bool },
    /// `KmdOptimalGdiTexture`: a tiled image, never CPU-written.
    OptimalGdiTexture,
    /// A UMD's adopted resource, a raw HOST3D blob, a VidMm tracking allocation.
    Other,
}

/// Why an allocation was not (or could not be) made from RM system memory. The codes
/// are the `RmSysWhy` breadcrumb, so they never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Why {
    KnobOff = 1,
    /// A standard buffer: DWM opens it as Venus memory by identity and the Present buffer
    /// ABI (`allocate_present_buffer_blob`, `register_present_buffer`, the system-backing
    /// policy) is Venus-specific: not simple, so not RM (15.3).
    PresentBuffer = 2,
    /// A tiled GPU-only image: not CPU-written, so not system memory.
    NotCpuWritten = 3,
    /// Not a KMD-created allocation.
    NotKmdOwned = 4,
    /// The ABI-defensive `KmdStandardBuffer { primary: true }`.
    OddPrimary = 5,
    NoDisplay = 6,
    NoTransport = 7,
    NoContext = 8,
    Format = 9,
    Extent = 10,
    Size = 11,
    /// Bring-up failed for this transport generation.
    Dead = 12,
    /// Too many allocations in a row failed: no new ones this generation.
    NoNew = 13,
    /// Another thread's bring-up did not finish in time.
    BringUpBusy = 14,
    TableFull = 15,
    BringUp = 16,
    /// `RM_ALLOC`, export, GEM import or the close of the export file failed.
    Alloc = 17,
    /// The foreign import (the Venus resource) failed.
    Import = 18,
    /// The host's `map_info` is not the cache attribute the memory was made with.
    Cache = 19,
    /// The trial `RESOURCE_MAP_BLOB` failed.
    Trial = 20,
    /// The WDDM allocation could not adopt the resource.
    Adopt = 21,
    /// The creation took longer than its allowance.
    Slow = 22,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// Where `kind` is allocated at `level`: `Ok(())` = RM system memory, `Err` = Venus and
/// why. Only the primary is RM; the table is the decision of 15.3.
pub fn route(level: u32, kind: Kind) -> Result<(), Why> {
    if level < LEVEL {
        return Err(Why::KnobOff);
    }
    match kind {
        Kind::LinearPrimary => Ok(()),
        Kind::StandardBuffer { primary: true } => Err(Why::OddPrimary),
        Kind::StandardBuffer { primary: false } => Err(Why::PresentBuffer),
        Kind::OptimalGdiTexture => Err(Why::NotCpuWritten),
        Kind::Other => Err(Why::NotKmdOwned),
    }
}

// ---- geometry and size ----------------------------------------------------------------

/// What an RM system-memory allocation is: pitch-linear, 32 bits per pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row: `width * 4` rounded up to 256 (what the KMD authors for every
    /// standard allocation, `cross_adapter_pitch`).
    pub pitch: u32,
    /// What RM is asked for: `pitch * height` rounded up to a page.
    pub size: u64,
    pub fourcc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    Extent,
    Format,
    Size,
}

impl LayoutError {
    pub const fn why(self) -> Why {
        match self {
            LayoutError::Extent => Why::Extent,
            LayoutError::Format => Why::Format,
            LayoutError::Size => Why::Size,
        }
    }
}

/// `DRM_FORMAT_*` of the KMD's DXGI format (28 R8G8B8A8, 87 B8G8R8A8, 88 B8G8R8X8: the
/// three it ever names); `None` for anything else (0 is the legacy "no hint").
pub const fn fourcc_for_dxgi(dxgi: u32) -> Option<u32> {
    match dxgi {
        28 => Some(FOURCC_ABGR8888),
        87 => Some(FOURCC_ARGB8888),
        88 => Some(FOURCC_XRGB8888),
        _ => None,
    }
}

/// The layout of a `width` x `height` allocation of DXGI format `dxgi`.
pub fn layout(width: u32, height: u32, dxgi: u32) -> Result<SysLayout, LayoutError> {
    if !(MIN_DIM..=MAX_DIM).contains(&width) || !(MIN_DIM..=MAX_DIM).contains(&height) {
        return Err(LayoutError::Extent);
    }
    let fourcc = fourcc_for_dxgi(dxgi).ok_or(LayoutError::Format)?;
    let pitch = crate::cross_adapter_pitch(width);
    if pitch > MAX_STRIDE || u64::from(pitch) < u64::from(width) * 4 {
        return Err(LayoutError::Extent);
    }
    let size = crate::round_up_page(u64::from(pitch) * u64::from(height));
    if size > MAX_SURFACE_BYTES || size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(LayoutError::Size);
    }
    Ok(SysLayout {
        width,
        height,
        pitch,
        size,
        fourcc,
    })
}

/// What RM answered for the size (`size` of the parameter block, offset 64: RM may round
/// it up). `reported` 0 means RM left it alone. The size adopted is page-granular, holds
/// the asked-for one and stays inside what the KMD and the host accept; it is the size
/// the foreign record, the blob table, the WDDM allocation and VidMm all see.
pub fn adopt_size(want: u64, reported: u64) -> Result<u64, Why> {
    let size = if reported == 0 { want } else { reported };
    if size < want {
        return Err(Why::Size);
    }
    let size = crate::round_up_page(size);
    if size > MAX_SURFACE_BYTES || size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(Why::Size);
    }
    Ok(size)
}

/// The layout the foreign record and every importer are told, for a size RM settled on.
/// `None` if it does not hold the picture.
pub fn foreign_layout(l: &SysLayout, size: u64) -> Option<FrLayout> {
    let fl = FrLayout {
        width: l.width,
        height: l.height,
        stride: l.pitch,
        offset: 0,
        fourcc: l.fourcc,
        modifier: MOD_LINEAR,
    };
    fl.validate_for(size).ok().map(|()| fl)
}

/// The flip layout of a foreign layout (`ScanoutFlip` carries the same words).
pub const fn flip_layout(l: &FrLayout) -> FlipLayout {
    FlipLayout {
        width: l.width,
        height: l.height,
        stride: l.stride,
        offset: l.offset,
        fourcc: l.fourcc,
        modifier: l.modifier,
    }
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// `NV_MEMORY_ALLOCATION_PARAMS` (128 bytes) of system memory, as `crm_alloc` /
/// `rm_sysmem_flip.c` fill it: owner, type IMAGE, `ALIGNMENT_FORCE`, `attr`, `attr2`,
/// `size` and `alignment`; width, height and pitch stay zero (they are hints for video
/// memory). The backend learns "system, cached or write-combined" only from what RM
/// writes back of `attr` (`0x2a800000` / `0x4a800000`), so this is the only place the
/// kind is chosen.
pub fn params(root: u32, cache: Cache, size: u64) -> [u8; MEM_ALLOC_BYTES] {
    let mut a = [0u8; MEM_ALLOC_BYTES];
    put32(&mut a, 0, root); // owner
    put32(&mut a, 4, NVOS32_TYPE_IMAGE);
    put32(&mut a, 8, NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE);
    put32(&mut a, 24, cache.attr());
    put32(&mut a, 28, ATTR2);
    put64(&mut a, 64, size);
    put64(&mut a, 72, ALIGNMENT);
    a
}

/// The size RM wrote back into a reply's parameter block (`nested`), 0 if short.
pub fn reply_size(nested: &[u8]) -> u64 {
    match nested.get(64..72) {
        Some(s) => u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]),
        None => 0,
    }
}

// ---- the cache attribute --------------------------------------------------------------

/// How CPU views of the memory are cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    Cached,
    WriteCombine,
}

impl Cache {
    pub const fn attr(self) -> u32 {
        match self {
            Cache::Cached => ATTR_CACHED,
            Cache::WriteCombine => ATTR_WC,
        }
    }

    /// The `map_info` nibble the host reports for it.
    pub const fn nibble(self) -> u32 {
        match self {
            Cache::Cached => MAP_CACHE_CACHED,
            Cache::WriteCombine => MAP_CACHE_WC,
        }
    }
}

/// The `KmdRmSysCache` knob: what the PRIMARY is made of, and how it is told to dxgkrnl.
///
/// dxgkrnl maps a CpuVisible allocation's aperture write-combined unless the allocation
/// carries `Cached`, and rejected `Cached` together with the primary (the 36th-session
/// finding), so the primary's own CPU view is write-combined whatever memory sits behind
/// it. Cached memory behind that view would be a write-back / write-combined ALIAS of the
/// same physical pages, a mixed-attribute mapping that is architecturally invalid and that
/// nobody has measured on this stack, so it is NOT the default: the default is
/// write-combined memory, every view of which agrees with dxgkrnl's (parity with the
/// Venus primary, whose aperture view was write-combined too: GDI reads were already
/// write-combined). The cached variants are explicit opt-ins for the hardware run that
/// measures them (15.5, 15.13).
///
/// The knob values (anything else, 0 included, is the default: an unknown value never
/// picks an alias):
///
/// | value | variant | memory | dxgkrnl's view | alias |
/// |---|---|---|---|---|
/// | 0 (absent) | [`PrimaryCache::WriteCombine`] | write-combined | write-combined | no |
/// | 1 | [`PrimaryCache::WriteCombine`] | the same, spelled out | | no |
/// | 2 | [`PrimaryCache::CachedFlag`] | cached | write-back (asked with `Cached`) | no if dxgkrnl takes it |
/// | 3 | [`PrimaryCache::CachedAlias`] | cached | write-combined | YES |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrimaryCache {
    /// 0 / 1 (default): write-combined system memory: every view agrees with dxgkrnl's, no
    /// alias. Slow reads and writes (the host measured 75 MB/s and 28 MB/s), the same as the
    /// primary's mapping is today.
    #[default]
    WriteCombine,
    /// 2 (opt-in experiment): cached system memory AND the `Cached` flag on the primary, so
    /// dxgkrnl maps it write-back too and there is no alias. dxgkrnl may refuse the
    /// allocation.
    CachedFlag,
    /// 3 (opt-in): cached system memory while dxgkrnl's view stays write-combined: a mixed
    /// attribute alias of the same pages (counted, `RmSysAlias`), fast for the KMD's own
    /// kernel maps. Only for a run that watches for stale lines.
    CachedAlias,
}

impl PrimaryCache {
    pub const fn from_knob(v: u32) -> Self {
        match v {
            2 => PrimaryCache::CachedFlag,
            3 => PrimaryCache::CachedAlias,
            _ => PrimaryCache::WriteCombine,
        }
    }

    /// What RM is asked for.
    pub const fn sysmem(self) -> Cache {
        match self {
            PrimaryCache::WriteCombine => Cache::WriteCombine,
            PrimaryCache::CachedFlag | PrimaryCache::CachedAlias => Cache::Cached,
        }
    }

    /// Whether the allocation is created with the `Cached` flag.
    pub const fn cached_flag(self) -> bool {
        matches!(self, PrimaryCache::CachedFlag)
    }

    /// How dxgkrnl maps the primary's aperture (`alloc_cached` is the `AllocCached`
    /// kill switch: with it off nothing is flagged `Cached`).
    pub const fn dxgkrnl_view(self, alloc_cached: bool) -> Cache {
        if alloc_cached && self.cached_flag() {
            Cache::Cached
        } else {
            Cache::WriteCombine
        }
    }

    /// Whether the memory and dxgkrnl's view of it differ in cache attribute (an alias
    /// of the same pages). Counted (`RmSysAlias`), never a refusal. It exists only when the
    /// alias was asked for (`CachedAlias`), or `Cached` was asked for and `AllocCached`
    /// took it away; the default never has one.
    pub const fn aliases(self, alloc_cached: bool) -> bool {
        !matches!(
            (self.sysmem(), self.dxgkrnl_view(alloc_cached)),
            (Cache::Cached, Cache::Cached) | (Cache::WriteCombine, Cache::WriteCombine)
        )
    }
}

/// The host's `map_info` of a trial map agrees with what the memory was made with. A
/// host that reports another attribute is mapping it differently from what RM thinks:
/// the allocation is given up (Venus) rather than aliased.
pub const fn host_cache_ok(made: Cache, nibble: u32) -> bool {
    nibble == made.nibble()
}

// ---- the service ----------------------------------------------------------------------

/// Live allocations the service tracks (the RM handle of one is `H_BASE + slot`).
pub const TABLE_CAP: usize = 32;
/// Failed creations in a row after which no new allocation is made this generation.
pub const MAX_STRIKES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing opened.
    Cold,
    /// One thread is bringing the RM client up.
    BringingUp,
    Up,
    /// [`MAX_STRIKES`] creations failed in a row: no new allocations; the live ones and
    /// the client stay (their memory, GEMs and the flip source depend on them).
    NoNew,
    /// Bring-up failed.
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    /// Reserved by a creation in progress.
    Building,
    Live,
    /// A release took it; the RM objects are being closed.
    Closing,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    state: SlotState,
    resid: u32,
    gem: u32,
    epoch: u64,
}

impl Slot {
    const FREE: Slot = Slot {
        state: SlotState::Free,
        resid: 0,
        gem: 0,
        epoch: 0,
    };
}

/// What [`Svc::admit`] says to a creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// The caller brings the client up (and reports with [`Svc::bring_up_done`]).
    BringUp,
    /// Another thread is bringing it up: wait a moment and ask again.
    Wait,
    /// Go: slot `0` is reserved ([`Svc::commit`] or [`Svc::abort`] it).
    Go(usize),
    Refuse(Why),
}

/// A slot [`Svc::take`] closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub slot: usize,
    pub gem: u32,
    pub epoch: u64,
}

/// The service state: plain data under one leaf spinlock.
#[derive(Debug, Clone, Copy)]
pub struct Svc {
    phase: Phase,
    epoch: u64,
    strikes: u8,
    live: u32,
    slots: [Slot; TABLE_CAP],
}

impl Svc {
    pub const fn new() -> Self {
        Svc {
            phase: Phase::Cold,
            epoch: 0,
            strikes: 0,
            live: 0,
            slots: [Slot::FREE; TABLE_CAP],
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn strikes(&self) -> u8 {
        self.strikes
    }
    /// Allocations alive (committed, not yet freed).
    pub fn live(&self) -> u32 {
        self.live
    }

    /// The RM handle of slot `i`.
    pub const fn handle(slot: usize) -> u32 {
        H_BASE + slot as u32
    }

    /// Forget everything (the transport generation ended; the sweep closed the host side).
    pub fn reset(&mut self) {
        *self = Svc::new();
    }

    /// Reconcile with transport generation `now` (0 = none): a different one drops
    /// everything. Returns whether anything was dropped.
    pub fn sync_epoch(&mut self, now: u64) -> bool {
        if now == self.epoch {
            return false;
        }
        let had = self.phase != Phase::Cold || self.live != 0;
        self.reset();
        self.epoch = now;
        had
    }

    /// A creation asks to go. Takes what it needs: the bring-up duty, or a slot.
    pub fn admit(&mut self, epoch: u64) -> Admit {
        self.sync_epoch(epoch);
        match self.phase {
            Phase::Dead => Admit::Refuse(Why::Dead),
            Phase::NoNew => Admit::Refuse(Why::NoNew),
            Phase::BringingUp => Admit::Wait,
            Phase::Cold => {
                self.phase = Phase::BringingUp;
                Admit::BringUp
            }
            Phase::Up => match self.slots.iter().position(|s| s.state == SlotState::Free) {
                Some(i) => {
                    self.slots[i] = Slot {
                        state: SlotState::Building,
                        resid: 0,
                        gem: 0,
                        epoch,
                    };
                    Admit::Go(i)
                }
                None => Admit::Refuse(Why::TableFull),
            },
        }
    }

    /// The answer to [`Admit::BringUp`].
    pub fn bring_up_done(&mut self, ok: bool) {
        if self.phase == Phase::BringingUp {
            self.phase = if ok { Phase::Up } else { Phase::Dead };
        }
    }

    /// The creation in `slot` succeeded: the resource `resid` is backed by GEM `gem`.
    pub fn commit(&mut self, slot: usize, resid: u32, gem: u32) {
        if let Some(s) = self.slots.get_mut(slot) {
            if s.state == SlotState::Building {
                s.state = SlotState::Live;
                s.resid = resid;
                s.gem = gem;
                self.live += 1;
                self.strikes = 0;
            }
        }
    }

    /// The creation in `slot` failed and was undone: a strike; the last one stops new
    /// allocations for the generation.
    pub fn abort(&mut self, slot: usize) {
        self.abort_slot(slot, false);
    }

    /// The creation in `slot` failed and the undo could not make sure RM freed its memory
    /// ([`mem_may_be_live`]): the same strike, but the slot is NOT free. Its RM handle
    /// ([`Svc::handle`]) may still be RM's, and a creation that took the slot again would
    /// `RM_ALLOC` the same number and be refused as a duplicate (three of those end the
    /// generation's allocations). The slot stays `Closing`, the state of a destroyed
    /// allocation whose free failed (`sysmem::released`): nothing takes it and nothing finds
    /// it, and the generation's end (the transport sweep closes the client, which frees the
    /// memory) resets the table.
    pub fn abort_leaked(&mut self, slot: usize) {
        self.abort_slot(slot, true);
    }

    fn abort_slot(&mut self, slot: usize, quarantine: bool) {
        if let Some(s) = self.slots.get_mut(slot) {
            if s.state == SlotState::Building {
                *s = if quarantine {
                    Slot {
                        state: SlotState::Closing,
                        resid: 0,
                        gem: 0,
                        epoch: s.epoch,
                    }
                } else {
                    Slot::FREE
                };
                self.strikes = self.strikes.saturating_add(1);
                if self.strikes >= MAX_STRIKES && self.phase == Phase::Up {
                    self.phase = Phase::NoNew;
                }
            }
        }
    }

    /// Slots a failed creation left quarantined (taken, nothing live in them): for the
    /// counters and the tests.
    pub fn quarantined(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Closing && s.resid == 0)
            .count()
    }

    /// A creation that did not fail in the service (a Venus fallback for another
    /// reason, say) leaves the strikes alone. Nothing to call.
    ///
    /// The live slot of resource `resid`, for a flip: `(slot, gem, epoch)`.
    pub fn find(&self, resid: u32) -> Option<(usize, u32, u64)> {
        self.slots
            .iter()
            .enumerate()
            .find(|(_, s)| s.state == SlotState::Live && s.resid == resid)
            .map(|(i, s)| (i, s.gem, s.epoch))
    }

    /// A release of `resid`: takes the live slot (no flip finds it from now on) and hands
    /// back what must be closed. `None` if `resid` is not (or no longer) one of ours.
    pub fn take(&mut self, resid: u32) -> Option<Taken> {
        let i = self
            .slots
            .iter()
            .position(|s| s.state == SlotState::Live && s.resid == resid)?;
        let s = &mut self.slots[i];
        s.state = SlotState::Closing;
        Some(Taken {
            slot: i,
            gem: s.gem,
            epoch: s.epoch,
        })
    }

    /// The RM objects of a taken slot are closed (or given up on): the slot is free.
    pub fn freed(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            if s.state == SlotState::Closing {
                *s = Slot::FREE;
                self.live = self.live.saturating_sub(1);
            }
        }
    }
}

impl Default for Svc {
    fn default() -> Self {
        Self::new()
    }
}

// ---- how long a creation may take -----------------------------------------------------------

/// The whole forward part of one creation: the wait for another thread's bring-up, the
/// bring-up itself (eleven messages), the memory, the import, the trial map and the adopt.
/// One monotonic deadline (`now_100ns`, interrupt time) for all of it: a count of sleeps is
/// not a bound (`sleep_ms(1)` lasts a timer tick, about 15.6 ms), and eleven messages of
/// 2.5 s each are not one.
pub const CREATE_BUDGET_MS: u64 = 6_000;
/// What the undo of a failed creation may spend, on its own allowance: the create budget may
/// be the very thing that ran out (`Why::Slow`), and an undo that could send nothing would
/// leave everything the creation made to the transport sweep. The worst case of a creation
/// that fails is therefore [`CREATE_BUDGET_MS`] + this.
pub const UNDO_BUDGET_MS: u64 = 3_000;

/// The create budget starting at `now` (interrupt time, 100 ns), no message waiting longer
/// than `call_cap_ms`.
pub const fn create_budget(now: u64, call_cap_ms: u64) -> SweepBudget {
    SweepBudget::new(now, CREATE_BUDGET_MS * UNITS_PER_MS, call_cap_ms)
}

/// The undo budget starting at `now`.
pub const fn undo_budget(now: u64, call_cap_ms: u64) -> SweepBudget {
    SweepBudget::new(now, UNDO_BUDGET_MS * UNITS_PER_MS, call_cap_ms)
}

// ---- a failed creation and the RM handle of its slot ------------------------------------------

/// `NV_ERR_OBJECT_NOT_FOUND`: what `RM_FREE` answers for a handle RM does not hold.
pub const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;

/// What is known of the `RM_ALLOC` of a creation's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AllocOutcome {
    /// Nothing was sent, or RM (or the host, or the KMD's own policy) refused it: no object.
    #[default]
    NotMade,
    /// RM said yes.
    Made,
    /// The request may have reached RM and the answer did not come back (a timeout, a
    /// transport error, a reply that cannot be read): the object may exist.
    Unknown,
}

impl AllocOutcome {
    /// What a failed `alloc_sys` leaves open, by how it failed. A `Transport` failure or an
    /// unreadable reply came after the request was sent (the host may have allocated);
    /// `Refused` (the KMD's own policy), `Host`, `Rm`, `Layout`, `Os` and `Busy` are either
    /// an answer that says no or a failure before anything was sent.
    pub const fn after_failure(kind: FailKind) -> Self {
        match kind {
            FailKind::Transport | FailKind::Parse => AllocOutcome::Unknown,
            FailKind::Refused
            | FailKind::Host
            | FailKind::Rm
            | FailKind::Layout
            | FailKind::Os
            | FailKind::Busy => AllocOutcome::NotMade,
        }
    }
}

/// What the undo's `RM_FREE` of the memory came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeOutcome {
    /// Not sent (there was nothing to free).
    NotSent,
    Freed,
    /// RM answered that it holds no such object.
    NotFound,
    /// Refused, timed out or not sent for want of budget: unknown.
    Failed,
}

impl FreeOutcome {
    pub fn of(r: Result<(), Fail>) -> Self {
        match r {
            Ok(()) => FreeOutcome::Freed,
            Err(f) if f.kind == FailKind::Rm && f.code == NV_ERR_OBJECT_NOT_FOUND => {
                FreeOutcome::NotFound
            }
            Err(_) => FreeOutcome::Failed,
        }
    }
}

/// Whether RM may still hold the object of the slot's handle after a creation's undo. If so
/// the slot must not be reused ([`Svc::abort_leaked`]). Only an answer settles it: RM freed
/// it, or RM says it never had it. No attempt at all (nothing was allocated, or the request
/// was refused) is settled too; a free that failed is not, whatever the allocation said.
pub const fn mem_may_be_live(alloc: AllocOutcome, free: FreeOutcome) -> bool {
    match (alloc, free) {
        (AllocOutcome::NotMade, _) => false,
        (_, FreeOutcome::Freed | FreeOutcome::NotFound) => false,
        (_, FreeOutcome::NotSent | FreeOutcome::Failed) => true,
    }
}

// ---- the presenter's restart --------------------------------------------------------------

/// How long after the flip service gave up (three failed attempts in a row) it starts over:
/// 5 s.
pub const RESTART_AFTER_GIVING_UP_100NS: u64 = 50_000_000;

/// The time the worker must sleep until, while a presenter that gave up waits to start over
/// (`restart_at` is that moment, 0 for none), or `None` once it is due. A reset presenter
/// does not wait by itself: its first `decide` is `Act::Register` at once (`retry_at` is 0),
/// so without this gate every desktop frame edge would restart register, flip, fail,
/// withdraw (each failed flip holding the worker for the flip's timeout).
pub const fn restart_pause(restart_at: u64, now: u64) -> Option<u64> {
    if restart_at != 0 && now < restart_at {
        Some(restart_at)
    } else {
        None
    }
}

// ---- what the screen shows ---------------------------------------------------------------

/// The RM primary the flips name: the resource, the DRM file and GEM it is, the layout
/// the arbiter registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub resid: u32,
    pub drm: u32,
    pub gem: u32,
    pub epoch: u64,
    pub layout: FlipLayout,
}

/// What a [`TargetBook::set`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing was shown before.
    New,
    /// The same primary again (a repeated `SetVidPnSourceAddress`).
    Same,
    /// Another primary replaced it.
    Replaced,
}

/// The one RM primary that is the screen's source (plain data under a leaf lock).
#[derive(Debug, Clone, Copy)]
pub struct TargetBook {
    cur: Option<Target>,
}

impl TargetBook {
    pub const fn new() -> Self {
        TargetBook { cur: None }
    }

    pub fn current(&self) -> Option<Target> {
        self.cur
    }

    pub fn set(&mut self, t: Target) -> Change {
        let change = match self.cur {
            None => Change::New,
            Some(c) if c == t => Change::Same,
            Some(_) => Change::Replaced,
        };
        self.cur = Some(t);
        change
    }

    /// The allocation `resid` is being destroyed: if it is the one shown, forget it and
    /// say so (the resident source must be withdrawn before its GEM is closed).
    pub fn gone(&mut self, resid: u32) -> bool {
        match self.cur {
            Some(c) if c.resid == resid => {
                self.cur = None;
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.cur = None;
    }
}

impl Default for TargetBook {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the shown target is usable in transport generation `epoch`.
pub fn target_ready(t: Option<&Target>, epoch: u64) -> bool {
    t.is_some_and(|t| t.epoch == epoch && epoch != 0)
}

// ---- the flip service's inputs, and the release seam ------------------------------------

/// The presenter inputs of one level 5 pass. The flip service is the level 3 presenter
/// with a ring of ONE, and a ring of one never waits for a release: the one buffer is
/// the one on the scanout, and the host never releases the buffer on the scanout (it is
/// replaced only by itself, a re-flip, and "a buffer flipped again is released again
/// later"). `release_tracked` is therefore always `false` here: the answer to "has the
/// host released the surface the next frame writes" is not asked, because the surface the
/// next frame writes is the one on screen (the CPU keeps writing the primary in place,
/// the flip only tells the viewer it changed). The presenter itself agrees
/// (`Presenter::back_wait_seq` is `None` for a one-surface ring, tested below), so even
/// real release answers could not hold a flip; this makes it a fact of the call, not a
/// property to rediscover.
pub const fn flip_inputs(
    now: u64,
    ready: bool,
    arbiter_has_resident: bool,
    foreground: bool,
    frame_edge: bool,
    resume_edge: bool,
) -> Inputs {
    Inputs {
        now,
        ring_ready: ready,
        source_ok: ready,
        arbiter_has_resident,
        foreground,
        frame_edge,
        resume_edge,
        release_tracked: false,
        back_released: true,
    }
}

/// What the viewer was last told to show, and the buffer that flip replaced: the facts the
/// one place that can WAIT for the host's `ScanoutReleased` needs, which is the close of a
/// GEM (a flip never waits, see [`CloseWait`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlipLog {
    /// `(gem, seq)` of the newest flip the host took.
    cur: Option<(u32, u64)>,
    /// The previous DIFFERENT buffer: `(gem, its newest seq, when the flip that replaced
    /// it was taken)`. Only one is kept: a buffer replaced twice ago is long released (or
    /// aged out of the book) and a close that finds nothing goes ahead.
    prev: Option<(u32, u64, u64)>,
}

/// What closing GEM `gem` has to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseWait {
    /// Nothing: the GEM was never flipped, is the buffer ON the scanout (the host never
    /// releases that one: waiting would last until the limit for nothing), or is older
    /// than the log remembers.
    Free,
    /// The GEM was shown and then replaced by another buffer's flip taken at `at`
    /// (100 ns): the host's release of flip `seq` is due, and the book answers
    /// `is_done(seq)`.
    Replaced { seq: u64, at: u64 },
}

impl FlipLog {
    pub const fn new() -> Self {
        FlipLog {
            cur: None,
            prev: None,
        }
    }

    /// The host took the flip of `gem` as `seq` at `now`.
    pub fn flipped(&mut self, gem: u32, seq: u64, now: u64) {
        match self.cur {
            // A re-flip of the buffer on the scanout (the ring of one's whole life): still
            // the current buffer, nothing was replaced.
            Some((g, _)) if g == gem => self.cur = Some((gem, seq)),
            Some((g, s)) => {
                self.prev = Some((g, s, now.max(1)));
                self.cur = Some((gem, seq));
            }
            None => self.cur = Some((gem, seq)),
        }
        // A buffer that is shown again is not a replaced one any more.
        if matches!(self.prev, Some((g, _, _)) if g == gem) {
            self.prev = None;
        }
    }

    /// What a close of `gem` must wait for.
    pub fn closing(&self, gem: u32) -> CloseWait {
        match self.prev {
            Some((g, seq, at)) if g == gem => CloseWait::Replaced { seq, at },
            _ => CloseWait::Free,
        }
    }

    /// `gem` was closed: nothing is remembered of it (the host forgets it with no event).
    pub fn forget(&mut self, gem: u32) {
        if matches!(self.cur, Some((g, _)) if g == gem) {
            self.cur = None;
        }
        if matches!(self.prev, Some((g, _, _)) if g == gem) {
            self.prev = None;
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }
}

impl Default for FlipLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The wait rule of a GEM close: `released` is the book's answer for `w`'s `seq`. The
/// same rule and the same 500 ms limit, from the replacing flip, as the level 3 ring
/// presenter's wait (`scanout_release::ring_wait`: the host overrules a client that holds
/// a replaced buffer after 500 ms, and so does the close).
///
/// THE RELEASE SEAM, decided. A level 5 FLIP never waits for a release: the buffer it
/// shows is either the one already on the scanout (a re-flip: never released while shown,
/// so a wait would last the whole limit for nothing, every frame) or ANOTHER primary (a
/// mode change), whose flip is itself what makes the host release the previous one: a
/// wait before it could only time out. The one thing that can need a release is giving
/// the memory back: the GEM of a primary that WAS replaced is closed (and its RM memory
/// freed) only once the host released it, or the limit passed.
pub fn close_gate(w: CloseWait, released: bool, now: u64) -> Wait {
    match w {
        CloseWait::Free => Wait::Go,
        CloseWait::Replaced { at, .. } => ring_wait(released, at, now),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::foreign_scanout::{ForeignScanout, ResidentKind};
    use crate::paging::{clamp_range, Clamp};
    use crate::rm_present::{Act, FlipResult, Inputs, Presenter};

    // ---- which allocations ----------------------------------------------------

    #[test]
    fn only_the_primary_goes_to_rm_and_only_at_level_5() {
        for level in 0..5 {
            assert_eq!(route(level, Kind::LinearPrimary), Err(Why::KnobOff));
        }
        assert_eq!(route(5, Kind::LinearPrimary), Ok(()));
        // Values above 5 count as 5 in the driver; the policy does not care.
        assert_eq!(route(6, Kind::LinearPrimary), Ok(()));
        assert_eq!(
            route(5, Kind::StandardBuffer { primary: false }),
            Err(Why::PresentBuffer)
        );
        assert_eq!(
            route(5, Kind::StandardBuffer { primary: true }),
            Err(Why::OddPrimary)
        );
        assert_eq!(route(5, Kind::OptimalGdiTexture), Err(Why::NotCpuWritten));
        assert_eq!(route(5, Kind::Other), Err(Why::NotKmdOwned));
    }

    #[test]
    fn why_codes_are_stable_and_nonzero() {
        // They are a registry breadcrumb: append, never renumber.
        assert_eq!(Why::KnobOff.code(), 1);
        assert_eq!(Why::Dead.code(), 12);
        assert_eq!(Why::NoNew.code(), 13);
        assert_eq!(Why::Cache.code(), 19);
        assert_eq!(Why::Slow.code(), 22);
    }

    // ---- sizes -----------------------------------------------------------------

    #[test]
    fn the_primary_is_pitch_linear_256_and_page_granular() {
        let l = layout(1920, 1080, 88).unwrap();
        assert_eq!(
            (l.pitch, l.size, l.fourcc),
            (7680, 8_294_400, FOURCC_XRGB8888)
        );
        assert_eq!(l.size % PAGE, 0);
        // The 1896-wide mode whose width*4 = 7584 once sheared.
        let l = layout(1896, 1030, 88).unwrap();
        assert_eq!(l.pitch, 7680);
        assert_eq!(l.size, crate::round_up_page(7680 * 1030));
        // 5120x1440: 29,491,200 bytes, 7200 pages.
        let l = layout(5120, 1440, 88).unwrap();
        assert_eq!((l.pitch, l.size), (20_480, 29_491_200));
        // A size that is not a page multiple is rounded up, never down.
        let l = layout(65, 65, 87).unwrap();
        assert_eq!(l.pitch, 512);
        assert_eq!(l.size, 36_864);
        assert!(l.size >= u64::from(l.pitch) * 65);
        assert_eq!(l.fourcc, FOURCC_ARGB8888);
    }

    #[test]
    fn layouts_outside_what_every_consumer_accepts_are_refused() {
        assert_eq!(layout(63, 1080, 88), Err(LayoutError::Extent));
        assert_eq!(layout(1920, 63, 88), Err(LayoutError::Extent));
        assert_eq!(layout(16_385, 64, 88), Err(LayoutError::Extent));
        assert_eq!(layout(1920, 1080, 0), Err(LayoutError::Format));
        assert_eq!(layout(1920, 1080, 61), Err(LayoutError::Format));
        // 16384 x 16384 x 4 = 1 GiB > the 256 MiB cap.
        assert_eq!(layout(16_384, 16_384, 88), Err(LayoutError::Size));
        assert_eq!(layout(8192, 8193, 88), Err(LayoutError::Size));
        assert!(layout(8192, 8192, 88).is_ok(), "exactly the cap");
        assert!(layout(4096, 4096, 88).is_ok());
        assert_eq!(LayoutError::Size.why(), Why::Size);
    }

    #[test]
    fn rms_answer_is_adopted_only_if_it_holds_the_request() {
        assert_eq!(adopt_size(8_294_400, 0), Ok(8_294_400));
        assert_eq!(adopt_size(8_294_400, 8_294_400), Ok(8_294_400));
        // RM rounds up (to its 64 KiB): adopted, page-granular.
        assert_eq!(adopt_size(8_294_400, 8_323_072), Ok(8_323_072));
        assert_eq!(adopt_size(8_294_400, 8_294_401), Ok(8_298_496));
        assert_eq!(adopt_size(8_294_400, 8_294_399), Err(Why::Size));
        assert_eq!(
            adopt_size(8_294_400, MAX_SURFACE_BYTES + PAGE),
            Err(Why::Size)
        );
    }

    #[test]
    fn the_foreign_layout_must_fit_the_size_rm_settled_on() {
        let l = layout(1920, 1080, 88).unwrap();
        let f = foreign_layout(&l, l.size).unwrap();
        assert_eq!(
            (f.width, f.height, f.stride, f.offset, f.modifier),
            (1920, 1080, 7680, 0, MOD_LINEAR)
        );
        assert!(foreign_layout(&l, l.size - PAGE).is_none());
        let flip = flip_layout(&f);
        assert!(flip.validate().is_ok());
        assert_eq!(
            (flip.stride, flip.fourcc, flip.modifier),
            (7680, FOURCC_XRGB8888, 0)
        );
    }

    /// The lesson of the paging fix: VidMm sizes a virtual transfer's window itself (it
    /// measured 0x1E10000 against a recorded 0x1C20000), so what the driver records is
    /// what VidMm is told, what the aperture check counts and what the blob mapping
    /// covers, all one page-granular number; the rest of a transfer is padding.
    #[test]
    fn vidmm_sees_the_recorded_size_and_the_padding_is_cut() {
        for (w, h) in [(1920u32, 1080u32), (1896, 1030), (5120, 1440), (1366, 768)] {
            let l = layout(w, h, 88).unwrap();
            let recorded = adopt_size(l.size, 0).unwrap();
            let vidmm = crate::round_up_page(recorded);
            assert_eq!(vidmm, recorded);
            // `validate_aperture_request`: pages of the allocation size ...
            let aperture_pages = (vidmm + 4095) >> 12;
            // ... against what `blob_map_begin` maps: the blob table's size, page-rounded.
            let map_pages = crate::round_up_page(recorded) >> 12;
            assert_eq!(aperture_pages, map_pages);
        }
        let recorded = layout(5120, 1440, 88).unwrap().size;
        assert_eq!(
            clamp_range(recorded, 0, recorded + 0x1E1_0000),
            Clamp::Clamped(recorded)
        );
        assert_eq!(clamp_range(recorded, 0, recorded), Clamp::Full(recorded));
    }

    /// The host's size rule: the declared size is checked against the dma-buf's
    /// `lseek(SEEK_END)` (larger: ERANGE; equal or smaller passes), a MAPPABLE resource
    /// needs `page_align(size) <= hostmem_len`, `SET_SCANOUT_BLOB` needs
    /// `offset + stride * height <= size`, and a size rounded up to 64 KiB passes only if
    /// the GEM import (the `mem_size` of the NVKMS import) used that same rounded size.
    /// `build_steps` takes ONE `size` from `adopt_size` and hands it to the GEM import, to
    /// the foreign request validation, to the reservation and to the resource creation; this
    /// pins what that one number is for every answer RM can give.
    #[test]
    fn one_rounded_size_serves_the_gem_import_the_import_and_the_flip() {
        use crate::foreign_resource::{validate_request, FLAG_LAYOUT};
        const K64: u64 = 64 * 1024;
        let round64 = |n: u64| n.div_ceil(K64) * K64;
        for (w, h) in [
            (1920u32, 1080u32),
            (1896, 1030),
            (5120, 1440),
            (1366, 768),
            (3840, 2160),
        ] {
            let l = layout(w, h, 88).unwrap();
            for reported in [0, l.size, round64(l.size), round64(l.size) + PAGE] {
                let size = adopt_size(l.size, reported).unwrap();
                assert!(size >= l.size, "{w}x{h} {reported}");
                assert_eq!(
                    size % PAGE,
                    0,
                    "page-granular: MAPPABLE rounds it again, to itself"
                );
                assert_eq!(crate::round_up_page(size), size);
                let fl = foreign_layout(&l, size).expect("the picture fits");
                // The flip's `offset + stride * height <= size`.
                assert!(u64::from(fl.offset) + u64::from(fl.stride) * u64::from(fl.height) <= size);
                // The request the import is validated and reserved with is that same size.
                assert!(validate_request(1, 2, 3, FLAG_LAYOUT, size, Some(fl)).is_ok());
                // RM's own rounding to 64 KiB is adopted as it is, not shrunk to the ask.
                if reported == round64(l.size) {
                    assert_eq!(size, reported, "{w}x{h}");
                }
            }
            // RM answering less than the picture needs is refused, never papered over.
            assert_eq!(adopt_size(l.size, l.size - PAGE), Err(Why::Size));
        }
        // A size that is not a page multiple would be refused by the import request.
        let l = layout(1920, 1080, 88).unwrap();
        let fl = foreign_layout(&l, l.size).unwrap();
        assert!(validate_request(1, 2, 3, FLAG_LAYOUT, l.size + 1, Some(fl)).is_err());
    }

    // ---- the RM block ------------------------------------------------------------

    fn rd32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn rd64(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn the_params_block_is_what_crm_alloc_writes_for_sysmem() {
        let p = params(0xC1D0_0001, Cache::Cached, 8_294_400);
        assert_eq!(p.len(), 128);
        assert_eq!(rd32(&p, 0), 0xC1D0_0001, "owner");
        assert_eq!(rd32(&p, 4), 0, "NVOS32_TYPE_IMAGE");
        assert_eq!(rd32(&p, 8), 0x100, "ALIGNMENT_FORCE");
        assert_eq!(rd32(&p, 12), 0);
        assert_eq!(rd32(&p, 16), 0);
        assert_eq!(rd32(&p, 20), 0);
        assert_eq!(rd32(&p, 24), 0x3a00_0000, "PCI | CACHED | ALLOW_NONCONTIG");
        assert_eq!(rd32(&p, 28), 1, "PREFER_NO_ZBC");
        assert_eq!(rd64(&p, 64), 8_294_400);
        assert_eq!(rd64(&p, 72), 4096);
        assert!(p[80..].iter().all(|b| *b == 0));
        let w = params(1, Cache::WriteCombine, 4096);
        assert_eq!(
            rd32(&w, 24),
            0x5a00_0000,
            "PCI | WRITE_COMBINE | ALLOW_NONCONTIG"
        );
        assert_eq!(reply_size(&w), 4096);
        assert_eq!(reply_size(&w[..70]), 0);
    }

    // ---- the cache attribute -------------------------------------------------------

    #[test]
    fn the_primary_is_write_combined_by_default_and_has_no_alias() {
        // The default, the absent knob and the explicit 1 are the same thing.
        for v in [0, 1] {
            let d = PrimaryCache::from_knob(v);
            assert_eq!(d, PrimaryCache::WriteCombine);
            assert_eq!(d, PrimaryCache::default());
            assert_eq!(d.sysmem(), Cache::WriteCombine);
            assert!(
                !d.cached_flag(),
                "the default never asks dxgkrnl for Cached"
            );
            assert_eq!(d.dxgkrnl_view(true), Cache::WriteCombine);
            assert_eq!(d.dxgkrnl_view(false), Cache::WriteCombine);
            assert!(
                !d.aliases(true),
                "write-combined memory, write-combined view"
            );
            assert!(
                !d.aliases(false),
                "the AllocCached kill switch changes nothing"
            );
        }
        // An unknown value is the default, never an alias.
        for v in [4, 5, 77, u32::MAX] {
            let d = PrimaryCache::from_knob(v);
            assert_eq!(d, PrimaryCache::WriteCombine);
            assert!(!d.aliases(true));
        }
    }

    #[test]
    fn the_cached_variants_are_explicit_opt_ins() {
        // 2: cached memory and the Cached flag: no alias if dxgkrnl takes it.
        let flag = PrimaryCache::from_knob(2);
        assert_eq!(flag, PrimaryCache::CachedFlag);
        assert_eq!(flag.sysmem(), Cache::Cached);
        assert!(flag.cached_flag());
        assert_eq!(flag.dxgkrnl_view(true), Cache::Cached);
        assert!(!flag.aliases(true));
        // The AllocCached kill switch takes the flag away again: now it IS an alias.
        assert_eq!(flag.dxgkrnl_view(false), Cache::WriteCombine);
        assert!(flag.aliases(false));
        // 3: cached memory under dxgkrnl's write-combined view: the named alias.
        let alias = PrimaryCache::from_knob(3);
        assert_eq!(alias, PrimaryCache::CachedAlias);
        assert_eq!(alias.sysmem(), Cache::Cached);
        assert!(!alias.cached_flag());
        assert_eq!(alias.dxgkrnl_view(true), Cache::WriteCombine);
        assert!(alias.aliases(true), "cached memory under dxgkrnl's WC view");
        assert!(alias.aliases(false));
    }

    /// Whatever the knob says, the memory is aliased only by an opt-in value.
    #[test]
    fn only_the_opt_in_values_can_alias() {
        for v in 0..=8u32 {
            for alloc_cached in [false, true] {
                let aliases = PrimaryCache::from_knob(v).aliases(alloc_cached);
                let opted_in = v == 3 || (v == 2 && !alloc_cached);
                assert_eq!(aliases, opted_in, "knob {v} alloc_cached {alloc_cached}");
            }
        }
    }

    /// The request the default sends is write-combined system memory, decoded the way the
    /// host decodes what RM writes back (`RmPlacement`: location in bits 26:25, coherency in
    /// 31:29): PCI, coherency 2 = write-combine. RM answers `0x4a800000` for it (and
    /// `0x2a800000` for the cached request: the same transformation, bit 28 cleared and bit
    /// 23 set).
    #[test]
    fn the_default_request_asks_rm_for_write_combined_system_memory() {
        let want = PrimaryCache::default().sysmem();
        let p = params(7, want, 4096);
        let attr = rd32(&p, 24);
        assert_eq!(attr, 0x5a00_0000);
        assert_eq!(attr, ATTR_WC);
        assert_eq!(
            (attr >> 25) & 3,
            1,
            "NVOS32_ATTR_LOCATION_PCI: system memory"
        );
        assert_eq!(attr >> 29, 2, "NVOS32_ATTR_COHERENCY_WRITE_COMBINE");
        // What RM writes back: the host's contract values.
        let written_back = |a: u32| (a & !0x1000_0000) | 0x0080_0000;
        assert_eq!(written_back(ATTR_WC), 0x4a80_0000);
        assert_eq!(written_back(ATTR_CACHED), 0x2a80_0000);
        assert_eq!(written_back(ATTR_WC) >> 29, 2);
        assert_eq!(written_back(ATTR_CACHED) >> 29, 1);
        // The cached request is only ever made by an opt-in value.
        for v in [2, 3] {
            let c = PrimaryCache::from_knob(v).sysmem();
            assert_eq!(rd32(&params(7, c, 4096), 24), ATTR_CACHED);
        }
    }

    #[test]
    fn a_host_that_reports_another_attribute_than_the_memory_was_made_with_is_refused() {
        assert!(host_cache_ok(Cache::Cached, MAP_CACHE_CACHED));
        assert!(host_cache_ok(Cache::WriteCombine, MAP_CACHE_WC));
        assert!(!host_cache_ok(Cache::Cached, MAP_CACHE_WC));
        assert!(!host_cache_ok(Cache::Cached, MAP_CACHE_UNCACHED));
        assert!(!host_cache_ok(Cache::WriteCombine, MAP_CACHE_CACHED));
        assert!(!host_cache_ok(Cache::WriteCombine, 0));
    }

    /// The trial map's check follows what the memory was made with, so it guards the
    /// DEFAULT too: a host that reports cached or uncached for the write-combined memory
    /// the default asks for is refused (Venus), for every knob value.
    #[test]
    fn the_trial_check_guards_the_default_and_every_opt_in() {
        let default = PrimaryCache::default().sysmem();
        assert!(host_cache_ok(default, MAP_CACHE_WC));
        assert!(!host_cache_ok(default, MAP_CACHE_CACHED));
        assert!(!host_cache_ok(default, MAP_CACHE_UNCACHED));
        for v in 0..=8u32 {
            let made = PrimaryCache::from_knob(v).sysmem();
            let agrees = if made == Cache::WriteCombine {
                MAP_CACHE_WC
            } else {
                MAP_CACHE_CACHED
            };
            for nibble in [0, MAP_CACHE_CACHED, MAP_CACHE_UNCACHED, MAP_CACHE_WC, 0xf] {
                assert_eq!(
                    host_cache_ok(made, nibble),
                    nibble == agrees,
                    "knob {v} nibble {nibble}"
                );
            }
        }
    }

    // ---- the service ---------------------------------------------------------------

    fn up(epoch: u64) -> Svc {
        let mut s = Svc::new();
        assert_eq!(s.admit(epoch), Admit::BringUp);
        s.bring_up_done(true);
        s
    }

    #[test]
    fn the_first_creation_brings_the_client_up_and_the_others_wait() {
        let mut s = Svc::new();
        assert_eq!(s.admit(7), Admit::BringUp);
        assert_eq!(s.admit(7), Admit::Wait);
        assert_eq!(s.phase(), Phase::BringingUp);
        s.bring_up_done(true);
        assert_eq!(s.phase(), Phase::Up);
        assert_eq!(s.admit(7), Admit::Go(0));
        assert_eq!(s.admit(7), Admit::Go(1));
        assert_eq!(Svc::handle(1), H_BASE + 1);
    }

    #[test]
    fn a_failed_bring_up_is_dead_for_the_generation_and_a_new_one_starts_cold() {
        let mut s = Svc::new();
        assert_eq!(s.admit(3), Admit::BringUp);
        s.bring_up_done(false);
        assert_eq!(s.admit(3), Admit::Refuse(Why::Dead));
        assert_eq!(s.admit(3), Admit::Refuse(Why::Dead));
        // A new transport generation: another chance.
        assert_eq!(s.admit(4), Admit::BringUp);
        // `bring_up_done` outside a bring-up changes nothing.
        let mut u = up(1);
        u.bring_up_done(false);
        assert_eq!(u.phase(), Phase::Up);
    }

    #[test]
    fn three_failed_creations_in_a_row_stop_new_allocations_but_keep_the_live_ones() {
        let mut s = up(1);
        let Admit::Go(a) = s.admit(1) else { panic!() };
        s.commit(a, 10, 100);
        assert_eq!(s.live(), 1);
        for _ in 0..2 {
            let Admit::Go(i) = s.admit(1) else { panic!() };
            s.abort(i);
        }
        assert_eq!(s.phase(), Phase::Up);
        assert_eq!(s.strikes(), 2);
        // A success clears the strikes.
        let Admit::Go(b) = s.admit(1) else { panic!() };
        s.commit(b, 11, 101);
        assert_eq!(s.strikes(), 0);
        for _ in 0..3 {
            let Admit::Go(i) = s.admit(1) else { panic!() };
            s.abort(i);
        }
        assert_eq!(s.phase(), Phase::NoNew);
        assert_eq!(s.admit(1), Admit::Refuse(Why::NoNew));
        // What lives still resolves and still releases.
        assert_eq!(s.find(10), Some((a, 100, 1)));
        let t = s.take(10).unwrap();
        assert_eq!((t.slot, t.gem, t.epoch), (a, 100, 1));
        assert_eq!(s.find(10), None, "no flip finds a slot a release took");
        assert_eq!(s.take(10), None, "a second release finds nothing");
        s.freed(t.slot);
        assert_eq!(s.live(), 1);
        // A new generation starts over.
        assert_eq!(s.admit(2), Admit::BringUp);
    }

    #[test]
    fn the_table_is_bounded_and_a_slot_is_reused_only_after_it_is_freed() {
        let mut s = up(1);
        let mut slots = std::vec::Vec::new();
        for n in 0..TABLE_CAP as u32 {
            let Admit::Go(i) = s.admit(1) else { panic!() };
            s.commit(i, 100 + n, 500 + n);
            slots.push(i);
        }
        assert_eq!(s.admit(1), Admit::Refuse(Why::TableFull));
        // Full is not a failure: no strike.
        assert_eq!(s.strikes(), 0);
        let t = s.take(105).unwrap();
        assert_eq!(
            s.admit(1),
            Admit::Refuse(Why::TableFull),
            "Closing is not free"
        );
        s.freed(t.slot);
        assert_eq!(s.admit(1), Admit::Go(t.slot));
        // Handles are distinct per slot.
        let mut h: std::vec::Vec<u32> = slots.iter().map(|&i| Svc::handle(i)).collect();
        h.sort();
        h.dedup();
        assert_eq!(h.len(), TABLE_CAP);
    }

    #[test]
    fn a_new_generation_forgets_every_slot() {
        let mut s = up(1);
        let Admit::Go(i) = s.admit(1) else { panic!() };
        s.commit(i, 5, 50);
        assert!(s.sync_epoch(2));
        assert_eq!(s.live(), 0);
        assert_eq!(s.find(5), None);
        assert_eq!(s.phase(), Phase::Cold);
        // A stale commit or free for the old generation's slot does nothing.
        s.commit(i, 5, 50);
        s.freed(i);
        assert_eq!(s.live(), 0);
        assert!(!s.sync_epoch(2));
    }

    // ---- a failed creation and the slot's RM handle ---------------------------------

    #[test]
    fn a_creation_whose_memory_may_be_live_keeps_its_slot_and_its_handle() {
        let mut s = up(1);
        let Admit::Go(a) = s.admit(1) else { panic!() };
        s.abort_leaked(a);
        assert_eq!(s.strikes(), 1, "a strike like any failed creation");
        assert_eq!(s.quarantined(), 1);
        assert_eq!(s.live(), 0, "nothing is live in it");
        // The next creation gets ANOTHER slot, hence another RM handle: the quarantined
        // one's number is still RM's.
        let Admit::Go(b) = s.admit(1) else { panic!() };
        assert_ne!(a, b);
        assert_ne!(Svc::handle(a), Svc::handle(b));
        // A cleanly undone slot, in contrast, is the first one taken again.
        s.abort(b);
        assert_eq!(s.admit(1), Admit::Go(b));
        assert_eq!(s.quarantined(), 1);
    }

    #[test]
    fn a_quarantined_slot_is_never_found_or_taken_by_a_release() {
        let mut s = up(1);
        let Admit::Go(a) = s.admit(1) else { panic!() };
        s.abort_leaked(a);
        assert_eq!(s.find(0), None);
        assert_eq!(s.take(0), None);
        // The slot is not the table's to free by a stray call either: it is Closing, and
        // the one way out is the generation's end.
        s.freed(a);
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn repeated_leaks_end_the_generations_allocations_and_a_new_generation_cleans_up() {
        let mut s = up(1);
        for _ in 0..MAX_STRIKES {
            let Admit::Go(i) = s.admit(1) else { panic!() };
            s.abort_leaked(i);
        }
        assert_eq!(s.phase(), Phase::NoNew);
        assert_eq!(s.quarantined(), MAX_STRIKES as usize);
        assert_eq!(s.admit(1), Admit::Refuse(Why::NoNew));
        // A new generation: the sweep closed the client, so the table is clean.
        assert_eq!(s.admit(2), Admit::BringUp);
        s.bring_up_done(true);
        assert_eq!(s.quarantined(), 0);
        assert_eq!(s.admit(2), Admit::Go(0));
    }

    #[test]
    fn a_success_clears_the_strikes_a_leak_made() {
        let mut s = up(1);
        let Admit::Go(a) = s.admit(1) else { panic!() };
        s.abort_leaked(a);
        let Admit::Go(b) = s.admit(1) else { panic!() };
        s.commit(b, 9, 90);
        assert_eq!(s.strikes(), 0);
        assert_eq!(s.quarantined(), 1);
    }

    #[test]
    fn aborting_a_slot_that_is_not_building_changes_nothing() {
        let mut s = up(1);
        let Admit::Go(a) = s.admit(1) else { panic!() };
        s.commit(a, 9, 90);
        s.abort_leaked(a);
        s.abort(a);
        assert_eq!(s.strikes(), 0);
        assert_eq!(s.find(9), Some((a, 90, 1)));
    }

    #[test]
    fn memory_is_taken_for_live_unless_an_answer_says_otherwise() {
        use AllocOutcome::*;
        use FreeOutcome::*;
        // Never made (refused, or never sent): no handle in use, whatever the free says.
        for f in [NotSent, Freed, NotFound, Failed] {
            assert!(!mem_may_be_live(NotMade, f));
        }
        for a in [Made, Unknown] {
            assert!(!mem_may_be_live(a, Freed));
            assert!(!mem_may_be_live(a, NotFound), "RM says it holds nothing");
            assert!(
                mem_may_be_live(a, Failed),
                "the free failed: RM may hold it"
            );
            assert!(mem_may_be_live(a, NotSent));
        }
    }

    #[test]
    fn what_a_failed_alloc_leaves_open_follows_how_it_failed() {
        use AllocOutcome::*;
        // The reply never came (timeout) or cannot be read: the host may have allocated.
        assert_eq!(AllocOutcome::after_failure(FailKind::Transport), Unknown);
        assert_eq!(AllocOutcome::after_failure(FailKind::Parse), Unknown);
        // An answer that says no, or a refusal before anything was sent.
        for k in [
            FailKind::Refused,
            FailKind::Host,
            FailKind::Rm,
            FailKind::Layout,
            FailKind::Os,
            FailKind::Busy,
        ] {
            assert_eq!(AllocOutcome::after_failure(k), NotMade);
        }
    }

    #[test]
    fn a_free_that_rm_answers_not_found_is_settled_and_any_other_failure_is_not() {
        assert_eq!(FreeOutcome::of(Ok(())), FreeOutcome::Freed);
        assert_eq!(
            FreeOutcome::of(Err(Fail::new(FailKind::Rm, NV_ERR_OBJECT_NOT_FOUND))),
            FreeOutcome::NotFound
        );
        // Another RM status (the object is busy), a host errno, a timeout: unknown.
        for f in [
            Fail::new(FailKind::Rm, 0x1f),
            Fail::new(FailKind::Host, NV_ERR_OBJECT_NOT_FOUND),
            Fail::new(FailKind::Transport, 1),
        ] {
            assert_eq!(FreeOutcome::of(Err(f)), FreeOutcome::Failed);
        }
        // A creation whose alloc timed out and whose free timed out too: quarantined.
        let a = AllocOutcome::after_failure(FailKind::Transport);
        assert!(mem_may_be_live(
            a,
            FreeOutcome::of(Err(Fail::new(FailKind::Transport, 1)))
        ));
        // ... and one whose alloc timed out but whose free is answered "not found".
        assert!(!mem_may_be_live(
            a,
            FreeOutcome::of(Err(Fail::new(FailKind::Rm, NV_ERR_OBJECT_NOT_FOUND)))
        ));
    }

    // ---- how long a creation may take -----------------------------------------------

    const CAP: u64 = 2_500;

    #[test]
    fn the_create_budget_is_six_seconds_in_all_and_no_message_outlives_it() {
        let t = 7_000_000_000u64;
        let b = create_budget(t, CAP);
        assert_eq!(CREATE_BUDGET_MS, 6_000);
        assert_eq!(
            b.call_timeout_ms(t),
            Some(CAP),
            "a message gets its own cap"
        );
        assert_eq!(b.call_timeout_ms(t + 3_500 * MS), Some(CAP));
        // What is left bounds the last ones.
        assert_eq!(b.call_timeout_ms(t + 5_000 * MS), Some(1_000));
        assert_eq!(b.call_timeout_ms(t + 5_999 * MS + 1), Some(1));
        assert!(!b.expired(t + 5_999 * MS));
        assert!(b.expired(t + 6_000 * MS));
        assert_eq!(b.call_timeout_ms(t + 6_000 * MS), None);
        assert_eq!(b.call_timeout_ms(t + 9_000 * MS), None);
    }

    /// The old wait counted 5000 sleeps of "1 ms"; a sleep lasts a timer tick (15.6 ms), so
    /// it ran for 78 s. A wait on the deadline ends within a tick of it.
    #[test]
    fn a_wait_on_the_deadline_ends_on_time_whatever_a_sleep_costs() {
        const TICK: u64 = 156_000; // 15.6 ms in 100 ns
        let t = 1_000_000u64;
        let b = create_budget(t, CAP);
        let mut now = t;
        let mut sleeps = 0u32;
        while !b.expired(now) {
            now += TICK;
            sleeps += 1;
        }
        let spent_ms = (now - t) / MS;
        assert!((6_000..6_000 + 16).contains(&spent_ms), "{spent_ms}");
        assert!(sleeps < 400, "{sleeps} sleeps, not 5000");
    }

    #[test]
    fn the_undo_has_its_own_allowance_and_a_spent_create_budget_does_not_starve_it() {
        let t = 5_000_000u64;
        let c = create_budget(t, CAP);
        let spent_at = t + 6_000 * MS;
        assert!(c.expired(spent_at));
        let u = undo_budget(spent_at, CAP);
        assert_eq!(UNDO_BUDGET_MS, 3_000);
        assert_eq!(u.call_timeout_ms(spent_at), Some(CAP));
        assert_eq!(u.call_timeout_ms(spent_at + 2_500 * MS), Some(500));
        assert_eq!(u.call_timeout_ms(spent_at + 3_000 * MS), None);
        // The worst case of a failed creation is the two allowances.
        assert_eq!(CREATE_BUDGET_MS + UNDO_BUDGET_MS, 9_000);
    }

    // ---- what the screen shows ----------------------------------------------------

    fn target(resid: u32, gem: u32) -> Target {
        let l = layout(1920, 1080, 88).unwrap();
        Target {
            resid,
            drm: 9,
            gem,
            epoch: 4,
            layout: flip_layout(&foreign_layout(&l, l.size).unwrap()),
        }
    }

    #[test]
    fn the_book_names_one_primary_and_hears_it_go() {
        let mut b = TargetBook::new();
        assert_eq!(b.set(target(1, 10)), Change::New);
        assert_eq!(b.set(target(1, 10)), Change::Same);
        assert_eq!(b.set(target(2, 11)), Change::Replaced);
        // The old primary being destroyed after Windows flipped to the new one is not
        // the shown one going.
        assert!(!b.gone(1));
        assert_eq!(b.current().unwrap().resid, 2);
        assert!(b.gone(2));
        assert!(b.current().is_none());
        assert!(!b.gone(2));
        assert!(target_ready(Some(&target(1, 10)), 4));
        assert!(!target_ready(Some(&target(1, 10)), 5));
        assert!(!target_ready(None, 4));
        assert!(!target_ready(Some(&target(1, 10)), 0));
    }

    // ---- the flip: the level 3 presenter with a ring of one ---------------------------

    const MS: u64 = 10_000;

    /// A model of the flip service against the real arbiter: registers the target, flips
    /// on edges, yields to a user source and flips again when it ends, withdraws when the
    /// target goes, gives up after three refused flips. It makes the calls the driver
    /// makes (`Act::Register` -> `resident_set`, flips -> `FlipResult`).
    struct Model {
        p: Presenter,
        arb: ForeignScanout,
        flips: std::vec::Vec<u32>,
        now: u64,
        tgt: Option<Target>,
        refuse_flips: bool,
    }

    impl Model {
        fn new() -> Self {
            Model {
                p: Presenter::new(1),
                arb: ForeignScanout::new(),
                flips: std::vec::Vec::new(),
                now: 10 * MS,
                tgt: None,
                refuse_flips: false,
            }
        }

        fn step(&mut self, frame: bool, resume: bool) -> Act {
            let (has, fg) = (
                self.arb.resident().is_some(),
                self.arb.resident_foreground(),
            );
            let i = flip_inputs(self.now, self.tgt.is_some(), has, fg, frame, resume);
            let act = self.p.decide(i);
            match act {
                Act::Register => {
                    let t = self.tgt.unwrap();
                    let ok = self
                        .arb
                        .resident_set(0xFFFF, t.drm, t.epoch, t.layout, self.now)
                        .is_ok();
                    self.p.registration(ok, self.now);
                }
                Act::Withdraw => {
                    self.arb.resident_drop();
                }
                Act::CopyFlip { slot } | Act::Reflip { slot } => {
                    let copied = matches!(act, Act::CopyFlip { .. });
                    let t = self.tgt.unwrap();
                    let r = if self.refuse_flips {
                        FlipResult::Failed
                    } else if !self.arb.resident_foreground() {
                        FlipResult::Yielded
                    } else {
                        self.flips.push(t.gem);
                        FlipResult::Shown
                    };
                    self.p.flipped(slot, copied, r, self.now);
                }
                Act::Idle | Act::WaitUntil(_) => {}
            }
            act
        }

        fn run(&mut self, frame: bool, resume: bool) {
            let mut f = frame;
            let mut r = resume;
            for _ in 0..6 {
                let a = self.step(f, r);
                f = false;
                r = false;
                if matches!(a, Act::Idle | Act::WaitUntil(_)) {
                    break;
                }
            }
        }
    }

    #[test]
    fn the_first_primary_registers_and_is_flipped_and_edges_flip_it_again_at_60hz() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        assert!(m.arb.resident_foreground());
        assert_eq!(m.flips, [10]);
        // A burst of 240 Hz edges coalesces to one flip per 16 ms.
        for _ in 0..3 {
            m.now += 4 * MS;
            m.run(true, false);
        }
        assert_eq!(m.flips.len(), 1, "inside the pacing interval");
        m.now += 10 * MS;
        m.run(false, false);
        assert_eq!(m.flips.len(), 2);
    }

    #[test]
    fn a_user_source_preempts_and_the_end_of_it_flips_the_primary_again() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        // A user source sets: the resident one is parked.
        let user = crate::foreign_scanout::Layout {
            width: 640,
            height: 480,
            stride: 2560,
            offset: 0,
            fourcc: FOURCC_XRGB8888,
            modifier: 0,
        };
        let o = m.arb.set(0xA, 5, 4, user, 2000, m.now).unwrap();
        assert_eq!(o.kind, crate::foreign_scanout::SetKind::Preempted);
        m.now += 20 * MS;
        m.run(true, false);
        assert_eq!(
            m.flips.len(),
            1,
            "no flip while a user source holds scanout 0"
        );
        // It ends: the resident source takes the screen back and a re-flip is owed.
        assert!(matches!(
            m.arb.release(0xA, Some(5)),
            crate::foreign_scanout::ReleaseOutcome::Released { .. }
        ));
        assert!(m.arb.take_resume_owed());
        m.now += 20 * MS;
        m.run(false, true);
        assert_eq!(m.flips, [10, 10]);
    }

    #[test]
    fn a_new_primary_is_shown_by_the_next_flip_without_a_new_registration() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        // Windows flips to another allocation: the book changed, the arbiter's resident
        // layout is updated in place (the driver calls resident_set), an edge is raised.
        let t2 = target(2, 11);
        m.tgt = Some(t2);
        let gen_before = m.arb.resident().unwrap().generation;
        let r = m
            .arb
            .resident_set(0xFFFF, t2.drm, t2.epoch, t2.layout, m.now)
            .unwrap();
        assert_eq!(r.kind, ResidentKind::Foreground);
        assert_eq!(r.generation, gen_before, "updated in place");
        m.now += 20 * MS;
        m.run(true, false);
        assert_eq!(m.flips, [10, 11]);
    }

    #[test]
    fn the_shown_primary_going_withdraws_the_source_and_nothing_flips_a_closed_gem() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        m.tgt = None; // `TargetBook::gone`
        m.now += 20 * MS;
        m.run(true, false);
        assert!(m.arb.resident().is_none());
        assert!(m.arb.restore_pending(), "the desktop is owed its flush");
        assert_eq!(m.flips, [10]);
        // Edges with no target flip nothing.
        m.now += 20 * MS;
        m.run(true, false);
        assert_eq!(m.flips, [10]);
    }

    #[test]
    fn three_refused_flips_give_the_screen_back_to_venus() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        m.refuse_flips = true;
        for _ in 0..4 {
            m.now += 200 * MS;
            m.run(true, false);
        }
        assert!(m.p.gave_up());
        assert!(m.arb.resident().is_none());
        assert_eq!(m.flips, [10]);
    }

    /// The flip service's restart: a presenter that gave up is reset (a fresh one registers at
    /// the very next look) and the wake-ups go on (every desktop frame edge), so the pause is
    /// the caller's gate, [`restart_pause`], and the presenter is not asked during it.
    #[test]
    fn a_presenter_that_gave_up_starts_over_only_after_five_seconds() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        m.refuse_flips = true;
        for _ in 0..4 {
            m.now += 200 * MS;
            m.run(true, false);
        }
        assert!(m.p.gave_up());
        assert!(m.arb.resident().is_none(), "withdrawn");
        // What the service does: reset the presenter, remember the restart time.
        m.p.reset();
        let restart_at = m.now + RESTART_AFTER_GIVING_UP_100NS;
        m.refuse_flips = false;
        let flips = m.flips.len();
        let mut wakes = 0u32;
        loop {
            m.now += 16 * MS;
            if restart_pause(restart_at, m.now).is_none() {
                break;
            }
            wakes += 1;
            assert_eq!(m.flips.len(), flips, "nothing is flipped during the pause");
            assert!(m.arb.resident().is_none(), "nor registered");
        }
        assert!(wakes > 300, "{wakes} frame-edge wakes were held back");
        assert!(m.now >= restart_at && m.now < restart_at + 16 * MS);
        m.run(true, false);
        assert!(m.p.registered(), "after the pause it registers again");
        assert_eq!(m.flips.len(), flips + 1, "and flips the primary");
    }

    #[test]
    fn the_restart_pause_is_a_gate_on_the_clock() {
        assert_eq!(restart_pause(0, 123), None, "none pending");
        let at = 1_000 + RESTART_AFTER_GIVING_UP_100NS;
        assert_eq!(restart_pause(at, 1_000), Some(at));
        assert_eq!(restart_pause(at, at - 1), Some(at));
        assert_eq!(restart_pause(at, at), None, "due exactly at the time");
        assert_eq!(restart_pause(at, at + 1), None);
        assert_eq!(RESTART_AFTER_GIVING_UP_100NS, 5_000 * MS);
    }

    /// Without the gate a reset presenter registers at once: what the pause is for.
    #[test]
    fn a_reset_presenter_registers_at_once_so_the_gate_is_the_callers() {
        let mut m = Model::new();
        m.tgt = Some(target(1, 10));
        m.run(true, false);
        m.p.reset();
        assert_eq!(m.step(true, false), Act::Register);
    }

    // ---- the release seam ---------------------------------------------------------------

    use crate::scanout_release::{ReleaseBook, RING_WAIT_100NS};

    /// Even the real release answers could not hold a one-surface ring's flip: the surface
    /// to write is the one on screen, which the host never releases.
    #[test]
    fn a_ring_of_one_never_waits_for_a_release_that_cannot_come() {
        let mut p = Presenter::new(1);
        let mut t = 10 * MS;
        let first = Inputs {
            now: t,
            ring_ready: true,
            source_ok: true,
            arbiter_has_resident: false,
            foreground: true,
            frame_edge: true,
            resume_edge: false,
            // The values a ring client would pass: releases tracked, nothing released.
            release_tracked: true,
            back_released: false,
        };
        assert_eq!(p.decide(first), Act::Register);
        p.registration(true, t);
        let mut i = Inputs {
            arbiter_has_resident: true,
            ..first
        };
        assert_eq!(p.decide(i), Act::CopyFlip { slot: 0 });
        p.flipped(0, true, FlipResult::Shown, t);
        p.note_seq(0, 5);
        assert_eq!(
            p.back_wait_seq(),
            None,
            "the buffer to write is the one shown"
        );
        // Frame after frame, a re-flip of the same buffer is never held, at any time.
        for k in 1..50u64 {
            t += 20 * MS + k;
            i.now = t;
            i.frame_edge = true;
            assert_eq!(p.decide(i), Act::CopyFlip { slot: 0 }, "frame {k}");
            p.flipped(0, true, FlipResult::Shown, t);
            p.note_seq(0, 5 + k);
        }
        // And the inputs the driver actually passes never ask.
        let d = flip_inputs(t, true, true, true, true, false);
        assert!(!d.release_tracked && d.back_released);
        assert!(d.ring_ready && d.source_ok);
    }

    #[test]
    fn a_reflipped_buffer_is_never_something_a_close_waits_for() {
        let mut log = FlipLog::new();
        log.flipped(10, 1, 100);
        // The same buffer again and again: the current one, never released while shown.
        for k in 2..20u64 {
            log.flipped(10, k, 100 + k);
            assert_eq!(log.closing(10), CloseWait::Free);
        }
        // Whatever the book says (the book never releases the buffer on the scanout), the
        // gate lets a free close through at once and at any time.
        for released in [false, true] {
            assert_eq!(close_gate(CloseWait::Free, released, 0), Wait::Go);
            assert_eq!(close_gate(CloseWait::Free, released, u64::MAX), Wait::Go);
        }
        assert_eq!(log.closing(99), CloseWait::Free, "never flipped");
    }

    #[test]
    fn a_replaced_primary_is_waited_for_until_the_host_releases_it_or_the_limit() {
        let mut log = FlipLog::new();
        log.flipped(10, 1, 100);
        log.flipped(10, 2, 200); // re-flip
        log.flipped(11, 3, 1_000); // a new primary replaces it
        assert_eq!(log.closing(11), CloseWait::Free, "the shown one");
        let w = log.closing(10);
        assert_eq!(w, CloseWait::Replaced { seq: 2, at: 1_000 });
        let until = 1_000 + RING_WAIT_100NS;
        // Not released: held until the limit, counted from the replacing flip.
        assert_eq!(close_gate(w, false, 1_001), Wait::Hold { until });
        assert_eq!(close_gate(w, false, until - 1), Wait::Hold { until });
        // The limit passes: the close goes ahead (the same overrule as the host's).
        assert_eq!(close_gate(w, false, until), Wait::TimedOut);
        assert_eq!(close_gate(w, false, until + 5 * MS), Wait::TimedOut);
        // Released: go.
        assert_eq!(close_gate(w, true, 1_001), Wait::Go);
    }

    #[test]
    fn the_log_follows_a_buffer_that_comes_back_and_forgets_a_closed_one() {
        let mut log = FlipLog::new();
        log.flipped(10, 1, 100);
        log.flipped(11, 2, 200);
        assert!(matches!(log.closing(10), CloseWait::Replaced { .. }));
        // 10 is shown again: it is current, not replaced; 11 is the replaced one now.
        log.flipped(10, 3, 300);
        assert_eq!(log.closing(10), CloseWait::Free);
        assert_eq!(log.closing(11), CloseWait::Replaced { seq: 2, at: 300 });
        log.forget(11);
        assert_eq!(log.closing(11), CloseWait::Free);
        log.forget(10);
        log.flipped(12, 4, 400); // nothing current: nothing replaced
        assert_eq!(log.closing(10), CloseWait::Free);
        log.clear();
        assert_eq!(log, FlipLog::new());
    }

    /// The release book's own account of the same facts: the buffer on the scanout is
    /// never done (so nothing may wait for it), a replaced one is once the host says so.
    #[test]
    fn the_book_agrees_with_the_log_on_what_can_be_waited_for() {
        const DRM: u32 = 5;
        const KMD: usize = usize::MAX;
        let mut b = ReleaseBook::new();
        let mut log = FlipLog::new();
        let mut seq = 0u64;
        let mut flip = |b: &mut ReleaseBook, log: &mut FlipLog, gem: u32, t: u64| {
            seq += 1;
            b.minted(seq, KMD, DRM, gem);
            assert!(b.sent(seq, t).is_some());
            log.flipped(gem, seq, t);
            seq
        };
        flip(&mut b, &mut log, 10, 100);
        let s2 = flip(&mut b, &mut log, 10, 200);
        // The current buffer: the book says "not done" for ever, the log says "do not wait".
        assert!(!b.is_done(s2, 10 * RING_WAIT_100NS));
        assert_eq!(log.closing(10), CloseWait::Free);
        // A new primary replaces it: now the host's release is expected.
        let s3 = flip(&mut b, &mut log, 11, 1_000);
        let CloseWait::Replaced { seq: waits_for, at } = log.closing(10) else {
            panic!("replaced");
        };
        assert_eq!((waits_for, at), (s2, 1_000));
        assert!(!b.is_done(s2, 1_001));
        assert_eq!(
            close_gate(log.closing(10), b.is_done(s2, 1_001), 1_001),
            Wait::Hold {
                until: 1_000 + RING_WAIT_100NS
            }
        );
        // The host's ScanoutReleased(owner handle, gem, newest seq of that gem).
        b.released(DRM, 10, s2);
        assert_eq!(
            close_gate(log.closing(10), b.is_done(s2, 1_002), 1_002),
            Wait::Go
        );
        // The new current buffer is still never done.
        assert!(!b.is_done(s3, 1_002));
        assert_eq!(log.closing(11), CloseWait::Free);
    }

    /// The re-flip refresh sends the SAME buffer's GEM again and again (up to the mode's
    /// rate). The book must not leak entries, must not evict a live one, must keep the
    /// floor of the source monotone and never above the live flip, and a replaced primary
    /// must still be released by the host's one event.
    #[test]
    fn hammering_one_buffer_with_re_flips_leaks_nothing_and_never_moves_the_floor_wrongly() {
        use crate::scanout_release::BOOK_SLOTS;
        const DRM: u32 = 5;
        const KMD: usize = usize::MAX;
        let mut b = ReleaseBook::new();
        let mut log = FlipLog::new();
        let mut t = 1_000u64;
        let mut seq = 0u64;
        let mut floors = std::vec::Vec::new();
        for k in 0..20_000u64 {
            seq += 1;
            t += 41_667; // 240 Hz
            let m = b.minted(seq, KMD, DRM, 10);
            assert!(!m.evicted_live, "re-flip {k}: a live entry was overwritten");
            // Between mint and the host's reply the previous flip is still the live one.
            let (f_mid, last_mid) = b.floor(DRM, t);
            assert!(f_mid < last_mid, "the latest flip is live: floor < last");
            let sent = b.sent(seq, t).expect("known");
            if k > 0 {
                assert_eq!(
                    sent.superseded, 1,
                    "the older flip of the same buffer is done"
                );
            }
            log.flipped(10, seq, t);
            let (floor, last) = b.floor(DRM, t);
            assert_eq!(last, seq);
            assert_eq!(
                floor,
                seq - 1,
                "everything older is done, the shown one never is"
            );
            floors.push(floor);
            assert!(b.len() <= BOOK_SLOTS);
            assert!(
                !b.is_done(seq, t + 100 * MS),
                "the buffer on the scanout is never released"
            );
            if k > 0 {
                assert!(b.is_done(seq - 1, t), "a superseded flip is done");
            }
            assert_eq!(log.closing(10), CloseWait::Free);
        }
        assert!(
            floors.windows(2).all(|w| w[0] < w[1]),
            "the floor only rises"
        );
        assert_eq!(b.len(), BOOK_SLOTS, "a full book, recycled, not grown");
        // A mode change: another primary replaces it; the host releases the old one once.
        seq += 1;
        t += 41_667;
        b.minted(seq, KMD, DRM, 11);
        b.sent(seq, t).unwrap();
        log.flipped(11, seq, t);
        let CloseWait::Replaced { seq: waits_for, .. } = log.closing(10) else {
            panic!("replaced")
        };
        assert_eq!(waits_for, seq - 1);
        assert!(!b.is_done(waits_for, t + 1));
        assert!(matches!(
            b.released(DRM, 10, waits_for),
            crate::scanout_release::Release::Matched { .. }
        ));
        assert!(b.is_done(waits_for, t + 2));
        // A second release for the same buffer (a late duplicate) changes nothing.
        assert!(matches!(
            b.released(DRM, 10, waits_for),
            crate::scanout_release::Release::Matched { newly: 0, .. }
        ));
        // The new primary is the live one now.
        let (floor, last) = b.floor(DRM, t + 3);
        assert_eq!((floor, last), (waits_for, seq));
        // A flip the host refused is retired, not left live.
        seq += 1;
        b.minted(seq, KMD, DRM, 11);
        assert_eq!(b.gone(seq), Some(KMD));
        let (floor, _) = b.floor(DRM, t + 4);
        assert_eq!(
            floor,
            seq - 2,
            "the shown flip (still live) holds the floor below itself"
        );
    }
}
