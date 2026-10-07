//! `conduit setup fetch-nfpm`: download nfpm from its GitHub release, check the
//! archive against the release's own checksums.txt and install the binary to
//! ~/.local/bin. The decisions are pure functions; only `fetch` touches the net.

use super::data::link;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The release archive for this machine, as named in checksums.txt.
pub const ASSET_SUFFIX: &str = "_Linux_x86_64.tar.gz";

#[derive(Debug, PartialEq, Eq)]
pub struct Asset {
    pub file: String,
    pub sha256: String,
    pub version: String,
}

/// Find the Linux x86_64 archive in a `sha256  file` list.
pub fn parse_checksums(text: &str) -> Result<Asset> {
    for l in text.lines() {
        let mut w = l.split_whitespace();
        let (Some(sha), Some(file)) = (w.next(), w.next()) else {
            continue;
        };
        let file = file.trim_start_matches('*');
        if let Some(rest) = file.strip_prefix("nfpm_") {
            if let Some(version) = rest.strip_suffix(ASSET_SUFFIX) {
                if sha.len() == 64
                    && sha.chars().all(|c| c.is_ascii_hexdigit())
                    && !version.is_empty()
                {
                    return Ok(Asset {
                        file: file.into(),
                        sha256: sha.to_ascii_lowercase(),
                        version: version.into(),
                    });
                }
            }
        }
    }
    bail!("checksums.txt lists no nfpm{ASSET_SUFFIX} archive")
}

pub fn download_url(a: &Asset) -> String {
    format!("{}/v{}/{}", link("nfpm-download"), a.version, a.file)
}

/// The hash `sha256sum FILE` printed.
pub fn parse_sha256sum(out: &str) -> Option<String> {
    let h = out.split_whitespace().next()?;
    (h.len() == 64).then(|| h.to_ascii_lowercase())
}

pub fn bin_dir() -> PathBuf {
    crate::paths::home().join(".local/bin")
}

fn curl(url: &str, dest: Option<&Path>) -> Result<String> {
    let mut args = vec!["-fsSL", "--retry", "2", url];
    let d;
    if let Some(p) = dest {
        d = p.to_string_lossy().into_owned();
        args.extend(["-o", &d]);
    }
    crate::sys::output("curl", &args)
}

pub fn fetch() -> Result<()> {
    let sums = curl(link("nfpm-checksums"), None).context("could not download checksums.txt")?;
    let asset = parse_checksums(&sums)?;
    let dir = crate::paths::cache_dir().join("nfpm");
    std::fs::create_dir_all(&dir)?;
    let tar = dir.join(&asset.file);
    println!(
        "downloading nfpm {} ({})",
        asset.version,
        download_url(&asset)
    );
    curl(&download_url(&asset), Some(&tar))?;
    let got = parse_sha256sum(&crate::sys::output("sha256sum", &[&tar.to_string_lossy()])?)
        .context("could not read sha256sum's answer")?;
    if got != asset.sha256 {
        let _ = std::fs::remove_file(&tar);
        bail!(
            "checksum mismatch for {}: checksums.txt says {}, the download is {got}; nothing was installed",
            asset.file,
            asset.sha256
        );
    }
    println!("sha256 matches checksums.txt: {got}");
    let bin = bin_dir();
    std::fs::create_dir_all(&bin)?;
    crate::sys::output(
        "tar",
        &[
            "-xzf",
            &tar.to_string_lossy(),
            "-C",
            &bin.to_string_lossy(),
            "nfpm",
        ],
    )?;
    println!("installed {}", bin.join("nfpm").display());
    if !std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|p| Path::new(p) == bin)
    {
        println!("note: {} is not on your PATH yet", bin.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &str = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  nfpm_2.43.0_Darwin_arm64.tar.gz
0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF  nfpm_2.43.0_Linux_x86_64.tar.gz
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  nfpm_2.43.0_Linux_arm64.tar.gz
";

    #[test]
    fn picks_the_linux_x86_64_archive_and_its_version() {
        let a = parse_checksums(SUMS).unwrap();
        assert_eq!(a.file, "nfpm_2.43.0_Linux_x86_64.tar.gz");
        assert_eq!(a.version, "2.43.0");
        assert_eq!(
            a.sha256,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            download_url(&a),
            "https://github.com/goreleaser/nfpm/releases/download/v2.43.0/nfpm_2.43.0_Linux_x86_64.tar.gz"
        );
    }

    #[test]
    fn a_list_without_the_archive_or_with_a_short_hash_is_refused() {
        assert!(parse_checksums("deadbeef  nfpm_2.43.0_Linux_x86_64.tar.gz\n").is_err());
        assert!(parse_checksums("").is_err());
    }

    #[test]
    fn sha256sum_output_is_read() {
        let h = "0".repeat(64);
        assert_eq!(parse_sha256sum(&format!("{h}  file\n")), Some(h));
        assert_eq!(parse_sha256sum("oops"), None);
    }
}
