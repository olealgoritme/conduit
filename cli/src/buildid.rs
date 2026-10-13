//! Build identity of Conduit's binaries (packaging/build-id.sh).
//!
//! Every Conduit binary's `--version` ends in `VERSION (git COMMIT, KIND)`,
//! and the bundled QEMU's reads
//! `QEMU emulator version 11.1.2 (conduit VERSION (git COMMIT, KIND), patches N SHA8)`.
//! `conduit doctor` reads them back to show which build each installed
//! binary comes from and to warn about a mix.

/// This CLI's own identity.
pub const OWN: &str = env!("CONDUIT_BUILD_ID");

/// The QEMU patch set this CLI was built with ("COUNT SHA8" of
/// host/qemu/patches), empty when it was built outside a checkout.
pub const QEMU_PATCHES: &str = env!("CONDUIT_QEMU_PATCHES");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id {
    pub version: String,
    pub commit: String,
    pub kind: String,
}

impl Id {
    /// Finds `VERSION (git COMMIT, KIND)` in a `--version` line.
    pub fn parse(s: &str) -> Option<Id> {
        let i = s.find(" (git ")?;
        let version = s[..i].rsplit([' ', '(']).next()?.to_string();
        let (commit, rest) = s[i + 6..].split_once(", ")?;
        let kind = rest.split(')').next()?;
        if version.is_empty() || commit.is_empty() || kind.is_empty() || commit.contains(' ') {
            return None;
        }
        Some(Id {
            version,
            commit: commit.to_string(),
            kind: kind.to_string(),
        })
    }

    pub fn own() -> Id {
        Id::parse(&format!("conduit {OWN}")).unwrap_or(Id {
            version: env!("CARGO_PKG_VERSION").into(),
            commit: "unknown".into(),
            kind: "local".into(),
        })
    }

    /// Built from the same commit, the same way. "-dirty" does not count:
    /// parts of one local build are often built minutes apart. A build from
    /// a source tarball names no commit ("unknown"); then the version has
    /// to do.
    pub fn same_build(&self, other: &Id) -> bool {
        let base = |c: &str| c.trim_end_matches("-dirty").to_string();
        if self.commit == "unknown" || other.commit == "unknown" {
            return self.kind == other.kind
                && self.commit == other.commit
                && self.version == other.version;
        }
        self.kind == other.kind && base(&self.commit) == base(&other.commit)
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (git {}, {})", self.version, self.commit, self.kind)
    }
}

/// The bundled QEMU's `--version`: its identity and patch set ("N SHA8",
/// or "stock"), when it is a Conduit build that names them.
pub fn parse_qemu(s: &str) -> Option<(Id, String)> {
    let first = s.lines().next()?;
    let i = first.find("(conduit ")?;
    let id = Id::parse(&first[i..])?;
    let patches = if let Some(j) = first.find(", patches ") {
        first[j + 10..].trim_end_matches(')').trim().to_string()
    } else if first.contains(", stock)") {
        "stock".to_string()
    } else {
        return None;
    };
    Some((id, patches))
}

/// What is wrong with the bundled QEMU next to a CLI of identity `own`
/// that expects `expected_patches` (empty: not known), if anything. QEMU is
/// rebuilt only when its recipe changes, so in a local checkout an older
/// commit is normal; a different patch set, a different kind of build, or
/// another release is not.
pub fn qemu_mismatch(qemu: &Id, patches: &str, own: &Id, expected_patches: &str) -> Option<String> {
    if !expected_patches.is_empty() && patches != expected_patches {
        return Some(format!(
            "carries QEMU patch set {patches}, this Conduit expects {expected_patches}"
        ));
    }
    if qemu.kind != own.kind {
        return Some(format!(
            "a {} build, the rest of Conduit is a {} build",
            qemu.kind, own.kind
        ));
    }
    if own.kind == "release" && qemu.version != own.version {
        return Some(format!(
            "from release {}, the rest of Conduit from {}",
            qemu.version, own.version
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conduit_binaries() {
        let id = Id::parse("conduit-backend 0.1.8 (git 2f2340ff1a, release)\n").unwrap();
        assert_eq!(id.version, "0.1.8");
        assert_eq!(id.commit, "2f2340ff1a");
        assert_eq!(id.kind, "release");
        assert_eq!(id.to_string(), "0.1.8 (git 2f2340ff1a, release)");
        let dirty =
            Id::parse("conduit-viewer 0.1.8+git5.gfe551204 (git fe55120446-dirty, local)").unwrap();
        assert_eq!(dirty.version, "0.1.8+git5.gfe551204");
        assert_eq!(dirty.commit, "fe55120446-dirty");
        assert!(Id::parse("conduit-backend 0.1.8").is_none());
        assert!(Id::parse("QEMU emulator version 11.1.2").is_none());
    }

    #[test]
    fn same_build_ignores_dirty_but_not_kind() {
        let a = Id::parse("x 0.1.8 (git abc, local)").unwrap();
        let b = Id::parse("y 0.1.8 (git abc-dirty, local)").unwrap();
        let c = Id::parse("z 0.1.8 (git abc, release)").unwrap();
        let d = Id::parse("w 0.1.8 (git abd, local)").unwrap();
        assert!(a.same_build(&b));
        assert!(!a.same_build(&c));
        assert!(!a.same_build(&d));
        let u = Id::parse("u 0.1.8 (git unknown, local)").unwrap();
        assert!(u.same_build(&u));
        assert!(!u.same_build(&a));
        let u7 = Id::parse("u 0.1.7 (git unknown, local)").unwrap();
        assert!(!u.same_build(&u7));
    }

    #[test]
    fn parses_qemu() {
        let v = "QEMU emulator version 11.1.2 (conduit 0.1.8 (git 2f2340ff1a, release), patches 9 6a30db54)\n\
                 Copyright (c) 2003-2026 Fabrice Bellard and the QEMU Project developers\n";
        let (id, p) = parse_qemu(v).unwrap();
        assert_eq!(id.to_string(), "0.1.8 (git 2f2340ff1a, release)");
        assert_eq!(p, "9 6a30db54");
        let stock = "QEMU emulator version 11.1.2 (conduit 0.1.8 (git abc, local), stock)\n";
        assert_eq!(parse_qemu(stock).unwrap().1, "stock");
        assert!(parse_qemu("QEMU emulator version 11.1.2\n").is_none());
        assert!(parse_qemu("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1)\n").is_none());
    }

    #[test]
    fn qemu_rules() {
        let own = Id::parse("c 0.1.8 (git aaa, release)").unwrap();
        let q = Id::parse("q 0.1.8 (git aaa, release)").unwrap();
        assert_eq!(qemu_mismatch(&q, "9 6a30db54", &own, "9 6a30db54"), None);
        assert!(qemu_mismatch(&q, "8 11111111", &own, "9 6a30db54").is_some());
        let local = Id::parse("q 0.1.8 (git bbb, local)").unwrap();
        assert!(qemu_mismatch(&local, "9 6a30db54", &own, "")
            .unwrap()
            .contains("local"));
        let old = Id::parse("q 0.1.7 (git ccc, release)").unwrap();
        assert!(qemu_mismatch(&old, "9 6a30db54", &own, "").is_some());
        // A local checkout's QEMU from an older commit with the same patches is fine.
        let own_local = Id::parse("c 0.1.8 (git ddd, local)").unwrap();
        assert_eq!(
            qemu_mismatch(&local, "9 6a30db54", &own_local, "9 6a30db54"),
            None
        );
    }
}
