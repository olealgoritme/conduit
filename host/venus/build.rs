//! Links virglrenderer for the `renderer` feature; does nothing otherwise, so
//! the backend's dependency on this crate needs neither virglrenderer nor
//! Vulkan.
//!
//! Found through the `pkg-config` tool rather than a build-dependency crate,
//! to keep the default build free of extra crates. Point `PKG_CONFIG_PATH` at
//! a local build (build-virglrenderer.sh prints the path); when nothing is
//! set and that local build exists, it is used.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=CONDUIT_VENUS_RPATH");
    if std::env::var_os("CARGO_FEATURE_RENDERER").is_none() {
        return;
    }
    // Only the renderer binary prints it; the library the backend links
    // stays independent of the commit.
    conduit_build_id(&["src"]);

    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/build/install/lib/pkgconfig");
    let mut cmd = Command::new("pkg-config");
    cmd.args(["--libs-only-L", "--libs-only-l", "virglrenderer"]);
    if std::env::var_os("PKG_CONFIG_PATH").is_none() && local.is_dir() {
        cmd.env("PKG_CONFIG_PATH", &local);
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => panic!(
            "pkg-config virglrenderer failed: {}\nbuild it with host/venus/build-virglrenderer.sh \
             and set PKG_CONFIG_PATH",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => panic!("running pkg-config: {e}"),
    };
    for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
            // A private build is not on the loader path; an rpath lets the
            // binary run without LD_LIBRARY_PATH. Harmless for a system one.
            // Packages ship the library elsewhere and set CONDUIT_VENUS_RPATH
            // (packaging/build.sh venus: $ORIGIN/../lib).
            let rpath = std::env::var("CONDUIT_VENUS_RPATH").unwrap_or_else(|_| dir.to_string());
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{rpath}");
        } else if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }
}

/// Build identity for `--version`: `VERSION (git COMMIT, KIND)`, from
/// packaging/build-id.sh (which documents the fields) as the rustc env
/// CONDUIT_BUILD_ID. Builds without the script (a Nix source subset) use the
/// same environment variables, else the crate version. `watch` lists the
/// package directories whose edits re-run this, so "-dirty" stays current.
/// Returns the script's output for any further fields. Kept in step with the
/// other Conduit build scripts that carry a copy of it.
fn conduit_build_id(watch: &[&str]) -> String {
    use std::{env, path::Path, process::Command};
    for v in ["CONDUIT_VERSION", "CONDUIT_GIT_COMMIT", "CONDUIT_BUILD_KIND"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    for d in watch {
        println!("cargo:rerun-if-changed={d}");
    }
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let script = Path::new(&dir).ancestors().map(|d| d.join("packaging/build-id.sh")).find(|p| p.is_file());
    let out = script
        .and_then(|s| {
            println!("cargo:rerun-if-changed={}", s.display());
            Command::new("sh").arg(&s).arg("--watch").output().ok()
        })
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for p in out.lines().filter_map(|l| l.strip_prefix("watch=")) {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let var = |k: &str, or: &str| env::var(k).unwrap_or_else(|_| or.to_string());
    let id = out.lines().find_map(|l| l.strip_prefix("id=")).map(str::to_string).unwrap_or_else(|| {
        format!(
            "{} (git {}, {})",
            var("CONDUIT_VERSION", &var("CARGO_PKG_VERSION", "0.0.0")),
            var("CONDUIT_GIT_COMMIT", "unknown"),
            var("CONDUIT_BUILD_KIND", "local")
        )
    });
    println!("cargo:rustc-env=CONDUIT_BUILD_ID={id}");
    out
}
