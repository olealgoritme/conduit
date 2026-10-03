//! Compiles the C half: the GPU pipeline (csrc/*.c) and the vendored ENet
//! (Moonlight's fork) and nanors (Reed-Solomon), both MIT, unchanged.

fn main() {
    let tp = "csrc/third_party";
    println!("cargo:rerun-if-changed=csrc");

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

    cc::Build::new()
        .file("csrc/net_shim.c")
        .include(format!("{tp}/enet/include"))
        .include(format!("{tp}/nanors"))
        .include(format!("{tp}/nanors/deps/obl"))
        .compile("netshim");

    let mut gpu = cc::Build::new();
    gpu.files(["csrc/gpu.c", "csrc/enc.c", "csrc/dec.c"])
        .include("csrc")
        .include(format!("{tp}/nvcodec"))
        .flag("-std=gnu11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers");
    for lib in ["egl", "gbm"] {
        let l = pkg_config::Config::new()
            .cargo_metadata(true)
            .probe(lib)
            .unwrap_or_else(|e| panic!("{lib} development files are needed: {e}"));
        for p in l.include_paths {
            gpu.include(p);
        }
    }
    gpu.compile("csgpu");
    println!("cargo:rustc-link-lib=dl");
}
