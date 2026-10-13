//! Build identity for `--version` (packaging/build-id.sh).

fn main() {
    conduit_build_id(&["src", "bin"]);
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
    for v in [
        "CONDUIT_VERSION",
        "CONDUIT_GIT_COMMIT",
        "CONDUIT_BUILD_KIND",
    ] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    for d in watch {
        println!("cargo:rerun-if-changed={d}");
    }
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let script = Path::new(&dir)
        .ancestors()
        .map(|d| d.join("packaging/build-id.sh"))
        .find(|p| p.is_file());
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
    let id = out
        .lines()
        .find_map(|l| l.strip_prefix("id="))
        .map(str::to_string)
        .unwrap_or_else(|| {
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
