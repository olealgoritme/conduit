// crates/abi/src/version.rs
//
// NVIDIA driver version representation and parsing.
//
// Ported from gVisor pkg/sentry/devices/nvproxy/version.go.

use core::fmt;

/// A parsed NVIDIA driver version, e.g. `535.129.03`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DriverVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl DriverVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse from the string returned by `NV_ESC_CHECK_VERSION_STR`.
    /// Expected format: `"535.129.03"`, or two parts for a release NVIDIA
    /// numbers that way (`"565.77"`), which is patch 0 -- the same name
    /// `.github/scripts/abi_update.py` gives its tables (`v565_77_00`).
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().splitn(3, '.');
        let major = number(parts.next()?)?;
        let minor = number(parts.next()?)?;
        let patch = match parts.next() {
            Some(p) => number(p)?,
            None => 0,
        };
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

/// Digits only: no sign, no space, no empty part.
fn number(p: &str) -> Option<u32> {
    if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    p.parse().ok()
}

/// What `/proc/driver/nvidia/version` says about the loaded module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcVersion {
    pub version: DriverVersion,
    /// The open kernel modules, as opposed to the closed (proprietary) ones.
    pub open: bool,
    /// The release exactly as the module prints it (`565.77`, not
    /// `565.77.00`): the form the guest is told and file names are matched on.
    pub raw: String,
}

/// The one reader of `/proc/driver/nvidia/version`.
///
/// Looks only at the `NVRM version:` line (the `GCC version:` line under it
/// carries a three-part number of its own) and accepts exactly the two
/// wordings the modules print, for any architecture:
///
/// ```text
/// NVRM version: NVIDIA UNIX x86_64 Kernel Module  565.77  Wed Oct 23 ...
/// NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  610.57.04  Release Build ...
/// ```
///
/// The release is two or three dotted numbers (two is patch 0). Anything else
/// is `None`: a new wording is a reason to look at it, not to guess a number
/// out of it. `gen/fixtures/proc_version.tsv` holds the lines this is tested
/// on, and the guest module's C reader is tested on the same file.
pub fn parse_proc_version(text: &str) -> Option<ProcVersion> {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("NVRM version:"))?;
    let w: Vec<&str> = line.split_whitespace().collect();
    // [NVIDIA UNIX <arch> Kernel Module <version>] or
    // [NVIDIA UNIX Open Kernel Module for <arch> <version>]
    let (open, at) = match w.as_slice() {
        ["NVIDIA", "UNIX", "Open", "Kernel", "Module", "for", _arch, ..] => (true, 7),
        ["NVIDIA", "UNIX", arch, "Kernel", "Module", ..] if *arch != "Open" => (false, 5),
        _ => return None,
    };
    let raw = *w.get(at)?;
    Some(ProcVersion {
        version: DriverVersion::parse(raw)?,
        open,
        raw: raw.to_string(),
    })
}

impl fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{:02}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let v = DriverVersion::new(535, 129, 3);
        assert_eq!(v.to_string(), "535.129.03");
    }

    #[test]
    fn a_two_part_release_is_patch_zero() {
        // The closed 565.77 module reports exactly this from
        // NV_ESC_CHECK_VERSION_STR and /proc/driver/nvidia/version.
        assert_eq!(
            DriverVersion::parse("565.77"),
            Some(DriverVersion::new(565, 77, 0))
        );
        assert_eq!(DriverVersion::parse("565"), None);
        assert_eq!(DriverVersion::parse("565.x"), None);
    }

    /// `fixtures/proc_version.tsv`: `text<TAB>expected`, `\\n` for a newline in
    /// the text; expected is `none` or `<version> <open|closed> <raw>`. The
    /// guest module's C reader (guest/linux/test/version_test.c) reads the
    /// same file, so the two cannot drift apart.
    const PROC_VERSION: &str = include_str!("../fixtures/proc_version.tsv");

    #[test]
    fn proc_version_fixture() {
        let mut rows = 0;
        for line in PROC_VERSION.lines().filter(|l| !l.starts_with('#')) {
            let (text, want) = line.split_once('\t').expect("text<TAB>expected");
            let text = text.replace("\\n", "\n");
            let got = parse_proc_version(&text).map(|p| {
                format!(
                    "{} {} {}",
                    p.version,
                    if p.open { "open" } else { "closed" },
                    p.raw
                )
            });
            assert_eq!(got.as_deref().unwrap_or("none"), want, "{text:?}");
            rows += 1;
        }
        assert!(rows >= 10, "the fixture lost rows");
    }

    #[test]
    fn digits_only() {
        for bad in ["+565.77", "565.+77", "565. 77", "565.77.", "565.77.1.2", ""] {
            assert_eq!(DriverVersion::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_ok() {
        assert_eq!(
            DriverVersion::parse("535.129.03"),
            Some(DriverVersion::new(535, 129, 3))
        );
    }
}
