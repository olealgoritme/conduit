//! Embeds the Conduit icon (res/conduit.ico, resource 1) in the Windows exe:
//! the tray, its windows and the context menu command (which names
//! `conduit-gpu-tray.exe,-1`) all use it.
//!
//! GNU targets compile res/app.rc with windres (`WINDRES`, default
//! `x86_64-w64-mingw32-windres`), MSVC targets with the Windows SDK's rc.exe
//! (`RC`, default `rc.exe` on PATH). Without the tool the exe builds without
//! an icon, with a warning; in CI (`CI` set, as GitHub Actions does) or with
//! `CONDUIT_REQUIRE_ICON` set that is a build error instead.
fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/conduit.ico");
    for v in ["WINDRES", "RC", "CI", "CONDUIT_REQUIRE_ICON"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let (tool, out, status) = if msvc {
        let rc = std::env::var("RC").unwrap_or_else(|_| "rc.exe".into());
        let out = out_dir.join("app.res");
        let s = std::process::Command::new(&rc)
            .args(["/nologo", "/i", "res", "/fo"])
            .arg(&out)
            .arg("res/app.rc")
            .status();
        (rc, out, s)
    } else {
        let windres =
            std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
        let out = out_dir.join("app.o");
        let s = std::process::Command::new(&windres)
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
            .status();
        (windres, out, s)
    };
    if status.map(|s| s.success()).unwrap_or(false) {
        println!("cargo:rustc-link-arg-bins={}", out.display());
        return;
    }
    let required = ["CI", "CONDUIT_REQUIRE_ICON"]
        .iter()
        .any(|v| std::env::var(v).is_ok_and(|s| !s.is_empty() && s != "0" && s != "false"));
    if required {
        panic!("{tool} not found or failed: the exe would have no icon (set RC or WINDRES)");
    }
    println!("cargo:warning={tool} not found or failed: the exe has no icon");
}
