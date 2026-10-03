//! Facts about the host: NVIDIA driver, supported driver versions, QEMU.

use crate::paths;

#[derive(Debug, PartialEq)]
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

pub fn major(v: &str) -> u32 {
    v.split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// Driver versions the backend has ABI tables for.
/// Installed: share/conduit/supported-drivers.txt (one per line). Source
/// checkout: the generated table files. Else the list this CLI was built with.
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
        for d in ["host/backend/gen/src/versions", "gen/src/versions"] {
            if let Ok(rd) = std::fs::read_dir(r.join(d)) {
                let mut v: Vec<String> = rd
                    .flatten()
                    .filter_map(|e| version_from_table_name(&e.file_name().to_string_lossy()))
                    .collect();
                if !v.is_empty() {
                    v.sort();
                    return (v, "source tree");
                }
            }
        }
    }
    (
        BUILT_IN.iter().map(|s| s.to_string()).collect(),
        "built-in list",
    )
}

const BUILT_IN: &[&str] = &["535.129.03", "580.178.04", "595.71.05", "610.57.04"];

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
        assert_eq!(parse_driver("garbage"), None);
        assert_eq!(major("610.57.04"), 610);
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
