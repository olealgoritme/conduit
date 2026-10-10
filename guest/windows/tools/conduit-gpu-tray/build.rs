//! Embeds the Conduit icon (res/conduit.ico, resource 1) in the Windows exe.
fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/conduit.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.o");
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let ok = std::process::Command::new(&windres)
        .args([
            "--input-format=rc",
            "-O",
            "coff",
            "-I",
            "res",
            "res/app.rc",
            "-o",
        ])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning={windres} not found or failed: the exe has no icon");
    }
}
