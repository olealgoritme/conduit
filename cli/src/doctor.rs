//! `conduit doctor`: check this computer, explain fixes in plain words.

use crate::host;
use crate::paths::{self, Tool};
use crate::sys;
use crate::ui;
use std::path::Path;

enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report {
    fails: usize,
    warns: usize,
}

impl Report {
    fn line(&mut self, lvl: Level, what: &str, detail: &str, fix: &str) {
        let tag = match lvl {
            Level::Ok => "  ok  ",
            Level::Warn => {
                self.warns += 1;
                " warn "
            }
            Level::Fail => {
                self.fails += 1;
                " FAIL "
            }
        };
        println!("[{tag}] {what}: {detail}");
        if !fix.is_empty() && !matches!(lvl, Level::Ok) {
            for l in fix.lines() {
                println!("         {l}");
            }
        }
    }
}

fn can_open_rw(p: &str) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(p)
        .is_ok()
}

fn in_group(g: &str) -> bool {
    sys::output("id", &["-Gn"])
        .map(|s| s.split_whitespace().any(|x| x == g))
        .unwrap_or(false)
}

pub fn run() -> i32 {
    let mut r = Report { fails: 0, warns: 0 };
    println!("Checking this computer for Conduit…\n");

    // KVM
    if !Path::new("/dev/kvm").exists() {
        r.line(Level::Fail, "KVM", "not available (/dev/kvm is missing)",
            "Turn on virtualization in your BIOS/UEFI settings (called VT-x, VT-d, AMD-V or SVM),\nthen restart. If it is on, load the module: sudo modprobe kvm_intel  (or kvm_amd)");
    } else if !can_open_rw("/dev/kvm") {
        r.line(Level::Fail, "KVM", "present, but you may not use it",
            &format!("Add yourself to the kvm group, then log out and back in:\n  sudo usermod -aG kvm {}", paths::username()));
    } else {
        r.line(Level::Ok, "KVM", "available", "");
    }

    // NVIDIA driver
    let (supported, from) = host::supported_drivers();
    match host::driver() {
        None => r.line(Level::Fail, "NVIDIA driver", "not loaded",
            "Install NVIDIA's driver with the OPEN kernel modules, version 580 or newer\n(Ubuntu: sudo apt install nvidia-driver-580-open), then restart."),
        Some(d) => {
            if !d.open {
                r.line(Level::Fail, "NVIDIA driver", &format!("{} uses the closed kernel modules", d.version),
                    "Conduit needs the OPEN kernel modules. On Ubuntu install the -open package\n(e.g. nvidia-driver-580-open) and restart.");
            } else if host::major(&d.version) < 580 {
                r.line(Level::Fail, "NVIDIA driver", &format!("{} is too old", d.version),
                    "Update to version 580 or newer (open kernel modules), then restart.");
            } else {
                r.line(Level::Ok, "NVIDIA driver", &format!("{} (open kernel modules)", d.version), "");
            }
            if supported.iter().any(|v| v == &d.version) {
                r.line(Level::Ok, "Driver support", &format!("Conduit knows driver {}", d.version), "");
            } else {
                r.line(Level::Fail, "Driver support", &format!("driver {} is not one Conduit supports yet ({from}: {})", d.version, supported.join(", ")),
                    "Each NVIDIA driver release needs a matching Conduit update. Update Conduit,\nor install one of the listed driver versions.");
            }
        }
    }
    for dev in ["/dev/nvidiactl", "/dev/nvidia-uvm"] {
        if Path::new(dev).exists() && !can_open_rw(dev) {
            r.line(Level::Warn, "GPU access", &format!("you cannot open {dev}"), "Usually fixed by logging in on the desktop as yourself, or the 'video'/'render' group.");
        }
    }
    if !Path::new("/dev/nvidia-uvm").exists() {
        r.line(
            Level::Warn,
            "CUDA",
            "/dev/nvidia-uvm is missing (CUDA in VMs will not work)",
            "Load it once: sudo modprobe nvidia-uvm   (or run nvidia-smi once)",
        );
    }

    // Desktop
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(w) if paths::xdg_runtime().join(&w).exists() || Path::new(&w).is_absolute() => {
            let desk = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown desktop".into());
            r.line(Level::Ok, "Desktop", &format!("Wayland session ({desk})"), "");
        }
        _ => r.line(Level::Warn, "Desktop", "no Wayland session in this terminal",
            "`conduit view` must run from a terminal inside your Wayland desktop (GNOME, KDE,\nHyprland, Sway...). `conduit up` works anywhere."),
    }

    let (m, src) = crate::mode::detect();
    r.line(
        Level::Ok,
        "Display",
        &format!("VMs will get {m} (from {src}); `conduit view NAME WxH@HZ` overrides"),
        "",
    );

    // sudo
    if unsafe { libc::geteuid() } == 0 {
        r.line(
            Level::Warn,
            "sudo",
            "you are running as root",
            "Run conduit as your normal user; it asks for sudo only when it needs it.",
        );
    } else if !sys::have("sudo") {
        r.line(
            Level::Fail,
            "sudo",
            "not installed",
            "Install sudo: Conduit needs it to set up the VM network and to build disks.",
        );
    } else if in_group("sudo")
        || in_group("wheel")
        || in_group("admin")
        || sys::quiet("sudo", &["-n", "true"])
    {
        r.line(
            Level::Ok,
            "sudo",
            "available (asked only for network setup and disk building)",
            "",
        );
    } else {
        r.line(
            Level::Warn,
            "sudo",
            "your user may not be allowed to use sudo",
            "Ask an administrator to add you to the 'sudo' (or 'wheel') group.",
        );
    }

    // Tools
    let mut missing: Vec<&str> = [
        "ip",
        "iptables",
        "ssh",
        "curl",
        "mkfs.ext4",
        "tar",
        "xz",
        "sha256sum",
    ]
    .into_iter()
    .filter(|t| !sys::have(t))
    .collect();
    if !Path::new("/dev/net/tun").exists() {
        missing.push("/dev/net/tun");
    }
    if missing.is_empty() {
        r.line(Level::Ok, "Tools", "network and disk tools present", "");
    } else {
        r.line(Level::Fail, "Tools", &format!("missing: {}", missing.join(", ")),
            "Ubuntu: sudo apt install iproute2 iptables openssh-client curl e2fsprogs xz-utils coreutils");
    }

    // Conduit's own parts
    for t in [
        Tool::Backend,
        Tool::Vmm,
        Tool::Viewer,
        Tool::Kernel,
        Tool::GuestModule,
        Tool::Userspace,
    ] {
        match t.find() {
            Some(p) => r.line(Level::Ok, t.label(), &p.display().to_string(), ""),
            None => r.line(Level::Fail, t.label(), "not found",
                &format!("Reinstall the conduit package. In a source checkout, build it first.\n(looked in {} and the source checkout; or point to it with {}=/path)", paths::prefix().display(), t.env_var())),
        }
    }

    // Disk space
    let data = paths::data_dir();
    let probe = if data.exists() {
        data.clone()
    } else {
        paths::home()
    };
    if let Some(free) = sys::free_bytes(&probe) {
        let msg = format!(
            "{} free for VMs ({})",
            ui::human_bytes(free),
            data.display()
        );
        if free < 12 << 30 {
            r.line(Level::Fail, "Disk space", &msg, "A new VM needs about 12 GB (disks are sparse and grow as you use them). Free some space.");
        } else if free < 40 << 30 {
            r.line(
                Level::Warn,
                "Disk space",
                &msg,
                "Enough for one VM; games and apps inside need more.",
            );
        } else {
            r.line(Level::Ok, "Disk space", &msg, "");
        }
    }

    // QEMU (only matters for `conduit attach`)
    match host::system_qemu() {
        Some((_, v)) if v >= crate::libvirt::MIN_QEMU => r.line(Level::Ok, "QEMU (for attach)", &format!("system QEMU {}.{}", v.0, v.1), ""),
        Some((_, v)) => r.line(Level::Ok, "QEMU (for attach)", &format!("system QEMU {}.{} is older than 11.1; Conduit will use its own (your QEMU is not touched)", v.0, v.1), ""),
        None => r.line(Level::Ok, "QEMU (for attach)", "not installed (only needed for libvirt VMs)", ""),
    }

    println!();
    if r.fails == 0 && r.warns == 0 {
        println!("All good. Next: conduit create myvm");
    } else if r.fails == 0 {
        println!("Ready, with {} warning(s) above.", r.warns);
    } else {
        println!(
            "{} problem(s) to fix first; see the lines marked FAIL.",
            r.fails
        );
    }
    if r.fails > 0 {
        1
    } else {
        0
    }
}
