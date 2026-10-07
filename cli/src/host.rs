//! Facts about the host: NVIDIA driver, supported driver versions, QEMU.

use crate::paths;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub version: String,
    pub open: bool,
}

/// Parse /proc/driver/nvidia/version.
pub fn parse_driver(text: &str) -> Option<Driver> {
    let line = text.lines().find(|l| l.starts_with("NVRM version:"))?;
    let open = line.contains("Open Kernel Module");
    let version = line
        .split_whitespace()
        .find(|w| {
            w.split('.').count() >= 2
                && w.split('.')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })?
        .to_string();
    Some(Driver { version, open })
}

pub fn driver() -> Option<Driver> {
    parse_driver(&std::fs::read_to_string("/proc/driver/nvidia/version").ok()?)
}

/// "565.77" and "565.77.00" are one release: a missing patch is 0, as in the
/// backend's `DriverVersion::parse` and the table file names (`v565_77_00`).
pub fn same_release(a: &str, b: &str) -> bool {
    fn key(v: &str) -> Vec<u64> {
        let mut k = version_key(v);
        while k.len() < 3 {
            k.push(0);
        }
        k
    }
    key(a) == key(b)
}

/// Why a loaded driver is outside what Conduit was built and tested on: the
/// open kernel modules, release 580 or newer. `None` is the proven setup.
/// The one rule behind `conduit doctor`'s module line and every protection
/// that is on by default (protect.rs).
pub fn untested_because(d: &Driver) -> Option<&'static str> {
    match (d.open, major(&d.version) < 580) {
        (true, false) => None,
        (false, false) => Some("the closed kernel modules"),
        (true, true) => Some("a branch older than 580"),
        (false, true) => Some("the closed kernel modules and a branch older than 580"),
    }
}

pub fn major(v: &str) -> u32 {
    v.split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// Driver releases the backend accepts: those with exact ABI tables.
/// Installed: share/conduit/supported-drivers.txt (one per line, written by
/// packaging/build.sh via packaging/supported-drivers.sh). Source checkout:
/// the generated table files. Else the list this CLI was built with.
pub fn supported_drivers() -> (Vec<String>, &'static str) {
    let f = paths::prefix().join("share/conduit/supported-drivers.txt");
    if let Ok(s) = std::fs::read_to_string(&f) {
        let v: Vec<String> = s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        if !v.is_empty() {
            return (v, "installed list");
        }
    }
    if let Some(r) = paths::repo_root() {
        let v = tree_releases(&r.join("host/backend/gen/src"));
        if !v.is_empty() {
            return (v, "source tree");
        }
    }
    (
        BUILT_IN.iter().map(|s| s.to_string()).collect(),
        "built-in list",
    )
}

/// The table directories the backend's exact-table check reads
/// (`NvidiaBackend::inexact_tables`); a release needs its own file in each.
pub const EXACT_TABLES: &[&str] = &["rmctrl", "rmallow", "uvm", "vidmem", "devinfo", "nvkms"];

/// Releases with a table of their own in every one of [`EXACT_TABLES`]
/// under `gen_src` (host/backend/gen/src), ascending.
pub fn tree_releases(gen_src: &Path) -> Vec<String> {
    let mut sets = EXACT_TABLES.iter().map(|t| {
        std::fs::read_dir(gen_src.join(t))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| version_from_table_name(&e.file_name().to_string_lossy()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let mut v = sets.next().unwrap_or_default();
    for s in sets {
        v.retain(|r| s.contains(r));
    }
    v.sort_by_key(|r| version_key(r));
    v
}

/// "595.104.02" -> [595, 104, 2], for numeric ordering.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

const BUILT_IN: &[&str] = &[
    "535.129.03",
    "565.77.00",
    "580.178.04",
    "595.71.05",
    "595.104.02",
    "610.57.04",
    "615.71.09",
];

/// "v610_57_04.rs" -> "610.57.04"
pub fn version_from_table_name(n: &str) -> Option<String> {
    let s = n.strip_prefix('v')?.strip_suffix(".rs")?;
    let parts: Vec<&str> = s.split('_').collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(parts.join("."))
}

/// QEMU version as (major, minor) from `qemu-system-x86_64 --version`.
pub fn parse_qemu_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.split("version").nth(1)?;
    let v = rest.split_whitespace().next()?;
    let mut it = v.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((maj, min))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI's list (`conduit doctor`), the installed list build.sh writes
    /// (packaging/supported-drivers.sh) and the built-in fallback agree. The
    /// backend's own test checks its accepted releases against the same script.
    #[test]
    fn supported_drivers_match_the_backend() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let gen = root.join("host/backend/gen/src");
        let tree = tree_releases(&gen);
        assert!(tree.iter().any(|v| v == "595.104.02"), "{tree:?}");
        assert!(tree.iter().any(|v| v == "615.71.09"), "{tree:?}");
        assert!(tree.iter().any(|v| v == "565.77.00"), "{tree:?}");
        let out = std::process::Command::new(root.join("packaging/supported-drivers.sh"))
            .arg(&gen)
            .output()
            .unwrap();
        assert!(out.status.success());
        let script: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(tree, script, "CLI vs packaging/supported-drivers.sh");
        assert_eq!(tree, BUILT_IN, "update host.rs BUILT_IN");
    }

    #[test]
    fn driver_version() {
        let t = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  610.57.04  Release Build  (dvs-builder@U22)  Wed Jul 29 02:45:17 UTC 2026\nGCC version:  gcc version 13.3.0\n";
        assert_eq!(
            parse_driver(t),
            Some(Driver {
                version: "610.57.04".into(),
                open: true
            })
        );
        let p = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Thu Feb 22 01:44:30 UTC 2024\n";
        assert_eq!(
            parse_driver(p),
            Some(Driver {
                version: "550.54.14".into(),
                open: false
            })
        );
        // The closed 565.77 module: two-part version, and the GCC line's
        // three-part number is not the driver's.
        let c = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  565.77  Wed Oct 23 12:00:00 UTC 2024\nGCC version:  gcc version 13.3.0 (Ubuntu 13.3.0-6ubuntu2~24.04)\n";
        assert_eq!(
            parse_driver(c),
            Some(Driver {
                version: "565.77".into(),
                open: false
            })
        );
        assert_eq!(parse_driver("garbage"), None);
        assert_eq!(major("610.57.04"), 610);
    }

    #[test]
    fn a_missing_patch_is_zero() {
        assert!(same_release("565.77", "565.77.00"));
        assert!(same_release("580.178.04", "580.178.4"));
        assert!(!same_release("565.77", "565.77.01"));
        assert!(!same_release("565.77", "565.57.01"));
    }

    #[test]
    fn table_names() {
        assert_eq!(
            version_from_table_name("v610_57_04.rs").as_deref(),
            Some("610.57.04")
        );
        assert_eq!(version_from_table_name("mod.rs"), None);
        assert_eq!(version_from_table_name("v_x.rs"), None);
    }

    #[test]
    fn qemu_versions() {
        assert_eq!(
            parse_qemu_version("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.18)\n"),
            Some((8, 2))
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 11.1.0\nCopyright"),
            Some((11, 1))
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 10.0.50 (v10.0.0-123)"),
            Some((10, 0))
        );
        assert_eq!(parse_qemu_version("nope"), None);
    }
}
