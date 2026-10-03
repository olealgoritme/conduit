//! `conduit create` (build a new VM disk) and `conduit import` (adopt one).

use crate::paths::{self, Tool};
use crate::sys;
use crate::ui::{self, oops};
use crate::vm::{self, VmConfig};
use anyhow::{Context, Result};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const IMAGE_BASE: &str = "https://cloud-images.ubuntu.com/noble/current";
pub const IMAGE_FILE: &str = "noble-server-cloudimg-amd64-root.tar.xz";
const UBUNTU_CLOUD_KEYRING: &str = "/usr/share/keyrings/ubuntu-cloudimage-keyring.gpg";

const BUILD_SCRIPT: &str = include_str!("../assets/build-disk.sh");
const GUEST_FILES: &[(&str, &str)] = &[
    (
        "conduit-guest.service",
        include_str!("../assets/guest/conduit-guest.service"),
    ),
    (
        "99-conduit.rules",
        include_str!("../assets/guest/99-conduit.rules"),
    ),
    (
        "71-conduit-seat.rules",
        include_str!("../assets/guest/71-conduit-seat.rules"),
    ),
    (
        "zz-conduit-nvidia.conf",
        include_str!("../assets/guest/zz-conduit-nvidia.conf"),
    ),
    (
        "conduit-nvidia.sh",
        include_str!("../assets/guest/conduit-nvidia.sh"),
    ),
    (
        "90-conduit-nvidia.conf",
        include_str!("../assets/guest/90-conduit-nvidia.conf"),
    ),
    (
        "10-conduit.network",
        include_str!("../assets/guest/10-conduit.network"),
    ),
    (
        "gdm-custom.conf",
        include_str!("../assets/guest/gdm-custom.conf"),
    ),
    (
        "lightdm-autologin.conf",
        include_str!("../assets/guest/lightdm-autologin.conf"),
    ),
    (
        "dconf-00-conduit",
        include_str!("../assets/guest/dconf-00-conduit"),
    ),
    (
        "nm-unmanaged.conf",
        include_str!("../assets/guest/nm-unmanaged.conf"),
    ),
];

pub struct CreateOpts {
    pub name: String,
    pub size: u64,
    pub desktop: String,
    pub ram_mib: u64,
    pub cpus: u32,
    pub user: Option<String>,
    pub tarball: Option<PathBuf>,
}

/// Find `file`'s hash in a SHA256SUMS listing ("<hash> *<file>").
pub fn sum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let h = it.next()?;
        let f = it.next()?.trim_start_matches('*');
        (f == file && h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| h.to_ascii_lowercase())
    })
}

fn sha256(file: &Path) -> Result<String> {
    let out = sys::output("sha256sum", &[file.to_str().context("odd path")?])?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .context("sha256sum printed nothing")
}

fn download(url: &str, to: &Path, progress: bool) -> Result<()> {
    let tmp = to.with_extension("part");
    if !progress {
        // Small files: never resume a partial copy of an older version.
        let _ = std::fs::remove_file(&tmp);
    }
    let mut cmd = Command::new("curl");
    cmd.args(["-fL", "--retry", "3", "-C", "-", "-o"])
        .arg(&tmp)
        .arg(url);
    if !progress {
        cmd.arg("-sS");
    }
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        return Err(oops(
            format!("download failed: {url}"),
            "Check your internet connection, then run the same command again (it resumes)",
        ));
    }
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// Download (or reuse) the Ubuntu root tarball and check it against Ubuntu's signed checksums.
fn get_image(local: Option<&Path>) -> Result<PathBuf> {
    let dir = paths::cache_dir().join("images");
    std::fs::create_dir_all(&dir)?;
    let sums_path = dir.join("SHA256SUMS");
    ui::info("checking Ubuntu's published checksums");
    download(&format!("{IMAGE_BASE}/SHA256SUMS"), &sums_path, false)?;
    let gpg = dir.join("SHA256SUMS.gpg");
    if sys::have("gpgv")
        && Path::new(UBUNTU_CLOUD_KEYRING).is_file()
        && download(&format!("{IMAGE_BASE}/SHA256SUMS.gpg"), &gpg, false).is_ok()
    {
        let ok = sys::quiet(
            "gpgv",
            &[
                "--keyring",
                UBUNTU_CLOUD_KEYRING,
                gpg.to_str().unwrap(),
                sums_path.to_str().unwrap(),
            ],
        );
        if !ok {
            return Err(oops(
                "Ubuntu's checksum file does not carry a valid Ubuntu signature",
                "This can mean a broken download or tampering. Try again later.",
            ));
        }
    } else {
        ui::warn("could not check the checksum file's signature (gpgv or Ubuntu keyring missing); using HTTPS only");
    }
    let sums = std::fs::read_to_string(&sums_path)?;
    let want = sum_for(&sums, IMAGE_FILE)
        .context("Ubuntu's checksum list has no entry for the root image")?;

    let file = match local {
        Some(p) => p.to_path_buf(),
        None => dir.join(IMAGE_FILE),
    };
    if file.is_file() {
        ui::info(format!("verifying {}", file.display()));
        if sha256(&file)? == want {
            return Ok(file);
        }
        if local.is_some() {
            return Err(oops(
                format!(
                    "{} does not match Ubuntu's current checksum",
                    file.display()
                ),
                "It may be an older release. Leave out --tarball to download the current one.",
            ));
        }
        ui::info("cached image is outdated; downloading the current one");
    }
    ui::info(format!(
        "downloading Ubuntu 24.04 ({IMAGE_FILE}, about 300 MB)"
    ));
    download(&format!("{IMAGE_BASE}/{IMAGE_FILE}"), &file, true)?;
    if sha256(&file)? != want {
        let _ = std::fs::remove_file(&file);
        return Err(oops(
            "the downloaded image is damaged (checksum mismatch)",
            "Run the same command again",
        ));
    }
    Ok(file)
}

/// Conduit's own ssh key, used to log in to (and cleanly shut down) VMs.
pub fn ensure_ssh_key() -> Result<PathBuf> {
    let key = paths::ssh_key();
    if !key.is_file() {
        std::fs::create_dir_all(key.parent().unwrap())?;
        std::fs::set_permissions(
            key.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )?;
        let ok = sys::quiet(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "conduit",
                "-f",
                key.to_str().unwrap(),
            ],
        );
        if !ok {
            return Err(oops(
                "could not make an ssh key",
                "Install openssh-client (it provides ssh-keygen)",
            ));
        }
    }
    Ok(key)
}

fn authorized_keys() -> Result<String> {
    let mut keys = std::fs::read_to_string(paths::ssh_key().with_extension("pub"))?;
    // Also let the user's own keys in, so plain `ssh` works too.
    for k in ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"] {
        if let Ok(s) = std::fs::read_to_string(paths::home().join(".ssh").join(k)) {
            keys.push_str(&s);
        }
    }
    Ok(keys)
}

fn env_line(k: &str, v: &str) -> String {
    format!("{k}={}\n", ui::shell_quote(v))
}

/// The config.env build-disk.sh reads.
pub fn build_env(
    o: &CreateOpts,
    c: &VmConfig,
    tarball: &Path,
    user: &str,
    uid: u32,
    gid: u32,
) -> String {
    let n = c.net();
    let mut s = String::new();
    s += &env_line("DISK", &c.disk_path().to_string_lossy());
    s += &env_line("SIZE_BYTES", &o.size.to_string());
    s += &env_line("TARBALL", &tarball.to_string_lossy());
    s += &env_line("DESKTOP", &o.desktop);
    s += &env_line("GUEST_USER", user);
    s += &env_line("GUEST_PASSWORD", "conduit");
    s += &env_line("VM_HOSTNAME", &o.name);
    s += &env_line("GUEST_IP", &n.guest_ip);
    s += &env_line("HOST_IP", &n.host_ip);
    s += &env_line("OWNER_UID", &uid.to_string());
    s += &env_line("OWNER_GID", &gid.to_string());
    s
}

/// Guest user names: lowercase letters, digits, -, _ ; must start with a letter.
fn guest_user_name(host_user: &str) -> String {
    let u: String = host_user
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(31)
        .collect();
    if u.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && u != "root" {
        u
    } else {
        "conduit".into()
    }
}

pub fn create(o: CreateOpts) -> Result<()> {
    vm::check_name(&o.name)?;
    if !["gnome", "xfce", "none"].contains(&o.desktop.as_str()) {
        return Err(oops(
            format!("unknown desktop \"{}\"", o.desktop),
            "Choose gnome, xfce or none",
        ));
    }
    let dir = paths::vm_dir(&o.name);
    if dir.join("vm.json").exists() {
        return Err(oops(
            format!("a VM called \"{}\" already exists", o.name),
            "Pick another name, or remove the old one's folder: ".to_string()
                + &dir.display().to_string(),
        ));
    }
    for t in [
        "curl",
        "tar",
        "xz",
        "sha256sum",
        "mkfs.ext4",
        "truncate",
        "ssh-keygen",
    ] {
        if !sys::have(t) {
            return Err(oops(
                format!("the program `{t}` is needed but not installed"),
                "Install it with your package manager, then try again",
            ));
        }
    }
    let module = Tool::GuestModule.require()?;
    if Tool::Kernel.find().is_none() {
        ui::warn("the guest kernel was not found; the disk will be built, but `conduit up` needs it (see `conduit doctor`)");
    }
    let need = 12u64 << 30;
    std::fs::create_dir_all(paths::data_dir())?;
    if let Some(free) = sys::free_bytes(&paths::data_dir()) {
        if free < need {
            return Err(oops(
                format!(
                    "not enough free disk space ({} free, about 12 GB needed)",
                    ui::human_bytes(free)
                ),
                format!(
                    "Free some space where VMs are stored: {}",
                    paths::vms_dir().display()
                ),
            ));
        }
    }

    let host_user = paths::username();
    let user = o
        .user
        .clone()
        .unwrap_or_else(|| guest_user_name(&host_user));
    let tarball = get_image(o.tarball.as_deref())?;
    ensure_ssh_key()?;

    let mut c = VmConfig::new(
        &o.name,
        o.ram_mib,
        o.cpus,
        vm::free_net_index()?,
        &user,
        &o.desktop,
    );
    std::fs::create_dir_all(c.logs_dir())?;

    // Work folder handed to the root helper.
    let work = paths::cache_dir().join(format!("build-{}", o.name));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("guest"))?;
    for (f, body) in GUEST_FILES {
        std::fs::write(work.join("guest").join(f), body)?;
    }
    let script = work.join("build-disk.sh");
    std::fs::write(&script, BUILD_SCRIPT)?;
    std::fs::copy(&module, work.join("virtio_gpu_nv.ko")).context("copying the guest driver")?;
    std::fs::write(work.join("authorized_keys"), authorized_keys()?)?;
    let meta = std::fs::metadata(paths::data_dir())?;
    std::fs::write(
        work.join("config.env"),
        build_env(&o, &c, &tarball, &user, meta.uid(), meta.gid()),
    )?;

    sys::sudo_ready(
        "Building the VM disk: it is formatted and filled through a loop mount and a chroot,\n         \
         which only the administrator may do. Nothing outside the new disk file is changed.",
    )?;
    ui::info(format!(
        "building {} ({}, {} desktop)",
        o.name,
        ui::human_bytes(o.size),
        o.desktop
    ));
    let log = c.logs_dir().join("create.log");
    // tee runs as you, so the log stays yours; only the build script runs as root.
    let status = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -o pipefail; sudo bash {} {} 2>&1 | tee {}",
            ui::shell_quote(&script.to_string_lossy()),
            ui::shell_quote(&work.to_string_lossy()),
            ui::shell_quote(&log.to_string_lossy())
        ))
        .status()
        .context("could not run sudo")?;
    let _ = std::fs::remove_dir_all(&work);
    if !status.success() {
        let _ = std::fs::remove_file(c.disk_path());
        return Err(oops(
            format!("building {} failed", o.name),
            format!(
                "The full log is in {}\nFix the problem above and run the same command again.",
                log.display()
            ),
        ));
    }
    c.created = vm::now();
    c.save()?;
    println!();
    println!("{} is ready.", o.name);
    println!("  open it:      conduit view {}", o.name);
    println!("  terminal:     conduit ssh {}", o.name);
    println!("  user / pass:  {user} / conduit  (change it with `passwd` inside the VM)");
    Ok(())
}

pub struct ImportOpts {
    pub path: PathBuf,
    pub name: String,
    pub mv: bool,
    pub ram_mib: u64,
    pub cpus: u32,
    pub user: String,
    pub kernel: Option<PathBuf>,
    pub share: Option<PathBuf>,
    pub net_index: Option<u8>,
}

/// Is any process holding this file open? (fuser exits 0 when one is.)
fn in_use(p: &Path) -> bool {
    sys::have("fuser") && sys::quiet("fuser", &["-s", p.to_str().unwrap_or("")])
}

pub fn import(o: ImportOpts) -> Result<()> {
    vm::check_name(&o.name)?;
    let src = o.path.canonicalize().map_err(|_| {
        oops(
            format!("no such disk image: {}", o.path.display()),
            "Give the path to a raw ext4 disk image (e.g. rootfs.ext4)",
        )
    })?;
    if !src.is_file() {
        return Err(oops(
            format!("{} is not a file", src.display()),
            "Give the path to a disk image file",
        ));
    }
    if in_use(&src) {
        return Err(oops(
            format!("{} is in use by a running program (a VM?)", src.display()),
            "Stop that VM first",
        ));
    }
    let dir = paths::vm_dir(&o.name);
    if dir.join("vm.json").exists() {
        return Err(oops(
            format!("a VM called \"{}\" already exists", o.name),
            "Pick another name",
        ));
    }
    let net_index = match o.net_index {
        Some(i) => i,
        None => vm::free_net_index()?,
    };
    let mut c = VmConfig::new(&o.name, o.ram_mib, o.cpus, net_index, &o.user, "imported");
    c.kernel = o.kernel.map(|k| k.canonicalize().unwrap_or(k));
    c.share = o.share.map(|s| s.canonicalize().unwrap_or(s));
    std::fs::create_dir_all(c.logs_dir())?;
    let dst = c.disk_path();
    let meta = std::fs::metadata(&src)?;
    if o.mv && std::fs::rename(&src, &dst).is_ok() {
        ui::info(format!("moved {} -> {}", src.display(), dst.display()));
    } else {
        if let Some(free) = sys::free_bytes(&dir) {
            let used = meta.blocks() * 512;
            if free < used + (1 << 30) {
                return Err(oops(
                    format!(
                        "not enough free space to copy the disk ({} needed, {} free)",
                        ui::human_bytes(used),
                        ui::human_bytes(free)
                    ),
                    "Free some space, or use --move to move the file instead of copying it",
                ));
            }
        }
        ui::info(format!(
            "copying {} ({} used) …",
            src.display(),
            ui::human_bytes(meta.blocks() * 512)
        ));
        let ok = Command::new("cp")
            .args(["--sparse=always", "--reflink=auto"])
            .arg(&src)
            .arg(&dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&dst);
            return Err(oops(
                "copying the disk failed",
                "Check free space and permissions, then try again",
            ));
        }
        if o.mv {
            std::fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
        }
    }
    c.save()?;
    let n = c.net();
    println!("Imported {} as {}.", src.display(), o.name);
    println!(
        "  It must use the static address {} with gateway {} inside (this is VM network #{}).",
        n.guest_ip, n.host_ip, net_index
    );
    if c.kernel.is_none() && Tool::Kernel.find().is_none() {
        println!(
            "  Note: no guest kernel found. Set one with --kernel, or edit \"kernel\" in {}",
            c.dir().join("vm.json").display()
        );
    }
    println!("  Start it: conduit view {}", o.name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_lookup() {
        let sums = format!(
            "{} *noble-server-cloudimg-amd64.img\n{} *{IMAGE_FILE}\n",
            "a".repeat(64),
            "B".repeat(64)
        );
        assert_eq!(sum_for(&sums, IMAGE_FILE), Some("b".repeat(64)));
        assert_eq!(sum_for(&sums, "nope.tar.xz"), None);
        assert_eq!(sum_for("short *x\n", "x"), None);
    }

    #[test]
    fn user_names() {
        assert_eq!(guest_user_name("Ole"), "ole");
        assert_eq!(guest_user_name("root"), "conduit");
        assert_eq!(guest_user_name("1abc"), "conduit");
        assert_eq!(guest_user_name("a.b c"), "abc");
    }

    #[test]
    fn build_env_is_shell_safe() {
        let o = CreateOpts {
            name: "vm1".into(),
            size: 64 << 30,
            desktop: "gnome".into(),
            ram_mib: 8192,
            cpus: 4,
            user: None,
            tarball: None,
        };
        let mut c = VmConfig::new("vm1", 8192, 4, 5, "me", "gnome");
        c.disk = PathBuf::from("/home/a b/disk.img");
        let s = build_env(&o, &c, Path::new("/c/root.tar.xz"), "me", 1000, 1000);
        assert!(s.contains("DISK='/home/a b/disk.img'\n"));
        assert!(s.contains("SIZE_BYTES=68719476736\n"));
        assert!(s.contains("GUEST_IP=172.30.5.2\n"));
        assert!(s.contains("HOST_IP=172.30.5.1\n"));
        assert!(s.contains("DESKTOP=gnome\n"));
    }

    #[test]
    fn every_guest_file_is_used_by_the_script() {
        for (f, _) in GUEST_FILES {
            assert!(BUILD_SCRIPT.contains(f), "build-disk.sh never installs {f}");
        }
    }
}
