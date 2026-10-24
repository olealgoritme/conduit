//! The tray's "Upscaling and frame generation" menu: the choices and the
//! registry values they write, read by NVK's composed present
//! (guest/nvk-rm/docs/enhance.md).
//!
//! Global settings are `HKLM\SOFTWARE\Helios` `Enhance*`; a per-exe profile
//! is `HKLM\SOFTWARE\Helios\Enhance\<exe>` with the same names without the
//! prefix (`Scaler`, `FrameGen`, ...). Pure logic: the Win32 side is in
//! `sys.rs` and `app.rs`.

/// One setting: its value name (global; the profile name drops `Enhance`),
/// the registry type, and the choices (value, menu text).
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub name: &'static str,
    pub dword: bool,
    pub title: &'static str,
    pub choices: &'static [(&'static str, &'static str)],
    /// The value NVK uses when there is none
    pub default: &'static str,
    /// Also in per-exe profiles
    pub per_exe: bool,
}

pub const SCALER: Setting = Setting {
    name: "EnhanceScaler",
    dword: false,
    title: "Upscaling",
    choices: &[
        ("off", "Off"),
        ("fsr", "FSR 1 (edge-adaptive, sharpened)"),
        ("bicubic", "Bicubic"),
        ("bilinear", "Bilinear"),
        ("nearest", "Nearest"),
        ("integer", "Integer (sharp pixels)"),
    ],
    default: "off",
    per_exe: true,
};

pub const SHARPNESS: Setting = Setting {
    name: "EnhanceSharpness",
    dword: true,
    title: "Sharpness",
    choices: &[
        ("0", "0 (none)"),
        ("25", "25"),
        ("50", "50"),
        ("75", "75"),
        ("100", "100"),
    ],
    default: "50",
    per_exe: true,
};

pub const FRAMEGEN: Setting = Setting {
    name: "EnhanceFrameGen",
    dword: false,
    title: "Frame generation",
    choices: &[
        ("off", "Off"),
        ("2x", "2x"),
        ("3x", "3x"),
        ("4x", "4x"),
        ("adaptive", "Adaptive (fill to the refresh rate)"),
    ],
    default: "off",
    per_exe: true,
};

pub const FG_QUALITY: Setting = Setting {
    name: "EnhanceFgQuality",
    dword: false,
    title: "Motion estimation",
    choices: &[
        ("fast", "Fast (weaker GPUs)"),
        ("quality", "Quality (stronger GPUs)"),
    ],
    default: "fast",
    per_exe: true,
};

pub const LOW_LATENCY: Setting = Setting {
    name: "EnhanceLowLatency",
    dword: true,
    title: "Latency",
    choices: &[("0", "Even pacing"), ("1", "Lower latency (uneven pacing)")],
    default: "0",
    per_exe: true,
};

pub const FIT: Setting = Setting {
    name: "EnhanceFit",
    dword: false,
    title: "Picture",
    choices: &[
        ("aspect", "Keep aspect ratio"),
        ("stretch", "Stretch to the window"),
    ],
    default: "aspect",
    per_exe: true,
};

pub const WINDOWS: Setting = Setting {
    name: "EnhanceWindows",
    dword: false,
    title: "Applies to",
    choices: &[
        ("fullscreen", "Fullscreen and borderless windows"),
        ("all", "All windows"),
    ],
    default: "fullscreen",
    per_exe: false,
};

pub const D3D: Setting = Setting {
    name: "EnhanceD3D",
    dword: true,
    title: "D3D11 and D3D12 games",
    choices: &[("1", "On"), ("0", "Off (Vulkan and OpenGL only)")],
    default: "1",
    per_exe: true,
};

/// Menu order
pub const ALL: [Setting; 8] = [
    SCALER,
    SHARPNESS,
    FRAMEGEN,
    FG_QUALITY,
    LOW_LATENCY,
    FIT,
    WINDOWS,
    D3D,
];

impl Setting {
    /// The value name in a per-exe profile
    pub fn profile_name(&self) -> &'static str {
        self.name.strip_prefix("Enhance").unwrap_or(self.name)
    }

    /// The choice index a stored value selects (NVK's default when absent
    /// or unknown, as NVK reads it)
    pub fn index_of(&self, stored: Option<&str>) -> usize {
        let pick = |v: &str| {
            self.choices
                .iter()
                .position(|(c, _)| c.eq_ignore_ascii_case(v.trim()))
        };
        if let Some(v) = stored {
            if let Some(i) = pick(v) {
                return i;
            }
            // NVK also takes fsr1 for fsr
            if self.name == SCALER.name && v.trim().eq_ignore_ascii_case("fsr1") {
                return 1;
            }
            // DWORD settings: the nearest choice
            if self.dword {
                if let Ok(n) = v.trim().parse::<i64>() {
                    return self
                        .choices
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, (c, _))| (c.parse::<i64>().unwrap_or(0) - n).abs())
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
        }
        pick(self.default).unwrap_or(0)
    }
}

/// The profile key of an executable, `None` for a name that is not a plain
/// file name ending in `.exe`.
pub fn profile_key(exe: &str) -> Option<String> {
    let e = exe.trim();
    let ok = e.len() > 4
        && e.len() < 200
        && e.to_ascii_lowercase().ends_with(".exe")
        && !e.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
        && e != "."
        && e != "..";
    ok.then(|| format!(r"SOFTWARE\Helios\Enhance\{e}"))
}

/// Executables worth a profile entry: the processes with visible windows,
/// without the shell and Conduit's own, each once (case-insensitive), in
/// the order given, at most `max`.
pub fn profile_candidates(exes: &[String], max: usize) -> Vec<String> {
    const SKIP: &[&str] = &[
        "explorer.exe",
        "conduit-gpu-tray.exe",
        "searchhost.exe",
        "startmenuexperiencehost.exe",
        "shellexperiencehost.exe",
        "textinputhost.exe",
        "applicationframehost.exe",
        "systemsettings.exe",
        "lockapp.exe",
        "dwm.exe",
    ];
    let mut out: Vec<String> = Vec::new();
    for e in exes {
        let l = e.to_ascii_lowercase();
        if SKIP.contains(&l.as_str()) || profile_key(e).is_none() {
            continue;
        }
        if out.iter().any(|o| o.eq_ignore_ascii_case(e)) {
            continue;
        }
        out.push(e.clone());
        if out.len() == max {
            break;
        }
    }
    out
}

/// A one-line summary for the menu ("FSR 1, 2x" / "Off")
pub fn summary(scaler: usize, framegen: usize) -> String {
    let s = if scaler == 0 {
        None
    } else {
        Some(SCALER.choices[scaler].1.split(" (").next().unwrap_or(""))
    };
    let f = if framegen == 0 {
        None
    } else {
        Some(
            FRAMEGEN.choices[framegen]
                .1
                .split(" (")
                .next()
                .unwrap_or(""),
        )
    };
    match (s, f) {
        (None, None) => "Off".to_string(),
        (Some(s), None) => s.to_string(),
        (None, Some(f)) => format!("frame generation {f}"),
        (Some(s), Some(f)) => format!("{s}, frame generation {f}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_follows_nvk() {
        assert_eq!(SCALER.index_of(None), 0);
        assert_eq!(SCALER.index_of(Some("FSR")), 1);
        assert_eq!(SCALER.index_of(Some("fsr1")), 1);
        assert_eq!(SCALER.index_of(Some("integer")), 5);
        assert_eq!(SCALER.index_of(Some("nonsense")), 0);
        assert_eq!(FRAMEGEN.index_of(Some("adaptive")), 4);
        assert_eq!(SHARPNESS.index_of(None), 2);
        assert_eq!(SHARPNESS.index_of(Some("60")), 2);
        assert_eq!(SHARPNESS.index_of(Some("90")), 4);
        assert_eq!(FG_QUALITY.index_of(None), 0);
        assert_eq!(WINDOWS.index_of(Some("ALL")), 1);
    }

    #[test]
    fn profile_names_drop_the_prefix() {
        assert_eq!(SCALER.profile_name(), "Scaler");
        assert_eq!(FRAMEGEN.profile_name(), "FrameGen");
        assert_eq!(FG_QUALITY.profile_name(), "FgQuality");
        assert!(!WINDOWS.per_exe);
        assert_eq!(D3D.profile_name(), "D3D");
        assert_eq!(D3D.index_of(None), 0);
        assert_eq!(D3D.index_of(Some("0")), 1);
    }

    #[test]
    fn profile_keys() {
        assert_eq!(
            profile_key("cs2.exe").as_deref(),
            Some(r"SOFTWARE\Helios\Enhance\cs2.exe")
        );
        assert_eq!(profile_key("..\\x.exe"), None);
        assert_eq!(profile_key("game"), None);
        assert_eq!(profile_key("a/b.exe"), None);
        assert_eq!(profile_key(".exe"), None);
    }

    #[test]
    fn candidates_skip_the_shell_and_duplicates() {
        let v: Vec<String> = [
            "explorer.exe",
            "Game.exe",
            "game.exe",
            "x",
            "conduit-gpu-tray.exe",
            "b.exe",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(profile_candidates(&v, 8), vec!["Game.exe", "b.exe"]);
        assert_eq!(profile_candidates(&v, 1), vec!["Game.exe"]);
    }

    #[test]
    fn summaries() {
        assert_eq!(summary(0, 0), "Off");
        assert_eq!(summary(1, 0), "FSR 1");
        assert_eq!(summary(0, 1), "frame generation 2x");
        assert_eq!(summary(1, 4), "FSR 1, frame generation Adaptive");
    }
}
