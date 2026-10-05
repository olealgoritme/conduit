//! The EDID `GET_EDID` answers with (docs/VENUS.md "EDID"): an EDID 1.4 base
//! block and one DisplayID 2.0 extension, 256 bytes.
//!
//! The base block alone cannot describe a display wider or taller than 4095
//! pixels, or one whose pixel clock is above 655.35 MHz: 5120x1440@240 is
//! both. So the configured mode goes in the DisplayID extension, as its one
//! Type VII detailed timing, marked preferred, and the base block's first
//! detailed timing is the largest mode it can hold (`base_timing`): the
//! configured one when it fits, else a stand-in of the same aspect ratio
//! that a guest reading only the base block can still drive.
//!
//! Every timing is CVT reduced blanking v2 (VESA CVT 1.2, `cvt_rb2`).
//!
//! Layouts are from the specs as the two parsers that matter read them:
//! Linux's drivers/gpu/drm/drm_edid.c (`drm_mode_displayid_detailed`,
//! struct `displayid_detailed_timings_1` in drm_displayid_internal.h) and
//! edid-decode (v4l-utils utils/edid-decode, parse-base-block.cpp and
//! parse-displayid-block.cpp `parse_displayid_type_1_7_timing`). Its output
//! for 5120x1440@240 is checked in `testdata/` and passes `edid-decode
//! --check`.

/// The EDID served: base block and one extension.
pub const EDID_LEN: usize = 256;
const BLOCK: usize = 128;

/// Without a configured refresh.
pub const DEFAULT_REFRESH_HZ: u32 = 60;
/// Refreshes are clamped to this: `DisplayMode` takes no more, and CVT's
/// 460 us of vertical blanking must leave some of the frame.
const MAX_REFRESH_HZ: u32 = 1000;

// CVT-RB2 (VESA CVT 1.2 §5.4; edid-decode calc-gtf-cvt.cpp `calc_cvt_mode`).
const RB2_H_BLANK: u32 = 80;
const RB2_H_FRONT: u32 = 8;
const RB2_H_SYNC: u32 = 32;
const RB2_V_SYNC: u32 = 8;
const RB2_V_BACK: u32 = 6;
const RB2_V_FRONT_MIN: u32 = 1;
const RB2_MIN_V_BLANK_US: u64 = 460;

// What an 18-byte detailed timing descriptor can hold.
const DTD_MAX_ACTIVE: u32 = 4095;
const DTD_MAX_BLANK: u32 = 4095;
const DTD_MAX_H_PORCH: u32 = 1023;
const DTD_MAX_V_PORCH: u32 = 63;
/// 10 kHz units in 16 bits: 655.35 MHz.
const DTD_MAX_CLOCK_KHZ: u32 = 65535 * 10;

/// Manufacturer `CDT`, product 0x0001.
const VENDOR: [u8; 3] = *b"CDT";
const PRODUCT: u16 = 0x0001;
const NAME: &[u8] = b"Conduit";
/// Model year 2026 (EDID counts from 1990).
const YEAR: u8 = 36;

/// sRGB primaries and D65, in 1/10000: red, green, blue, white (x, y).
const SRGB: [(u32, u32); 4] = [(6400, 3300), (3000, 6000), (1500, 600), (3127, 3290)];

/// A video timing. Sync widths and porches in pixels and lines; sync is
/// horizontal positive and vertical negative, as CVT reduced blanking has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub hactive: u32,
    /// Front porch, sync and back porch together.
    pub hblank: u32,
    pub hfront: u32,
    pub hsync: u32,
    pub vactive: u32,
    pub vblank: u32,
    pub vfront: u32,
    pub vsync: u32,
    pub clock_khz: u32,
}

impl Timing {
    pub fn htotal(&self) -> u32 {
        self.hactive + self.hblank
    }

    pub fn vtotal(&self) -> u32 {
        self.vactive + self.vblank
    }

    /// The refresh this timing really has, in millihertz: a little under
    /// the one asked for, as the clock is rounded down to a kHz.
    pub fn refresh_mhz(&self) -> u32 {
        (self.clock_khz as u64 * 1_000_000 / (self.htotal() as u64 * self.vtotal() as u64)) as u32
    }
}

/// CVT-RB2 timing for `width`x`height` at `hz`:
///
/// - horizontal: blank 80 (front porch 8, sync 32, back porch 40);
/// - vertical: sync 8, back porch 6, and the front porch whatever makes the
///   blanking at least 460 us: with the line period estimated as
///   `(1/hz - 460 us) / height`, `vblank = max(floor(460 us / period) + 1,
///   1 + 8 + 6)` lines, so `vfront = vblank - 14`;
/// - clock = `htotal * vtotal * hz`, rounded down to a kHz.
pub fn cvt_rb2(width: u32, height: u32, hz: u32) -> Timing {
    let (w, h) = (width.max(1), height.max(1));
    let hz = hz.clamp(1, MAX_REFRESH_HZ) as u64;
    // floor(460 / ((1e6 / hz - 460) / h)), in integers.
    let vbi =
        (RB2_MIN_V_BLANK_US * hz * h as u64 / (1_000_000 - RB2_MIN_V_BLANK_US * hz)) as u32 + 1;
    let vblank = vbi.max(RB2_V_FRONT_MIN + RB2_V_SYNC + RB2_V_BACK);
    let htotal = (w + RB2_H_BLANK) as u64;
    let vtotal = (h + vblank) as u64;
    Timing {
        hactive: w,
        hblank: RB2_H_BLANK,
        hfront: RB2_H_FRONT,
        hsync: RB2_H_SYNC,
        vactive: h,
        vblank,
        vfront: vblank - RB2_V_SYNC - RB2_V_BACK,
        vsync: RB2_V_SYNC,
        clock_khz: (htotal * vtotal * hz / 1000) as u32,
    }
}

/// `t` as a detailed timing descriptor can carry it, if it can. A vertical
/// front porch over the field's 63 lines gives the rest to the back porch:
/// the same totals, clock and refresh, the sync a little later.
fn dtd_form(t: Timing) -> Option<Timing> {
    let fits = t.hactive <= DTD_MAX_ACTIVE
        && t.vactive <= DTD_MAX_ACTIVE
        && t.hblank <= DTD_MAX_BLANK
        && t.vblank <= DTD_MAX_BLANK
        && t.hfront <= DTD_MAX_H_PORCH
        && t.hsync <= DTD_MAX_H_PORCH
        && t.vsync <= DTD_MAX_V_PORCH
        && t.clock_khz <= DTD_MAX_CLOCK_KHZ;
    fits.then(|| Timing {
        vfront: t.vfront.min(DTD_MAX_V_PORCH),
        ..t
    })
}

/// The base block's detailed timing, and whether it is the configured mode
/// itself. If it is not, the size is halved until both sides fit in 4095
/// (keeping the aspect ratio), then the refresh lowered a hertz at a time
/// until the clock fits: 5120x1440@240 stands in as 2560x720@240,
/// 3840x2160@144 as 3840x2160@74.
pub fn base_timing(width: u32, height: u32, hz: u32) -> (Timing, bool) {
    let hz = hz.clamp(1, MAX_REFRESH_HZ);
    let (mut w, mut h) = (width.max(1), height.max(1));
    while w > DTD_MAX_ACTIVE || h > DTD_MAX_ACTIVE {
        w = w.div_ceil(2);
        h = h.div_ceil(2);
    }
    for r in (1..=hz).rev() {
        if let Some(t) = dtd_form(cvt_rb2(w, h, r)) {
            return (t, (w, h, r) == (width, height, hz));
        }
    }
    // Unreachable: anything up to 4095x4095 fits at 1 Hz.
    (dtd_form(cvt_rb2(640, 480, 60)).expect("fits"), false)
}

/// The EDID for a display of `width`x`height` at `hz`.
pub fn edid(width: u32, height: u32, hz: u32) -> [u8; EDID_LEN] {
    let native = cvt_rb2(width, height, hz);
    let (base, exact) = base_timing(width, height, hz);
    let mut out = [0u8; EDID_LEN];
    out[..BLOCK].copy_from_slice(&base_block(&base, exact));
    out[BLOCK..].copy_from_slice(&displayid_block(&native));
    out
}

/// Set the last byte so the block sums to zero.
fn checksum(b: &mut [u8]) {
    let (last, rest) = b.split_last_mut().expect("not empty");
    *last = rest.iter().fold(0u8, |s, &v| s.wrapping_sub(v));
}

/// The EDID 1.4 base block (VESA E-EDID A2 §3).
fn base_block(t: &Timing, exact: bool) -> [u8; BLOCK] {
    let mut b = [0u8; BLOCK];
    b[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    // Three letters, 5 bits each ('A' = 1), big-endian.
    let id = VENDOR
        .iter()
        .fold(0u16, |v, &c| (v << 5) | (c - b'A' + 1) as u16);
    b[8..10].copy_from_slice(&id.to_be_bytes());
    b[10..12].copy_from_slice(&PRODUCT.to_le_bytes());
    // 12..16 serial number: none. 16 week: unknown.
    b[17] = YEAR;
    (b[18], b[19]) = (1, 4);
    // Digital, 8 bits per colour, interface not defined.
    b[20] = 0x80 | (0b010 << 4);
    // 21, 22: physical size unknown (both zero).
    // Gamma 2.20, stored as gamma * 100 - 100.
    b[23] = 120;
    // RGB 4:4:4, sRGB the default colour space; bit 1: the first detailed
    // timing is the native mode and refresh.
    b[24] = 0x04 | if exact { 0x02 } else { 0 };
    // Chromaticity: 10-bit fractions, low bits packed in 25 and 26.
    let c: Vec<u32> = SRGB
        .iter()
        .flat_map(|&(x, y)| [x, y])
        .map(|v| (v * 1024 + 5000) / 10000)
        .collect();
    let lo = |v: u32| (v & 3) as u8;
    b[25] = lo(c[0]) << 6 | lo(c[1]) << 4 | lo(c[2]) << 2 | lo(c[3]);
    b[26] = lo(c[4]) << 6 | lo(c[5]) << 4 | lo(c[6]) << 2 | lo(c[7]);
    for (i, v) in c.iter().enumerate() {
        b[27 + i] = (v >> 2) as u8;
    }
    // 35..38 established timings: none. 38..54 standard timings: unused.
    b[38..54].fill(0x01);
    b[54..72].copy_from_slice(&dtd(t));
    b[72..90].copy_from_slice(&text_descriptor(0xfc, NAME));
    // Two dummy descriptors. No range limits: EDID 1.4 asks for them only
    // of a continuous-frequency display, and they could not cover every
    // refresh `--display` takes anyway.
    b[93] = 0x10;
    b[111] = 0x10;
    b[126] = 1;
    checksum(&mut b);
    b
}

/// An 18-byte detailed timing descriptor (E-EDID A2 §3.10.2). `t` must
/// already fit (`dtd_form`).
fn dtd(t: &Timing) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[..2].copy_from_slice(&((t.clock_khz / 10) as u16).to_le_bytes());
    let hi = |a: u32, b: u32| ((a >> 8) << 4 | (b >> 8)) as u8;
    d[2] = t.hactive as u8;
    d[3] = t.hblank as u8;
    d[4] = hi(t.hactive, t.hblank);
    d[5] = t.vactive as u8;
    d[6] = t.vblank as u8;
    d[7] = hi(t.vactive, t.vblank);
    d[8] = t.hfront as u8;
    d[9] = t.hsync as u8;
    d[10] = ((t.vfront & 0xf) << 4 | (t.vsync & 0xf)) as u8;
    d[11] =
        ((t.hfront >> 8) << 6 | (t.hsync >> 8) << 4 | (t.vfront >> 4) << 2 | (t.vsync >> 4)) as u8;
    // 12..15 image size in mm: unknown. 15, 16 borders: none.
    // Not interlaced, digital separate sync, vertical negative, horizontal
    // positive.
    d[17] = 0x18 | 0x02;
    d
}

/// A display descriptor holding up to 13 bytes of text, newline-terminated
/// and space-padded.
fn text_descriptor(tag: u8, s: &[u8]) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = tag;
    d[5..].fill(b' ');
    d[5..5 + s.len()].copy_from_slice(s);
    if s.len() < 13 {
        d[5 + s.len()] = b'\n';
    }
    d
}

/// DisplayID's aspect ratio code for a Type VII timing, 8 (undefined) for
/// anything else.
fn aspect_code(w: u32, h: u32) -> u8 {
    const RATIOS: [(u32, u32); 8] = [
        (1, 1),
        (5, 4),
        (4, 3),
        (15, 9),
        (16, 9),
        (16, 10),
        (64, 27),
        (256, 135),
    ];
    RATIOS
        .iter()
        .position(|&(a, b)| w * b == h * a)
        .map_or(8, |i| i as u8)
}

/// DisplayID 2.0 primary use case: none of the listed; a generic display.
const PRIMARY_USE_GENERIC: u8 = 2;
/// DisplayID 2.0 data block tags.
const TAG_PRODUCT_ID: u8 = 0x20;
const TAG_DISPLAY_PARAMETERS: u8 = 0x21;
const TAG_TYPE_VII_TIMING: u8 = 0x22;
const TAG_INTERFACE_FEATURES: u8 = 0x26;
/// Bytes of the Product Identification data block.
const PRODUCT_ID_LEN: usize = 3 + 12 + NAME.len();
/// Where the Type VII descriptor starts in the extension block: after the
/// tag, the section header, the Product Identification and Display
/// Parameters blocks, and its own block header.
pub const TYPE_VII_AT: usize = 1 + 4 + PRODUCT_ID_LEN + 32 + 3;
/// "Not specified" for a DisplayID luminance (IEEE 754 half-precision -0).
const LUMINANCE_UNSPECIFIED: u16 = 0x8000;

/// The DisplayID 2.0 extension block (EDID extension tag 0x70): one section
/// of Product Identification, Display Parameters, Type VII detailed timing
/// and Display Interface Features data blocks. The section's primary use is
/// a display, which DisplayID 2.0 says must have the Display Parameters
/// block, and edid-decode releases before 2024 also want the other two.
fn displayid_block(t: &Timing) -> [u8; BLOCK] {
    let mut blocks = Vec::with_capacity(BLOCK);
    blocks.extend_from_slice(&product_id());
    blocks.extend_from_slice(&display_parameters(t.hactive, t.vactive));
    blocks.extend_from_slice(&[TAG_TYPE_VII_TIMING, 0x00, 20]);
    debug_assert_eq!(5 + blocks.len(), TYPE_VII_AT);
    blocks.extend_from_slice(&type_vii(t, true));
    blocks.extend_from_slice(&interface_features());

    let mut b = [0u8; BLOCK];
    b[0] = 0x70;
    // Section: version 2.0, bytes of data blocks, primary use, extensions.
    b[1] = 0x20;
    b[2] = blocks.len() as u8;
    b[3] = PRIMARY_USE_GENERIC;
    b[4] = 0;
    b[5..5 + blocks.len()].copy_from_slice(&blocks);
    // The section checksum follows the data blocks and covers the section
    // from its version byte; the 0x70 tag is outside it.
    let end = 5 + blocks.len();
    checksum(&mut b[1..=end]);
    checksum(&mut b);
    b
}

/// Product Identification data block (DisplayID 2.0 §4.1, tag 0x20): no
/// IEEE OUI (Conduit has none; the EDID's `CDT` is a PNP id, and edid-decode
/// warns of a zero OUI, the only warning it has for this EDID), product 1, no
/// serial number, made 2026, named "Conduit".
fn product_id() -> [u8; PRODUCT_ID_LEN] {
    let mut d = [0u8; PRODUCT_ID_LEN];
    (d[0], d[1], d[2]) = (TAG_PRODUCT_ID, 0x00, (PRODUCT_ID_LEN - 3) as u8);
    // 3..6 OUI: none.
    d[6..8].copy_from_slice(&PRODUCT.to_le_bytes());
    // 8..12 serial number: none. 12 week: unknown.
    // Year, counted from 2000.
    d[13] = YEAR - 10;
    d[14] = NAME.len() as u8;
    d[15..].copy_from_slice(NAME);
    d
}

/// Display Interface Features data block (DisplayID 2.0 §4.6, tag 0x26, 9
/// bytes): RGB at 8 bits per colour, sRGB; no YCbCr, no audio.
fn interface_features() -> [u8; 12] {
    let mut d = [0u8; 12];
    (d[0], d[1], d[2]) = (TAG_INTERFACE_FEATURES, 0x00, 9);
    // Bits per colour for RGB: bit 1, 8 bpc.
    d[3] = 0x02;
    // Colour space and EOTF combinations: bit 0, sRGB.
    d[9] = 0x01;
    d
}

/// Display Parameters data block (DisplayID 2.0 §4.2, tag 0x21, revision 0,
/// 29 bytes): image size unknown, native `w`x`h`, sRGB, 8 bits per colour,
/// luminance unspecified, gamma 2.2.
fn display_parameters(w: u32, h: u32) -> [u8; 32] {
    let mut d = [0u8; 32];
    (d[0], d[1], d[2]) = (TAG_DISPLAY_PARAMETERS, 0x00, 29);
    // 3..7: image size, 0.1 mm: unknown.
    d[7..9].copy_from_slice(&(w.min(0xffff) as u16).to_le_bytes());
    d[9..11].copy_from_slice(&(h.min(0xffff) as u16).to_le_bytes());
    // Left to right, top to bottom; luminance minimum guaranteed; CIE 1931;
    // no integrated audio.
    d[11] = 0x80;
    // Chromaticity: 12-bit fractions, x then y, 3 bytes a pair.
    for (i, &(x, y)) in SRGB.iter().enumerate() {
        let x = (x * 4096 + 5000) / 10000;
        let y = (y * 4096 + 5000) / 10000;
        let at = 12 + i * 3;
        d[at] = x as u8;
        d[at + 1] = ((y & 0xf) << 4 | (x >> 8)) as u8;
        d[at + 2] = (y >> 4) as u8;
    }
    for at in [24, 26, 28] {
        d[at..at + 2].copy_from_slice(&LUMINANCE_UNSPECIFIED.to_le_bytes());
    }
    // 8 bits per colour; technology not specified.
    d[30] = 0x02;
    // Gamma 2.20, gamma * 100 - 100.
    d[31] = 120;
    d
}

/// A 20-byte Type VII detailed timing descriptor (DisplayID 2.0 §4.3.1).
/// Every field is little-endian and stored as its value minus one:
///
/// | bytes | field |
/// |---|---|
/// | 0..3 | pixel clock, kHz, minus 1 (24 bits) |
/// | 3 | bit 7 preferred, 6:5 stereo (0 none), 4 interlaced, 3:0 aspect |
/// | 4..6 | horizontal active - 1 |
/// | 6..8 | horizontal blank - 1 |
/// | 8..10 | bits 14:0 horizontal front porch - 1; bit 15 sync positive |
/// | 10..12 | horizontal sync width - 1 |
/// | 12..14 | vertical active - 1 |
/// | 14..16 | vertical blank - 1 |
/// | 16..18 | bits 14:0 vertical front porch - 1; bit 15 sync positive |
/// | 18..20 | vertical sync width - 1 |
fn type_vii(t: &Timing, preferred: bool) -> [u8; 20] {
    let mut d = [0u8; 20];
    d[..3].copy_from_slice(&(t.clock_khz - 1).to_le_bytes()[..3]);
    d[3] = if preferred { 0x80 } else { 0 } | aspect_code(t.hactive, t.vactive);
    let mut put = |at: usize, v: u32| {
        d[at..at + 2].copy_from_slice(&((v - 1) as u16).to_le_bytes());
    };
    put(4, t.hactive);
    put(6, t.hblank);
    put(8, t.hfront);
    put(10, t.hsync);
    put(12, t.vactive);
    put(14, t.vblank);
    put(16, t.vfront);
    put(18, t.vsync);
    // CVT reduced blanking: horizontal sync positive, vertical negative.
    d[9] |= 0x80;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Type VII descriptor read back as Linux's
    /// `drm_mode_displayid_detailed` reads it.
    fn parse_type_vii(d: &[u8]) -> (Timing, bool, bool, bool) {
        let le = |at: usize| u16::from_le_bytes([d[at], d[at + 1]]) as u32;
        let t = Timing {
            clock_khz: (d[0] as u32 | (d[1] as u32) << 8 | (d[2] as u32) << 16) + 1,
            hactive: le(4) + 1,
            hblank: le(6) + 1,
            hfront: (le(8) & 0x7fff) + 1,
            hsync: le(10) + 1,
            vactive: le(12) + 1,
            vblank: le(14) + 1,
            vfront: (le(16) & 0x7fff) + 1,
            vsync: le(18) + 1,
        };
        (
            t,
            d[3] & 0x80 != 0,
            le(8) & 0x8000 != 0,
            le(16) & 0x8000 != 0,
        )
    }

    /// A detailed timing descriptor read back.
    fn parse_dtd(d: &[u8]) -> Timing {
        let hi = |b: u8, shift: u32| ((b >> shift) & 0xf) as u32;
        Timing {
            clock_khz: u16::from_le_bytes([d[0], d[1]]) as u32 * 10,
            hactive: d[2] as u32 | hi(d[4], 4) << 8,
            hblank: d[3] as u32 | hi(d[4], 0) << 8,
            vactive: d[5] as u32 | hi(d[7], 4) << 8,
            vblank: d[6] as u32 | hi(d[7], 0) << 8,
            hfront: d[8] as u32 | ((d[11] >> 6) as u32 & 3) << 8,
            hsync: d[9] as u32 | ((d[11] >> 4) as u32 & 3) << 8,
            vfront: (d[10] >> 4) as u32 | ((d[11] >> 2) as u32 & 3) << 4,
            vsync: (d[10] & 0xf) as u32 | (d[11] as u32 & 3) << 4,
        }
    }

    fn sums_to_zero(b: &[u8]) -> bool {
        b.iter().fold(0u8, |s, &v| s.wrapping_add(v)) == 0
    }

    /// The modes Conduit is run with, and what each one's EDID says.
    const MODES: [(u32, u32, u32); 5] = [
        (5120, 1440, 240),
        (3840, 2160, 144),
        (2560, 1440, 240),
        (1920, 1080, 60),
        (7680, 4320, 60),
    ];

    #[test]
    fn rb2_timings_are_cvts() {
        // `edid-decode --cvt w=W,h=H,fps=HZ,rb=2` gives these (its
        // `calc_cvt_mode`): vertical front porch and clock in kHz.
        let want = [
            ((5120, 1440, 240), 165, 2_020_512),
            ((3840, 2160, 144), 140, 1_306_206),
            ((2560, 1440, 240), 165, 1_025_798),
            ((1920, 1080, 60), 17, 133_320),
            ((7680, 4320, 60), 109, 2_068_660),
        ];
        for ((w, h, hz), vfront, clock_khz) in want {
            let t = cvt_rb2(w, h, hz);
            assert_eq!((t.vfront, t.clock_khz), (vfront, clock_khz), "{w}x{h}@{hz}");
            assert_eq!((t.vsync, t.vblank - t.vfront - t.vsync), (8, 6));
            assert_eq!(
                (t.hfront, t.hsync, t.hblank - t.hfront - t.hsync),
                (8, 32, 40)
            );
        }
        for (w, h, hz) in MODES {
            let t = cvt_rb2(w, h, hz);
            // The blanking is at least 460 us and less than a line more.
            let line_ns = t.htotal() as u64 * 1_000_000 / t.clock_khz as u64;
            let blank_ns = line_ns * t.vblank as u64;
            assert!(blank_ns + line_ns >= 460_000, "{w}x{h}@{hz}");
            // The refresh is the asked-for one, less the clock rounding.
            let mhz = t.refresh_mhz();
            assert!(
                mhz <= hz * 1000 && mhz + 10 > hz * 1000,
                "{w}x{h}@{hz}: {mhz}"
            );
        }
        // A tiny mode at a low rate keeps the minimum blanking.
        assert_eq!(cvt_rb2(640, 480, 1).vblank, 15);
    }

    #[test]
    fn base_timing_is_the_mode_when_it_fits() {
        let (t, exact) = base_timing(1920, 1080, 60);
        assert!(exact);
        assert_eq!(t, cvt_rb2(1920, 1080, 60));
        // 1920x1080@240 fits but for its front porch, which moves to the
        // back porch: same totals and clock.
        let rb2 = cvt_rb2(1920, 1080, 240);
        assert!(rb2.vfront > 63);
        let (t, exact) = base_timing(1920, 1080, 240);
        assert!(exact);
        assert_eq!(t.vfront, 63);
        assert_eq!((t.vtotal(), t.clock_khz), (rb2.vtotal(), rb2.clock_khz));
    }

    #[test]
    fn base_timing_stands_in_at_the_same_aspect() {
        let cases = [
            ((5120, 1440, 240), (2560, 720, 240)),
            ((3840, 2160, 144), (3840, 2160, 74)),
            ((2560, 1440, 240), (2560, 1440, 159)),
            ((7680, 4320, 60), (3840, 2160, 60)),
        ];
        for ((w, h, hz), (bw, bh, bhz)) in cases {
            let (t, exact) = base_timing(w, h, hz);
            assert!(!exact, "{w}x{h}@{hz}");
            assert_eq!((t.hactive, t.vactive), (bw, bh), "{w}x{h}@{hz}");
            assert_eq!(t.hactive * h, t.vactive * w, "aspect of {w}x{h}");
            assert!(t.clock_khz <= DTD_MAX_CLOCK_KHZ);
            assert_eq!(t.refresh_mhz().div_ceil(1000), bhz, "{w}x{h}@{hz}");
            // The next hertz up would not fit.
            assert!(dtd_form(cvt_rb2(bw, bh, bhz + 1)).is_none() || bhz == hz);
        }
    }

    #[test]
    fn blocks_are_well_formed() {
        for (w, h, hz) in MODES {
            let e = edid(w, h, hz);
            let (base, ext) = e.split_at(BLOCK);
            assert_eq!(base[..8], [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
            assert_eq!(&base[8..10], &[0x0c, 0x94], "CDT");
            assert_eq!((base[18], base[19]), (1, 4));
            assert_eq!(base[126], 1, "one extension");
            assert!(sums_to_zero(base), "{w}x{h}@{hz} base checksum");
            assert!(sums_to_zero(ext), "{w}x{h}@{hz} extension checksum");
            // Monitor name: exactly "Conduit", newline, space padding, as
            // the guest driver's own fallback EDID has it.
            assert_eq!(base[72..77], [0, 0, 0, 0xfc, 0]);
            assert_eq!(&base[77..90], b"Conduit\n     ");
            // DisplayID: tag, version, and the section checksum.
            assert_eq!((ext[0], ext[1], ext[3], ext[4]), (0x70, 0x20, 2, 0));
            let len = ext[2] as usize;
            assert_eq!(len, PRODUCT_ID_LEN + 32 + 23 + 12);
            assert!(sums_to_zero(&ext[1..6 + len]), "section checksum");
            assert!(ext[6 + len..127].iter().all(|&v| v == 0), "padding");
            // The base DTD is the base_timing, read back.
            let (bt, exact) = base_timing(w, h, hz);
            let mut want = bt;
            want.clock_khz = bt.clock_khz / 10 * 10;
            assert_eq!(parse_dtd(&base[54..72]), want);
            assert_eq!(base[24] & 0x02 != 0, exact);
            assert_eq!(base[71], 0x1a, "digital separate, H+ V-");
        }
    }

    #[test]
    fn displayid_carries_the_real_mode() {
        for (w, h, hz) in MODES {
            let e = edid(w, h, hz);
            let ext = &e[BLOCK..];
            // Product Identification, then Display Parameters: tag,
            // revision, length, native size.
            assert_eq!(ext[5..8], [0x20, 0x00, 19]);
            assert_eq!(&ext[20..27], b"Conduit");
            let p = 5 + PRODUCT_ID_LEN;
            assert_eq!(ext[p..p + 3], [0x21, 0x00, 29]);
            let le = |at: usize| u16::from_le_bytes([ext[at], ext[at + 1]]) as u32;
            assert_eq!((le(p + 7), le(p + 9)), (w, h));
            // Type VII: tag, revision 0 (20-byte descriptors), 20 bytes.
            let at = TYPE_VII_AT;
            assert_eq!(ext[at - 3..at], [0x22, 0x00, 20]);
            let (t, preferred, hpos, vpos) = parse_type_vii(&ext[at..at + 20]);
            assert_eq!(t, cvt_rb2(w, h, hz), "{w}x{h}@{hz}");
            assert!(preferred && hpos && !vpos);
            assert_eq!(t.refresh_mhz().div_ceil(1000), hz);
            // Display Interface Features last.
            assert_eq!(ext[at + 20..at + 23], [0x26, 0x00, 9]);
        }
    }

    /// The bytes, for 5120x1440@240, as checked in for the guest's parser
    /// tests (edid-decode's reading of them is beside it).
    #[test]
    fn sample_matches_testdata() {
        let want = include_bytes!("testdata/edid-5120x1440-240.bin");
        let got = edid(5120, 1440, 240);
        if std::env::var_os("CONDUIT_WRITE_EDID_SAMPLE").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/venus/testdata/edid-5120x1440-240.bin"
                ),
                got,
            )
            .unwrap();
        }
        assert_eq!(&got[..], &want[..]);
        let at = BLOCK + TYPE_VII_AT;
        let (t, ..) = parse_type_vii(&got[at..at + 20]);
        assert_eq!((t.hactive, t.vactive, t.clock_khz), (5120, 1440, 2_020_512));
    }

    #[test]
    fn aspect_codes() {
        assert_eq!(aspect_code(3840, 2160), 4);
        assert_eq!(aspect_code(1920, 1200), 5);
        assert_eq!(aspect_code(3440, 1440), 8);
        assert_eq!(aspect_code(5120, 1440), 8);
        assert_eq!(aspect_code(5120, 2160), 6);
    }
}
