//! Display modes ("2560x1440@240") and finding the host monitor's mode.

use crate::hypr;
use crate::ui::oops;
use anyhow::Result;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

pub const FALLBACK: Mode = Mode { width: 2560, height: 1440, hz: 60 };

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.hz)
    }
}

impl Mode {
    pub fn size(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

fn bad(s: &str) -> anyhow::Error {
    oops(
        format!("\"{s}\" is not a display mode"),
        "Write it as WIDTHxHEIGHT or WIDTHxHEIGHT@HZ, for example 1920x1080 or 2560x1440@144",
    )
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    /// "WxH" (60 Hz), "WxH@HZ", "WxH@59.94" (rounded). Also accepts "X" or "×".
    fn from_str(s: &str) -> Result<Self> {
        let t = s.trim().to_ascii_lowercase().replace('×', "x");
        let (size, hz) = match t.split_once('@') {
            Some((a, b)) => (a, Some(b.trim_end_matches("hz"))),
            None => (t.as_str(), None),
        };
        let (w, h) = size.split_once('x').ok_or_else(|| bad(s))?;
        let width: u32 = w.trim().parse().map_err(|_| bad(s))?;
        let height: u32 = h.trim().parse().map_err(|_| bad(s))?;
        let hz = match hz {
            None => 60,
            Some(x) => {
                let f: f64 = x.trim().parse().map_err(|_| bad(s))?;
                if !f.is_finite() {
                    return Err(bad(s));
                }
                f.round() as u32
            }
        };
        if !(320..=8192).contains(&width) || !(200..=8192).contains(&height) {
            return Err(oops(
                format!("{width}x{height} is not a usable screen size"),
                "Use a size between 320x200 and 8192x8192",
            ));
        }
        if !(1..=1000).contains(&hz) {
            return Err(oops(format!("{hz} Hz is not a usable refresh rate"), "Use a rate between 1 and 1000"));
        }
        Ok(Mode { width, height, hz })
    }
}

/// The focused (else first) monitor from `hyprctl monitors -j`.
pub fn from_hyprctl_json(json: &str) -> Option<Mode> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let ms = v.as_array()?;
    let m = ms
        .iter()
        .find(|m| m.get("focused").and_then(|f| f.as_bool()) == Some(true))
        .or_else(|| ms.first())?;
    Some(Mode {
        width: m.get("width")?.as_u64()? as u32,
        height: m.get("height")?.as_u64()? as u32,
        hz: m.get("refreshRate")?.as_f64()?.round() as u32,
    })
}

/// The first "current" mode in `wlr-randr` output, e.g.
/// "    2560x1440 px, 239.970001 Hz (preferred, current)".
pub fn from_wlr_randr(text: &str) -> Option<Mode> {
    for line in text.lines() {
        if !line.contains("current") {
            continue;
        }
        let mut it = line.split_whitespace();
        let size = it.next()?;
        let (w, h) = size.split_once('x')?;
        let hz = line
            .split(',')
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok()?;
        return Some(Mode { width: w.parse().ok()?, height: h.parse().ok()?, hz: hz.round() as u32 });
    }
    None
}

/// Preferred size of a connected monitor from /sys/class/drm (no refresh: 60).
fn from_sysfs() -> Option<Mode> {
    let rd = std::fs::read_dir("/sys/class/drm").ok()?;
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
        if status.trim() != "connected" {
            continue;
        }
        let modes = std::fs::read_to_string(p.join("modes")).unwrap_or_default();
        if let Some(first) = modes.lines().next() {
            if let Ok(m) = first.parse::<Mode>() {
                return Some(m);
            }
        }
    }
    None
}

/// The host monitor's mode: Hyprland, else wlr-randr, else the kernel's
/// preferred size at 60 Hz, else 2560x1440@60. Returns where it came from too.
pub fn detect() -> (Mode, &'static str) {
    if let Some(sig) = hypr::instance() {
        if let Ok(out) = hypr::hyprctl(&sig, &["monitors", "-j"]) {
            if let Some(m) = from_hyprctl_json(&out) {
                return (m, "Hyprland");
            }
        }
    }
    if crate::sys::have("wlr-randr") {
        if let Ok(out) = crate::sys::output("wlr-randr", &[]) {
            if let Some(m) = from_wlr_randr(&out) {
                return (m, "wlr-randr");
            }
        }
    }
    if let Some(m) = from_sysfs() {
        return (m, "monitor's preferred size");
    }
    (FALLBACK, "default")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!("2560x1440@240".parse::<Mode>().unwrap(), Mode { width: 2560, height: 1440, hz: 240 });
        assert_eq!("1920x1080".parse::<Mode>().unwrap(), Mode { width: 1920, height: 1080, hz: 60 });
        assert_eq!("1920X1080@59.94".parse::<Mode>().unwrap().hz, 60);
        assert_eq!("3840x2160@143.9Hz".parse::<Mode>().unwrap().hz, 144);
        assert_eq!(" 1280×720@75 ".parse::<Mode>().unwrap(), Mode { width: 1280, height: 720, hz: 75 });
        for bad in ["", "1920", "x1080", "1920x", "axb", "1920x1080@", "1920x1080@fast", "10x10", "1920x1080@0", "99999x1080"] {
            assert!(bad.parse::<Mode>().is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn display_roundtrip() {
        let m: Mode = "2560x1440@240".parse().unwrap();
        assert_eq!(m.to_string(), "2560x1440@240");
        assert_eq!(m.size(), "2560x1440");
    }

    #[test]
    fn hyprctl_focused_monitor_wins() {
        let j = r#"[
          {"id":0,"name":"DP-1","width":1920,"height":1080,"refreshRate":60.00,"focused":false},
          {"id":1,"name":"DP-2","width":2560,"height":1440,"refreshRate":239.97,"focused":true}
        ]"#;
        assert_eq!(from_hyprctl_json(j), Some(Mode { width: 2560, height: 1440, hz: 240 }));
        let one = r#"[{"width":3840,"height":2160,"refreshRate":143.856,"focused":false}]"#;
        assert_eq!(from_hyprctl_json(one), Some(Mode { width: 3840, height: 2160, hz: 144 }));
        assert_eq!(from_hyprctl_json("[]"), None);
        assert_eq!(from_hyprctl_json("ok"), None);
    }

    #[test]
    fn wlr_randr_current() {
        let t = "DP-1 \"Some Monitor\"\n  Enabled: yes\n  Modes:\n    3840x2160 px, 60.000000 Hz (preferred)\n    2560x1440 px, 164.958000 Hz (current)\n    1920x1080 px, 60.000000 Hz\n";
        assert_eq!(from_wlr_randr(t), Some(Mode { width: 2560, height: 1440, hz: 165 }));
        assert_eq!(from_wlr_randr("nothing here"), None);
    }
}
