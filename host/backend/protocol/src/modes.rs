//! The display's mode list: what the virtual monitor offers, like a physical
//! monitor's EDID does (docs/SCANOUT.md "Mode list").
//!
//! One list per VM, built from three parts:
//!
//! * the native mode -- the backend's `--display` size, the guest's boot
//!   mode, first in the list;
//! * the standard modes no larger than native (both dimensions), so a
//!   game's fullscreen menu offers the usual choices;
//! * the VM's custom modes, any size within [`MODE_MIN`]..=[`MODE_MAX`]
//!   (`conduit display VM --add WxH`, or the viewer's menu), stored one
//!   `WxH` per line in the VM's `display-modes` file.
//!
//! Every mode runs at the native refresh rate. The guest gets the list as a
//! [`crate::messages::MsgType::DisplayModeList`] event, a display client
//! (viewer) as `CMD_MODES` records; both always describe the same list.
//!
//! The CLI, the backend and the tests share this one implementation.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The standard modes offered up to native, largest area last.
pub const STANDARD_MODES: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1024, 768),
    (1280, 720),
    (1280, 800),
    (1280, 960),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1440, 1080),
    (1600, 900),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (2560, 1600),
    (3440, 1440),
    (3840, 1080),
    (3840, 1600),
    (3840, 2160),
    (5120, 1440),
    (5120, 2160),
    (5120, 2880),
];

/// The smallest and largest dimension a mode may have (the backend's own
/// clamp for mode hints).
pub const MODE_MIN: u32 = 64;
pub const MODE_MAX: u32 = 8192;

/// At most this many modes in one list: what fits one guest event buffer
/// (`16 + 16 + 8 * 48` bytes) with room to spare.
pub const MODE_LIST_MAX: usize = 48;

/// At most this many custom modes are kept per VM.
pub const CUSTOM_MAX: usize = 16;

/// Why a custom mode was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeError {
    /// Not `WxH` with two positive numbers.
    Syntax,
    /// A dimension outside [`MODE_MIN`]..=[`MODE_MAX`].
    Range,
    /// Already [`CUSTOM_MAX`] custom modes.
    Full,
}

impl fmt::Display for ModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeError::Syntax => write!(f, "expected WIDTHxHEIGHT, e.g. 1920x1080"),
            ModeError::Range => write!(
                f,
                "width and height must be between {MODE_MIN} and {MODE_MAX}"
            ),
            ModeError::Full => write!(f, "at most {CUSTOM_MAX} custom modes per VM"),
        }
    }
}

impl core::error::Error for ModeError {}

/// `WxH` (also `W*H` or `W X H`, surrounding blanks ignored), checked against
/// the mode range. A refresh suffix (`@HZ`) is not accepted: every mode runs
/// at the native rate.
pub fn parse_wxh(s: &str) -> Result<(u32, u32), ModeError> {
    let s = s.trim();
    let (a, b) = s
        .split_once(['x', 'X', '*'])
        .ok_or(ModeError::Syntax)?;
    let num = |t: &str| -> Result<u32, ModeError> {
        let t = t.trim();
        if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(ModeError::Syntax);
        }
        t.parse::<u32>().map_err(|_| ModeError::Range)
    };
    let (w, h) = (num(a)?, num(b)?);
    if w == 0 || h == 0 {
        return Err(ModeError::Syntax);
    }
    if !(MODE_MIN..=MODE_MAX).contains(&w) || !(MODE_MIN..=MODE_MAX).contains(&h) {
        return Err(ModeError::Range);
    }
    Ok((w, h))
}

/// The custom modes in a `display-modes` file: one `WxH` per line, `#`
/// starts a comment. Lines that do not parse are skipped (a hand edit must
/// not cost the VM its display); duplicates are dropped; at most
/// [`CUSTOM_MAX`] are kept, in file order.
pub fn parse_custom(text: &str) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(m) = parse_wxh(line)
            && !out.contains(&m)
        {
            out.push(m);
            if out.len() == CUSTOM_MAX {
                break;
            }
        }
    }
    out
}

/// The `display-modes` file for these custom modes.
pub fn format_custom(modes: &[(u32, u32)]) -> String {
    let mut s = String::from(
        "# Custom display modes for this VM, one WIDTHxHEIGHT per line.\n\
         # Edited by `conduit display VM --add/--rm` and the viewer's menu.\n",
    );
    for (w, h) in modes {
        s.push_str(&format!("{w}x{h}\n"));
    }
    s
}

/// `modes` with `m` added (at the end; no change if present).
pub fn add_custom(modes: &[(u32, u32)], m: (u32, u32)) -> Result<Vec<(u32, u32)>, ModeError> {
    parse_wxh(&format!("{}x{}", m.0, m.1))?;
    let mut v = modes.to_vec();
    if !v.contains(&m) {
        if v.len() >= CUSTOM_MAX {
            return Err(ModeError::Full);
        }
        v.push(m);
    }
    Ok(v)
}

/// `modes` without `m`; `None` if it was not there.
pub fn remove_custom(modes: &[(u32, u32)], m: (u32, u32)) -> Option<Vec<(u32, u32)>> {
    if !modes.contains(&m) {
        return None;
    }
    Some(modes.iter().copied().filter(|x| *x != m).collect())
}

/// Where a mode in the list comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeEntry {
    pub width: u32,
    pub height: u32,
    /// The native mode (always entry 0).
    pub native: bool,
    /// One of the VM's custom modes (a custom mode may also be native or
    /// standard; it is still removable).
    pub custom: bool,
}

/// The whole list: native first, then every other mode by area, largest
/// first (ties: wider first). Standard modes are those that fit within native
/// in both dimensions; custom modes are kept whatever their size. At most
/// [`MODE_LIST_MAX`]: custom modes are never the ones dropped.
pub fn mode_list(native: (u32, u32), custom: &[(u32, u32)]) -> Vec<ModeEntry> {
    let (nw, nh) = native;
    let mut rest: Vec<ModeEntry> = Vec::new();
    for &(w, h) in custom {
        if (w, h) != native && !rest.iter().any(|e| (e.width, e.height) == (w, h)) {
            rest.push(ModeEntry {
                width: w,
                height: h,
                native: false,
                custom: true,
            });
        }
    }
    let n_custom = rest.len();
    for &(w, h) in STANDARD_MODES {
        if w <= nw && h <= nh && (w, h) != native && !rest.iter().any(|e| (e.width, e.height) == (w, h)) {
            rest.push(ModeEntry {
                width: w,
                height: h,
                native: false,
                custom: false,
            });
        }
    }
    // Drop the smallest standard modes first when over the cap.
    let room = MODE_LIST_MAX - 1;
    if rest.len() > room {
        let mut standard: Vec<ModeEntry> = rest.split_off(n_custom);
        standard.sort_by_key(|e| core::cmp::Reverse(u64::from(e.width) * u64::from(e.height)));
        standard.truncate(room.saturating_sub(rest.len()));
        rest.extend(standard);
        rest.truncate(room);
    }
    rest.sort_by(|a, b| {
        let area = |e: &ModeEntry| u64::from(e.width) * u64::from(e.height);
        area(b).cmp(&area(a)).then(b.width.cmp(&a.width))
    });
    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push(ModeEntry {
        width: nw,
        height: nh,
        native: true,
        custom: custom.contains(&native),
    });
    out.extend(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sizes(v: &[ModeEntry]) -> Vec<(u32, u32)> {
        v.iter().map(|e| (e.width, e.height)).collect()
    }

    #[test]
    fn wxh_parses_and_nonsense_does_not() {
        assert_eq!(parse_wxh("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_wxh(" 1280 X 960 "), Ok((1280, 960)));
        assert_eq!(parse_wxh("800*600"), Ok((800, 600)));
        assert_eq!(parse_wxh("1920"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("1920x"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("x1080"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("-1x5"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("1920x1080@60"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("0x0"), Err(ModeError::Syntax));
        assert_eq!(parse_wxh("32x32"), Err(ModeError::Range));
        assert_eq!(parse_wxh("9000x1000"), Err(ModeError::Range));
        assert_eq!(parse_wxh("99999999999x1"), Err(ModeError::Range));
        assert_eq!(parse_wxh("64x8192"), Ok((64, 8192)));
    }

    #[test]
    fn the_file_round_trips_and_tolerates_hand_edits() {
        let v = vec![(1280, 960), (2000, 1000)];
        assert_eq!(parse_custom(&format_custom(&v)), v);
        let text = "# mine\n1280x960  # 4:3 for CS\n\ngarbage\n1280x960\n10x10\n 1600x1200\n";
        assert_eq!(parse_custom(text), vec![(1280, 960), (1600, 1200)]);
        let many: String = (0..40).map(|i| format!("{}x600\n", 800 + i)).collect();
        assert_eq!(parse_custom(&many).len(), CUSTOM_MAX);
        assert!(parse_custom("").is_empty());
    }

    #[test]
    fn add_and_remove() {
        let v = add_custom(&[], (1280, 960)).unwrap();
        assert_eq!(v, vec![(1280, 960)]);
        assert_eq!(add_custom(&v, (1280, 960)).unwrap(), v);
        assert_eq!(add_custom(&v, (10, 10)), Err(ModeError::Range));
        let full: Vec<(u32, u32)> = (0..CUSTOM_MAX as u32).map(|i| (800 + i, 600)).collect();
        assert_eq!(add_custom(&full, (1024, 700)), Err(ModeError::Full));
        assert_eq!(add_custom(&full, (800, 600)).unwrap(), full);
        assert_eq!(remove_custom(&v, (1280, 960)), Some(vec![]));
        assert_eq!(remove_custom(&v, (1, 1)), None);
    }

    #[test]
    fn native_first_then_standard_modes_that_fit_by_area() {
        let l = mode_list((1920, 1080), &[]);
        assert_eq!(l[0], ModeEntry { width: 1920, height: 1080, native: true, custom: false });
        assert!(l[1..].iter().all(|e| !e.native && !e.custom));
        assert!(l.iter().all(|e| e.width <= 1920 && e.height <= 1080));
        // 1920x1200 and 1600x1200 do not fit in 1080 lines.
        assert!(!sizes(&l).contains(&(1920, 1200)));
        assert!(!sizes(&l).contains(&(1600, 1200)));
        assert!(sizes(&l).contains(&(1440, 1080)));
        assert_eq!(*sizes(&l).last().unwrap(), (640, 480));
        for w in l[1..].windows(2) {
            assert!(w[0].width * w[0].height >= w[1].width * w[1].height);
        }
        // No duplicates.
        let mut s = sizes(&l);
        s.sort();
        s.dedup();
        assert_eq!(s.len(), l.len());
    }

    #[test]
    fn an_ultrawide_native_offers_the_ultrawide_and_16_9_modes() {
        let l = sizes(&mode_list((5120, 1440), &[]));
        assert_eq!(l[0], (5120, 1440));
        for m in [(3840, 1080), (3440, 1440), (2560, 1440), (1920, 1080), (1280, 960), (640, 480)] {
            assert!(l.contains(&m), "{m:?}");
        }
        assert!(!l.contains(&(3840, 2160)));
    }

    #[test]
    fn custom_modes_join_whatever_their_size() {
        let l = mode_list((1920, 1080), &[(1280, 960), (2560, 1440), (1920, 1080), (1024, 768)]);
        let s = sizes(&l);
        assert_eq!(s[0], (1920, 1080));
        assert!(l[0].custom, "native also listed as custom stays removable");
        let at = |m| l.iter().find(|e| (e.width, e.height) == m).unwrap();
        assert!(at((2560, 1440)).custom);
        assert!(at((1280, 960)).custom);
        // A custom mode that is also standard is listed once, as custom.
        assert!(at((1024, 768)).custom);
        assert_eq!(s.iter().filter(|m| **m == (1024, 768)).count(), 1);
        // Larger than native: still after native, first among the rest.
        assert_eq!(s[1], (2560, 1440));
    }

    #[test]
    fn the_list_is_capped_and_custom_modes_survive_the_cap() {
        let custom: Vec<(u32, u32)> = (0..CUSTOM_MAX as u32).map(|i| (700 + i, 500)).collect();
        let l = mode_list((8192, 8192), &custom);
        assert!(l.len() <= MODE_LIST_MAX);
        for m in &custom {
            assert!(sizes(&l).contains(m), "{m:?}");
        }
        assert!(l[0].native);
    }
}
