//! OptiScaler per game from the tray ("Profile for <exe>" > OptiScaler),
//! independent of Windows.
//!
//! The tray does not install anything itself: it runs
//! `Install-OptiScaler.ps1` (next to the tray exe, guest/windows/packaging/windows)
//! elevated, which downloads the pinned OptiScaler release, checks its hash
//! and copies it next to the game. This module holds what the menu needs to
//! know beforehand: the anti-cheat list (the same manifest the script reads,
//! built in), what an install left in the game folder, what its
//! OptiScaler.ini selects, and the queued picks for a game that is running.

use crate::upscaler;

/// The manifest the script reads: the pinned release and the anti-cheat list.
pub const MANIFEST: &str = include_str!("../../../packaging/windows/optiscaler-manifest.txt");
/// The install script, next to the tray exe.
pub const SCRIPT: &str = "Install-OptiScaler.ps1";
/// What the script leaves in the game folder: the files it copied.
pub const MARKER: &str = "conduit-optiscaler.txt";

/// Script exit codes (see the script's help).
pub const EXIT_ANTICHEAT: i32 = 2;
pub const EXIT_RUNNING: i32 = 3;
pub const EXIT_NOT_INSTALLED: i32 = 4;

/// The per-game upscaler choices: (mode, menu text). The modes are
/// [`conduit_ctl::UPSCALER_MODES`] words, written by the script's
/// `-Upscaler` with the same values as [`upscaler::optiscaler_values`].
pub const UPSCALERS: [(&str, &str); 4] = [
    ("dlss", "DLSS (the guest's NGX setup)"),
    ("fsr", "FSR 3.1"),
    ("xess", "XeSS"),
    ("default", "OptiScaler's default"),
];

/// A manifest value (`release.version` ...).
pub fn manifest_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next() == Some(key)).then(|| it.next().map(str::to_string))?
    })
}

/// The pinned OptiScaler version.
pub fn pinned_version() -> String {
    manifest_value(MANIFEST, "release.version").unwrap_or_default()
}

/// The games the script refuses (anticheat.* lines).
#[derive(Debug, Default, Clone)]
pub struct AntiCheat {
    /// (exe name pattern, label)
    pub exe: Vec<(String, String)>,
    /// (file or folder name pattern, label)
    pub paths: Vec<(String, String)>,
    /// Library folder names the folder search stops below
    pub roots: Vec<String>,
}

impl AntiCheat {
    pub fn parse(text: &str) -> AntiCheat {
        let mut a = AntiCheat::default();
        for l in text.lines() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let Some((key, rest)) = l.split_once(char::is_whitespace) else {
                continue;
            };
            let rest = rest.trim();
            let (value, label) = match rest.split_once(char::is_whitespace) {
                Some((v, lab)) => (v.to_string(), lab.trim().to_string()),
                None => (rest.to_string(), rest.to_string()),
            };
            match key {
                "anticheat.exe" => a.exe.push((value, label)),
                "anticheat.path" => a.paths.push((value, label)),
                "anticheat.root" => a.roots.push(rest.to_string()),
                _ => {}
            }
        }
        a
    }

    /// The list built into the tray.
    pub fn builtin() -> AntiCheat {
        AntiCheat::parse(MANIFEST)
    }

    /// The folders searched for anti-cheat files: `dir` and up to four
    /// above it, never a drive root, stopping below a library folder.
    pub fn search_dirs(&self, dir: &str) -> Vec<String> {
        let dir = dir.trim_end_matches('\\');
        let mut out = vec![dir.to_string()];
        let mut d = dir;
        for _ in 0..4 {
            let Some((parent, _)) = d.rsplit_once('\\') else {
                break;
            };
            // "G:" is the drive root: it and everything above stay out
            if !parent.contains('\\') {
                break;
            }
            let name = parent.rsplit('\\').next().unwrap_or("");
            if self.roots.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                break;
            }
            out.push(parent.to_string());
            d = parent;
        }
        out
    }

    /// The anti-cheat a game has ("label (what matched)"), or `None`.
    /// `list` gives the file and folder names in a folder.
    pub fn find(
        &self,
        dir: &str,
        exe: Option<&str>,
        list: impl Fn(&str) -> Vec<String>,
    ) -> Option<String> {
        let mut exes: Vec<String> = exe.map(str::to_string).into_iter().collect();
        exes.extend(
            list(dir)
                .into_iter()
                .filter(|n| n.to_ascii_lowercase().ends_with(".exe")),
        );
        for e in &exes {
            if let Some((_, label)) = self.exe.iter().find(|(p, _)| name_matches(e, p)) {
                return Some(format!("{label} ({e})"));
            }
        }
        for folder in self.search_dirs(dir) {
            for n in list(&folder) {
                if let Some((_, label)) = self.paths.iter().find(|(p, _)| name_matches(&n, p)) {
                    return Some(format!("{label} ({folder}\\{n})"));
                }
            }
        }
        None
    }
}

/// A name against a pattern: case-insensitive, `*` at the end matches any rest.
pub fn name_matches(name: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name
            .get(..prefix.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(prefix)),
        None => name.eq_ignore_ascii_case(pattern),
    }
}

/// What an install recorded in [`MARKER`].
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Marker {
    pub version: Option<String>,
    pub proxy: Option<String>,
    pub exe: Option<String>,
}

pub fn parse_marker(text: &str) -> Marker {
    let mut m = Marker::default();
    for l in text.lines() {
        let Some(rest) = l.strip_prefix("# ") else {
            continue;
        };
        let (k, v) = rest.split_once(' ').unwrap_or((rest, ""));
        let v = Some(v.trim().to_string()).filter(|v| !v.is_empty());
        match k {
            "version" => m.version = v,
            "proxy" => m.proxy = v,
            "exe" => m.exe = v,
            _ => {}
        }
    }
    m
}

/// A key's value in an OptiScaler.ini section.
pub fn ini_value<'a>(ini: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut inside = false;
    for line in ini.lines() {
        if let Some(s) = upscaler::section_of(line) {
            inside = s.eq_ignore_ascii_case(section);
            continue;
        }
        if inside && upscaler::key_of(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            return line.split_once('=').map(|(_, v)| v.trim());
        }
    }
    None
}

/// The upscaler mode an OptiScaler.ini is set to, when it is exactly one of
/// [`UPSCALERS`] (a hand-edited one is `None`).
pub fn ini_mode(ini: &str) -> Option<&'static str> {
    UPSCALERS.iter().map(|(m, _)| *m).find(|m| {
        upscaler::optiscaler_values(m)
            .iter()
            .all(|(s, k, v)| ini_value(ini, s, k).is_some_and(|x| x.eq_ignore_ascii_case(v)))
    })
}

/// Is OptiScaler's frame generation on in this OptiScaler.ini?
pub fn ini_framegen(ini: &str) -> bool {
    ini_value(ini, "FrameGen", "Enabled").is_some_and(|v| v.eq_ignore_ascii_case("true"))
        && ini_value(ini, "FrameGen", "FGOutput")
            .is_some_and(|v| !v.is_empty() && !v.eq_ignore_ascii_case("nofg") && v != "auto")
}

/// A menu pick for one game.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Install,
    Remove,
    Upscaler(&'static str),
    FrameGen(bool),
}

/// The picks for one game not yet run (the game is running, or the script
/// is busy with the previous ones), merged: the last pick of each kind wins,
/// Remove drops the rest, any later pick drops a Remove.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Pending {
    /// Some(true) install, Some(false) remove
    pub install: Option<bool>,
    pub upscaler: Option<&'static str>,
    pub framegen: Option<bool>,
}

impl Pending {
    pub fn add(&mut self, p: Pick) {
        match p {
            Pick::Install => self.install = Some(true),
            Pick::Remove => {
                *self = Pending {
                    install: Some(false),
                    ..Pending::default()
                }
            }
            Pick::Upscaler(m) => {
                if self.install == Some(false) {
                    self.install = None;
                }
                self.upscaler = Some(m);
            }
            Pick::FrameGen(on) => {
                if self.install == Some(false) {
                    self.install = None;
                }
                self.framegen = Some(on);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Pending::default()
    }

    /// The script's arguments for these picks on `exe` (full path).
    pub fn args(&self, exe: &str) -> Option<Vec<String>> {
        let mut a = vec!["-Game".to_string(), exe.to_string()];
        match self.install {
            Some(false) => {
                a.push("-Uninstall".into());
                return Some(a);
            }
            Some(true) => {}
            None if self.upscaler.is_none() && self.framegen.is_none() => return None,
            None => a.push("-Configure".into()),
        }
        if let Some(m) = self.upscaler {
            a.push("-Upscaler".into());
            a.push(m.into());
        }
        if let Some(on) = self.framegen {
            a.push("-FrameGen".into());
            a.push(if on { "on" } else { "off" }.into());
        }
        Some(a)
    }

    /// For the menu: "install, FSR 3.1, frame generation on"
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.install {
            Some(true) => parts.push("install".into()),
            Some(false) => parts.push("remove".into()),
            None => {}
        }
        if let Some(m) = self.upscaler {
            let t = UPSCALERS
                .iter()
                .find(|(k, _)| *k == m)
                .map_or(m, |(_, t)| *t);
            parts.push(t.split(" (").next().unwrap_or(t).to_string());
        }
        if let Some(on) = self.framegen {
            parts.push(format!(
                "frame generation {}",
                if on { "on" } else { "off" }
            ));
        }
        parts.join(", ")
    }
}

/// Menu ids inside a profile's block of 256 (app.rs): the enhance settings
/// use 0..16 * settings, the profile's "remove" 255; OptiScaler's picks are
/// these offsets from [`MENU_BASE`].
pub const MENU_BASE: usize = 160;
pub const MENU_INSTALL: usize = 0;
pub const MENU_REMOVE: usize = 1;
/// The upscalers, in [`UPSCALERS`] order
pub const MENU_UPSCALER: usize = 2;
pub const MENU_FG_ON: usize = 6;
pub const MENU_FG_OFF: usize = 7;
pub const MENU_COUNT: usize = 8;
// above the enhance settings' ids, below the profile's own 255
const _: () = assert!(crate::enhance::ALL.len() * 16 <= MENU_BASE && MENU_BASE + MENU_COUNT < 255);

/// The pick a profile menu offset (id - [`MENU_BASE`]) stands for.
pub fn pick_of(offset: usize) -> Option<Pick> {
    match offset {
        MENU_INSTALL => Some(Pick::Install),
        MENU_REMOVE => Some(Pick::Remove),
        MENU_FG_ON => Some(Pick::FrameGen(true)),
        MENU_FG_OFF => Some(Pick::FrameGen(false)),
        o if (MENU_UPSCALER..MENU_UPSCALER + UPSCALERS.len()).contains(&o) => {
            Some(Pick::Upscaler(UPSCALERS[o - MENU_UPSCALER].0))
        }
        _ => None,
    }
}

/// The script's full argument list for powershell.exe.
pub fn powershell_args(script: &str, args: &[String]) -> Vec<String> {
    let mut a: Vec<String> = [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(script.to_string());
    a.extend(args.iter().cloned());
    a
}

/// The notification text for a finished run: the script's own last line
/// when there is one, else what its exit code means.
pub fn result_text(exe: &str, code: Option<i32>, output: &str) -> (bool, String) {
    let last = |prefix: &str| {
        output
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix(prefix).map(|s| s.trim().to_string()))
    };
    match code {
        Some(0) => (
            true,
            last("OK:").unwrap_or_else(|| format!("OptiScaler: done for {exe}.")),
        ),
        Some(c) => {
            let msg = last("Install-OptiScaler:").unwrap_or_else(|| match c {
                EXIT_ANTICHEAT => format!(
                    "Not installed: {exe} has anti-cheat; OptiScaler could get the account banned."
                ),
                EXIT_RUNNING => format!("Nothing changed: {exe} is running."),
                EXIT_NOT_INSTALLED => format!("OptiScaler is not installed for {exe}."),
                _ => format!("Install-OptiScaler.ps1 failed for {exe} (exit code {c})."),
            });
            (false, msg)
        }
        None => (
            false,
            format!("Install-OptiScaler.ps1 did not run for {exe}."),
        ),
    }
}

/// The folder of a Windows path ("G:\\x\\game.exe" -> "G:\\x").
pub fn parent_dir(path: &str) -> &str {
    path.rsplit_once('\\').map_or(path, |(d, _)| d)
}

/// Is `path` (a process image) inside `dir` or below it? Case-insensitive.
pub fn runs_from(path: &str, dir: &str) -> bool {
    let d = dir.trim_end_matches('\\').to_lowercase();
    let p = path.to_lowercase();
    !d.is_empty() && p.strip_prefix(&d).is_some_and(|r| r.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(&str, &[&str])]) -> impl Fn(&str) -> Vec<String> {
        let owned: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect();
        move |d: &str| {
            owned
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(d))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        }
    }

    #[test]
    fn manifest_pins_a_release() {
        assert_eq!(pinned_version(), "0.9.4");
        let sha = manifest_value(MANIFEST, "release.sha256").unwrap();
        assert_eq!(sha.len(), 64);
        assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(manifest_value(MANIFEST, "release.url")
            .unwrap()
            .starts_with("https://github.com/optiscaler/OptiScaler/releases/download/"));
        assert!(manifest_value(MANIFEST, "license.url")
            .unwrap()
            .starts_with("https://"));
        assert_eq!(manifest_value(MANIFEST, "nope"), None);
    }

    #[test]
    fn builtin_list_has_the_required_games() {
        let a = AntiCheat::builtin();
        for exe in [
            "cs2.exe",
            "dota2.exe",
            "VALORANT-Win64-Shipping.exe",
            "FortniteClient-Win64-Shipping.exe",
            "r5apex_dx12.exe",
            "TslGame.exe",
            "RainbowSix_Vulkan.exe",
        ] {
            assert!(
                a.find(r"G:\Games\X", Some(exe), |_| Vec::new()).is_some(),
                "{exe}"
            );
        }
        assert!(a.roots.iter().any(|r| r == "Program Files (x86)"));
        assert!(a
            .find(r"G:\Games\X", Some("Cyberpunk2077.exe"), |_| Vec::new())
            .is_none());
    }

    #[test]
    fn names() {
        assert!(name_matches("CS2.EXE", "cs2.exe"));
        assert!(!name_matches("cs2x.exe", "cs2.exe"));
        assert!(name_matches("EasyAntiCheat_EOS", "EasyAntiCheat*"));
        assert!(!name_matches("Easy", "EasyAntiCheat*"));
        assert!(name_matches("anything", "*"));
    }

    #[test]
    fn folders_found_in_the_game_tree_not_the_library() {
        let a = AntiCheat::builtin();
        let dir = r"G:\SteamLibrary\steamapps\common\Foo\Binaries\Win64";
        assert_eq!(
            a.search_dirs(dir),
            vec![
                dir.to_string(),
                r"G:\SteamLibrary\steamapps\common\Foo\Binaries".to_string(),
                r"G:\SteamLibrary\steamapps\common\Foo".to_string(),
            ]
        );
        let clean = tree(&[(dir, &["Foo-Win64-Shipping.exe"])]);
        assert_eq!(a.find(dir, Some("Foo-Win64-Shipping.exe"), clean), None);
        let eac = tree(&[
            (dir, &["Foo-Win64-Shipping.exe"]),
            (
                r"G:\SteamLibrary\steamapps\common\Foo",
                &["EasyAntiCheat", "Foo.exe"],
            ),
        ]);
        assert!(a
            .find(dir, None, eac)
            .is_some_and(|s| s.starts_with("Easy Anti-Cheat (")));
        // C:\Program Files (x86)\EasyAntiCheat is the service, not the game's
        let pf = tree(&[(r"C:\Program Files (x86)", &["EasyAntiCheat", "Game"])]);
        assert_eq!(a.find(r"C:\Program Files (x86)\Game", None, pf), None);
        // never the drive root
        assert_eq!(a.search_dirs(r"D:\Foo\Bin"), vec![r"D:\Foo\Bin", r"D:\Foo"]);
        assert_eq!(a.search_dirs(r"D:\Foo"), vec![r"D:\Foo"]);
        // exes in the folder count when no exe is given
        let pubg = tree(&[(r"G:\Games\PUBG\Bin", &["TslGame.exe"])]);
        assert!(a
            .find(r"G:\Games\PUBG\Bin", None, pubg)
            .is_some_and(|s| s.contains("PUBG")));
        // at most four levels up
        let deep = r"G:\A\B\C\D\E\F";
        assert_eq!(a.search_dirs(deep).len(), 5);
    }

    #[test]
    fn markers() {
        let m = parse_marker(
            "# copied by Install-OptiScaler.ps1 (Conduit); -Uninstall removes these\r\n# version 0.9.4\r\n# proxy winmm\r\n# exe Game.exe\r\nwinmm.dll\r\n",
        );
        assert_eq!(m.version.as_deref(), Some("0.9.4"));
        assert_eq!(m.proxy.as_deref(), Some("winmm"));
        assert_eq!(m.exe.as_deref(), Some("Game.exe"));
        // the older marker: only the file list
        assert_eq!(parse_marker("# copied by x\ndxgi.dll\n"), Marker::default());
    }

    #[test]
    fn ini_state() {
        let base = "[Upscalers]\r\nDx11Upscaler=auto\r\nDx12Upscaler=auto\r\nVulkanUpscaler=auto\r\n[FrameGen]\r\nEnabled=auto\r\nFGOutput=auto\r\n[DLSS]\r\nRenderPresetOverride=auto\r\nRenderPresetForAll=auto\r\n";
        assert_eq!(ini_mode(base), Some("default"));
        assert!(!ini_framegen(base));
        for (m, _) in UPSCALERS {
            assert_eq!(ini_mode(&upscaler::optiscaler_ini(base, m)), Some(m));
        }
        let hand = base.replace("Dx12Upscaler=auto", "Dx12Upscaler=fsr22");
        assert_eq!(ini_mode(&hand), None);
        let fg = base
            .replace("Enabled=auto", "Enabled=true")
            .replace("FGOutput=auto", "FGOutput=fsrfg");
        assert!(ini_framegen(&fg));
        assert!(!ini_framegen(
            &fg.replace("FGOutput=fsrfg", "FGOutput=nofg")
        ));
        assert_eq!(ini_value(base, "dlss", "renderpresetforall"), Some("auto"));
        assert_eq!(ini_value(base, "DLSS", "Missing"), None);
    }

    #[test]
    fn pending_picks_merge() {
        let exe = r"G:\Games\Foo\foo.exe";
        let mut p = Pending::default();
        assert!(p.is_empty());
        assert_eq!(p.args(exe), None);
        p.add(Pick::Upscaler("fsr"));
        assert_eq!(
            p.args(exe).unwrap(),
            ["-Game", exe, "-Configure", "-Upscaler", "fsr"]
        );
        p.add(Pick::Install);
        p.add(Pick::FrameGen(true));
        p.add(Pick::Upscaler("xess"));
        assert_eq!(
            p.args(exe).unwrap(),
            ["-Game", exe, "-Upscaler", "xess", "-FrameGen", "on"]
        );
        assert_eq!(p.describe(), "install, XeSS, frame generation on");
        p.add(Pick::Remove);
        assert_eq!(p.args(exe).unwrap(), ["-Game", exe, "-Uninstall"]);
        assert_eq!(p.describe(), "remove");
        p.add(Pick::FrameGen(false));
        assert_eq!(
            p.args(exe).unwrap(),
            ["-Game", exe, "-Configure", "-FrameGen", "off"]
        );
    }

    #[test]
    fn menu_ids() {
        assert_eq!(MENU_UPSCALER + UPSCALERS.len(), MENU_FG_ON);
        assert_eq!(pick_of(MENU_INSTALL), Some(Pick::Install));
        assert_eq!(pick_of(MENU_REMOVE), Some(Pick::Remove));
        assert_eq!(pick_of(MENU_UPSCALER + 1), Some(Pick::Upscaler("fsr")));
        assert_eq!(pick_of(MENU_UPSCALER + 3), Some(Pick::Upscaler("default")));
        assert_eq!(pick_of(MENU_FG_OFF), Some(Pick::FrameGen(false)));
        assert_eq!(pick_of(MENU_COUNT), None);
    }

    #[test]
    fn powershell_command() {
        let a = powershell_args(
            r"C:\Program Files\Conduit\Install-OptiScaler.ps1",
            &["-Game".into(), r"G:\My Games\x.exe".into()],
        );
        assert_eq!(a[4], "-File");
        assert_eq!(
            crate::procs::command_line("powershell.exe", &a),
            r#"powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "C:\Program Files\Conduit\Install-OptiScaler.ps1" -Game "G:\My Games\x.exe""#
        );
    }

    #[test]
    fn results() {
        let out = "Downloading ...\r\nOK: OptiScaler 0.9.4 installed in G:\\x as dxgi.dll\r\n";
        assert_eq!(
            result_text("x.exe", Some(0), out),
            (
                true,
                "OptiScaler 0.9.4 installed in G:\\x as dxgi.dll".to_string()
            )
        );
        let (ok, t) = result_text(
            "cs2.exe",
            Some(2),
            "Install-OptiScaler: Refused: this game has anti-cheat: Counter-Strike 2 (VAC) (cs2.exe).",
        );
        assert!(!ok && t.starts_with("Refused: this game has anti-cheat"));
        assert!(result_text("cs2.exe", Some(2), "").1.contains("anti-cheat"));
        assert!(result_text("a.exe", Some(3), "").1.contains("running"));
        assert!(result_text("a.exe", Some(9), "").1.contains("exit code 9"));
        assert!(!result_text("a.exe", None, "").0);
    }

    #[test]
    fn paths() {
        assert_eq!(parent_dir(r"G:\x\game.exe"), r"G:\x");
        assert!(runs_from(r"g:\X\bin\game.exe", r"G:\x"));
        assert!(runs_from(r"G:\x\game.exe", r"G:\x\"));
        assert!(!runs_from(r"G:\xy\game.exe", r"G:\x"));
        assert!(!runs_from(r"G:\x", r"G:\x"));
        assert!(!runs_from(r"G:\x\a.exe", ""));
    }

    /// The script writes the same OptiScaler.ini values per mode as the
    /// guest-wide setting (upscaler.rs).
    #[test]
    fn script_mode_table_matches_upscaler_rs() {
        let ps = include_str!("../../../packaging/windows/Install-OptiScaler.ps1");
        let start = ps.find("$modeTable = @'").expect("mode table") + "$modeTable = @'".len();
        let end = start + ps[start..].find("'@").expect("table end");
        let rows: Vec<Vec<&str>> = ps[start..end]
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .filter(|r| !r.is_empty())
            .collect();
        for (m, _) in UPSCALERS {
            let script: Vec<(&str, &str, &str)> = rows
                .iter()
                .filter(|r| r[0] == m)
                .map(|r| (r[1], r[2], r[3]))
                .collect();
            assert_eq!(script, upscaler::optiscaler_values(m), "{m}");
        }
        assert!(rows.iter().all(|r| r.len() == 4));
    }
}
