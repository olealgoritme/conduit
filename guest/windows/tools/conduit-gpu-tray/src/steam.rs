//! Steam games: library folders, app manifests and library art.

use crate::vdf::{self, Vdf};
use conduit_ctl::App;
use std::path::{Path, PathBuf};

/// Steam's own components, not games.
const SKIP_IDS: [&str; 6] = [
    "228980",  // Steamworks Common Redistributables
    "1070560", // Steam Linux Runtime 1.0
    "1391110", // Steam Linux Runtime 2.0
    "1628350", // Steam Linux Runtime 3.0
    "1493710", // Proton Experimental
    "1826330", // Proton EasyAntiCheat Runtime
];
const SKIP_PREFIXES: [&str; 4] = [
    "Steam Linux Runtime",
    "Proton",
    "Steamworks",
    "Steam Controller Configs",
];

/// Library roots listed in a `libraryfolders.vdf`, old and new layout.
pub fn library_paths(text: &str) -> Vec<String> {
    let Some(root) = vdf::parse(text) else {
        return Vec::new();
    };
    let Some(libs) = root.get("libraryfolders") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in libs.pairs() {
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = match v {
            Vdf::Str(s) => Some(s.as_str()),
            Vdf::Obj(_) => v.get("path").and_then(Vdf::as_str),
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            out.push(p.to_string());
        }
    }
    out
}

/// `(appid, name)` of an `appmanifest_*.acf` that is an installed game.
pub fn parse_manifest(text: &str) -> Option<(String, String)> {
    let root = vdf::parse(text)?;
    let st = root.get("AppState")?;
    let id = st.get("appid")?.as_str()?.trim().to_string();
    let name = st.get("name")?.as_str()?.trim().to_string();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || name.is_empty() {
        return None;
    }
    // StateFlags bit 2 is "fully installed"; absent means unknown, accept.
    if let Some(f) = st
        .get("StateFlags")
        .and_then(Vdf::as_str)
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        if f & 4 == 0 {
            return None;
        }
    }
    Some((id, name))
}

/// Tools and runtimes that are not games.
pub fn is_tool(id: &str, name: &str) -> bool {
    SKIP_IDS.contains(&id) || SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub fn run_url(id: &str) -> String {
    format!("steam://rungameid/{id}")
}

/// The id of a `steam:ID` icon key; digits only.
pub fn icon_key_id(key: &str) -> Option<&str> {
    let id = key.strip_prefix("steam:")?;
    (!id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// Installed games of every library under a Steam install.
pub fn scan(steam_root: &Path) -> Vec<App> {
    let mut libs = vec![steam_root.to_path_buf()];
    if let Ok(t) = std::fs::read_to_string(steam_root.join("steamapps").join("libraryfolders.vdf"))
    {
        for p in library_paths(&t) {
            let p = PathBuf::from(p);
            if !libs.iter().any(|l| same_path(l, &p)) {
                libs.push(p);
            }
        }
    }
    let mut apps: Vec<App> = Vec::new();
    for lib in libs {
        let Ok(rd) = std::fs::read_dir(lib.join("steamapps")) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if !(n.starts_with("appmanifest_") && n.ends_with(".acf")) {
                continue;
            }
            let Some((id, name)) = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|t| parse_manifest(&t))
            else {
                continue;
            };
            if is_tool(&id, &name) || apps.iter().any(|a| a.icon == format!("steam:{id}")) {
                continue;
            }
            apps.push(App {
                name,
                target: run_url(&id),
                args: Vec::new(),
                icon: format!("steam:{id}"),
                source: "steam".into(),
            });
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

/// Library art to try for an app's icon, best first: the old flat
/// `ID_icon.jpg`, then the per-app folder's client icon (a 40-hex-digit
/// name, the smallest such file) and its larger art.
pub fn icon_candidates(steam_root: &Path, id: &str) -> Vec<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache");
    let mut out = vec![cache.join(format!("{id}_icon.jpg"))];
    let dir = cache.join(id);
    let mut hashed: Vec<(u64, PathBuf)> = Vec::new();
    let mut walk = vec![(dir.clone(), 0u8)];
    while let Some((d, depth)) = walk.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 2 {
                    walk.push((p, depth + 1));
                }
                continue;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if let Some(stem) = name.strip_suffix(".jpg") {
                if stem.len() == 40 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                    hashed.push((e.metadata().map(|m| m.len()).unwrap_or(u64::MAX), p));
                }
            }
        }
    }
    hashed.sort();
    out.extend(hashed.into_iter().map(|(_, p)| p));
    for n in [
        "library_capsule.jpg",
        "library_600x900.jpg",
        "capsule_231x87.jpg",
        "header.jpg",
    ] {
        out.push(dir.join(n));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const ACF: &str = r#""AppState"
{
	"appid"		"730"
	"Universe"		"1"
	"name"		"Counter-Strike 2"
	"StateFlags"		"4"
	"installdir"		"Counter-Strike Global Offensive"
}"#;

    #[test]
    fn manifests() {
        assert_eq!(
            parse_manifest(ACF),
            Some(("730".into(), "Counter-Strike 2".into()))
        );
        // Not fully installed (downloading), no name, junk ids, not a manifest.
        assert_eq!(parse_manifest(&ACF.replace("\"4\"", "\"1026\"")), None);
        assert!(parse_manifest(&ACF.replace("\"4\"", "\"6\"")).is_some());
        assert_eq!(parse_manifest(&ACF.replace("\"730\"", "\"../x\"")), None);
        assert_eq!(parse_manifest("\"Other\" { }"), None);
        assert_eq!(parse_manifest("garbage {"), None);
    }

    #[test]
    fn tools_are_skipped() {
        assert!(is_tool("228980", "Steamworks Common Redistributables"));
        assert!(is_tool("1", "Steam Linux Runtime 3.0 (sniper)"));
        assert!(is_tool("2", "Proton 9.0"));
        assert!(is_tool("3", "Steamworks Shared"));
        assert!(!is_tool("730", "Counter-Strike 2"));
    }

    #[test]
    fn library_paths_new_and_old_layouts() {
        let new = "\"libraryfolders\" { \"0\" { \"path\" \"C:\\\\Steam\" } \"1\" { \"path\" \"D:\\\\Lib\" } \"contentstatsid\" \"123\" }";
        assert_eq!(library_paths(new), [r"C:\Steam", r"D:\Lib"]);
        let old = "\"LibraryFolders\" { \"TimeNextStatsReport\" \"99\" \"1\" \"E:\\\\Old Lib\" }";
        assert_eq!(library_paths(old), [r"E:\Old Lib"]);
        assert!(library_paths("nonsense {").is_empty());
        assert!(library_paths("\"x\" { }").is_empty());
    }

    #[test]
    fn icon_keys() {
        assert_eq!(icon_key_id("steam:730"), Some("730"));
        assert_eq!(icon_key_id("steam:"), None);
        assert_eq!(icon_key_id("steam:..\\x"), None);
        assert_eq!(icon_key_id("C:\\x.lnk"), None);
    }

    #[test]
    fn scans_libraries_and_dedupes() {
        let t = TempDir::new("steam");
        let root = t.path().join("Steam");
        let lib2 = t.path().join("Lib 2 \u{e6}");
        for (dir, id, name) in [
            (&root, "730", "Counter-Strike 2"),
            (&root, "228980", "Steamworks Common Redistributables"),
            (&lib2, "570", "Dota 2"),
            (&lib2, "730", "Counter-Strike 2 (dup)"),
        ] {
            let sa = dir.join("steamapps");
            std::fs::create_dir_all(&sa).unwrap();
            std::fs::write(
                sa.join(format!("appmanifest_{id}.acf")),
                ACF.replace("730", id).replace("Counter-Strike 2", name),
            )
            .unwrap();
        }
        let vdf = format!(
            "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{r}\" }} \"1\" {{ \"path\" \"{l}\" }} \"2\" {{ \"path\" \"{t}/missing\" }} }}",
            r = root.to_string_lossy().replace('\\', "\\\\"),
            l = lib2.to_string_lossy().replace('\\', "\\\\"),
            t = t.path().to_string_lossy().replace('\\', "\\\\"),
        );
        std::fs::write(root.join("steamapps").join("libraryfolders.vdf"), vdf).unwrap();
        let apps = scan(&root);
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Counter-Strike 2", "Dota 2"]);
        assert_eq!(apps[0].target, "steam://rungameid/730");
        assert_eq!(apps[0].icon, "steam:730");
        assert_eq!(apps[1].source, "steam");
        assert!(scan(&t.path().join("nope")).is_empty());
    }

    #[test]
    fn icon_candidates_prefer_the_small_hashed_jpg() {
        let t = TempDir::new("art");
        let d = t
            .path()
            .join("appcache")
            .join("librarycache")
            .join("730")
            .join("sub");
        std::fs::create_dir_all(&d).unwrap();
        let small = format!("{}.jpg", "a".repeat(40));
        let big = format!("{}.jpg", "b".repeat(40));
        std::fs::write(d.join(&big), vec![0u8; 100]).unwrap();
        std::fs::write(d.join(&small), vec![0u8; 10]).unwrap();
        std::fs::write(d.join("header.jpg"), b"x").unwrap();
        let c = icon_candidates(t.path(), "730");
        assert!(c[0].ends_with("730_icon.jpg"));
        assert!(c[1].ends_with(&small));
        assert!(c[2].ends_with(&big));
        assert!(c.last().unwrap().ends_with("header.jpg"));
    }
}
