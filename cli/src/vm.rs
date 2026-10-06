//! A VM's settings (vm.json in its folder) and the config handed to the VM runner.

use crate::paths;
use crate::ui::oops;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VmConfig {
    pub name: String,
    /// Disk image (raw ext4 root filesystem). Relative paths are inside the VM folder.
    #[serde(default = "default_disk")]
    pub disk: PathBuf,
    pub ram_mib: u64,
    pub cpus: u32,
    /// Picks the VM's private network: tap `conduit<N>`, 172.30.<N>.0/24.
    pub net_index: u8,
    /// Login user inside the VM (`conduit ssh`).
    pub user: String,
    #[serde(default)]
    pub desktop: String,
    /// Kernel file to boot (an ELF vmlinux); empty = the one installed on the
    /// VM's own disk (needs QEMU). See boot.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
    /// Extra kernel command line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kernel_args: String,
    /// NVIDIA userspace folder shared into the guest; empty = staged from the host driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<PathBuf>,
    #[serde(default)]
    pub created: String,
}

fn default_disk() -> PathBuf {
    PathBuf::from("disk.img")
}

/// VM names become folder, socket and hostnames: keep them simple.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(oops(
            format!("\"{name}\" cannot be used as a VM name"),
            "Use letters, digits, - and _ (up to 32 characters), starting with a letter or digit, e.g. myvm",
        ))
    }
}

pub struct Net {
    pub tap: String,
    pub subnet: String,
    pub host_ip: String,
    pub guest_ip: String,
    pub mac: String,
}

impl VmConfig {
    pub fn new(
        name: &str,
        ram_mib: u64,
        cpus: u32,
        net_index: u8,
        user: &str,
        desktop: &str,
    ) -> Self {
        VmConfig {
            name: name.into(),
            disk: default_disk(),
            ram_mib,
            cpus,
            net_index,
            user: user.into(),
            desktop: desktop.into(),
            kernel: None,
            kernel_args: String::new(),
            share: None,
            created: now(),
        }
    }

    pub fn dir(&self) -> PathBuf {
        paths::vm_dir(&self.name)
    }

    pub fn disk_path(&self) -> PathBuf {
        if self.disk.is_absolute() {
            self.disk.clone()
        } else {
            self.dir().join(&self.disk)
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir().join("logs")
    }

    pub fn net(&self) -> Net {
        let i = self.net_index;
        Net {
            tap: format!("conduit{i}"),
            subnet: format!("172.30.{i}.0/24"),
            host_ip: format!("172.30.{i}.1"),
            guest_ip: format!("172.30.{i}.2"),
            mac: format!("02:00:00:00:{i:02x}:01"),
        }
    }

    pub fn load(name: &str) -> Result<VmConfig> {
        check_name(name)?;
        let f = paths::vm_dir(name).join("vm.json");
        let text = std::fs::read_to_string(&f).map_err(|_| {
            oops(
                format!("there is no VM called \"{name}\""),
                "Run `conduit list` to see your VMs, or `conduit create NAME` to make one",
            )
        })?;
        let mut c: VmConfig =
            serde_json::from_str(&text).with_context(|| format!("{} is damaged", f.display()))?;
        c.name = name.to_string();
        Ok(c)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(self.dir())?;
        let f = self.dir().join("vm.json");
        let tmp = self.dir().join(".vm.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, &f).with_context(|| format!("saving {}", f.display()))
    }
}

/// All VM names, sorted.
pub fn all() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(paths::vms_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("vm.json").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Lowest network index no other VM uses.
pub fn free_net_index() -> Result<u8> {
    let used: Vec<u8> = all()
        .iter()
        .filter_map(|n| VmConfig::load(n).ok())
        .map(|c| c.net_index)
        .collect();
    (0..=254u8)
        .find(|i| !used.contains(i))
        .ok_or_else(|| oops("too many VMs (255)", "Remove one you no longer need"))
}

pub fn now() -> String {
    crate::sys::output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The built-in VM runner's (conduit-vmm) JSON config.
pub fn vmm_config(c: &VmConfig, kernel: &Path, gpu_sock: &Path, share: &Path) -> serde_json::Value {
    vmm_config_with(c, kernel, gpu_sock, share, crate::config::window_mib())
}

/// [`vmm_config`] with the window's size, which must be the backend's
/// `--window-mib` (run.rs passes both); `None` leaves both at their default.
fn vmm_config_with(
    c: &VmConfig,
    kernel: &Path,
    gpu_sock: &Path,
    share: &Path,
    window_mib: Option<u64>,
) -> serde_json::Value {
    let n = c.net();
    let mut gpu_forward = json!({ "socket": gpu_sock });
    if let Some(mib) = window_mib {
        gpu_forward["window-mib"] = json!(mib);
    }
    let mut args = String::from("console=hvc0 root=/dev/vda rw");
    if !c.kernel_args.trim().is_empty() {
        args.push(' ');
        args.push_str(c.kernel_args.trim());
    }
    json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "boot_args": args,
        },
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": c.disk_path(),
            "is_root_device": true,
            "is_read_only": false,
        }],
        "machine-config": {
            "vcpu_count": c.cpus,
            "mem_size_mib": c.ram_mib,
            // Never commit all guest RAM at boot: pages are faulted in as the
            // guest touches them (several prefaulted VMs ran a host out of RAM).
            "prefault": false,
        },
        "gpu-forward": gpu_forward,
        "shared-directories": [{
            "tag": "nvidia",
            "path-on-host": share,
            "read-only": true,
        }],
        "network": {
            "tap-name": n.tap,
            "host-ip": n.host_ip,
            "netmask": "255.255.255.0",
            "mac": n.mac,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["myvm", "a", "game-box_2", "9lives"] {
            check_name(ok).unwrap();
        }
        for bad in [
            "",
            "-x",
            "_x",
            "my vm",
            "../etc",
            "a/b",
            "x.y",
            &"a".repeat(33),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn network_from_index() {
        let mut c = VmConfig::new("t", 8192, 4, 0, "me", "gnome");
        let n = c.net();
        assert_eq!(
            (n.tap.as_str(), n.host_ip.as_str(), n.guest_ip.as_str()),
            ("conduit0", "172.30.0.1", "172.30.0.2")
        );
        assert_eq!(n.mac, "02:00:00:00:00:01");
        c.net_index = 17;
        let n = c.net();
        assert_eq!(n.subnet, "172.30.17.0/24");
        assert_eq!(n.mac, "02:00:00:00:11:01");
    }

    #[test]
    fn vmm_config_matches_runner_format() {
        let mut c = VmConfig::new("t", 8192, 6, 3, "me", "gnome");
        c.disk = PathBuf::from("/vms/t/disk.img");
        c.kernel_args = "quiet".into();
        let v = vmm_config_with(
            &c,
            Path::new("/k/vmlinux"),
            Path::new("/run/gpu.sock"),
            Path::new("/share"),
            None,
        );
        assert_eq!(v["boot-source"]["kernel_image_path"], "/k/vmlinux");
        assert_eq!(
            v["boot-source"]["boot_args"],
            "console=hvc0 root=/dev/vda rw quiet"
        );
        assert_eq!(v["drives"][0]["path_on_host"], "/vms/t/disk.img");
        assert_eq!(v["drives"][0]["is_root_device"], true);
        assert_eq!(v["machine-config"]["vcpu_count"], 6);
        assert_eq!(v["machine-config"]["mem_size_mib"], 8192);
        assert_eq!(v["machine-config"]["prefault"], false);
        assert_eq!(v["gpu-forward"]["socket"], "/run/gpu.sock");
        assert_eq!(v["shared-directories"][0]["tag"], "nvidia");
        assert_eq!(v["shared-directories"][0]["read-only"], true);
        assert_eq!(v["network"]["tap-name"], "conduit3");
        assert_eq!(v["network"]["host-ip"], "172.30.3.1");
        assert_eq!(v["network"]["netmask"], "255.255.255.0");
    }

    /// The window's size reaches conduit-vmm only when it is set, and then as
    /// the number the backend is given.
    #[test]
    fn vmm_config_carries_the_window_size_when_set() {
        let c = VmConfig::new("t", 8192, 6, 3, "me", "gnome");
        let (k, g, s) = (Path::new("/k"), Path::new("/g"), Path::new("/s"));
        let v = vmm_config_with(&c, k, g, s, None);
        assert!(v["gpu-forward"].get("window-mib").is_none());
        let v = vmm_config_with(&c, k, g, s, Some(8192));
        assert_eq!(v["gpu-forward"]["window-mib"], 8192);
        assert_eq!(v["gpu-forward"]["socket"], "/g");
    }

    #[test]
    fn vm_json_roundtrip_and_defaults() {
        let c = VmConfig::new("t", 4096, 2, 1, "me", "none");
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("kernel\""), "unset kernel is not written: {s}");
        let back: VmConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
        // Minimal hand-written file.
        let m: VmConfig = serde_json::from_str(
            r#"{"name":"x","ram_mib":1024,"cpus":1,"net_index":0,"user":"u"}"#,
        )
        .unwrap();
        assert_eq!(m.disk, PathBuf::from("disk.img"));
    }
}
