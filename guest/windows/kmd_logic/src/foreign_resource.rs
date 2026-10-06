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
//!
//! # Cross-process lifetime (S6, `docs/shared-foreign-surfaces.md`)
//!
//! Once a WDDM allocation has adopted the resource, other processes may open that
//! allocation (`DxgkDdiOpenAllocation`). The host resource must live until the LAST
//! of {the adopting allocation destroyed, every open closed}, whichever order
//! dxgkrnl delivers them in. This table counts opens per `(resource, process)`
//! ([`ForeignTable::open`] / [`ForeignTable::close`]) and decides who releases:
//!
//! ```text
//!   adopted (creator None, destroyed = false)
//!      │ open(p) / close(p): rows change, nothing is released
//!      │ allocation_destroyed
//!      ├─ no opens ──────────────► Release   (the destroyer tears the resource down)
//!      └─ opens > 0 ─► destroyed = true, Deferred
//!                         │ open(p) refused (the allocation is gone)
//!                         │ close(p): ... the close that drops the last open ► Release
//! ```
//!
//! `Release` is returned exactly once per resource: by `allocation_destroyed`
//! when nothing is open, else by the `close` that drains the last open of a
//! destroyed allocation. Both transitions test and set `destroyed` in the same
//! call, so two racing destroys or a destroy racing the last close cannot both
//! win. The caller then removes the record (`remove`) and unrefs the host
//! resource under the existing one-shot guard.

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

/// Most `(resource, process)` open rows across every foreign resource. A row is
/// one process holding one or more opens of one allocation, so this bounds the
/// number of distinct (resource, process) pairs, not the number of opens.
pub const MAX_FOREIGN_OPEN_ROWS: usize = 2048;

const PAGE: u64 = 4096;

/// Private-data bytes that hold the version-1 layout trailer (96 + 32) and the
/// version-2 trailer with plane 1 after it (128 + 16). Copies of
/// `helios_protocol::HELIOS_WDDM_PRIVATE_WITH_LAYOUT_BYTES` /
/// `_WITH_PLANES_BYTES`, pinned by `kmd_render`.
pub const PRIVATE_WITH_LAYOUT_BYTES: usize = 128;
pub const PRIVATE_WITH_PLANES_BYTES: usize = 144;

/// Private-data bytes the KMD's trailer write needs for `layout`: 128 for a
/// one-plane record (version 1, as always), 144 for a two-plane one (version 2).
pub const fn trailer_bytes(layout: &Layout) -> usize {
    if layout.plane1.is_some() {
        PRIVATE_WITH_PLANES_BYTES
    } else {
        PRIVATE_WITH_LAYOUT_BYTES
    }
}

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
/// `IMPORT_RM.flags` bit: plane 1 of a two-plane format follows the layout
/// (`HeliosForeignImportRmPlanes`, 120 bytes). Only with [`FLAG_LAYOUT`]; set iff
/// the request's layout carries `plane1` (`docs/shared-formats.md`).
pub const FLAG_PLANE1: u32 = 1 << 1;

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
// The rules mirror `foreign_scanout::Layout::validate` for the four 32-bit RGB
// formats (branch `kmd/foreign-scanout`, 66c714f): the same stride bound.
// Differences, on purpose: the extent floor is 1 (a foreign resource is any
// adopted allocation, not only a mode-sized scanout image) and the modifier set
// is closed to what the host's NVIDIA Vulkan driver was shown to import with the
// exact layout (host spike c6fab91): LINEAR and the NVIDIA block-linear family
// NVK builds for B8G8R8A8 / R8G8B8A8.
//
// Shared formats (`docs/shared-formats.md`): the record also takes the other
// formats Windows shares between processes ([`share_format`]): 8/16/64 bpp
// single-plane ones, YUYV, and the two-plane 4:2:0 NV12 / P010 / P016 with a
// second plane ([`Plane`]). The table is a copy of `helios_protocol::share_format`
// (this crate has no dependency edge to the protocol crate); `kmd_render` pins
// the two together with a const assertion over every fourcc, so a drift fails
// the driver build.

/// `DRM_FORMAT_*` accepted: the four 32-bit RGB formats first.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;
pub const FOURCC_ARGB8888: u32 = 0x3432_5241;
pub const FOURCC_XBGR8888: u32 = 0x3432_4258;
pub const FOURCC_ABGR8888: u32 = 0x3432_4241;
/// Then the shared formats beyond them.
pub const FOURCC_R8: u32 = 0x2020_3852;
pub const FOURCC_GR88: u32 = 0x3838_5247;
pub const FOURCC_R16: u32 = 0x2036_3152;
pub const FOURCC_GR1616: u32 = 0x3233_5247;
pub const FOURCC_RGB565: u32 = 0x3631_4752;
pub const FOURCC_ARGB1555: u32 = 0x3531_5241;
pub const FOURCC_ARGB4444: u32 = 0x3231_5241;
pub const FOURCC_ABGR2101010: u32 = 0x3033_4241;
pub const FOURCC_ABGR16161616F: u32 = 0x4834_4241;
pub const FOURCC_ABGR16161616: u32 = 0x3834_4241;
pub const FOURCC_YUYV: u32 = 0x5659_5559;
pub const FOURCC_NV12: u32 = 0x3231_564E;
pub const FOURCC_P010: u32 = 0x3031_3050;
pub const FOURCC_P016: u32 = 0x3631_3050;

/// How a shared format lays out its bytes: a copy of
/// `helios_protocol::ShareFormat` (same fields, same meaning).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareFormat {
    /// 1, or 2 for the 4:2:0 formats (plane 1: interleaved chroma at half width
    /// and half height).
    pub planes: u32,
    /// Bytes of one texel of plane 0 (YUYV: of one two-pixel group).
    pub bpp0: u32,
    /// Bytes of one texel of plane 1 (a chroma pair), 0 for one plane.
    pub bpp1: u32,
    /// Plane 0 texels per row = `ceil(width / hdiv0)` (2 for YUYV, else 1).
    pub hdiv0: u32,
    /// `width` must be even (4:2:2 and 4:2:0).
    pub even_width: bool,
    /// `height` must be even (4:2:0).
    pub even_height: bool,
}

impl ShareFormat {
    const fn one(bpp: u32) -> Self {
        Self {
            planes: 1,
            bpp0: bpp,
            bpp1: 0,
            hdiv0: 1,
            even_width: false,
            even_height: false,
        }
    }

    const fn yuv420(bpp0: u32) -> Self {
        Self {
            planes: 2,
            bpp0,
            bpp1: bpp0 * 2,
            hdiv0: 1,
            even_width: true,
            even_height: true,
        }
    }

    /// Bytes of one row of plane `plane` of a `width`-pixel image, unpadded.
    pub const fn row_bytes(&self, plane: u32, width: u32) -> u64 {
        if plane == 0 {
            (width.div_ceil(self.hdiv0) as u64) * self.bpp0 as u64
        } else {
            (width.div_ceil(2) as u64) * self.bpp1 as u64
        }
    }

    /// Rows of plane `plane` of a `height`-row image.
    pub const fn rows(&self, plane: u32, height: u32) -> u32 {
        if plane == 0 {
            height
        } else {
            height.div_ceil(2)
        }
    }

    /// The alignment a plane's stride must have: its texel size, at most 4.
    pub const fn stride_align(&self, plane: u32) -> u32 {
        let bpp = if plane == 0 { self.bpp0 } else { self.bpp1 };
        if bpp > 4 {
            4
        } else {
            bpp
        }
    }
}

/// The layout facts of `fourcc`, or `None` for a format a foreign resource may
/// not hold. Mirrors `helios_protocol::share_format` exactly.
pub const fn share_format(fourcc: u32) -> Option<ShareFormat> {
    Some(match fourcc {
        FOURCC_XRGB8888 | FOURCC_ARGB8888 | FOURCC_XBGR8888 | FOURCC_ABGR8888 => {
            ShareFormat::one(4)
        }
        FOURCC_R8 => ShareFormat::one(1),
        FOURCC_GR88 | FOURCC_R16 | FOURCC_RGB565 | FOURCC_ARGB1555 | FOURCC_ARGB4444 => {
            ShareFormat::one(2)
        }
        FOURCC_GR1616 | FOURCC_ABGR2101010 => ShareFormat::one(4),
        FOURCC_ABGR16161616F | FOURCC_ABGR16161616 => ShareFormat::one(8),
        FOURCC_YUYV => ShareFormat {
            planes: 1,
            bpp0: 4,
            bpp1: 0,
            hdiv0: 2,
            even_width: true,
            even_height: false,
        },
        FOURCC_NV12 => ShareFormat::yuv420(1),
        FOURCC_P010 | FOURCC_P016 => ShareFormat::yuv420(2),
        _ => return None,
    })
}

/// One of the four 32-bit RGB formats: the only ones the KMD's scanout, flip,
/// copy and blit consumers carry. Every consumer that reads a record's pixels
/// refuses anything else (`Layout::is_rgb32`).
pub const fn is_rgb32_fourcc(fourcc: u32) -> bool {
    matches!(
        fourcc,
        FOURCC_XRGB8888 | FOURCC_ARGB8888 | FOURCC_XBGR8888 | FOURCC_ABGR8888
    )
}

/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0, s=1, g=2, k=0x06, h=0)`: the
/// family NVK advertises, `base | h` with `h` the log2 block height in GOBs.
pub const MOD_NVIDIA_BLOCK_LINEAR_BASE: u64 = 0x0300_0000_0060_6010;
/// The GB20x family for 1-byte elements: `BASE` with the sector-layout field (bit 22 and
/// bits 26..27) naming the Blackwell 8-bit GOB. Copy of `helios_protocol::MOD_NVIDIA_BL_GB20X_8BPP`,
/// pinned by `kmd_render`.
pub const MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP: u64 = 0x0300_0000_0420_6010;
/// The same for 2-byte elements (the Blackwell 16-bit GOB). Copy of
/// `helios_protocol::MOD_NVIDIA_BL_GB20X_16BPP`, pinned by `kmd_render`.
pub const MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP: u64 = 0x0300_0000_0460_6010;

/// The block-linear family a plane whose elements are `element_bytes` wide must use:
/// 1 byte [`MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP`], 2 bytes
/// [`MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP`], anything else (4 and 8 bytes, YUYV's 4)
/// [`MOD_NVIDIA_BLOCK_LINEAR_BASE`]. Copy of `helios_protocol::gb20x_family`, pinned by
/// `kmd_render`. `element_bytes` is [`ShareFormat::bpp0`] for plane 0 and
/// [`ShareFormat::bpp1`] for plane 1.
pub const fn gb20x_family(element_bytes: u32) -> u64 {
    match element_bytes {
        1 => MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP,
        2 => MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP,
        _ => MOD_NVIDIA_BLOCK_LINEAR_BASE,
    }
}

/// Largest accepted `h` (32 GOBs = 256 rows per block).
pub const MAX_BLOCK_HEIGHT_LOG2: u32 = 5;
/// Rows in one GOB.
pub const GOB_ROWS: u64 = 8;
/// Smallest and largest extent, and the largest pitch.
pub const MIN_DIM: u32 = 1;
pub const MAX_DIM: u32 = 16_384;
pub const MAX_STRIDE: u32 = 1 << 20;

/// Plane 1 of a two-plane format (NV12 / P010 / P016): the interleaved chroma
/// at `ceil(w/2) x ceil(h/2)` texel pairs. Its own modifier, because NVK picks the
/// block height per plane extent (`docs/shared-formats.md` 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    /// Plane 1 pitch in bytes.
    pub stride: u32,
    /// Plane 1 offset in bytes from the start of the object; at or past the end
    /// of plane 0 ([`Layout::validate`]).
    pub offset: u32,
    /// `DRM_FORMAT_MOD_*` of plane 1: LINEAR iff plane 0 is, else
    /// `gb20x_family(bpp1) | h` (its `h`, and its family, may differ from plane 0's).
    pub modifier: u64,
}

/// The picture inside a foreign resource: everything a scanout flip or an
/// importer needs besides which object it is. Plane 0 in the fields, plane 1 (the
/// two-plane formats only) in [`Layout::plane1`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Plane 0 pitch in bytes (`rowPitch` of the explicit-modifier image).
    pub stride: u32,
    /// Plane 0 offset in bytes from the start of the object.
    pub offset: u32,
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*`: [`MOD_LINEAR`] or `gb20x_family(bpp0) | h`, `h <= 5` (for the four
    /// 32 bpp formats that is `MOD_NVIDIA_BLOCK_LINEAR_BASE | h`).
    pub modifier: u64,
    /// Plane 1 of a two-plane format; `Some` iff the format has two planes.
    pub plane1: Option<Plane>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Width or height outside `MIN_DIM..=MAX_DIM`, or odd where the format
    /// needs it even (YUYV width, NV12 / P010 / P016 width and height).
    Dimensions,
    /// A fourcc this KMD does not forward ([`share_format`] has no row for it).
    Format,
    /// A plane's stride under its row bytes, off its alignment, or over
    /// [`MAX_STRIDE`].
    Stride,
    /// A plane's modifier is not LINEAR and not `gb20x_family(element bytes of that plane) | h`
    /// with `h <= 5` (another family's modifier is refused, whatever its `h`); or plane 1
    /// LINEAR with plane 0 block-linear (or the other way round).
    Modifier,
    /// The layout needs more bytes than the resource has, or plane 1 starts
    /// before plane 0 ends.
    TooLarge,
    /// `plane1` is present on a one-plane format, or absent on a two-plane one.
    Planes,
}

/// `h` of `modifier` if it is `family | h` with `h <= MAX_BLOCK_HEIGHT_LOG2`, else `None`.
const fn family_log2(modifier: u64, family: u64) -> Option<u32> {
    if modifier >= family && modifier <= family + MAX_BLOCK_HEIGHT_LOG2 as u64 {
        Some((modifier - family) as u32)
    } else {
        None
    }
}

/// `h` of a block-linear modifier of ANY of the three GB20x families
/// ([`MOD_NVIDIA_BLOCK_LINEAR_BASE`] and its 8BPP and 16BPP variants), or `None` for LINEAR
/// and for anything else. Which family a plane may carry is [`plane_modifier_ok`]'s rule; the
/// block height is the same function of `h` in all three (see [`plane_min_bytes`]).
const fn block_log2(modifier: u64) -> Option<u32> {
    if let Some(h) = family_log2(modifier, MOD_NVIDIA_BLOCK_LINEAR_BASE) {
        return Some(h);
    }
    if let Some(h) = family_log2(modifier, MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP) {
        return Some(h);
    }
    family_log2(modifier, MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP)
}

/// A modifier a plane whose elements are `element_bytes` wide may carry: LINEAR, or the one
/// block-linear family [`gb20x_family`] names for that element size, with `h <= 5`. Every
/// other value, another family's included, is refused.
const fn plane_modifier_ok(modifier: u64, element_bytes: u32) -> bool {
    modifier == MOD_LINEAR || family_log2(modifier, gb20x_family(element_bytes)).is_some()
}

/// `offset + stride * rows`, `rows` rounded up to the plane's block when it is
/// block-linear. The block is `GOB_ROWS << h` whatever the family: the GB20x 8-bit and 16-bit
/// GOBs differ from the desktop GOB in the sector layout (the order of the sectors inside the
/// GOB), which the modifier names in bits 22 and 26..27; they are the same 64 byte x 8 row
/// GOB, so the row count a block spans, and with it this lower bound, does not depend on the
/// family. That is an assumption (no hardware measurement here); a family whose GOB were
/// taller would only make this bound too LOW, never too high, and the host's check of the
/// image against the object (`image size <= resource size`) stays the backstop.
/// Saturating, so even an unvalidated layout (a `u32::MAX` stride
/// and height) yields a large bound and never wraps.
const fn plane_min_bytes(offset: u32, stride: u32, rows: u64, modifier: u64) -> u64 {
    let rows = match block_log2(modifier) {
        None => rows,
        Some(h) => {
            let block = GOB_ROWS << h;
            (rows + block - 1) / block * block
        }
    };
    (offset as u64).saturating_add((stride as u64).saturating_mul(rows))
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
        let Some(f) = share_format(self.fourcc) else {
            return Err(LayoutError::Format);
        };
        if self.plane1.is_some() != (f.planes == 2) {
            return Err(LayoutError::Planes);
        }
        if (f.even_width && self.width % 2 != 0) || (f.even_height && self.height % 2 != 0) {
            return Err(LayoutError::Dimensions);
        }
        if (self.stride as u64) < f.row_bytes(0, self.width)
            || self.stride > MAX_STRIDE
            || self.stride % f.stride_align(0) != 0
        {
            return Err(LayoutError::Stride);
        }
        if !plane_modifier_ok(self.modifier, f.bpp0) {
            return Err(LayoutError::Modifier);
        }
        if let Some(p) = self.plane1 {
            if (p.stride as u64) < f.row_bytes(1, self.width)
                || p.stride > MAX_STRIDE
                || p.stride % f.stride_align(1) != 0
            {
                return Err(LayoutError::Stride);
            }
            // LINEAR planes stay LINEAR together; block-linear ones may differ in `h` and in
            // family (each plane's family follows its own element size: NV12 is 8BPP + 16BPP).
            if !plane_modifier_ok(p.modifier, f.bpp1)
                || (p.modifier == MOD_LINEAR) != (self.modifier == MOD_LINEAR)
            {
                return Err(LayoutError::Modifier);
            }
            // Plane 1 starts at or after the end of plane 0.
            if (p.offset as u64) < self.plane0_min_bytes() {
                return Err(LayoutError::TooLarge);
            }
        }
        Ok(())
    }

    /// `h` of plane 0's NVIDIA block-linear modifier, or `None` for LINEAR and
    /// for a modifier outside the accepted family.
    pub const fn block_height_log2(&self) -> Option<u32> {
        block_log2(self.modifier)
    }

    /// Number of planes the record carries (1 or 2).
    pub const fn plane_count(&self) -> u32 {
        if self.plane1.is_some() {
            2
        } else {
            1
        }
    }

    /// One of the four 32-bit RGB formats, one plane: the only record the
    /// scanout, flip, copy and blit consumers read. They test this (or their own
    /// narrower fourcc match) before they trust the stride as "width * 4".
    pub const fn is_rgb32(&self) -> bool {
        self.plane1.is_none() && is_rgb32_fourcc(self.fourcc)
    }

    /// A LOWER BOUND on the bytes plane 0 occupies from the start of the object:
    /// `offset + stride * rows`, `rows` being `height` for LINEAR and `height`
    /// rounded up to the block for block-linear.
    pub const fn plane0_min_bytes(&self) -> u64 {
        plane_min_bytes(self.offset, self.stride, self.height as u64, self.modifier)
    }

    /// The same bound for plane 1 (`ceil(height / 2)` rows, rounded to plane 1's
    /// own block when it is block-linear), `None` for a one-plane record.
    pub const fn plane1_min_bytes(&self) -> Option<u64> {
        match self.plane1 {
            None => None,
            Some(p) => Some(plane_min_bytes(
                p.offset,
                p.stride,
                (self.height as u64 + 1) / 2,
                p.modifier,
            )),
        }
    }

    /// A LOWER BOUND on the bytes the image occupies from the start of the
    /// object: the larger of the planes' bounds ([`Self::plane0_min_bytes`],
    /// [`Self::plane1_min_bytes`]). It is a bound and not the exact size: RM
    /// rounds allocations up (a 1080p linear image is 0x7e9000 in a 0x7f0000
    /// object), so the check is `min_bytes() <= size`, never equality. Meaningful
    /// for a validated layout (no overflow: every factor is bounded); saturating
    /// for any other.
    pub const fn min_bytes(&self) -> u64 {
        let p0 = self.plane0_min_bytes();
        match self.plane1_min_bytes() {
            Some(p1) if p1 > p0 => p1,
            _ => p0,
        }
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
    /// `flags` has a bit this KMD does not know, or [`FLAG_PLANE1`] disagrees with
    /// the decoded layout (the flag set with no plane tail, or a tail with the
    /// flag clear).
    Flags,
    /// `size` is 0 or not a whole number of pages.
    Size,
    /// `size` is over [`MAX_FOREIGN_RESOURCE_BYTES`].
    TooLarge,
    /// No layout was supplied ([`FLAG_LAYOUT`] clear, which includes
    /// [`FLAG_PLANE1`] alone). Not optional.
    LayoutRequired,
    /// The layout is invalid, or does not fit `size`.
    Layout(LayoutError),
}

/// Structural checks of an import request. Pure; ownership and quotas are
/// checked later, under the lock that makes them atomic with the reservation.
/// Returns the layout to record.
///
/// `layout` is what the escape layer decoded from the tail when
/// [`FLAG_LAYOUT`] is set (with `plane1` from the 16-byte plane tail when
/// [`FLAG_PLANE1`] is also set), else `None`.
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
    if flags & !(FLAG_LAYOUT | FLAG_PLANE1) != 0 {
        return Err(RequestError::Flags);
    }
    if size == 0 || size % PAGE != 0 {
        return Err(RequestError::Size);
    }
    if size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(RequestError::TooLarge);
    }
    // `FLAG_PLANE1` without `FLAG_LAYOUT` has no layout to hang a plane on.
    let layout = match layout {
        Some(l) if flags & FLAG_LAYOUT != 0 => l,
        _ => return Err(RequestError::LayoutRequired),
    };
    // The flag says whether the request carried the plane tail, and the escape
    // layer decoded `plane1` from exactly that tail: the two must agree. (A
    // two-plane fourcc without the flag, or a one-plane one with it, is then
    // `Layout(Planes)` from the check below.)
    if (flags & FLAG_PLANE1 != 0) != layout.plane1.is_some() {
        return Err(RequestError::Flags);
    }
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
    /// The adopting allocation was destroyed: the release is the last close's job
    /// (or already done). Never set before adoption.
    pub destroyed: bool,
    /// The memory is RM SYSTEM memory the KMD made for one of its own allocations
    /// (`KmdRmClient` = 5, `rm_sysmem`) and the host maps into the Venus window on
    /// `RESOURCE_MAP_BLOB`: the one class of foreign resource that has a CPU view. Set
    /// by the KMD's own service only ([`ForeignTable::mark_sysmem`]), between the import
    /// and the adoption; no user-mode request can set it.
    pub sysmem: bool,
    /// The device token that imported it, KEPT after a WDDM allocation adopted it
    /// (`creator` is `None` from then on). Only the KMD's own foreign flip reads it
    /// (`ForeignFlip`, `docs/kmd-rm-client.md` 15.18): a flip names the DRM file of the
    /// device that made the resource, and the arbiter's source is that device's.
    pub origin: u64,
    /// The importer closed the DRM file `rm_handle` names (or the importer's device went
    /// away) since this record was made. The `(rm_handle, gem_handle)` pair of such a
    /// record is never to be flipped again: the host may have reused the file number for
    /// another file. Set by [`ForeignTable::file_closed`] / [`ForeignTable::owner_closed`],
    /// never cleared.
    pub file_closed: bool,
}

/// What the KMD's own flip reads of a record (see [`ForeignTable::flip_record`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlipRecord {
    pub origin: u64,
    /// A WDDM allocation owns the resource (the creator is gone from the record).
    pub adopted: bool,
    pub destroyed: bool,
    pub sysmem: bool,
    pub file_closed: bool,
    pub rm_handle: u32,
    pub gem_handle: u32,
    pub layout: Layout,
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
    /// WDDM allocations that named a foreign resource and were turned away
    /// ([`AdoptRefusal`]).
    pub refused_adopt: u32,
    pub live_high_water: u32,
    /// Opens of an adopted foreign allocation that were counted
    /// ([`ForeignTable::open`] returned `Opened`).
    pub opened: u32,
    /// Closes that matched an open row.
    pub closed: u32,
    /// Opens turned away ([`OpenRefusal`]). Included in [`Counters::refused`].
    pub refused_open: u32,
    /// Closes that found no record or no row of that process: a lifecycle bug
    /// upstream, or a record the transport teardown already swept.
    pub close_missed: u32,
    /// Allocation destroys that found opens still live and deferred the release.
    /// Expected 0 under dxgkrnl's contract (every open is closed before the
    /// allocation is destroyed); nonzero means the guard earned its keep.
    pub deferred: u32,
    /// Of those, how many were later completed by the last close.
    pub deferred_released: u32,
    /// `ATTACH_RESOURCE` attempts naming a foreign resource.
    pub attached: u32,
    /// Of those, attempts by a caller that is neither the creating device nor
    /// a process holding an open of the resource ([`AttachOutcome`]).
    pub attached_unsanctioned: u32,
    // ---- shared formats (`docs/shared-formats.md`) ------------------------------------
    /// Imports of a one-plane record outside the four 32-bit RGB formats
    /// (`FgImpFmt`). Included in [`Counters::imported`].
    pub imported_format: u32,
    /// Imports of a two-plane record (`FgImp2P`). Included in [`Counters::imported`].
    pub imported_planes: u32,
    /// Adoptions of a two-plane record by a WDDM allocation (`FgAdo2P`). Included in
    /// [`Counters::adopted`].
    pub adopted_planes: u32,
    /// Requests refused for a fourcc outside [`share_format`] (`FgRefFmt`). Included in
    /// [`Counters::refused_request`].
    pub refused_format: u32,
    /// Requests refused because the plane tail and the format disagree: a plane on a
    /// one-plane format, none on a two-plane one, or [`FLAG_PLANE1`] against the tail
    /// (`FgRefPln`). Included in [`Counters::refused_request`].
    pub refused_planes: u32,
    /// Requests for a shared format beyond the four 32-bit ones refused for their
    /// geometry, stride, modifier or size (`FgRefNewG`). Included in
    /// [`Counters::refused_request`].
    pub refused_new_geometry: u32,
    /// Requests refused for a modifier that is neither LINEAR nor `gb20x_family(plane
    /// element bytes) | h`, `h <= 5`, or for mixing a LINEAR plane with a block-linear one
    /// (`FgRefMod`, [`LayoutError::Modifier`]). Any format, the 32-bit four included.
    /// Included in [`Counters::refused_request`]; for a format beyond the 32-bit four also
    /// included in [`Counters::refused_new_geometry`].
    pub refused_modifier: u32,
    /// Adoptions of a two-plane record refused for want of the 144-byte private data
    /// (`FgAdoNoPln`, [`AdoptRefusal::NoPlaneRoom`]). Included in
    /// [`Counters::refused_adopt`].
    pub refused_no_plane_room: u32,
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
            .saturating_add(self.refused_open)
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
    /// The buffer can also take plane 1 after it (at least
    /// [`PRIVATE_WITH_PLANES_BYTES`] bytes): required to adopt a two-plane record.
    pub plane_room: bool,
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
            Self::NoPlaneRoom => 12,
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
    /// The record has two planes and the private data cannot take the version-2
    /// trailer (under [`PRIVATE_WITH_PLANES_BYTES`]).
    NoPlaneRoom,
}

/// One process's opens of one adopted allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenRow {
    resource_id: u32,
    /// dxgkrnl's opaque `hKmdProcess`, compared only for equality.
    process: u64,
    refs: u32,
}

/// Why [`ForeignTable::open`] turned an open away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenRefusal {
    /// The record exists but no WDDM allocation adopted it: an allocation
    /// opened a foreign resid it does not own. Cannot happen through dxgkrnl
    /// (the identity is the KMD's own write); refused rather than counted.
    NotAdopted,
    /// The adopting allocation was already destroyed.
    Destroyed,
    /// The open table is full, or the process's count would overflow.
    Rows,
}

impl OpenRefusal {
    /// A stable nonzero code for the registry trace.
    pub const fn code(self) -> u32 {
        match self {
            Self::NotAdopted => 1,
            Self::Destroyed => 2,
            Self::Rows => 3,
        }
    }
}

/// The outcome of [`ForeignTable::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// No record: an ordinary Venus resource, the legacy open applies.
    NotForeign,
    /// Counted. The opener's identity is built from this (the recorded size and
    /// layout, never anything the creator wrote).
    Opened(Adopted),
    Refused(OpenRefusal),
}

/// The outcome of [`ForeignTable::close`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    /// No record: the resource was swept (transport teardown) or never foreign.
    Gone,
    /// The process holds no open of it (counted, `close_missed`).
    NoRow,
    /// Closed; the resource lives on.
    Kept,
    /// Closed the last open of a destroyed allocation: the caller must release
    /// the host resource now (once; see the module docs).
    Release,
}

/// The outcome of [`ForeignTable::allocation_destroyed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestroyOutcome {
    /// Not an adopted foreign resource: the ordinary teardown applies unchanged.
    Proceed,
    /// Nothing is open: release the host resource now.
    Release,
    /// `opens` are still live: do NOT release; the last close will.
    Deferred { opens: u32 },
    /// A second destroy of the same allocation: release nothing.
    Repeat,
}

/// The outcome of [`ForeignTable::note_attach`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachOutcome {
    NotForeign,
    /// The caller created the resource, or its process holds an open of it.
    Sanctioned,
    /// Neither. Counted; the policy decides whether it is refused.
    Unsanctioned,
}

pub struct ForeignTable {
    limits: Limits,
    entries: Vec<Entry>,
    reserved: Vec<Reservation>,
    opens: Vec<OpenRow>,
    open_row_cap: usize,
    counters: Counters,
}

impl ForeignTable {
    /// The KMD's table: [`Limits::DEFAULT`], storage reserved up front.
    pub fn new() -> Self {
        Self::with_limits(Limits::DEFAULT)
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self::with_limits_and_rows(limits, MAX_FOREIGN_OPEN_ROWS)
    }

    /// As [`Self::with_limits`], with an explicit open-row capacity (tests).
    pub fn with_limits_and_rows(limits: Limits, open_rows: usize) -> Self {
        Self {
            limits,
            entries: Vec::with_capacity(limits.total),
            reserved: Vec::with_capacity(limits.total),
            opens: Vec::with_capacity(open_rows),
            open_row_cap: open_rows,
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
            destroyed: false,
            sysmem: false,
            origin: r.owner,
            file_closed: false,
        });
        self.counters.imported = self.counters.imported.saturating_add(1);
        if layout.plane1.is_some() {
            self.counters.imported_planes = self.counters.imported_planes.saturating_add(1);
        } else if !is_rgb32_fourcc(layout.fourcc) {
            self.counters.imported_format = self.counters.imported_format.saturating_add(1);
        }
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
                if e.layout.plane1.is_some() {
                    self.counters.adopted_planes = self.counters.adopted_planes.saturating_add(1);
                }
                true
            }
            None => false,
        }
    }

    /// Mark `resource_id` as the KMD's own RM system memory: from now on it may be mapped
    /// into the host-visible window ([`Self::cpu_mappable`]). Only a resource that no
    /// allocation has adopted yet, whose creator is `owner` (the KMD's own token), can be
    /// marked; `false` otherwise (and nothing changes).
    pub fn mark_sysmem(&mut self, resource_id: u32, owner: u64) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|e| e.resource_id == resource_id && e.creator == Some(owner))
        {
            Some(e) => {
                e.sysmem = true;
                true
            }
            None => false,
        }
    }

    /// Whether `resource_id` is a foreign resource with a CPU view (see
    /// [`Entry::sysmem`]). Every other foreign resource, user-imported or the KMD's
    /// vidmem, stays unmappable.
    pub fn cpu_mappable(&self, resource_id: u32) -> bool {
        self.get(resource_id).is_some_and(|e| e.sysmem)
    }

    /// The facts a flip of an allocation that adopted RM system memory needs: the DRM
    /// file and GEM handle the resource was imported from, the layout and the size.
    /// `None` unless it is sysmem AND adopted (an allocation owns it).
    pub fn sysmem_source(&self, resource_id: u32) -> Option<(u32, u32, Layout, u64)> {
        self.get(resource_id)
            .filter(|e| e.sysmem && e.creator.is_none() && !e.destroyed)
            .map(|e| (e.rm_handle, e.gem_handle, e.layout, e.size))
    }

    /// The facts the KMD's flip of an allocation that adopted a foreign resource decides
    /// on (`foreign_flip::decide`): who imported it, which DRM file and GEM, the layout, and
    /// where the record is in its life. `None` for an id with no record.
    pub fn flip_record(&self, resource_id: u32) -> Option<FlipRecord> {
        self.get(resource_id).map(|e| FlipRecord {
            origin: e.origin,
            adopted: e.creator.is_none(),
            destroyed: e.destroyed,
            sysmem: e.sysmem,
            file_closed: e.file_closed,
            rm_handle: e.rm_handle,
            gem_handle: e.gem_handle,
            layout: e.layout,
            size: e.size,
        })
    }

    /// `owner` closed the DRM file `rm_handle`: every record it imported from that file
    /// is poisoned for flipping (the host may reuse the number). Returns how many.
    pub fn file_closed(&mut self, owner: u64, rm_handle: u32) -> usize {
        let mut n = 0;
        for e in self.entries.iter_mut() {
            if e.origin == owner && e.rm_handle == rm_handle && !e.file_closed {
                e.file_closed = true;
                n += 1;
            }
        }
        n
    }

    /// `owner`'s device is gone (its files are closed with it, and the token may be
    /// handed to a new device): every record it imported is poisoned for flipping.
    pub fn owner_closed(&mut self, owner: u64) -> usize {
        let mut n = 0;
        for e in self.entries.iter_mut() {
            if e.origin == owner && !e.file_closed {
                e.file_closed = true;
                n += 1;
            }
        }
        n
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
    /// no room / geometry / format differ ............. NoTrailerRoom / NoPlaneRoom / GeometryMismatch / LayoutMismatch
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
        } else if e.layout.plane1.is_some() && !req.plane_room {
            Some(AdoptRefusal::NoPlaneRoom)
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
        if r == AdoptRefusal::NoPlaneRoom {
            self.counters.refused_no_plane_room =
                self.counters.refused_no_plane_room.saturating_add(1);
        }
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
        // The rows die with the record (`retain` never allocates). A later close
        // of one of them finds no record: `CloseOutcome::Gone`.
        self.opens.retain(|r| r.resource_id != resource_id);
        Some(self.entries.swap_remove(idx))
    }

    // ---- cross-process opens and the release decision -----------------------

    /// Live opens of `resource_id`, summed over processes.
    pub fn opens(&self, resource_id: u32) -> u32 {
        self.opens
            .iter()
            .filter(|r| r.resource_id == resource_id)
            .fold(0u32, |a, r| a.saturating_add(r.refs))
    }

    /// Live opens across every resource.
    pub fn open_refs_total(&self) -> u32 {
        self.opens
            .iter()
            .fold(0u32, |a, r| a.saturating_add(r.refs))
    }

    /// Destroyed allocations whose release still waits for opens to drain.
    pub fn orphans(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.destroyed && self.opens(e.resource_id) != 0)
            .count()
    }

    /// Whether `process` holds at least one open of `resource_id`.
    pub fn process_has_open(&self, resource_id: u32, process: u64) -> bool {
        self.opens
            .iter()
            .any(|r| r.resource_id == resource_id && r.process == process && r.refs != 0)
    }

    /// `DxgkDdiOpenAllocation` of an allocation that names `resource_id`, by
    /// `process`. Counts the open iff the resource is an adopted foreign one whose
    /// allocation still lives; the caller must pair every `Opened` with exactly
    /// one [`Self::close`] (the open handle remembers it).
    pub fn open(&mut self, resource_id: u32, process: u64) -> OpenOutcome {
        let Some(e) = self.get(resource_id).copied() else {
            return OpenOutcome::NotForeign;
        };
        let refusal = if e.creator.is_some() {
            Some(OpenRefusal::NotAdopted)
        } else if e.destroyed {
            Some(OpenRefusal::Destroyed)
        } else {
            self.add_open_ref(resource_id, process).err()
        };
        match refusal {
            Some(r) => {
                self.counters.refused_open = self.counters.refused_open.saturating_add(1);
                OpenOutcome::Refused(r)
            }
            None => {
                self.counters.opened = self.counters.opened.saturating_add(1);
                OpenOutcome::Opened(Adopted {
                    size: e.size,
                    layout: e.layout,
                })
            }
        }
    }

    fn add_open_ref(&mut self, resource_id: u32, process: u64) -> Result<(), OpenRefusal> {
        if let Some(row) = self
            .opens
            .iter_mut()
            .find(|r| r.resource_id == resource_id && r.process == process)
        {
            row.refs = row.refs.checked_add(1).ok_or(OpenRefusal::Rows)?;
            return Ok(());
        }
        if self.opens.len() >= self.open_row_cap {
            return Err(OpenRefusal::Rows);
        }
        // `len < cap` and the vector was reserved at `cap`: no growth.
        self.opens.push(OpenRow {
            resource_id,
            process,
            refs: 1,
        });
        Ok(())
    }

    /// `DxgkDdiCloseAllocation` (or the unwind of a failed open) of an open this
    /// table counted. See [`CloseOutcome`].
    pub fn close(&mut self, resource_id: u32, process: u64) -> CloseOutcome {
        let Some(destroyed) = self.get(resource_id).map(|e| e.destroyed) else {
            self.counters.close_missed = self.counters.close_missed.saturating_add(1);
            return CloseOutcome::Gone;
        };
        let Some(idx) = self
            .opens
            .iter()
            .position(|r| r.resource_id == resource_id && r.process == process)
        else {
            self.counters.close_missed = self.counters.close_missed.saturating_add(1);
            return CloseOutcome::NoRow;
        };
        if self.opens[idx].refs > 1 {
            self.opens[idx].refs -= 1;
        } else {
            self.opens.swap_remove(idx);
        }
        self.counters.closed = self.counters.closed.saturating_add(1);
        if destroyed && self.opens(resource_id) == 0 {
            self.counters.deferred_released = self.counters.deferred_released.saturating_add(1);
            CloseOutcome::Release
        } else {
            CloseOutcome::Kept
        }
    }

    /// `DxgkDdiDestroyAllocation` of the allocation that adopted `resource_id`:
    /// decide whether this call releases the host resource. Test-and-set of
    /// `destroyed`, so only one caller ever gets `Release` from this side.
    pub fn allocation_destroyed(&mut self, resource_id: u32) -> DestroyOutcome {
        let opens = self.opens(resource_id);
        let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.resource_id == resource_id)
        else {
            return DestroyOutcome::Proceed;
        };
        if e.creator.is_some() {
            return DestroyOutcome::Proceed;
        }
        if e.destroyed {
            return DestroyOutcome::Repeat;
        }
        e.destroyed = true;
        if opens == 0 {
            DestroyOutcome::Release
        } else {
            self.counters.deferred = self.counters.deferred.saturating_add(1);
            DestroyOutcome::Deferred { opens }
        }
    }

    /// An `ATTACH_RESOURCE` of `resource_id` by `owner` (the escaping device's
    /// token, 0 if none) of `process`. Only counts and classifies; refusing is the
    /// caller's policy. The sanctioned routes are: the device that imported the
    /// resource (before or after adoption), and any process that opened the
    /// allocation that adopted it.
    pub fn note_attach(&mut self, resource_id: u32, owner: u64, process: u64) -> AttachOutcome {
        let Some(creator) = self.get(resource_id).map(|e| e.creator) else {
            return AttachOutcome::NotForeign;
        };
        self.counters.attached = self.counters.attached.saturating_add(1);
        let by_creator = owner != 0 && creator == Some(owner);
        if by_creator || self.process_has_open(resource_id, process) {
            AttachOutcome::Sanctioned
        } else {
            self.counters.attached_unsanctioned =
                self.counters.attached_unsanctioned.saturating_add(1);
            AttachOutcome::Unsanctioned
        }
    }

    /// Count an import request [`validate_request`] refused: [`RefusalKind::BadRequest`],
    /// plus the shared-format reasons. `layout` is the decoded layout the request
    /// carried, if any (`None` when it carried none).
    pub fn note_request_refusal(&mut self, why: RequestError, layout: Option<&Layout>) {
        self.note_refusal(RefusalKind::BadRequest);
        let c = &mut self.counters;
        match why {
            RequestError::Layout(LayoutError::Format) => {
                c.refused_format = c.refused_format.saturating_add(1);
            }
            RequestError::Layout(LayoutError::Planes) => {
                c.refused_planes = c.refused_planes.saturating_add(1);
            }
            // The flag and the tail disagree (the escape layer builds both from the one
            // request, so this is a caller that set a plane tail without the flag bit's
            // meaning): a plane problem, not an unknown bit.
            RequestError::Flags if layout.is_some_and(|l| l.plane1.is_some()) => {
                c.refused_planes = c.refused_planes.saturating_add(1);
            }
            RequestError::Layout(why) => {
                // A modifier that is not LINEAR or the plane's own GB20x family (any format,
                // the four 32-bit ones included). Also counted below when the format is one
                // of the shared ones: `FgRefMod` is a subset of `FgRefNewG` there.
                if why == LayoutError::Modifier {
                    c.refused_modifier = c.refused_modifier.saturating_add(1);
                }
                // A known shared format beyond the 32-bit four whose geometry was refused.
                if layout.is_some_and(|l| share_format(l.fourcc).is_some() && !l.is_rgb32()) {
                    c.refused_new_geometry = c.refused_new_geometry.saturating_add(1);
                }
            }
            _ => {}
        }
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
            plane1: None,
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
        // An unknown flag bit is refused, with or without the layout bit. (Bit 1 is
        // `FLAG_PLANE1` now; its own rules are in the `shared_formats` tests.)
        assert_eq!(
            validate(FLAG_LAYOUT | 4, 8 * MIB, Some(lay())),
            Err(RequestError::Flags)
        );
        assert_eq!(validate(4, 8 * MIB, None), Err(RequestError::Flags));
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
        // 'BG24' (BGR888), 'XR30' (XRGB2101010), 0: real or empty fourccs that are
        // not shared, so not forwarded. (RGB565 and the fp16 formats were here before
        // the shared formats; they are accepted now, see `shared_formats`.)
        for f in [0x3432_4742, 0x3033_5258, 0] {
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
            plane_room: true,
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

    // ---- cross-process opens: the lifetime state machine ---------------------

    const PROC_A: u64 = 0x1000;
    const PROC_B: u64 = 0x2000;
    const PROC_C: u64 = 0x3000;

    /// Resource 50, imported by device 1 and adopted by an allocation.
    fn adopted() -> ForeignTable {
        let mut t = with_import();
        assert!(matches!(adopt(&mut t, &req()), Ok(AdoptPlan::Foreign(_))));
        t
    }

    #[test]
    fn an_open_of_an_ordinary_resource_is_not_counted() {
        let mut t = adopted();
        assert_eq!(t.open(999, PROC_A), OpenOutcome::NotForeign);
        assert_eq!(t.counters().opened, 0);
        assert_eq!(t.counters().refused_open, 0);
    }

    #[test]
    fn an_open_returns_the_recorded_size_and_layout() {
        let mut t = adopted();
        assert_eq!(
            t.open(50, PROC_B),
            OpenOutcome::Opened(Adopted {
                size: 8 * MIB,
                layout: lay(),
            })
        );
        assert_eq!(t.opens(50), 1);
        assert!(t.process_has_open(50, PROC_B));
        assert!(!t.process_has_open(50, PROC_C));
    }

    #[test]
    fn a_resource_nobody_adopted_cannot_be_opened() {
        let mut t = with_import();
        assert_eq!(
            t.open(50, PROC_A),
            OpenOutcome::Refused(OpenRefusal::NotAdopted)
        );
        assert_eq!(t.opens(50), 0);
        assert_eq!(t.counters().refused_open, 1);
        assert_eq!(t.counters().opened, 0);
    }

    #[test]
    fn opens_are_counted_per_process_and_merge_into_one_row() {
        let mut t = adopted();
        for _ in 0..3 {
            assert!(matches!(t.open(50, PROC_A), OpenOutcome::Opened(_)));
        }
        assert!(matches!(t.open(50, PROC_B), OpenOutcome::Opened(_)));
        assert_eq!(t.opens(50), 4);
        assert_eq!(t.open_refs_total(), 4);
        assert_eq!(t.opens.len(), 2, "one row per (resource, process)");
        // Closing is per process too: B cannot close A's opens.
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Kept);
        assert_eq!(t.close(50, PROC_B), CloseOutcome::NoRow);
        assert_eq!(t.opens(50), 3);
        assert_eq!(t.counters().close_missed, 1);
    }

    #[test]
    fn the_usual_order_closes_every_open_then_destroys_and_releases_once() {
        let mut t = adopted();
        let _ = t.open(50, PROC_A); // the creating device's own open
        let _ = t.open(50, PROC_B); // DWM
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Kept);
        assert_eq!(t.close(50, PROC_A), CloseOutcome::Kept);
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Release);
        // A second destroy of the same allocation releases nothing.
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Repeat);
        assert_eq!(t.counters().deferred, 0);
        assert!(t.remove(50).is_some());
    }

    #[test]
    fn a_destroy_with_an_opener_alive_defers_and_the_last_close_releases() {
        let mut t = adopted();
        let _ = t.open(50, PROC_A);
        let _ = t.open(50, PROC_B);
        // The creator's allocation is destroyed while DWM still has it open.
        assert_eq!(
            t.allocation_destroyed(50),
            DestroyOutcome::Deferred { opens: 2 }
        );
        assert_eq!(t.orphans(), 1);
        assert_eq!(t.counters().deferred, 1);
        // A new open of a destroyed allocation is refused: nothing may take a
        // new reference on a resource whose release is pending.
        assert_eq!(
            t.open(50, PROC_C),
            OpenOutcome::Refused(OpenRefusal::Destroyed)
        );
        assert_eq!(t.close(50, PROC_A), CloseOutcome::Kept);
        assert_eq!(t.orphans(), 1);
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Release);
        assert_eq!(t.orphans(), 0);
        assert_eq!(t.counters().deferred_released, 1);
        // The caller removes the record; nothing releases it twice.
        assert!(t.remove(50).is_some());
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Gone);
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Proceed);
    }

    #[test]
    fn a_second_destroy_while_deferred_does_not_release() {
        let mut t = adopted();
        let _ = t.open(50, PROC_B);
        assert!(matches!(
            t.allocation_destroyed(50),
            DestroyOutcome::Deferred { .. }
        ));
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Repeat);
        assert_eq!(t.counters().deferred, 1);
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Release);
    }

    #[test]
    fn a_close_that_finds_no_row_never_releases() {
        let mut t = adopted();
        let _ = t.open(50, PROC_A);
        assert!(matches!(
            t.allocation_destroyed(50),
            DestroyOutcome::Deferred { .. }
        ));
        // A stray close by a process that never opened it: counted, ignored, and
        // above all not the "last close".
        assert_eq!(t.close(50, PROC_C), CloseOutcome::NoRow);
        assert_eq!(t.opens(50), 1);
        assert_eq!(t.close(50, PROC_A), CloseOutcome::Release);
        // After the last close the row is gone: a repeated close is not a release.
        assert_eq!(t.close(50, PROC_A), CloseOutcome::NoRow);
    }

    #[test]
    fn a_destroy_of_an_unadopted_or_unknown_resource_is_the_legacy_teardown() {
        let mut t = with_import();
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Proceed);
        assert!(!t.get(50).unwrap().destroyed);
        assert_eq!(t.allocation_destroyed(77), DestroyOutcome::Proceed);
    }

    #[test]
    fn removal_drops_the_open_rows_with_the_record() {
        let mut t = adopted();
        let _ = t.open(50, PROC_A);
        let _ = t.open(50, PROC_B);
        assert!(t.remove(50).is_some());
        assert_eq!(t.opens(50), 0);
        assert_eq!(t.opens.len(), 0);
        // The sweep got there first: the opener's later close is "gone".
        assert_eq!(t.close(50, PROC_A), CloseOutcome::Gone);
        assert_eq!(t.open(50, PROC_A), OpenOutcome::NotForeign);
    }

    #[test]
    fn the_open_table_is_bounded_and_never_grows() {
        let mut t = ForeignTable::with_limits_and_rows(
            Limits {
                total: 4,
                per_owner: 2,
                bytes_per_owner: 64 * MIB,
            },
            3,
        );
        let r = t.reserve(1, 8 * MIB).unwrap();
        t.commit(r, 50, 7, 3, 9, lay()).unwrap();
        assert!(t.adopt(50));
        let cap = t.opens.capacity();
        assert!(cap >= 3);
        for p in [PROC_A, PROC_B, PROC_C] {
            assert!(matches!(t.open(50, p), OpenOutcome::Opened(_)));
        }
        assert_eq!(t.open(50, 0x4000), OpenOutcome::Refused(OpenRefusal::Rows));
        // An existing process still can: it adds a count, not a row.
        assert!(matches!(t.open(50, PROC_A), OpenOutcome::Opened(_)));
        assert_eq!(t.opens.capacity(), cap);
        assert_eq!(t.counters().refused_open, 1);
        // Closing the last open of a row frees it for another process.
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Kept);
        assert!(matches!(t.open(50, 0x4000), OpenOutcome::Opened(_)));
    }

    #[test]
    fn open_refusals_are_in_the_total_and_have_distinct_codes() {
        let mut t = with_import();
        let _ = t.open(50, PROC_A);
        assert_eq!(t.counters().refused(), 1);
        let codes = [
            OpenRefusal::NotAdopted.code(),
            OpenRefusal::Destroyed.code(),
            OpenRefusal::Rows.code(),
        ];
        assert!(codes.iter().all(|&c| c != 0));
        assert!(codes[0] != codes[1] && codes[1] != codes[2] && codes[0] != codes[2]);
    }

    #[test]
    fn an_attach_is_sanctioned_for_the_creator_and_for_openers_only() {
        let mut t = with_import();
        // Before adoption the importing device attaches (its own other contexts).
        assert_eq!(t.note_attach(50, 1, PROC_A), AttachOutcome::Sanctioned);
        assert_eq!(t.note_attach(50, 2, PROC_B), AttachOutcome::Unsanctioned);
        assert!(t.adopt(50));
        // After adoption the creator is gone from the record, so the creating
        // device's own process needs its own open (dxgkrnl makes one).
        assert_eq!(t.note_attach(50, 1, PROC_A), AttachOutcome::Unsanctioned);
        let _ = t.open(50, PROC_A);
        let _ = t.open(50, PROC_B);
        assert_eq!(t.note_attach(50, 1, PROC_A), AttachOutcome::Sanctioned);
        assert_eq!(t.note_attach(50, 9, PROC_B), AttachOutcome::Sanctioned);
        assert_eq!(t.note_attach(50, 9, PROC_C), AttachOutcome::Unsanctioned);
        // A caller with no device token and no process is never sanctioned.
        assert_eq!(t.note_attach(50, 0, 0), AttachOutcome::Unsanctioned);
        // Not a foreign resource: untouched, uncounted.
        assert_eq!(t.note_attach(999, 1, PROC_A), AttachOutcome::NotForeign);
        let c = t.counters();
        assert_eq!((c.attached, c.attached_unsanctioned), (7, 4));
        // A closed open stops sanctioning.
        assert_eq!(t.close(50, PROC_B), CloseOutcome::Kept);
        assert_eq!(t.note_attach(50, 9, PROC_B), AttachOutcome::Unsanctioned);
    }

    /// The state machine against a reference model, over a long pseudo-random
    /// interleaving of opens, closes and destroys of a few resources by a few
    /// processes. The invariants the KMD relies on:
    ///   * a `Release` never happens while the model has an open;
    ///   * a `Release` never happens before the destroy;
    ///   * every resource is released exactly once, as soon as it has been
    ///     destroyed and its opens have drained (never later, never twice).
    #[test]
    fn release_happens_exactly_once_after_the_last_of_destroy_and_close() {
        const RES: usize = 4;
        const PROCS: u64 = 3;
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _round in 0..200 {
            let mut t = ForeignTable::with_limits(Limits {
                total: 8,
                per_owner: 8,
                bytes_per_owner: 64 * MIB,
            });
            for i in 0..RES as u32 {
                let r = t.reserve(1, MIB).unwrap();
                t.commit(r, 100 + i, 7, 3, 9 + i, lay_for(MIB)).unwrap();
                assert!(t.adopt(100 + i));
            }
            // Model: open counts per (resource, process), destroyed, releases.
            let mut model_opens = [[0u32; PROCS as usize]; RES];
            let mut destroyed = [false; RES];
            let mut releases = [0u32; RES];
            for _step in 0..60 {
                let r = (next() % RES as u64) as usize;
                let p = (next() % PROCS) as usize;
                let id = 100 + r as u32;
                let proc_id = 0x1000 * (p as u64 + 1);
                let total: u32 = model_opens[r].iter().sum();
                match next() % 4 {
                    0 | 1 => {
                        let out = t.open(id, proc_id);
                        if destroyed[r] {
                            assert_eq!(out, OpenOutcome::Refused(OpenRefusal::Destroyed));
                        } else {
                            assert!(matches!(out, OpenOutcome::Opened(_)));
                            model_opens[r][p] += 1;
                        }
                    }
                    2 => {
                        let out = t.close(id, proc_id);
                        if model_opens[r][p] == 0 {
                            assert!(matches!(out, CloseOutcome::NoRow | CloseOutcome::Gone));
                        } else {
                            model_opens[r][p] -= 1;
                            let left: u32 = model_opens[r].iter().sum();
                            if destroyed[r] && left == 0 {
                                assert_eq!(out, CloseOutcome::Release);
                                releases[r] += 1;
                            } else {
                                assert_eq!(out, CloseOutcome::Kept);
                            }
                        }
                    }
                    _ => {
                        let out = t.allocation_destroyed(id);
                        if destroyed[r] {
                            assert!(matches!(
                                out,
                                DestroyOutcome::Repeat | DestroyOutcome::Proceed
                            ));
                        } else {
                            destroyed[r] = true;
                            if total == 0 {
                                assert_eq!(out, DestroyOutcome::Release);
                                releases[r] += 1;
                            } else {
                                assert_eq!(out, DestroyOutcome::Deferred { opens: total });
                            }
                        }
                    }
                }
                for k in 0..RES {
                    assert!(releases[k] <= 1, "released twice");
                    assert_eq!(t.opens(100 + k as u32), model_opens[k].iter().sum::<u32>());
                    if releases[k] == 1 {
                        // Released implies destroyed and drained.
                        assert!(destroyed[k]);
                        assert_eq!(model_opens[k].iter().sum::<u32>(), 0);
                    }
                }
            }
            // Drain: close everything, destroy what is left. Every resource ends
            // with exactly one release.
            for r in 0..RES {
                let id = 100 + r as u32;
                for p in 0..PROCS as usize {
                    while model_opens[r][p] > 0 {
                        let out = t.close(id, 0x1000 * (p as u64 + 1));
                        model_opens[r][p] -= 1;
                        let left: u32 = model_opens[r].iter().sum();
                        if destroyed[r] && left == 0 {
                            assert_eq!(out, CloseOutcome::Release);
                            releases[r] += 1;
                        } else {
                            assert_eq!(out, CloseOutcome::Kept);
                        }
                    }
                }
                if !destroyed[r] {
                    assert_eq!(t.allocation_destroyed(id), DestroyOutcome::Release);
                    releases[r] += 1;
                }
                assert_eq!(releases[r], 1, "resource {r}");
            }
            assert_eq!(t.orphans(), 0);
            assert_eq!(t.open_refs_total(), 0);
        }
    }

    /// A layout that fits `size` bytes (1080p linear needs 0x7e9000).
    fn lay_for(size: u64) -> Layout {
        let l = Layout {
            width: 64,
            height: 64,
            stride: 256,
            ..lay()
        };
        assert!(l.min_bytes() <= size);
        l
    }

    /// The KMD's own owner token (`DeviceOwner::KMD_RM`, `usize::MAX` widened) is just an
    /// owner to the table: quota per owner, adoption frees it, the record stays.
    #[test]
    fn a_resource_the_kmd_created_is_adopted_like_any_creators() {
        const KMD: u64 = u64::MAX;
        let mut t = small();
        let r = t.reserve(KMD, 8 * MIB).unwrap();
        t.commit(r, 50, 7, 3, 9, lay()).unwrap();
        assert_eq!(t.owner_live(KMD), 1);
        assert_eq!(t.owner_live(1), 0, "a user device's count is not the KMD's");
        assert_eq!(t.get(50).unwrap().creator, Some(KMD));
        // The glue proves the holder context and the KMD-owned slot, then the table adopts.
        assert_eq!(
            adopt(&mut t, &req()),
            Ok(AdoptPlan::Foreign(Adopted {
                size: 8 * MIB,
                layout: lay()
            }))
        );
        assert_eq!(t.owner_live(KMD), 0);
        assert_eq!(t.owner_bytes(KMD), 0);
        assert_eq!(t.layout(50), Some(lay()));
        // Without the proofs it is refused and nothing moves.
        let mut t = small();
        let r = t.reserve(KMD, 8 * MIB).unwrap();
        t.commit(r, 50, 7, 3, 9, lay()).unwrap();
        assert!(t.adopt_for_allocation(50, &req(), false, true).is_err());
        assert!(t.adopt_for_allocation(50, &req(), true, false).is_err());
        assert_eq!(t.owner_live(KMD), 1);
        // The KMD's quota is its own: filling it does not touch a device's.
        let per_owner = t.limits().per_owner;
        let mut t = small();
        for _ in 0..per_owner {
            assert!(t.reserve(KMD, MIB).is_ok());
        }
        assert!(t.reserve(KMD, MIB).is_err());
    }

    // ---- KmdRmClient = 5: the KMD's own RM system memory has a CPU view ------------

    #[test]
    fn only_the_kmds_own_sysmem_is_mappable_and_only_after_the_service_marked_it() {
        let mut t = with_import(); // owner 1 imported resource 50
        assert!(t.contains(50));
        assert!(!t.cpu_mappable(50), "an import is unmappable by default");
        assert!(
            !t.cpu_mappable(51),
            "a resource that is not foreign is not ours to say"
        );
        // Another owner cannot mark it, nor can a dead id be marked.
        assert!(!t.mark_sysmem(50, 2));
        assert!(!t.mark_sysmem(99, 1));
        assert!(!t.cpu_mappable(50));
        assert!(t.mark_sysmem(50, 1));
        assert!(t.cpu_mappable(50));
        // Not a flip source until a WDDM allocation owns it.
        assert_eq!(t.sysmem_source(50), None);
        assert!(adopt(&mut t, &req()).is_ok());
        let (rm, gem, layout, size) = t.sysmem_source(50).unwrap();
        assert_eq!(layout, lay());
        assert_eq!(size, 8 * MIB);
        assert_eq!(
            (rm, gem),
            (t.get(50).unwrap().rm_handle, t.get(50).unwrap().gem_handle)
        );
        // An adopted resource can no longer be marked (the creator is gone).
        assert!(!t.mark_sysmem(50, 1));
        // The destroy ends it as a source and removes the record.
        assert_eq!(t.allocation_destroyed(50), DestroyOutcome::Release);
        assert_eq!(t.sysmem_source(50), None);
        assert!(t.remove(50).is_some());
        assert!(!t.cpu_mappable(50));
    }

    #[test]
    fn an_unmarked_adopted_resource_is_never_a_sysmem_source() {
        let mut t = with_import();
        assert!(adopt(&mut t, &req()).is_ok());
        assert_eq!(t.sysmem_source(50), None);
        assert!(!t.cpu_mappable(50));
    }
}

/// The shared formats (`docs/shared-formats.md`): one test per rule of section 5.
#[cfg(test)]
mod shared_format_tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const MIB: u64 = 1 << 20;
    const BL: u64 = MOD_NVIDIA_BLOCK_LINEAR_BASE;

    /// (fourcc, name, bytes per texel of plane 0, texel columns per pixel divisor)
    /// for every one-plane format of the table, the four 32-bit RGB ones first.
    const ONE_PLANE: [(u32, &str, u32, u32); 15] = [
        (FOURCC_XRGB8888, "XRGB8888", 4, 1),
        (FOURCC_ARGB8888, "ARGB8888", 4, 1),
        (FOURCC_XBGR8888, "XBGR8888", 4, 1),
        (FOURCC_ABGR8888, "ABGR8888", 4, 1),
        (FOURCC_R8, "R8", 1, 1),
        (FOURCC_GR88, "GR88", 2, 1),
        (FOURCC_R16, "R16", 2, 1),
        (FOURCC_GR1616, "GR1616", 4, 1),
        (FOURCC_RGB565, "RGB565", 2, 1),
        (FOURCC_ARGB1555, "ARGB1555", 2, 1),
        (FOURCC_ARGB4444, "ARGB4444", 2, 1),
        (FOURCC_ABGR2101010, "ABGR2101010", 4, 1),
        (FOURCC_ABGR16161616F, "ABGR16161616F", 8, 1),
        (FOURCC_ABGR16161616, "ABGR16161616", 8, 1),
        (FOURCC_YUYV, "YUYV", 4, 2),
    ];
    /// (fourcc, name, bytes per texel of plane 0); plane 1 is twice that.
    const TWO_PLANE: [(u32, &str, u32); 3] = [
        (FOURCC_NV12, "NV12", 1),
        (FOURCC_P010, "P010", 2),
        (FOURCC_P016, "P016", 2),
    ];
    /// Fourccs that are real DRM formats nobody shares here, and zero.
    const UNSHARED: [u32; 6] = [
        0,
        0x3432_4742,     // 'BG24' BGR888
        0x3033_5258,     // 'XR30' XRGB2101010
        0x3231_5659,     // 'YV12' (three planes)
        0x3231_564e + 1, // not a fourcc at all
        u32::MAX,
    ];

    fn rup(x: u64, to: u64) -> u64 {
        x.div_ceil(to) * to
    }

    fn row0(bpp: u32, hdiv: u32, w: u32) -> u32 {
        (w.div_ceil(hdiv) * bpp) as u32
    }

    fn one(fourcc: u32, w: u32, h: u32, stride: u32) -> Layout {
        Layout {
            width: w,
            height: h,
            stride,
            offset: 0,
            fourcc,
            modifier: MOD_LINEAR,
            plane1: None,
        }
    }

    /// A two-plane layout packed back to back, LINEAR, plane 1 at plane 0's end.
    fn two(fourcc: u32, w: u32, h: u32) -> Layout {
        let (_, _, bpp) = *TWO_PLANE.iter().find(|t| t.0 == fourcc).unwrap();
        let s0 = w * bpp;
        let s1 = w.div_ceil(2) * bpp * 2;
        Layout {
            width: w,
            height: h,
            stride: s0,
            offset: 0,
            fourcc,
            modifier: MOD_LINEAR,
            plane1: Some(Plane {
                stride: s1,
                offset: s0 * h,
                modifier: MOD_LINEAR,
            }),
        }
    }

    /// `l` with plane 1 given block-linear moduli `h0`/`h1`, offset kept past plane 0.
    fn two_bl(fourcc: u32, w: u32, h: u32, h0: u64, h1: u64) -> Layout {
        let mut l = two(fourcc, w, h);
        let (_, _, bpp) = *TWO_PLANE.iter().find(|t| t.0 == fourcc).unwrap();
        l.modifier = gb20x_family(bpp) | h0;
        let off = l.plane0_min_bytes();
        let p = l.plane1.as_mut().unwrap();
        p.modifier = gb20x_family(bpp * 2) | h1;
        p.offset = off as u32;
        l
    }

    // ---- the table -----------------------------------------------------------------

    #[test]
    fn the_table_is_the_documented_one() {
        // fourcc, planes, bpp0, bpp1, hdiv0, even width, even height: section 2 of the doc.
        let rows: [(u32, (u32, u32, u32, u32, bool, bool)); 18] = [
            (0x3432_5258, (1, 4, 0, 1, false, false)),
            (0x3432_5241, (1, 4, 0, 1, false, false)),
            (0x3432_4258, (1, 4, 0, 1, false, false)),
            (0x3432_4241, (1, 4, 0, 1, false, false)),
            (0x2020_3852, (1, 1, 0, 1, false, false)),
            (0x3838_5247, (1, 2, 0, 1, false, false)),
            (0x2036_3152, (1, 2, 0, 1, false, false)),
            (0x3233_5247, (1, 4, 0, 1, false, false)),
            (0x3631_4752, (1, 2, 0, 1, false, false)),
            (0x3531_5241, (1, 2, 0, 1, false, false)),
            (0x3231_5241, (1, 2, 0, 1, false, false)),
            (0x3033_4241, (1, 4, 0, 1, false, false)),
            (0x4834_4241, (1, 8, 0, 1, false, false)),
            (0x3834_4241, (1, 8, 0, 1, false, false)),
            (0x5659_5559, (1, 4, 0, 2, true, false)),
            (0x3231_564E, (2, 1, 2, 1, true, true)),
            (0x3031_3050, (2, 2, 4, 1, true, true)),
            (0x3631_3050, (2, 2, 4, 1, true, true)),
        ];
        for (fourcc, (planes, bpp0, bpp1, hdiv0, ew, eh)) in rows {
            let f = share_format(fourcc).unwrap_or_else(|| panic!("{fourcc:#x} missing"));
            assert_eq!(
                (
                    f.planes,
                    f.bpp0,
                    f.bpp1,
                    f.hdiv0,
                    f.even_width,
                    f.even_height
                ),
                (planes, bpp0, bpp1, hdiv0, ew, eh),
                "{fourcc:#x}"
            );
        }
        // The constants spell the DRM fourccs.
        let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
        assert_eq!(FOURCC_R8, cc(b"R8  "));
        assert_eq!(FOURCC_GR88, cc(b"GR88"));
        assert_eq!(FOURCC_R16, cc(b"R16 "));
        assert_eq!(FOURCC_GR1616, cc(b"GR32"));
        assert_eq!(FOURCC_RGB565, cc(b"RG16"));
        assert_eq!(FOURCC_ARGB1555, cc(b"AR15"));
        assert_eq!(FOURCC_ARGB4444, cc(b"AR12"));
        assert_eq!(FOURCC_ABGR2101010, cc(b"AB30"));
        assert_eq!(FOURCC_ABGR16161616F, cc(b"AB4H"));
        assert_eq!(FOURCC_ABGR16161616, cc(b"AB48"));
        assert_eq!(FOURCC_YUYV, cc(b"YUYV"));
        assert_eq!(FOURCC_NV12, cc(b"NV12"));
        assert_eq!(FOURCC_P010, cc(b"P010"));
        assert_eq!(FOURCC_P016, cc(b"P016"));
        // Nothing else is in it, and the unshared ones have no row.
        for f in UNSHARED {
            assert_eq!(share_format(f), None, "{f:#x}");
        }
        let known = ONE_PLANE.len() + TWO_PLANE.len();
        assert_eq!(known, rows.len());
        // The two lists above cover exactly the table.
        let mut seen: Vec<u32> = ONE_PLANE.iter().map(|t| t.0).collect();
        seen.extend(TWO_PLANE.iter().map(|t| t.0));
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), known);
        for (f, _) in rows {
            assert!(seen.contains(&f));
        }
    }

    #[test]
    fn share_format_helpers_follow_the_doc() {
        let yuyv = share_format(FOURCC_YUYV).unwrap();
        assert_eq!(yuyv.row_bytes(0, 1920), 3840); // ceil(w/2) * 4
        assert_eq!(yuyv.row_bytes(0, 2), 4);
        let nv = share_format(FOURCC_NV12).unwrap();
        assert_eq!(nv.row_bytes(0, 1920), 1920);
        assert_eq!(nv.row_bytes(1, 1920), 1920); // 960 chroma pairs of 2 bytes
        assert_eq!(nv.rows(1, 1080), 540);
        assert_eq!(nv.rows(1, 1), 1); // ceil
        assert_eq!((nv.stride_align(0), nv.stride_align(1)), (1, 2));
        let p = share_format(FOURCC_P010).unwrap();
        assert_eq!(p.row_bytes(1, 1920), 3840);
        assert_eq!((p.stride_align(0), p.stride_align(1)), (2, 4));
        // The alignment never exceeds 4, whatever the texel size.
        for f in [FOURCC_ABGR16161616F, FOURCC_ABGR16161616] {
            assert_eq!(share_format(f).unwrap().stride_align(0), 4);
        }
        assert_eq!(share_format(FOURCC_R8).unwrap().stride_align(0), 1);
        assert!(is_rgb32_fourcc(FOURCC_XRGB8888) && !is_rgb32_fourcc(FOURCC_R8));
        assert!(!is_rgb32_fourcc(0));
    }

    // ---- one-plane formats -------------------------------------------------------

    #[test]
    fn every_one_plane_format_is_valid_tight_padded_and_at_its_limits() {
        for (f, name, bpp, hdiv) in ONE_PLANE {
            let dims: &[(u32, u32)] = if hdiv == 2 {
                &[(2, 1), (2, 2), (64, 63), (1920, 1080), (16384, 16384)]
            } else {
                &[
                    (1, 1),
                    (3, 5),
                    (64, 64),
                    (1919, 1079),
                    (1920, 1080),
                    (16384, 16384),
                ]
            };
            for &(w, h) in dims {
                let row = row0(bpp, hdiv, w);
                let l = one(f, w, h, row);
                assert_eq!(l.validate(), Ok(()), "{name} {w}x{h}");
                assert_eq!(l.min_bytes(), u64::from(row) * u64::from(h), "{name}");
                assert_eq!(l.validate_for(l.min_bytes()), Ok(()), "{name}");
                assert_eq!(
                    l.validate_for(l.min_bytes() - 1),
                    Err(LayoutError::TooLarge),
                    "{name}"
                );
                // Padding past the row, at the format's alignment, is fine.
                let align = bpp.min(4);
                let padded = one(f, w, h, row + 64 * align);
                if padded.stride <= MAX_STRIDE {
                    assert_eq!(padded.validate(), Ok(()), "{name} padded");
                }
                // An offset moves the bound.
                let moved = Layout {
                    offset: 0x1000,
                    ..l
                };
                assert_eq!(moved.min_bytes(), l.min_bytes() + 0x1000);
            }
        }
    }

    #[test]
    fn one_plane_stride_rules_per_format() {
        for (f, name, bpp, hdiv) in ONE_PLANE {
            let w = if hdiv == 2 { 1920 } else { 1919 };
            let row = row0(bpp, hdiv, w);
            let align = bpp.min(4);
            // One byte under the row, or one alignment under it.
            assert_eq!(
                one(f, w, 8, row - 1).validate(),
                Err(LayoutError::Stride),
                "{name} under"
            );
            assert_eq!(
                one(f, w, 8, row - align).validate(),
                Err(LayoutError::Stride),
                "{name} under by one unit"
            );
            // Off the alignment (only formats with texels over a byte have one). The row
            // itself is on it, so row + 1 is off it.
            if align > 1 {
                assert_eq!(
                    one(f, w, 8, row + 1).validate(),
                    Err(LayoutError::Stride),
                    "{name} misaligned"
                );
                assert_eq!(
                    one(f, w, 8, row + align - 1).validate(),
                    Err(LayoutError::Stride),
                    "{name} misaligned by align-1"
                );
            } else {
                assert_eq!(
                    one(f, w, 8, row + 1).validate(),
                    Ok(()),
                    "{name} byte stride"
                );
            }
            // The cap is on the stride, not the row.
            let big = MAX_STRIDE; // a multiple of every alignment
            assert_eq!(one(f, 64, 8, big).validate(), Ok(()), "{name} at cap");
            assert_eq!(
                one(f, 64, 8, big + align).validate(),
                Err(LayoutError::Stride),
                "{name} over cap"
            );
            assert_eq!(
                one(f, 64, 8, u32::MAX - (u32::MAX % align)).validate(),
                Err(LayoutError::Stride),
                "{name} u32 max"
            );
            assert_eq!(
                one(f, 64, 8, 0).validate(),
                Err(LayoutError::Stride),
                "{name} 0"
            );
        }
    }

    #[test]
    fn the_documented_strides_of_the_doc_examples() {
        // R8 1920x1080: a stride of 1920 is enough (1-byte texels, no alignment).
        assert_eq!(one(FOURCC_R8, 1920, 1080, 1920).validate(), Ok(()));
        assert_eq!(
            one(FOURCC_R8, 1920, 1080, 1919).validate(),
            Err(LayoutError::Stride)
        );
        assert_eq!(one(FOURCC_R8, 1920, 1080, 1921).validate(), Ok(()));
        // fp16 stride 8 * w.
        assert_eq!(
            one(FOURCC_ABGR16161616F, 1920, 1080, 8 * 1920).validate(),
            Ok(())
        );
        assert_eq!(
            one(FOURCC_ABGR16161616F, 1920, 1080, 8 * 1920 - 4).validate(),
            Err(LayoutError::Stride)
        );
        // The 32 bpp ones keep their `width * 4`, multiple of 4.
        assert_eq!(one(FOURCC_ARGB8888, 1920, 1080, 7680).validate(), Ok(()));
        assert_eq!(
            one(FOURCC_ARGB8888, 1920, 1080, 7676).validate(),
            Err(LayoutError::Stride)
        );
        // YUYV row is ceil(w/2) * 4.
        assert_eq!(one(FOURCC_YUYV, 1920, 1080, 3840).validate(), Ok(()));
        assert_eq!(
            one(FOURCC_YUYV, 1920, 1080, 3836).validate(),
            Err(LayoutError::Stride)
        );
    }

    #[test]
    fn extent_rules_per_format() {
        for (f, name, bpp, hdiv) in ONE_PLANE {
            let ok = |w: u32, h: u32| one(f, w, h, row0(bpp, hdiv, w.max(2)).max(1));
            // Zero and over-limit extents, both axes.
            assert_eq!(
                Layout {
                    width: 0,
                    ..ok(2, 2)
                }
                .validate(),
                Err(LayoutError::Dimensions),
                "{name}"
            );
            assert_eq!(
                Layout {
                    height: 0,
                    ..ok(2, 2)
                }
                .validate(),
                Err(LayoutError::Dimensions),
                "{name}"
            );
            assert_eq!(
                Layout {
                    width: MAX_DIM + 1,
                    stride: MAX_STRIDE,
                    ..ok(2, 2)
                }
                .validate(),
                Err(LayoutError::Dimensions),
                "{name}"
            );
            assert_eq!(
                Layout {
                    height: MAX_DIM + 1,
                    ..ok(2, 2)
                }
                .validate(),
                Err(LayoutError::Dimensions),
                "{name}"
            );
            assert_eq!(
                Layout {
                    width: u32::MAX,
                    height: u32::MAX,
                    ..ok(2, 2)
                }
                .validate(),
                Err(LayoutError::Dimensions),
                "{name}"
            );
            // Odd width is refused only where the format subsamples horizontally
            // (YUYV); odd height by none of the one-plane formats.
            let odd_w = one(f, 3, 2, row0(bpp, hdiv, 3));
            if hdiv == 2 {
                assert_eq!(
                    odd_w.validate(),
                    Err(LayoutError::Dimensions),
                    "{name} odd w"
                );
            } else {
                assert_eq!(odd_w.validate(), Ok(()), "{name} odd w");
            }
            assert_eq!(
                one(f, 4, 3, row0(bpp, hdiv, 4)).validate(),
                Ok(()),
                "{name} odd h"
            );
            // Exactly the limits are in.
            assert_eq!(
                one(f, MAX_DIM, MAX_DIM, row0(bpp, hdiv, MAX_DIM)).validate(),
                Ok(()),
                "{name} 16384"
            );
            assert_eq!(
                one(f, 2, 1, row0(bpp, hdiv, 2)).validate(),
                Ok(()),
                "{name} 2x1"
            );
        }
    }

    #[test]
    fn one_plane_formats_refuse_a_plane_1() {
        // Spec test: a plane tail on a one-plane fourcc is `Planes`, and it is checked
        // before the plane's own fields (a plane that would be valid on NV12).
        let p = Plane {
            stride: 4096,
            offset: 0x100_0000,
            modifier: MOD_LINEAR,
        };
        for (f, name, bpp, hdiv) in ONE_PLANE {
            let l = Layout {
                plane1: Some(p),
                ..one(f, 64, 64, row0(bpp, hdiv, 64))
            };
            assert_eq!(l.validate(), Err(LayoutError::Planes), "{name}");
            assert_eq!(l.validate_for(u64::MAX), Err(LayoutError::Planes), "{name}");
        }
    }

    // ---- block-linear, one plane ---------------------------------------------------

    #[test]
    fn block_linear_heights_round_rows_to_the_block_for_every_format() {
        for (f, name, bpp, hdiv) in ONE_PLANE {
            for &(w, h) in &[(64u32, 1080u32), (128, 1), (130, 257), (128, 8), (128, 256)] {
                let w = if hdiv == 2 { w & !1 } else { w };
                let row = row0(bpp, hdiv, w);
                let fam = gb20x_family(bpp);
                for hh in 0..=5u64 {
                    let l = Layout {
                        modifier: fam | hh,
                        ..one(f, w, h, row)
                    };
                    assert_eq!(l.validate(), Ok(()), "{name} h{hh}");
                    assert_eq!(l.block_height_log2(), Some(hh as u32));
                    let block = 8u64 << hh;
                    let want = u64::from(row) * rup(u64::from(h), block);
                    assert_eq!(l.min_bytes(), want, "{name} {w}x{h} h{hh}");
                    assert_eq!(l.validate_for(want), Ok(()));
                    assert_eq!(l.validate_for(want - 1), Err(LayoutError::TooLarge));
                }
                // h = 6 and the neighbours of the family are refused.
                for m in [
                    fam | 6,
                    fam - 1,
                    fam + 6,
                    1,
                    0x0100_0000_0000_0001,
                    u64::MAX,
                ] {
                    assert_eq!(
                        Layout {
                            modifier: m,
                            ..one(f, w, h, row)
                        }
                        .validate(),
                        Err(LayoutError::Modifier),
                        "{name} {m:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn r8_1080p_linear_and_block_linear() {
        // The doc's first spec test: A8_UNORM shell surfaces.
        let lin = one(FOURCC_R8, 1920, 1080, 1920);
        assert_eq!(lin.min_bytes(), 1920 * 1080);
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT, 4 * MIB, Some(lin)),
            Ok(lin)
        );
        // 2_073_600 bytes is not a page multiple: 0x1fb000 is the first page multiple
        // that holds it, and RM (64 KiB granularity) would hand 0x200000.
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT, 0x1f_a000, Some(lin)),
            Err(RequestError::Layout(LayoutError::TooLarge)),
            "0x1fa000 is under 2073600"
        );
        for size in [0x1f_b000, 0x20_0000] {
            assert_eq!(
                validate_request(1, 2, 3, FLAG_LAYOUT, size, Some(lin)),
                Ok(lin)
            );
        }
        for hh in 0..=5u64 {
            let bl = Layout {
                modifier: MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP | hh,
                ..lin
            };
            let rows = rup(1080, 8 << hh);
            assert_eq!(bl.min_bytes(), 1920 * rows);
            let size = rup(bl.min_bytes(), 0x1_0000);
            assert_eq!(
                validate_request(1, 2, 3, FLAG_LAYOUT, size, Some(bl)),
                Ok(bl)
            );
        }
    }

    // ---- two-plane formats ---------------------------------------------------------

    #[test]
    fn nv12_1080p_plane_1_at_the_end_of_plane_0() {
        let l = two(FOURCC_NV12, 1920, 1080);
        let p1 = l.plane1.unwrap();
        assert_eq!((l.stride, p1.stride), (1920, 1920));
        assert_eq!(p1.offset as u64, l.plane0_min_bytes());
        assert_eq!(p1.offset, 1920 * 1080);
        assert_eq!(l.plane0_min_bytes(), 2_073_600);
        assert_eq!(l.plane1_min_bytes(), Some(2_073_600 + 1920 * 540));
        assert_eq!(l.min_bytes(), 3_110_400);
        assert_eq!(l.plane_count(), 2);
        assert!(!l.is_rgb32());
        assert_eq!(l.validate(), Ok(()));
        assert_eq!(l.validate_for(3_110_400), Ok(()));
        assert_eq!(l.validate_for(3_110_399), Err(LayoutError::TooLarge));
        // Plane 0's bound alone is not enough: plane 1 is past it.
        assert_eq!(l.validate_for(2_073_600), Err(LayoutError::TooLarge));
        // Through the request, with the flag.
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT | FLAG_PLANE1, 0x2f_8000, Some(l)),
            Ok(l)
        );
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT | FLAG_PLANE1, 0x2f_7000, Some(l)),
            Err(RequestError::Layout(LayoutError::TooLarge))
        );
    }

    #[test]
    fn p010_and_p016_1080p() {
        for f in [FOURCC_P010, FOURCC_P016] {
            let l = two(f, 1920, 1080);
            let p1 = l.plane1.unwrap();
            assert_eq!((l.stride, p1.stride), (3840, 3840));
            assert_eq!(l.min_bytes(), 3840 * 1080 + 3840 * 540);
            assert_eq!(l.validate(), Ok(()));
            assert_eq!(l.validate_for(l.min_bytes()), Ok(()));
            assert_eq!(
                l.validate_for(l.min_bytes() - 1),
                Err(LayoutError::TooLarge)
            );
        }
    }

    #[test]
    fn two_plane_formats_across_extents() {
        for (f, name, _) in TWO_PLANE {
            for &(w, h) in &[
                (2u32, 2u32),
                (2, 4),
                (4, 2),
                (64, 64),
                (1280, 720),
                (3840, 2160),
                (16384, 16384),
            ] {
                let l = two(f, w, h);
                assert_eq!(l.validate(), Ok(()), "{name} {w}x{h}");
                let p1 = l.plane1.unwrap();
                let sh = share_format(f).unwrap();
                assert_eq!(u64::from(l.stride), sh.row_bytes(0, w));
                assert_eq!(u64::from(p1.stride), sh.row_bytes(1, w));
                assert_eq!(
                    l.min_bytes(),
                    u64::from(l.stride) * u64::from(h)
                        + u64::from(p1.stride) * u64::from(sh.rows(1, h)),
                    "{name} {w}x{h}"
                );
            }
        }
    }

    #[test]
    fn two_plane_extent_must_be_even_on_both_axes() {
        for (f, name, _) in TWO_PLANE {
            let good = two(f, 64, 64);
            for (w, h) in [(63u32, 64u32), (64, 63), (63, 63), (1, 1), (1, 2), (2, 1)] {
                let l = Layout {
                    width: w,
                    height: h,
                    ..good
                };
                assert_eq!(l.validate(), Err(LayoutError::Dimensions), "{name} {w}x{h}");
            }
            for (w, h) in [(0u32, 64u32), (64, 0), (MAX_DIM + 1, 64), (64, MAX_DIM + 1)] {
                let l = Layout {
                    width: w,
                    height: h,
                    ..good
                };
                assert_eq!(l.validate(), Err(LayoutError::Dimensions), "{name} {w}x{h}");
            }
        }
        // Plane 1's chroma extent is ceil(w/2) x ceil(h/2): an even 2x2 has 1x1 chroma.
        let l = two(FOURCC_NV12, 2, 2);
        assert_eq!(l.plane1.unwrap().stride, 2);
        assert_eq!(l.plane1_min_bytes(), Some(4 + 2));
    }

    #[test]
    fn plane_presence_must_match_the_format() {
        // Spec: NV12 without plane 1, and each one-plane format with one.
        for (f, name, _) in TWO_PLANE {
            let l = Layout {
                plane1: None,
                ..two(f, 64, 64)
            };
            assert_eq!(l.validate(), Err(LayoutError::Planes), "{name}");
            assert_eq!(l.validate_for(u64::MAX), Err(LayoutError::Planes), "{name}");
        }
        // Order of the checks: the extent and the fourcc first.
        let nv_no_plane = Layout {
            plane1: None,
            width: 0,
            ..two(FOURCC_NV12, 64, 64)
        };
        assert_eq!(nv_no_plane.validate(), Err(LayoutError::Dimensions));
        let unknown_with_plane = Layout {
            fourcc: 0,
            ..two(FOURCC_NV12, 64, 64)
        };
        assert_eq!(unknown_with_plane.validate(), Err(LayoutError::Format));
        // `Planes` before the even-extent rule: an odd NV12 with no plane is Planes.
        let odd_no_plane = Layout {
            plane1: None,
            width: 63,
            ..two(FOURCC_NV12, 64, 64)
        };
        assert_eq!(odd_no_plane.validate(), Err(LayoutError::Planes));
    }

    #[test]
    fn plane_1_stride_rules() {
        for (f, name, bpp) in TWO_PLANE {
            let good = two(f, 1920, 1080);
            let row1 = 960 * bpp * 2;
            let align1 = (bpp * 2).min(4);
            let with = |stride: u32| {
                let mut l = good;
                l.plane1.as_mut().unwrap().stride = stride;
                l
            };
            assert_eq!(with(row1).validate(), Ok(()), "{name}");
            assert_eq!(
                with(row1 + align1 * 100).validate(),
                Ok(()),
                "{name} padded"
            );
            assert_eq!(
                with(row1 - 1).validate(),
                Err(LayoutError::Stride),
                "{name}"
            );
            assert_eq!(
                with(row1 - align1).validate(),
                Err(LayoutError::Stride),
                "{name}"
            );
            assert_eq!(
                with(row1 + 1).validate(),
                Err(LayoutError::Stride),
                "{name} misaligned"
            );
            assert_eq!(with(0).validate(), Err(LayoutError::Stride), "{name}");
            assert_eq!(
                with(MAX_STRIDE + align1).validate(),
                Err(LayoutError::Stride),
                "{name}"
            );
            assert_eq!(
                with(u32::MAX).validate(),
                Err(LayoutError::Stride),
                "{name}"
            );
            // Plane 0's stride rule is its own: NV12 bytes need no alignment.
            let p0 = |stride: u32| Layout { stride, ..good };
            assert_eq!(
                p0(good.stride - 1).validate(),
                Err(LayoutError::Stride),
                "{name} p0"
            );
        }
        // NV12 plane 0 may be padded by one byte; plane 1 may not (2-byte texels).
        let nv = two(FOURCC_NV12, 1920, 1080);
        let mut padded0 = Layout { stride: 1921, ..nv };
        padded0.plane1.as_mut().unwrap().offset = 1921 * 1080;
        assert_eq!(padded0.validate(), Ok(()));
        let mut odd_p1 = nv;
        odd_p1.plane1.as_mut().unwrap().stride = 1921;
        assert_eq!(odd_p1.validate(), Err(LayoutError::Stride));
        // P010 plane 1 has 4-byte texels: 3842 is off the alignment.
        let mut p010 = two(FOURCC_P010, 1920, 1080);
        p010.plane1.as_mut().unwrap().stride = 3842;
        assert_eq!(p010.validate(), Err(LayoutError::Stride));
    }

    #[test]
    fn plane_modifiers_linear_together_and_h_may_differ() {
        for (f, name, bpp) in TWO_PLANE {
            // Every h0 / h1 pair of the family is valid, plane 1 placed past plane 0.
            for h0 in 0..=5u64 {
                for h1 in 0..=5u64 {
                    let l = two_bl(f, 1920, 1080, h0, h1);
                    assert_eq!(l.validate(), Ok(()), "{name} {h0}/{h1}");
                    let want0 = u64::from(l.stride) * rup(1080, 8 << h0);
                    assert_eq!(l.plane0_min_bytes(), want0);
                    let p1 = l.plane1.unwrap();
                    assert_eq!(p1.offset as u64, want0);
                    assert_eq!(
                        l.plane1_min_bytes(),
                        Some(want0 + u64::from(p1.stride) * rup(540, 8 << h1)),
                        "{name} {h0}/{h1}"
                    );
                    assert_eq!(l.min_bytes(), l.plane1_min_bytes().unwrap());
                }
            }
            // Mixed LINEAR / block-linear, both ways, is refused.
            let mut lin0 = two(f, 1920, 1080);
            lin0.plane1.as_mut().unwrap().modifier = gb20x_family(bpp * 2) | 2;
            assert_eq!(
                lin0.validate(),
                Err(LayoutError::Modifier),
                "{name} lin0/bl1"
            );
            let mut bl0 = two_bl(f, 1920, 1080, 3, 3);
            bl0.plane1.as_mut().unwrap().modifier = MOD_LINEAR;
            assert_eq!(
                bl0.validate(),
                Err(LayoutError::Modifier),
                "{name} bl0/lin1"
            );
            // Plane 1's modifier is held to the family like plane 0's.
            let (fam0, fam1) = (gb20x_family(bpp), gb20x_family(bpp * 2));
            for m in [fam1 | 6, fam1 - 1, 1, 0x0100_0000_0000_0001, u64::MAX] {
                let mut l = two_bl(f, 1920, 1080, 2, 2);
                l.plane1.as_mut().unwrap().modifier = m;
                assert_eq!(l.validate(), Err(LayoutError::Modifier), "{name} p1 {m:#x}");
            }
            for m in [fam0 | 6, fam0 - 1, 1, 0x0100_0000_0000_0001, u64::MAX] {
                let mut l = two(f, 1920, 1080);
                l.modifier = m;
                l.plane1.as_mut().unwrap().modifier = m;
                assert_eq!(l.validate(), Err(LayoutError::Modifier), "{name} p0 {m:#x}");
            }
        }
        // 1080p NV12 with the heights NVK would pick: 1080 rows in 128-row blocks,
        // 540 chroma rows in 64-row blocks.
        let l = two_bl(FOURCC_NV12, 1920, 1080, 4, 3);
        assert_eq!(l.plane0_min_bytes(), 1920 * 1152);
        assert_eq!(l.plane1_min_bytes(), Some(1920 * 1152 + 1920 * 576));
    }

    #[test]
    fn plane_1_may_not_overlap_plane_0() {
        for (f, name, _) in TWO_PLANE {
            let good = two(f, 1920, 1080);
            let end0 = good.plane0_min_bytes();
            let at = |off: u64| {
                let mut l = good;
                l.plane1.as_mut().unwrap().offset = off as u32;
                l
            };
            // Spec: overlap refused; the boundary and anything past it is fine.
            assert_eq!(at(end0).validate(), Ok(()), "{name}");
            assert_eq!(at(end0 + 1).validate(), Ok(()), "{name}");
            assert_eq!(at(end0 + 0x1_0000).validate(), Ok(()), "{name}");
            assert_eq!(
                at(end0 - 1).validate(),
                Err(LayoutError::TooLarge),
                "{name}"
            );
            assert_eq!(at(0).validate(), Err(LayoutError::TooLarge), "{name}");
            assert_eq!(
                at(0).validate_for(u64::MAX),
                Err(LayoutError::TooLarge),
                "{name}"
            );
            // A request with the overlap is refused as a layout fault.
            assert_eq!(
                validate_request(
                    1,
                    2,
                    3,
                    FLAG_LAYOUT | FLAG_PLANE1,
                    64 * MIB,
                    Some(at(end0 - 1))
                ),
                Err(RequestError::Layout(LayoutError::TooLarge)),
                "{name}"
            );
            // Plane 0's own offset moves its end: plane 1 at the old end now overlaps.
            let mut shifted = good;
            shifted.offset = 0x4000;
            assert_eq!(
                shifted.validate(),
                Err(LayoutError::TooLarge),
                "{name} shifted"
            );
            shifted.plane1.as_mut().unwrap().offset = (end0 + 0x4000) as u32;
            assert_eq!(shifted.validate(), Ok(()), "{name} shifted, moved");
            // Block-linear rounding counts: plane 0's end is the rounded one.
            let bl = two_bl(f, 1920, 1080, 5, 0);
            let rounded_end = u64::from(bl.stride) * 1280;
            let mut under = bl;
            under.plane1.as_mut().unwrap().offset = (rounded_end - 1) as u32;
            assert_eq!(
                under.validate(),
                Err(LayoutError::TooLarge),
                "{name} bl overlap"
            );
            let mut exact = bl;
            exact.plane1.as_mut().unwrap().offset = rounded_end as u32;
            assert_eq!(exact.validate(), Ok(()), "{name} bl boundary");
            // Plane 1's offset past the object is `TooLarge` through the size.
            let far = at(0x4000_0000);
            assert_eq!(far.validate(), Ok(()));
            assert_eq!(
                far.validate_for(MIB * 8),
                Err(LayoutError::TooLarge),
                "{name} far"
            );
        }
    }

    // ---- requests, flags ---------------------------------------------------------

    #[test]
    fn the_request_flags_and_the_plane_tail_must_agree() {
        let nv = two(FOURCC_NV12, 1920, 1080);
        let one_plane = lay1();
        let size = 8 * MIB;
        let v = |flags: u32, layout: Option<Layout>| validate_request(1, 2, 3, flags, size, layout);
        let both = FLAG_LAYOUT | FLAG_PLANE1;
        // NV12 with the flag and the tail.
        assert_eq!(v(both, Some(nv)), Ok(nv));
        // NV12 without the flag: the decoded layout has no tail, so it is the format
        // that is wrong.
        let nv_no_tail = Layout { plane1: None, ..nv };
        assert_eq!(
            v(FLAG_LAYOUT, Some(nv_no_tail)),
            Err(RequestError::Layout(LayoutError::Planes))
        );
        // The flag with no tail, and a tail with no flag, disagree.
        assert_eq!(v(both, Some(nv_no_tail)), Err(RequestError::Flags));
        assert_eq!(v(FLAG_LAYOUT, Some(nv)), Err(RequestError::Flags));
        // Spec: PLANE1 flag with a one-plane fourcc refused (the escape layer decoded the
        // tail, as the flag says it is there).
        let one_with_tail = Layout {
            plane1: nv.plane1,
            ..one_plane
        };
        assert_eq!(
            v(both, Some(one_with_tail)),
            Err(RequestError::Layout(LayoutError::Planes))
        );
        // PLANE1 without LAYOUT has no layout at all.
        assert_eq!(v(FLAG_PLANE1, None), Err(RequestError::LayoutRequired));
        assert_eq!(v(FLAG_PLANE1, Some(nv)), Err(RequestError::LayoutRequired));
        // Unknown bits stay unknown, with or without PLANE1.
        assert_eq!(v(both | 4, Some(nv)), Err(RequestError::Flags));
        assert_eq!(v(FLAG_PLANE1 | 4, None), Err(RequestError::Flags));
        assert_eq!(v(1 << 31, None), Err(RequestError::Flags));
        assert_eq!(
            v(FLAG_LAYOUT | 1 << 2, Some(one_plane)),
            Err(RequestError::Flags)
        );
        // The one-plane record with its flags: unchanged.
        assert_eq!(v(FLAG_LAYOUT, Some(one_plane)), Ok(one_plane));
        assert_eq!(v(0, Some(one_plane)), Err(RequestError::LayoutRequired));
        // The size rules still come first.
        assert_eq!(
            validate_request(1, 2, 3, both, 0, Some(nv)),
            Err(RequestError::Size)
        );
        assert_eq!(
            validate_request(1, 2, 3, both, MAX_FOREIGN_RESOURCE_BYTES + PAGE, Some(nv)),
            Err(RequestError::TooLarge)
        );
        // The flag value is the one the protocol pins in kmd_render.
        assert_eq!(FLAG_PLANE1, 2);
    }

    fn lay1() -> Layout {
        one(FOURCC_XRGB8888, 1920, 1080, 7680)
    }

    #[test]
    fn plane_1_modifier_mismatch_is_refused_through_the_request_too() {
        let mut nv = two_bl(FOURCC_NV12, 1920, 1080, 4, 3);
        nv.plane1.as_mut().unwrap().modifier = MOD_LINEAR;
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT | FLAG_PLANE1, 16 * MIB, Some(nv)),
            Err(RequestError::Layout(LayoutError::Modifier))
        );
    }

    // ---- hostile values ----------------------------------------------------------

    #[test]
    fn hostile_values_cannot_overflow_or_panic() {
        // stride * rows in u64: u32::MAX * (u32::MAX rounded up to a block) wraps without
        // saturation. `min_bytes` is called on unvalidated layouts by tests and tracing.
        let huge = Layout {
            width: u32::MAX,
            height: u32::MAX,
            stride: u32::MAX,
            offset: u32::MAX,
            fourcc: FOURCC_NV12,
            modifier: BL | 5,
            plane1: Some(Plane {
                stride: u32::MAX,
                offset: u32::MAX,
                modifier: BL | 5,
            }),
        };
        assert_eq!(huge.min_bytes(), u64::MAX);
        assert_eq!(huge.plane0_min_bytes(), u64::MAX);
        // (2^32 - 1) + (2^32 - 1) * 2^31: half the chroma rows, no saturation needed.
        assert_eq!(
            huge.plane1_min_bytes(),
            Some(u64::from(u32::MAX) + u64::from(u32::MAX) * (1 << 31))
        );
        assert_eq!(huge.validate(), Err(LayoutError::Dimensions));
        assert_eq!(huge.validate_for(u64::MAX), Err(LayoutError::Dimensions));
        // The same in every format, linear and block-linear, with an in-range extent but
        // hostile strides and offsets: an error, never a panic.
        for fourcc in ONE_PLANE
            .iter()
            .map(|t| t.0)
            .chain(TWO_PLANE.iter().map(|t| t.0))
        {
            for modifier in [MOD_LINEAR, BL, BL | 5, B8 | 5, B16 | 5, u64::MAX] {
                for stride in [0, 1, 3, MAX_STRIDE, MAX_STRIDE + 1, u32::MAX] {
                    for offset in [0, 1, u32::MAX] {
                        for plane1 in [
                            None,
                            Some(Plane {
                                stride,
                                offset,
                                modifier,
                            }),
                            Some(Plane {
                                stride: u32::MAX,
                                offset: u32::MAX,
                                modifier: u64::MAX,
                            }),
                        ] {
                            let l = Layout {
                                width: 16384,
                                height: 16384,
                                stride,
                                offset,
                                fourcc,
                                modifier,
                                plane1,
                            };
                            let _ = l.min_bytes();
                            let _ = l.validate();
                            let _ = l.validate_for(u64::MAX);
                            let _ = l.validate_for(0);
                            let _ = validate_request(
                                1,
                                2,
                                3,
                                FLAG_LAYOUT | FLAG_PLANE1,
                                MAX_FOREIGN_RESOURCE_BYTES,
                                Some(l),
                            );
                        }
                    }
                }
            }
        }
        // u32 extents with a valid-looking stride: the dimension check is first.
        for (w, h) in [
            (0, 0),
            (u32::MAX, 1),
            (1, u32::MAX),
            (MAX_DIM + 1, MAX_DIM + 1),
        ] {
            for fourcc in [FOURCC_R8, FOURCC_NV12, FOURCC_YUYV, FOURCC_XRGB8888, 0] {
                let l = Layout {
                    width: w,
                    height: h,
                    ..one(fourcc, 1, 1, 4096)
                };
                assert_eq!(l.validate(), Err(LayoutError::Dimensions));
            }
        }
        // A 16384 x 16384 fp16 image needs 2 GiB: refused against any size the
        // import takes (1 GiB at most), by its bound and not by wrapping.
        let fp16 = one(FOURCC_ABGR16161616F, 16384, 16384, 8 * 16384);
        assert_eq!(fp16.min_bytes(), 2 << 30);
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT, MAX_FOREIGN_RESOURCE_BYTES, Some(fp16)),
            Err(RequestError::Layout(LayoutError::TooLarge))
        );
        // 16384 x 16384 formats that do fit 1 GiB.
        for (l, why) in [
            (one(FOURCC_R8, 16384, 16384, 16384), "R8"),
            (one(FOURCC_RGB565, 16384, 16384, 2 * 16384), "RGB565"),
            (two(FOURCC_NV12, 16384, 16384), "NV12"),
            (two(FOURCC_P016, 16384, 16384), "P016"),
        ] {
            let planes = if l.plane1.is_some() { FLAG_PLANE1 } else { 0 };
            let size = rup(l.min_bytes(), PAGE);
            assert!(size <= MAX_FOREIGN_RESOURCE_BYTES, "{why}");
            assert_eq!(
                validate_request(1, 2, 3, FLAG_LAYOUT | planes, size, Some(l)),
                Ok(l),
                "{why}"
            );
        }
        // The bound of any validated layout stays far from u64 overflow: the largest
        // stride, the tallest block-linear extent, the largest offsets.
        let max = Layout {
            width: MAX_DIM,
            height: MAX_DIM,
            stride: MAX_STRIDE,
            offset: u32::MAX,
            fourcc: FOURCC_ABGR16161616,
            modifier: BL | 5,
            plane1: None,
        };
        assert_eq!(max.validate(), Ok(()));
        assert!(max.min_bytes() < 1 << 40);
        // An offset of u32::MAX is a valid number, and an impossible fit.
        assert_eq!(
            max.validate_for(MAX_FOREIGN_RESOURCE_BYTES),
            Err(LayoutError::TooLarge)
        );
    }

    // ---- the GB20x block-linear families (docs/shared-formats.md section 5) -----------

    const B8: u64 = MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP;
    const B16: u64 = MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP;
    const FAMILIES: [u64; 3] = [BL, B8, B16];

    /// The family each plane of each format must carry, written out by hand (not derived
    /// from `gb20x_family` or from the table): (fourcc, plane 0 family, plane 1 family or 0).
    const FAMILY_OF: [(u32, u64, u64); 18] = [
        (FOURCC_XRGB8888, BL, 0),
        (FOURCC_ARGB8888, BL, 0),
        (FOURCC_XBGR8888, BL, 0),
        (FOURCC_ABGR8888, BL, 0),
        (FOURCC_R8, B8, 0),
        (FOURCC_GR88, B16, 0),
        (FOURCC_R16, B16, 0),
        (FOURCC_GR1616, BL, 0),
        (FOURCC_RGB565, B16, 0),
        (FOURCC_ARGB1555, B16, 0),
        (FOURCC_ARGB4444, B16, 0),
        (FOURCC_ABGR2101010, BL, 0),
        (FOURCC_ABGR16161616F, BL, 0),
        (FOURCC_ABGR16161616, BL, 0),
        (FOURCC_YUYV, BL, 0),
        (FOURCC_NV12, B8, B16),
        (FOURCC_P010, B16, BL),
        (FOURCC_P016, B16, BL),
    ];

    /// Every modifier worth offering a plane: LINEAR, each family with every `h` in 0..=7,
    /// and the values around and far from them.
    fn modifier_candidates() -> Vec<u64> {
        let mut v = std::vec![MOD_LINEAR, 1, 0x10, 0x0100_0000_0000_0001, u64::MAX];
        for f in FAMILIES {
            for h in 0..=7u64 {
                v.push(f | h);
            }
            v.extend([
                f - 1,
                f + 0x10,
                f | 0xf,
                f ^ 1 << 63,
                f ^ 1 << 22,
                f ^ 3 << 26,
            ]);
        }
        // The sector-layout bits of the three families, mixed the two other ways.
        v.push(BL | 1 << 22);
        v.push(BL | 3 << 26);
        v.push(B8 | 3 << 26);
        v.push(B16 & !(1 << 22));
        v
    }

    /// Whether `m` is acceptable for a plane whose family is `fam`: LINEAR, or `fam | h`,
    /// `h <= 5`. Spelled out without any of the production helpers.
    fn plane_accepts(m: u64, fam: u64) -> bool {
        m == 0 || (0..=5u64).any(|h| m == (fam | h))
    }

    #[test]
    fn the_three_families_and_the_element_size_rule() {
        assert_eq!(BL, 0x0300_0000_0060_6010);
        assert_eq!(B8, 0x0300_0000_0420_6010);
        assert_eq!(B16, 0x0300_0000_0460_6010);
        // They differ only in the sector-layout field: bit 22 and bits 26..27.
        let sector = |m: u64| ((m >> 22) & 1) | (((m >> 26) & 3) << 1);
        assert_eq!((sector(BL), sector(B8), sector(B16)), (1, 2, 3));
        let rest = !((1u64 << 22) | (3u64 << 26));
        assert_eq!(BL & rest, B8 & rest);
        assert_eq!(BL & rest, B16 & rest);
        // `h` lives in the low nibble, clear of every family bit.
        for f in FAMILIES {
            assert_eq!(f & 0xf, 0);
        }
        assert_eq!(gb20x_family(1), B8);
        assert_eq!(gb20x_family(2), B16);
        for bytes in [0, 3, 4, 5, 8, 16, u32::MAX] {
            assert_eq!(gb20x_family(bytes), BL, "{bytes}");
        }
        // The table's element sizes give the hand-written families.
        for (fourcc, f0, f1) in FAMILY_OF {
            let f = share_format(fourcc).unwrap();
            assert_eq!(gb20x_family(f.bpp0), f0, "{fourcc:#x} plane 0");
            if f.planes == 2 {
                assert_eq!(gb20x_family(f.bpp1), f1, "{fourcc:#x} plane 1");
            } else {
                assert_eq!(f1, 0, "{fourcc:#x} has no plane 1");
            }
        }
        assert_eq!(
            FAMILY_OF.len(),
            ONE_PLANE.len() + TWO_PLANE.len(),
            "every format of the table is listed"
        );
        // Every `h` of every family reads back.
        for f in FAMILIES {
            for h in 0..=5u64 {
                let l = Layout {
                    modifier: f | h,
                    ..one(FOURCC_R8, 64, 64, 64)
                };
                assert_eq!(l.block_height_log2(), Some(h as u32));
            }
            for h in 6..=7u64 {
                let l = Layout {
                    modifier: f | h,
                    ..one(FOURCC_R8, 64, 64, 64)
                };
                assert_eq!(l.block_height_log2(), None);
            }
        }
        assert_eq!(one(FOURCC_R8, 64, 64, 64).block_height_log2(), None);
    }

    /// Every format x every modifier candidate on plane 0 (x every candidate on plane 1 for
    /// the two-plane formats): accepted exactly when each plane's modifier is LINEAR or its
    /// own family with `h <= 5`, and LINEAR together; refused as `Modifier` otherwise, and
    /// never anything else (the layouts are otherwise valid).
    #[test]
    fn only_the_matching_family_is_accepted_per_plane() {
        let cands = modifier_candidates();
        let mut accepted = 0u32;
        let mut checked = 0u32;
        for (fourcc, f0, f1) in FAMILY_OF {
            let fmt = share_format(fourcc).unwrap();
            let (w, h) = (64u32, 64u32);
            let base = if fmt.planes == 2 {
                two(fourcc, w, h)
            } else {
                let (_, _, bpp, hdiv) = *ONE_PLANE.iter().find(|t| t.0 == fourcc).unwrap();
                one(fourcc, w, h, row0(bpp, hdiv, w))
            };
            for &m0 in &cands {
                let ok0 = plane_accepts(m0, f0);
                if fmt.planes == 1 {
                    let l = Layout {
                        modifier: m0,
                        ..base
                    };
                    let want = if ok0 {
                        Ok(())
                    } else {
                        Err(LayoutError::Modifier)
                    };
                    assert_eq!(l.validate(), want, "{fourcc:#x} {m0:#x}");
                    assert_eq!(l.validate_for(u64::MAX), want, "{fourcc:#x} {m0:#x}");
                    if ok0 {
                        let size = rup(l.min_bytes(), PAGE);
                        assert_eq!(validate_request(1, 2, 3, FLAG_LAYOUT, size, Some(l)), Ok(l));
                        accepted += 1;
                    } else {
                        assert_eq!(
                            validate_request(1, 2, 3, FLAG_LAYOUT, 1 << 30, Some(l)),
                            Err(RequestError::Layout(LayoutError::Modifier)),
                            "{fourcc:#x} {m0:#x}"
                        );
                    }
                    checked += 1;
                    continue;
                }
                for &m1 in &cands {
                    let mut l = Layout {
                        modifier: m0,
                        ..base
                    };
                    let off = l.plane0_min_bytes();
                    let p = l.plane1.as_mut().unwrap();
                    p.modifier = m1;
                    p.offset = off as u32;
                    let ok1 = plane_accepts(m1, f1);
                    let together = (m0 == 0) == (m1 == 0);
                    let good = ok0 && ok1 && together;
                    let want = if good {
                        Ok(())
                    } else {
                        Err(LayoutError::Modifier)
                    };
                    assert_eq!(l.validate(), want, "{fourcc:#x} {m0:#x} / {m1:#x}");
                    if good {
                        let size = rup(l.min_bytes(), PAGE);
                        assert_eq!(
                            validate_request(1, 2, 3, FLAG_LAYOUT | FLAG_PLANE1, size, Some(l)),
                            Ok(l)
                        );
                        accepted += 1;
                    }
                    checked += 1;
                }
            }
        }
        // 32 candidates... the exact count is not the point; the sweep ran and accepted some.
        assert!(checked > 5_000, "{checked}");
        assert!(accepted > 100, "{accepted}");
    }

    /// The explicit cases the NVK owner listed.
    #[test]
    fn the_named_formats_take_the_named_families() {
        let ok = |l: Layout| assert_eq!(l.validate(), Ok(()), "{l:?}");
        let bad = |l: Layout| assert_eq!(l.validate(), Err(LayoutError::Modifier), "{l:?}");
        // NV12: plane 0 8BPP, plane 1 16BPP, h free per plane.
        let mk = |fourcc, m0, m1| {
            let mut l = two(fourcc, 1920, 1080);
            l.modifier = m0;
            let off = l.plane0_min_bytes();
            let p = l.plane1.as_mut().unwrap();
            p.modifier = m1;
            p.offset = off as u32;
            l
        };
        ok(mk(FOURCC_NV12, B8 | 4, B16 | 3));
        ok(mk(FOURCC_NV12, B8, B16 | 5));
        bad(mk(FOURCC_NV12, BL | 4, B16 | 3));
        bad(mk(FOURCC_NV12, B8 | 4, BL | 3));
        bad(mk(FOURCC_NV12, B16 | 4, B8 | 3));
        bad(mk(FOURCC_NV12, B8 | 4, B8 | 3));
        bad(mk(FOURCC_NV12, B16 | 4, B16 | 3));
        bad(mk(FOURCC_NV12, B8 | 6, B16 | 3));
        bad(mk(FOURCC_NV12, B8 | 4, B16 | 6));
        // P010 / P016: plane 0 16BPP, plane 1 BASE.
        for f in [FOURCC_P010, FOURCC_P016] {
            ok(mk(f, B16 | 4, BL | 3));
            bad(mk(f, BL | 4, BL | 3));
            bad(mk(f, B16 | 4, B16 | 3));
            bad(mk(f, B8 | 4, BL | 3));
            bad(mk(f, B16 | 4, B8 | 3));
        }
        // LINEAR with LINEAR stays fine, and LINEAR never mixes with a family.
        ok(mk(FOURCC_NV12, 0, 0));
        bad(mk(FOURCC_NV12, 0, B16 | 1));
        bad(mk(FOURCC_NV12, B8 | 1, 0));
        // One-plane formats.
        let r8 = |m| Layout {
            modifier: m,
            ..one(FOURCC_R8, 1920, 1080, 1920)
        };
        ok(r8(B8 | 5));
        bad(r8(BL | 5));
        bad(r8(B16 | 5));
        for f in [
            FOURCC_GR88,
            FOURCC_R16,
            FOURCC_RGB565,
            FOURCC_ARGB1555,
            FOURCC_ARGB4444,
        ] {
            let l = |m| Layout {
                modifier: m,
                ..one(f, 1920, 1080, 3840)
            };
            ok(l(B16 | 2));
            bad(l(BL | 2));
            bad(l(B8 | 2));
        }
        // 4 and 8 byte elements, YUYV included, keep BASE only.
        for (f, stride) in [
            (FOURCC_GR1616, 7680),
            (FOURCC_ABGR2101010, 7680),
            (FOURCC_ABGR16161616F, 15360),
            (FOURCC_ABGR16161616, 15360),
            (FOURCC_YUYV, 3840),
            (FOURCC_XRGB8888, 7680),
            (FOURCC_ABGR8888, 7680),
        ] {
            let l = |m| Layout {
                modifier: m,
                ..one(f, 1920, 1080, stride)
            };
            ok(l(BL | 2));
            bad(l(B8 | 2));
            bad(l(B16 | 2));
        }
    }

    /// The block height of the size bound is `8 << h` in every family: the same layout
    /// geometry gives the same bound whichever family carries the `h` (the assumption in
    /// `plane_min_bytes`: the sector layout reorders bytes inside a GOB, it does not change
    /// the rows a block spans).
    #[test]
    fn the_size_bound_uses_h_whatever_the_family() {
        for hh in 0..=5u64 {
            let want = 1920 * rup(1080, 8 << hh);
            for f in FAMILIES {
                let l = Layout {
                    modifier: f | hh,
                    ..one(FOURCC_R8, 1920, 1080, 1920)
                };
                assert_eq!(l.plane0_min_bytes(), want, "{f:#x} h{hh}");
                assert_eq!(l.min_bytes(), want);
            }
        }
        // And per plane of NV12 with the families it really has.
        for h0 in 0..=5u64 {
            for h1 in 0..=5u64 {
                let l = two_bl(FOURCC_NV12, 1920, 1080, h0, h1);
                assert_eq!(l.modifier, B8 | h0);
                assert_eq!(l.plane1.unwrap().modifier, B16 | h1);
                assert_eq!(l.validate(), Ok(()));
                let p0 = 1920 * rup(1080, 8 << h0);
                assert_eq!(l.plane0_min_bytes(), p0);
                assert_eq!(
                    l.plane1_min_bytes(),
                    Some(p0 + 1920 * rup(540, 8 << h1)),
                    "{h0}/{h1}"
                );
            }
        }
    }

    /// The 32 bpp consumers (scanout, flip, copy, blit, level 5) only ever see records the
    /// table validated, so a 32 bpp record carries `BASE | h` or LINEAR: a layout naming the
    /// 8BPP / 16BPP family on a 32 bpp format never becomes a record.
    #[test]
    fn a_32_bpp_layout_with_another_family_is_refused_at_the_request() {
        for (f, name, _, _) in &ONE_PLANE[..4] {
            for fam in [B8, B16] {
                for h in 0..=7u64 {
                    let l = Layout {
                        modifier: fam | h,
                        ..one(*f, 1920, 1080, 7680)
                    };
                    assert_eq!(
                        validate_request(1, 2, 3, FLAG_LAYOUT, 16 * MIB, Some(l)),
                        Err(RequestError::Layout(LayoutError::Modifier)),
                        "{name} {fam:#x} h{h}"
                    );
                }
            }
            for h in 0..=5u64 {
                let l = Layout {
                    modifier: BL | h,
                    ..one(*f, 1920, 1080, 7680)
                };
                assert_eq!(
                    validate_request(1, 2, 3, FLAG_LAYOUT, 16 * MIB, Some(l)),
                    Ok(l),
                    "{name} h{h}"
                );
            }
        }
    }

    #[test]
    fn modifier_refusals_are_counted_as_fgrefmod() {
        let mut t = table();
        let refuse = |t: &mut ForeignTable, l: Layout, flags: u32| {
            let e = validate_request(1, 2, 3, flags, 64 * MIB, Some(l)).unwrap_err();
            t.note_request_refusal(e, Some(&l));
            e
        };
        // A 32 bpp record with the 16BPP family: a modifier refusal, not "new geometry".
        refuse(
            &mut t,
            Layout {
                modifier: B16 | 1,
                ..lay1()
            },
            FLAG_LAYOUT,
        );
        let c = t.counters();
        assert_eq!((c.refused_modifier, c.refused_new_geometry), (1, 0));
        assert_eq!(c.refused_request, 1);
        // R8 with the BASE family: both a modifier refusal and a new-format geometry one.
        refuse(
            &mut t,
            Layout {
                modifier: BL,
                ..one(FOURCC_R8, 64, 64, 64)
            },
            FLAG_LAYOUT,
        );
        // NV12 plane 1 in the wrong family.
        let mut nv = two(FOURCC_NV12, 64, 64);
        nv.modifier = B8;
        nv.plane1.as_mut().unwrap().modifier = B8;
        refuse(&mut t, nv, FLAG_LAYOUT | FLAG_PLANE1);
        let c = t.counters();
        assert_eq!((c.refused_modifier, c.refused_new_geometry), (3, 2));
        assert_eq!(c.refused_request, 3);
        // Other refusals do not move it: a bad stride, an odd extent, an unknown fourcc.
        refuse(
            &mut t,
            Layout {
                stride: 3,
                modifier: BL,
                ..lay1()
            },
            FLAG_LAYOUT,
        );
        refuse(&mut t, one(0, 64, 64, 64), FLAG_LAYOUT);
        let c = t.counters();
        assert_eq!(c.refused_modifier, 3);
        assert_eq!(c.refused_request, 5);
        assert_eq!(c.refused(), 5, "a subset counter does not add to the total");
    }

    // ---- the 32 bpp behaviour is unchanged -------------------------------------------

    /// The validation of `foreign_resource` before the shared formats, verbatim (only the
    /// new field is absent), as the reference the four 32 bpp formats must keep matching.
    fn old_validate(
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
    ) -> Result<(), LayoutError> {
        if width < MIN_DIM || width > MAX_DIM || height < MIN_DIM || height > MAX_DIM {
            return Err(LayoutError::Dimensions);
        }
        if !matches!(
            fourcc,
            FOURCC_XRGB8888 | FOURCC_ARGB8888 | FOURCC_XBGR8888 | FOURCC_ABGR8888
        ) {
            return Err(LayoutError::Format);
        }
        let block = modifier >= BL && modifier <= BL + 5;
        if (stride as u64) < (width as u64) * 4 || stride > MAX_STRIDE || stride % 4 != 0 {
            return Err(LayoutError::Stride);
        }
        if modifier != MOD_LINEAR && !block {
            return Err(LayoutError::Modifier);
        }
        Ok(())
    }

    fn old_min_bytes(height: u32, stride: u32, offset: u32, modifier: u64) -> u64 {
        let rows = if modifier >= BL && modifier <= BL + 5 {
            let b = GOB_ROWS << (modifier - BL);
            ((height as u64) + b - 1) / b * b
        } else {
            height as u64
        };
        offset as u64 + (stride as u64) * rows
    }

    #[test]
    fn the_four_32_bpp_formats_behave_exactly_as_before() {
        let dims = [
            0,
            1,
            2,
            3,
            15,
            16,
            63,
            64,
            1080,
            1919,
            1920,
            4096,
            16383,
            16384,
            16385,
            u32::MAX,
        ];
        let strides = [
            0,
            1,
            3,
            4,
            8,
            60,
            252,
            256,
            7676,
            7679,
            7680,
            7681,
            7682,
            8192,
            65536,
            65540,
            MAX_STRIDE - 4,
            MAX_STRIDE,
            MAX_STRIDE + 4,
            u32::MAX - 3,
            u32::MAX,
        ];
        let mods = [
            MOD_LINEAR,
            BL,
            BL | 1,
            BL | 3,
            BL | 5,
            BL | 6,
            BL - 1,
            1,
            0x0100_0000_0000_0001,
            u64::MAX,
            // The GB20x 8BPP / 16BPP families: the old rules never knew them, so a 32 bpp
            // layout carrying one is refused exactly as any other unknown modifier was.
            MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP,
            MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP | 1,
            MOD_NVIDIA_BLOCK_LINEAR_BASE_8BPP | 5,
            MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP,
            MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP | 2,
            MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP | 5,
        ];
        let fourccs = [
            FOURCC_XRGB8888,
            FOURCC_ARGB8888,
            FOURCC_XBGR8888,
            FOURCC_ABGR8888,
        ];
        let mut checked = 0u64;
        for &f in &fourccs {
            for &w in &dims {
                for &h in &dims {
                    for &stride in &strides {
                        for &m in &mods {
                            let l = Layout {
                                width: w,
                                height: h,
                                stride,
                                offset: 0x1000,
                                fourcc: f,
                                modifier: m,
                                plane1: None,
                            };
                            assert_eq!(
                                l.validate(),
                                old_validate(w, h, stride, f, m),
                                "{f:#x} {w}x{h} stride {stride} mod {m:#x}"
                            );
                            if l.validate().is_ok() {
                                assert_eq!(l.min_bytes(), old_min_bytes(h, stride, 0x1000, m));
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 100_000);
        // And through the request, the old flag rules (bit 1 is the only one that moved).
        let l = lay1();
        assert_eq!(
            validate_request(1, 2, 3, FLAG_LAYOUT, 8 * MIB, Some(l)),
            Ok(l)
        );
        assert_eq!(
            validate_request(1, 2, 3, 0, 8 * MIB, Some(l)),
            Err(RequestError::LayoutRequired)
        );
    }

    #[test]
    fn only_the_four_are_rgb32() {
        for (f, name, _, _) in ONE_PLANE {
            let l = one(f, 64, 64, 64 * 8);
            assert_eq!(
                l.is_rgb32(),
                ONE_PLANE[..4].iter().any(|t| t.0 == f),
                "{name}"
            );
        }
        for (f, name, _) in TWO_PLANE {
            assert!(!two(f, 64, 64).is_rgb32(), "{name}");
        }
        // A 32 bpp fourcc carrying a plane is not rgb32 whatever else it is.
        let l = Layout {
            plane1: two(FOURCC_NV12, 64, 64).plane1,
            ..lay1()
        };
        assert!(!l.is_rgb32());
        assert_eq!(l.plane_count(), 2);
        assert_eq!(lay1().plane_count(), 1);
    }

    // ---- the table: counters, adoption ---------------------------------------------

    fn table() -> ForeignTable {
        ForeignTable::with_limits(Limits {
            total: 16,
            per_owner: 16,
            bytes_per_owner: 1 << 32,
        })
    }

    fn import(t: &mut ForeignTable, id: u32, l: Layout, size: u64) {
        let r = t.reserve(1, size).unwrap();
        t.commit(r, id, 7, 3, id, l).unwrap();
    }

    fn adopt_req(l: &Layout, plane_room: bool) -> AdoptRequest {
        AdoptRequest {
            declares_foreign: true,
            take_ownership: true,
            ctx_id: 7,
            width: l.width,
            height: l.height,
            pitch: l.stride,
            plane_offset: u64::from(l.offset),
            claimed_alloc_size: 0,
            supplied_layout: None,
            trailer_room: true,
            plane_room,
        }
    }

    #[test]
    fn imports_and_adoptions_of_the_new_formats_are_counted() {
        let mut t = table();
        let nv = two(FOURCC_NV12, 1920, 1080);
        let r8 = one(FOURCC_R8, 1920, 1080, 1920);
        let rgb = lay1();
        import(&mut t, 10, nv, 4 * MIB);
        import(&mut t, 11, r8, 4 * MIB);
        import(&mut t, 12, rgb, 8 * MIB);
        let c = t.counters();
        assert_eq!(
            (c.imported, c.imported_planes, c.imported_format),
            (3, 1, 1)
        );
        // Adoption of a two-plane record counts; the others do not.
        for (id, l) in [(10, nv), (11, r8), (12, rgb)] {
            let plan = t
                .adopt_for_allocation(id, &adopt_req(&l, true), true, true)
                .unwrap();
            assert_eq!(
                plan,
                AdoptPlan::Foreign(Adopted {
                    size: t.get(id).unwrap().size,
                    layout: l
                })
            );
        }
        let c = t.counters();
        assert_eq!((c.adopted, c.adopted_planes), (3, 1));
        assert_eq!(c.refused(), 0);
    }

    #[test]
    fn a_two_plane_record_needs_the_144_byte_private_data() {
        let mut t = table();
        let nv = two(FOURCC_NV12, 1920, 1080);
        import(&mut t, 10, nv, 4 * MIB);
        // Spec: a 2-plane record with only the 128-byte buffer is refused, counted, and
        // changes nothing (a later adoption with room still works).
        assert_eq!(
            t.adopt_for_allocation(10, &adopt_req(&nv, false), true, true),
            Err(AdoptRefusal::NoPlaneRoom)
        );
        let c = t.counters();
        assert_eq!(
            (c.refused_adopt, c.refused_no_plane_room, c.adopted_planes),
            (1, 1, 0)
        );
        assert_eq!(c.refused(), 1);
        assert!(t.get(10).unwrap().creator.is_some());
        // No trailer room at all is the older refusal, not this one.
        let mut none = adopt_req(&nv, false);
        none.trailer_room = false;
        assert_eq!(
            t.adopt_for_allocation(10, &none, true, true),
            Err(AdoptRefusal::NoTrailerRoom)
        );
        assert_eq!(t.counters().refused_no_plane_room, 1);
        assert!(t
            .adopt_for_allocation(10, &adopt_req(&nv, true), true, true)
            .is_ok());
        assert_eq!(AdoptRefusal::NoPlaneRoom.code(), 12);
        // A one-plane record never needs it.
        import(&mut t, 11, lay1(), 8 * MIB);
        assert!(t
            .adopt_for_allocation(11, &adopt_req(&lay1(), false), true, true)
            .is_ok());
        // The trailer sizes.
        assert_eq!(trailer_bytes(&lay1()), 128);
        assert_eq!(trailer_bytes(&nv), 144);
        assert_eq!(trailer_bytes(&one(FOURCC_R8, 64, 64, 64)), 128);
    }

    #[test]
    fn the_supplied_trailer_must_repeat_plane_1_too() {
        let mut t = table();
        let nv = two(FOURCC_NV12, 1920, 1080);
        import(&mut t, 10, nv, 4 * MIB);
        let mut req = adopt_req(&nv, true);
        // A supplied record that drops plane 1, or changes any of its fields.
        req.supplied_layout = Some(Layout { plane1: None, ..nv });
        assert_eq!(
            t.adopt_for_allocation(10, &req, true, true),
            Err(AdoptRefusal::LayoutMismatch)
        );
        let edits: [fn(&mut Plane); 3] =
            [|p| p.stride += 2, |p| p.offset += 4096, |p| p.modifier = BL];
        for edit in edits {
            let mut l = nv;
            edit(l.plane1.as_mut().unwrap());
            req.supplied_layout = Some(l);
            assert_eq!(
                t.adopt_for_allocation(10, &req, true, true),
                Err(AdoptRefusal::LayoutMismatch)
            );
        }
        // The geometry words are plane 0's: a plane 1 stride in `pitch` is a mismatch.
        let mut bad = adopt_req(&nv, true);
        bad.pitch = nv.plane1.unwrap().stride + 2;
        assert_eq!(
            t.adopt_for_allocation(10, &bad, true, true),
            Err(AdoptRefusal::GeometryMismatch)
        );
        let mut bad = adopt_req(&nv, true);
        bad.plane_offset = u64::from(nv.plane1.unwrap().offset);
        assert_eq!(
            t.adopt_for_allocation(10, &bad, true, true),
            Err(AdoptRefusal::GeometryMismatch)
        );
        // The exact record, supplied, adopts.
        req.supplied_layout = Some(nv);
        assert!(t.adopt_for_allocation(10, &req, true, true).is_ok());
    }

    #[test]
    fn open_and_flip_record_carry_plane_1() {
        let mut t = table();
        let nv = two_bl(FOURCC_NV12, 1920, 1080, 4, 3);
        import(&mut t, 10, nv, 4 * MIB);
        t.adopt_for_allocation(10, &adopt_req(&nv, true), true, true)
            .unwrap();
        // An opener is handed the recorded layout, plane 1 included, never the creator's.
        match t.open(10, 99) {
            OpenOutcome::Opened(a) => assert_eq!(a.layout, nv),
            other => panic!("{other:?}"),
        }
        assert_eq!(t.flip_record(10).unwrap().layout, nv);
        assert_eq!(t.layout(10), Some(nv));
    }

    #[test]
    fn request_refusals_are_counted_by_reason() {
        let mut t = table();
        let nv = two(FOURCC_NV12, 1920, 1080);
        let flags = FLAG_LAYOUT | FLAG_PLANE1;
        let refuse = |t: &mut ForeignTable, flags: u32, size: u64, l: Option<Layout>| {
            let e = validate_request(1, 2, 3, flags, size, l).unwrap_err();
            t.note_request_refusal(e, l.as_ref());
            e
        };
        // An unknown fourcc.
        refuse(
            &mut t,
            FLAG_LAYOUT,
            8 * MIB,
            Some(one(0x3432_4742, 64, 64, 256)),
        );
        // Plane tail against the format: both ways.
        refuse(
            &mut t,
            FLAG_LAYOUT,
            8 * MIB,
            Some(Layout { plane1: None, ..nv }),
        );
        refuse(
            &mut t,
            flags,
            8 * MIB,
            Some(Layout {
                plane1: nv.plane1,
                ..lay1()
            }),
        );
        // Geometry of a new format: odd extent, stride, size, modifier, overlap.
        refuse(&mut t, flags, 8 * MIB, Some(Layout { width: 63, ..nv }));
        refuse(
            &mut t,
            FLAG_LAYOUT,
            8 * MIB,
            Some(one(FOURCC_R8, 64, 64, 63)),
        );
        refuse(&mut t, flags, PAGE, Some(nv));
        refuse(
            &mut t,
            FLAG_LAYOUT,
            8 * MIB,
            Some(Layout {
                modifier: MOD_NVIDIA_BLOCK_LINEAR_BASE_16BPP | 6,
                ..one(FOURCC_R16, 64, 64, 128)
            }),
        );
        // The 32 bpp ones are not "new": their geometry refusals count as requests only.
        refuse(
            &mut t,
            FLAG_LAYOUT,
            8 * MIB,
            Some(Layout {
                stride: 4,
                ..lay1()
            }),
        );
        // Requests that never reached the layout, or carried none.
        refuse(&mut t, FLAG_LAYOUT | 4, 8 * MIB, Some(lay1()));
        refuse(&mut t, FLAG_LAYOUT, 0, Some(lay1()));
        refuse(&mut t, 0, 8 * MIB, None);
        let c = t.counters();
        assert_eq!(c.refused_request, 11);
        assert_eq!(c.refused_format, 1);
        assert_eq!(c.refused_planes, 2);
        assert_eq!(c.refused_new_geometry, 4);
        // The R16 request with h = 6 is the one modifier refusal.
        assert_eq!(c.refused_modifier, 1);
        // All of them are requests refused, and `refused` still sums once.
        assert_eq!(c.refused(), 11);
    }

    // ---- the overlay: never advertised ---------------------------------------------

    /// The KMD advertises no overlay planes, which is the whole reason NV12 is never
    /// scanned out (`docs/shared-formats.md`): dxgkrnl only hands a YUV surface to the
    /// display hardware through a multi-plane-overlay present, and the KMD neither
    /// reports `SupportMultiPlaneOverlay` in its caps nor registers the MPO3 DDI
    /// interface. A static check of the driver's source: no executable line outside the
    /// one refusal (`ddi/present_packet.rs`, which names the flag only to REFUSE an MPO
    /// present) mentions a multi-plane-overlay capability, DDI or interface.
    #[test]
    fn the_kmd_never_advertises_overlay_planes() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !root.exists() {
            return; // a copy of this crate without its sibling: nothing to scan
        }
        const FORBIDDEN: [&str; 7] = [
            "SupportMultiPlaneOverlay",
            "MultiPlaneOverlaySupport",
            "SetVidPnSourceAddressWithMultiPlaneOverlay",
            "CheckMultiPlaneOverlay",
            "MaxOverlay",
            "MPO3",
            "Mpo3",
        ];
        let mut stack = std::vec![root];
        let mut checked = 0;
        let mut allowed_mentions = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                checked += 1;
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&p).unwrap();
                for (n, line) in text.lines().enumerate() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue; // prose: the doc comments explain why there is none
                    }
                    for tok in FORBIDDEN {
                        assert!(
                            !code.contains(tok),
                            "{}:{} names `{tok}`: the KMD must not advertise overlay planes",
                            p.display(),
                            n + 1
                        );
                    }
                    // `MultiPlaneOverlay` as a word is allowed in exactly one file, where it
                    // is the refused arm of the present payload.
                    if code.contains("MultiPlaneOverlay") || code.contains("MultiplaneOverlay") {
                        assert_eq!(
                            name, "present_packet.rs",
                            "{}:{}: only the present packet may name a multi-plane overlay (to refuse it)",
                            p.display(),
                            n + 1
                        );
                        allowed_mentions += 1;
                    }
                }
            }
        }
        assert!(checked > 20);
        // The refusal is there: the payload arm exists (`PresentPayload::MultiPlaneOverlay`),
        // and it never produces an allocation list.
        assert!(allowed_mentions >= 1);
    }
}

/// Fixtures the other consumers' refusal tests share: the shared formats beyond the four
/// 32-bit RGB ones, and a valid record for each.
#[cfg(test)]
pub(crate) mod test_formats {
    use super::*;

    /// Every fourcc of [`share_format`] that is not one of the four 32 bpp RGB formats.
    pub(crate) const BEYOND_RGB32: [u32; 14] = [
        FOURCC_R8,
        FOURCC_GR88,
        FOURCC_R16,
        FOURCC_GR1616,
        FOURCC_RGB565,
        FOURCC_ARGB1555,
        FOURCC_ARGB4444,
        FOURCC_ABGR2101010,
        FOURCC_ABGR16161616F,
        FOURCC_ABGR16161616,
        FOURCC_YUYV,
        FOURCC_NV12,
        FOURCC_P010,
        FOURCC_P016,
    ];

    /// A valid record of `fourcc` (tightly packed; plane 1 right after plane 0 for the
    /// two-plane formats). `modifier` is [`MOD_LINEAR`] (both planes) or
    /// `MOD_NVIDIA_BLOCK_LINEAR_BASE | h`, which names the block height `h` only: each plane
    /// gets the family its element size requires ([`gb20x_family`]) with that `h`.
    pub(crate) fn valid_layout(fourcc: u32, w: u32, h: u32, modifier: u64) -> Layout {
        let f = share_format(fourcc).unwrap();
        let stride = f.row_bytes(0, w) as u32;
        let (m0, m1) = if modifier == MOD_LINEAR {
            (MOD_LINEAR, MOD_LINEAR)
        } else {
            let bh = modifier - MOD_NVIDIA_BLOCK_LINEAR_BASE;
            (gb20x_family(f.bpp0) | bh, gb20x_family(f.bpp1) | bh)
        };
        let mut l = Layout {
            width: w,
            height: h,
            stride,
            offset: 0,
            fourcc,
            modifier: m0,
            plane1: None,
        };
        if f.planes == 2 {
            l.plane1 = Some(Plane {
                stride: f.row_bytes(1, w) as u32,
                offset: l.plane0_min_bytes() as u32,
                modifier: m1,
            });
        }
        assert_eq!(l.validate(), Ok(()), "fixture {fourcc:#x} {w}x{h}");
        l
    }

    /// The shared test fixtures are themselves valid, cover the table, and are not rgb32.
    #[test]
    fn the_fixtures_are_valid_and_not_rgb32() {
        assert_eq!(BEYOND_RGB32.len() + 4, 18);
        for f in BEYOND_RGB32 {
            for m in [MOD_LINEAR, MOD_NVIDIA_BLOCK_LINEAR_BASE | 4] {
                let l = valid_layout(f, 1920, 1080, m);
                assert!(!l.is_rgb32());
                assert_eq!(l.plane_count(), share_format(f).unwrap().planes);
                assert!(l.validate_for(l.min_bytes()).is_ok());
            }
        }
    }
}
