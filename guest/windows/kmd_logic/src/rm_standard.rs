//! Which of the KMD's STANDARD allocations (`DxgkDdiGetStandardAllocationDriverData`) may be
//! given a foreign (RM-importable) identity, and with what layout record. The pure half of
//! `docs/rm-backed-standard.md`; nothing in `kmd_render` calls it yet.
//!
//! It is deliberately independent of HOW the bytes are made. Both candidates of that
//! document need exactly these answers before they differ:
//!
//! * R, the allocation comes from RM system memory (the level 5 service, 15 in
//!   `kmd-rm-client.md`), and
//! * D, the allocation stays a Venus blob and is only made importable (dma-buf).
//!
//! In both, an opener (DWM on NVK) is told "this is a LINEAR 32-bit surface of this extent
//! and pitch" and builds an image over it; the decision of WHICH standard allocations can
//! say that, and what the record then holds, is this module. The mapping from the three
//! numbers the OS hands the KMD to a class mirrors, line for line, what
//! `ddi/create_allocation.rs` and `protocol::classify` do today (tests pin both).
//!
//! No memory, no transport, no clock.

use crate::foreign_resource::{
    Layout, FOURCC_ABGR8888, FOURCC_ARGB8888, FOURCC_XRGB8888, MAX_DIM, MAX_FOREIGN_RESOURCE_BYTES,
    MAX_STRIDE, MIN_DIM, MOD_LINEAR,
};
use crate::round_up_page;

// ---- what the OS hands `GetStandardAllocationDriverData` ---------------------------------

/// `D3DKMDT_STANDARDALLOCATION_TYPE` (d3dkmdt.h).
pub const STD_SHAREDPRIMARYSURFACE: u32 = 1;
pub const STD_SHADOWSURFACE: u32 = 2;
pub const STD_STAGINGSURFACE: u32 = 3;
pub const STD_GDISURFACE: u32 = 4;
pub const STD_VGPU: u32 = 5;
pub const STD_FENCESTORAGE: u32 = 6;

/// `D3DKMDT_GDISURFACETYPE` (d3dkmdt.h), meaningful only with [`STD_GDISURFACE`].
pub const GDI_INVALID: u32 = 0;
pub const GDI_TEXTURE: u32 = 1;
pub const GDI_STAGING_CPUVISIBLE: u32 = 2;
pub const GDI_STAGING: u32 = 3;
pub const GDI_LOOKUPTABLE: u32 = 4;
pub const GDI_EXISTINGSYSMEM: u32 = 5;
pub const GDI_TEXTURE_CPUVISIBLE: u32 = 6;
pub const GDI_TEXTURE_CROSSADAPTER: u32 = 7;
pub const GDI_TEXTURE_CPUVISIBLE_CROSSADAPTER: u32 = 8;

/// `D3DDDIFMT_*` the KMD knows how to name (d3dukmdt.h): the three with a DXGI peer, and so
/// with a DRM fourcc (`ddi/create_allocation.rs::d3dddi_to_dxgi`).
pub const D3DDDIFMT_A8R8G8B8: u32 = 21;
pub const D3DDDIFMT_X8R8G8B8: u32 = 22;
pub const D3DDDIFMT_A8B8G8R8: u32 = 32;

/// What kind of memory a standard allocation is, from the three numbers the OS supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdClass {
    /// `SHAREDPRIMARYSURFACE`: the VidPn primary. Its own arm (level 5's service already
    /// has it); never a candidate here.
    Primary,
    /// A pitched, CPU-visible byte buffer: shadow, staging and every GDI surface type but
    /// `TEXTURE`. The KMD makes a mappable blob, dxgkrnl maps it through the CPU host
    /// aperture (GDI read-modify-write) and the KMD's Present Blt writes into it.
    CpuBuffer,
    /// `GDISURFACE_TEXTURE`: a GPU-only OPTIMAL (tiled) image, never mapped by the CPU.
    GpuTexture,
    /// A type the KMD does not serve (`VGPU`, `FENCESTORAGE`, anything unknown):
    /// `GetStandardAllocationDriverData` answers `STATUS_NOT_SUPPORTED`.
    Unsupported,
}

/// The class of a standard allocation, as `GetStandardAllocationDriverData` and
/// `protocol::classify` decide it (primary first, then the GDI texture, then a buffer).
pub const fn classify(std_type: u32, gdi_type: u32) -> StdClass {
    match std_type {
        STD_SHAREDPRIMARYSURFACE => StdClass::Primary,
        STD_SHADOWSURFACE | STD_STAGINGSURFACE => StdClass::CpuBuffer,
        STD_GDISURFACE => {
            if gdi_type == GDI_TEXTURE {
                StdClass::GpuTexture
            } else {
                StdClass::CpuBuffer
            }
        }
        _ => StdClass::Unsupported,
    }
}

/// `(DXGI format, DRM fourcc)` of a `D3DDDIFORMAT`, `None` for anything the foreign layout
/// record cannot name (8 and 16 bit surfaces, palettes, FP formats: they stay as they are).
pub const fn format_for(d3dddi: u32) -> Option<(u32, u32)> {
    match d3dddi {
        D3DDDIFMT_A8R8G8B8 => Some((87, FOURCC_ARGB8888)),
        D3DDDIFMT_X8R8G8B8 => Some((88, FOURCC_XRGB8888)),
        D3DDDIFMT_A8B8G8R8 => Some((28, FOURCC_ABGR8888)),
        _ => None,
    }
}

// ---- pitch ---------------------------------------------------------------------------------

/// What the KMD authors for every pitched standard allocation today
/// (`kmd_logic::CROSS_ADAPTER_PITCH_ALIGN`, D3D12's `TEXTURE_DATA_PITCH_ALIGNMENT`).
pub const PITCH_ALIGN_CROSS_ADAPTER: u32 = 256;
/// What NVK's image library (NIL) gives a LINEAR image when nobody asks for a stride
/// (`nil/image.rs`, `new_linear`: `extent_B.width.next_multiple_of(128)`; 256 on Kepler).
/// An opener that builds its image from the D3D description and CHECKS the layout record
/// against it (NVK patch 0031, `nvk_helios_check_import_layout`) sees this stride.
pub const PITCH_ALIGN_NVK_DEFAULT: u32 = 128;

/// `width * 4` rounded up to `align` (a power of two), saturating.
pub const fn row_pitch(width: u32, align: u32) -> u32 {
    let raw = width.saturating_mul(4);
    raw.saturating_add(align - 1) & !(align - 1)
}

/// Whether an image NVK builds without an explicit stride has the pitch the KMD authored for
/// a surface `width` pixels wide: true for every width that is a multiple of 64 pixels,
/// false otherwise (800 px: NVK 3200, KMD 3328). The two disagree ONLY for widths whose row
/// is an odd multiple of 128 bytes; whoever opens such a surface must pass the stride, or the
/// KMD must author the 128-aligned one.
pub const fn nvk_default_stride_agrees(width: u32, authored_pitch: u32) -> bool {
    row_pitch(width, PITCH_ALIGN_NVK_DEFAULT) == authored_pitch
}

// ---- the decision --------------------------------------------------------------------------

/// Why a standard allocation was not given a foreign identity (the code is a breadcrumb, so
/// it never changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Why {
    /// The knob that turns this on is off.
    NotEnabled = 1,
    /// The VidPn primary has its own arm.
    Primary = 2,
    /// `GDISURFACE_TEXTURE`: a tiled GPU-only image, a later stage.
    GpuOnly = 3,
    /// `VGPU`, `FENCESTORAGE`, unknown.
    Unsupported = 4,
    /// `GDISURFACE_EXISTINGSYSMEM`: dxgkrnl supplies the pages, the KMD makes none.
    ExistingSysmem = 5,
    /// Not a 32-bit RGB `D3DDDIFORMAT` with a DRM fourcc.
    Format = 6,
    /// Width or height outside `1..=16384`, or a pitch over the record's bound.
    Extent = 7,
    /// The surface is over the per-allocation cap.
    Size = 8,
    /// dxgkrnl would map the CPU view write-combined (`AllocCached` = 0): a cached memory
    /// behind it is an alias, and a write-combined one makes GDI reads 300 times slower.
    NotCached = 9,
    /// The live bytes of foreign standard allocations would pass the budget.
    Budget = 10,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// The settings of the decision, from knobs and constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The knob (a sub-knob of `KmdRmClient`, read once per transport generation).
    pub enabled: bool,
    /// Row pitch alignment authored for these surfaces: [`PITCH_ALIGN_CROSS_ADAPTER`] or
    /// [`PITCH_ALIGN_NVK_DEFAULT`].
    pub pitch_align: u32,
    /// Largest single surface, bytes.
    pub max_bytes: u64,
    /// Most live bytes of foreign standard allocations (0 = unbounded).
    pub budget_bytes: u64,
}

impl Policy {
    /// What a first run would use: off, the pitch the KMD authors today, the foreign record's
    /// own 1 GiB bound per surface, and a 1 GiB total (the size of the BAR segment VidMm
    /// places these in, `ddi/bar_segment.rs` `BAR_SEGMENT_MAX_BYTES`).
    pub const DEFAULT: Policy = Policy {
        enabled: false,
        pitch_align: PITCH_ALIGN_CROSS_ADAPTER,
        max_bytes: MAX_FOREIGN_RESOURCE_BYTES,
        budget_bytes: 1 << 30,
    };
}

/// What `GetStandardAllocationDriverData` knows about one standard allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub std_type: u32,
    pub gdi_type: u32,
    /// The `D3DDDIFORMAT` the OS asked for (staging surfaces are always `A8R8G8B8`).
    pub d3dddi_format: u32,
    pub width: u32,
    pub height: u32,
    /// `AllocCached` (default 1): whether dxgkrnl is asked for a write-back CPU view.
    pub alloc_cached: bool,
    /// Bytes of foreign standard allocations alive now.
    pub live_bytes: u64,
}

/// The record a foreign standard allocation gets, and the meta words that agree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, authored: what `Pitch` answers to dxgkrnl, what `meta.pitch` and the
    /// record's `stride` say, and what an opener's image must use.
    pub pitch: u32,
    /// `pitch * height` rounded to a page: the smallest object that holds the picture. (A
    /// Venus blob is made larger, `paging::linear_blob_size`; RM's answer, if larger, is
    /// adopted by the creator.)
    pub size: u64,
    /// `meta.dxgi_format`: 87, 88 or 28 (the legacy "zero hint" is for allocations that are
    /// not foreign).
    pub dxgi_format: u32,
    pub fourcc: u32,
}

impl StdLayout {
    /// The foreign-resource layout record: plane 0, offset 0, [`MOD_LINEAR`]. `None` if it
    /// does not hold `size` bytes (cannot happen for a layout this module made).
    pub fn foreign_layout(&self, size: u64) -> Option<Layout> {
        let l = Layout {
            width: self.width,
            height: self.height,
            stride: self.pitch,
            offset: 0,
            fourcc: self.fourcc,
            modifier: MOD_LINEAR,
        };
        l.validate_for(size).ok().map(|()| l)
    }
}

/// Decide, in this order (first match wins, so the counter says the first reason):
/// knob, class, `EXISTINGSYSMEM`, format, extent, size, cache view, budget.
pub fn decide(p: &Policy, r: &Request) -> Result<StdLayout, Why> {
    if !p.enabled {
        return Err(Why::NotEnabled);
    }
    match classify(r.std_type, r.gdi_type) {
        StdClass::CpuBuffer => {}
        StdClass::Primary => return Err(Why::Primary),
        StdClass::GpuTexture => return Err(Why::GpuOnly),
        StdClass::Unsupported => return Err(Why::Unsupported),
    }
    if r.std_type == STD_GDISURFACE && r.gdi_type == GDI_EXISTINGSYSMEM {
        return Err(Why::ExistingSysmem);
    }
    let (dxgi_format, fourcc) = format_for(r.d3dddi_format).ok_or(Why::Format)?;
    if !(MIN_DIM..=MAX_DIM).contains(&r.width) || !(MIN_DIM..=MAX_DIM).contains(&r.height) {
        return Err(Why::Extent);
    }
    let pitch = row_pitch(r.width, p.pitch_align);
    if pitch > MAX_STRIDE || u64::from(pitch) < u64::from(r.width) * 4 {
        return Err(Why::Extent);
    }
    let size = round_up_page(u64::from(pitch) * u64::from(r.height));
    if size > p.max_bytes || size > MAX_FOREIGN_RESOURCE_BYTES {
        return Err(Why::Size);
    }
    if !r.alloc_cached {
        return Err(Why::NotCached);
    }
    if p.budget_bytes != 0 && r.live_bytes.saturating_add(size) > p.budget_bytes {
        return Err(Why::Budget);
    }
    Ok(StdLayout {
        width: r.width,
        height: r.height,
        pitch,
        size,
        dxgi_format,
        fourcc,
    })
}

// ---- the census (S-A0): what the OS actually asks for -------------------------------------

/// Slots of the census histogram: one per standard type and per GDI surface type.
pub const HIST_SLOTS: usize = 14;

/// The census slot of an allocation: 0 primary, 1 shadow, 2 staging, 3 to 11 the GDI surface
/// types 0 to 8, 12 a GDI type above 8, 13 any other standard type.
pub const fn hist_slot(std_type: u32, gdi_type: u32) -> usize {
    match std_type {
        STD_SHAREDPRIMARYSURFACE => 0,
        STD_SHADOWSURFACE => 1,
        STD_STAGINGSURFACE => 2,
        STD_GDISURFACE => {
            if gdi_type <= GDI_TEXTURE_CPUVISIBLE_CROSSADAPTER {
                3 + gdi_type as usize
            } else {
                12
            }
        }
        _ => 13,
    }
}

/// The registry value of a census slot (at most 14 bytes, like every counter name).
pub const fn hist_name(slot: usize) -> &'static [u8] {
    match slot {
        0 => b"StdNPrimary",
        1 => b"StdNShadow",
        2 => b"StdNStaging",
        3 => b"StdNGdi0",
        4 => b"StdNGdiTex",
        5 => b"StdNGdiStgCpu",
        6 => b"StdNGdiStg",
        7 => b"StdNGdiLut",
        8 => b"StdNGdiSys",
        9 => b"StdNGdiTexCpu",
        10 => b"StdNGdiTexXa",
        11 => b"StdNGdiTexCXa",
        12 => b"StdNGdiOther",
        _ => b"StdNOther",
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeSet;

    fn on() -> Policy {
        Policy {
            enabled: true,
            ..Policy::DEFAULT
        }
    }

    fn shadow(w: u32, h: u32) -> Request {
        Request {
            std_type: STD_SHADOWSURFACE,
            gdi_type: 0,
            d3dddi_format: D3DDDIFMT_A8R8G8B8,
            width: w,
            height: h,
            alloc_cached: true,
            live_bytes: 0,
        }
    }

    #[test]
    fn the_class_follows_what_the_kmd_does_today() {
        // `protocol::classify`: standard + PRIMARY first, then the OPTIMAL GDI texture
        // (GDISURFACE type 1 only), then a buffer; `GetStandardAllocationDriverData` refuses
        // everything but the four types.
        assert_eq!(classify(STD_SHAREDPRIMARYSURFACE, 0), StdClass::Primary);
        assert_eq!(classify(STD_SHADOWSURFACE, 0), StdClass::CpuBuffer);
        assert_eq!(classify(STD_STAGINGSURFACE, 0), StdClass::CpuBuffer);
        assert_eq!(classify(STD_GDISURFACE, GDI_TEXTURE), StdClass::GpuTexture);
        for g in [
            GDI_INVALID,
            GDI_STAGING_CPUVISIBLE,
            GDI_STAGING,
            GDI_LOOKUPTABLE,
            GDI_EXISTINGSYSMEM,
            GDI_TEXTURE_CPUVISIBLE,
            GDI_TEXTURE_CROSSADAPTER,
            GDI_TEXTURE_CPUVISIBLE_CROSSADAPTER,
            9,
            u32::MAX,
        ] {
            assert_eq!(classify(STD_GDISURFACE, g), StdClass::CpuBuffer, "gdi {g}");
        }
        // The GDI type means nothing for the other types.
        assert_eq!(
            classify(STD_SHADOWSURFACE, GDI_TEXTURE),
            StdClass::CpuBuffer
        );
        assert_eq!(
            classify(STD_SHAREDPRIMARYSURFACE, GDI_TEXTURE),
            StdClass::Primary
        );
        for s in [0, STD_VGPU, STD_FENCESTORAGE, 7, u32::MAX] {
            assert_eq!(classify(s, 0), StdClass::Unsupported, "std {s}");
        }
    }

    #[test]
    fn only_the_three_bgra_rgba_formats_have_a_record() {
        assert_eq!(format_for(21), Some((87, FOURCC_ARGB8888)));
        assert_eq!(format_for(22), Some((88, FOURCC_XRGB8888)));
        assert_eq!(format_for(32), Some((28, FOURCC_ABGR8888)));
        // X8B8G8R8 has no DXGI peer in this KMD, R5G6B5, P8, A8, 16F: not named.
        for f in [0, 23, 33, 20, 41, 50, 113, 114, 116, u32::MAX] {
            assert_eq!(format_for(f), None, "fmt {f}");
        }
    }

    #[test]
    fn the_pitch_is_width_times_four_rounded_and_saturates() {
        assert_eq!(row_pitch(1920, 256), 7680);
        assert_eq!(row_pitch(1896, 256), 7680, "the 7584 shear case");
        assert_eq!(row_pitch(800, 256), 3328);
        assert_eq!(row_pitch(800, 128), 3200);
        assert_eq!(row_pitch(1, 256), 256);
        assert_eq!(row_pitch(0, 256), 0);
        assert_eq!(row_pitch(5120, 256), 20480);
        // The 256 rule is the one the rest of the KMD authors.
        for w in [1u32, 31, 32, 64, 65, 704, 800, 1024, 1366, 1920, 3840, 5120] {
            assert_eq!(
                row_pitch(w, PITCH_ALIGN_CROSS_ADAPTER),
                crate::cross_adapter_pitch(w)
            );
        }
        assert_eq!(row_pitch(u32::MAX, 256) % 256, 0);
    }

    #[test]
    fn the_kmd_and_nvk_agree_on_a_linear_pitch_only_at_multiples_of_64_pixels() {
        // Measured widths of the T1 run (`docs/dwm-on-nvk.md`): 704 and 1024, 1920, 5120 are
        // fine; 800 is not.
        for (w, agrees) in [
            (704u32, true),
            (1024, true),
            (1920, true),
            (5120, true),
            (3840, true),
            (800, false),
            (1366, false),
            (1896, true),
            (32, false),
            (96, false),
        ] {
            let kmd = row_pitch(w, PITCH_ALIGN_CROSS_ADAPTER);
            assert_eq!(nvk_default_stride_agrees(w, kmd), agrees, "width {w}");
            // Authoring the 128-aligned pitch removes the difference at every width.
            assert!(nvk_default_stride_agrees(
                w,
                row_pitch(w, PITCH_ALIGN_NVK_DEFAULT)
            ));
        }
        // Exactly the widths with an odd number of 128-byte units in the row.
        for w in 1u32..=2048 {
            let row = u64::from(w) * 4;
            let odd_128 = row % 256 != 0 && row % 128 == 0;
            let nvk = row_pitch(w, 128);
            let kmd = row_pitch(w, 256);
            if nvk == kmd {
                assert!(!(row % 256 > 0 && row % 256 <= 128), "width {w}");
            } else {
                assert!(row % 256 > 0 && row % 256 <= 128, "width {w}");
            }
            if odd_128 {
                assert_ne!(nvk, kmd, "width {w}");
            }
        }
    }

    #[test]
    fn a_shadow_buffer_gets_a_linear_record_that_holds_the_picture() {
        let l = decide(&on(), &shadow(1920, 1080)).unwrap();
        assert_eq!((l.pitch, l.size, l.dxgi_format), (7680, 8_294_400, 87));
        let fl = l.foreign_layout(l.size).unwrap();
        assert_eq!(
            (fl.width, fl.height, fl.stride, fl.offset, fl.modifier),
            (1920, 1080, 7680, 0, MOD_LINEAR)
        );
        assert_eq!(fl.fourcc, FOURCC_ARGB8888);
        // A smaller object than the picture is refused by the record.
        assert!(l.foreign_layout(l.size - 4096).is_none());
        // 1x1 is a legal foreign extent (the flip floor of 64 is the primary's).
        let tiny = decide(&on(), &shadow(1, 1)).unwrap();
        assert_eq!((tiny.pitch, tiny.size), (256, 4096));
        // A window-sized surface at a width NVK would give another stride.
        let w = decide(&on(), &shadow(800, 704)).unwrap();
        assert_eq!(w.pitch, 3328);
        assert_eq!(w.size, round_up_page(3328 * 704));
        let nvk = decide(
            &Policy {
                pitch_align: PITCH_ALIGN_NVK_DEFAULT,
                ..on()
            },
            &shadow(800, 704),
        )
        .unwrap();
        assert_eq!(nvk.pitch, 3200);
        assert!(nvk_default_stride_agrees(800, nvk.pitch));
    }

    #[test]
    fn every_cpu_visible_gdi_type_but_existing_sysmem_is_a_candidate() {
        for g in [
            GDI_INVALID,
            GDI_STAGING_CPUVISIBLE,
            GDI_STAGING,
            GDI_LOOKUPTABLE,
            GDI_TEXTURE_CPUVISIBLE,
            GDI_TEXTURE_CROSSADAPTER,
            GDI_TEXTURE_CPUVISIBLE_CROSSADAPTER,
        ] {
            let r = Request {
                std_type: STD_GDISURFACE,
                gdi_type: g,
                ..shadow(640, 480)
            };
            assert!(decide(&on(), &r).is_ok(), "gdi {g}");
        }
        let r = Request {
            std_type: STD_GDISURFACE,
            gdi_type: GDI_EXISTINGSYSMEM,
            ..shadow(640, 480)
        };
        assert_eq!(decide(&on(), &r), Err(Why::ExistingSysmem));
        let r = Request {
            std_type: STD_STAGINGSURFACE,
            ..shadow(640, 480)
        };
        assert!(decide(&on(), &r).is_ok());
    }

    #[test]
    fn the_primary_the_texture_and_the_unknown_are_not_this_modules() {
        let mk = |s, g| Request {
            std_type: s,
            gdi_type: g,
            ..shadow(640, 480)
        };
        assert_eq!(
            decide(&on(), &mk(STD_SHAREDPRIMARYSURFACE, 0)),
            Err(Why::Primary)
        );
        assert_eq!(
            decide(&on(), &mk(STD_GDISURFACE, GDI_TEXTURE)),
            Err(Why::GpuOnly)
        );
        assert_eq!(decide(&on(), &mk(STD_VGPU, 0)), Err(Why::Unsupported));
        assert_eq!(
            decide(&on(), &mk(STD_FENCESTORAGE, 0)),
            Err(Why::Unsupported)
        );
    }

    #[test]
    fn refusals_are_given_in_the_tables_order() {
        let mut p = on();
        p.budget_bytes = 1 << 20;
        // Everything wrong at once: an unsupported type, a bad format, a huge extent, no
        // cached view, a full budget.
        let mut r = Request {
            std_type: STD_VGPU,
            gdi_type: GDI_EXISTINGSYSMEM,
            d3dddi_format: 99,
            width: 100_000,
            height: 100_000,
            alloc_cached: false,
            live_bytes: u64::MAX,
        };
        let mut p_off = p;
        p_off.enabled = false;
        assert_eq!(decide(&p_off, &r), Err(Why::NotEnabled));
        assert_eq!(decide(&p, &r), Err(Why::Unsupported));
        r.std_type = STD_GDISURFACE;
        assert_eq!(decide(&p, &r), Err(Why::ExistingSysmem));
        r.gdi_type = GDI_STAGING;
        assert_eq!(decide(&p, &r), Err(Why::Format));
        r.d3dddi_format = D3DDDIFMT_X8R8G8B8;
        assert_eq!(decide(&p, &r), Err(Why::Extent));
        r.width = 4096;
        r.height = 16384;
        // 4096 x 4 = 16384 bytes a row, 16384 rows: 256 MiB, inside the cap; shrink the cap.
        p.max_bytes = 100 << 20;
        assert_eq!(decide(&p, &r), Err(Why::Size));
        p.max_bytes = MAX_FOREIGN_RESOURCE_BYTES;
        assert_eq!(decide(&p, &r), Err(Why::NotCached));
        r.alloc_cached = true;
        assert_eq!(decide(&p, &r), Err(Why::Budget));
        r.live_bytes = 0;
        p.budget_bytes = 1 << 30;
        assert!(decide(&p, &r).is_ok());
    }

    #[test]
    fn extents_and_sizes_are_bounded_by_the_foreign_record() {
        let r = |w, h| Request {
            alloc_cached: true,
            ..shadow(w, h)
        };
        assert_eq!(decide(&on(), &r(0, 100)), Err(Why::Extent));
        assert_eq!(decide(&on(), &r(100, 0)), Err(Why::Extent));
        assert_eq!(decide(&on(), &r(16385, 100)), Err(Why::Extent));
        assert_eq!(decide(&on(), &r(100, 16385)), Err(Why::Extent));
        // 16384 x 16384 x 4 is exactly 1 GiB: the record's own bound, over the 1 GiB budget
        // with anything else alive and not over it alone.
        let max = decide(&on(), &r(16384, 16384)).unwrap();
        assert_eq!(max.size, 1 << 30);
        assert!(max.foreign_layout(max.size).is_some());
        let mut q = on();
        q.budget_bytes = 0;
        let mut big = r(16384, 16384);
        big.live_bytes = 1 << 40;
        assert!(decide(&q, &big).is_ok(), "budget 0 is unbounded");
        // The largest stride the record allows: 16384 x 4 = 65536 is far below 1 MiB.
        assert!(max.pitch <= MAX_STRIDE);
    }

    #[test]
    fn a_write_combined_cpu_view_is_refused() {
        let mut r = shadow(1920, 1080);
        r.alloc_cached = false;
        assert_eq!(decide(&on(), &r), Err(Why::NotCached));
    }

    #[test]
    fn the_budget_counts_what_is_alive_plus_this_surface() {
        let p = Policy {
            budget_bytes: 20 << 20,
            ..on()
        };
        let mut r = shadow(1920, 1080); // 8_294_400 -> 8_294_400 bytes (already a page multiple)
        r.live_bytes = 0;
        assert!(decide(&p, &r).is_ok());
        r.live_bytes = 2 * 8_294_400;
        assert_eq!(decide(&p, &r), Err(Why::Budget));
        r.live_bytes = (20 << 20) - 8_294_400;
        assert!(decide(&p, &r).is_ok(), "exactly the budget fits");
        r.live_bytes += 1;
        assert_eq!(decide(&p, &r), Err(Why::Budget));
        // No overflow with an absurd live count.
        r.live_bytes = u64::MAX;
        assert_eq!(decide(&p, &r), Err(Why::Budget));
    }

    #[test]
    fn the_why_codes_are_stable() {
        let all = [
            (Why::NotEnabled, 1),
            (Why::Primary, 2),
            (Why::GpuOnly, 3),
            (Why::Unsupported, 4),
            (Why::ExistingSysmem, 5),
            (Why::Format, 6),
            (Why::Extent, 7),
            (Why::Size, 8),
            (Why::NotCached, 9),
            (Why::Budget, 10),
        ];
        for (w, c) in all {
            assert_eq!(w.code(), c);
        }
    }

    #[test]
    fn the_census_has_a_distinct_named_slot_for_every_kind() {
        let mut seen = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut put = |s: u32, g: u32| {
            let slot = hist_slot(s, g);
            assert!(slot < HIST_SLOTS);
            seen.insert(slot);
            names.insert(hist_name(slot));
            slot
        };
        assert_eq!(put(STD_SHAREDPRIMARYSURFACE, 0), 0);
        assert_eq!(put(STD_SHADOWSURFACE, 0), 1);
        assert_eq!(put(STD_STAGINGSURFACE, 0), 2);
        for g in 0..=GDI_TEXTURE_CPUVISIBLE_CROSSADAPTER {
            assert_eq!(put(STD_GDISURFACE, g), 3 + g as usize);
        }
        assert_eq!(put(STD_GDISURFACE, 9), 12);
        assert_eq!(put(STD_GDISURFACE, u32::MAX), 12);
        assert_eq!(put(STD_VGPU, 0), 13);
        assert_eq!(put(0, 0), 13);
        assert_eq!(seen.len(), HIST_SLOTS);
        assert_eq!(names.len(), HIST_SLOTS, "names are unique");
        for slot in 0..HIST_SLOTS {
            let n = hist_name(slot);
            assert!(n.len() <= 14, "{:?}", std::str::from_utf8(n));
            assert!(n.starts_with(b"StdN"));
        }
        // A slot past the table is the catch-all, not a panic.
        assert_eq!(hist_name(HIST_SLOTS + 5), b"StdNOther");
    }

    #[test]
    fn the_gdi_type_of_a_non_gdi_allocation_does_not_move_its_slot() {
        assert_eq!(hist_slot(STD_SHADOWSURFACE, 7), 1);
        assert_eq!(hist_slot(STD_STAGINGSURFACE, 7), 2);
        assert_eq!(hist_slot(STD_SHAREDPRIMARYSURFACE, 7), 0);
    }
}
