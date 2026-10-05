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
