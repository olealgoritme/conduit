//! The Conduit BIOS: the distro's UEFI firmware (edk2 OVMF) rebuilt with the
//! Conduit boot logo, shipped in the optional `conduit-bios` package
//! (packaging/bios). `conduit attach` points a VM's <loader> at it when the VM
//! uses the stock firmware variant it was built to match (Debian/Ubuntu's
//! ovmf, the same edk2 build); the VM's NVRAM vars file stays as it is (same
//! layout), so boot entries and Secure Boot keys carry over. A TPM measures
//! the firmware (PCR0), so a key sealed to the TPM (BitLocker, LUKS with a TPM
//! token) asks for its recovery once after the swap ([`TPM_WARNING`]).
//! `conduit detach` puts the stock loader back.

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

/// Where the package puts the images, in lookup order after $CONDUIT_BIOS_DIR
/// (which points a development build at packaging/bios/build.sh's output).
const DIRS: &[&str] = &["/usr/share/conduit/bios", "/opt/conduit/share/conduit/bios"];

/// The directories [`installed_dir`] looks in, in order. Only installed
/// locations (and an explicit $CONDUIT_BIOS_DIR): a firmware a VM boots from
/// must not live in a source checkout's build output.
fn candidates(env: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = env.into_iter().map(PathBuf::from).collect();
    v.extend(DIRS.iter().map(PathBuf::from));
    v.push(crate::paths::prefix().join("share/conduit/bios"));
    v
}

/// The directory holding the installed Conduit BIOS, if any.
pub fn installed_dir() -> Option<PathBuf> {
    candidates(std::env::var_os("CONDUIT_BIOS_DIR"))
        .into_iter()
        .find(|d| d.join(PLAIN).is_file())
}

/// What a firmware swap means for a VM with a TPM.
pub const TPM_WARNING: &str = "This VM has a TPM, and changing its firmware changes the TPM's boot measurement (PCR0): \
a disk key sealed to the TPM (BitLocker, LUKS with a TPM2 token) asks for its recovery key once at the next boot. \
In Windows, suspend BitLocker before restarting the VM: manage-bde -protectors -disable C: -RebootCount 1";

/// The Conduit image matching a stock loader path.
pub fn image_for(stock: &str) -> Option<&'static str> {
    STOCK.iter().find(|(s, _)| *s == stock).map(|(_, c)| *c)
}

/// Is this loader path a Conduit BIOS image (wherever it is installed)?
pub fn is_ours(loader: &str) -> bool {
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

/// The stock loader a Conduit BIOS loader stands in for: the one an earlier
/// attach recorded, else the stock image of the same variant.
pub fn stock_for(cur: &str, recorded: Option<&str>) -> String {
    recorded
        .map(str::to_string)
        .or_else(|| {
            STOCK
                .iter()
                .find(|(_, c)| Path::new(cur).file_name().is_some_and(|n| n == *c))
                .map(|(s, _)| s.to_string())
        })
        .unwrap_or_else(|| STOCK[0].0.to_string())
}

/// Decide the loader. `loader` is the domain's <loader> path, `pflash`
/// whether it is a pflash loader, `recorded` the stock path an earlier attach
/// remembered, `bios` the installed Conduit BIOS directory, and `fits` whether
/// the installed image may replace a given stock file (`Err` says why not;
/// it is asked again for a VM already on the Conduit BIOS, so a missing or
/// mismatched image puts the stock loader back).
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
        // Attached before: keep it (moved to the current install) while it
        // still fits, or go back to the stock loader.
        let stock = stock_for(cur, recorded);
        let name = Path::new(cur)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let back = |why: String| Plan {
            note: Some(format!(
                "{why}: the VM boots its stock firmware again ({stock})."
            )),
            loader: Some(stock.clone()),
            stock: None,
        };
        return match bios {
            Some(d) => {
                let want = d.join(&name);
                match fits(&stock, &want) {
                    Ok(()) => {
                        let want = want.display().to_string();
                        Plan {
                            loader: (want != cur).then_some(want),
                            stock: Some(stock),
                            note: None,
                        }
                    }
                    Err(why) => back(format!("The Conduit BIOS no longer fits ({why})")),
                }
            }
            None => back("The Conduit BIOS is no longer installed".into()),
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
            "Boots with the Conduit BIOS ({img}); its NVRAM vars file (boot entries, Secure Boot keys) is kept."
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

/// The Ubuntu ovmf build the images were made from: OVMF_VERSION next to
/// them (packaging/bios/build.sh), else the pin this conduit was built with.
fn built_for(conduit: &Path) -> String {
    conduit
        .parent()
        .and_then(|d| std::fs::read_to_string(d.join("OVMF_VERSION")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| pinned_ovmf().to_string())
}

/// EDK2_DEB_VERSION from packaging/bios/version.sh.
fn pinned_ovmf() -> &'static str {
    include_str!("../../packaging/bios/version.sh")
        .lines()
        .find_map(|l| l.strip_prefix("EDK2_DEB_VERSION="))
        .map(str::trim)
        .expect("packaging/bios/version.sh sets EDK2_DEB_VERSION")
}

/// The host's ovmf package version (Debian/Ubuntu), if dpkg knows one.
fn host_ovmf() -> Option<String> {
    crate::sys::output("dpkg-query", &["-W", "-f=${Version}", "ovmf"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Is the host's stock firmware (ovmf `host`) the edk2 build the Conduit
/// images were made from (`built`, e.g. 2024.02-2ubuntu0.9)? The same
/// upstream release and Ubuntu packaging base ("2024.02-2ubuntu"): Ubuntu's
/// stable updates (0.9 -> 0.10) keep the flash and varstore layout, another
/// upstream release or Debian's own build (other flags) may not.
pub fn same_build(host: Option<&str>, built: &str) -> Result<(), String> {
    let base = |v: &str| {
        v.find("ubuntu")
            .map(|i| v[..i + "ubuntu".len()].to_string())
    };
    let Some(host) = host else {
        return Err(
            "the stock firmware is not Debian/Ubuntu's ovmf package (dpkg does not list it)".into(),
        );
    };
    match (base(host), base(built)) {
        (Some(h), Some(b)) if h == b => Ok(()),
        _ => Err(format!(
            "the stock firmware is ovmf {host}, a different edk2 build than the Conduit BIOS's ({built})"
        )),
    }
}

/// The real check behind `fits`. The Conduit image must exist, the host's
/// ovmf must be the edk2 build it was made from ([`same_build`]), and an
/// existing stock file must have the same size (same flash layout). With
/// firmware='efi' libvirt only accepts a loader some firmware descriptor
/// names; the conduit-bios package installs them (packaging/bios/firmware).
pub fn check(stock: &str, conduit: &Path, autoselect: bool) -> Result<(), String> {
    check_with(stock, conduit, autoselect, host_ovmf().as_deref())
}

fn check_with(
    stock: &str,
    conduit: &Path,
    autoselect: bool,
    host: Option<&str>,
) -> Result<(), String> {
    let c = std::fs::metadata(conduit).map_err(|e| format!("{}: {e}", conduit.display()))?;
    same_build(host, &built_for(conduit))?;
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
    fn reattach_falls_back_to_stock_when_the_image_no_longer_fits() {
        // A missing image (the package half removed, a moved install) or a
        // different build: the stock loader comes back, with the reason.
        let cur = "/usr/share/conduit/bios/conduit-bios.secboot.fd";
        let rec = Some("/usr/share/OVMF/OVMF_CODE_4M.ms.fd");
        let p = plan(Some(cur), true, rec, Some(Path::new(DIR)), &no);
        assert_eq!(p.loader.as_deref(), rec);
        assert_eq!(p.stock, None);
        let note = p.note.unwrap();
        assert!(note.contains("different build"), "{note}");
        assert!(note.contains("OVMF_CODE_4M.ms.fd"), "{note}");
        // The real check: an image that is not there does not fit.
        let gone = Path::new("/nonexistent/conduit-bios/conduit-bios.fd");
        let p = plan(
            Some(cur),
            true,
            rec,
            Some(Path::new("/nonexistent/conduit-bios")),
            &|s, i| check_with(s, i, false, Some("2024.02-2ubuntu0.9")),
        );
        assert_eq!(p.loader.as_deref(), rec, "{p:?}");
        assert!(check_with("/x", gone, false, Some("2024.02-2ubuntu0.9")).is_err());
    }

    #[test]
    fn the_bios_is_only_looked_for_where_it_is_installed() {
        let c = candidates(Some("/dev/bios".into()));
        assert_eq!(c[0], PathBuf::from("/dev/bios"), "$CONDUIT_BIOS_DIR first");
        assert!(c.contains(&PathBuf::from("/usr/share/conduit/bios")));
        assert!(
            !c.iter().any(|d| d.to_string_lossy().contains("target/")),
            "no build output of a source checkout: {c:?}"
        );
        if let Some(r) = crate::paths::repo_root() {
            assert!(!candidates(None).iter().any(|d| d.starts_with(&r)), "{c:?}");
        }
    }

    #[test]
    fn only_the_same_edk2_build_is_swapped() {
        let built = "2024.02-2ubuntu0.9";
        assert_eq!(same_build(Some("2024.02-2ubuntu0.9"), built), Ok(()));
        assert_eq!(
            same_build(Some("2024.02-2ubuntu0.11"), built),
            Ok(()),
            "an Ubuntu stable update of the same build"
        );
        for other in [
            "2025.02-3ubuntu1",   // a newer Ubuntu release
            "2024.02-3ubuntu0.1", // another packaging base
            "2024.02-2",          // Debian's own build
            "2025.02-8",          // Debian trixie
        ] {
            let e = same_build(Some(other), built).unwrap_err();
            assert!(e.contains(other), "{e}");
        }
        assert!(same_build(None, built)
            .unwrap_err()
            .contains("not Debian/Ubuntu"));
        // check() refuses a matching file size from another build.
        let d = std::env::temp_dir().join(format!("conduit-bios-build-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let img = d.join(PLAIN);
        let stock = d.join("OVMF_CODE_4M.fd");
        std::fs::write(&img, [0u8; 64]).unwrap();
        std::fs::write(&stock, [1u8; 64]).unwrap();
        std::fs::write(d.join("OVMF_VERSION"), "2024.02-2ubuntu0.9\n").unwrap();
        let s = stock.to_str().unwrap();
        assert_eq!(
            check_with(s, &img, false, Some("2024.02-2ubuntu0.10")),
            Ok(())
        );
        assert!(check_with(s, &img, false, Some("2025.02-8")).is_err());
        assert!(check_with(s, &img, false, None).is_err());
        std::fs::remove_dir_all(&d).unwrap();
        // Without OVMF_VERSION (an older package) the compiled-in pin counts.
        assert!(pinned_ovmf().contains("ubuntu"), "{}", pinned_ovmf());
    }

    #[test]
    fn the_tpm_warning_says_what_to_do() {
        assert!(TPM_WARNING.contains("PCR0"));
        assert!(TPM_WARNING.contains("manage-bde -protectors -disable C: -RebootCount 1"));
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
