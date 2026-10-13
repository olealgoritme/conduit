//! Compiles the C half: the GPU pipeline (csrc/*.c) and the vendored ENet
//! (Moonlight's fork) and nanors (Reed-Solomon), both MIT, unchanged.

fn main() {
    conduit_build_id(&["src"]);
    let tp = "csrc/third_party";
    println!("cargo:rerun-if-changed=csrc");

    // Archives are listed to the linker in the order compiled, and ld.bfd
    // (the Nix toolchain's; rust-lld does not care) resolves left to right:
    // each user comes before what it uses.
    cc::Build::new()
        .file("csrc/net_shim.c")
        .include(format!("{tp}/enet/include"))
        .include(format!("{tp}/nanors"))
        .include(format!("{tp}/nanors/deps/obl"))
        .compile("netshim");

    let mut enet = cc::Build::new();
    enet.include(format!("{tp}/enet/include")).warnings(false);
    for f in [
        "callbacks",
        "compress",
        "host",
        "list",
        "packet",
        "peer",
        "protocol",
        "unix",
    ] {
        enet.file(format!("{tp}/enet/{f}.c"));
    }
    enet.compile("enet");

    cc::Build::new()
        .file(format!("{tp}/nanors/rs.c"))
        .file(format!("{tp}/nanors/deps/obl/oblas_common.c"))
        .file(format!("{tp}/nanors/deps/obl/oblas_lite.c"))
        .include(format!("{tp}/nanors"))
        .include(format!("{tp}/nanors/deps/obl"))
        .warnings(false)
        .compile("nanors");

    let mut gpu = cc::Build::new();
    gpu.files(["csrc/gpu.c", "csrc/enc.c", "csrc/dec.c"])
        .include("csrc")
        .include(format!("{tp}/nvcodec"))
        .flag("-std=gnu11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers");
    // Probed without cargo metadata so -lEGL/-lgbm follow libcsgpu.a.
    let mut libs = Vec::new();
    for lib in ["egl", "gbm"] {
        let l = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(lib)
            .unwrap_or_else(|e| panic!("{lib} development files are needed: {e}"));
        for p in &l.include_paths {
            gpu.include(p);
        }
        libs.push(l);
    }
    gpu.compile("csgpu");
    for l in libs {
        for p in l.link_paths {
            println!("cargo:rustc-link-search=native={}", p.display());
        }
        for n in l.libs {
            println!("cargo:rustc-link-lib={n}");
        }
    }
    println!("cargo:rustc-link-lib=dl");
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
