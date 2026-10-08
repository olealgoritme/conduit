//! RM VIDEO memory behind the KMD's own GPU-only allocations (`RedirVram`, stages V2-V5 of
//! `docs/vram-redirection.md`): the pure half. The I/O halves are
//! `kmd_render/src/virtio/rm_client/vidmem.rs` (the allocation service) and
//! `kmd_render/src/virtio/rm_client/ce_vram.rs` (the objects in the copy-engine channel: GPU
//! mapping, VRAM-to-VRAM copies, the bounce transfers for CPU readers and writers).
//!
//! Three reusable pieces, shared with the copy-only GDI acceleration (fallback A):
//!
//! 1. **The allocation** ([`route`], [`layout`], [`adopt`], [`foreign_layout`]): a pitch-linear
//!    32 bpp surface of `NV01_MEMORY_LOCAL_USER` (the parameters of the KMD's ring surfaces,
//!    `rm_client::mem_alloc_params`), exported, imported as a GEM, created as an `RM_EXPORT`
//!    resource of the KMD's own owner (no `USE_MAPPABLE`: video memory is never CPU-mapped
//!    through the Venus window), adopted by the WDDM allocation as a foreign resource with a
//!    LINEAR layout. The service state is [`crate::rm_sysmem::Svc`] (the same bounded table,
//!    bring-up and strikes) in its own RM client, so its handles never meet level 5's.
//! 2. **The objects in the channel** ([`MapBook`]): each live VRAM object, dup'd into the CE
//!    channel's client and GPU-mapped there once, keyed by RESOURCE ID (never by RM handle: the
//!    service reuses a slot's handle after a free, a resource id is never reused while the
//!    adapter lives), with fixed handles and 64 MiB VA windows outside every other namespace of
//!    that client ([`map_handles`], [`map_va`]).
//! 3. **The copies** ([`vram_copy`], [`bounce_copy`]): pitch-linear CE copies between two
//!    mapped surfaces, and between a mapped surface and the bounce buffer (RM system memory of
//!    the channel's client, CPU-mapped) for the CPU side.
//!
//! Nothing here does I/O, reads a clock or takes a lock.

use crate::ce_present::{CopyRect, Remap, SurfaceLayout, MAX_VA};
use crate::foreign_resource::{Layout as FrLayout, MAX_FOREIGN_RESOURCE_BYTES, MOD_LINEAR};
use crate::rm_ce_channel as cc;
use crate::rm_client::{self as rc, SurfaceLayout as RcLayout};
use crate::rm_sysmem::{fourcc_for_dxgi, Kind};

// ---- the knob ---------------------------------------------------------------------------------

/// `RedirVram` 0 (default): nothing here runs; every entry point is one relaxed load.
pub const KNOB_OFF: u32 = 0;
/// `RedirVram` 1: GPU-only GDI surfaces (`D3DKMDT_GDISURFACE_TEXTURE`, the redirection surface
/// Windows hands out with `NonCpuVisiblePrimary`) from RM video memory; the redirected Blt into
/// them on the copy engine (with `RmCopyEngine` 1); CPU readers and writers through the bounce.
pub const KNOB_ON: u32 = 1;

/// `RvOff` (default 0): per-path switches for bisecting `RedirVram` on hardware (a set bit turns
/// that path OFF, the caller then takes its fallback), plus one opt-in.
pub mod off {
    /// `ce_vram::foreign_source` refuses (both the record and the import path).
    pub const FOREIGN: u32 = 0x1;
    /// Its import-by-resource-id path refuses (records still serve).
    pub const FOREIGN_IMPORT: u32 = 0x2;
    /// `ce_sysmem::with_standard_pair` refuses.
    pub const PAIR: u32 = 0x8;
    /// `ce_sysmem::with_standard` (and the pair) refuse: GDI staging copies go to the CPU.
    pub const SYSMEM: u32 = 0x10;
    /// GDI staging buffers stay Venus blobs (no RM system memory, `sysmem::try_create_standard`).
    pub const STAGING_RM: u32 = 0x40;
    /// GDI lookup tables (`D3DKMDT_GDISURFACE_LOOKUPTABLE`) stay Venus blobs; the staging
    /// buffers still come from RM system memory unless `STAGING_RM` is set too.
    pub const LUT_RM: u32 = 0x400;
    /// A new VRAM surface is NOT cleared at its first copy-engine mapping (A/B for the clear).
    pub const CLEAR: u32 = 0x800;
    /// An NVK frame into a VRAM window surface takes the synchronous copy in the Present instead
    /// of the copy-engine route (queued, ordered after the producer's frame by its semaphore,
    /// the Present completing on the copy). The route is the default since 384.1 (429 fps against
    /// the synchronous copy's 434, every frame routed); it was opt-in under the same bit before.
    pub const ROUTE_OFF: u32 = 0x1000;
    /// DIAGNOSTIC: a new VRAM surface is cleared to opaque magenta instead of 0, so content no
    /// path ever wrote shows as magenta wherever DWM composes the surface.
    pub const CLEAR_MAGENTA: u32 = 0x2000;
    /// The Present hook (`ddi/vram_redirect.rs`) skips every Blt with a VRAM surface (counted).
    pub const PRESENT_HOOK: u32 = 0x80;
    /// OPT-IN: the CPU helpers reuse blob views (`build_paging_buffer`). Off by default since 364.1:
    /// a view kept after its blob left that window range is a cached alias of whatever the host maps
    /// there next.
    pub const CPU_VIEWS_ON: u32 = 0x200;
    /// OPT-IN, not a switch-off: a foreign copy from a record ACQUIREs the producer's semaphore.
    /// Off by default since 364.1 (a record's semaphore value is not guaranteed to be released
    /// again, e.g. a recreated swap chain, and an acquire that never releases holds the channel).
    pub const FOREIGN_ACQUIRE_ON: u32 = 0x100;
}

/// Whether a KMD standard allocation is made from RM system memory (`sysmem::try_create_standard`)
/// rather than a Venus blob: a CPU-visible GDI staging buffer, and the GDI lookup table CDD uploads
/// from one (the ClearType gamma table): 370.1 `GdiSysCpuT` 0x00440142, a STAGING_CPUVISIBLE to
/// LOOKUPTABLE copy that `with_standard_pair` refused because the table was a Venus blob, so it ran
/// on the CPU (10-17 ms). Neither is ever opened by a UMD (DWM composes the GDI TEXTURE). Both keep
/// the authored pitch, `cross_adapter_pitch(width)` whatever the format, which is the RM layout's.
/// `off` is the `RvOff` mask.
pub const fn rm_backed_standard(std_type: u32, gdi_type: u32, primary: bool, off: u32) -> bool {
    use crate::rm_standard::{GDI_LOOKUPTABLE, GDI_STAGING_CPUVISIBLE, STD_GDISURFACE};
    if primary || std_type != STD_GDISURFACE || off & off::STAGING_RM != 0 {
        return false;
    }
    gdi_type == GDI_STAGING_CPUVISIBLE || (gdi_type == GDI_LOOKUPTABLE && off & off::LUT_RM == 0)
}

/// The knob as read from the service key: anything but 1 is off.
pub const fn knob_on(v: u32) -> bool {
    v == KNOB_ON
}

/// Why an allocation was not made from RM video memory. `RvWhy`; never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Why {
    KnobOff = 1,
    /// Not a GPU-only KMD surface (a CPU-visible standard buffer, the primary, a UMD resource).
    NotGpuOnly = 2,
    NoTransport = 3,
    NoContext = 4,
    Format = 5,
    Extent = 6,
    Size = 7,
    Dead = 8,
    NoNew = 9,
    BringUpBusy = 10,
    TableFull = 11,
    BringUp = 12,
    /// `RM_ALLOC`, export, GEM import, or the close of the export file.
    Alloc = 13,
    /// The `RM_EXPORT` resource (the foreign import) failed.
    Import = 14,
    Adopt = 15,
    Slow = 16,
    /// The live surfaces would pass [`BUDGET_BYTES`].
    Budget = 17,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }
    /// The level 5 service's reason, for the shared admission machine.
    pub const fn from_sysmem(w: crate::rm_sysmem::Why) -> Why {
        use crate::rm_sysmem::Why as S;
        match w {
            S::Dead => Why::Dead,
            S::NoNew => Why::NoNew,
            S::BringUpBusy => Why::BringUpBusy,
            S::TableFull => Why::TableFull,
            S::BringUp => Why::BringUp,
            S::Slow => Why::Slow,
            S::NoTransport => Why::NoTransport,
            S::NoContext => Why::NoContext,
            _ => Why::Alloc,
        }
    }
}

/// Which KMD allocation kinds come from video memory with the knob on: only the GPU-only GDI
/// texture. A CPU-visible standard buffer is never video memory (its CPU view would be a
/// write-combined BAR read at ~200 MB/s, `docs/kmd-rm-client.md` 5.2).
pub fn route(knob: u32, kind: Kind) -> Result<(), Why> {
    if !knob_on(knob) {
        return Err(Why::KnobOff);
    }
    match kind {
        Kind::OptimalGdiTexture => Ok(()),
        _ => Err(Why::NotGpuOnly),
    }
}

/// Video memory the service keeps alive at most (all live surfaces together). A 5120x1440
/// window is 29 MiB; 1 GiB is ~35 of those, far below any host's `--vram-limit-mib`.
pub const BUDGET_BYTES: u64 = 1 << 30;

/// Whether one more surface of `size` fits next to `live_bytes`.
pub const fn within_budget(live_bytes: u64, size: u64) -> bool {
    match live_bytes.checked_add(size) {
        Some(t) => t <= BUDGET_BYTES,
        None => false,
    }
}

// ---- geometry -----------------------------------------------------------------------------------

/// A video-memory surface: pitch-linear, 32 bpp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VidLayout {
    pub surface: RcLayout,
    pub fourcc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    Extent,
    Format,
}

impl LayoutError {
    pub const fn why(self) -> Why {
        match self {
            LayoutError::Extent => Why::Extent,
            LayoutError::Format => Why::Format,
        }
    }
}

/// The layout of a `width` x `height` surface of DXGI format `dxgi` (28, 87, 88): the ring
/// surfaces' geometry (pitch rounded to 256, size to 64 KiB), at most `MAX_SURFACE_BYTES`.
///
/// The extents are the foreign record's (`foreign_resource::MIN_DIM`..=`MAX_DIM`, 1..=16384), NOT
/// the scanout's (`rm_client::surface_layout` starts at 64): GDI textures of small windows,
/// tooltips and the cursor are below 64 in a dimension (357.1: `RvWhy` 6 for 8 of 19 requests).
pub fn layout(width: u32, height: u32, dxgi: u32) -> Result<VidLayout, LayoutError> {
    use crate::foreign_resource::{MAX_DIM, MIN_DIM};
    let fourcc = fourcc_for_dxgi(dxgi).ok_or(LayoutError::Format)?;
    if !(MIN_DIM..=MAX_DIM).contains(&width) || !(MIN_DIM..=MAX_DIM).contains(&height) {
        return Err(LayoutError::Extent);
    }
    let row = u64::from(width) * u64::from(rc::BYTES_PER_PIXEL);
    let align = u64::from(rc::PITCH_ALIGN);
    let pitch = row.div_ceil(align) * align;
    if pitch > u64::from(crate::foreign_scanout::MAX_STRIDE) {
        return Err(LayoutError::Extent);
    }
    let size = (pitch * u64::from(height)).div_ceil(rc::SIZE_ALIGN) * rc::SIZE_ALIGN;
    if size > rc::MAX_SURFACE_BYTES || size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(LayoutError::Extent);
    }
    Ok(VidLayout {
        surface: RcLayout {
            width,
            height,
            pitch: pitch as u32,
            size,
        },
        fourcc,
    })
}

/// What RM made (it may round pitch and size up), or why it cannot be used.
pub fn adopt(want: &VidLayout, nested: &[u8]) -> Result<VidLayout, Why> {
    let surface = rc::adopt_alloc_reply(&want.surface, nested).map_err(|_| Why::Size)?;
    if surface.size > MAX_FOREIGN_RESOURCE_BYTES || surface.size % 4096 != 0 {
        return Err(Why::Size);
    }
    Ok(VidLayout {
        surface,
        fourcc: want.fourcc,
    })
}

/// The layout the foreign record and every importer (DWM's NVK) are told.
pub fn foreign_layout(l: &VidLayout) -> Option<FrLayout> {
    let fl = FrLayout {
        width: l.surface.width,
        height: l.surface.height,
        stride: l.surface.pitch,
        offset: 0,
        fourcc: l.fourcc,
        modifier: MOD_LINEAR,
        plane1: None,
    };
    fl.validate_for(l.surface.size).ok().map(|()| fl)
}

// ---- the objects in the channel's client --------------------------------------------------------

/// Objects mapped in the channel at once: the KMD's VRAM surfaces, the RM staging buffers and the
/// NVK images imported by resource id (`ce_vram::foreign_source`), one LRU. 16 thrashed once every
/// window of the NVK desktop was a VRAM surface fed by an NVK swap chain: 377.1 imported again on
/// almost every Present (`RvSyFs` 827 us of the 940 us a Present took). 48.
pub const MAP_SLOTS: usize = 48;
/// Handles of map slot `i`: the dup, then its `NV50_MEMORY_VIRTUAL`. Above the system views'
/// (`H_SYS_BASE` + 16), the route's destination handles (`ce_route::H_DST_BASE` + 16) and the dup
/// cache's (`H_DUP_BASE` + 32).
pub const H_MAP_BASE: u32 = cc::H_BASE + 0x200;
/// The bounce buffer (RM system memory of the channel's client) and its virtual allocation.
pub const H_BOUNCE: u32 = cc::H_BASE + 0x0F0;
pub const H_BOUNCE_VIRT: u32 = cc::H_BASE + 0x0F1;

pub const fn map_handles(slot: u8) -> (u32, u32) {
    let h = H_MAP_BASE + 2 * slot as u32;
    (h, h + 1)
}

/// The GPU window of map slot `i`: 64 MiB windows from 80 windows above the channel's base (the
/// route's destinations use 32..40, the dup cache 4..20, the bounce 44, the system views 64..72).
pub const MAP_VA_BASE: u64 = cc::VA_BASE + 80 * cc::VA_WINDOW;
pub const fn map_va(slot: u8) -> u64 {
    MAP_VA_BASE + slot as u64 * cc::VA_WINDOW
}
/// The bounce buffer's window (one 64 MiB window, 44 windows above the base).
pub const BOUNCE_VA: u64 = cc::VA_BASE + 44 * cc::VA_WINDOW;
/// The largest surface a window maps, and the largest bounce transfer.
pub const MAX_MAP_BYTES: u64 = cc::VA_WINDOW;

const _: () = assert!(map_va(MAP_SLOTS as u8 - 1) + cc::VA_WINDOW <= MAX_VA);
const _: () = assert!(BOUNCE_VA + cc::VA_WINDOW <= MAP_VA_BASE);
const _: () = assert!(H_BOUNCE_VIRT < H_MAP_BASE);

/// CE views of CPU-visible standard buffers (OS descriptors over their system pages, `ce_sysmem`).
pub const SYS_SLOTS: usize = 8;
/// Handles of system view slot `i`: the OS descriptor, then its `NV50_MEMORY_VIRTUAL`.
pub const H_SYS_BASE: u32 = cc::H_BASE + 0x140;
pub const fn sys_handles(slot: u8) -> (u32, u32) {
    let h = H_SYS_BASE + 2 * slot as u32;
    (h, h + 1)
}
/// The GPU window of system view slot `i`: 64 MiB windows from 64 windows above the base.
pub const SYS_VA_BASE: u64 = cc::VA_BASE + 64 * cc::VA_WINDOW;
pub const fn sys_va(slot: u8) -> u64 {
    SYS_VA_BASE + slot as u64 * cc::VA_WINDOW
}
const _: () = assert!(sys_va(SYS_SLOTS as u8 - 1) + cc::VA_WINDOW <= MAX_VA);
const _: () = assert!(sys_va(SYS_SLOTS as u8 - 1) + cc::VA_WINDOW <= MAP_VA_BASE);
const _: () = assert!(H_SYS_BASE + 2 * SYS_SLOTS as u32 <= H_MAP_BASE);
const _: () = assert!(MAP_SLOTS <= u8::MAX as usize);

/// One mapped object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub resid: u32,
    pub slot: u8,
    pub va: u64,
    pub len: u64,
    /// The object is gone (its allocation was destroyed): the slot must be given back before it
    /// is used again, and is never a hit.
    pub stale: bool,
    used: u64,
}

/// What [`MapBook::plan`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapPlan {
    Hit(Mapped),
    /// Dup and map into `slot`; `evict` (a stale or the least recently used entry) is given back
    /// first.
    Make { slot: u8, evict: Option<Mapped> },
}

/// The bounded table of VRAM objects mapped in the channel's client.
#[derive(Debug, Clone, Copy)]
pub struct MapBook {
    slots: [Option<Mapped>; MAP_SLOTS],
    tick: u64,
}

impl Default for MapBook {
    fn default() -> Self {
        Self::new()
    }
}

impl MapBook {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAP_SLOTS],
            tick: 0,
        }
    }

    /// The slot for `resid`: its live mapping (a hit), else a free slot, a stale one, or the least
    /// recently used.
    pub fn plan(&mut self, resid: u32) -> MapPlan {
        self.tick += 1;
        let now = self.tick;
        let mut free = None;
        let mut stale = None;
        let mut oldest: Option<(usize, u64)> = None;
        for (i, s) in self.slots.iter_mut().enumerate() {
            match s {
                Some(m) if m.resid == resid && !m.stale => {
                    m.used = now;
                    return MapPlan::Hit(*m);
                }
                Some(m) if m.stale => {
                    if stale.is_none() {
                        stale = Some(i);
                    }
                }
                Some(m) => {
                    if oldest.map_or(true, |(_, u)| m.used < u) {
                        oldest = Some((i, m.used));
                    }
                }
                None => {
                    if free.is_none() {
                        free = Some(i);
                    }
                }
            }
        }
        let i = free.or(stale).or(oldest.map(|(i, _)| i)).unwrap_or(0);
        MapPlan::Make {
            slot: i as u8,
            evict: self.slots[i],
        }
    }

    pub fn insert(&mut self, slot: u8, resid: u32, va: u64, len: u64) {
        self.tick += 1;
        if let Some(s) = self.slots.get_mut(slot as usize) {
            *s = Some(Mapped {
                resid,
                slot,
                va,
                len,
                stale: false,
                used: self.tick,
            });
        }
    }

    pub fn remove(&mut self, slot: u8) -> Option<Mapped> {
        self.slots.get_mut(slot as usize).and_then(Option::take)
    }

    /// The live mapping of `resid` (no RM call, not marked used).
    pub fn find(&self, resid: u32) -> Option<Mapped> {
        self.slots
            .iter()
            .flatten()
            .find(|m| m.resid == resid && !m.stale)
            .copied()
    }

    /// `resid`'s allocation is gone: its mapping is never a hit again. Whether one existed.
    pub fn mark_stale(&mut self, resid: u32) -> bool {
        let mut any = false;
        for m in self.slots.iter_mut().flatten() {
            if m.resid == resid {
                m.stale = true;
                any = true;
            }
        }
        any
    }

    /// Whether a stale entry waits to be given back.
    pub fn has_stale(&self) -> bool {
        self.slots.iter().flatten().any(|m| m.stale)
    }

    /// A stale entry, taken out (to be given back).
    pub fn take_stale(&mut self) -> Option<Mapped> {
        let i = self.slots.iter().position(|s| s.is_some_and(|m| m.stale))?;
        self.slots[i].take()
    }

    /// Any entry, taken out (teardown).
    pub fn take_any(&mut self) -> Option<Mapped> {
        let i = self.slots.iter().position(Option::is_some)?;
        self.slots[i].take()
    }

    pub fn live(&self) -> u32 {
        self.slots.iter().flatten().filter(|m| !m.stale).count() as u32
    }

    pub fn clear(&mut self) {
        self.slots = [None; MAP_SLOTS];
    }
}

/// The map flags of a video-memory surface in the channel: big pages, the memory's own (pitch)
/// kind; RM's refusal of big pages retries with the system-memory flags (as `ce_dup::map_tries`
/// does for a pitch-linear source).
pub const fn map_flags_first() -> u32 {
    cc::MAP_FLAGS_PAGE_SIZE_BIG
}
pub const fn map_flags_second() -> u32 {
    cc::MAP_FLAGS_SYSMEM
}

/// Bytes to map of an object of `size`: whole pages, at most one window.
pub const fn map_len(size: u64) -> Option<u64> {
    if size == 0 || size > MAX_MAP_BYTES || size % 4096 != 0 {
        None
    } else {
        Some(size)
    }
}

// ---- the copies -----------------------------------------------------------------------------------

/// One pitch-linear surface in the channel's VA space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub va: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
}

/// A rectangle in pixels, `[left, right) x [top, bottom)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rect {
    pub const fn whole(width: u32, height: u32) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }
    const fn valid_in(&self, width: u32, height: u32) -> bool {
        self.left < self.right && self.top < self.bottom && self.right <= width && self.bottom <= height
    }
}

/// Why a copy cannot be planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    Rect,
    Pitch,
    Va,
    TooBig,
}

const BPP: u64 = 4;

fn origin(s: &Surface, x: u32, y: u32) -> Option<u64> {
    s.va
        .checked_add(u64::from(y) * u64::from(s.pitch))?
        .checked_add(u64::from(x) * BPP)
}

/// A 32 bpp pitch-linear copy of `src_rect` of `src` to `(dst_x, dst_y)` of `dst`, no remap (the
/// caller passes the remap when the formats differ). Both rectangles must lie inside their
/// surfaces and every address below [`MAX_VA`].
pub fn vram_copy(
    src: &Surface,
    src_rect: Rect,
    dst: &Surface,
    dst_x: u32,
    dst_y: u32,
    remap: Remap,
) -> Result<CopyRect, CopyError> {
    if !src_rect.valid_in(src.width, src.height) {
        return Err(CopyError::Rect);
    }
    let w = src_rect.right - src_rect.left;
    let h = src_rect.bottom - src_rect.top;
    let dst_rect = Rect {
        left: dst_x,
        top: dst_y,
        right: dst_x.checked_add(w).ok_or(CopyError::Rect)?,
        bottom: dst_y.checked_add(h).ok_or(CopyError::Rect)?,
    };
    if !dst_rect.valid_in(dst.width, dst.height) {
        return Err(CopyError::Rect);
    }
    let line = u64::from(w) * BPP;
    if line > u64::from(src.pitch) || line > u64::from(dst.pitch) {
        return Err(CopyError::Pitch);
    }
    let s = origin(src, src_rect.left, src_rect.top).ok_or(CopyError::Va)?;
    let d = origin(dst, dst_x, dst_y).ok_or(CopyError::Va)?;
    let s_end = s + (u64::from(h) - 1) * u64::from(src.pitch) + line;
    let d_end = d + (u64::from(h) - 1) * u64::from(dst.pitch) + line;
    if s_end > MAX_VA || d_end > MAX_VA {
        return Err(CopyError::Va);
    }
    Ok(CopyRect {
        src_va: s,
        dst_va: d,
        src_pitch: src.pitch,
        dst_pitch: dst.pitch,
        line_bytes: line as u32,
        lines: h,
        layout: SurfaceLayout::Pitch,
        dst_layout: SurfaceLayout::Pitch,
        remap,
        stamp: None,
    })
}

/// A copy out of an NVK-made (foreign) image into a pitch-linear surface: `src_rect` of the image
/// (whose base is at `src_va` in the channel, laid out as `plan` says) to `(dst_x, dst_y)` of `dst`.
/// Any rectangle: pitch-linear by address, block-linear by the copy's source origin.
#[allow(clippy::too_many_arguments)]
pub fn foreign_copy(
    plan: &crate::ce_present::SourcePlan,
    src_va: u64,
    src_pitch: u32,
    src_width: u32,
    src_height: u32,
    src_rect: Rect,
    dst: &Surface,
    dst_x: u32,
    dst_y: u32,
    remap: Remap,
) -> Result<CopyRect, CopyError> {
    if !src_rect.valid_in(src_width, src_height) {
        return Err(CopyError::Rect);
    }
    match plan.layout {
        SurfaceLayout::Pitch => {
            let src = Surface {
                va: src_va.checked_add(plan.offset).ok_or(CopyError::Va)?,
                pitch: src_pitch,
                width: src_width,
                height: src_height,
            };
            vram_copy(&src, src_rect, dst, dst_x, dst_y, remap)
        }
        SurfaceLayout::BlockLinear {
            block_height_log2,
            element_bytes,
            image_height,
            ..
        } => {
            let w = src_rect.right - src_rect.left;
            let h = src_rect.bottom - src_rect.top;
            let dst_rect = Rect {
                left: dst_x,
                top: dst_y,
                right: dst_x.checked_add(w).ok_or(CopyError::Rect)?,
                bottom: dst_y.checked_add(h).ok_or(CopyError::Rect)?,
            };
            if !dst_rect.valid_in(dst.width, dst.height) {
                return Err(CopyError::Rect);
            }
            let line = u64::from(w) * BPP;
            if line > u64::from(dst.pitch) {
                return Err(CopyError::Pitch);
            }
            let d = origin(dst, dst_x, dst_y).ok_or(CopyError::Va)?;
            let d_end = d + (u64::from(h) - 1) * u64::from(dst.pitch) + line;
            if d_end > MAX_VA || src_va >= MAX_VA {
                return Err(CopyError::Va);
            }
            Ok(CopyRect {
                src_va,
                dst_va: d,
                src_pitch,
                dst_pitch: dst.pitch,
                line_bytes: line as u32,
                lines: h,
                // The origin selects the rectangle inside the block-linear image.
                layout: SurfaceLayout::BlockLinear {
                    block_height_log2,
                    element_bytes,
                    image_height,
                    origin_x_bytes: src_rect.left * BPP as u32,
                    origin_y: src_rect.top,
                },
                dst_layout: SurfaceLayout::Pitch,
                remap,
                stamp: None,
            })
        }
    }
}

// ---- importing an NVK image into the channel's client (no producer record) ---------------------

/// `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD`.
pub const CTRL_IMPORT_OBJECT_FROM_FD: u32 = 0x3d06;
/// `DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY`: `DRM_IOWR(0x40 + 0x09, 24)`.
pub const DRM_IOCTL_GEM_EXPORT_NVKMS: u32 = 0xC018_6449;
pub const GEM_EXPORT_BYTES: usize = 24;
pub const IMPORT_FROM_FD_BYTES: usize = 20;

/// `drm_nvidia_gem_export_nvkms_memory_params` of GEM `handle`: the NVKMS block (4 bytes, the
/// control file's handle; the host replaces the pointer) follows as the nested block.
pub fn gem_export_params(handle: u32) -> [u8; GEM_EXPORT_BYTES] {
    let mut a = [0u8; GEM_EXPORT_BYTES];
    a[0..4].copy_from_slice(&handle.to_le_bytes());
    a[16..24].copy_from_slice(&4u64.to_le_bytes());
    a
}

/// `NV0000_CTRL_OS_UNIX_IMPORT_OBJECT_FROM_FD_PARAMS`: the memory exported to the control file
/// `fd` becomes `h_object` under `h_device` (also the parent) of the caller's client.
pub fn import_from_fd_params(fd: u32, h_device: u32, h_object: u32) -> [u8; IMPORT_FROM_FD_BYTES] {
    let mut a = [0u8; IMPORT_FROM_FD_BYTES];
    a[0..4].copy_from_slice(&fd.to_le_bytes());
    a[4..8].copy_from_slice(&rc::EXPORT_OBJECT_TYPE_RM.to_le_bytes());
    a[8..12].copy_from_slice(&h_device.to_le_bytes());
    a[12..16].copy_from_slice(&h_device.to_le_bytes());
    a[16..20].copy_from_slice(&h_object.to_le_bytes());
    a
}

/// A copy INTO an NVK-made (foreign) image (a GDI BitBlt whose destination is an app's image):
/// `src_rect` of the pitch-linear `src` to `(dst_x, dst_y)` of the image whose base is `dst_va`,
/// laid out as `plan` says. Pitch-linear destination by address, block-linear by the copy's
/// destination origin.
#[allow(clippy::too_many_arguments)]
pub fn foreign_write(
    src: &Surface,
    src_rect: Rect,
    plan: &crate::ce_present::SourcePlan,
    dst_va: u64,
    dst_pitch: u32,
    dst_width: u32,
    dst_height: u32,
    dst_x: u32,
    dst_y: u32,
    remap: Remap,
) -> Result<CopyRect, CopyError> {
    if !src_rect.valid_in(src.width, src.height) {
        return Err(CopyError::Rect);
    }
    match plan.layout {
        SurfaceLayout::Pitch => {
            let dst = Surface {
                va: dst_va.checked_add(plan.offset).ok_or(CopyError::Va)?,
                pitch: dst_pitch,
                width: dst_width,
                height: dst_height,
            };
            vram_copy(src, src_rect, &dst, dst_x, dst_y, remap)
        }
        SurfaceLayout::BlockLinear {
            block_height_log2,
            element_bytes,
            image_height,
            ..
        } => {
            let w = src_rect.right - src_rect.left;
            let h = src_rect.bottom - src_rect.top;
            let dst_rect = Rect {
                left: dst_x,
                top: dst_y,
                right: dst_x.checked_add(w).ok_or(CopyError::Rect)?,
                bottom: dst_y.checked_add(h).ok_or(CopyError::Rect)?,
            };
            if !dst_rect.valid_in(dst_width, dst_height) {
                return Err(CopyError::Rect);
            }
            let line = u64::from(w) * BPP;
            if line > u64::from(src.pitch) {
                return Err(CopyError::Pitch);
            }
            let s = origin(src, src_rect.left, src_rect.top).ok_or(CopyError::Va)?;
            let s_end = s + (u64::from(h) - 1) * u64::from(src.pitch) + line;
            if s_end > MAX_VA || dst_va >= MAX_VA {
                return Err(CopyError::Va);
            }
            Ok(CopyRect {
                src_va: s,
                dst_va,
                src_pitch: src.pitch,
                dst_pitch,
                line_bytes: line as u32,
                lines: h,
                layout: SurfaceLayout::Pitch,
                dst_layout: SurfaceLayout::BlockLinear {
                    block_height_log2,
                    element_bytes,
                    image_height,
                    origin_x_bytes: dst_x * BPP as u32,
                    origin_y: dst_y,
                },
                remap,
                stamp: None,
            })
        }
    }
}

/// `SET_REMAP_COMPONENTS` of a clear: every destination component `CONST_A`, 4-byte components,
/// one source and one destination component per element (one 32-bit element per pixel; Mesa NVK's
/// `nvk_cmd_fill_memory_ce` fields).
pub const CLEAR_COMPONENTS: u32 = 4 | (4 << 4) | (4 << 8) | (4 << 12) | (3 << 16);

/// The push of a clear of `[va, va + pitch * lines)` to the 32-bit `value` (multi-line, pitch-linear,
/// remap from `CONST_A`): `SET_REMAP_CONST_A/B, COMPONENTS`, `OFFSET_IN_UPPER..LINE_COUNT`,
/// `LAUNCH_DMA`. The caller appends its release. `LINE_LENGTH_IN` is in 4-byte elements.
pub fn clear(
    p: &mut crate::ce_present::Push<'_>,
    va: u64,
    pitch: u32,
    lines: u32,
    value: u32,
) -> Result<(), crate::ce_present::PushError> {
    use crate::ce_present as cp;
    if pitch == 0 || pitch % 4 != 0 || lines == 0 {
        return Err(cp::PushError::Shape);
    }
    let end = va
        .checked_add(u64::from(pitch) * u64::from(lines))
        .ok_or(cp::PushError::Va)?;
    if end > MAX_VA {
        return Err(cp::PushError::Va);
    }
    p.method(cp::SUBC_CE, cp::CE_SET_REMAP_CONST_A, &[value, value, CLEAR_COMPONENTS])?;
    p.method(
        cp::SUBC_CE,
        cp::CE_OFFSET_IN_UPPER,
        &[(va >> 32) as u32, va as u32, (va >> 32) as u32, va as u32, pitch, pitch, pitch / 4, lines],
    )?;
    p.method(
        cp::SUBC_CE,
        cp::CE_LAUNCH_DMA,
        &[cp::LAUNCH_TRANSFER_NON_PIPELINED
            | cp::LAUNCH_FLUSH_ENABLE
            | cp::LAUNCH_SRC_PITCH
            | cp::LAUNCH_DST_PITCH
            | cp::LAUNCH_MULTI_LINE
            | cp::LAUNCH_REMAP_ENABLE],
    )
}

/// Direction of a bounce transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// CPU bytes -> bounce -> the VRAM surface (a GDI writer: CDD's staging into the texture).
    Upload,
    /// The VRAM surface -> bounce -> CPU bytes (a reader: PrintWindow, capture, a staging copy).
    Readback,
}

/// The bounce buffer's layout for `rect`: tightly packed rows (`width * 4`), at most one window.
pub fn bounce_surface(rect: Rect) -> Result<Surface, CopyError> {
    if rect.left >= rect.right || rect.top >= rect.bottom {
        return Err(CopyError::Rect);
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    let pitch = u64::from(w) * BPP;
    let bytes = pitch * u64::from(h);
    if bytes > MAX_MAP_BYTES || pitch > u64::from(u32::MAX) {
        return Err(CopyError::TooBig);
    }
    Ok(Surface {
        va: BOUNCE_VA,
        pitch: pitch as u32,
        width: w,
        height: h,
    })
}

/// Bytes the bounce buffer must hold for `rect`.
pub fn bounce_bytes(rect: Rect) -> Result<u64, CopyError> {
    let s = bounce_surface(rect)?;
    Ok(crate::round_up_page(u64::from(s.pitch) * u64::from(s.height)))
}

/// The CE copy of a bounce transfer of `rect` of `vram`.
pub fn bounce_copy(vram: &Surface, rect: Rect, dir: Dir) -> Result<CopyRect, CopyError> {
    let b = bounce_surface(rect)?;
    match dir {
        Dir::Readback => vram_copy(vram, rect, &b, 0, 0, Remap::None),
        Dir::Upload => vram_copy(&b, Rect::whole(b.width, b.height), vram, rect.left, rect.top, Remap::None),
    }
}

/// The bounce copy of a rectangle of a foreign (NVK-made) image, block-linear or pitch-linear by
/// its `plan`: `Readback` copies `rect` of the image into the packed bounce rows, `Upload` the
/// bounce rows into `rect` of the image. The pixel layout is the image's (no R/B exchange): the
/// CPU works on the image's own format.
#[allow(clippy::too_many_arguments)]
pub fn foreign_bounce_copy(
    plan: &crate::ce_present::SourcePlan,
    va: u64,
    pitch: u32,
    width: u32,
    height: u32,
    rect: Rect,
    dir: Dir,
) -> Result<CopyRect, CopyError> {
    let b = bounce_surface(rect)?;
    match dir {
        Dir::Readback => foreign_copy(plan, va, pitch, width, height, rect, &b, 0, 0, Remap::None),
        Dir::Upload => foreign_write(
            &b,
            Rect::whole(b.width, b.height),
            plan,
            va,
            pitch,
            width,
            height,
            rect.left,
            rect.top,
            Remap::None,
        ),
    }
}

// ---- counters ---------------------------------------------------------------------------------------

/// Every value the two I/O halves write (at most 14 characters each; the name scan of the
/// driver checks the list).
pub const COUNTERS: &[&str] = &[
    // the service (`vidmem.rs`)
    "RvKnob", "RvOffEff", "RvWaitTmo", "RvCleared", "RvClrFail", "RvClrSkip", "RvClrLate", "RvTry", "RvOk", "RvVenus", "RvWhy", "RvStage", "RvFail", "RvState", "RvLive",
    "RvBytes", "RvFreed", "RvBring", "RvMs", "RvMsMax", "RvSoft", "RvLeak", "RvOpen", "RvOpenFg",
    "RvOpenLay", "RvOpenPid", "RvOpenNoRm", "RvOpenNoRmPid",
    // the channel side (`ce_vram.rs`)
    "RvMapOk", "RvMapFail", "RvMapStat", "RvMapLive", "RvMapGive", "RvBookReent", "RvXfer", "RvXferFail",
    "RvXferWhy", "RvXferUs", "RvXferMax", "RvCopy", "RvCopyFail",
    // the Present (`ddi/vram_redirect.rs`)
    "RvBltSeen", "RvBltRoute", "RvBltSkip", "RvBltWhy", "RvRdBack", "RvUpload", "RvGdiFail", "RvBltSync", "RvRtWhy", "RvSyncTry", "RvSyncWhy", "RvMkNone", "RvMkRes", "RvMkStr", "RvMkVramNo", "RvSyPix", "RvSyPixN", "RvSyPixNz", "RvDst0", "RvDst1", "RvDst2", "RvDst3", "RvDst4", "RvDst5", "RvDst6", "RvDst7", "RvDstMore", "RvPgXfer", "RvPgFill", "RvPgDisc", "RvPgOther", "RvPgLast", "RvNew0", "RvNew1", "RvNew2", "RvNew3", "RvNew4", "RvNew5", "RvNew6", "RvNew7", "RvNewWH0", "RvNewWH1", "RvNewWH2", "RvNewWH3", "RvNewWH4", "RvNewWH5", "RvNewWH6", "RvNewWH7", "RvNewOp0", "RvNewOp1", "RvNewOp2", "RvNewOp3", "RvNewOp4", "RvNewOp5", "RvNewOp6", "RvNewOp7", "RvNewPid0", "RvNewPid1", "RvNewPid2", "RvNewPid3", "RvNewPid4", "RvNewPid5", "RvNewPid6", "RvNewPid7", "RvNewGone", "RvSyDst", "RvSySrc", "RvSyWH", "RvPrUs", "RvPrN", "RvPrMax", "RvRtUs", "RvRtN", "RvRtMax", "RvSyFsUs", "RvSyFsN", "RvSyFsMax", "RvSyDsUs", "RvSyDsN", "RvSyDsMax", "RvSySubUs", "RvSySubN", "RvSySubMax", "RvSyWtUs", "RvSyWtN", "RvSyWtMax",
    // foreign NVK sources for GDI commands (`ce_vram.rs`)
    "RvFgnRec", "RvFgnImp", "RvFgnFail", "RvFgnWhy", "RvFgnWrite",
    // the CE views of standard buffers (`ddi/ce_sysmem.rs`)
    "RvSysMade", "RvSysHit", "RvSysRefuse", "RvSysWhy", "RvSysFreed", "RvSysLeak", "RvSysObj",
    // the CPU helpers' blob views (`ddi/build_paging_buffer.rs`)
    "RvCpuMapUs", "RvCpuCpyUs", "RvCpuKB", "RvCpuCache", "RvCpuHit", "RvCpuView",
];

/// The files that write [`COUNTERS`] (relative to `kmd_render/src`).
pub const WRITERS: [&str; 5] = [
    "virtio/rm_client/vidmem.rs",
    "virtio/rm_client/ce_vram.rs",
    "ddi/vram_redirect.rs",
    "ddi/ce_sysmem.rs",
    "ddi/build_paging_buffer.rs",
];

#[cfg(test)]
mod tests {
    #[test]
    fn staging_and_lookup_tables_are_rm_backed() {
        use super::{off, rm_backed_standard};
        use crate::rm_standard::*;
        assert!(rm_backed_standard(STD_GDISURFACE, GDI_STAGING_CPUVISIBLE, false, 0));
        assert!(rm_backed_standard(STD_GDISURFACE, GDI_LOOKUPTABLE, false, 0));
        assert!(!rm_backed_standard(STD_GDISURFACE, GDI_LOOKUPTABLE, false, off::LUT_RM));
        assert!(rm_backed_standard(STD_GDISURFACE, GDI_STAGING_CPUVISIBLE, false, off::LUT_RM));
        for g in [GDI_STAGING_CPUVISIBLE, GDI_LOOKUPTABLE] {
            assert!(!rm_backed_standard(STD_GDISURFACE, g, false, off::STAGING_RM));
            assert!(!rm_backed_standard(STD_GDISURFACE, g, true, 0));
        }
        for g in [GDI_INVALID, GDI_TEXTURE, GDI_STAGING, GDI_EXISTINGSYSMEM, GDI_TEXTURE_CPUVISIBLE, 9] {
            assert!(!rm_backed_standard(STD_GDISURFACE, g, false, 0), "gdi {g}");
        }
        for s in [STD_SHAREDPRIMARYSURFACE, STD_SHADOWSURFACE, STD_STAGINGSURFACE] {
            assert!(!rm_backed_standard(s, GDI_STAGING_CPUVISIBLE, false, 0));
            assert!(!rm_backed_standard(s, GDI_LOOKUPTABLE, false, 0));
        }
    }

    #[test]
    fn a_lookup_table_of_any_format_gets_the_authored_pitch() {
        // CDD's gamma table is 8 bpp; the KMD authors every non-TEXTURE GDI surface with
        // `cross_adapter_pitch(width)` (32 bpp), and the RM layout must equal it.
        for dxgi in [0u32, 61, 62, 65, 87, 88] {
            let l = crate::rm_sysmem::layout_standard(512, 16, dxgi).unwrap();
            assert_eq!(l.pitch, crate::cross_adapter_pitch(512), "dxgi {dxgi}");
            assert!(l.size >= u64::from(l.pitch) * 16);
        }
    }

    use super::*;
    use crate::foreign_resource::FOURCC_ARGB8888;

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        crate::ce_dup::scan::exact_list(COUNTERS, &WRITERS, "Rv");
    }

    #[test]
    fn only_the_gpu_only_texture_routes() {
        assert_eq!(route(0, Kind::OptimalGdiTexture), Err(Why::KnobOff));
        assert_eq!(route(2, Kind::OptimalGdiTexture), Err(Why::KnobOff));
        assert_eq!(route(1, Kind::OptimalGdiTexture), Ok(()));
        assert_eq!(route(1, Kind::LinearPrimary), Err(Why::NotGpuOnly));
        assert_eq!(route(1, Kind::StandardBuffer { primary: false }), Err(Why::NotGpuOnly));
        assert_eq!(route(1, Kind::StandardBuffer { primary: true }), Err(Why::NotGpuOnly));
        assert_eq!(route(1, Kind::Other), Err(Why::NotGpuOnly));
    }

    #[test]
    fn layout_of_a_heaven_window() {
        let l = layout(1600, 900, 87).unwrap();
        assert_eq!(l.surface.pitch, 6400);
        assert_eq!(l.surface.size, (6400u64 * 900).div_ceil(65536) * 65536);
        assert_eq!(l.fourcc, FOURCC_ARGB8888);
        let fl = foreign_layout(&l).unwrap();
        assert_eq!(fl.stride, 6400);
        assert_eq!(fl.modifier, MOD_LINEAR);
        // 800 px: the 256-aligned pitch, carried in the record (the importer uses the stride).
        assert_eq!(layout(800, 600, 87).unwrap().surface.pitch, 3328);
        assert_eq!(layout(1600, 900, 0), Err(LayoutError::Format));
        assert_eq!(layout(0, 900, 87), Err(LayoutError::Extent));
        // Small GDI textures (tooltips, the cursor, narrow windows) are VRAM too (357.1 RvWhy 6).
        let t = layout(32, 32, 87).unwrap();
        assert_eq!((t.surface.pitch, t.surface.size), (256, 65536));
        assert!(foreign_layout(&t).is_some());
        let n = layout(1, 1, 88).unwrap();
        assert_eq!((n.surface.pitch, n.surface.size), (256, 65536));
        assert!(foreign_layout(&n).is_some());
        // The same answer as the ring surfaces' rule where both apply.
        assert_eq!(layout(1600, 900, 87).unwrap().surface, rc::surface_layout(1600, 900).unwrap());
        assert_eq!(layout(16385, 8, 87), Err(LayoutError::Extent));
    }

    #[test]
    fn adopt_takes_rm_rounding_and_refuses_too_small() {
        let want = layout(1600, 900, 87).unwrap();
        let mut nested = [0u8; 128];
        nested[20..24].copy_from_slice(&6656u32.to_le_bytes());
        let size = 6656u64 * 900;
        let size = size.div_ceil(65536) * 65536;
        nested[64..72].copy_from_slice(&size.to_le_bytes());
        let got = adopt(&want, &nested).unwrap();
        assert_eq!(got.surface.pitch, 6656);
        assert_eq!(got.surface.size, size);
        nested[64..72].copy_from_slice(&4096u64.to_le_bytes());
        assert_eq!(adopt(&want, &nested), Err(Why::Size));
    }

    #[test]
    fn budget() {
        assert!(within_budget(0, 6 << 20));
        assert!(within_budget(BUDGET_BYTES - 4096, 4096));
        assert!(!within_budget(BUDGET_BYTES - 4096, 8192));
        assert!(!within_budget(u64::MAX, 1));
    }

    #[test]
    fn handles_and_windows_stay_out_of_the_other_namespaces() {
        let (d, v) = map_handles(MAP_SLOTS as u8 - 1);
        assert!(d > crate::ce_route::H_DST_BASE + 2 * crate::ce_route::MAX_DSTS as u32);
        assert_eq!(v, d + 1);
        assert!(H_BOUNCE > crate::ce_route::H_DST_BASE + 2 * crate::ce_route::MAX_DSTS as u32);
        assert!(H_BOUNCE > cc::H_DUP_BASE + 32);
        // windows: dup cache 4..20, route destinations 32..40, bounce 44, system views 64..72,
        // maps 80..128
        assert!(BOUNCE_VA >= crate::ce_route::dst_va(crate::ce_route::MAX_DSTS as u8 - 1) + cc::VA_WINDOW);
        assert!(crate::ce_dup::slot_va(15) + cc::VA_WINDOW <= crate::ce_route::DST_VA_BASE);
        assert_eq!(map_va(1) - map_va(0), cc::VA_WINDOW);
        assert!(map_va(0) >= sys_va(SYS_SLOTS as u8 - 1) + cc::VA_WINDOW);
        let (s_last, _) = sys_handles(SYS_SLOTS as u8 - 1);
        assert!(map_handles(0).0 > s_last + 1);
    }

    #[test]
    fn map_book_hits_evicts_and_never_aliases_a_gone_object() {
        let mut b = MapBook::new();
        let MapPlan::Make { slot, evict: None } = b.plan(10) else { panic!() };
        b.insert(slot, 10, map_va(slot), 1 << 20);
        assert!(matches!(b.plan(10), MapPlan::Hit(m) if m.va == map_va(slot)));
        assert!(b.mark_stale(10));
        // a stale entry is never a hit; a new object takes a free slot first
        let MapPlan::Make { slot: s2, evict: None } = b.plan(10) else { panic!() };
        assert_ne!(s2, slot);
        assert_eq!(b.find(10), None);
        assert_eq!(b.take_stale().map(|m| m.slot), Some(slot));
        // fill every slot, then the least recently used goes
        let mut b = MapBook::new();
        for r in 0..MAP_SLOTS as u32 {
            let MapPlan::Make { slot, evict: None } = b.plan(100 + r) else { panic!() };
            b.insert(slot, 100 + r, map_va(slot), 4096);
        }
        assert!(matches!(b.plan(100), MapPlan::Hit(_)));
        let MapPlan::Make { evict: Some(e), .. } = b.plan(999) else { panic!() };
        assert_eq!(e.resid, 101);
        assert_eq!(b.live(), MAP_SLOTS as u32);
        // a stale slot goes before the least recently used live one
        b.mark_stale(105);
        let MapPlan::Make { evict: Some(e), .. } = b.plan(998) else { panic!() };
        assert_eq!(e.resid, 105);
    }

    #[test]
    fn vram_copy_whole_and_sub_rect() {
        let src = Surface { va: map_va(0), pitch: 6400, width: 1600, height: 900 };
        let dst = Surface { va: map_va(1), pitch: 6656, width: 1600, height: 900 };
        let c = vram_copy(&src, Rect::whole(1600, 900), &dst, 0, 0, Remap::None).unwrap();
        assert_eq!((c.src_va, c.dst_va), (src.va, dst.va));
        assert_eq!((c.line_bytes, c.lines, c.src_pitch, c.dst_pitch), (6400, 900, 6400, 6656));
        let r = Rect { left: 10, top: 20, right: 110, bottom: 70 };
        let c = vram_copy(&src, r, &dst, 5, 6, Remap::None).unwrap();
        assert_eq!(c.src_va, src.va + 20 * 6400 + 40);
        assert_eq!(c.dst_va, dst.va + 6 * 6656 + 20);
        assert_eq!((c.line_bytes, c.lines), (400, 50));
        assert_eq!(
            vram_copy(&src, Rect { left: 0, top: 0, right: 1601, bottom: 1 }, &dst, 0, 0, Remap::None),
            Err(CopyError::Rect)
        );
        assert_eq!(vram_copy(&src, r, &dst, 1550, 0, Remap::None), Err(CopyError::Rect));
        let narrow = Surface { pitch: 100, ..dst };
        assert_eq!(vram_copy(&src, r, &narrow, 0, 0, Remap::None), Err(CopyError::Pitch));
        let high = Surface { va: MAX_VA - 4096, ..src };
        assert_eq!(vram_copy(&high, r, &dst, 0, 0, Remap::None), Err(CopyError::Va));
    }

    #[test]
    fn foreign_copies_pitch_sub_rects_and_whole_block_linear_images() {
        use crate::ce_present::SourcePlan;
        let dst = Surface { va: map_va(3), pitch: 7680, width: 1920, height: 1080 };
        let pitch_plan = SourcePlan { layout: SurfaceLayout::Pitch, page_kind: None, line_bytes: 1908 * 4, offset: 256 };
        let r = Rect { left: 8, top: 2, right: 108, bottom: 12 };
        let c = foreign_copy(&pitch_plan, map_va(4), 7680, 1908, 910, r, &dst, 10, 20, Remap::SwapRb).unwrap();
        assert_eq!(c.src_va, map_va(4) + 256 + 2 * 7680 + 32);
        assert_eq!(c.dst_va, dst.va + 20 * 7680 + 40);
        assert_eq!((c.line_bytes, c.lines), (400, 10));
        let bl = SurfaceLayout::BlockLinear { block_height_log2: 4, element_bytes: 4, image_height: 910, origin_x_bytes: 0, origin_y: 0 };
        let bl_plan = SourcePlan { layout: bl, page_kind: Some(6), line_bytes: 1908 * 4, offset: 0 };
        let w = Rect::whole(1908, 910);
        let c = foreign_copy(&bl_plan, map_va(4), 7680, 1908, 910, w, &dst, 0, 0, Remap::None).unwrap();
        assert_eq!((c.src_va, c.dst_va, c.lines, c.line_bytes), (map_va(4), dst.va, 910, 7632));
        assert_eq!(c.layout, bl);
        let c = foreign_copy(&bl_plan, map_va(4), 7680, 1908, 910, r, &dst, 5, 6, Remap::None).unwrap();
        assert_eq!(c.src_va, map_va(4));
        assert_eq!(c.dst_va, dst.va + 6 * 7680 + 20);
        assert_eq!((c.line_bytes, c.lines), (400, 10));
        assert!(matches!(c.layout, SurfaceLayout::BlockLinear { origin_x_bytes: 32, origin_y: 2, image_height: 910, .. }));
        assert_eq!(foreign_copy(&bl_plan, map_va(4), 7680, 1908, 910, w, &dst, 100, 0, Remap::None), Err(CopyError::Rect));
        assert_eq!(
            foreign_copy(&bl_plan, map_va(4), 7680, 1908, 910, Rect { left: 0, top: 0, right: 1909, bottom: 1 }, &dst, 0, 0, Remap::None),
            Err(CopyError::Rect)
        );
    }

    #[test]
    fn a_foreign_rect_bounces_both_ways_through_the_packed_rows() {
        use crate::ce_present::SourcePlan;
        let plan = SourcePlan { layout: SurfaceLayout::Pitch, page_kind: None, line_bytes: 1908 * 4, offset: 0 };
        let r = Rect { left: 10, top: 20, right: 110, bottom: 70 };
        let rb = foreign_bounce_copy(&plan, map_va(4), 7680, 1908, 910, r, Dir::Readback).unwrap();
        assert_eq!(rb.dst_va, BOUNCE_VA);
        assert_eq!(rb.dst_pitch, 100 * 4);
        assert_eq!(rb.lines, 50);
        let up = foreign_bounce_copy(&plan, map_va(4), 7680, 1908, 910, r, Dir::Upload).unwrap();
        assert_eq!(up.src_va, BOUNCE_VA);
        assert_eq!(up.src_pitch, 100 * 4);
        assert_eq!(up.lines, 50);
        let outside = Rect { left: 1900, top: 0, right: 1910, bottom: 1 };
        assert!(foreign_bounce_copy(&plan, map_va(4), 7680, 1908, 910, outside, Dir::Readback).is_err());
    }

    #[test]
    fn foreign_writes_pitch_by_address_block_linear_by_origin() {
        use crate::ce_present::SourcePlan;
        let src = Surface { va: map_va(3), pitch: 6400, width: 1600, height: 900 };
        let r = Rect { left: 10, top: 20, right: 110, bottom: 70 };
        let pitch_plan = SourcePlan { layout: SurfaceLayout::Pitch, page_kind: None, line_bytes: 7632, offset: 0 };
        let c = foreign_write(&src, r, &pitch_plan, map_va(4), 7680, 1908, 910, 5, 6, Remap::SwapRb).unwrap();
        assert_eq!(c.src_va, src.va + 20 * 6400 + 40);
        assert_eq!(c.dst_va, map_va(4) + 6 * 7680 + 20);
        assert_eq!(c.dst_layout, SurfaceLayout::Pitch);
        let bl = SurfaceLayout::BlockLinear { block_height_log2: 4, element_bytes: 4, image_height: 910, origin_x_bytes: 0, origin_y: 0 };
        let bl_plan = SourcePlan { layout: bl, page_kind: Some(6), line_bytes: 7632, offset: 0 };
        let c = foreign_write(&src, r, &bl_plan, map_va(4), 7680, 1908, 910, 5, 6, Remap::None).unwrap();
        assert_eq!((c.src_va, c.dst_va, c.line_bytes, c.lines), (src.va + 20 * 6400 + 40, map_va(4), 400, 50));
        assert_eq!(c.layout, SurfaceLayout::Pitch);
        assert!(matches!(c.dst_layout, SurfaceLayout::BlockLinear { origin_x_bytes: 20, origin_y: 6, image_height: 910, .. }));
        assert_eq!(foreign_write(&src, r, &bl_plan, map_va(4), 7680, 1908, 910, 1850, 0, Remap::None), Err(CopyError::Rect));
    }

    #[test]
    fn clear_push_words() {
        let mut buf = [0u32; 32];
        let mut p = crate::ce_present::Push::new(&mut buf);
        clear(&mut p, map_va(0), 2816, 440, 0xff00_0000).unwrap();
        let w = p.words().to_vec();
        assert_eq!(w.len(), 4 + 9 + 2);
        assert_eq!(&w[1..4], &[0xff00_0000, 0xff00_0000, CLEAR_COMPONENTS]);
        assert_eq!(&w[5..13], &[(map_va(0) >> 32) as u32, map_va(0) as u32, (map_va(0) >> 32) as u32, map_va(0) as u32, 2816, 2816, 704, 440]);
        assert_eq!(CLEAR_COMPONENTS, 0x34444);
        let mut p = crate::ce_present::Push::new(&mut buf);
        assert!(clear(&mut p, map_va(0), 2817, 1, 0).is_err());
    }

    #[test]
    fn import_messages_match_nvk_s_layout() {
        // `DRM_IOWR('d', 0x49, 24)`: dir 3 << 30, size 24 << 16, type 'd' << 8, nr 0x49.
        assert_eq!(DRM_IOCTL_GEM_EXPORT_NVKMS, (3 << 30) | (24 << 16) | ((b'd' as u32) << 8) | 0x49);
        let e = gem_export_params(0x1234);
        assert_eq!(&e[0..4], &0x1234u32.to_le_bytes());
        assert_eq!(&e[16..24], &4u64.to_le_bytes());
        let i = import_from_fd_params(7, 0x4b4d_0001, 0x4b4d_3100);
        assert_eq!(&i[0..4], &7u32.to_le_bytes());
        assert_eq!(&i[4..8], &1u32.to_le_bytes());
        assert_eq!(&i[8..12], &0x4b4d_0001u32.to_le_bytes());
        assert_eq!(&i[12..16], &0x4b4d_0001u32.to_le_bytes());
        assert_eq!(&i[16..20], &0x4b4d_3100u32.to_le_bytes());
    }

    #[test]
    fn bounce_transfers_are_packed_and_bounded() {
        let vram = Surface { va: map_va(2), pitch: 6400, width: 1600, height: 900 };
        let r = Rect { left: 100, top: 10, right: 300, bottom: 30 };
        let rb = bounce_copy(&vram, r, Dir::Readback).unwrap();
        assert_eq!(rb.dst_va, BOUNCE_VA);
        assert_eq!(rb.dst_pitch, 800);
        assert_eq!(rb.src_va, vram.va + 10 * 6400 + 400);
        let up = bounce_copy(&vram, r, Dir::Upload).unwrap();
        assert_eq!(up.src_va, BOUNCE_VA);
        assert_eq!(up.dst_va, rb.src_va);
        assert_eq!(bounce_bytes(r).unwrap(), 16384);
        assert_eq!(bounce_bytes(Rect::whole(1600, 900)).unwrap(), crate::round_up_page(5_760_000));
        assert_eq!(bounce_surface(Rect::whole(8192, 8192)), Err(CopyError::TooBig));
        assert_eq!(bounce_surface(Rect { left: 5, top: 0, right: 5, bottom: 1 }), Err(CopyError::Rect));
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        for (i, a) in COUNTERS.iter().enumerate() {
            assert!(a.len() <= 14, "{a}");
            assert!(a.starts_with("Rv"));
            for b in &COUNTERS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
