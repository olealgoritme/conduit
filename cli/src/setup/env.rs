//! What the wizard knows about this computer. Read once by [`Env::detect`]
//! and again after every command; tests build one by hand.

use crate::doctor::{self, Check};
use crate::host;
use crate::protect::{self, Protection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Ubuntu, Debian and relatives: the only family with automated fixes.
    Debian,
    Fedora,
    Arch,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    pub id: String,
    pub like: Vec<String>,
}

impl Distro {
    /// From the text of /etc/os-release (`ID=`, `ID_LIKE=`).
    pub fn parse(os_release: &str) -> Distro {
        let get = |key: &str| {
            os_release
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
                .unwrap_or_default()
        };
        Distro {
            id: get("ID"),
            like: get("ID_LIKE")
                .split_whitespace()
                .map(String::from)
                .collect(),
        }
    }

    pub fn family(&self) -> Family {
        let any = |names: &[&str]| {
            std::iter::once(&self.id)
                .chain(self.like.iter())
                .any(|n| names.contains(&n.as_str()))
        };
        if any(&["ubuntu", "debian", "linuxmint", "pop"]) {
            Family::Debian
        } else if any(&["fedora", "rhel", "centos"]) {
            Family::Fedora
        } else if any(&["arch", "manjaro", "endeavouros"]) {
            Family::Arch
        } else {
            Family::Other
        }
    }
}

/// The state of one libvirt domain the recipes build.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DomainState {
    pub defined: bool,
    /// `virsh domstate`, when defined.
    pub state: Option<String>,
    /// Conduit's GPU is in the definition.
    pub attached: bool,
    /// The guest agent answers.
    pub agent: bool,
    /// A CD drive still holds the installer ISO.
    pub cd_inserted: bool,
}

#[derive(Debug, Clone)]
pub struct Env {
    pub distro: Distro,
    pub user: String,
    pub checks: Vec<Check>,
    pub protection: Protection,
    pub driver: Option<host::Driver>,
    pub supported: Vec<String>,
    /// Running from a source checkout (packaging/build.sh is there).
    pub source_checkout: bool,
    /// Build packages not installed yet (only looked at in a source checkout on Debian).
    pub build_deps_missing: Vec<String>,
    pub nfpm: bool,
    pub virsh: bool,
    pub libvirt_session: bool,
    pub vm_name: String,
    pub iso: Option<String>,
    pub iso_sha256: Option<String>,
    pub domain: DomainState,
    /// Steps the user (or an exit code) vouched for.
    pub confirmed: Vec<String>,
    /// Names of the VMs `conduit list` knows.
    pub conduit_vms: Vec<String>,
}

impl Env {
    pub fn detect() -> Env {
        let mut e = Env::blank(Distro::parse(
            &std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
        ));
        e.user = crate::paths::username();
        e.refresh();
        e
    }

    fn blank(distro: Distro) -> Env {
        Env {
            distro,
            user: "user".into(),
            checks: Vec::new(),
            protection: Protection::Untested,
            driver: None,
            supported: Vec::new(),
            source_checkout: false,
            build_deps_missing: Vec::new(),
            nfpm: false,
            virsh: false,
            libvirt_session: false,
            vm_name: String::new(),
            iso: None,
            iso_sha256: None,
            domain: DomainState::default(),
            confirmed: Vec::new(),
            conduit_vms: Vec::new(),
        }
    }

    /// Read the computer again; keeps what the user typed.
    pub fn refresh(&mut self) {
        self.checks = doctor::host_checks();
        self.driver = host::driver();
        self.protection = protect::protection(self.driver.as_ref());
        self.supported = host::supported_drivers().0;
        self.source_checkout =
            crate::paths::repo_root().is_some_and(|r| r.join("packaging/build.sh").is_file());
        self.nfpm = crate::sys::have("nfpm");
        self.virsh = crate::sys::have("virsh");
        self.libvirt_session = self.virsh && crate::virt::session_available();
        self.build_deps_missing = if self.source_checkout && self.distro.family() == Family::Debian
        {
            missing_deb(super::data::BUILD_DEPS_APT)
        } else {
            Vec::new()
        };
        self.conduit_vms = crate::vm::all();
        self.domain = self.read_domain();
    }

    fn read_domain(&self) -> DomainState {
        if !self.libvirt_session || self.vm_name.is_empty() {
            return DomainState::default();
        }
        let v = crate::virt::Virsh::new(super::domain::SESSION_URI);
        let Ok(xml) = v.inactive_xml(&self.vm_name) else {
            return DomainState::default();
        };
        let state = v.state(&self.vm_name);
        let up = state.as_deref().is_some_and(crate::virt::state_is_up);
        DomainState {
            defined: true,
            attached: crate::virt::is_ours(&xml),
            agent: up && crate::guest::agent_ping(&v, &self.vm_name),
            cd_inserted: super::domain::has_cd_media(&xml),
            state,
        }
    }

    pub fn check(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    pub fn field(&self, f: super::Field) -> Option<String> {
        match f {
            super::Field::VmName => Some(self.vm_name.clone()),
            super::Field::IsoPath => self.iso.clone(),
            super::Field::IsoSha256 => self.iso_sha256.clone(),
        }
    }

    pub fn set_field(&mut self, f: super::Field, v: &str) {
        let opt = (!v.is_empty()).then(|| v.to_string());
        match f {
            super::Field::VmName => self.vm_name = v.to_string(),
            super::Field::IsoPath => self.iso = opt,
            super::Field::IsoSha256 => {
                self.iso_sha256 = opt.map(|s| s.to_ascii_lowercase());
                self.confirmed.retain(|c| c != "iso-sha256");
            }
        }
        self.domain = self.read_domain();
    }

    pub fn confirm(&mut self, id: &str) {
        if !self.is_confirmed(id) {
            self.confirmed.push(id.to_string());
        }
    }

    pub fn is_confirmed(&self, id: &str) -> bool {
        self.confirmed.iter().any(|c| c == id)
    }
}

/// Packages of `pkgs` that `dpkg-query` does not list as installed.
pub fn missing_deb(pkgs: &[&str]) -> Vec<String> {
    let mut cmd = std::process::Command::new("dpkg-query");
    cmd.args(["-W", "-f", "${Package} ${db:Status-Abbrev}\\n"])
        .args(pkgs)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let out = cmd
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    not_installed(pkgs, &out)
}

/// `dpkg-query -W -f '${Package} ${db:Status-Abbrev}\n'` output: a package is
/// installed when its status starts with `ii`; unknown ones are not listed.
pub fn not_installed(pkgs: &[&str], out: &str) -> Vec<String> {
    let installed: Vec<&str> = out
        .lines()
        .filter_map(|l| {
            let (p, st) = l.split_once(' ')?;
            st.trim_start().starts_with("ii").then_some(p)
        })
        .collect();
    pkgs.iter()
        .filter(|p| !installed.contains(p))
        .map(|p| p.to_string())
        .collect()
}

#[cfg(test)]
impl Env {
    /// A healthy Ubuntu host with no VM yet, for tests.
    pub fn fixture() -> Env {
        let mut e = Env::blank(Distro::parse("ID=ubuntu\nID_LIKE=debian\n"));
        e.user = "ole".into();
        e.protection = Protection::Proven;
        e.supported = vec!["580.95.05".into()];
        e.vm_name = "omarchy".into();
        e.virsh = true;
        e.libvirt_session = true;
        e
    }
}
