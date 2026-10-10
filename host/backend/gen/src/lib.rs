// crates/abi/src/lib.rs
//
// NVIDIA kernel driver ABI definitions.
//
// Ported from gVisor's pkg/abi/nvgpu/ (Apache-2.0).

pub mod devinfo;
pub mod fixtures;
pub mod ioctl;
pub mod names;
pub mod nvkms;
pub mod osdesc;
pub mod rmallow;
pub mod rmctrl;
pub mod types;
pub mod uvm;
pub mod version;
pub mod versions;
pub mod vidmem;

#[cfg(test)]
mod registration {
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The tables with one generated file per driver release.
    const TABLES: &[&str] = &[
        "versions", "rmctrl", "rmallow", "uvm", "vidmem", "devinfo", "nvkms", "osdesc",
    ];

    fn version_key(stem: &str) -> (u32, u32, u32) {
        let mut p = stem[1..].split('_').map(|n| n.parse::<u32>().unwrap());
        (p.next().unwrap(), p.next().unwrap(), p.next().unwrap())
    }

    fn is_stem(s: &str) -> bool {
        s.strip_prefix('v')
            .map(|r| r.split('_').count() == 3 && r.split('_').all(|n| n.parse::<u32>().is_ok()))
            .unwrap_or(false)
    }

    /// Every release file has a `pub mod` and an entry in the table's
    /// `PROFILES`, and the entries are in ascending order. A file that is only
    /// declared is compiled but never selected, and the release looks
    /// unsupported (or silently falls back to an older table).
    #[test]
    fn every_release_file_is_declared_and_registered_in_order() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for table in TABLES {
            let dir = src.join(table);
            let files: BTreeSet<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| {
                    let name = e.unwrap().file_name().into_string().unwrap();
                    name.strip_suffix(".rs")
                        .filter(|s| is_stem(s))
                        .map(str::to_string)
                })
                .collect();
            let text = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
            let (decls, rest) = text
                .split_once("static PROFILES")
                .unwrap_or_else(|| panic!("{table}/mod.rs has no PROFILES"));
            let declared: BTreeSet<String> = decls
                .lines()
                .filter_map(|l| l.trim().strip_prefix("pub mod ")?.strip_suffix(';'))
                .map(str::to_string)
                .collect();
            assert_eq!(
                files, declared,
                "{table}: release files and `pub mod` lines differ"
            );

            let profiles = rest.split_once("\n];").map(|(p, _)| p).unwrap_or(rest);
            let mut used: Vec<&str> = Vec::new();
            for tok in profiles.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                if is_stem(tok) && !used.contains(&tok) {
                    used.push(tok);
                }
            }
            let used_set: BTreeSet<String> = used.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                files, used_set,
                "{table}: every release file needs an entry in PROFILES"
            );
            let mut sorted = used.clone();
            sorted.sort_by_key(|s| version_key(s));
            assert_eq!(used, sorted, "{table}: PROFILES must ascend");
        }
    }
}
