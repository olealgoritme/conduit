//! The Conduit BIOS: the distro's UEFI firmware (edk2 OVMF) rebuilt with the
//! Conduit boot logo, shipped in the optional `conduit-bios` package
//! (packaging/bios). `conduit attach` points a VM's <loader> at it when the VM
//! uses the stock firmware variant it was built to match; the VM's NVRAM vars
//! file stays as it is (same layout), so boot entries and TPM state carry over.
//! `conduit detach` puts the original definition back, stock loader included.

use std::path::{Path, PathBuf};

/// The images in the conduit-bios package.
pub const PLAIN: &str = "conduit-bios.fd";
pub const SECBOOT: &str = "conduit-bios.secboot.fd";

/// Stock loaders (Debian/Ubuntu `ovmf`) and the Conduit BIOS image built with
/// the same flags. Anything else (SeaBIOS, other distros' layouts, 2 MB
/// images, AMD SEV, ...) is left alone.
const STOCK: &[(&str, &str)] = &[
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", PLAIN),
    ("/usr/share/OVMF/OVMF_CODE_4M.secboot.fd", SECBOOT),
    ("/usr/share/OVMF/OVMF_CODE_4M.ms.fd", SECBOOT),
    ("/usr/share/OVMF/OVMF_CODE_4M.snakeoil.fd", SECBOOT),
];

/// Where the package puts the images, in lookup order after $CONDUIT_BIOS_DIR.
const DIRS: &[&str] = &["/usr/share/conduit/bios", "/opt/conduit/share/conduit/bios"];

/// The directory holding the installed Conduit BIOS, if any.
pub fn installed_dir() -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("CONDUIT_BIOS_DIR") {
        v.push(d.into());
    }
    v.extend(DIRS.iter().map(PathBuf::from));
    v.push(crate::paths::prefix().join("share/conduit/bios"));
    if let Some(r) = crate::paths::repo_root() {
        v.push(r.join("target/conduit-bios/out")); // packaging/bios/build.sh
    }
    v.into_iter().find(|d| d.join(PLAIN).is_file())
}

/// The Conduit image matching a stock loader path.
pub fn image_for(stock: &str) -> Option<&'static str> {
    STOCK.iter().find(|(s, _)| *s == stock).map(|(_, c)| *c)
}

/// Is this loader path a Conduit BIOS image (wherever it is installed)?
fn is_ours(loader: &str) -> bool {
    Path::new(loader)
        .file_name()
        .is_some_and(|n| n == PLAIN || n == SECBOOT)
}

/// What attach does with the VM's <loader>.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The new loader path, if it changes.
    pub loader: Option<String>,
    /// The stock loader to remember (Conduit's metadata), while the BIOS is in use.
    pub stock: Option<String>,
    /// One line for the user, if there is something to say.
    pub note: Option<String>,
}

/// Decide the loader. `loader` is the domain's <loader> path, `pflash`
/// whether it is a pflash loader, `recorded` the stock path an earlier attach
/// remembered, `bios` the installed Conduit BIOS directory, and `fits` whether
/// the installed image may replace a given stock file (`Err` says why not).
pub fn plan(
    loader: Option<&str>,
    pflash: bool,
    recorded: Option<&str>,
    bios: Option<&Path>,
    fits: &dyn Fn(&str, &Path) -> Result<(), String>,
) -> Plan {
    let none = |note: Option<String>| Plan {
        loader: None,
        stock: None,
        note,
    };
    let Some(cur) = loader else {
        return none(None); // SeaBIOS, or firmware='efi' without an explicit loader
    };
    if is_ours(cur) {
        // Attached before: keep it (moved to the current install), or go
        // back to the stock loader when the package is gone.
        let stock = recorded
            .map(str::to_string)
            .or_else(|| {
                STOCK
                    .iter()
                    .find(|(_, c)| Path::new(cur).file_name().is_some_and(|n| n == *c))
                    .map(|(s, _)| s.to_string())
            })
            .unwrap_or_else(|| STOCK[0].0.to_string());
        let name = Path::new(cur)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        return match bios {
            Some(d) => {
                let want = d.join(&name).display().to_string();
                Plan {
                    loader: (want != cur).then_some(want),
                    stock: Some(stock),
                    note: None,
                }
            }
            None => Plan {
                note: Some(format!(
                    "The Conduit BIOS is no longer installed: the VM boots its stock firmware again ({stock})."
                )),
                loader: Some(stock),
                stock: None,
            },
        };
    }
    let Some(img) = image_for(cur).filter(|_| pflash) else {
        return none(Some(format!(
            "Firmware {cur} is kept: the Conduit BIOS only replaces the stock 4 MB OVMF images in /usr/share/OVMF."
        )));
    };
    let Some(dir) = bios else {
        return none(Some(
            "Install the conduit-bios package to boot with the Conduit logo (`conduit attach` again picks it up)."
                .into(),
        ));
    };
    let path = dir.join(img);
    if let Err(why) = fits(cur, &path) {
        return none(Some(format!("Firmware {cur} is kept: {why}.")));
    }
    Plan {
        loader: Some(path.display().to_string()),
        stock: Some(cur.to_string()),
        note: Some(format!(
            "Boots with the Conduit BIOS ({img}); its NVRAM vars file is kept."
        )),
    }
}

/// Firmware descriptors libvirt reads (QEMU's firmware.json interop spec).
fn descriptor_dirs() -> Vec<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join(".config"));
    vec![
        "/usr/share/qemu/firmware".into(),
        "/etc/qemu/firmware".into(),
        config.join("qemu/firmware"),
    ]
}

/// The real check behind `fits`. An existing stock file must have the same
/// size as the Conduit image (same flash layout). With firmware='efi'
/// libvirt only accepts a loader some firmware descriptor names; the
/// conduit-bios package installs them (packaging/bios/firmware).
pub fn check(stock: &str, conduit: &Path, autoselect: bool) -> Result<(), String> {
    let c = std::fs::metadata(conduit).map_err(|e| format!("{}: {e}", conduit.display()))?;
    if let Ok(s) = std::fs::metadata(stock) {
        if s.len() != c.len() {
            return Err("it does not match the installed Conduit BIOS build (flash size)".into());
        }
    }
    if autoselect && !described(conduit, &descriptor_dirs()) {
        return Err(format!(
            "no firmware descriptor names {} (reinstall the conduit-bios package)",
            conduit.display()
        ));
    }
    Ok(())
}

/// Does a firmware descriptor in one of `dirs` name this image?
fn described(image: &Path, dirs: &[PathBuf]) -> bool {
    let want = format!("\"{}\"", image.display());
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .any(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains(&want)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = "/usr/share/conduit/bios";
    fn yes(_: &str, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn no(_: &str, _: &Path) -> Result<(), String> {
        Err("different build".into())
    }

    #[test]
    fn stock_plain_and_secboot_are_swapped() {
        let p = plan(
            Some("/usr/share/OVMF/OVMF_CODE_4M.fd"),
            true,
            None,
            Some(Path::new(DIR)),
            &yes,
        );
        assert_eq!(
            p.loader.as_deref(),
            Some("/usr/share/conduit/bios/conduit-bios.fd")
        );
        assert_eq!(p.stock.as_deref(), Some("/usr/share/OVMF/OVMF_CODE_4M.fd"));
        let p = plan(
            Some("/usr/share/OVMF/OVMF_CODE_4M.ms.fd"),
            true,
            None,
            Some(Path::new(DIR)),
            &yes,
        );
        assert_eq!(
            p.loader.as_deref(),
            Some("/usr/share/conduit/bios/conduit-bios.secboot.fd")
        );
        assert_eq!(
            p.stock.as_deref(),
            Some("/usr/share/OVMF/OVMF_CODE_4M.ms.fd")
        );
    }

    #[test]
    fn other_firmware_is_left_alone_with_a_note() {
        for (l, pflash) in [
            ("/usr/share/OVMF/OVMF_CODE.fd", true),
            ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", true),
            ("/usr/share/ovmf/OVMF.amdsev.fd", false),
            ("/usr/share/OVMF/OVMF_CODE_4M.fd", false),
        ] {
            let p = plan(Some(l), pflash, None, Some(Path::new(DIR)), &yes);
            assert_eq!(p.loader, None, "{l}");
            assert!(p.note.unwrap().contains("is kept"), "{l}");
        }
        // SeaBIOS / no explicit loader: nothing to say.
        assert_eq!(
            plan(None, false, None, Some(Path::new(DIR)), &yes),
            Plan {
                loader: None,
                stock: None,
                note: None
            }
        );
        // Same path, different build (size): kept.
        let p = plan(
            Some("/usr/share/OVMF/OVMF_CODE_4M.fd"),
            true,
            None,
            Some(Path::new(DIR)),
            &no,
        );
        assert_eq!(p.loader, None);
        assert!(p.note.unwrap().ends_with("is kept: different build."));
    }

    #[test]
    fn without_the_package_it_hints() {
        let p = plan(
            Some("/usr/share/OVMF/OVMF_CODE_4M.fd"),
            true,
            None,
            None,
            &yes,
        );
        assert_eq!(p.loader, None);
        assert!(p.note.unwrap().contains("conduit-bios package"));
    }

    #[test]
    fn reattach_is_stable_and_restores_when_uninstalled() {
        let cur = "/usr/share/conduit/bios/conduit-bios.fd";
        let rec = Some("/usr/share/OVMF/OVMF_CODE_4M.fd");
        let p = plan(Some(cur), true, rec, Some(Path::new(DIR)), &yes);
        assert_eq!(
            p,
            Plan {
                loader: None,
                stock: rec.map(String::from),
                note: None
            }
        );
        let p = plan(Some(cur), true, rec, None, &yes);
        assert_eq!(p.loader.as_deref(), rec);
        assert_eq!(p.stock, None);
        // Nothing recorded: the matching stock image.
        let p = plan(Some("/x/conduit-bios.secboot.fd"), true, None, None, &yes);
        assert_eq!(
            p.loader.as_deref(),
            Some("/usr/share/OVMF/OVMF_CODE_4M.secboot.fd")
        );
    }

    #[test]
    fn descriptors_are_found_by_image_path() {
        let d = std::env::temp_dir().join(format!("conduit-bios-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("90-conduit-bios.json"),
            r#"{"mapping":{"executable":{"filename":"/usr/share/conduit/bios/conduit-bios.fd"}}}"#,
        )
        .unwrap();
        let dirs = vec![d.clone(), PathBuf::from("/nonexistent")];
        assert!(described(
            Path::new("/usr/share/conduit/bios/conduit-bios.fd"),
            &dirs
        ));
        assert!(!described(
            Path::new("/usr/share/conduit/bios/conduit-bios.secboot.fd"),
            &dirs
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
