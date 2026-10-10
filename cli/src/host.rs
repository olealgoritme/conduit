//! Facts about the host: NVIDIA driver, supported driver versions, QEMU.

use crate::paths;
use abi::version::DriverVersion;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub version: String,
    pub open: bool,
}

/// Parse /proc/driver/nvidia/version: the rule is `abi::version::parse_proc_version`.
pub fn parse_driver(text: &str) -> Option<Driver> {
    let p = abi::version::parse_proc_version(text)?;
    Some(Driver {
        version: p.raw,
        open: p.open,
    })
}

pub fn driver() -> Option<Driver> {
    parse_driver(&std::fs::read_to_string("/proc/driver/nvidia/version").ok()?)
}

/// "565.77" and "565.77.00" are one release: a missing patch is 0, as in the
/// backend's `DriverVersion::parse` and the table file names (`v565_77_00`).
pub fn same_release(a: &str, b: &str) -> bool {
    match (DriverVersion::parse(a), DriverVersion::parse(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
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
    DriverVersion::parse(v).map_or(0, |v| v.major)
}

/// An NVIDIA display GPU bound to the `nvidia` driver, as `conduit doctor` shows it.
#[derive(Debug, PartialEq)]
pub struct Gpu {
    /// PCI address, e.g. `0000:01:00.0`.
    pub addr: String,
    /// PCI device id (0x2b85 for an RTX 5090, 0x2786 / 0x2709 for an RTX 4070).
    pub device: u32,
    /// `Model:` from /proc/driver/nvidia/gpus/<addr>/information.
    pub model: Option<String>,
    /// BAR1 length in bytes (the second line of sysfs `resource`).
    pub bar1: Option<u64>,
}

/// The `Model:` line of /proc/driver/nvidia/gpus/<addr>/information.
pub fn parse_gpu_model(info: &str) -> Option<String> {
    let line = info
        .lines()
        .find(|l| l.trim_start().starts_with("Model:"))?;
    let m = line.trim_start().trim_start_matches("Model:").trim();
    (!m.is_empty()).then(|| m.to_string())
}

/// BAR1's length from a sysfs `resource` file (one `start end flags` line per BAR).
pub fn parse_bar1(resource: &str) -> Option<u64> {
    let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
    let mut f = resource.lines().nth(1)?.split_whitespace();
    let (start, end) = (hex(f.next()?)?, hex(f.next()?)?);
    (end > start).then(|| end - start + 1)
}

/// NVIDIA display GPUs under `pci_devices` (/sys/bus/pci/devices) bound to `nvidia`, with
/// their model from `proc_gpus` (/proc/driver/nvidia/gpus). Sorted by address.
pub fn gpus_in(pci_devices: &Path, proc_gpus: &Path) -> Vec<Gpu> {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).ok();
    let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let Ok(dir) = std::fs::read_dir(pci_devices) else {
        return Vec::new();
    };
    let mut v: Vec<Gpu> = dir
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            if hex(&read(d.join("vendor"))?)? != 0x10de
                || hex(&read(d.join("class"))?)? >> 16 != 0x03
            {
                return None;
            }
            let driver = std::fs::read_link(d.join("driver")).ok()?;
            if driver.file_name()? != "nvidia" {
                return None;
            }
            let addr = e.file_name().to_string_lossy().into_owned();
            Some(Gpu {
                device: hex(&read(d.join("device"))?)? as u32,
                model: read(proc_gpus.join(&addr).join("information"))
                    .and_then(|t| parse_gpu_model(&t)),
                bar1: read(d.join("resource")).and_then(|t| parse_bar1(&t)),
                addr,
            })
        })
        .collect();
    v.sort_by(|a, b| a.addr.cmp(&b.addr));
    v
}

pub fn gpus() -> Vec<Gpu> {
    gpus_in(
        Path::new("/sys/bus/pci/devices"),
        Path::new("/proc/driver/nvidia/gpus"),
    )
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
    "595.99.02",
    "595.104.02",
    "610.43.02",
    "610.43.03",
    "610.57.04",
    "615.71.09",
    "615.78.08",
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

    /// The lines `abi::version` and the backend and guest readers are tested
    /// on: the CLI must say the same about each.
    #[test]
    fn driver_follows_the_shared_fixture() {
        let fixture = include_str!("../../host/backend/gen/fixtures/proc_version.tsv");
        for line in fixture.lines().filter(|l| !l.starts_with('#')) {
            let (text, want) = line.split_once('\t').unwrap();
            let got = parse_driver(&text.replace("\\n", "\n"))
                .map(|d| format!("{} {}", d.version, if d.open { "open" } else { "closed" }));
            // "<canonical> <open|closed> <raw>" becomes "<raw> <open|closed>".
            let f: Vec<&str> = want.split(' ').collect();
            let want = (f.len() == 3).then(|| format!("{} {}", f[2], f[1]));
            assert_eq!(got, want, "{text:?}");
        }
    }

    #[test]
    fn a_missing_patch_is_zero() {
        assert!(same_release("565.77", "565.77.00"));
        assert!(same_release("580.178.04", "580.178.4"));
        assert!(!same_release("565.77", "565.77.01"));
        assert!(!same_release("565.77", "565.57.01"));
    }

    #[test]
    fn gpu_model_and_bar1() {
        let info = "Model: \t\t NVIDIA GeForce RTX 5090\nIRQ:   \t\t 180\nGPU UUID: \t GPU-x\n";
        assert_eq!(
            parse_gpu_model(info).as_deref(),
            Some("NVIDIA GeForce RTX 5090")
        );
        assert_eq!(parse_gpu_model("IRQ: 1\n"), None);
        // An RTX 5090: BAR0 64 MiB, BAR1 32 GiB.
        let res = "0x00000000f8000000 0x00000000fbffffff 0x0000000000040200\n\
                   0x0000004000000000 0x00000047ffffffff 0x000000000014220c\n";
        assert_eq!(parse_bar1(res), Some(32 << 30));
        // ReBAR off: 256 MiB.
        let off = "0x00000000f8000000 0x00000000fbffffff 0x0000000000040200\n\
                   0x00000000e0000000 0x00000000efffffff 0x000000000014220c\n";
        assert_eq!(parse_bar1(off), Some(256 << 20));
        assert_eq!(parse_bar1("0x0 0x0 0x0\n0x0 0x0 0x0\n"), None);
        assert_eq!(parse_bar1(""), None);
    }

    #[test]
    fn gpus_from_sysfs() {
        let t = std::env::temp_dir().join(format!("conduit-gpus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let (pci, procg) = (t.join("pci"), t.join("proc"));
        let dev = |addr: &str, vendor: &str, class: &str, device: &str, driver: &str| {
            let d = pci.join(addr);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), vendor).unwrap();
            std::fs::write(d.join("class"), class).unwrap();
            std::fs::write(d.join("device"), device).unwrap();
            std::fs::write(
                d.join("resource"),
                "0x00000000f8000000 0x00000000fbffffff 0x0\n0x0000004000000000 0x00000043ffffffff 0x0\n",
            )
            .unwrap();
            let drv = t.join("drivers").join(driver);
            std::fs::create_dir_all(&drv).unwrap();
            std::os::unix::fs::symlink(&drv, d.join("driver")).unwrap();
        };
        dev(
            "0000:01:00.0",
            "0x10de\n",
            "0x030000\n",
            "0x2786\n",
            "nvidia",
        );
        dev(
            "0000:01:00.1",
            "0x10de\n",
            "0x040300\n",
            "0x22bc\n",
            "snd_hda_intel",
        ); // its audio
        dev(
            "0000:02:00.0",
            "0x10de\n",
            "0x030000\n",
            "0x2b85\n",
            "vfio-pci",
        ); // not ours
        dev("0000:03:00.0", "0x8086\n", "0x030000\n", "0xa780\n", "i915");
        std::fs::create_dir_all(procg.join("0000:01:00.0")).unwrap();
        std::fs::write(
            procg.join("0000:01:00.0/information"),
            "Model: \t\t NVIDIA GeForce RTX 4070\n",
        )
        .unwrap();
        let g = gpus_in(&pci, &procg);
        let _ = std::fs::remove_dir_all(&t);
        assert_eq!(
            g,
            vec![Gpu {
                addr: "0000:01:00.0".into(),
                device: 0x2786,
                model: Some("NVIDIA GeForce RTX 4070".into()),
                bar1: Some(16 << 30),
            }]
        );
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
