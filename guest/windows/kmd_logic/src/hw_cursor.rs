//! Hardware cursor (`HwCursor`): the pure half. The I/O half is
//! `kmd_render/src/ddi/hw_cursor.rs` (knob, counters, the blob, the host command); the design,
//! the transitions and the test recipe are `docs/independent-flip.md` section 12, the host side
//! `docs/SCANOUT.md` "Hardware cursor, Windows guests".
//!
//! dxgkrnl hands `DxgkDdiSetPointerShape` one of three shapes; the host shows one kind, a
//! premultiplied ARGB8888 image (the host pointer's image, as a Linux guest's cursor plane):
//!
//! * **Color** (`DXGK_POINTERFLAGS.Color`): 32-bpp ARGB, per-pixel alpha. Premultiplied when it
//!   is not already ([`needs_premultiply`]: some colour channel above its alpha means straight
//!   alpha).
//! * **MaskedColor**: 32-bpp ARGB whose alpha is a mask: 0x00 replaces the screen pixel with the
//!   colour, 0xFF XORs the colour into it (an XOR with black is transparent).
//! * **Monochrome**: a 1-bpp AND mask followed by a 1-bpp XOR mask of the same size, rows top
//!   down, bits MSB first: AND 0 / XOR 0 black, 0 / 1 white, 1 / 0 transparent, 1 / 1 inverts.
//!
//! An ARGB plane cannot invert what is under it. An inverting pixel is drawn black, and every
//! transparent pixel next to one (4-neighbourhood) white: the text-select I-beam, which inverts,
//! stays visible on dark and light backgrounds alike ([`Px::Invert`]).
//!
//! What is here: [`validate`] (what the KMD accepts, else dxgkrnl draws a software cursor),
//! [`convert_row`] (one output row), [`advertise`] (whether the caps carry a pointer), and the
//! slot layout of the cursor blob ([`Slots`]).

/// Largest side of a cursor the host shows (the Linux cursor plane's limit, and the viewer's).
pub const MAX_DIM: u32 = 256;
/// Images in the cursor blob: one on screen while the next is written.
pub const SLOTS: u32 = 2;
/// Bytes of one output row at the largest width.
pub const MAX_ROW_BYTES: u32 = MAX_DIM * 4;

/// `DXGK_POINTERFLAGS` bits.
pub const FLAG_MONOCHROME: u32 = 1 << 0;
pub const FLAG_COLOR: u32 = 1 << 1;
pub const FLAG_MASKED_COLOR: u32 = 1 << 2;
/// `DXGK_DRIVERCAPS.PointerCaps` with the pointer on: all three.
pub const POINTER_CAPS: u32 = FLAG_MONOCHROME | FLAG_COLOR | FLAG_MASKED_COLOR;

/// `HwCursor` values. Absent is [`KNOB_ON`].
pub const KNOB_OFF: u32 = 0;
pub const KNOB_ON: u32 = 1;
/// Advertise even when the host does not say it serves the cursor (bring-up: every shape then
/// fails over to the software cursor on an old host).
pub const KNOB_FORCE: u32 = 2;

/// Host config `features` bits (`host/backend/protocol/src/messages.rs`); this crate has no
/// dependency on the protocol crate, so the two are restated and checked there.
pub const CFG_CURSOR: u32 = 1 << 9;
pub const CFG_VENUS: u32 = 1 << 10;
pub const CFG_VENUS_CURSOR: u32 = 1 << 18;

/// The host serves `CMD_SET_CURSOR_BLOB`.
pub const fn host_serves(features: u32) -> bool {
    let need = CFG_CURSOR | CFG_VENUS | CFG_VENUS_CURSOR;
    features & need == need
}

/// Whether `DXGK_DRIVERCAPS` reports a hardware pointer. `host` is the config `features` word,
/// `None` when the transport is not up (then the caps follow the knob, and a shape the host
/// refuses falls back to the software cursor).
pub const fn advertise(knob: u32, display_half: bool, host: Option<u32>) -> bool {
    if knob == KNOB_OFF || !display_half {
        return false;
    }
    if knob == KNOB_FORCE {
        return true;
    }
    match host {
        Some(f) => host_serves(f),
        None => true,
    }
}

/// The shape kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Monochrome,
    Color,
    MaskedColor,
}

impl Kind {
    /// `CurFmt`: 1 monochrome, 2 color, 4 masked color (the flag bit).
    pub const fn code(self) -> u32 {
        match self {
            Kind::Monochrome => FLAG_MONOCHROME,
            Kind::Color => FLAG_COLOR,
            Kind::MaskedColor => FLAG_MASKED_COLOR,
        }
    }
}

/// Why a shape is not shown by the host (dxgkrnl then draws it in software).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refuse {
    /// Not exactly one of the three kinds.
    Flags = 1,
    /// Zero, or above [`MAX_DIM`].
    Size = 2,
    /// The pitch cannot hold a row.
    Pitch = 3,
    /// The hotspot is outside the image.
    Hotspot = 4,
    /// No pixels.
    NoPixels = 5,
    /// Not on source 0 (one output).
    Source = 6,
    /// The host has no cursor, or refused it (`CurWhy` 7 and up are the I/O half's).
    Host = 7,
    /// The cursor blob could not be made or mapped.
    Blob = 8,
}

impl Refuse {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// A shape [`validate`] accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    /// Source bytes per row (of each mask, for a monochrome shape).
    pub pitch: u32,
    pub hot_x: u32,
    pub hot_y: u32,
}

impl Shape {
    /// Bytes of source the shape reads.
    pub const fn src_len(&self) -> usize {
        let one = self.pitch as usize * self.height as usize;
        match self.kind {
            Kind::Monochrome => one * 2,
            _ => one,
        }
    }
}

/// `DXGKARG_SETPOINTERSHAPE`, judged.
#[allow(clippy::too_many_arguments)]
pub fn validate(
    flags: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hot_x: u32,
    hot_y: u32,
    source: u32,
    has_pixels: bool,
) -> Result<Shape, Refuse> {
    if source != 0 {
        return Err(Refuse::Source);
    }
    let kind = match flags & POINTER_CAPS {
        FLAG_MONOCHROME => Kind::Monochrome,
        FLAG_COLOR => Kind::Color,
        FLAG_MASKED_COLOR => Kind::MaskedColor,
        _ => return Err(Refuse::Flags),
    };
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(Refuse::Size);
    }
    let row = match kind {
        Kind::Monochrome => width.div_ceil(8),
        _ => width * 4,
    };
    if pitch < row {
        return Err(Refuse::Pitch);
    }
    if hot_x >= width || hot_y >= height {
        return Err(Refuse::Hotspot);
    }
    if !has_pixels {
        return Err(Refuse::NoPixels);
    }
    Ok(Shape {
        kind,
        width,
        height,
        pitch,
        hot_x,
        hot_y,
    })
}

/// One source pixel, classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Px {
    Transparent,
    /// ARGB, straight or premultiplied as the source has it.
    Argb(u32),
    /// Inverts the screen under it: drawn black, outlined white.
    Invert,
}

/// The inverting pixel and its outline.
pub const INVERT_CORE: u32 = 0xFF00_0000;
pub const INVERT_OUTLINE: u32 = 0xFFFF_FFFF;

fn le32(src: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]])
}

/// Pixel (`x`, `y`) of `shape` in `src` (at least [`Shape::src_len`] bytes).
pub fn pixel(shape: &Shape, src: &[u8], x: u32, y: u32) -> Px {
    let (x, y) = (x as usize, y as usize);
    let pitch = shape.pitch as usize;
    match shape.kind {
        Kind::Color => match le32(src, y * pitch + x * 4) {
            p if p >> 24 == 0 => Px::Transparent,
            p => Px::Argb(p),
        },
        Kind::MaskedColor => {
            let p = le32(src, y * pitch + x * 4);
            let rgb = p & 0x00FF_FFFF;
            if p >> 24 == 0 {
                Px::Argb(0xFF00_0000 | rgb)
            } else if rgb == 0 {
                Px::Transparent
            } else {
                Px::Invert
            }
        }
        Kind::Monochrome => {
            let bit = |base: usize| src[base + y * pitch + x / 8] >> (7 - (x % 8)) & 1;
            let and = bit(0);
            let xor = bit(pitch * shape.height as usize);
            match (and, xor) {
                (0, 0) => Px::Argb(0xFF00_0000),
                (0, _) => Px::Argb(0xFFFF_FFFF),
                (_, 0) => Px::Transparent,
                _ => Px::Invert,
            }
        }
    }
}

/// A color shape whose colour exceeds its alpha somewhere is straight alpha and needs
/// premultiplying; one that never does is taken as premultiplied already.
pub fn needs_premultiply(shape: &Shape, src: &[u8]) -> bool {
    if shape.kind != Kind::Color {
        return false;
    }
    for y in 0..shape.height {
        for x in 0..shape.width {
            let p = le32(src, (y * shape.pitch + x * 4) as usize);
            let a = p >> 24;
            if (p >> 16 & 0xFF) > a || (p >> 8 & 0xFF) > a || (p & 0xFF) > a {
                return true;
            }
        }
    }
    false
}

/// `p` with its colour multiplied by its alpha, rounded.
pub const fn premultiply(p: u32) -> u32 {
    const fn m(c: u32, a: u32) -> u32 {
        (c * a + 127) / 255
    }
    let a = p >> 24;
    a << 24 | m(p >> 16 & 0xFF, a) << 16 | m(p >> 8 & 0xFF, a) << 8 | m(p & 0xFF, a)
}

/// Output row `y`: `out[..width]` premultiplied ARGB. Returns the inverting pixels in the row.
pub fn convert_row(shape: &Shape, src: &[u8], y: u32, premul: bool, out: &mut [u32]) -> u32 {
    let mut inverts = 0;
    let near_invert = |x: u32, y: u32| {
        (x > 0 && pixel(shape, src, x - 1, y) == Px::Invert)
            || (x + 1 < shape.width && pixel(shape, src, x + 1, y) == Px::Invert)
            || (y > 0 && pixel(shape, src, x, y - 1) == Px::Invert)
            || (y + 1 < shape.height && pixel(shape, src, x, y + 1) == Px::Invert)
    };
    for x in 0..shape.width {
        out[x as usize] = match pixel(shape, src, x, y) {
            Px::Argb(p) if premul => premultiply(p),
            Px::Argb(p) => p,
            Px::Invert => {
                inverts += 1;
                INVERT_CORE
            }
            Px::Transparent if near_invert(x, y) => INVERT_OUTLINE,
            Px::Transparent => 0,
        };
    }
    inverts
}

/// Where the images live in the cursor blob: a linear image [`MAX_DIM`] wide and
/// `MAX_DIM * SLOTS` high, `row_pitch` bytes per row from `plane_offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    pub plane_offset: u32,
    pub row_pitch: u32,
    pub blob_size: u64,
}

impl Slots {
    /// The layout, if the image holds every slot.
    pub fn new(plane_offset: u64, row_pitch: u64, blob_size: u64) -> Option<Self> {
        if row_pitch < u64::from(MAX_ROW_BYTES) || row_pitch > u64::from(u32::MAX) {
            return None;
        }
        let end = plane_offset.checked_add(row_pitch.checked_mul(u64::from(MAX_DIM * SLOTS))?)?;
        if end > blob_size || plane_offset > u64::from(u32::MAX) || end > u64::from(u32::MAX) {
            return None;
        }
        Some(Self {
            plane_offset: plane_offset as u32,
            row_pitch: row_pitch as u32,
            blob_size,
        })
    }

    /// Byte offset of slot `slot` in the blob.
    pub const fn offset(&self, slot: u32) -> u32 {
        self.plane_offset + self.row_pitch * MAX_DIM * (slot % SLOTS)
    }

    /// The other slot.
    pub const fn next(slot: u32) -> u32 {
        (slot + 1) % SLOTS
    }
}

/// How a cursor command the host did not answer with success failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFailure {
    /// The host answered with an error: it cannot serve the command (an old backend).
    Refused,
    /// No answer within the timeout. The command is on the queue and the host runs it later (the
    /// control queue is in order and shared with every Venus `GpuCmd`: a busy backend delays it).
    Late,
    /// The command never reached the queue (full, or the transport is going away).
    NotSent,
}

/// What the KMD does about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterFailure {
    /// Leave the shape to dxgkrnl's software cursor (and hide the host image). Only for a refusal:
    /// the host will never show it.
    SoftwareCursor,
    /// Treat it as done: the host shows it when it gets to it. Falling back to software here
    /// hid a working host cursor and left the pointer to DWM's frames, which froze it for as long
    /// as the backend was busy (397.1, after Basemark).
    AssumeTaken,
    /// Keep the host image as it is and send again later ([`retry_due`]).
    RetryLater,
}

pub const fn after_failure(f: HostFailure) -> AfterFailure {
    match f {
        HostFailure::Refused => AfterFailure::SoftwareCursor,
        HostFailure::Late => AfterFailure::AssumeTaken,
        HostFailure::NotSent => AfterFailure::RetryLater,
    }
}

/// A command that was not sent is tried again from a position call at most this often (250 ms):
/// a position call runs at mouse rate and must not wait on a full queue every time.
pub const RETRY_AFTER_100NS: u64 = 2_500_000;

/// Whether a position call should try an owed command again (`last_try` 0: never tried).
pub const fn retry_due(now: u64, last_try: u64) -> bool {
    last_try == 0 || now.saturating_sub(last_try) >= RETRY_AFTER_100NS
}

/// Counter names (`kmd_render/src/ddi/hw_cursor.rs`), at most 14 characters, in the order the
/// driver writes them.
pub const COUNTERS: [&str; 20] = [
    "CurKnob",
    "CurCaps",
    "CurShapeN",
    "CurPosN",
    "CurShow",
    "CurHide",
    "CurFmt",
    "CurSize",
    "CurRefuse",
    "CurWhy",
    "CurHostErr",
    "CurXor",
    "CurRttUs",  // the last cursor command's round trip, microseconds
    "CurRttMax", // its maximum
    "CurTmo",    // commands that timed out (assumed taken: the host runs them late)
    "CurGateMs", // the longest a shape waited for another pointer operation's I/O, ms
    "CurSwN",    // software-cursor episodes (a refused shape until the host shows one again)
    "CurSwMs",   // the last episode's length, ms
    "CurSwMax",  // the longest, ms
    "CurRetry",  // commands sent again after one that never reached the queue
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn shape(kind: Kind, w: u32, h: u32, pitch: u32) -> Shape {
        Shape {
            kind,
            width: w,
            height: h,
            pitch,
            hot_x: 0,
            hot_y: 0,
        }
    }

    fn convert(s: &Shape, src: &[u8]) -> Vec<Vec<u32>> {
        let premul = needs_premultiply(s, src);
        (0..s.height)
            .map(|y| {
                let mut row = vec![0u32; s.width as usize];
                convert_row(s, src, y, premul, &mut row);
                row
            })
            .collect()
    }

    #[test]
    fn caps_follow_the_knob_and_the_host() {
        let host = CFG_CURSOR | CFG_VENUS | CFG_VENUS_CURSOR;
        assert!(advertise(KNOB_ON, true, Some(host)));
        assert!(
            !advertise(KNOB_ON, true, Some(CFG_CURSOR | CFG_VENUS)),
            "an old backend"
        );
        assert!(
            !advertise(KNOB_ON, true, Some(CFG_VENUS | CFG_VENUS_CURSOR)),
            "cursor off"
        );
        assert!(advertise(KNOB_ON, true, None), "transport not up: the knob");
        assert!(!advertise(KNOB_OFF, true, Some(host)));
        assert!(
            !advertise(KNOB_ON, false, Some(host)),
            "render-only has no pointer"
        );
        assert!(advertise(KNOB_FORCE, true, Some(0)));
        assert!(advertise(7, true, Some(host)), "any other value is on");
        assert!(!advertise(7, true, Some(0)));
    }

    #[test]
    fn validate_refuses_what_the_host_cannot_show() {
        let ok = validate(FLAG_COLOR, 32, 32, 128, 0, 0, 0, true).unwrap();
        assert_eq!((ok.kind, ok.src_len()), (Kind::Color, 32 * 128));
        let m = validate(FLAG_MONOCHROME, 32, 32, 4, 31, 31, 0, true).unwrap();
        assert_eq!(m.src_len(), 2 * 32 * 4);
        assert_eq!(validate(0, 32, 32, 128, 0, 0, 0, true), Err(Refuse::Flags));
        assert_eq!(
            validate(FLAG_COLOR | FLAG_MONOCHROME, 32, 32, 128, 0, 0, 0, true),
            Err(Refuse::Flags)
        );
        assert_eq!(
            validate(FLAG_COLOR, 0, 32, 128, 0, 0, 0, true),
            Err(Refuse::Size)
        );
        assert_eq!(
            validate(FLAG_COLOR, 257, 32, 2048, 0, 0, 0, true),
            Err(Refuse::Size)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 257, 128, 0, 0, 0, true),
            Err(Refuse::Size)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 32, 127, 0, 0, 0, true),
            Err(Refuse::Pitch)
        );
        assert_eq!(
            validate(FLAG_MONOCHROME, 33, 32, 4, 0, 0, 0, true),
            Err(Refuse::Pitch)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 32, 128, 32, 0, 0, true),
            Err(Refuse::Hotspot)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 32, 128, 0, 32, 0, true),
            Err(Refuse::Hotspot)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 32, 128, 0, 0, 0, false),
            Err(Refuse::NoPixels)
        );
        assert_eq!(
            validate(FLAG_COLOR, 32, 32, 128, 0, 0, 1, true),
            Err(Refuse::Source)
        );
        assert!(validate(FLAG_MASKED_COLOR, 256, 256, 1024, 255, 255, 0, true).is_ok());
    }

    #[test]
    fn color_is_premultiplied_only_when_straight() {
        let s = shape(Kind::Color, 2, 1, 8);
        // Straight: white at alpha 0x80 -> 0x80 grey.
        let src: Vec<u8> = [0x80FF_FFFFu32, 0xFF10_2030]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        assert!(needs_premultiply(&s, &src));
        assert_eq!(convert(&s, &src), vec![vec![0x8080_8080, 0xFF10_2030]]);
        // Already premultiplied: kept.
        let src: Vec<u8> = [0x8080_8080u32, 0x4000_0000]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        assert!(!needs_premultiply(&s, &src));
        assert_eq!(convert(&s, &src), vec![vec![0x8080_8080, 0x4000_0000]]);
        // Alpha 0 is transparent whatever the colour says.
        let src: Vec<u8> = [0x00FF_FFFFu32, 0]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        assert_eq!(convert(&s, &src), vec![vec![0, 0]]);
    }

    #[test]
    fn monochrome_masks_map_to_argb_with_an_outlined_inversion() {
        // 4x3, pitch 1. Row 0: black, white, transparent, invert. Rows 1, 2 transparent.
        let s = shape(Kind::Monochrome, 4, 3, 1);
        let and = [0b0011_0000u8, 0xFF, 0xFF];
        let xor = [0b0101_0000u8, 0x00, 0x00];
        let src: Vec<u8> = and.iter().chain(xor.iter()).copied().collect();
        let px: Vec<Px> = (0..4).map(|x| pixel(&s, &src, x, 0)).collect();
        assert_eq!(
            px,
            vec![
                Px::Argb(0xFF00_0000),
                Px::Argb(0xFFFF_FFFF),
                Px::Transparent,
                Px::Invert
            ]
        );
        let out = convert(&s, &src);
        assert_eq!(
            out[0],
            vec![0xFF00_0000, 0xFFFF_FFFF, INVERT_OUTLINE, INVERT_CORE]
        );
        // Under the inverting pixel: outline; elsewhere transparent.
        assert_eq!(out[1], vec![0, 0, 0, INVERT_OUTLINE]);
        assert_eq!(out[2], vec![0, 0, 0, 0]);
        let mut row = [0u32; 4];
        assert_eq!(convert_row(&s, &src, 0, false, &mut row), 1);
    }

    #[test]
    fn masked_color_alpha_is_the_xor_mask() {
        let s = shape(Kind::MaskedColor, 3, 1, 12);
        // Replace red; XOR black (transparent); XOR white (inverts).
        let src: Vec<u8> = [0x00FF_0000u32, 0xFF00_0000, 0xFFFF_FFFF]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect();
        assert_eq!(
            convert(&s, &src),
            vec![vec![0xFFFF_0000, INVERT_OUTLINE, INVERT_CORE]]
        );
    }

    #[test]
    fn slots_fit_the_image_or_are_refused() {
        let s = Slots::new(0, 1024, 512 * 1024).unwrap();
        assert_eq!((s.offset(0), s.offset(1), s.offset(2)), (0, 256 * 1024, 0));
        assert_eq!((Slots::next(0), Slots::next(1)), (1, 0));
        let s = Slots::new(4096, 1280, 4096 + 1280 * 512).unwrap();
        assert_eq!(s.offset(1), 4096 + 1280 * 256);
        assert!(Slots::new(0, 1020, 1 << 20).is_none(), "a row does not fit");
        assert!(
            Slots::new(0, 1024, 512 * 1024 - 1).is_none(),
            "the image does not fit"
        );
        assert!(Slots::new(u64::MAX, 1024, u64::MAX).is_none());
    }

    #[test]
    fn only_a_refusal_falls_back_to_the_software_cursor() {
        assert_eq!(
            after_failure(HostFailure::Refused),
            AfterFailure::SoftwareCursor
        );
        assert_eq!(after_failure(HostFailure::Late), AfterFailure::AssumeTaken);
        assert_eq!(
            after_failure(HostFailure::NotSent),
            AfterFailure::RetryLater
        );
    }

    #[test]
    fn a_retry_waits_a_quarter_second() {
        assert!(retry_due(10, 0));
        assert!(!retry_due(RETRY_AFTER_100NS, 1));
        assert!(retry_due(RETRY_AFTER_100NS + 1, 1));
    }

    #[test]
    fn counter_names_fit() {
        for n in COUNTERS {
            assert!(n.len() <= 14, "{n}");
        }
    }

    /// The counters `kmd_render/src/ddi/hw_cursor.rs` writes are exactly [`COUNTERS`] (skipped
    /// without the render tree beside this crate, unless `HELIOS_REQUIRE_NAME_SCAN=1`).
    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kmd_render/src/ddi/hw_cursor.rs");
        let Ok(text) = std::fs::read_to_string(&path) else {
            assert!(
                std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
                "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist",
                path.display()
            );
            return;
        };
        let mut found: Vec<&str> = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("record_named_bytes(b\"") {
            let tail = &rest[i + "record_named_bytes(b\"".len()..];
            let end = tail.find('"').unwrap();
            found.push(&tail[..end]);
            rest = &tail[end..];
        }
        assert_eq!(found, COUNTERS.to_vec());
    }
}
