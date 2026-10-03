//! What a VM boots: by default the kernel and initrd installed on its own disk
//! (the distro's stock kernel, with the guest driver built by DKMS), read out
//! of the disk image before each start. A VM can instead name a kernel file in
//! vm.json ("kernel"), which is how VMs from before stock kernels boot.
//!
//! The disk is a bare ext4 filesystem, so the files are read with `debugfs`
//! (e2fsprogs) as you, without mounting anything.

use crate::sys;
use crate::ui::oops;
use crate::vm::VmConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq)]
pub enum Boot {
    /// A kernel file named in vm.json (an ELF vmlinux; no initrd).
    Custom(PathBuf),
    /// The disk's own kernel, copied out of /boot.
    Disk {
        version: String,
        kernel: PathBuf,
        initrd: Option<PathBuf>,
    },
}

impl Boot {
    pub fn kernel(&self) -> &Path {
        match self {
            Boot::Custom(k) => k,
            Boot::Disk { kernel, .. } => kernel,
        }
    }
    pub fn initrd(&self) -> Option<&Path> {
        match self {
            Boot::Custom(_) => None,
            Boot::Disk { initrd, .. } => initrd.as_deref(),
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Boot::Custom(k) => format!("kernel {}", k.display()),
            Boot::Disk { version, .. } => format!("the VM's own kernel {version}"),
        }
    }
}

/// Compare kernel versions like `6.8.0-85-generic` numerically, part by part.
fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(0))
        .collect()
}

/// Names in /boot from `debugfs -R "ls -p /boot"` (`/ino/mode/uid/gid/name/size/`).
pub fn parse_ls(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split('/').collect();
            // ["", ino, mode, uid, gid, name, size, ""]
            (f.len() >= 7 && f[2].starts_with("100")).then(|| f[5].to_string())
        })
        .collect()
}

/// The newest kernel in a /boot listing, and its initrd if there is one.
pub fn pick(names: &[String]) -> Option<(String, Option<String>)> {
    let ver = names
        .iter()
        .filter_map(|n| n.strip_prefix("vmlinuz-"))
        .max_by_key(|v| version_key(v))?
        .to_string();
    let initrd = format!("initrd.img-{ver}");
    let has_initrd = names.contains(&initrd);
    Some((ver, has_initrd.then_some(initrd)))
}

fn debugfs() -> Result<PathBuf> {
    sys::which("debugfs").ok_or_else(|| {
        oops(
            "`debugfs` is needed to read the VM's kernel from its disk",
            "Install e2fsprogs (it provides debugfs)",
        )
    })
}

fn run_debugfs(tool: &Path, disk: &Path, req: &str) -> Result<String> {
    // -c (catastrophic mode): read without the allocation bitmaps, so a disk
    // left unclean by a forced power-off (bitmap checksums not yet fixed by
    // the guest's journal replay) still gives up its kernel. Read-only.
    let out = Command::new(tool)
        .arg("-c")
        .arg("-R")
        .arg(req)
        .arg(disk)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {}", tool.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn dump(tool: &Path, disk: &Path, from: &str, to: &Path) -> Result<()> {
    let tmp = to.with_extension("part");
    let _ = std::fs::remove_file(&tmp);
    run_debugfs(tool, disk, &format!("dump {from} {}", tmp.display()))?;
    let ok = std::fs::metadata(&tmp)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("could not copy {from} out of {}", disk.display());
    }
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// Work out what to boot; for the disk's own kernel, copy it (and its initrd)
/// into the VM folder's boot/ (refreshed on every start, so a kernel update
/// inside the VM takes effect on the next start).
pub fn resolve(c: &VmConfig) -> Result<Boot> {
    if let Some(k) = &c.kernel {
        if k.is_file() {
            return Ok(Boot::Custom(k.clone()));
        }
        return Err(oops(
            format!("the kernel set for this VM is missing: {}", k.display()),
            format!(
                "Fix or remove \"kernel\" in {} (without it the VM boots its own kernel)",
                c.dir().join("vm.json").display()
            ),
        ));
    }
    let disk = c.disk_path();
    let tool = debugfs()?;
    let names = parse_ls(&run_debugfs(&tool, &disk, "ls -p /boot")?);
    let Some((ver, initrd)) = pick(&names) else {
        return Err(oops(
            format!("no kernel found in /boot on {}'s disk", c.name),
            format!(
                "Install one inside the VM (Ubuntu: linux-image-generic), or set \"kernel\" in {}",
                c.dir().join("vm.json").display()
            ),
        ));
    };
    let dir = c.dir().join("boot");
    std::fs::create_dir_all(&dir)?;
    let kernel = dir.join("vmlinuz");
    dump(&tool, &disk, &format!("/boot/vmlinuz-{ver}"), &kernel)?;
    let initrd = match initrd {
        Some(i) => {
            let to = dir.join("initrd.img");
            dump(&tool, &disk, &format!("/boot/{i}"), &to)?;
            Some(to)
        }
        None => {
            let _ = std::fs::remove_file(dir.join("initrd.img"));
            None
        }
    };
    Ok(Boot::Disk {
        version: ver,
        kernel,
        initrd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_newest_kernel_and_its_initrd() {
        let ls = "\
 /2/040755/0/0/./4096/
 /12/100644/0/0/vmlinuz-6.8.0-79-generic/14978952/
 /13/100600/0/0/vmlinuz-6.8.0-85-generic/15000000/
 /14/100644/0/0/initrd.img-6.8.0-85-generic/70000000/
 /15/120777/0/0/vmlinuz/24/
 /16/100644/0/0/config-6.8.0-85-generic/280000/
";
        let names = parse_ls(ls);
        assert!(names.contains(&"vmlinuz-6.8.0-79-generic".to_string()));
        assert!(!names.contains(&"vmlinuz".to_string()), "symlinks skipped");
        assert_eq!(
            pick(&names),
            Some((
                "6.8.0-85-generic".into(),
                Some("initrd.img-6.8.0-85-generic".into())
            ))
        );
        let only = vec!["vmlinuz-6.11.0-1-generic".to_string()];
        assert_eq!(pick(&only), Some(("6.11.0-1-generic".into(), None)));
        assert_eq!(pick(&[]), None);
        // 6.10 is newer than 6.9 (numeric, not text, order).
        let v = vec![
            "vmlinuz-6.9.0-1-generic".to_string(),
            "vmlinuz-6.10.0-1-generic".to_string(),
        ];
        assert_eq!(pick(&v).unwrap().0, "6.10.0-1-generic");
    }
}
