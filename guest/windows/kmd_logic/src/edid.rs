//! Virtual-monitor EDID 1.4. No allocation, runtime globals, or WDK dependency.
//! The 128-byte base block can represent only 12-bit extents and 16-bit clocks.
//! Return None rather than wrapping/truncating an unsupported host mode.
//! Wire contract: VESA E-EDID A2, sections 3.4, 3.6, 3.10.

pub fn build_edid(
    w: u32,
    h: u32,
    name: &str,
    publisher: &str,
    model_year: u16,
) -> Option<[u8; 128]> {
    if !(1..=4095).contains(&w)
        || !(1..=4095).contains(&h)
        || !(1990..=2245).contains(&model_year)
        || [name, publisher].iter().any(|text| {
            text.is_empty() || text.len() > 12 || !text.bytes().all(|b| (32..=126).contains(&b))
        })
    {
        return None;
    }
    let hb: u32 = ((w / 4) & !7).max(160);
    let vb: u32 = 45;
    let ht = w + hb;
    let vt = h + vb;
    let pc = (ht as u64 * vt as u64 * 60) / 10_000;
    if !(1..=65535).contains(&pc) {
        return None;
    }
    let pc = pc as u32;
    let mut e = [0u8; 128];
    // Header + manufacturer "HLS" (5-bit letters, A=1) + product 0x0001.
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    let mfg: u16 = (8 << 10) | (12 << 5) | 19;
    e[8] = (mfg >> 8) as u8;
    e[9] = (mfg & 0xFF) as u8;
    e[10] = 0x01;
    e[16] = 0xFF; // Model year, not a fabricated manufacture week.
    e[17] = (model_year - 1990) as u8;
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0xA0; // Digital, 8bpc; no physical connector standard is claimed.
                  // The virtual display has no physical dimensions. Encode its aspect ratio
                  // when EDID can represent it; 0/0 means unspecified for extreme ratios.
    let ratio = if w >= h {
        (w * 100 + h / 2) / h
    } else {
        (h * 100 + w / 2) / w
    };
    if (100..=354).contains(&ratio) {
        e[if w >= h { 21 } else { 22 }] = (ratio - 99) as u8;
    }
    e[23] = 120; // gamma 2.2
    e[24] = 0x06; // sRGB, preferred native timing, no continuous-frequency claim
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Standard timings unused (0x0101 × 8).
    for b in e.iter_mut().take(54).skip(38) {
        *b = 0x01;
    }
    // Detailed Timing Descriptor 1 (bytes 54..72): w × h, ~60 Hz.
    let hfp = (hb / 3).min(88);
    let hsw = (hb / 5).min(44);
    let (vfp, vsw) = (3u32, 5u32);
    let d = &mut e[54..72];
    d[0] = (pc & 0xFF) as u8;
    d[1] = (pc >> 8) as u8;
    d[2] = (w & 0xFF) as u8;
    d[3] = (hb & 0xFF) as u8;
    d[4] = ((((w >> 8) & 0xF) << 4) | ((hb >> 8) & 0xF)) as u8;
    d[5] = (h & 0xFF) as u8;
    d[6] = (vb & 0xFF) as u8;
    d[7] = ((((h >> 8) & 0xF) << 4) | ((vb >> 8) & 0xF)) as u8;
    d[8] = (hfp & 0xFF) as u8;
    d[9] = (hsw & 0xFF) as u8;
    d[10] = (((vfp & 0xF) << 4) | (vsw & 0xF)) as u8;
    d[17] = 0x1E; // digital separate sync, +H +V
                  // No synthetic range-limit descriptor: this virtual display advertises its
                  // explicit timing only (E-EDID 1.4 section 3.10.3.3 makes ranges optional).
                  // ASCII text identifies the software publisher; HLS remains a stable PnP ID.
    text_descriptor(&mut e[72..90], 0xFE, publisher);
    // Descriptor 3: shared product name (0xFC). The build validates the EDID limit.
    text_descriptor(&mut e[90..108], 0xFC, name);
    // Descriptor 4: unused (0x10).
    e[108..113].copy_from_slice(&[0x00, 0x00, 0x00, 0x10, 0x00]);
    // Byte 126 = extension count (0). Byte 127 = checksum: sum of all 128 == 0 mod 256.
    let sum: u32 = e[..127].iter().map(|&b| b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    Some(e)
}

/// The monitor's native timing as read back out of an EDID.
///
/// `refresh_mhz` is the refresh rate in millihertz (60 Hz = 60_000), so a
/// non-integer rate such as 59.94 survives. All three numbers come from the
/// timing the EDID itself describes, never from a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeTiming {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// DisplayID 2.0 data block tag: Type VII Detailed Timing (3-byte pixel clock
/// in 1 kHz units, stored minus one).
const DID_TYPE_VII: u8 = 0x22;
/// DisplayID 1.3 data block tag: Type I Detailed Timing (3-byte pixel clock in
/// 10 kHz units, stored minus one).
const DID_TYPE_I: u8 = 0x03;
/// EDID extension tag that carries a DisplayID section.
const EXT_DISPLAYID: u8 = 0x70;
const DID_TIMING_LEN: usize = 20;

/// Extract the native (preferred) timing from an EDID of one base block plus
/// any extension blocks.
///
/// Order: a preferred DisplayID timing, then the first DisplayID timing, then
/// the base block's first detailed timing. A DisplayID timing wins over the
/// base block because the base block cannot hold modes above 4095 pixels or
/// 655.35 MHz, so a host that needs more puts a same-aspect placeholder there
/// and the real mode in the extension. Returns `None` for a block that is too
/// short, has a bad header, or describes no usable timing; it never panics.
pub fn native_timing(edid: &[u8]) -> Option<NativeTiming> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.get(..8)? != HEADER {
        return None;
    }
    let base = edid.get(..128)?;
    let ext_count = usize::from(*base.get(126)?);
    let mut first_displayid = None;
    for n in 1..=ext_count {
        let start = n.checked_mul(128)?;
        let Some(block) = edid.get(start..start.checked_add(128)?) else {
            break;
        };
        if block.first() != Some(&EXT_DISPLAYID) {
            continue;
        }
        let mut best = None;
        displayid_timings(block, |timing, preferred| {
            if preferred && best.is_none() {
                best = Some(timing);
            }
            if first_displayid.is_none() {
                first_displayid = Some(timing);
            }
        });
        if best.is_some() {
            return best;
        }
    }
    first_displayid.or_else(|| base_dtd(base))
}

/// Walk the DisplayID data blocks of one EDID extension block, calling `f` for
/// every timing in a Type VII or Type I block.
fn displayid_timings(block: &[u8], mut f: impl FnMut(NativeTiming, bool)) {
    // Byte 0 = 0x70, 1 = DisplayID revision, 2 = section payload size,
    // 3 = product use case, 4 = extension count, then the data blocks.
    let Some(section_len) = block.get(2).map(|b| usize::from(*b)) else {
        return;
    };
    let end = 5usize.saturating_add(section_len).min(block.len());
    let mut at = 5usize;
    while at.saturating_add(3) <= end {
        let (Some(&tag), Some(&len)) = (block.get(at), block.get(at + 2)) else {
            return;
        };
        let len = usize::from(len);
        let body_start = at + 3;
        let Some(body_end) = body_start.checked_add(len) else {
            return;
        };
        if body_end > end {
            return;
        }
        if tag == 0 {
            return; // padding: no more data blocks
        }
        let khz_per_unit = match tag {
            DID_TYPE_VII => 1u64,
            DID_TYPE_I => 10u64,
            _ => 0,
        };
        if khz_per_unit != 0 {
            let mut t = body_start;
            while t + DID_TIMING_LEN <= body_end {
                if let Some(entry) = block.get(t..t + DID_TIMING_LEN) {
                    if let Some(timing) = displayid_timing(entry, khz_per_unit) {
                        f(timing, entry.get(3).is_some_and(|b| b & 0x80 != 0));
                    }
                }
                t += DID_TIMING_LEN;
            }
        }
        at = body_end;
    }
}

/// One 20-byte DisplayID detailed timing (identical layout in Type I and VII;
/// only the pixel clock unit differs). Every size is stored minus one.
fn displayid_timing(entry: &[u8], khz_per_unit: u64) -> Option<NativeTiming> {
    let le16 = |at: usize| -> Option<u64> {
        Some(u64::from(u16::from_le_bytes([*entry.get(at)?, *entry.get(at + 1)?])))
    };
    let clock_units = u64::from(*entry.first()?)
        | (u64::from(*entry.get(1)?) << 8)
        | (u64::from(*entry.get(2)?) << 16);
    let clock_khz = (clock_units + 1) * khz_per_unit;
    let h_active = le16(4)? + 1;
    let h_blank = le16(6)? + 1;
    let v_active = le16(12)? + 1;
    let v_blank = le16(14)? + 1;
    timing_from(clock_khz, h_active, h_blank, v_active, v_blank)
}

/// The base block's first detailed timing descriptor (bytes 54..72).
fn base_dtd(base: &[u8]) -> Option<NativeTiming> {
    let d = base.get(54..72)?;
    let clock_10khz = u64::from(u16::from_le_bytes([*d.first()?, *d.get(1)?]));
    if clock_10khz == 0 {
        return None; // a display descriptor, not a timing
    }
    let (b2, b3, b4) = (*d.get(2)?, *d.get(3)?, *d.get(4)?);
    let (b5, b6, b7) = (*d.get(5)?, *d.get(6)?, *d.get(7)?);
    let h_active = u64::from(b2) | (u64::from(b4 >> 4) << 8);
    let h_blank = u64::from(b3) | (u64::from(b4 & 0xF) << 8);
    let v_active = u64::from(b5) | (u64::from(b7 >> 4) << 8);
    let v_blank = u64::from(b6) | (u64::from(b7 & 0xF) << 8);
    timing_from(clock_10khz * 10, h_active, h_blank, v_active, v_blank)
}

fn timing_from(
    clock_khz: u64,
    h_active: u64,
    h_blank: u64,
    v_active: u64,
    v_blank: u64,
) -> Option<NativeTiming> {
    let total = (h_active + h_blank).checked_mul(v_active + v_blank)?;
    if h_active == 0 || v_active == 0 || total == 0 {
        return None;
    }
    // mHz = pixels per second * 1000 / pixels per frame.
    let refresh_mhz = clock_khz.checked_mul(1_000_000)? / total;
    Some(NativeTiming {
        width: u32::try_from(h_active).ok()?,
        height: u32::try_from(v_active).ok()?,
        refresh_mhz: u32::try_from(refresh_mhz).ok()?,
    })
}

fn text_descriptor(bytes: &mut [u8], tag: u8, text: &str) {
    bytes[3] = tag;
    bytes[5..].fill(b' ');
    bytes[5..5 + text.len()].copy_from_slice(text.as_bytes());
    bytes[5 + text.len()] = b'\n';
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 128-byte DisplayID extension block with one Type VII (or Type I)
    /// timing, built the way a host would.
    fn displayid_ext(tag: u8, khz_per_unit: u64, w: u32, h: u32, hz: u32, preferred: bool) -> [u8; 128] {
        let hb = 80u32;
        let vb = 60u32;
        let clock_khz = u64::from(w + hb) * u64::from(h + vb) * u64::from(hz) / 1000;
        let units = clock_khz / khz_per_unit - 1;
        let mut e = [0u8; 128];
        e[0] = 0x70;
        e[1] = if tag == 0x22 { 0x20 } else { 0x13 };
        e[2] = 3 + 20; // section payload: one data block header + one timing
        let t = &mut e[5..28];
        t[0] = tag;
        t[2] = 20;
        let d = &mut t[3..];
        d[0] = units as u8;
        d[1] = (units >> 8) as u8;
        d[2] = (units >> 16) as u8;
        d[3] = if preferred { 0x80 } else { 0 };
        d[4..6].copy_from_slice(&((w - 1) as u16).to_le_bytes());
        d[6..8].copy_from_slice(&((hb - 1) as u16).to_le_bytes());
        d[8..10].copy_from_slice(&7u16.to_le_bytes());
        d[10..12].copy_from_slice(&31u16.to_le_bytes());
        d[12..14].copy_from_slice(&((h - 1) as u16).to_le_bytes());
        d[14..16].copy_from_slice(&((vb - 1) as u16).to_le_bytes());
        d[16..18].copy_from_slice(&3u16.to_le_bytes());
        d[18..20].copy_from_slice(&4u16.to_le_bytes());
        let sum: u32 = e[..127].iter().map(|&b| b as u32).sum();
        e[127] = ((256 - (sum % 256)) % 256) as u8;
        e
    }

    fn with_ext(mut base: [u8; 128], ext: [u8; 128]) -> [u8; 256] {
        base[126] = 1;
        let sum: u32 = base[..127].iter().map(|&b| b as u32).sum();
        base[127] = ((256 - (sum % 256)) % 256) as u8;
        let mut out = [0u8; 256];
        out[..128].copy_from_slice(&base);
        out[128..].copy_from_slice(&ext);
        out
    }

    #[test]
    fn base_block_timing_is_read_back() {
        let e = build_edid(1920, 1080, "Helios vGPU", "WinBoat", 2026).unwrap();
        let t = native_timing(&e).unwrap();
        assert_eq!((t.width, t.height), (1920, 1080));
        // build_edid targets 60 Hz; the 10 kHz clock step leaves it within 0.1 Hz.
        assert!(t.refresh_mhz.abs_diff(60_000) < 100, "{}", t.refresh_mhz);
    }

    #[test]
    fn displayid_type_vii_beats_the_base_placeholder() {
        let base = build_edid(1920, 1080, "Helios vGPU", "WinBoat", 2026).unwrap();
        for (w, h, hz) in [
            (5120, 1440, 240),
            (5120, 2560, 240),
            (7680, 4320, 120),
            (3840, 1080, 144),
        ] {
            let e = with_ext(base, displayid_ext(0x22, 1, w, h, hz, true));
            let t = native_timing(&e).unwrap();
            assert_eq!((t.width, t.height), (w, h));
            assert!(t.refresh_mhz.abs_diff(hz * 1000) <= 5, "{w}x{h}@{hz}: {}", t.refresh_mhz);
        }
    }

    /// The EDID the Conduit host generates for `--display 5120x1440@240`
    /// (host/backend/device/src/venus/testdata, checked there with
    /// `edid-decode --check`): a 2560x720 placeholder in the base block and the
    /// real mode in a DisplayID 2.0 Type VII timing.
    #[test]
    fn the_hosts_real_5120x1440_at_240_edid_is_read() {
        let edid = include_bytes!("testdata/edid-5120x1440-240.bin");
        assert_eq!(edid.len(), 256);
        // The placeholder alone would say 2560x720.
        let base_only = native_timing(&edid[..128]).unwrap();
        assert_eq!((base_only.width, base_only.height), (2560, 720));
        let t = native_timing(edid).unwrap();
        assert_eq!((t.width, t.height), (5120, 1440));
        assert!(t.refresh_mhz.abs_diff(240_000) <= 50, "{}", t.refresh_mhz);
    }

    #[test]
    fn displayid_type_i_is_accepted_too() {
        let base = build_edid(1920, 1080, "Helios vGPU", "WinBoat", 2026).unwrap();
        let e = with_ext(base, displayid_ext(0x03, 10, 5120, 1440, 120, false));
        let t = native_timing(&e).unwrap();
        assert_eq!((t.width, t.height), (5120, 1440));
        assert!(t.refresh_mhz.abs_diff(120_000) < 200);
    }

    #[test]
    fn garbage_never_panics_and_yields_nothing() {
        assert!(native_timing(&[]).is_none());
        assert!(native_timing(&[0u8; 128]).is_none());
        let base = build_edid(1920, 1080, "Helios vGPU", "WinBoat", 2026).unwrap();
        // Truncated base block, and an extension count that points past the buffer.
        assert!(native_timing(&base[..100]).is_none());
        let mut lying = base;
        lying[126] = 7;
        assert!(native_timing(&lying).is_some());
        // Every one-byte corruption of a DisplayID section: no panic.
        let good = with_ext(base, displayid_ext(0x22, 1, 5120, 1440, 240, true));
        for i in 128..256 {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut bad = good;
                bad[i] = v;
                let _ = native_timing(&bad);
            }
        }
    }

    #[test]
    fn mode_roundtrips_without_inventing_a_panel_or_frequency_range() {
        for (w, h) in [
            (320, 240),
            (1896, 1066),
            (1920, 1080),
            (3840, 2160),
            (1080, 1920),
        ] {
            let e = build_edid(w, h, "Helios vGPU", "WinBoat", 2026).unwrap();
            assert_eq!(e.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)), 0);
            assert_eq!(u32::from(e[56]) | (u32::from(e[58] >> 4) << 8), w);
            assert_eq!(u32::from(e[59]) | (u32::from(e[61] >> 4) << 8), h);
            assert_eq!(&e[12..16], &[0; 4]); // No fabricated serial number.
            assert_eq!(&e[16..20], &[255, 36, 1, 4]); // Model year 2026, EDID 1.4.
            assert!(e[21] == 0 || e[22] == 0); // Aspect ratio, never centimeters.
            assert_eq!(&e[77..85], b"WinBoat\n");
            assert_eq!(&e[95..107], b"Helios vGPU\n");
            assert_eq!(e[75], 0xFE); // Publisher text replaces the false ranges.
            let ht = w + (((w / 4) & !7).max(160));
            let vt = h + 45;
            let clock = u32::from(u16::from_le_bytes([e[54], e[55]])) * 10_000;
            assert!(clock <= ht * vt * 60 && ht * vt * 60 - clock < 10_000);
        }
    }

    #[test]
    fn invalid_extents_and_identity_are_refused() {
        for (w, h) in [
            (0, 1080),
            (1920, 0),
            (4096, 2160),
            (4095, 4095),
            (u32::MAX, u32::MAX),
        ] {
            assert!(build_edid(w, h, "Helios vGPU", "WinBoat", 2026).is_none());
        }
        for name in ["", "Too long a product", "Bad\nName", "Non-ASCII é"] {
            assert!(build_edid(1920, 1080, name, "WinBoat", 2026).is_none());
        }
        assert!(build_edid(1920, 1080, "Helios vGPU", "Publisher too long", 2026).is_none());
        assert!(build_edid(1920, 1080, "Helios vGPU", "WinBoat", 1989).is_none());
        assert!(build_edid(1920, 1080, "Helios vGPU", "WinBoat", 2246).is_none());
    }
}
