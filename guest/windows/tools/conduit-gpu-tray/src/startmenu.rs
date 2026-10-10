//! Start Menu shortcuts as the app list.

use conduit_ctl::App;
use std::path::{Path, PathBuf};

/// Shortcuts that are not apps: uninstallers, documentation, web links.
pub fn skip(name: &str) -> bool {
    let n = name.to_lowercase();
    ["uninstall", "readme", "help", "website"]
        .iter()
        .any(|w| n.contains(w))
}

/// The apps among these files: `.lnk` only, named by file stem, noise
/// dropped, the first of equal names (compared without case) kept.
pub fn collect(paths: impl IntoIterator<Item = PathBuf>) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in paths {
        let is_lnk = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
        let Some(name) = p
            .file_stem()
            .map(|s| s.to_string_lossy().trim().to_string())
        else {
            continue;
        };
        if !is_lnk || name.is_empty() || skip(&name) || !seen.insert(name.to_lowercase()) {
            continue;
        }
        let target = p.to_string_lossy().into_owned();
        apps.push(App {
            name,
            icon: target.clone(),
            target,
            args: Vec::new(),
            source: "startmenu".into(),
        });
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 8 || out.len() > 20_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), depth + 1, out);
        } else {
            out.push(e.path());
        }
    }
}

/// Every app under these Start Menu `Programs` folders (earlier roots win).
pub fn scan(roots: &[PathBuf]) -> Vec<App> {
    let mut files = Vec::new();
    for r in roots {
        walk(r, 0, &mut files);
    }
    collect(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn skip_rules() {
        for n in [
            "Uninstall Foo",
            "Foo README",
            "Foo Help",
            "Visit the Foo WebSite",
            "Remove (UNINSTALL)",
        ] {
            assert!(skip(n), "{n}");
        }
        for n in ["Notepad++", "Steam", "Shelp"] {
            assert_eq!(skip(n), n == "Shelp", "{n}");
        }
    }

    #[test]
    fn collect_filters_and_dedupes() {
        let p = |s: &str| PathBuf::from(s);
        let apps = collect([
            p("C:/PD/Programs/Zed.lnk"),
            p("C:/PD/Programs/Tools/notepad.LNK"),
            p("C:/PD/Programs/Tools/Uninstall Zed.lnk"),
            p("C:/PD/Programs/desktop.ini"),
            p("C:/PD/Programs/Docs.url"),
            p("C:/PD/Programs/Alpha.lnk"),
            p("C:/Users/u/Programs/NOTEPAD.lnk"),
            p("C:/Users/u/Programs/Zed.lnk"),
        ]);
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "notepad", "Zed"]);
        assert_eq!(apps[2].target, "C:/PD/Programs/Zed.lnk");
        assert_eq!(apps[2].icon, apps[2].target);
        assert_eq!(apps[2].source, "startmenu");
    }

    #[test]
    fn scan_walks_recursively_with_unicode_and_spaces() {
        let t = TempDir::new("sm");
        let all = t.path().join("all");
        let user = t.path().join("user");
        std::fs::create_dir_all(all.join("Sub Folder").join("Deep")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            all.join("Sub Folder")
                .join("Deep")
                .join("\u{c6}ble Spill.lnk"),
            b"",
        )
        .unwrap();
        std::fs::write(all.join("Both.lnk"), b"").unwrap();
        std::fs::write(user.join("Both.lnk"), b"").unwrap();
        std::fs::write(user.join("Mine.lnk"), b"").unwrap();
        let apps = scan(&[all.clone(), user, t.path().join("missing")]);
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Both", "Mine", "\u{c6}ble Spill"]);
        assert!(apps[0].target.starts_with(all.to_string_lossy().as_ref()));
    }
}
