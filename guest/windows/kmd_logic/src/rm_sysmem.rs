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
use crate::rm_client::{MAX_SURFACE_BYTES, MEM_ALLOC_BYTES};

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
/// finding), so the primary's own CPU view is write-combined by default. The host
/// measured write-combined access to this memory at 28 MB/s (writes) and 75 MB/s
/// (reads) against 28 GB/s for cached reads, so the default is cached memory and the
/// alias with dxgkrnl's view is the first thing the hardware run checks (15.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryCache {
    /// 0 (default): cached system memory; dxgkrnl's view stays write-combined.
    Cached,
    /// 1: write-combined system memory: every view agrees with dxgkrnl's, no alias, and
    /// reads and writes are slow. The kill switch for the alias.
    WriteCombine,
    /// 2: cached system memory AND the `Cached` flag on the primary, so dxgkrnl maps it
    /// write-back too. An experiment: dxgkrnl may refuse the allocation.
    CachedFlag,
}

impl PrimaryCache {
    pub const fn from_knob(v: u32) -> Self {
        match v {
            1 => PrimaryCache::WriteCombine,
            2 => PrimaryCache::CachedFlag,
            _ => PrimaryCache::Cached,
        }
    }

    /// What RM is asked for.
    pub const fn sysmem(self) -> Cache {
        match self {
            PrimaryCache::WriteCombine => Cache::WriteCombine,
            _ => Cache::Cached,
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
    /// of the same pages). Counted (`RmSysAlias`), never a refusal: it is the default.
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
        if let Some(s) = self.slots.get_mut(slot) {
            if s.state == SlotState::Building {
                *s = Slot::FREE;
                self.strikes = self.strikes.saturating_add(1);
                if self.strikes >= MAX_STRIKES && self.phase == Phase::Up {
                    self.phase = Phase::NoNew;
                }
            }
        }
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
    fn the_primary_is_cached_by_default_and_the_alias_is_named() {
        let d = PrimaryCache::from_knob(0);
        assert_eq!(d, PrimaryCache::Cached);
        assert_eq!(d.sysmem(), Cache::Cached);
        assert!(!d.cached_flag());
        assert_eq!(d.dxgkrnl_view(true), Cache::WriteCombine);
        assert!(d.aliases(true), "cached memory under dxgkrnl's WC view");
        let wc = PrimaryCache::from_knob(1);
        assert_eq!(wc.sysmem(), Cache::WriteCombine);
        assert!(!wc.aliases(true));
        assert!(!wc.aliases(false));
        let flag = PrimaryCache::from_knob(2);
        assert!(flag.cached_flag());
        assert_eq!(flag.dxgkrnl_view(true), Cache::Cached);
        assert!(!flag.aliases(true));
        // The AllocCached kill switch takes the flag away again.
        assert_eq!(flag.dxgkrnl_view(false), Cache::WriteCombine);
        assert!(flag.aliases(false));
        assert_eq!(PrimaryCache::from_knob(77), PrimaryCache::Cached);
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
            let i = Inputs {
                now: self.now,
                ring_ready: self.tgt.is_some(),
                source_ok: self.tgt.is_some(),
                arbiter_has_resident: has,
                foreground: fg,
                frame_edge: frame,
                resume_edge: resume,
            };
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
}
