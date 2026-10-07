//! Host findings as steps: one per `doctor::Check`, with the fix the wizard
//! can offer. Debian and Ubuntu get commands to run; every other distro gets
//! the same words as a guide, with its own package names. The wizard never
//! touches the NVIDIA driver.

use super::data::*;
use super::{CheckResult, Env, Family, Fix, Step, Verify};
use crate::doctor::{Check, Level, Remedy};

/// The step for one doctor check.
pub fn step_for(c: &Check, env: &Env) -> Step {
    let (id, title) = (c.id.clone(), c.title.clone());
    let lookup = (id.clone(), title.clone());
    Step::new(
        &id,
        &title,
        &c.hint(),
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

/// The wizard's fix for a check that is not ok: its own remedy, resolved for
/// this host's distribution. None when the check says nothing to do.
pub fn fix_for(c: &Check, env: &Env) -> Option<Fix> {
    c.remedy.as_ref().map(|r| fix_of(r, env))
}

/// A remedy as something the wizard can offer: commands on Debian, the same
/// words elsewhere.
pub fn fix_of(r: &Remedy, env: &Env) -> Fix {
    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    match r {
        Remedy::Guide { text } | Remedy::Explain { guide: text, .. } => Fix::guide(text.clone()),
        Remedy::Sudo { cmd, .. } => sudo_fix(env, &strs(cmd)),
        Remedy::Install {
            apt, dnf, pacman, ..
        } => install_fix(env, &strs(apt), &strs(dnf), &strs(pacman)),
    }
}
