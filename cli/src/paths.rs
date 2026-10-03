//! Where things live: state directories and the helper programs Conduit runs.
//!
//! Installed layout ($CONDUIT_PREFIX, default /opt/conduit):
//!   bin/conduit-backend   bin/conduit-vmm   bin/conduit-viewer   bin/conduit-userspace
//!   bin/qemu-system-x86_64              bundled QEMU 11.1, the default VM runner
//!   share/conduit/guest/conduit-guest.deb the guest driver (DKMS) `conduit create` installs
//!   share/conduit/supported-drivers.txt  host driver versions the backend speaks
//! In a source checkout the build outputs are used instead (see `Tool::candidates`).

use crate::ui::oops;
use anyhow::Result;
use std::env;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn username() -> String {
    env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            crate::sys::output("id", &["-un"])
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "user".into())
        })
}

/// ~/.config/conduit
pub fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("conduit")
}

/// ~/.local/share/conduit
pub fn data_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"))
        .join("conduit")
}

/// ~/.cache/conduit (downloaded images, staged driver share)
pub fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".cache"))
        .join("conduit")
}

pub fn vms_dir() -> PathBuf {
    data_dir().join("vms")
}

pub fn vm_dir(name: &str) -> PathBuf {
    vms_dir().join(name)
}

/// /run/user/$UID
pub fn xdg_runtime() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uid())))
}

/// /run/user/$UID/conduit/NAME
pub fn run_dir(name: &str) -> PathBuf {
    xdg_runtime().join("conduit").join(name)
}

pub fn ssh_key() -> PathBuf {
    config_dir().join("ssh").join("id_ed25519")
}

pub fn prefix() -> PathBuf {
    env::var_os("CONDUIT_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/conduit"))
}

/// The source checkout, when running from one (dev builds).
pub fn repo_root() -> Option<PathBuf> {
    if let Some(r) = env::var_os("CONDUIT_REPO") {
        return Some(PathBuf::from(r));
    }
    let is_repo =
        |p: &Path| p.join("cli/Cargo.toml").is_file() && p.join("docs/STRUCTURE.md").is_file();
    // Compiled-in location of this crate (cli/..).
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    if is_repo(&built) {
        return built.canonicalize().ok();
    }
    // Walk up from the running binary (target/release/conduit -> repo).
    let exe = env::current_exe().ok()?;
    exe.ancestors().find(|p| is_repo(p)).map(Path::to_path_buf)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Backend,
    Vmm,
    Viewer,
    Userspace,
    GuestDeb,
    BundledQemu,
    Stream,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Backend => "GPU backend",
            Tool::Vmm => "built-in VM runner",
            Tool::Viewer => "viewer",
            Tool::Userspace => "driver share tool",
            Tool::GuestDeb => "guest driver package (conduit-guest .deb)",
            Tool::BundledQemu => "bundled QEMU",
            Tool::Stream => "stream host (conduit-stream)",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Tool::Backend => "CONDUIT_BACKEND",
            Tool::Vmm => "CONDUIT_VMM",
            Tool::Viewer => "CONDUIT_VIEWER",
            Tool::Userspace => "CONDUIT_USERSPACE",
            Tool::GuestDeb => "CONDUIT_GUEST_DEB",
            Tool::BundledQemu => "CONDUIT_QEMU",
            Tool::Stream => "CONDUIT_STREAM",
        }
    }

    /// Where to look, in order: env override, install prefix, source checkout.
    pub fn candidates(self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(p) = env::var_os(self.env_var()) {
            v.push(PathBuf::from(p));
        }
        let pf = prefix();
        let (installed, dev): (&[&str], &[&str]) = match self {
            Tool::Backend => (
                &["bin/conduit-backend"],
                &[
                    "target/release/conduit-backend",
                    "host/backend/target/release/conduit-backend",
                ],
            ),
            Tool::Vmm => (
                &["bin/conduit-vmm"],
                &[
                    "target/release/conduit-vmm",
                    "host/vmm/target/release/conduit-vmm",
                ],
            ),
            Tool::Viewer => (&["bin/conduit-viewer"], &["host/viewer/conduit-viewer"]),
            Tool::Userspace => (
                &["bin/conduit-userspace"],
                &[
                    "target/release/conduit-userspace",
                    "host/backend/target/release/conduit-userspace",
                ],
            ),
            // In a checkout: `packaging/build.sh package guest-deb` (newest wins, below).
            Tool::GuestDeb => (&["share/conduit/guest/conduit-guest.deb"], &[]),
            Tool::BundledQemu => (
                &["bin/qemu-system-x86_64"],
                &["host/qemu/build/qemu-system-x86_64"],
            ),
            Tool::Stream => (
                &["bin/conduit-stream"],
                &["host/stream/target/release/conduit-stream"],
            ),
        };
        v.extend(installed.iter().map(|p| pf.join(p)));
        if let Some(r) = repo_root() {
            v.extend(dev.iter().map(|p| r.join(p)));
            if self == Tool::GuestDeb {
                v.extend(newest_guest_deb(&r.join("dist/out")));
            }
        }
        v
    }

    pub fn find(self) -> Option<PathBuf> {
        self.candidates().into_iter().find(|p| p.is_file())
    }

    /// Find it, or explain what is missing and how to get it.
    pub fn require(self) -> Result<PathBuf> {
        self.find().ok_or_else(|| {
            let looked: Vec<String> = self
                .candidates()
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect();
            oops(
                format!("could not find the {}", self.label()),
                format!(
                    "Conduit looked in:\n{}\nReinstall the conduit package, or in a source checkout build it first.\nYou can also point to it with {}=/path/to/file",
                    looked.join("\n"),
                    self.env_var()
                ),
            )
        })
    }
}

/// The newest conduit-guest_*_all.deb in `dir` (by modification time).
fn newest_guest_deb(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("conduit-guest_") && n.ends_with("_all.deb")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// The kernel's process name for a binary (`comm`, at most 15 bytes).
pub fn comm_of(p: &Path) -> String {
    let n = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    n.chars().take(15).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_is_truncated() {
        assert_eq!(
            comm_of(Path::new("/x/qemu-system-x86_64")),
            "qemu-system-x86"
        );
        assert_eq!(comm_of(Path::new("/x/conduit-backend")), "conduit-backend");
        assert_eq!(comm_of(Path::new("conduit-vmm")), "conduit-vmm");
    }

    #[test]
    fn env_override_comes_first() {
        // SAFETY: tests in this module do not read this variable concurrently.
        unsafe { env::set_var("CONDUIT_VMM", "/tmp/my-vmm") };
        assert_eq!(Tool::Vmm.candidates()[0], PathBuf::from("/tmp/my-vmm"));
        unsafe { env::remove_var("CONDUIT_VMM") };
    }
}
