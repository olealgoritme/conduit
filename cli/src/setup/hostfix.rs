//! Host findings as steps: one per `doctor::Check`, with the fix the wizard
//! can offer. Debian and Ubuntu get commands to run; every other distro gets
//! the same words as a guide, with its own package names. The wizard never
//! touches the NVIDIA driver.

use super::data::*;
use super::{CheckResult, Env, Family, Fix, Step, Verify};
use crate::doctor::{Check, Level};

/// The step for one doctor check.
pub fn step_for(c: &Check, env: &Env) -> Step {
    let (id, title) = (c.id.clone(), c.title.clone());
    let lookup = (id.clone(), title.clone());
    Step::new(
        &id,
        &title,
        &c.hint,
        move |env: &Env| match env
            .checks
            .iter()
            .find(|c| c.id == lookup.0 && c.title == lookup.1)
        {
            Some(c) => CheckResult::from_level(c.level, &c.detail),
            None => CheckResult::Done("gone".into()),
        },
        (c.level != Level::Ok).then(|| fix_for(c, env)).flatten(),
        Verify::Recheck,
    )
}

/// Every host step: the doctor's checks, then the source-checkout extras.
pub fn host_steps(env: &Env) -> Vec<Step> {
    let mut v: Vec<Step> = env.checks.iter().map(|c| step_for(c, env)).collect();
    if env.source_checkout {
        v.push(Step::new(
            "build-deps",
            "Build packages",
            "Building Conduit from this source checkout needs these libraries and tools (the list packaging/build.sh deps installs).",
            |env: &Env| {
                if env.distro.family() != Family::Debian {
                    CheckResult::Warn("not checked on this distro".into())
                } else if env.build_deps_missing.is_empty() {
                    CheckResult::Done("all installed".into())
                } else {
                    CheckResult::Warn(format!("{} missing", env.build_deps_missing.len()))
                }
            },
            Some({
                // Only what is missing, when the host can tell.
                let apt: Vec<&str> = if env.build_deps_missing.is_empty() {
                    BUILD_DEPS_APT.to_vec()
                } else {
                    env.build_deps_missing.iter().map(|s| s.as_str()).collect()
                };
                install_fix(env, &apt, BUILD_DEPS_DNF, BUILD_DEPS_PACMAN)
            }),
            Verify::Recheck,
        ));
        v.push(Step::new(
            "nfpm",
            "nfpm",
            "nfpm builds the guest driver packages (`conduit create` and `conduit attach` need them in a source checkout). The download is checked against the release's checksums.txt.",
            |env: &Env| {
                if env.nfpm {
                    CheckResult::Done("installed".into())
                } else {
                    CheckResult::Warn("not installed".into())
                }
            },
            Some(Fix::run(&["conduit", "setup", "fetch-nfpm"], false)),
            Verify::Recheck,
        ));
    }
    v
}

/// "Install these packages": a command on Debian, words elsewhere.
pub fn install_fix(env: &Env, apt: &[&str], dnf: &[&str], pacman: &[&str]) -> Fix {
    match env.distro.family() {
        Family::Debian => {
            let mut cmd = vec!["apt-get".to_string(), "install".into(), "-y".into()];
            cmd.extend(apt.iter().map(|s| s.to_string()));
            Fix::Run { cmd, needs_sudo: true }
        }
        Family::Fedora => Fix::guide(format!("Run this in a terminal:\n  sudo dnf install {}", dnf.join(" "))),
        Family::Arch => Fix::guide(format!("Run this in a terminal:\n  sudo pacman -S --needed {}", pacman.join(" "))),
        Family::Other => Fix::guide(format!(
            "Install your distribution's equivalents of these packages. Debian/Ubuntu names:\n  {}\nFedora: {}\nArch: {}",
            apt.join(" "),
            dnf.join(" "),
            pacman.join(" ")
        )),
    }
}

/// A command only an administrator can run: automated on Debian, words elsewhere.
pub fn sudo_fix(env: &Env, cmd: &[&str]) -> Fix {
    if env.distro.family() == Family::Debian {
        Fix::run(cmd, true)
    } else {
        Fix::guide(format!("Run this in a terminal:\n  sudo {}", cmd.join(" ")))
    }
}

pub fn libvirt_fix(env: &Env) -> Fix {
    install_fix(env, LIBVIRT_APT, LIBVIRT_DNF, LIBVIRT_PACMAN)
}

/// The fix for a check that is not ok; None when the doctor's hint says all there is.
pub fn fix_for(c: &Check, env: &Env) -> Option<Fix> {
    let hint = || (!c.hint.is_empty()).then(|| Fix::guide(c.hint.clone()));
    match c.id.as_str() {
        "kvm-access" => Some(sudo_fix(env, &["usermod", "-aG", "kvm", &env.user])),
        "nvidia-driver" | "driver-support" => Some(Fix::guide(driver_guide(env))),
        "tools" => Some(install_fix(
            env,
            crate::doctor::TOOL_PACKAGES_APT,
            &["iproute", "iptables", "openssh-clients", "curl", "e2fsprogs", "xz", "coreutils"],
            &["iproute2", "iptables", "openssh", "curl", "e2fsprogs", "xz", "coreutils"],
        )),
        "virtiofsd" => Some(install_fix(env, &["virtiofsd"], &["virtiofsd"], &["virtiofsd"])),
        "conduit-part" => Some(Fix::guide(format!(
            "Install the Conduit package for your distribution ({}), or, from a source checkout, build it with packaging/build.sh (the build packages step above installs what it needs).",
            link("conduit-releases")
        ))),
        _ => hint(),
    }
}

/// What to know before touching the driver. The wizard changes nothing here.
pub fn driver_guide(env: &Env) -> String {
    let now = match &env.driver {
        Some(d) => format!(
            "Loaded now: {} ({} kernel modules).",
            d.version,
            if d.open { "open" } else { "closed" }
        ),
        None => "No NVIDIA driver is loaded now.".into(),
    };
    format!(
        "{now}\n\n\
         Conduit never installs or changes a driver for you. Do it yourself, in your distribution's way, then restart the computer: the new kernel module only loads at boot.\n\n\
         Open or closed: NVIDIA ships its kernel modules in two flavours. Conduit is built and tested on the OPEN modules, release 580 or newer (Ubuntu: the nvidia-driver-580-open package or a newer -open one). The closed modules and older branches may work for a release Conduit has tables for; Conduit then warns and starts in safe mode.\n\n\
         Releases Conduit knows: {}",
        if env.supported.is_empty() {
            "(the list could not be read)".to_string()
        } else {
            env.supported.join(", ")
        }
    )
}
