//! Running a VM under Conduit's QEMU (11.1 + the patches in host/qemu).
//!
//! Same VM as the built-in runner: direct kernel boot, the raw ext4 disk as
//! virtio-blk, virtio-net on the VM's tap, the NVIDIA user-space share over
//! virtiofs, and the GPU through QEMU's generic vhost-user device (see
//! docs/QEMU.md). On top of that QEMU gives the VM a sound card
//! (virtio-sound, speakers and microphone) played through the desktop's
//! PipeWire or PulseAudio, and a QMP socket for a clean ACPI shutdown.

use crate::sys;
use crate::vm::VmConfig;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// `sizeof(struct virtio_gpu_nv_config)`; the guest driver reads all of it.
pub const NVGPU_CONFIG_SIZE: u32 = 4036;
/// virtio device id the guest driver binds.
pub const NVGPU_VIRTIO_ID: u32 = 45;

/// Where virtiofsd lives: distributions keep it out of PATH.
const VIRTIOFSD_DIRS: &[&str] = &[
    "/usr/libexec",
    "/usr/lib",
    "/usr/lib/qemu",
    "/usr/local/libexec",
    "/usr/local/lib",
];

pub fn virtiofsd() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CONDUIT_VIRTIOFSD") {
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    let pf = crate::paths::prefix().join("libexec/virtiofsd");
    if pf.is_file() {
        return Some(pf);
    }
    sys::which("virtiofsd").or_else(|| {
        VIRTIOFSD_DIRS
            .iter()
            .map(|d| Path::new(d).join("virtiofsd"))
            .find(|p| p.is_file())
    })
}

/// Which sound server QEMU should play through, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audio {
    PipeWire,
    Pulse,
}

impl Audio {
    pub fn driver(self) -> &'static str {
        match self {
            Audio::PipeWire => "pipewire",
            Audio::Pulse => "pa",
        }
    }
}

/// Audio drivers compiled into this QEMU (`-audiodev help`).
fn qemu_audio_drivers(qemu: &Path) -> Vec<String> {
    Command::new(qemu)
        .args(["-audiodev", "help"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The desktop's sound server that this QEMU can talk to. `CONDUIT_AUDIO=off`
/// turns sound off; `=pipewire` / `=pa` forces one.
pub fn pick_audio(qemu: &Path) -> Option<Audio> {
    let want = std::env::var("CONDUIT_AUDIO").unwrap_or_default();
    if matches!(want.as_str(), "off" | "none" | "0") {
        return None;
    }
    let have = qemu_audio_drivers(qemu);
    let rt = crate::paths::xdg_runtime();
    let pw = have.iter().any(|d| d == "pipewire");
    let pa = have.iter().any(|d| d == "pa");
    match want.as_str() {
        "pipewire" if pw => return Some(Audio::PipeWire),
        "pa" | "pulse" | "pulseaudio" if pa => return Some(Audio::Pulse),
        _ => {}
    }
    if pw && rt.join("pipewire-0").exists() {
        Some(Audio::PipeWire)
    } else if pa && rt.join("pulse/native").exists() {
        Some(Audio::Pulse)
    } else {
        None
    }
}

pub struct Paths<'a> {
    pub kernel: &'a Path,
    pub initrd: Option<&'a Path>,
    pub gpu_sock: &'a Path,
    pub vfs_sock: &'a Path,
    pub qmp_sock: &'a Path,
    pub console_log: &'a Path,
}

/// The GPU: QEMU 11.1's generic vhost-user device. Two queues (control,
/// events), the backend's queue size, and the full device config, display
/// fields included (a smaller config_size hides them).
pub fn gpu_device(chardev: &str) -> String {
    format!(
        "vhost-user-test-device-pci,chardev={chardev},virtio-id={NVGPU_VIRTIO_ID},num_vqs=2,vq_size=256,config_size={NVGPU_CONFIG_SIZE}"
    )
}

/// Kernel command line: the console is the first serial port under QEMU.
pub fn kernel_args(c: &VmConfig) -> String {
    let mut args = String::from("console=ttyS0 root=/dev/vda rw");
    if !c.kernel_args.trim().is_empty() {
        args.push(' ');
        args.push_str(c.kernel_args.trim());
    }
    args
}

/// The full QEMU argument list (without the program).
pub fn args(c: &VmConfig, p: &Paths, audio: Option<Audio>) -> Vec<String> {
    let n = c.net();
    let mem = format!("{}M", c.ram_mib);
    let s = |x: &str| x.to_string();
    let d = |x: &Path| x.display().to_string();
    let mut a: Vec<String> = vec![
        s("-name"),
        format!("guest={},debug-threads=on", c.name),
        s("-machine"),
        s("q35,accel=kvm,memory-backend=mem"),
        // The nvgpu shared-memory BAR is 2 GiB and 64-bit: it goes above
        // 4 GiB, which needs the real physical address width.
        s("-cpu"),
        s("host,host-phys-bits=on"),
        s("-smp"),
        c.cpus.to_string(),
        s("-m"),
        mem.clone(),
        // The GPU backend reads requests out of guest RAM: RAM must be a shared fd.
        s("-object"),
        format!("memory-backend-memfd,id=mem,size={mem},share=on"),
        s("-nodefaults"),
        s("-no-user-config"),
        s("-display"),
        s("none"),
        s("-chardev"),
        format!("file,id=con0,path={},append=on", d(p.console_log)),
        s("-serial"),
        s("chardev:con0"),
        s("-qmp"),
        format!("unix:{},server=on,wait=off", d(p.qmp_sock)),
        s("-kernel"),
        d(p.kernel),
        s("-append"),
        kernel_args(c),
    ];
    if let Some(i) = p.initrd {
        a.extend([s("-initrd"), d(i)]);
    }
    a.extend([
        s("-drive"),
        format!(
            "file={},format=raw,if=virtio,cache=none,discard=unmap",
            d(&c.disk_path())
        ),
        s("-netdev"),
        format!("tap,id=net0,ifname={},script=no,downscript=no", n.tap),
        s("-device"),
        format!("virtio-net-pci,netdev=net0,mac={}", n.mac),
        s("-chardev"),
        format!("socket,id=vfs,path={}", d(p.vfs_sock)),
        s("-device"),
        s("vhost-user-fs-pci,chardev=vfs,tag=nvidia"),
        s("-chardev"),
        format!("socket,id=nvgpu,path={}", d(p.gpu_sock)),
        s("-device"),
        gpu_device("nvgpu"),
        s("-device"),
        s("virtio-rng-pci"),
    ]);
    if let Some(au) = audio {
        a.extend([
            s("-audiodev"),
            format!(
                "{},id=snd0,out.name=conduit-{},in.name=conduit-{}",
                au.driver(),
                c.name,
                c.name
            ),
            s("-device"),
            // streams=2: one playback (speakers) and one capture (microphone).
            s("virtio-sound-pci,audiodev=snd0,streams=2"),
        ]);
    }
    a
}

// ---------------------------------------------------------------- virtiofsd

pub fn supports_readonly(vfsd: &Path) -> bool {
    Command::new(vfsd)
        .arg("--help")
        .output()
        .map(|o| String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).contains("--readonly"))
        .unwrap_or(false)
}

/// The virtiofsd command for the NVIDIA share. Read-only when this virtiofsd
/// can (`--readonly` arrived in 1.11; Ubuntu 24.04 ships 1.10).
pub fn virtiofsd_cmd(vfsd: &Path, sock: &Path, share: &Path) -> (Command, bool) {
    let mut cmd = Command::new(vfsd);
    cmd.arg(format!("--socket-path={}", sock.display()))
        .arg(format!("--shared-dir={}", share.display()))
        // Ubuntu 24.04 restricts unprivileged user namespaces: run as you.
        .args(["--sandbox=none", "--cache=auto", "--log-level=warn"]);
    let ro = supports_readonly(vfsd);
    if ro {
        cmd.arg("--readonly");
    }
    (cmd, ro)
}

// ---------------------------------------------------------------- QMP

/// A minimal QMP client: greeting, capabilities, then one command per call.
pub struct Qmp {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Qmp {
    pub fn connect(sock: &Path) -> Result<Qmp> {
        let s = UnixStream::connect(sock)
            .with_context(|| format!("connecting to {}", sock.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let w = s.try_clone()?;
        let mut q = Qmp {
            r: BufReader::new(s),
            w,
        };
        q.read_msg()?; // {"QMP": {...}}
        q.exec("qmp_capabilities")?;
        Ok(q)
    }

    fn read_msg(&mut self) -> Result<serde_json::Value> {
        let mut line = String::new();
        if self.r.read_line(&mut line)? == 0 {
            bail!("QMP connection closed");
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// Run a command; events that arrive in between are skipped.
    pub fn exec(&mut self, cmd: &str) -> Result<serde_json::Value> {
        writeln!(self.w, "{}", serde_json::json!({ "execute": cmd }))?;
        loop {
            let m = self.read_msg()?;
            if let Some(e) = m.get("error") {
                bail!("QMP {cmd}: {e}");
            }
            if let Some(r) = m.get("return") {
                return Ok(r.clone());
            }
        }
    }
}

/// Send one argument-less QMP command (system_powerdown, system_reset, stop,
/// cont); true when QEMU accepted it.
pub fn ask(qmp_sock: &Path, cmd: &str) -> bool {
    Qmp::connect(qmp_sock)
        .and_then(|mut q| q.exec(cmd).map(|_| ()))
        .is_ok()
}

/// Ask QEMU to exit now (the guest is not asked). True when it exited.
pub fn quit(qmp_sock: &Path, pid: i32) -> bool {
    if let Ok(mut q) = Qmp::connect(qmp_sock) {
        let _ = q.exec("quit");
    }
    sys::wait_for(Duration::from_secs(5), || !sys::alive(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn command_line_matches_docs() {
        let mut c = VmConfig::new("t", 4096, 6, 3, "me", "none");
        c.disk = PathBuf::from("/vms/t/disk.img");
        c.kernel_args = "quiet".into();
        let p = Paths {
            kernel: Path::new("/k/vmlinux"),
            initrd: Some(Path::new("/k/initrd.img")),
            gpu_sock: Path::new("/run/gpu.sock"),
            vfs_sock: Path::new("/run/vfs.sock"),
            qmp_sock: Path::new("/run/qmp.sock"),
            console_log: Path::new("/vms/t/logs/vm.log"),
        };
        let a = args(&c, &p, Some(Audio::PipeWire));
        let j = joined(&a);
        for want in [
            "-machine q35,accel=kvm,memory-backend=mem",
            "-cpu host,host-phys-bits=on",
            "-smp 6 -m 4096M",
            "memory-backend-memfd,id=mem,size=4096M,share=on",
            "-display none",
            "-initrd /k/initrd.img",
            "-kernel /k/vmlinux -append console=ttyS0 root=/dev/vda rw quiet",
            "file=/vms/t/disk.img,format=raw,if=virtio",
            "tap,id=net0,ifname=conduit3,script=no,downscript=no",
            "virtio-net-pci,netdev=net0,mac=02:00:00:00:03:01",
            "vhost-user-fs-pci,chardev=vfs,tag=nvidia",
            "socket,id=nvgpu,path=/run/gpu.sock",
            "vhost-user-test-device-pci,chardev=nvgpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036",
            "-qmp unix:/run/qmp.sock,server=on,wait=off",
            "-audiodev pipewire,id=snd0",
            "virtio-sound-pci,audiodev=snd0,streams=2",
        ] {
            assert!(j.contains(want), "missing {want:?} in {j}");
        }
        // `-name ...,process=` would change the process name the pid checks rely on.
        assert!(!j.contains("process="));
        let quiet = args(&c, &p, None);
        assert!(!joined(&quiet).contains("audiodev"));
    }

    #[test]
    fn audio_driver_names() {
        assert_eq!(Audio::PipeWire.driver(), "pipewire");
        assert_eq!(Audio::Pulse.driver(), "pa");
    }
}
