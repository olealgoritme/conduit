//! GDI hardware acceleration limited to what the KMD can execute (`GdiAccel`, lane F fallback A):
//! the pure half. Design, sources and the hardware procedure: `docs/vram-redirection.md` section
//! 9. Nothing here does I/O; the I/O half is `kmd_render/src/ddi/gdi_accel.rs` (the caps, the
//! `DxgkDdiRenderKm` translation, the job queue and the fence gate) and
//! `kmd_render/src/ddi/gdi_exec.rs` (the executor on the copy-engine channel and the CPU).
//!
//! What is here:
//!
//! * [`resolve_caps`]: the `DXGK_PRESENTATIONCAPS` word for a `GdiAccel` value. 0 (the default)
//!   is exactly 0, the word the driver has reported since GDI acceleration was removed.
//! * [`Parser`], [`Cmd`]: a bounds-checked reader of the `DXGK_RENDERKM_COMMAND` stream CDD hands
//!   `DxgkDdiRenderKm` (x64 layout of WDK 10.0.26100.0 `d3dkmddi.h`, offsets in [`layout`] and
//!   checked against a C compile of the same declarations).
//! * [`plan`], [`Engine`], [`Why`]: which engine runs one command (copy engine, CPU, or nothing)
//!   and why not the copy engine.
//! * [`fill`], [`fill_rect`], [`copy_rect`]: the copy-engine words of a color fill (the remap
//!   unit's `CONST_A`, as Mesa NVK's `nvk_cmd_fill_memory_ce`) and of a pitch-linear rectangle copy.
//! * [`cpu`]: the reference executor of every operation (the formulas of the Microsoft Learn
//!   pages of each `DXGK_GDIARG_*`), used for the operations the copy engine cannot do and by the
//!   tests as the oracle of the copy-engine plan.
//! * [`Private`]: the record `DxgkDdiRenderKm` leaves in the DMA buffer's private data for
//!   `DxgkDdiSubmitCommand` (the job id), [`Timeline`]: the job sequence the WDDM fence waits on.
//! * [`KNOB`], [`COUNTERS`]: names (`Gdi*`, at most 14 characters).
//!
//! The contract this implements (Microsoft Learn, "GDI Hardware Acceleration" and the pages it
//! links): a driver that sets `SupportKernelModeCommandBuffer` must implement
//! `DxgkDdiCreateAllocation`, `DxgkDdiGetStandardAllocationDriverData` and `DxgkDdiRenderKm`, and
//! `DxgkDdiRenderKm` must translate the WHOLE command buffer (its return codes have no "this
//! operation is not supported, do it on the CPU"). The operations a driver may decline are
//! declined through the caps word only: `NoSameBitmap*`, `NoSameBitmapOverlapped*`, and by
//! leaving `SupportAllBltRops`, `SupportMirrorStretchBlt` and `SupportMonoStretchBltModes` clear.
//! So every BitBlt, ColorFill, AlphaBlend, StretchBlt, TransparentBlt and ClearTypeBlend CDD sends
//! must be executed; the KMD executes copies and fills on the copy engine and everything else on
//! the CPU.

use crate::ce_present::{self as cp, Gen, Push, PushError};

// ── caps ───────────────────────────────────────────────────────────────────────────────────────

/// `DXGK_PRESENTATIONCAPS` bit positions, in the C bitfield order of WDK 10.0.26100.0
/// `d3dkmddi.h` (`_DXGK_PRESENTATIONCAPS`, lines 1924-1965). The Learn page's "equivalent to
/// setting the Nth bit" sentences are wrong for every field after the 4-bit `AlignmentShift`; the
/// header's order is the authority.
pub mod pcaps {
    pub const NO_SCREEN_TO_SCREEN_BLT: u32 = 1 << 0;
    pub const NO_OVERLAP_SCREEN_BLT: u32 = 1 << 1;
    pub const SUPPORT_KERNEL_MODE_COMMAND_BUFFER: u32 = 1 << 2;
    pub const NO_SAME_BITMAP_ALPHA_BLEND: u32 = 1 << 3;
    pub const NO_SAME_BITMAP_STRETCH_BLT: u32 = 1 << 4;
    pub const NO_SAME_BITMAP_TRANSPARENT_BLT: u32 = 1 << 5;
    pub const NO_SAME_BITMAP_OVERLAPPED_ALPHA_BLEND: u32 = 1 << 6;
    pub const NO_SAME_BITMAP_OVERLAPPED_STRETCH_BLT: u32 = 1 << 7;
    pub const DRIVER_SUPPORTS_CDD_DWM_INTEROP: u32 = 1 << 8;
    /// 4 bits at 10: the `XxxPitch` alignment is `1 << AlignmentShift` bytes, at least 2.
    pub const ALIGNMENT_SHIFT_SHIFT: u32 = 10;
    /// 3 bits at 14: the maximum texture width is `2^(shift + 11)`.
    pub const MAX_TEXTURE_WIDTH_SHIFT_SHIFT: u32 = 14;
    /// 3 bits at 17.
    pub const MAX_TEXTURE_HEIGHT_SHIFT_SHIFT: u32 = 17;
    pub const SUPPORT_ALL_BLT_ROPS: u32 = 1 << 20;
    pub const SUPPORT_MIRROR_STRETCH_BLT: u32 = 1 << 21;
    pub const SUPPORT_MONO_STRETCH_BLT_MODES: u32 = 1 << 22;
    pub const STAGING_RECT_START_PITCH_ALIGNED: u32 = 1 << 23;
    pub const NO_SAME_BITMAP_BIT_BLT: u32 = 1 << 24;
    pub const NO_SAME_BITMAP_OVERLAPPED_BIT_BLT: u32 = 1 << 25;
    pub const NO_TEMP_SURFACE_FOR_CLEAR_TYPE_BLEND: u32 = 1 << 27;
    pub const SUPPORT_SOFTWARE_DEVICE_BITMAPS: u32 = 1 << 28;
    pub const NO_CACHE_COHERENT_APERTURE_MEMORY: u32 = 1 << 29;
    pub const SUPPORT_LINEAR_HEAP: u32 = 1 << 30;
}

/// The service knob (the service key, `diag.rs` `KnobName`): 0 off (default: the caps word is 0,
/// `DxgkDdiRenderKm` keeps its pass-through body), 1 GDI acceleration on (the caps word of
/// [`ACCEL_CAPS`], the RenderKm translation, the executor). Two one-bit experiments without GDI
/// acceleration (RenderKm stays the pass-through, nothing else changes): 2 reports only
/// `DriverSupportsCddDwmInterop` (CDD presents into DWM's UMD-created textures), 3 only
/// `SupportSoftwareDeviceBitmaps` (`TEXTURE_CPUVISIBLE` redirection bitmaps). Any other value is
/// 0. Read at AddAdapter and StartDevice with the other caps knobs.
pub const KNOB: &str = "GdiAccel";
pub const KNOB_DEFAULT: u32 = 0;

/// `AlignmentShift` 2: 4-byte pitches, the documented minimum (every 32 bpp row is already that).
pub const ALIGNMENT_SHIFT: u32 = 2;
/// `MaxTextureWidthShift` / `MaxTextureHeightShift` 3: 16384 texels, the largest GDI surface the
/// KMD takes (5120x1440 and 7680x4320 fit). CDD handles a larger surface itself.
pub const MAX_TEXTURE_SHIFT: u32 = 3;
/// The largest width and height the caps let CDD send: `2^(MAX_TEXTURE_SHIFT + 11)`.
pub const MAX_TEXTURE_DIM: u32 = 1 << (MAX_TEXTURE_SHIFT + 11);

/// The word with `GdiAccel` = 1.
///
/// * `SupportKernelModeCommandBuffer`: the opt-in. Microsoft: report it "only if the
///   cache-coherent GPU aperture segment exists" (segment 1 is one, `CacheCoherent`).
/// * Every `NoSameBitmap*`/`NoSameBitmapOverlapped*` bit except the plain `NoSameBitmapBitBlt`:
///   CDD does those itself (through a temporary surface). A non-overlapping BitBlt inside one
///   surface stays accepted (a copy-engine copy of disjoint rectangles is exact); an overlapping
///   one (a scroll) would need a copy direction or a bounce, which the copy engine's line order
///   does not promise.
/// * `NoTempSurfaceForClearTypeBlend`: the CPU executor reads the destination directly.
/// * `SupportAllBltRops`, `SupportMirrorStretchBlt`, `SupportMonoStretchBltModes` clear: CDD sends
///   only the named ROPs of `DXGK_GDIROP_BITBLT`/`DXGK_GDIROP_COLORFILL`, no mirroring and no
///   BLACKONWHITE/WHITEONBLACK modes (the CPU executor still implements ROP3 and mirroring, for a
///   build of Windows that sends them anyway: counted, never refused).
/// * `NoScreenToScreenBlt`/`NoOverlapScreenBlt` clear: they are about `DxgkDdiPresent` Blts within
///   the primary, not RenderKm, and the Present path is unchanged by this knob.
pub const ACCEL_CAPS: u32 = pcaps::SUPPORT_KERNEL_MODE_COMMAND_BUFFER
    | pcaps::NO_SAME_BITMAP_ALPHA_BLEND
    | pcaps::NO_SAME_BITMAP_STRETCH_BLT
    | pcaps::NO_SAME_BITMAP_TRANSPARENT_BLT
    | pcaps::NO_SAME_BITMAP_OVERLAPPED_ALPHA_BLEND
    | pcaps::NO_SAME_BITMAP_OVERLAPPED_STRETCH_BLT
    | pcaps::NO_SAME_BITMAP_OVERLAPPED_BIT_BLT
    | pcaps::NO_TEMP_SURFACE_FOR_CLEAR_TYPE_BLEND
    | (ALIGNMENT_SHIFT << pcaps::ALIGNMENT_SHIFT_SHIFT)
    | (MAX_TEXTURE_SHIFT << pcaps::MAX_TEXTURE_WIDTH_SHIFT_SHIFT)
    | (MAX_TEXTURE_SHIFT << pcaps::MAX_TEXTURE_HEIGHT_SHIFT_SHIFT);

/// The outcome of a `GdiAccel` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// GDI acceleration (RenderKm translation and executor) in force: knob 1 only.
    pub on: bool,
    /// The `DXGK_DRIVERCAPS.PresentationCaps` word (mirrored as `GdiCaps`).
    pub reported: u32,
}

/// `knob` with the prerequisites of `GdiAccel` = 1: GDI `TEXTURE` surfaces in RM video memory
/// (`RedirVram` = 1) and the copy-engine channel (`RmCopyEngine` = 1). Without them every GDI
/// texture is a Venus image no engine of the executor reaches, every command on it is dropped and
/// the desktop is garbage (362.1 row a): the caps are then not reported at all (`needs_more`).
pub const fn resolve_caps_with(knob: u32, redir_vram_on: bool, copy_engine_on: bool) -> Caps {
    if knob == 1 && !(redir_vram_on && copy_engine_on) {
        return Caps { on: false, reported: 0 };
    }
    resolve_caps(knob)
}

pub const fn resolve_caps(knob: u32) -> Caps {
    match knob {
        1 => Caps { on: true, reported: ACCEL_CAPS },
        2 => Caps { on: false, reported: pcaps::DRIVER_SUPPORTS_CDD_DWM_INTEROP },
        3 => Caps { on: false, reported: pcaps::SUPPORT_SOFTWARE_DEVICE_BITMAPS },
        _ => Caps { on: false, reported: 0 },
    }
}

// ── the command stream ─────────────────────────────────────────────────────────────────────────

/// Byte offsets of the x64 layouts (`RECT` 16 bytes, pointers 8, natural alignment). Checked
/// against `offsetof` of the same declarations compiled for x86_64 (the test
/// `layout_matches_the_c_compile` pins the numbers that run printed).
pub mod layout {
    /// `DXGK_RENDERKM_COMMAND`: `OpCode` 0, `CommandSize` 4, the union at 8 (pointer-aligned).
    pub const CMD_OPCODE: usize = 0;
    pub const CMD_SIZE: usize = 4;
    pub const CMD_ARG: usize = 8;
    /// `sizeof(DXGK_RENDERKM_COMMAND)`: 8 + the largest arm (BitBlt and ClearTypeBlend, 72).
    pub const CMD_BYTES: usize = 80;

    pub const BITBLT_BYTES: usize = 72;
    pub const BB_SRC_RECT: usize = 0;
    pub const BB_DST_RECT: usize = 16;
    pub const BB_SRC_INDEX: usize = 32;
    pub const BB_DST_INDEX: usize = 36;
    pub const BB_NUM_SUB: usize = 40;
    pub const BB_SUB_PTR: usize = 48;
    pub const BB_ROP: usize = 56;
    pub const BB_ROP3: usize = 58;
    pub const BB_SRC_PITCH: usize = 60;
    pub const BB_DST_PITCH: usize = 64;

    pub const COLORFILL_BYTES: usize = 40;
    pub const CF_DST_RECT: usize = 0;
    pub const CF_DST_INDEX: usize = 16;
    pub const CF_NUM_SUB: usize = 20;
    pub const CF_SUB_PTR: usize = 24;
    pub const CF_COLOR: usize = 32;
    pub const CF_ROP: usize = 36;
    pub const CF_ROP3: usize = 38;

    pub const ALPHABLEND_BYTES: usize = 64;
    pub const AB_SRC_RECT: usize = 0;
    pub const AB_DST_RECT: usize = 16;
    pub const AB_SRC_INDEX: usize = 32;
    pub const AB_DST_INDEX: usize = 36;
    pub const AB_NUM_SUB: usize = 40;
    pub const AB_SUB_PTR: usize = 48;
    pub const AB_CONST_ALPHA: usize = 56;
    pub const AB_HAS_ALPHA: usize = 57;
    pub const AB_SRC_PITCH: usize = 60;

    /// Note the order: `DstAllocationIndex` BEFORE `SrcAllocationIndex` in this one.
    pub const STRETCHBLT_BYTES: usize = 64;
    pub const SB_SRC_RECT: usize = 0;
    pub const SB_DST_RECT: usize = 16;
    pub const SB_DST_INDEX: usize = 32;
    pub const SB_SRC_INDEX: usize = 36;
    pub const SB_NUM_SUB: usize = 40;
    pub const SB_SUB_PTR: usize = 48;
    pub const SB_FLAGS: usize = 56;
    pub const SB_SRC_PITCH: usize = 60;

    pub const TRANSPARENTBLT_BYTES: usize = 64;
    pub const TB_SRC_RECT: usize = 0;
    pub const TB_DST_RECT: usize = 16;
    pub const TB_SRC_INDEX: usize = 32;
    pub const TB_DST_INDEX: usize = 36;
    pub const TB_COLOR: usize = 40;
    pub const TB_NUM_SUB: usize = 44;
    pub const TB_SUB_PTR: usize = 48;
    pub const TB_FLAGS: usize = 56;
    pub const TB_SRC_PITCH: usize = 60;

    pub const CLEARTYPE_BYTES: usize = 72;
    pub const CT_DST_RECT: usize = 0;
    pub const CT_TMP_INDEX: usize = 16;
    pub const CT_GAMMA_INDEX: usize = 20;
    pub const CT_ALPHA_INDEX: usize = 24;
    pub const CT_DST_INDEX: usize = 28;
    pub const CT_OFFSET_X: usize = 32;
    pub const CT_OFFSET_Y: usize = 36;
    pub const CT_COLOR: usize = 40;
    pub const CT_GAMMA: usize = 44;
    pub const CT_NUM_SUB: usize = 48;
    pub const CT_SUB_PTR: usize = 56;
    pub const CT_ALPHA_PITCH: usize = 64;
    pub const CT_COLOR2: usize = 68;

    pub const RECT_BYTES: usize = 16;
}

/// `DXGK_RENDERKM_OPERATION`.
pub mod op {
    pub const BITBLT: u32 = 1;
    pub const COLORFILL: u32 = 2;
    pub const ALPHABLEND: u32 = 3;
    pub const STRETCHBLT: u32 = 4;
    /// "Driver ignores this command" (`d3dkmddi.h`).
    pub const ESCAPE: u32 = 5;
    pub const TRANSPARENTBLT: u32 = 6;
    pub const CLEARTYPEBLEND: u32 = 7;
}

/// `DXGK_GDIROP_BITBLT`.
pub mod rop {
    pub const SRCCOPY: u16 = 1;
    pub const SRCINVERT: u16 = 2;
    pub const SRCAND: u16 = 3;
    pub const SRCOR: u16 = 4;
    pub const ROP3: u16 = 5;
}

/// `DXGK_GDIROP_COLORFILL`.
pub mod cfrop {
    pub const PATCOPY: u16 = 1;
    pub const PATINVERT: u16 = 2;
    pub const PDXN: u16 = 3;
    pub const DSTINVERT: u16 = 4;
    pub const PATAND: u16 = 5;
    pub const PATOR: u16 = 6;
    pub const ROP3: u16 = 7;
}

/// `D3DKM_INVALID_GAMMA_INDEX`.
pub const INVALID_GAMMA: u32 = 0xffff_ffff;

/// A Win32 `RECT`: lower-right exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, right, bottom }
    }

    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub const fn width(&self) -> u32 {
        if self.right > self.left {
            (self.right as i64 - self.left as i64) as u32
        } else {
            0
        }
    }

    pub const fn height(&self) -> u32 {
        if self.bottom > self.top {
            (self.bottom as i64 - self.top as i64) as u32
        } else {
            0
        }
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect {
            left: self.left.max(o.left),
            top: self.top.max(o.top),
            right: self.right.min(o.right),
            bottom: self.bottom.min(o.bottom),
        }
    }

    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect {
            left: self.left.min(o.left),
            top: self.top.min(o.top),
            right: self.right.max(o.right),
            bottom: self.bottom.max(o.bottom),
        }
    }

    pub fn overlaps(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }

    /// Inside `0..w` x `0..h`.
    pub const fn within(&self, w: u32, h: u32) -> bool {
        self.left >= 0
            && self.top >= 0
            && self.right >= self.left
            && self.bottom >= self.top
            && self.right as i64 <= w as i64
            && self.bottom as i64 <= h as i64
    }

    pub const fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            left: self.left.wrapping_add(dx),
            top: self.top.wrapping_add(dy),
            right: self.right.wrapping_add(dx),
            bottom: self.bottom.wrapping_add(dy),
        }
    }
}

fn rd_u8(b: &[u8], at: usize) -> Option<u8> {
    b.get(at).copied()
}

fn rd_u16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd_i32(b: &[u8], at: usize) -> Option<i32> {
    rd_u32(b, at).map(|v| v as i32)
}

fn rd_u64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

fn rd_rect(b: &[u8], at: usize) -> Option<Rect> {
    Some(Rect {
        left: rd_i32(b, at)?,
        top: rd_i32(b, at + 4)?,
        right: rd_i32(b, at + 8)?,
        bottom: rd_i32(b, at + 12)?,
    })
}

/// Where a command's sub-rectangles are. The pointer is a kernel address into dxgkrnl's buffer;
/// when it lies inside the command buffer the parser reads them from the slice itself, otherwise
/// the I/O half copies `count` rectangles from the (trusted, kernel) pointer: "Access to the
/// kernel buffers does not require protection from try/except code" (`DxgkDdiRenderKm`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubRects {
    /// `NumSubRects` 0: the destination rectangle alone (inference: no clipping; see 10.6 of
    /// `docs/vram-redirection.md`, unverified).
    None,
    /// At this byte offset of the command buffer.
    Inline { offset: usize, count: u32 },
    /// Outside the command buffer.
    External { ptr: u64, count: u32 },
}

impl SubRects {
    pub const fn count(&self) -> u32 {
        match *self {
            SubRects::None => 0,
            SubRects::Inline { count, .. } | SubRects::External { count, .. } => count,
        }
    }
}

/// The most sub-rectangles one command may carry. CDD clips against the window's visible region;
/// a few hundred is already a pathological region. A larger count is refused as a malformed
/// command (`Bad::SubRects`), so a corrupted count cannot make the KMD walk megabytes.
pub const MAX_SUB_RECTS: u32 = 4096;

/// One decoded command. Allocation indices are unchecked here (the I/O half resolves them against
/// the allocation list and refuses an index past its end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    BitBlt {
        src: Rect,
        dst: Rect,
        src_index: u32,
        dst_index: u32,
        subs: SubRects,
        rop: u16,
        rop3: u16,
        src_pitch: u32,
        dst_pitch: u32,
    },
    ColorFill {
        dst: Rect,
        dst_index: u32,
        subs: SubRects,
        color: u32,
        rop: u16,
        rop3: u16,
    },
    AlphaBlend {
        src: Rect,
        dst: Rect,
        src_index: u32,
        dst_index: u32,
        subs: SubRects,
        const_alpha: u8,
        has_alpha: bool,
        src_pitch: u32,
    },
    StretchBlt {
        src: Rect,
        dst: Rect,
        src_index: u32,
        dst_index: u32,
        subs: SubRects,
        /// `Mode` (16 bits), `MirrorX` bit 16, `MirrorY` bit 17.
        flags: u32,
        src_pitch: u32,
    },
    TransparentBlt {
        src: Rect,
        dst: Rect,
        src_index: u32,
        dst_index: u32,
        color: u32,
        subs: SubRects,
        honor_alpha: bool,
        src_pitch: u32,
    },
    ClearTypeBlend {
        dst: Rect,
        tmp_index: u32,
        gamma_index: u32,
        alpha_index: u32,
        dst_index: u32,
        dst_to_alpha_x: i32,
        dst_to_alpha_y: i32,
        color: u32,
        gamma: u32,
        subs: SubRects,
        alpha_pitch: u32,
        color2: u32,
    },
    /// `DXGK_GDIOP_ESCAPE`: ignored by contract.
    Escape,
}

impl Cmd {
    pub const fn opcode(&self) -> u32 {
        match self {
            Cmd::BitBlt { .. } => op::BITBLT,
            Cmd::ColorFill { .. } => op::COLORFILL,
            Cmd::AlphaBlend { .. } => op::ALPHABLEND,
            Cmd::StretchBlt { .. } => op::STRETCHBLT,
            Cmd::TransparentBlt { .. } => op::TRANSPARENTBLT,
            Cmd::ClearTypeBlend { .. } => op::CLEARTYPEBLEND,
            Cmd::Escape => op::ESCAPE,
        }
    }

    pub const fn subs(&self) -> SubRects {
        match *self {
            Cmd::BitBlt { subs, .. }
            | Cmd::ColorFill { subs, .. }
            | Cmd::AlphaBlend { subs, .. }
            | Cmd::StretchBlt { subs, .. }
            | Cmd::TransparentBlt { subs, .. }
            | Cmd::ClearTypeBlend { subs, .. } => subs,
            Cmd::Escape => SubRects::None,
        }
    }

    pub const fn dst_rect(&self) -> Rect {
        match *self {
            Cmd::BitBlt { dst, .. }
            | Cmd::ColorFill { dst, .. }
            | Cmd::AlphaBlend { dst, .. }
            | Cmd::StretchBlt { dst, .. }
            | Cmd::TransparentBlt { dst, .. }
            | Cmd::ClearTypeBlend { dst, .. } => dst,
            Cmd::Escape => Rect::new(0, 0, 0, 0),
        }
    }

    /// The allocation-list indices the command reads and writes: `(dst, sources...)`; unused
    /// slots are `None`. ClearTypeBlend reads the gamma and alpha surfaces (its temporary surface
    /// is not used: `NoTempSurfaceForClearTypeBlend`).
    pub const fn indices(&self) -> (Option<u32>, [Option<u32>; 2]) {
        match *self {
            Cmd::BitBlt { src_index, dst_index, .. }
            | Cmd::AlphaBlend { src_index, dst_index, .. }
            | Cmd::StretchBlt { src_index, dst_index, .. }
            | Cmd::TransparentBlt { src_index, dst_index, .. } => {
                (Some(dst_index), [Some(src_index), None])
            }
            Cmd::ColorFill { dst_index, .. } => (Some(dst_index), [None, None]),
            Cmd::ClearTypeBlend { dst_index, gamma, gamma_index, alpha_index, .. } => (
                Some(dst_index),
                [Some(alpha_index), if gamma == INVALID_GAMMA { None } else { Some(gamma_index) }],
            ),
            Cmd::Escape => (None, [None, None]),
        }
    }
}

/// Why a command buffer was refused. `code` is stable (appended, never renumbered); the I/O half
/// counts it in `GdiBad` and returns `STATUS_INVALID_PARAMETER` for the whole buffer (dxgkrnl's
/// documented answer to "instruction parameters the hardware cannot support").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// `CommandSize` smaller than the command's arm, not 4-aligned, or past the buffer.
    Size = 1,
    /// An opcode outside 1..=7.
    Opcode = 2,
    /// A sub-rectangle count above [`MAX_SUB_RECTS`], or an inline array past the command.
    SubRects = 3,
    /// A trailing fragment shorter than a command header.
    Trailer = 4,
}

impl Bad {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// A forward reader of one command buffer. `base` is the kernel address of `bytes[0]` (the
/// buffer's `pCommand`), used only to tell inline sub-rectangle arrays from external ones.
pub struct Parser<'a> {
    bytes: &'a [u8],
    base: u64,
    at: usize,
}

impl<'a> Parser<'a> {
    pub fn new(bytes: &'a [u8], base: u64) -> Self {
        Self { bytes, base, at: 0 }
    }

    /// Byte offset of the next command.
    pub fn offset(&self) -> usize {
        self.at
    }

    fn subs(&self, cmd_start: usize, cmd_end: usize, arg: usize, count_at: usize, ptr_at: usize) -> Result<SubRects, Bad> {
        let count = rd_u32(self.bytes, arg + count_at).ok_or(Bad::Size)?;
        if count == 0 {
            return Ok(SubRects::None);
        }
        if count > MAX_SUB_RECTS {
            return Err(Bad::SubRects);
        }
        let ptr = rd_u64(self.bytes, arg + ptr_at).ok_or(Bad::Size)?;
        let len = count as u64 * layout::RECT_BYTES as u64;
        let lo = self.base;
        let hi = self.base.saturating_add(self.bytes.len() as u64);
        if ptr >= lo && ptr < hi {
            let off = (ptr - lo) as usize;
            // Inline: must lie inside THIS command, after its arm.
            if off < cmd_start || (off as u64).saturating_add(len) > cmd_end as u64 {
                return Err(Bad::SubRects);
            }
            Ok(SubRects::Inline { offset: off, count })
        } else if ptr == 0 {
            Err(Bad::SubRects)
        } else {
            Ok(SubRects::External { ptr, count })
        }
    }

    /// The next command, `None` at the end, `Some(Err)` for a malformed one (the parser stops).
    pub fn next_cmd(&mut self) -> Option<Result<Cmd, Bad>> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let r = self.decode();
        match r {
            Ok((cmd, size)) => {
                self.at += size;
                Some(Ok(cmd))
            }
            Err(e) => {
                self.at = self.bytes.len();
                Some(Err(e))
            }
        }
    }

    fn decode(&self) -> Result<(Cmd, usize), Bad> {
        use layout::*;
        let start = self.at;
        let b = self.bytes;
        if b.len() - start < CMD_ARG {
            return Err(Bad::Trailer);
        }
        let opcode = rd_u32(b, start + CMD_OPCODE).ok_or(Bad::Trailer)?;
        let size = rd_u32(b, start + CMD_SIZE).ok_or(Bad::Trailer)? as usize;
        let need = match opcode {
            op::BITBLT => BITBLT_BYTES,
            op::COLORFILL => COLORFILL_BYTES,
            op::ALPHABLEND => ALPHABLEND_BYTES,
            op::STRETCHBLT => STRETCHBLT_BYTES,
            op::TRANSPARENTBLT => TRANSPARENTBLT_BYTES,
            op::CLEARTYPEBLEND => CLEARTYPE_BYTES,
            op::ESCAPE => 0,
            _ => return Err(Bad::Opcode),
        };
        let end = start.checked_add(size).ok_or(Bad::Size)?;
        if size < CMD_ARG + need || size % 4 != 0 || end > b.len() {
            return Err(Bad::Size);
        }
        let a = start + CMD_ARG;
        let cmd = match opcode {
            op::BITBLT => Cmd::BitBlt {
                src: rd_rect(b, a + BB_SRC_RECT).ok_or(Bad::Size)?,
                dst: rd_rect(b, a + BB_DST_RECT).ok_or(Bad::Size)?,
                src_index: rd_u32(b, a + BB_SRC_INDEX).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + BB_DST_INDEX).ok_or(Bad::Size)?,
                subs: self.subs(a + BITBLT_BYTES, end, a, BB_NUM_SUB, BB_SUB_PTR)?,
                rop: rd_u16(b, a + BB_ROP).ok_or(Bad::Size)?,
                rop3: rd_u16(b, a + BB_ROP3).ok_or(Bad::Size)?,
                src_pitch: rd_u32(b, a + BB_SRC_PITCH).ok_or(Bad::Size)?,
                dst_pitch: rd_u32(b, a + BB_DST_PITCH).ok_or(Bad::Size)?,
            },
            op::COLORFILL => Cmd::ColorFill {
                dst: rd_rect(b, a + CF_DST_RECT).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + CF_DST_INDEX).ok_or(Bad::Size)?,
                subs: self.subs(a + COLORFILL_BYTES, end, a, CF_NUM_SUB, CF_SUB_PTR)?,
                color: rd_u32(b, a + CF_COLOR).ok_or(Bad::Size)?,
                rop: rd_u16(b, a + CF_ROP).ok_or(Bad::Size)?,
                rop3: rd_u16(b, a + CF_ROP3).ok_or(Bad::Size)?,
            },
            op::ALPHABLEND => Cmd::AlphaBlend {
                src: rd_rect(b, a + AB_SRC_RECT).ok_or(Bad::Size)?,
                dst: rd_rect(b, a + AB_DST_RECT).ok_or(Bad::Size)?,
                src_index: rd_u32(b, a + AB_SRC_INDEX).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + AB_DST_INDEX).ok_or(Bad::Size)?,
                subs: self.subs(a + ALPHABLEND_BYTES, end, a, AB_NUM_SUB, AB_SUB_PTR)?,
                const_alpha: rd_u8(b, a + AB_CONST_ALPHA).ok_or(Bad::Size)?,
                has_alpha: rd_u8(b, a + AB_HAS_ALPHA).ok_or(Bad::Size)? != 0,
                src_pitch: rd_u32(b, a + AB_SRC_PITCH).ok_or(Bad::Size)?,
            },
            op::STRETCHBLT => Cmd::StretchBlt {
                src: rd_rect(b, a + SB_SRC_RECT).ok_or(Bad::Size)?,
                dst: rd_rect(b, a + SB_DST_RECT).ok_or(Bad::Size)?,
                src_index: rd_u32(b, a + SB_SRC_INDEX).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + SB_DST_INDEX).ok_or(Bad::Size)?,
                subs: self.subs(a + STRETCHBLT_BYTES, end, a, SB_NUM_SUB, SB_SUB_PTR)?,
                flags: rd_u32(b, a + SB_FLAGS).ok_or(Bad::Size)?,
                src_pitch: rd_u32(b, a + SB_SRC_PITCH).ok_or(Bad::Size)?,
            },
            op::TRANSPARENTBLT => Cmd::TransparentBlt {
                src: rd_rect(b, a + TB_SRC_RECT).ok_or(Bad::Size)?,
                dst: rd_rect(b, a + TB_DST_RECT).ok_or(Bad::Size)?,
                src_index: rd_u32(b, a + TB_SRC_INDEX).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + TB_DST_INDEX).ok_or(Bad::Size)?,
                color: rd_u32(b, a + TB_COLOR).ok_or(Bad::Size)?,
                subs: self.subs(a + TRANSPARENTBLT_BYTES, end, a, TB_NUM_SUB, TB_SUB_PTR)?,
                honor_alpha: rd_u32(b, a + TB_FLAGS).ok_or(Bad::Size)? & 1 != 0,
                src_pitch: rd_u32(b, a + TB_SRC_PITCH).ok_or(Bad::Size)?,
            },
            op::CLEARTYPEBLEND => Cmd::ClearTypeBlend {
                dst: rd_rect(b, a + CT_DST_RECT).ok_or(Bad::Size)?,
                tmp_index: rd_u32(b, a + CT_TMP_INDEX).ok_or(Bad::Size)?,
                gamma_index: rd_u32(b, a + CT_GAMMA_INDEX).ok_or(Bad::Size)?,
                alpha_index: rd_u32(b, a + CT_ALPHA_INDEX).ok_or(Bad::Size)?,
                dst_index: rd_u32(b, a + CT_DST_INDEX).ok_or(Bad::Size)?,
                dst_to_alpha_x: rd_i32(b, a + CT_OFFSET_X).ok_or(Bad::Size)?,
                dst_to_alpha_y: rd_i32(b, a + CT_OFFSET_Y).ok_or(Bad::Size)?,
                color: rd_u32(b, a + CT_COLOR).ok_or(Bad::Size)?,
                gamma: rd_u32(b, a + CT_GAMMA).ok_or(Bad::Size)?,
                subs: self.subs(a + CLEARTYPE_BYTES, end, a, CT_NUM_SUB, CT_SUB_PTR)?,
                alpha_pitch: rd_u32(b, a + CT_ALPHA_PITCH).ok_or(Bad::Size)?,
                color2: rd_u32(b, a + CT_COLOR2).ok_or(Bad::Size)?,
            },
            _ => Cmd::Escape,
        };
        Ok((normalize_rop(cmd), size))
    }

    /// Sub-rectangle `i` of an inline array.
    pub fn inline_rect(&self, offset: usize, i: u32) -> Option<Rect> {
        rd_rect(self.bytes, offset.checked_add(i as usize * layout::RECT_BYTES)?)
    }
}

/// A `ROP3` BitBlt whose result is the source alone (`0xCC`, and any code equal to it with P = 0)
/// is a SRCCOPY; a `ROP3` ColorFill whose result is the pattern alone (`0xF0`) is a PATCOPY. Both
/// then take the copy engine like the named forms.
pub fn normalize_rop(cmd: Cmd) -> Cmd {
    match cmd {
        Cmd::BitBlt { rop, rop3, .. } if rop == rop::ROP3 && cpu::bitblt_table(rop, rop3) == cpu::TABLE_S => {
            let mut c = cmd;
            if let Cmd::BitBlt { rop: r, .. } = &mut c {
                *r = rop::SRCCOPY;
            }
            c
        }
        Cmd::ColorFill { rop, rop3, .. } if rop == cfrop::ROP3 && cpu::rop3_code(rop3) == 0xF0 => {
            let mut c = cmd;
            if let Cmd::ColorFill { rop: r, .. } = &mut c {
                *r = cfrop::PATCOPY;
            }
            c
        }
        _ => cmd,
    }
}

/// Sub-rectangle `i` of an inline array of `bytes` (the command buffer).
pub fn inline_rect(bytes: &[u8], offset: usize, i: u32) -> Option<Rect> {
    rd_rect(bytes, offset.checked_add(i as usize * layout::RECT_BYTES)?)
}

/// Decode `count` rectangles from raw bytes (16 each), for an external array the I/O half copied.
pub fn rect_at(bytes: &[u8], i: u32) -> Option<Rect> {
    rd_rect(bytes, i as usize * layout::RECT_BYTES)
}

// ── coordinate transforms ──────────────────────────────────────────────────────────────────────

/// The source column of destination column `xd` for a scaled operation (StretchBlt, AlphaBlend,
/// TransparentBlt), the "truncate" variant of the Learn pages:
/// `Xs = truncate((Xd - Dl + 0.5) * Ws / Wd + Sl)`, in integers:
/// `Sl + floor((2 (Xd - Dl) + 1) Ws / (2 Wd))`. `Wd` 0 maps to `Sl`.
pub fn scale_coord(xd: i32, dl: i32, dw: u32, sl: i32, sw: u32) -> i32 {
    if dw == 0 {
        return sl;
    }
    let num = (2 * (xd as i64 - dl as i64) + 1) * sw as i64;
    let den = 2 * dw as i64;
    (sl as i64 + num.div_euclid(den)) as i32
}

/// The same with mirroring: the column read from the far end of the source rectangle.
pub fn scale_coord_mirror(xd: i32, dl: i32, dw: u32, sl: i32, sw: u32, mirror: bool) -> i32 {
    let x = scale_coord(xd, dl, dw, sl, sw);
    if mirror {
        sl + sl + sw as i32 - 1 - x
    } else {
        x
    }
}

/// An overlapping copy inside one surface (a scroll: `dst` = `src` moved by `(dx, dy)`) as a
/// sequence of non-overlapping copies that, executed IN ORDER, give the result of copying the whole
/// rectangle at once: bands of `|dy|` rows taken from the far side of the move (or, for a purely
/// horizontal move, columns of `|dx|`). Each band's source and destination are disjoint, and no
/// band reads rows an earlier band wrote. Calls `f(src_band, dst_band)` in execution order; returns
/// false (calling nothing) when the rectangles differ in size.
pub fn split_overlap(src: &Rect, dst: &Rect, mut f: impl FnMut(Rect, Rect)) -> bool {
    if src.width() != dst.width() || src.height() != dst.height() || dst.is_empty() {
        return false;
    }
    let dx = dst.left - src.left;
    let dy = dst.top - src.top;
    if !src.overlaps(dst) || (dx == 0 && dy == 0) {
        if dx != 0 || dy != 0 {
            f(*src, *dst);
        }
        return true;
    }
    if dy != 0 {
        let step = dy.unsigned_abs() as i32;
        let n = (dst.height() as i32 + step - 1) / step;
        for i in 0..n {
            // dy > 0 (down): bottom band first; dy < 0 (up): top band first.
            let (t, b) = if dy > 0 {
                let b = dst.bottom - i * step;
                ((b - step).max(dst.top), b)
            } else {
                let t = dst.top + i * step;
                (t, (t + step).min(dst.bottom))
            };
            let d = Rect::new(dst.left, t, dst.right, b);
            f(d.offset(-dx, -dy), d);
        }
    } else {
        let step = dx.unsigned_abs() as i32;
        let n = (dst.width() as i32 + step - 1) / step;
        for i in 0..n {
            let (l, r) = if dx > 0 {
                let r = dst.right - i * step;
                ((r - step).max(dst.left), r)
            } else {
                let l = dst.left + i * step;
                (l, (l + step).min(dst.right))
            };
            let d = Rect::new(l, dst.top, r, dst.bottom);
            f(d.offset(-dx, -dy), d);
        }
    }
    true
}

/// The source rectangle of a destination sub-rectangle for an unscaled BitBlt (the Learn
/// formula: `SrcSubRect = SubRect - DstRect.topleft + SrcRect.topleft`).
pub const fn bitblt_src(sub: &Rect, dst: &Rect, src: &Rect) -> Rect {
    sub.offset(src.left.wrapping_sub(dst.left), src.top.wrapping_sub(dst.top))
}

// ── the plan: which engine runs a command ──────────────────────────────────────────────────────

/// What the I/O half knows about one allocation of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    /// The host resource id (the identity `ce_vram::ce_surface` is keyed by).
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row of the CPU or GPU view (the allocation's authored pitch; for a
    /// `STAGING_CPUVISIBLE`/`EXISTINGSYSMEM` surface the command's own pitch wins, Learn
    /// `DXGK_GDIARG_BITBLT` remarks).
    pub pitch: u32,
    pub class: SurfaceClass,
    /// The allocation's D3DDDIFORMAT (0 unknown): picks the byte order of a copy ([`order_of`]).
    pub format: u32,
    /// Census only: `standard allocation type << 4 | GDI surface type` (each 4 bits) `| RM-backed
    /// << 8` (a KMD standard buffer whose memory is RM system memory, `ce_sysmem`'s object path).
    pub kind_bits: u32,
}

/// The byte order of a 32 bpp GDI surface format: `Some(false)` B G R A|X (`A8R8G8B8` 21,
/// `X8R8G8B8` 22), `Some(true)` R G B A|X (`A8B8G8R8` 32, `X8B8G8R8` 33), `None` for anything else
/// (unknown, or not 32 bpp: treated as the GDI default B G R A).
pub const fn order_of(d3dddi_format: u32) -> Option<bool> {
    match d3dddi_format {
        21 | 22 => Some(false),
        32 | 33 => Some(true),
        _ => None,
    }
}

/// Whether a copy from `src` to `dst` exchanges bytes 0 and 2 (one R G B, the other B G R). An
/// unknown format counts as B G R A, the format of every CDD surface.
pub const fn swaps_rb(src: &Surface, dst: &Surface) -> bool {
    let s = match order_of(src.format) {
        Some(v) => v,
        None => false,
    };
    let d = match order_of(dst.format) {
        Some(v) => v,
        None => false,
    };
    s != d
}

/// Where a surface's authoritative bytes are, as far as GDI acceleration can reach them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceClass {
    /// RM video memory with a copy-engine mapping (`RedirVram`, the V2 module): copy-engine
    /// copies and fills, CPU access through the bounce buffer.
    Vram,
    /// Guest system pages the KMD can read and write with the CPU (the allocation's system
    /// backing leases): CPU access only.
    System,
    /// Neither (a Venus image, an allocation VidMm holds in the Venus window, an unknown handle).
    Unreachable,
    /// A UMD-created NVK image with a foreign (RM) identity: a copy-engine SOURCE only
    /// (`ce_vram::foreign_source`, the producer's record or an import by resource id), for a
    /// SRCCOPY BitBlt into VRAM or a staging buffer. Anything else on it is dropped.
    Foreign,
}

/// Which engine executes a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Copy-engine copies (BitBlt SRCCOPY) or fills (ColorFill PATCOPY), all surfaces VRAM.
    Ce,
    /// The CPU reference executor over CPU views of every surface (bounce for VRAM).
    Cpu,
    /// Not executed (counted in `GdiDrop`); the fence still retires.
    Drop,
}

/// Why a command is not on the copy engine. `code` is stable (appended, never renumbered); the
/// last one is `GdiWhy`, the union of `1 << code` is `GdiMask`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// A ROP the copy engine cannot do (anything but SRCCOPY / PATCOPY).
    Rop = 1,
    /// AlphaBlend.
    Blend = 2,
    /// StretchBlt (scaled or mirrored).
    Stretch = 3,
    /// TransparentBlt.
    Transparent = 4,
    /// ClearTypeBlend.
    ClearType = 5,
    /// A surface is system memory (staging, existing sysmem), not VRAM.
    SystemSurface = 6,
    /// A BitBlt within one surface with overlapping rectangles.
    Overlap = 7,
    /// The copy engine refused or failed (channel down, ring full, push shape); the command was
    /// redone on the CPU.
    CeFailed = 8,
    /// A surface no engine reaches (dropped).
    Unreachable = 9,
    /// An allocation index past the allocation list, or a null handle (dropped).
    BadIndex = 10,
    /// A rectangle outside its surface (dropped; CDD promises sub-rectangles inside).
    OutOfBounds = 11,
    /// The CPU path failed (no memory for the scratch, bounce transfer refused) (dropped).
    CpuFailed = 12,
    /// The executor timed out on the copy engine and discharged the job (dropped).
    Timeout = 13,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }

    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// The engine for `cmd` given its surfaces (`dst`, `srcs` in [`Cmd::indices`] order; `None` for
/// an unused slot). Pure: the I/O half resolves the surfaces and may still fall back from `Ce` to
/// `Cpu` when a submission fails ([`Why::CeFailed`]).
pub fn plan(cmd: &Cmd, dst: Option<&Surface>, srcs: [Option<&Surface>; 2]) -> (Engine, Option<Why>) {
    if matches!(cmd, Cmd::Escape) {
        return (Engine::Drop, None);
    }
    let Some(dst) = dst else {
        return (Engine::Drop, Some(Why::BadIndex));
    };
    let (_, want) = cmd.indices();
    for (i, w) in want.iter().enumerate() {
        if w.is_some() && srcs[i].is_none() {
            return (Engine::Drop, Some(Why::BadIndex));
        }
    }
    let all = [Some(dst), srcs[0], srcs[1]];
    if all.iter().flatten().any(|s| s.class == SurfaceClass::Unreachable) {
        return (Engine::Drop, Some(Why::Unreachable));
    }
    if all.iter().flatten().any(|s| s.class == SurfaceClass::Foreign) {
        // Only a plain copy FROM the foreign image into a surface the executor writes, or a plain
        // copy INTO it from VRAM or a staging buffer (`ce_vram::foreign_write`).
        let srccopy = matches!(*cmd, Cmd::BitBlt { rop, .. } if rop == rop::SRCCOPY);
        let src_fgn = srcs[0].is_some_and(|s| s.class == SurfaceClass::Foreign);
        let dst_fgn = dst.class == SurfaceClass::Foreign;
        return match (srccopy, src_fgn, dst_fgn, dst.class, srcs[0].map(|s| s.class)) {
            (true, true, false, SurfaceClass::Vram, _) => (Engine::Ce, None),
            (true, true, false, _, _) => (Engine::Cpu, Some(Why::SystemSurface)),
            (true, false, true, _, Some(SurfaceClass::Vram)) => (Engine::Ce, None),
            (true, false, true, _, Some(SurfaceClass::System)) => (Engine::Cpu, Some(Why::SystemSurface)),
            _ => (Engine::Drop, Some(Why::Unreachable)),
        };
    }
    let all_vram = all.iter().flatten().all(|s| s.class == SurfaceClass::Vram);
    let ce_shape = match *cmd {
        Cmd::BitBlt { rop, src, dst: d, src_index, dst_index, .. } => {
            if rop != rop::SRCCOPY {
                Err(Why::Rop)
            } else if src_index == dst_index && bitblt_src(&d, &d, &src).overlaps(&d) {
                Err(Why::Overlap)
            } else {
                Ok(())
            }
        }
        Cmd::ColorFill { rop, .. } => {
            if rop == cfrop::PATCOPY {
                Ok(())
            } else {
                Err(Why::Rop)
            }
        }
        Cmd::AlphaBlend { .. } => Err(Why::Blend),
        Cmd::StretchBlt { .. } => Err(Why::Stretch),
        Cmd::TransparentBlt { .. } => Err(Why::Transparent),
        Cmd::ClearTypeBlend { .. } => Err(Why::ClearType),
        Cmd::Escape => Err(Why::BadIndex),
    };
    match ce_shape {
        Ok(()) if all_vram => (Engine::Ce, None),
        Ok(()) => (Engine::Cpu, Some(Why::SystemSurface)),
        Err(w) => (Engine::Cpu, Some(w)),
    }
}

// ── copy-engine words ──────────────────────────────────────────────────────────────────────────

/// `SET_REMAP_COMPONENTS` of a fill: every destination component `CONST_A`, `COMPONENT_SIZE_FOUR`
/// (3 at 17:16), one source and one destination component (0 at 21:20 and 25:24): one 32-bit
/// element per pixel. Mesa NVK `nvk_cmd_fill_memory_ce` programs the same fields.
pub const FILL_COMPONENTS: u32 = 4 | (4 << 4) | (4 << 8) | (4 << 12) | (3 << 16);

/// dwords one [`fill_rect`] adds after [`fill`]'s state: `OFFSET_IN_UPPER..LINE_COUNT` (1 + 8),
/// `LAUNCH_DMA` (1 + 1).
pub const FILL_RECT_DWORDS: usize = 11;
/// dwords of [`fill`]'s state: `SET_REMAP_CONST_A, CONST_B, COMPONENTS` (1 + 3).
pub const FILL_STATE_DWORDS: usize = 4;
/// dwords one [`copy_rect`] adds (a pitch-linear copy without remap: 1 + 8, 1 + 1).
pub const COPY_RECT_DWORDS: usize = 11;
/// dwords the closure of `ce_channel::submit_build` must leave for the release it is handed
/// (host `SEM_ADDR_LO..SEM_EXECUTE`: 1 + 5).
pub const RELEASE_DWORDS: usize = 6;

/// The fill's remap state (once per push and color).
pub fn fill(p: &mut Push<'_>, color: u32) -> Result<(), PushError> {
    p.method(cp::SUBC_CE, cp::CE_SET_REMAP_CONST_A, &[color, color, FILL_COMPONENTS])
}

/// One filled rectangle of a pitch-linear 32 bpp surface whose byte 0 is at `va`.
/// `LINE_LENGTH_IN` is in 4-byte elements (the remap is on), `PITCH_OUT` in bytes; the source
/// offset is the destination's (the remap reads no source component, as NVK leaves it).
pub fn fill_rect(p: &mut Push<'_>, va: u64, pitch: u32, r: &Rect) -> Result<(), PushError> {
    if r.is_empty() || r.left < 0 || r.top < 0 || pitch == 0 || pitch % 4 != 0 {
        return Err(PushError::Shape);
    }
    if (r.right as u64) * 4 > pitch as u64 {
        return Err(PushError::Shape);
    }
    let start = va
        .checked_add(r.top as u64 * pitch as u64 + r.left as u64 * 4)
        .ok_or(PushError::Va)?;
    if start >= cp::MAX_VA {
        return Err(PushError::Va);
    }
    p.method(
        cp::SUBC_CE,
        cp::CE_OFFSET_IN_UPPER,
        &[
            (start >> 32) as u32,
            start as u32,
            (start >> 32) as u32,
            start as u32,
            pitch,
            pitch,
            r.width(),
            r.height(),
        ],
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

/// A pitch-linear 32 bpp surface on the copy-engine channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CeView {
    pub va: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
}

/// One copied rectangle `src_r` of `src` to the same-sized rectangle at `dst_r` of `dst` (32 bpp;
/// `swap_rb` exchanges bytes 0 and 2, for an R G B surface on one side and B G R on the other).
pub fn copy_rect(
    p: &mut Push<'_>,
    gen: Gen,
    src: &CeView,
    src_r: &Rect,
    dst: &CeView,
    dst_r: &Rect,
    swap_rb: bool,
) -> Result<(), PushError> {
    if src_r.width() != dst_r.width()
        || src_r.height() != dst_r.height()
        || src_r.is_empty()
        || !src_r.within(src.width, src.height)
        || !dst_r.within(dst.width, dst.height)
    {
        return Err(PushError::Shape);
    }
    let sva = src
        .va
        .checked_add(src_r.top as u64 * src.pitch as u64 + src_r.left as u64 * 4)
        .ok_or(PushError::Va)?;
    let dva = dst
        .va
        .checked_add(dst_r.top as u64 * dst.pitch as u64 + dst_r.left as u64 * 4)
        .ok_or(PushError::Va)?;
    cp::copy(
        p,
        gen,
        &cp::CopyRect {
            src_va: sva,
            dst_va: dva,
            src_pitch: src.pitch,
            dst_pitch: dst.pitch,
            line_bytes: src_r.width() * 4,
            lines: src_r.height(),
            layout: cp::SurfaceLayout::Pitch,
            dst_layout: cp::SurfaceLayout::Pitch,
            remap: if swap_rb { cp::Remap::SwapRb } else { cp::Remap::None },
            stamp: None,
        },
    )
}

/// How many rectangles of `per_rect` dwords fit in one push slot of `slot_dwords` after `state`
/// dwords of state and the release.
pub const fn rects_per_push(slot_dwords: usize, state: usize, per_rect: usize) -> usize {
    let room = slot_dwords.saturating_sub(state + RELEASE_DWORDS);
    if per_rect == 0 {
        0
    } else {
        room / per_rect
    }
}

// ── the CPU reference executor ─────────────────────────────────────────────────────────────────

/// The CPU executor. Every surface is a [`View`] of a window of the surface in SURFACE
/// coordinates (the I/O half reads the bounding box of the command in each surface into a
/// scratch, runs this, and writes the destination window back). Out-of-window reads are skipped
/// (counted by the caller through the return value), never panicking.
pub mod cpu {
    use super::*;

    /// A window `[x0, x0 + w) x [y0, y0 + h)` of a 32 bpp surface, `pitch` bytes per row.
    pub struct View<'a> {
        pub data: &'a [u8],
        pub pitch: usize,
        pub x0: i32,
        pub y0: i32,
        pub w: u32,
        pub h: u32,
    }

    pub struct ViewMut<'a> {
        pub data: &'a mut [u8],
        pub pitch: usize,
        pub x0: i32,
        pub y0: i32,
        pub w: u32,
        pub h: u32,
    }

    fn at(x0: i32, y0: i32, w: u32, h: u32, pitch: usize, x: i32, y: i32, bpp: usize) -> Option<usize> {
        let dx = x.checked_sub(x0)?;
        let dy = y.checked_sub(y0)?;
        if dx < 0 || dy < 0 || dx as u32 >= w || dy as u32 >= h {
            return None;
        }
        (dy as usize).checked_mul(pitch)?.checked_add(dx as usize * bpp)
    }

    impl View<'_> {
        pub fn get(&self, x: i32, y: i32) -> Option<u32> {
            let o = at(self.x0, self.y0, self.w, self.h, self.pitch, x, y, 4)?;
            rd_u32(self.data, o)
        }

        /// An 8 bpp read (the gamma table).
        pub fn get8(&self, x: i32, y: i32) -> Option<u8> {
            let o = at(self.x0, self.y0, self.w, self.h, self.pitch, x, y, 1)?;
            self.data.get(o).copied()
        }
    }

    impl ViewMut<'_> {
        pub fn get(&self, x: i32, y: i32) -> Option<u32> {
            let o = at(self.x0, self.y0, self.w, self.h, self.pitch, x, y, 4)?;
            rd_u32(self.data, o)
        }

        pub fn put(&mut self, x: i32, y: i32, v: u32) -> bool {
            let Some(o) = at(self.x0, self.y0, self.w, self.h, self.pitch, x, y, 4) else {
                return false;
            };
            match self.data.get_mut(o..o + 4) {
                Some(s) => {
                    s.copy_from_slice(&v.to_le_bytes());
                    true
                }
                None => false,
            }
        }
    }

    /// A GDI ternary raster operation on 32-bit words: bit `(P << 2) | (S << 1) | D` of `code`
    /// gives each result bit (`0xCC` SRCCOPY = S, `0xF0` PATCOPY = P, `0x55` DSTINVERT = !D).
    pub fn rop3(code: u8, p: u32, s: u32, d: u32) -> u32 {
        let mut r = 0u32;
        for i in 0..8u32 {
            if code & (1 << i) != 0 {
                let pm = if i & 4 != 0 { p } else { !p };
                let sm = if i & 2 != 0 { s } else { !s };
                let dm = if i & 1 != 0 { d } else { !d };
                r |= pm & sm & dm;
            }
        }
        r
    }

    /// The 8-bit ROP3 code of a `Rop3` field (low byte; see 10.6 of `docs/vram-redirection.md`).
    pub const fn rop3_code(rop3: u16) -> u8 {
        rop3 as u8
    }

    /// A BitBlt's raster operation as a two-input truth table (no pattern in a BitBlt: P = 0):
    /// bit `(S << 1) | D` gives each result bit. [`TABLE_S`] is a copy, [`TABLE_D`] leaves the
    /// destination.
    pub fn bitblt_table(rop: u16, r3: u16) -> u8 {
        match rop {
            super::rop::SRCCOPY => TABLE_S,
            super::rop::SRCINVERT => 0b0110,
            super::rop::SRCAND => 0b1000,
            super::rop::SRCOR => 0b1110,
            super::rop::ROP3 => rop3_code(r3) & 0x0f,
            _ => TABLE_D,
        }
    }

    pub const TABLE_S: u8 = 0b1100;
    pub const TABLE_D: u8 = 0b1010;

    /// [`bitblt_table`] applied to 32 bits at once.
    #[inline(always)]
    pub fn apply_table(t: u8, s: u32, d: u32) -> u32 {
        let m = |i: u8| if t & (1 << i) != 0 { u32::MAX } else { 0 };
        (m(3) & s & d) | (m(2) & s & !d) | (m(1) & !s & d) | (m(0) & !s & !d)
    }

    /// An unscaled BitBlt of `sub` done row by row with [`apply_table`] when the whole
    /// sub-rectangle lies inside both views (the common case); `false` leaves it to [`run`].
    pub fn bitblt_rows(cmd: &Cmd, sub: &Rect, dst: &mut ViewMut<'_>, src: &View<'_>, done: &mut Done) -> bool {
        let Cmd::BitBlt { src: sr, dst: dr, rop, rop3, .. } = *cmd else {
            return false;
        };
        if sub.is_empty() {
            return true;
        }
        let (sx, sy) = (sub.left - dr.left + sr.left, sub.top - dr.top + sr.top);
        let inside = |x0: i32, y0: i32, w: u32, h: u32, x: i32, y: i32| {
            x >= x0
                && y >= y0
                && (x as i64 + sub.width() as i64) <= x0 as i64 + w as i64
                && (y as i64 + sub.height() as i64) <= y0 as i64 + h as i64
        };
        if !inside(dst.x0, dst.y0, dst.w, dst.h, sub.left, sub.top) || !inside(src.x0, src.y0, src.w, src.h, sx, sy) {
            return false;
        }
        let t = bitblt_table(rop, rop3);
        let n = sub.width() as usize * 4;
        for row in 0..sub.height() as usize {
            let d0 = (sub.top - dst.y0) as usize * dst.pitch + row * dst.pitch + (sub.left - dst.x0) as usize * 4;
            let s0 = (sy - src.y0) as usize * src.pitch + row * src.pitch + (sx - src.x0) as usize * 4;
            let (Some(d), Some(s)) = (dst.data.get_mut(d0..d0 + n), src.data.get(s0..s0 + n)) else {
                return false;
            };
            if t == TABLE_S {
                d.copy_from_slice(s);
            } else {
                for (dp, sp) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    let sv = u32::from_le_bytes([sp[0], sp[1], sp[2], sp[3]]);
                    let dv = u32::from_le_bytes([dp[0], dp[1], dp[2], dp[3]]);
                    dp.copy_from_slice(&apply_table(t, sv, dv).to_le_bytes());
                }
            }
        }
        done.pixels += sub.width() as u64 * sub.height() as u64;
        true
    }

    pub fn bitblt_pixel(rop: u16, r3: u16, s: u32, d: u32) -> u32 {
        match rop {
            super::rop::SRCCOPY => s,
            super::rop::SRCINVERT => d ^ s,
            super::rop::SRCAND => d & s,
            super::rop::SRCOR => d | s,
            // No pattern in a BitBlt: P is 0 (only sent with SupportAllBltRops, which is clear).
            super::rop::ROP3 => rop3(rop3_code(r3), 0, s, d),
            _ => d,
        }
    }

    pub fn fill_pixel(rop: u16, r3: u16, c: u32, d: u32) -> u32 {
        match rop {
            cfrop::PATCOPY => c,
            cfrop::PATINVERT => d ^ c,
            cfrop::PDXN => !(c ^ d),
            cfrop::DSTINVERT => !d,
            cfrop::PATAND => d & c,
            cfrop::PATOR => d | c,
            cfrop::ROP3 => rop3(rop3_code(r3), c, 0, d),
            _ => d,
        }
    }

    fn ch(v: u32, shift: u32) -> u32 {
        (v >> shift) & 0xff
    }

    fn div255(x: u32) -> u32 {
        (x + 127) / 255
    }

    /// GDI AlphaBlend with `AC_SRC_OVER` (premultiplied source when `has_alpha`).
    pub fn blend_pixel(s: u32, d: u32, const_alpha: u8, has_alpha: bool) -> u32 {
        let ca = const_alpha as u32;
        let mut out = 0u32;
        if has_alpha {
            let sa = div255(ch(s, 24) * ca);
            for shift in [0u32, 8, 16, 24] {
                let sc = div255(ch(s, shift) * ca);
                let dc = ch(d, shift);
                let v = (sc + div255(dc * (255 - sa))).min(255);
                out |= v << shift;
            }
        } else {
            for shift in [0u32, 8, 16, 24] {
                let v = div255(ch(s, shift) * ca + ch(d, shift) * (255 - ca)).min(255);
                out |= v << shift;
            }
        }
        out
    }

    /// TransparentBlt's test: is the source pixel copied.
    pub fn transparent_passes(s: u32, color: u32, honor_alpha: bool) -> bool {
        if honor_alpha {
            s != color
        } else {
            (s & 0x00ff_ffff) != color
        }
    }

    /// ClearTypeBlend of one pixel (Learn `DXGK_GDIARG_CLEARTYPEBLEND` remarks). `gamma`: the row
    /// of the gamma surface (512 entries: the gamma table then the inverse table), or `None` for
    /// `D3DKM_INVALID_GAMMA_INDEX`.
    pub fn cleartype_pixel(d: u32, a: u32, color: u32, color2: u32, gamma: Option<&[u8; 512]>) -> u32 {
        let mut out = d & 0xff00_0000;
        let ar = ch(a, 16);
        let ag = ch(a, 8);
        let ab = ch(a, 0);
        for (shift, ac) in [(16u32, ar), (8, ag), (0, ab)] {
            let dc = ch(d, shift);
            let cc = ch(color, shift);
            let v = if ac == 0 {
                dc
            } else if ac == 255 {
                ch(color2, shift)
            } else {
                match gamma {
                    Some(t) => {
                        let tmp = t[dc as usize] as i32;
                        let blended = tmp as f32 + (cc as i32 - tmp) as f32 * ac as f32 / 255.0;
                        let idx = (blended + 0.5) as i32;
                        t[256 + idx.clamp(0, 255) as usize] as u32
                    }
                    None => {
                        // OutputColor.c = D.c + (Color.c - D.c) * (Color.c >= D.c ? A.r : A.g) / 255
                        let k = if cc >= dc { ar } else { ag };
                        let v = dc as i32 + ((cc as i32 - dc as i32) * k as i32) / 255;
                        v.clamp(0, 255) as u32
                    }
                }
            };
            out |= v << shift;
        }
        out
    }

    /// What one CPU execution did.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Done {
        pub pixels: u64,
        /// Reads or writes that fell outside a view (a bounding-box bug or a lying command).
        pub skipped: u64,
    }

    /// Run `cmd` over one destination sub-rectangle `sub` (already clipped to the surface).
    /// `src` is the source view (BitBlt, AlphaBlend, StretchBlt, TransparentBlt; the alpha
    /// surface for ClearTypeBlend), `gamma` the gamma row for ClearTypeBlend.
    pub fn run(
        cmd: &Cmd,
        sub: &Rect,
        dst: &mut ViewMut<'_>,
        src: Option<&View<'_>>,
        gamma: Option<&[u8; 512]>,
        done: &mut Done,
    ) {
        for y in sub.top..sub.bottom {
            for x in sub.left..sub.right {
                let Some(d) = dst.get(x, y) else {
                    done.skipped += 1;
                    continue;
                };
                let v = match *cmd {
                    Cmd::ColorFill { color, rop, rop3, .. } => Some(fill_pixel(rop, rop3, color, d)),
                    Cmd::BitBlt { src: sr, dst: dr, rop, rop3, .. } => src
                        .and_then(|s| s.get(x - dr.left + sr.left, y - dr.top + sr.top))
                        .map(|s| bitblt_pixel(rop, rop3, s, d)),
                    Cmd::StretchBlt { src: sr, dst: dr, flags, .. } => {
                        let xs = scale_coord_mirror(x, dr.left, dr.width(), sr.left, sr.width(), flags & (1 << 16) != 0);
                        let ys = scale_coord_mirror(y, dr.top, dr.height(), sr.top, sr.height(), flags & (1 << 17) != 0);
                        src.and_then(|s| s.get(xs, ys))
                    }
                    Cmd::AlphaBlend { src: sr, dst: dr, const_alpha, has_alpha, .. } => {
                        let xs = scale_coord(x, dr.left, dr.width(), sr.left, sr.width());
                        let ys = scale_coord(y, dr.top, dr.height(), sr.top, sr.height());
                        src.and_then(|s| s.get(xs, ys)).map(|s| blend_pixel(s, d, const_alpha, has_alpha))
                    }
                    Cmd::TransparentBlt { src: sr, dst: dr, color, honor_alpha, .. } => {
                        let xs = scale_coord(x, dr.left, dr.width(), sr.left, sr.width());
                        let ys = scale_coord(y, dr.top, dr.height(), sr.top, sr.height());
                        src.and_then(|s| s.get(xs, ys))
                            .map(|s| if transparent_passes(s, color, honor_alpha) { s } else { d })
                    }
                    Cmd::ClearTypeBlend { dst_to_alpha_x, dst_to_alpha_y, color, color2, .. } => src
                        .and_then(|a| a.get(x + dst_to_alpha_x, y + dst_to_alpha_y))
                        .map(|a| cleartype_pixel(d, a, color, color2, gamma)),
                    Cmd::Escape => Some(d),
                };
                match v {
                    Some(v) => {
                        dst.put(x, y, v);
                        done.pixels += 1;
                    }
                    None => done.skipped += 1,
                }
            }
        }
    }

    /// The source pixels the destination sub-rectangle `sub` reads through the truncate mapping
    /// (mirrored where asked): the hull of the mapped first and last column and row, inside SrcRect.
    pub fn scaled_window(sub: &Rect, dst: &Rect, src: &Rect, mirror_x: bool, mirror_y: bool) -> Rect {
        if sub.is_empty() {
            return Rect::default();
        }
        let x0 = scale_coord_mirror(sub.left, dst.left, dst.width(), src.left, src.width(), mirror_x);
        let x1 = scale_coord_mirror(sub.right - 1, dst.left, dst.width(), src.left, src.width(), mirror_x);
        let y0 = scale_coord_mirror(sub.top, dst.top, dst.height(), src.top, src.height(), mirror_y);
        let y1 = scale_coord_mirror(sub.bottom - 1, dst.top, dst.height(), src.top, src.height(), mirror_y);
        Rect::new(x0.min(x1), y0.min(y1), x0.max(x1) + 1, y0.max(y1) + 1).intersect(src)
    }

    /// The source rectangle (in source-surface coordinates) a destination sub-rectangle reads,
    /// for sizing the source window: exact for BitBlt, the scaled hull for the scaled operations,
    /// the alpha-surface rectangle for ClearTypeBlend. `None` when the command has no source.
    pub fn src_window(cmd: &Cmd, sub: &Rect) -> Option<Rect> {
        match *cmd {
            Cmd::BitBlt { src, dst, .. } => Some(bitblt_src(sub, &dst, &src)),
            Cmd::StretchBlt { src, dst, flags, .. } => {
                Some(scaled_window(sub, &dst, &src, flags & (1 << 16) != 0, flags & (1 << 17) != 0))
            }
            Cmd::AlphaBlend { src, dst, .. } | Cmd::TransparentBlt { src, dst, .. } => {
                Some(scaled_window(sub, &dst, &src, false, false))
            }
            Cmd::ClearTypeBlend { dst_to_alpha_x, dst_to_alpha_y, .. } => {
                Some(sub.offset(dst_to_alpha_x, dst_to_alpha_y))
            }
            Cmd::ColorFill { .. } | Cmd::Escape => None,
        }
    }
}

// ── the DMA private record and the job timeline ────────────────────────────────────────────────

/// `"HGDA"`: the record `DxgkDdiRenderKm` writes at offset 0 of the DMA buffer's private data,
/// where a Present writes its own `"HPBL"` prefix (which it replaces: dxgkrnl recycles buffers).
pub const PRIVATE_MAGIC: u32 = 0x4847_4441;
pub const PRIVATE_VERSION: u32 = 1;
/// Bytes of [`Private`]; must fit the 32-byte Present prefix of the 112-byte private data.
pub const PRIVATE_BYTES: usize = 16;

/// The job id a RenderKm buffer names. Never mutated by SubmitCommand: a preempted buffer is
/// resubmitted with the same record, and the job table answers "already queued" or "already
/// done" (an id below the next one that is no longer in the table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Private {
    pub job: u64,
}

impl Private {
    pub fn encode(&self) -> [u8; PRIVATE_BYTES] {
        let mut b = [0u8; PRIVATE_BYTES];
        b[0..4].copy_from_slice(&PRIVATE_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&PRIVATE_VERSION.to_le_bytes());
        b[8..16].copy_from_slice(&self.job.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Private> {
        if rd_u32(b, 0)? != PRIVATE_MAGIC || rd_u32(b, 4)? != PRIVATE_VERSION {
            return None;
        }
        let job = rd_u64(b, 8)?;
        (job != 0).then_some(Private { job })
    }
}

/// What SubmitCommand does with a job id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// First submission: the job takes sequence `seq`; the fence waits for `completed >= seq`.
    Queue { seq: u64 },
    /// Already queued (a preempted replay): the fence waits for the same `seq`.
    Again { seq: u64 },
    /// Already executed and retired from the table, or never issued: no wait.
    NoWait,
}

/// The monotonic sequence of submitted jobs and the executor's completed watermark. The executor
/// runs jobs in sequence order, so "completed >= seq" means "this job and every earlier one are in
/// their destinations". One per adapter, in the I/O half under a spinlock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    /// The last sequence handed out.
    pub submitted: u64,
    /// The last sequence the executor finished (or discharged).
    pub completed: u64,
}

impl Timeline {
    pub fn next(&mut self) -> u64 {
        self.submitted += 1;
        self.submitted
    }

    pub const fn ready(&self, seq: u64) -> bool {
        seq <= self.completed
    }

    /// The executor finished `seq` (in order). A lower value is ignored.
    pub fn complete(&mut self, seq: u64) {
        if seq > self.completed && seq <= self.submitted {
            self.completed = seq;
        }
    }

    /// Everything submitted is complete (teardown, a transport generation's end).
    pub fn discharge_all(&mut self) {
        self.completed = self.submitted;
    }
}

/// The paths in force (`GdiPaths`: 1 foreign copies, 2 foreign acquire, 4 staging views, 8 two
/// staging views, 16 scrolls, 32 copies INTO foreign images) for a `GdiOff` mask (a set bit turns a
/// path off; bit 0x2 is the opt-in of the acquire, off by default).
pub const fn paths_from_off(off: u32) -> u32 {
    let mut p = 0;
    if off & 0x1 == 0 {
        p |= 1;
    }
    if off & 0x2 != 0 {
        p |= 2;
    }
    if off & 0x4 == 0 {
        p |= 4;
    }
    if off & 0x8 == 0 {
        p |= 8;
    }
    if off & 0x10 == 0 {
        p |= 16;
    }
    if off & 0x20 == 0 {
        p |= 32;
    }
    p
}

/// How long the executor waits for one copy-engine submission before it discharges the job
/// (`Why::Timeout`): the fence retires, the destination keeps what it had.
pub const CE_DEADLINE_MS: u64 = 100;

/// Jobs RenderKm may hold that SubmitCommand has not claimed. A buffer dxgkrnl renders and never
/// submits (a context destroyed in between) would otherwise hold its ops forever; the oldest
/// unclaimed job is dropped above this (`GdiOrph`).
pub const MAX_UNCLAIMED: usize = 256;

// ── counters ───────────────────────────────────────────────────────────────────────────────────

/// The counters `kmd_render/src/ddi/gdi_accel.rs` and `gdi_exec.rs` write. At most 14 characters,
/// prefix `Gdi`, unique across the driver (`GdiType`, `GdiOImg`, `GdiOFmt` are
/// `create_allocation.rs`'s and are not GDI acceleration counters).
pub const COUNTERS: &[&str] = &[
    // The knob in force and the PresentationCaps word reported.
    "GdiKnob",
    "GdiCaps",
    // RenderKm calls, commands parsed, refused buffers and the last refusal code, opcodes seen
    // (bit = opcode), ROPs seen (BitBlt bit = rop, ColorFill bit = 8 + rop).
    "GdiCmdN",
    "GdiOpN",
    "GdiBad",
    "GdiBadWhy",
    "GdiOpMask",
    "GdiRopMask",
    // Commands executed on the copy engine (BitBlt, ColorFill), on the CPU (GdiFall), not at all
    // (GdiDrop); the last reason off the copy engine and every reason seen.
    "GdiBltN",
    "GdiFillN",
    "GdiFall",
    "GdiDrop",
    "GdiWhy",
    "GdiMask",
    // Jobs: queued at SubmitCommand, replays, completed, orphans dropped, submissions to the copy
    // engine, executor time (sum us, max us), sub-rectangles executed.
    "GdiJobN",
    "GdiAgain",
    "GdiDone",
    "GdiOrph",
    "GdiCeSub",
    "GdiUs",
    "GdiUsMax",
    "GdiRects",
    // Census: the classes of the surfaces seen (destination bit 0 VRAM, 1 system, 2 unreachable;
    // sources the same at 4..6), and the last destination's resource id and size (w << 16 | h).
    "GdiCls",
    "GdiDstRes",
    "GdiDstWH",
    // Entry census, before any parsing: RenderKm and RenderGdi calls with the knob on; GDI devices
    // and GDI contexts created (counted with the knob off too), the last GDI context's raw
    // DXGK_CREATECONTEXTFLAGS (bit 2 VirtualAddressing: its commands come through RenderGdi).
    "GdiRkIn",
    "GdiRgIn",
    "GdiDevN",
    "GdiCtxN",
    "GdiCtxFl",
    // SubmitCommand on a GDI context: submissions, private records decoded, jobs claimed by
    // context instead, the private sizes (RenderGdi/RenderKm low 16 bits, SubmitCommand high 16),
    // SubmitCommand's UMD prefix size.
    "GdiSubN",
    "GdiPrvOk",
    "GdiCtxClm",
    "GdiPrvSz",
    "GdiPrvUmd",
    // Copy-engine channel bring-ups the executor asked for; why the last copy-engine attempt
    // failed (1 channel down, 2/3 destination/source mapping, 4 submit, 5 wait, 16 + the channel
    // state when it could not be brought up: 17 cold, 18 disabled, 19 broken, 20 other).
    "GdiChUp",
    "GdiCeWhy",
    // Copies from a VRAM surface into a standard buffer (GDI readback: screen or window reads),
    // and the executor thread's state (1 running, 0 on the HPD worker).
    "GdiRdBk",
    "GdiThr",
    // Copies and fills with a staging buffer on one side run on the copy engine over the buffer's
    // system pages; refused (the CPU instead); failed after the mapping (the CPU instead).
    "GdiSysCe",
    "GdiSysRef",
    "GdiSysFail",
    // Why the last staging copy was refused (1 staging to staging, 2 the VRAM side, 3 the channel
    // down, else ce_sysmem's fail word) and every class seen (1 s2s, 2 VRAM side, 4 not
    // system-resident, 8 uncovered, 16 busy, 32 RM unsure, 64 other, 128 channel down).
    "GdiSysWhy",
    "GdiSysMsk",
    // The last staging-path copy or fill that ran on the CPU: opcode | src class << 4 | dst class
    // << 8 | same buffer << 12 | GdiPaths << 16 | stage << 24 (1 path off, 2 refused, 3 failed).
    "GdiSysCpuK",
    // Its surfaces: std type << 4 | GDI type | RM-backed << 8, source low 16 bits, dest high 16.
    "GdiSysCpuT",
    // Unreachable surfaces: count, the last one's identity (storage << 24 | kind << 16 | foreign
    // layout << 8 | foreign identity << 9 | direct scanout << 10) and extent.
    "GdiUnrN",
    "GdiUnrK",
    "GdiUnrWH",
    // The channel bring-up's time outside the jobs (µs), the slowest command's time and signature
    // (opcode | engine << 4 | dst class << 8 | src class << 12 | sub-rects << 16 | big << 24).
    "GdiChUpUs",
    "GdiSlowUs",
    "GdiSlowOp",
    // The slowest command's reason and raster operation (Why code | DXGK rop enum << 8 | ROP3
    // << 16); commands the CPU ran: their reasons (bit per Why code, 1 none) and the last one's
    // reason and rop (same encoding).
    "GdiSlowRop",
    "GdiCpuMsk",
    "GdiCpuRop",
    // The slowest staging copy-engine command: total µs, µs to its views, to its last submit, in
    // the wait, and its pixels.
    "GdiSysUs",
    "GdiSysVwUs",
    "GdiSysSubUs",
    "GdiSysWtUs",
    "GdiSysPx",
    // Foreign NVK images resolved; copies from them done on the copy engine; refused or failed
    // (the last reason: 1 no source, 2 destination mapping, 3 submit, 4 wait, 5 staging view
    // refused, 6 channel down, 7 memory, 8 destination class, 9 GdiFgn 0).
    "GdiFgnN",
    "GdiFgnCe",
    "GdiFgnFail",
    "GdiFgnWhy",
    // The paths in force (from the GdiOff mask): 1 foreign copies, 2 foreign acquire, 4 staging
    // views, 8 two staging views, 16 scrolls.
    "GdiPaths",
    // Overlapping copies inside one surface (scrolls): seen, done as ordered copy-engine bands,
    // the last refusal (1 shape, 2 view, 3 submit, 4 wait, 5 GdiOvl 0, 6 channel down).
    "GdiOvlN",
    "GdiOvlCe",
    "GdiOvlWhy",
    // Commands dropped because a foreign image is in them other than as a SRCCOPY source, and the
    // last one's signature (opcode | foreign dst << 8 | foreign src << 9 | rop << 16).
    "GdiFgnDrop",
    // Copies INTO a foreign NVK image done on the copy engine (failures: GdiFgnFail, GdiFgnWhy 16 +
    // the step).
    "GdiFgnWr",
    "GdiFgnOp",
    // The slowest job: its command count and its three slowest commands (GdiSlowOp signature, µs).
    // Pixel self-check after a fill/copy that reported success (sampled): checks, mismatches, the
    // last mismatch's command (GdiSlowOp signature | path << 28: 1 CE, 2 staging view, 3 CPU,
    // 4 scroll), the pixel read and the pixel wanted.
    "GdiChkN",
    "GdiChkBad",
    "GdiChkK",
    "GdiChkGot",
    "GdiChkWant",
    // Staging commands whose pitch differs from the view's (authored) pitch; the last pair.
    "GdiPitchMis",
    "GdiPitchCmd",
    "GdiPitchAl",
    // Destinations written (up to 8, first come): resource id << 12 | commands (max 4095).
    "GdiRes0",
    "GdiRes1",
    "GdiRes2",
    "GdiRes3",
    "GdiRes4",
    "GdiRes5",
    "GdiRes6",
    "GdiRes7",
    "GdiJobMaxN",
    "GdiJobT1",
    "GdiJobT1Us",
    "GdiJobT2",
    "GdiJobT2Us",
    "GdiJobT3",
    "GdiJobT3Us",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::cpu::*;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn caps_off_is_zero_and_on_is_the_documented_word() {
        assert_eq!(resolve_caps(0), Caps { on: false, reported: 0 });
        assert_eq!(resolve_caps(2), Caps { on: false, reported: 0x100 });
        assert_eq!(resolve_caps(3), Caps { on: false, reported: 0x1000_0000 });
        assert_eq!(resolve_caps(4), Caps { on: false, reported: 0 });
        assert_eq!(resolve_caps_with(1, true, true), resolve_caps(1));
        assert_eq!(resolve_caps_with(1, false, true), Caps { on: false, reported: 0 });
        assert_eq!(resolve_caps_with(1, true, false), Caps { on: false, reported: 0 });
        assert_eq!(resolve_caps_with(2, false, false), resolve_caps(2));
        assert_eq!(resolve_caps(0xffff_ffff).reported, 0);
        let c = resolve_caps(1);
        assert!(c.on);
        assert_eq!(c.reported & pcaps::SUPPORT_KERNEL_MODE_COMMAND_BUFFER, 4);
        assert_eq!((c.reported >> 10) & 0xf, 2, "AlignmentShift");
        assert_eq!((c.reported >> 14) & 0x7, 3, "MaxTextureWidthShift");
        assert_eq!((c.reported >> 17) & 0x7, 3, "MaxTextureHeightShift");
        assert_eq!(c.reported & pcaps::SUPPORT_ALL_BLT_ROPS, 0);
        assert_eq!(c.reported & pcaps::NO_SAME_BITMAP_BIT_BLT, 0);
        assert_ne!(c.reported & pcaps::NO_SAME_BITMAP_OVERLAPPED_BIT_BLT, 0);
        assert_eq!(c.reported & (pcaps::NO_SCREEN_TO_SCREEN_BLT | pcaps::NO_OVERLAP_SCREEN_BLT), 0);
        assert_eq!(c.reported & (1 << 9 | 1 << 26 | 1 << 31), 0, "reserved bits");
        assert_eq!(c.reported, 0x0A06_C8FC);
        assert_eq!(MAX_TEXTURE_DIM, 16384);
    }

    #[test]
    fn layout_matches_the_c_compile() {
        // gcc x86_64 offsetof of the d3dkmddi.h declarations (UINT 4, RECT 4x LONG, pointers 8):
        // BB 72 SB 64 CF 40 AB 64 TB 64 CT 72 CMD 80, union at 8.
        use layout::*;
        assert_eq!((BITBLT_BYTES, STRETCHBLT_BYTES, COLORFILL_BYTES), (72, 64, 40));
        assert_eq!((ALPHABLEND_BYTES, TRANSPARENTBLT_BYTES, CLEARTYPE_BYTES), (64, 64, 72));
        assert_eq!((CMD_ARG, CMD_BYTES), (8, 80));
        assert_eq!((BB_NUM_SUB, BB_SUB_PTR, BB_ROP, BB_ROP3, BB_SRC_PITCH, BB_DST_PITCH), (40, 48, 56, 58, 60, 64));
        assert_eq!((SB_DST_INDEX, SB_SRC_INDEX, SB_NUM_SUB, SB_SUB_PTR, SB_FLAGS, SB_SRC_PITCH), (32, 36, 40, 48, 56, 60));
        assert_eq!((CF_DST_INDEX, CF_NUM_SUB, CF_SUB_PTR, CF_COLOR, CF_ROP, CF_ROP3), (16, 20, 24, 32, 36, 38));
        assert_eq!((AB_NUM_SUB, AB_SUB_PTR, AB_CONST_ALPHA, AB_HAS_ALPHA, AB_SRC_PITCH), (40, 48, 56, 57, 60));
        assert_eq!((TB_COLOR, TB_NUM_SUB, TB_SUB_PTR, TB_FLAGS, TB_SRC_PITCH), (40, 44, 48, 56, 60));
        assert_eq!(
            (CT_TMP_INDEX, CT_GAMMA_INDEX, CT_ALPHA_INDEX, CT_DST_INDEX, CT_OFFSET_X, CT_OFFSET_Y),
            (16, 20, 24, 28, 32, 36)
        );
        assert_eq!((CT_COLOR, CT_GAMMA, CT_NUM_SUB, CT_SUB_PTR, CT_ALPHA_PITCH, CT_COLOR2), (40, 44, 48, 56, 64, 68));
    }

    // ── command-buffer builders for the tests ─────────────────────────────────────────────────

    const BASE: u64 = 0xffff_8000_1000_0000;

    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn put_rect(b: &mut [u8], at: usize, r: Rect) {
        put32(b, at, r.left as u32);
        put32(b, at + 4, r.top as u32);
        put32(b, at + 8, r.right as u32);
        put32(b, at + 12, r.bottom as u32);
    }

    /// Append one command with `arg_len` bytes of arm and `subs` inline after it; `fill` writes
    /// the arm (offsets relative to the arm); returns nothing, the subs pointer is patched here.
    fn push_cmd(buf: &mut Vec<u8>, opcode: u32, arg_len: usize, count_at: usize, ptr_at: usize, subs: &[Rect], fill: impl FnOnce(&mut [u8])) {
        let start = buf.len();
        let size = layout::CMD_ARG + arg_len + subs.len() * 16;
        buf.resize(start + size, 0);
        put32(buf, start, opcode);
        put32(buf, start + 4, size as u32);
        let a = start + layout::CMD_ARG;
        fill(&mut buf[a..a + arg_len]);
        if !subs.is_empty() {
            put32(buf, a + count_at, subs.len() as u32);
            let sub_at = a + arg_len;
            put64(buf, a + ptr_at, BASE + sub_at as u64);
            for (i, r) in subs.iter().enumerate() {
                put_rect(buf, sub_at + 16 * i, *r);
            }
        }
    }

    fn bitblt(buf: &mut Vec<u8>, src: Rect, dst: Rect, si: u32, di: u32, rop_v: u16, subs: &[Rect]) {
        use layout::*;
        push_cmd(buf, op::BITBLT, BITBLT_BYTES, BB_NUM_SUB, BB_SUB_PTR, subs, |a| {
            put_rect(a, BB_SRC_RECT, src);
            put_rect(a, BB_DST_RECT, dst);
            put32(a, BB_SRC_INDEX, si);
            put32(a, BB_DST_INDEX, di);
            put16(a, BB_ROP, rop_v);
            put32(a, BB_SRC_PITCH, 4096);
            put32(a, BB_DST_PITCH, 8192);
        });
    }

    fn colorfill(buf: &mut Vec<u8>, dst: Rect, di: u32, color: u32, rop_v: u16, subs: &[Rect]) {
        use layout::*;
        push_cmd(buf, op::COLORFILL, COLORFILL_BYTES, CF_NUM_SUB, CF_SUB_PTR, subs, |a| {
            put_rect(a, CF_DST_RECT, dst);
            put32(a, CF_DST_INDEX, di);
            put32(a, CF_COLOR, color);
            put16(a, CF_ROP, rop_v);
        });
    }

    #[test]
    fn parses_a_bitblt_and_a_colorfill_with_inline_subrects() {
        let mut buf = Vec::new();
        let subs = [Rect::new(10, 10, 20, 20), Rect::new(30, 10, 40, 20)];
        bitblt(&mut buf, Rect::new(0, 0, 100, 50), Rect::new(5, 5, 105, 55), 1, 2, rop::SRCCOPY, &subs);
        colorfill(&mut buf, Rect::new(0, 0, 8, 8), 3, 0xff00ff00, cfrop::PATCOPY, &[]);
        let mut p = Parser::new(&buf, BASE);
        let c = p.next_cmd().unwrap().unwrap();
        match c {
            Cmd::BitBlt { src, dst, src_index, dst_index, subs: s, rop: r, src_pitch, dst_pitch, .. } => {
                assert_eq!(src, Rect::new(0, 0, 100, 50));
                assert_eq!(dst, Rect::new(5, 5, 105, 55));
                assert_eq!((src_index, dst_index, r, src_pitch, dst_pitch), (1, 2, rop::SRCCOPY, 4096, 8192));
                let SubRects::Inline { offset, count } = s else { panic!("{s:?}") };
                assert_eq!(count, 2);
                assert_eq!(p.inline_rect(offset, 0), Some(subs[0]));
                assert_eq!(p.inline_rect(offset, 1), Some(subs[1]));
            }
            other => panic!("{other:?}"),
        }
        let c = p.next_cmd().unwrap().unwrap();
        assert_eq!(
            c,
            Cmd::ColorFill { dst: Rect::new(0, 0, 8, 8), dst_index: 3, subs: SubRects::None, color: 0xff00ff00, rop: cfrop::PATCOPY, rop3: 0 }
        );
        assert!(p.next_cmd().is_none());
    }

    #[test]
    fn refuses_malformed_buffers() {
        // Bad opcode.
        let mut buf = vec![0u8; 16];
        put32(&mut buf, 0, 9);
        put32(&mut buf, 4, 16);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::Opcode)));
        // Size too small for the arm.
        put32(&mut buf, 0, op::COLORFILL);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::Size)));
        // Size past the end.
        let mut buf = Vec::new();
        colorfill(&mut buf, Rect::new(0, 0, 1, 1), 0, 0, 1, &[]);
        put32(&mut buf, 4, 4096);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::Size)));
        // Trailer.
        let buf = vec![0u8; 4];
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::Trailer)));
        // Inline sub-rects pointing into the arm, or past the command.
        let mut buf = Vec::new();
        colorfill(&mut buf, Rect::new(0, 0, 1, 1), 0, 0, 1, &[Rect::new(0, 0, 1, 1)]);
        let a = layout::CMD_ARG;
        put64(&mut buf, a + layout::CF_SUB_PTR, BASE + a as u64);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::SubRects)));
        put32(&mut buf, a + layout::CF_NUM_SUB, 2);
        put64(&mut buf, a + layout::CF_SUB_PTR, BASE + (a + layout::COLORFILL_BYTES) as u64);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::SubRects)));
        // Too many.
        put32(&mut buf, a + layout::CF_NUM_SUB, MAX_SUB_RECTS + 1);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::SubRects)));
        // Null pointer with a count.
        put32(&mut buf, a + layout::CF_NUM_SUB, 1);
        put64(&mut buf, a + layout::CF_SUB_PTR, 0);
        assert_eq!(Parser::new(&buf, BASE).next_cmd(), Some(Err(Bad::SubRects)));
        // An external pointer is accepted and named.
        put64(&mut buf, a + layout::CF_SUB_PTR, 0x1234_5678);
        let c = Parser::new(&buf, BASE).next_cmd().unwrap().unwrap();
        assert_eq!(c.subs(), SubRects::External { ptr: 0x1234_5678, count: 1 });
        // The parser stops after an error.
        let mut buf = vec![0u8; 32];
        put32(&mut buf, 0, 99);
        put32(&mut buf, 4, 16);
        let mut p = Parser::new(&buf, BASE);
        assert!(p.next_cmd().unwrap().is_err());
        assert!(p.next_cmd().is_none());
    }

    #[test]
    fn parses_the_other_arms() {
        use layout::*;
        let mut buf = Vec::new();
        push_cmd(&mut buf, op::STRETCHBLT, STRETCHBLT_BYTES, SB_NUM_SUB, SB_SUB_PTR, &[], |a| {
            put32(a, SB_DST_INDEX, 7);
            put32(a, SB_SRC_INDEX, 8);
            put32(a, SB_FLAGS, 3 | 1 << 16);
        });
        push_cmd(&mut buf, op::ALPHABLEND, ALPHABLEND_BYTES, AB_NUM_SUB, AB_SUB_PTR, &[], |a| {
            a[AB_CONST_ALPHA] = 200;
            a[AB_HAS_ALPHA] = 1;
            put32(a, AB_SRC_PITCH, 64);
        });
        push_cmd(&mut buf, op::TRANSPARENTBLT, TRANSPARENTBLT_BYTES, TB_NUM_SUB, TB_SUB_PTR, &[], |a| {
            put32(a, TB_COLOR, 0xff00ff);
            put32(a, TB_FLAGS, 1);
        });
        push_cmd(&mut buf, op::CLEARTYPEBLEND, CLEARTYPE_BYTES, CT_NUM_SUB, CT_SUB_PTR, &[], |a| {
            put32(a, CT_GAMMA, INVALID_GAMMA);
            put32(a, CT_OFFSET_X, (-3i32) as u32);
            put32(a, CT_COLOR2, 0x123456);
        });
        push_cmd(&mut buf, op::ESCAPE, 0, 0, 0, &[], |_| {});
        let mut p = Parser::new(&buf, BASE);
        let Cmd::StretchBlt { src_index, dst_index, flags, .. } = p.next_cmd().unwrap().unwrap() else { panic!() };
        assert_eq!((src_index, dst_index, flags), (8, 7, 3 | 1 << 16));
        let Cmd::AlphaBlend { const_alpha, has_alpha, src_pitch, .. } = p.next_cmd().unwrap().unwrap() else { panic!() };
        assert_eq!((const_alpha, has_alpha, src_pitch), (200, true, 64));
        let Cmd::TransparentBlt { color, honor_alpha, .. } = p.next_cmd().unwrap().unwrap() else { panic!() };
        assert_eq!((color, honor_alpha), (0xff00ff, true));
        let c = p.next_cmd().unwrap().unwrap();
        let Cmd::ClearTypeBlend { gamma, dst_to_alpha_x, color2, .. } = c else { panic!() };
        assert_eq!((gamma, dst_to_alpha_x, color2), (INVALID_GAMMA, -3, 0x123456));
        assert_eq!(c.indices().1[1], None, "no gamma surface with the invalid index");
        assert_eq!(p.next_cmd(), Some(Ok(Cmd::Escape)));
        assert!(p.next_cmd().is_none());
    }

    fn vram(id: u32) -> Surface {
        Surface { resource_id: id, width: 1600, height: 900, pitch: 6400, class: SurfaceClass::Vram, format: 21, kind_bits: 0 }
    }

    #[test]
    fn the_plan_puts_copies_and_fills_on_the_copy_engine_only_between_vram_surfaces() {
        let bb = |rop_v: u16, si: u32, di: u32, src: Rect, dst: Rect| Cmd::BitBlt {
            src,
            dst,
            src_index: si,
            dst_index: di,
            subs: SubRects::None,
            rop: rop_v,
            rop3: 0,
            src_pitch: 0,
            dst_pitch: 0,
        };
        let r = Rect::new(0, 0, 10, 10);
        let (a, b) = (vram(1), vram(2));
        let sys = Surface { class: SurfaceClass::System, ..vram(3) };
        let venus = Surface { class: SurfaceClass::Unreachable, ..vram(4) };
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&b), [Some(&a), None]), (Engine::Ce, None));
        assert_eq!(plan(&bb(rop::SRCOR, 0, 1, r, r), Some(&b), [Some(&a), None]), (Engine::Cpu, Some(Why::Rop)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&b), [Some(&sys), None]), (Engine::Cpu, Some(Why::SystemSurface)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&b), [Some(&venus), None]), (Engine::Drop, Some(Why::Unreachable)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&b), [None, None]), (Engine::Drop, Some(Why::BadIndex)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), None, [Some(&a), None]), (Engine::Drop, Some(Why::BadIndex)));
        // Same surface: disjoint is fine, overlapping goes to the CPU.
        assert_eq!(plan(&bb(rop::SRCCOPY, 1, 1, Rect::new(20, 0, 30, 10), r), Some(&a), [Some(&a), None]), (Engine::Ce, None));
        assert_eq!(plan(&bb(rop::SRCCOPY, 1, 1, Rect::new(5, 0, 15, 10), r), Some(&a), [Some(&a), None]), (Engine::Cpu, Some(Why::Overlap)));
        let cf = |rop_v: u16| Cmd::ColorFill { dst: r, dst_index: 0, subs: SubRects::None, color: 0, rop: rop_v, rop3: 0 };
        assert_eq!(plan(&cf(cfrop::PATCOPY), Some(&a), [None, None]), (Engine::Ce, None));
        assert_eq!(plan(&cf(cfrop::PATCOPY), Some(&sys), [None, None]), (Engine::Cpu, Some(Why::SystemSurface)));
        assert_eq!(plan(&cf(cfrop::DSTINVERT), Some(&a), [None, None]), (Engine::Cpu, Some(Why::Rop)));
        assert_eq!(plan(&Cmd::Escape, None, [None, None]), (Engine::Drop, None));
        // A foreign NVK image: only a SRCCOPY source.
        let fg = Surface { class: SurfaceClass::Foreign, ..vram(5) };
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&b), [Some(&fg), None]), (Engine::Ce, None));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&sys), [Some(&fg), None]), (Engine::Cpu, Some(Why::SystemSurface)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&fg), [Some(&b), None]), (Engine::Ce, None));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&fg), [Some(&sys), None]), (Engine::Cpu, Some(Why::SystemSurface)));
        assert_eq!(plan(&bb(rop::SRCCOPY, 0, 1, r, r), Some(&fg), [Some(&fg), None]), (Engine::Drop, Some(Why::Unreachable)));
        assert_eq!(plan(&bb(rop::SRCOR, 0, 1, r, r), Some(&b), [Some(&fg), None]), (Engine::Drop, Some(Why::Unreachable)));
    }

    #[test]
    fn fill_words_match_nvk_fill_memory_ce() {
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        fill(&mut p, 0xff336699).unwrap();
        fill_rect(&mut p, 0x10_0000_0000, 6400, &Rect::new(2, 3, 12, 7)).unwrap();
        let w = p.words();
        assert_eq!(w.len(), FILL_STATE_DWORDS + FILL_RECT_DWORDS);
        assert_eq!(w[0], cp::method(4, 0x700, 3));
        assert_eq!(&w[1..4], &[0xff336699, 0xff336699, 0x0003_4444]);
        assert_eq!(w[4], cp::method(4, 0x400, 8));
        let start = 0x10_0000_0000u64 + 3 * 6400 + 2 * 4;
        assert_eq!(&w[5..13], &[(start >> 32) as u32, start as u32, (start >> 32) as u32, start as u32, 6400, 6400, 10, 4]);
        assert_eq!(w[13], cp::method(4, 0x300, 1));
        // NON_PIPELINED 2, FLUSH, SRC/DST PITCH, MULTI_LINE, REMAP_ENABLE.
        assert_eq!(w[14], 2 | 1 << 2 | 1 << 7 | 1 << 8 | 1 << 9 | 1 << 10);
        // Shapes refused.
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        assert_eq!(fill_rect(&mut p, 0, 6400, &Rect::new(0, 0, 0, 1)), Err(PushError::Shape));
        assert_eq!(fill_rect(&mut p, 0, 6400, &Rect::new(0, 0, 1601, 1)), Err(PushError::Shape));
        assert_eq!(fill_rect(&mut p, 0, 6402, &Rect::new(0, 0, 1, 1)), Err(PushError::Shape));
        assert_eq!(fill_rect(&mut p, cp::MAX_VA, 6400, &Rect::new(0, 0, 1, 1)), Err(PushError::Va));
    }

    #[test]
    fn copy_words_are_a_pitch_copy_of_the_rectangle() {
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        let s = CeView { va: 0x1000_0000, pitch: 6400, width: 1600, height: 900 };
        let d = CeView { va: 0x2000_0000, pitch: 8192, width: 2048, height: 1080 };
        copy_rect(&mut p, Gen::Gb202, &s, &Rect::new(1, 2, 11, 22), &d, &Rect::new(100, 200, 110, 220), false).unwrap();
        let w = p.words();
        assert_eq!(w.len(), COPY_RECT_DWORDS);
        let sva = 0x1000_0000u64 + 2 * 6400 + 4;
        let dva = 0x2000_0000u64 + 200 * 8192 + 400;
        assert_eq!(&w[1..9], &[0, sva as u32, 0, dva as u32, 6400, 8192, 40, 20]);
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        assert_eq!(copy_rect(&mut p, Gen::Gb202, &s, &Rect::new(0, 0, 10, 10), &d, &Rect::new(0, 0, 10, 11), false), Err(PushError::Shape));
        assert_eq!(copy_rect(&mut p, Gen::Gb202, &s, &Rect::new(1595, 0, 1605, 10), &d, &Rect::new(0, 0, 10, 10), false), Err(PushError::Shape));
        // An R/B exchange programs the remap (components word, LAUNCH_DMA REMAP_ENABLE).
        let mut buf = [0u32; 64];
        let mut p = Push::new(&mut buf);
        copy_rect(&mut p, Gen::Gb202, &s, &Rect::new(0, 0, 4, 4), &d, &Rect::new(0, 0, 4, 4), true).unwrap();
        assert!(p.words().contains(&cp::SWAP_RB_COMPONENTS));
        let a = Surface { format: 32, ..vram(1) };
        let b = vram(2);
        assert!(swaps_rb(&a, &b) && !swaps_rb(&b, &b) && !swaps_rb(&Surface { format: 0, ..b }, &b));
        assert_eq!(rects_per_push(128, FILL_STATE_DWORDS, FILL_RECT_DWORDS), 10);
        assert_eq!(rects_per_push(128, 0, COPY_RECT_DWORDS), 11);
    }

    /// Executing the bands in order on a model surface equals the copy of the whole rectangle.
    #[test]
    fn split_overlap_is_a_scroll() {
        let w = 23usize;
        let h = 19usize;
        for &(dx, dy) in &[(0, 1), (0, -1), (0, 5), (0, -7), (3, 0), (-4, 0), (2, 3), (-3, -2), (1, -6), (0, 0)] {
            let src = Rect::new(8, 8, 15, 13);
            let dst = src.offset(dx, dy);
            let mut img: Vec<u32> = (0..(w * h) as u32).collect();
            let mut want = img.clone();
            for y in src.top..src.bottom {
                for x in src.left..src.right {
                    want[(y + dy) as usize * w + (x + dx) as usize] = img[y as usize * w + x as usize];
                }
            }
            let mut bands = Vec::new();
            assert!(split_overlap(&src, &dst, |s, d| bands.push((s, d))));
            for (s, d) in &bands {
                assert!(!s.overlaps(d), "{dx},{dy}: band overlaps itself");
                let rows: Vec<Vec<u32>> = (s.top..s.bottom)
                    .map(|y| (s.left..s.right).map(|x| img[y as usize * w + x as usize]).collect())
                    .collect();
                for (j, y) in (d.top..d.bottom).enumerate() {
                    for (i, x) in (d.left..d.right).enumerate() {
                        img[y as usize * w + x as usize] = rows[j][i];
                    }
                }
            }
            assert_eq!(img, want, "scroll {dx},{dy}");
        }
    }

    #[test]
    fn gdi_off_mask() {
        assert_eq!(paths_from_off(0), 0x3D);
        assert_eq!(paths_from_off(0x3D), 0);
        assert_eq!(paths_from_off(0x2), 0x3F);
        assert_eq!(paths_from_off(0x4), 0x39);
        assert_eq!(paths_from_off(0x20), 0x1D);
    }

    /// A knob is a service-key value the driver READS; a counter is one it WRITES. The same name
    /// for both makes the knob read the previous boot's counter (`GdiSysCe` did, 365.1-367.1).
    /// No knob name of `diag.rs` may appear as a byte-string literal anywhere else in
    /// `kmd_render`, except the listed historical ones.
    #[test]
    fn no_knob_name_is_a_counter_name() {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !render.exists() {
            assert!(std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"));
            return;
        }
        // `FlipLat` is the prefix of `FlipLat0..` histogram names; `NvSpinUs` mirrors its own
        // clamped effective value under the knob's name (an existing pattern, stable because the
        // mirror equals what the read will return next time).
        // The MSI breaker/latch state of `virtio/msi.rs` (on main): deliberate write-and-read-back
        // values, the knob and the state being one registry value by design.
        const ALLOWED: &[&str] = &[
            "FlipLat",
            "NvSpinUs",
            "MsiBreaker",
            "MsiLatch",
            "MsiLatchOld",
            "MsiLatchVer",
            "MsiLatchWhy",
            "MsiMarkerOld",
            "MsiStarting",
            "MsiStartingVer",
        ];
        fn lits(text: &str) -> Vec<std::string::String> {
            let mut out = Vec::new();
            let mut rest = text;
            while let Some(i) = rest.find("b\"") {
                let tail = &rest[i + 2..];
                let Some(end) = tail.find('"') else { break };
                let n = &tail[..end];
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric()) {
                    out.push(n.into());
                }
                rest = &tail[end + 1..];
            }
            out
        }
        let diag = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        let mut knobs = Vec::new();
        let mut rest = diag.as_str();
        while let Some(i) = rest.find("KnobName::new(b\"") {
            let tail = &rest[i + "KnobName::new(b\"".len()..];
            let end = tail.find('"').unwrap();
            knobs.push(std::string::String::from(&tail[..end]));
            rest = &tail[end..];
        }
        assert!(knobs.len() > 50, "found {} knobs", knobs.len());
        let mut stack = vec![render.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().map_or(true, |x| x != "rs") {
                    continue;
                }
                let mut text = std::fs::read_to_string(&p).unwrap();
                if p.ends_with("diag.rs") {
                    for k in &knobs {
                        text = text.replace(&std::format!("KnobName::new(b\"{k}\")"), "");
                    }
                }
                for l in lits(&text) {
                    if knobs.contains(&l) && !ALLOWED.contains(&l.as_str()) {
                        panic!("{} writes or names {l}, which is also a knob", p.display());
                    }
                }
            }
        }
    }

    #[test]
    fn the_truth_table_agrees_with_the_per_pixel_rops() {
        let vals = [0u32, u32::MAX, 0x1234_5678, 0xF0F0_0F0F, 0xCCCC_AAAA];
        for (r, codes) in [(rop::SRCCOPY, 0..1u16), (rop::SRCINVERT, 0..1), (rop::SRCAND, 0..1), (rop::SRCOR, 0..1), (rop::ROP3, 0..256)] {
            for c in codes {
                let t = bitblt_table(r, c);
                for &sv in &vals {
                    for &dv in &vals {
                        assert_eq!(apply_table(t, sv, dv), bitblt_pixel(r, c, sv, dv), "rop {r} code {c:#x}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_rop3_copy_or_pattern_fill_is_normalized_to_the_named_form() {
        let bb = |rop_v: u16, r3: u16| Cmd::BitBlt {
            src: Rect::new(0, 0, 4, 4),
            dst: Rect::new(0, 0, 4, 4),
            src_index: 0,
            dst_index: 1,
            subs: SubRects::None,
            rop: rop_v,
            rop3: r3,
            src_pitch: 16,
            dst_pitch: 16,
        };
        assert!(matches!(super::normalize_rop(bb(rop::ROP3, 0xCC)), Cmd::BitBlt { rop: rop::SRCCOPY, .. }));
        // 0x0C is also S when P = 0.
        assert!(matches!(super::normalize_rop(bb(rop::ROP3, 0x0C)), Cmd::BitBlt { rop: rop::SRCCOPY, .. }));
        assert!(matches!(super::normalize_rop(bb(rop::ROP3, 0x66)), Cmd::BitBlt { rop: rop::ROP3, .. }));
        let cf = |r3: u16| Cmd::ColorFill { dst: Rect::new(0, 0, 4, 4), dst_index: 1, subs: SubRects::None, color: 1, rop: cfrop::ROP3, rop3: r3 };
        assert!(matches!(super::normalize_rop(cf(0xF0)), Cmd::ColorFill { rop: cfrop::PATCOPY, .. }));
        assert!(matches!(super::normalize_rop(cf(0x5A)), Cmd::ColorFill { rop: cfrop::ROP3, .. }));
    }

    #[test]
    fn the_row_bitblt_matches_the_per_pixel_executor() {
        let src = surface(64, 32, |x, y| (y as u32) << 16 | x as u32 | 0x5500_0000);
        for (r, c) in [(rop::SRCCOPY, 0u16), (rop::SRCAND, 0), (rop::SRCINVERT, 0), (rop::ROP3, 0x33), (rop::ROP3, 0x88), (rop::ROP3, 0xEE)] {
            let cmd = Cmd::BitBlt {
                src: Rect::new(2, 1, 50, 30),
                dst: Rect::new(4, 2, 52, 31),
                src_index: 0,
                dst_index: 1,
                subs: SubRects::None,
                rop: r,
                rop3: c,
                src_pitch: 256,
                dst_pitch: 256,
            };
            let sub = Rect::new(10, 5, 40, 20);
            let sv = View { data: &src, pitch: 256, x0: 0, y0: 0, w: 64, h: 32 };
            let mut a = surface(64, 32, |x, y| (x as u32).wrapping_mul(0x0101_0101) ^ y as u32);
            let mut b = a.clone();
            let mut da = Done::default();
            let mut db = Done::default();
            run(&cmd, &sub, &mut ViewMut { data: &mut a, pitch: 256, x0: 0, y0: 0, w: 64, h: 32 }, Some(&sv), None, &mut da);
            // The window view starts at (3, 4): offsets in both views.
            let mut win = vec![0u8; 50 * 4 * 25];
            for y in 0..25 {
                win[y * 200..(y + 1) * 200].copy_from_slice(&b[(y + 4) * 256 + 12..(y + 4) * 256 + 212]);
            }
            let mut wv = ViewMut { data: &mut win, pitch: 200, x0: 3, y0: 4, w: 50, h: 25 };
            assert!(bitblt_rows(&cmd, &sub, &mut wv, &sv, &mut db));
            for y in 0..25 {
                b[(y + 4) * 256 + 12..(y + 4) * 256 + 212].copy_from_slice(&win[y * 200..(y + 1) * 200]);
            }
            assert_eq!(a, b, "rop {r} {c:#x}");
            assert_eq!(da.pixels, db.pixels);
            // A sub-rectangle outside the window is left to the per-pixel path.
            let mut wv = ViewMut { data: &mut win, pitch: 200, x0: 3, y0: 4, w: 50, h: 25 };
            assert!(!bitblt_rows(&cmd, &Rect::new(0, 0, 8, 8), &mut wv, &sv, &mut db));
        }
    }

    #[test]
    fn rop3_reproduces_the_named_rops() {
        let (p, s, d) = (0xF0F0_F0F0u32, 0xCCCC_CCCCu32, 0xAAAA_AAAAu32);
        assert_eq!(rop3(0xCC, p, s, d), s);
        assert_eq!(rop3(0xF0, p, s, d), p);
        assert_eq!(rop3(0xAA, p, s, d), d);
        assert_eq!(rop3(0x55, p, s, d), !d);
        assert_eq!(rop3(0x66, p, s, d), s ^ d, "SRCINVERT");
        assert_eq!(rop3(0x88, p, s, d), s & d, "SRCAND");
        assert_eq!(rop3(0xEE, p, s, d), s | d, "SRCPAINT");
        assert_eq!(rop3(0x5A, p, s, d), p ^ d, "PATINVERT");
        assert_eq!(rop3(0x00, p, s, d), 0);
        assert_eq!(rop3(0xFF, p, s, d), !0);
        for (r, f) in [(rop::SRCCOPY, s), (rop::SRCINVERT, s ^ d), (rop::SRCAND, s & d), (rop::SRCOR, s | d)] {
            assert_eq!(bitblt_pixel(r, 0, s, d), f);
        }
        for (r, f) in [
            (cfrop::PATCOPY, p),
            (cfrop::PATINVERT, p ^ d),
            (cfrop::PDXN, !(p ^ d)),
            (cfrop::DSTINVERT, !d),
            (cfrop::PATAND, p & d),
            (cfrop::PATOR, p | d),
            (cfrop::ROP3, p ^ d),
        ] {
            assert_eq!(fill_pixel(r, 0x5A, p, d), f, "{r}");
        }
    }

    #[test]
    fn blends_follow_the_documented_formulas() {
        // Opaque premultiplied source replaces the destination; a transparent one keeps it.
        assert_eq!(blend_pixel(0xff10_2030, 0xff80_8080, 255, true), 0xff10_2030);
        assert_eq!(blend_pixel(0x0000_0000, 0xff80_8080, 255, true), 0xff80_8080);
        // Half constant alpha without per-pixel alpha.
        assert_eq!(blend_pixel(0xffff_ffff, 0x0000_0000, 128, false), 0x8080_8080);
        // Transparent test.
        assert!(!transparent_passes(0xffff_00ff, 0x00ff_00ff, false));
        assert!(transparent_passes(0xffff_00ff, 0x00ff_00ff, true));
        // ClearType without gamma: zero coverage keeps D, full coverage takes Color2, alpha kept.
        assert_eq!(cleartype_pixel(0x7f11_2233, 0x0000_0000, 0x00ff_ffff, 0x0001_0203, None), 0x7f11_2233);
        assert_eq!(cleartype_pixel(0x7f11_2233, 0x00ff_ffff, 0x00ff_ffff, 0x0001_0203, None), 0x7f01_0203);
        // With an identity gamma table half coverage lands halfway.
        let mut t = [0u8; 512];
        for i in 0..256 {
            t[i] = i as u8;
            t[256 + i] = i as u8;
        }
        let v = cleartype_pixel(0xff00_0000, 0x0080_8080, 0x00c8_c8c8, 0, Some(&t));
        assert_eq!(v, 0xff64_6464);
    }

    #[test]
    fn scaling_uses_the_truncate_formula() {
        // 2x enlargement replicates, 2x shrink skips.
        let xs: Vec<i32> = (0..4).map(|x| scale_coord(x, 0, 4, 10, 2)).collect();
        assert_eq!(xs, [10, 10, 11, 11]);
        let xs: Vec<i32> = (0..2).map(|x| scale_coord(x, 0, 2, 0, 4)).collect();
        assert_eq!(xs, [1, 3]);
        // Identity.
        assert!((0..7).all(|x| scale_coord(x + 5, 5, 7, 20, 7) == 20 + x));
        // Mirror reads the far end.
        assert_eq!(scale_coord_mirror(0, 0, 4, 10, 4, true), 13);
        assert_eq!(scale_coord_mirror(3, 0, 4, 10, 4, true), 10);
    }

    fn surface(w: u32, h: u32, f: impl Fn(i32, i32) -> u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let o = (y as usize * w as usize + x as usize) * 4;
                v[o..o + 4].copy_from_slice(&f(x, y).to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn the_cpu_executor_agrees_with_the_copy_engine_plan_for_copies_and_fills() {
        // The oracle the hardware run compares against: a SRCCOPY BitBlt of a sub-rectangle is
        // exactly the rectangle copy `copy_rect` describes, a PATCOPY fill the `fill_rect`.
        let src = surface(64, 32, |x, y| (y as u32) << 16 | x as u32);
        let mut dst = surface(64, 32, |_, _| 0xdead_beef);
        let cmd = Cmd::BitBlt {
            src: Rect::new(0, 0, 64, 32),
            dst: Rect::new(4, 2, 68, 34),
            src_index: 0,
            dst_index: 1,
            subs: SubRects::None,
            rop: rop::SRCCOPY,
            rop3: 0,
            src_pitch: 256,
            dst_pitch: 256,
        };
        let sub = Rect::new(10, 5, 20, 9);
        let sv = View { data: &src, pitch: 256, x0: 0, y0: 0, w: 64, h: 32 };
        let mut dv = ViewMut { data: &mut dst, pitch: 256, x0: 0, y0: 0, w: 64, h: 32 };
        let mut done = Done::default();
        run(&cmd, &sub, &mut dv, Some(&sv), None, &mut done);
        assert_eq!(done, Done { pixels: 40, skipped: 0 });
        let s = bitblt_src(&sub, &cmd.dst_rect(), &Rect::new(0, 0, 64, 32));
        assert_eq!(s, Rect::new(6, 3, 16, 7));
        for y in 0..32 {
            for x in 0..64 {
                let v = dv.get(x, y).unwrap();
                if sub.within(64, 32) && x >= 10 && x < 20 && y >= 5 && y < 9 {
                    assert_eq!(v, sv.get(x - 4, y - 2).unwrap());
                } else {
                    assert_eq!(v, 0xdead_beef);
                }
            }
        }
        let fill_cmd = Cmd::ColorFill { dst: Rect::new(0, 0, 64, 32), dst_index: 1, subs: SubRects::None, color: 0x1122_3344, rop: cfrop::PATCOPY, rop3: 0 };
        let mut done = Done::default();
        run(&fill_cmd, &Rect::new(0, 0, 2, 2), &mut dv, None, None, &mut done);
        assert_eq!(done.pixels, 4);
        assert_eq!(dv.get(1, 1), Some(0x1122_3344));
        // A window that does not cover the sub-rectangle skips instead of panicking.
        let mut small = vec![0u8; 16];
        let mut sv2 = ViewMut { data: &mut small, pitch: 8, x0: 10, y0: 10, w: 2, h: 2 };
        let mut done = Done::default();
        run(&fill_cmd, &Rect::new(9, 9, 13, 13), &mut sv2, None, None, &mut done);
        assert_eq!(done, Done { pixels: 4, skipped: 12 });
    }

    #[test]
    fn src_window_bounds_what_the_executor_reads() {
        let cmd = Cmd::StretchBlt {
            src: Rect::new(0, 0, 8, 8),
            dst: Rect::new(0, 0, 16, 16),
            src_index: 0,
            dst_index: 1,
            subs: SubRects::None,
            flags: 3,
            src_pitch: 32,
        };
        let src = surface(8, 8, |x, y| (x + 8 * y) as u32);
        let sv = View { data: &src, pitch: 32, x0: 0, y0: 0, w: 8, h: 8 };
        assert_eq!(cpu::src_window(&cmd, &Rect::new(0, 0, 16, 16)), Some(Rect::new(0, 0, 8, 8)));
        // A corner sub-rectangle reads only its corner of the source (mirrored: the far corner).
        let mirrored = match cmd {
            Cmd::StretchBlt { src, dst, src_index, dst_index, subs, src_pitch, .. } => Cmd::StretchBlt {
                src, dst, src_index, dst_index, subs, src_pitch, flags: 3 | 1 << 16 | 1 << 17,
            },
            _ => unreachable!(),
        };
        assert_eq!(cpu::src_window(&mirrored, &Rect::new(0, 0, 4, 4)), Some(Rect::new(6, 6, 8, 8)));
        let plain = cmd;
        assert_eq!(cpu::src_window(&plain, &Rect::new(0, 0, 4, 4)), Some(Rect::new(0, 0, 2, 2)));
        assert_eq!(cpu::src_window(&plain, &Rect::new(5, 5, 6, 6)), Some(Rect::new(2, 2, 3, 3)));
        let mut dst = vec![0u8; 16 * 16 * 4];
        let mut dv = ViewMut { data: &mut dst, pitch: 64, x0: 0, y0: 0, w: 16, h: 16 };
        let mut done = Done::default();
        run(&cmd, &Rect::new(0, 0, 16, 16), &mut dv, Some(&sv), None, &mut done);
        assert_eq!(done, Done { pixels: 256, skipped: 0 });
        assert_eq!(dv.get(15, 15), Some(63));
        assert_eq!(dv.get(1, 0), Some(0));
        assert_eq!(dv.get(2, 0), Some(1));
    }

    #[test]
    fn private_record_round_trips_and_rejects_others() {
        let p = Private { job: 0x1234_5678_9abc };
        let b = p.encode();
        assert_eq!(Private::decode(&b), Some(p));
        let mut c = b;
        c[0] = 0;
        assert_eq!(Private::decode(&c), None);
        let mut c = b;
        c[4] = 2;
        assert_eq!(Private::decode(&c), None);
        assert_eq!(Private::decode(&Private { job: 0 }.encode()), None);
        assert_eq!(Private::decode(&b[..8]), None);
        // Not the Present prefix's magic.
        assert_ne!(PRIVATE_MAGIC, 0x4850_424C);
        assert!(PRIVATE_BYTES <= 32);
    }

    #[test]
    fn timeline_is_monotonic() {
        let mut t = Timeline::default();
        let a = t.next();
        let b = t.next();
        assert!(!t.ready(a));
        t.complete(a);
        assert!(t.ready(a) && !t.ready(b));
        t.complete(a - 1 + 0);
        assert_eq!(t.completed, a);
        t.complete(b + 5);
        assert_eq!(t.completed, a, "past the submitted end is ignored");
        t.discharge_all();
        assert!(t.ready(b));
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n}");
            assert!(n.starts_with("Gdi"), "{n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()));
            assert!(!["GdiType", "GdiOImg", "GdiOFmt"].contains(n), "{n}");
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
        assert_ne!(KNOB, "GdiKnob");
    }

    /// The I/O half writes exactly these, and no other file spells a `Gdi` acceleration counter.
    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if !render.exists() {
            assert!(
                std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
                "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
                render.display()
            );
            return;
        }
        const WRITERS: &[&str] = &["ddi/gdi_accel.rs", "ddi/gdi_exec.rs"];
        let mut written: Vec<std::string::String> = Vec::new();
        for f in WRITERS {
            let text = std::fs::read_to_string(render.join(f)).unwrap();
            let mut rest = text.as_str();
            while let Some(i) = rest.find("b\"Gdi") {
                let tail = &rest[i + 2..];
                let end = tail.find('"').unwrap();
                written.push(tail[..end].into());
                rest = &tail[end + 1..];
            }
        }
        for n in COUNTERS {
            assert!(written.iter().any(|w| w == n), "{n} is listed but not written");
        }
        for w in &written {
            assert!(COUNTERS.contains(&w.as_str()), "{w} is written but not listed");
        }
    }
}
