//! `conduit doctor`: check this computer, explain fixes in plain words.

use crate::host;
use crate::paths::{self, Tool};
use crate::sys;
use crate::ui;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// What the kernel-module line says about a loaded driver.
///
/// Support is decided by the exact-release tables (`known`): a release with
/// them is one the backend starts on. The open modules and the 580 floor are
/// what Conduit was built and tested on, so for a release that has tables a
/// closed module or a branch older than 580 is a warning that it is untested;
/// a release without tables stays a failure.
fn module_verdict(d: &host::Driver, known: bool) -> (Level, String, &'static str) {
    if known {
        return match host::untested_because(d) {
            None => (Level::Ok, format!("{} (open kernel modules)", d.version), ""),
            Some(what) => (
                Level::Warn,
                format!("{} uses {what}; Conduit has tables for this release but is untested with it", d.version),
                "Conduit is developed and tested on the OPEN kernel modules, version 580 or newer\n(Ubuntu: nvidia-driver-580-open). This setup may work; if it does not, switch to that.",
            ),
        };
    }
    if !d.open {
        (Level::Fail, format!("{} uses the closed kernel modules", d.version),
            "Conduit needs the OPEN kernel modules. On Ubuntu install the -open package\n(e.g. nvidia-driver-580-open) and restart.")
    } else if host::major(&d.version) < 580 {
        (
            Level::Fail,
            format!("{} is too old", d.version),
            "Update to version 580 or newer (open kernel modules), then restart.",
        )
    } else {
        (
            Level::Ok,
            format!("{} (open kernel modules)", d.version),
            "",
        )
    }
}

/// The Safe mode line: whether the backend starts in safe mode and why, from
/// the environment, `gpu.safe_mode` and the driver (protect.rs decides).
fn safe_mode_report(d: &host::Driver) -> String {
    safe_mode_text(
        std::env::var("CONDUIT_SAFE_MODE").ok().as_deref(),
        crate::config::safe_setting(),
        d,
    )
}

fn safe_mode_text(
    env: Option<&str>,
    setting: crate::config::SafeSetting,
    d: &host::Driver,
) -> String {
    use crate::config::SafeSetting::*;
    let protection = crate::protect::protection(Some(d));
    let on = crate::protect::safe_mode(env, setting, protection);
    // What a start that does not inherit this shell gets (libvirt units and
    // virt-manager run under systemd's environment): the setting, then auto.
    let unit_on = crate::protect::safe_mode(None, setting, protection);
    let auto = || match host::untested_because(d) {
        Some(what) => format!("{} ({what}) is untested", d.version),
        None => format!(
            "{} on the open kernel modules is what Conduit is tested on",
            d.version
        ),
    };
    let by_setting = match setting {
        On => "the setting (gpu.safe_mode is true)".to_string(),
        Off => "the setting (gpu.safe_mode is false)".to_string(),
        Auto => format!("auto, from the driver: {}", auto()),
    };
    let decided = match env {
        Some("1" | "0") => format!(
            "decided by the environment (CONDUIT_SAFE_MODE={} in this shell)",
            env.unwrap_or_default()
        ),
        _ => format!("decided by {by_setting}"),
    };
    let shell_note = match env {
        Some("1" | "0") => format!(
            "\n         A libvirt or virt-manager start does not see this shell: it follows gpu.safe_mode, \
             which gives safe mode {} ({})",
            if unit_on { "ON" } else { "off" },
            by_setting
        ),
        _ => String::new(),
    };
    if on {
        format!(
            "ON, {decided}: video memory capped at 2 GiB (a smaller gpu.vram_limit_mib stays) and 1 s limits on blocking GPU calls{shell_note}\n         `conduit config set gpu.safe_mode false` turns it off for every start"
        )
    } else {
        format!("off, {decided}{shell_note}\n         `conduit config set gpu.safe_mode true` turns it on for every start")
    }
}

/// The Debian/Ubuntu packages behind the tools the `Tools` check looks for.
pub const TOOL_PACKAGES_APT: &[&str] = &[
    "iproute2",
    "iptables",
    "openssh-client",
    "curl",
    "e2fsprogs",
    "xz-utils",
    "coreutils",
];

/// The same tools under Fedora and Arch names.
const TOOL_PACKAGES_DNF: &[&str] = &[
    "iproute",
    "iptables",
    "openssh-clients",
    "curl",
    "e2fsprogs",
    "xz",
    "coreutils",
];
const TOOL_PACKAGES_PACMAN: &[&str] = &[
    "iproute2",
    "iptables",
    "openssh",
    "curl",
    "e2fsprogs",
    "xz",
    "coreutils",
];

/// What to know before touching the driver. Conduit changes nothing here.
fn driver_guide(loaded: Option<&host::Driver>, supported: &[String]) -> String {
    let now = match loaded {
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
        if supported.is_empty() {
            "(the list could not be read)".to_string()
        } else {
            supported.join(", ")
        }
    )
}

/// A missing part of Conduit itself: the doctor's words for this part, the
/// wizard's pointer to the packages.
fn conduit_part_remedy(hint: &str) -> Remedy {
    Remedy::explain(
        hint,
        format!(
            "Install the Conduit package for your distribution ({}), or, from a source checkout, build it with packaging/build.sh (the build packages step above installs what it needs).",
            crate::setup::data::link("conduit-releases")
        ),
    )
}

/// One finding of a check: what `conduit doctor` prints, and what `conduit
/// setup` turns into steps. `id` is a stable key (the title as a slug, or an
/// explicit one where one title covers causes with different fixes).
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub id: String,
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// The one thing to do about it; what the doctor prints and what the
    /// setup wizard offers both come from this.
    pub remedy: Option<Remedy>,
}

impl Check {
    /// The doctor's words for the fix, lines separated by `\n`; empty when
    /// there is no remedy.
    pub fn hint(&self) -> String {
        self.remedy.as_ref().map(Remedy::hint).unwrap_or_default()
    }
}

/// How to fix a finding, built where the check is made (the one place that
/// knows what is wrong). The doctor prints its `hint`; the wizard turns it
/// into a command or a guide for the host's distribution.
#[derive(Debug, Clone, PartialEq)]
pub enum Remedy {
    /// Words only. The hint and the wizard's guide are these words.
    Guide { text: String },
    /// A command only an administrator can run (`sudo` is added); `intro`
    /// is the sentence before it.
    Sudo { intro: String, cmd: Vec<String> },
    /// Install packages; `hint` is the doctor's words, the lists are
    /// per distribution family.
    Install {
        hint: String,
        apt: Vec<String>,
        dnf: Vec<String>,
        pacman: Vec<String>,
    },
    /// Short words for the doctor, a longer guide for the wizard.
    Explain { hint: String, guide: String },
}

impl Remedy {
    pub fn guide(text: impl Into<String>) -> Remedy {
        Remedy::Guide { text: text.into() }
    }

    pub fn sudo(intro: &str, cmd: &[&str]) -> Remedy {
        Remedy::Sudo {
            intro: intro.into(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn install(hint: impl Into<String>, apt: &[&str], dnf: &[&str], pacman: &[&str]) -> Remedy {
        let own = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Remedy::Install {
            hint: hint.into(),
            apt: own(apt),
            dnf: own(dnf),
            pacman: own(pacman),
        }
    }

    pub fn explain(hint: impl Into<String>, guide: impl Into<String>) -> Remedy {
        Remedy::Explain {
            hint: hint.into(),
            guide: guide.into(),
        }
    }

    /// What `conduit doctor` prints under a finding.
    pub fn hint(&self) -> String {
        match self {
            Remedy::Guide { text } => text.clone(),
            Remedy::Sudo { intro, cmd } => format!("{intro}\n  sudo {}", cmd.join(" ")),
            Remedy::Install { hint, .. } | Remedy::Explain { hint, .. } => hint.clone(),
        }
    }

    /// Does the wizard have something to say beyond the hint?
    pub fn says_more_than_hint(&self) -> bool {
        !matches!(self, Remedy::Guide { .. })
    }
}

/// "NVIDIA driver" -> "nvidia-driver".
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// The one printer: `[ tag ] title: detail`, then the hint of anything not ok.
pub fn render_check(c: &Check) -> String {
    let tag = match c.level {
        Level::Ok => "  ok  ",
        Level::Warn => " warn ",
        Level::Fail => " FAIL ",
    };
    let mut s = format!("[{tag}] {}: {}\n", c.title, c.detail);
    let hint = c.hint();
    if c.level != Level::Ok {
        for l in hint.lines() {
            s.push_str(&format!("         {l}\n"));
        }
    }
    s
}

pub fn count(checks: &[Check], level: Level) -> usize {
    checks.iter().filter(|c| c.level == level).count()
}

/// Collects checks; with `stream` each is printed as it is made.
struct Report {
    checks: Vec<Check>,
    stream: bool,
}

impl Report {
    fn new(stream: bool) -> Report {
        Report {
            checks: Vec::new(),
            stream,
        }
    }

    /// A finding whose fix is plain words (none when `fix` is empty).
    fn line(&mut self, lvl: Level, what: &str, detail: &str, fix: &str) {
        self.line_remedy(
            &slug(what),
            lvl,
            what,
            detail,
            (!fix.is_empty()).then(|| Remedy::guide(fix)),
        );
    }

    fn line_remedy(
        &mut self,
        id: &str,
        level: Level,
        title: &str,
        detail: &str,
        remedy: Option<Remedy>,
    ) {
        let c = Check {
            id: id.into(),
            level,
            title: title.into(),
            detail: detail.into(),
            remedy,
        };
        if self.stream {
            print!("{}", render_check(&c));
        }
        self.checks.push(c);
    }

    fn fails(&self) -> usize {
        count(&self.checks, Level::Fail)
    }

    fn warns(&self) -> usize {
        count(&self.checks, Level::Warn)
    }
}

/// May this user open `p` for reading and writing? Answered by access(2),
/// from the permissions alone: nothing is opened, so probing a GPU node
/// never creates a client of the card that may be driving the desktop.
fn may_open_rw(p: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(p) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated string that outlives the call.
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

fn in_group(g: &str) -> bool {
    sys::output("id", &["-Gn"])
        .map(|s| s.split_whitespace().any(|x| x == g))
        .unwrap_or(false)
}

/// The lines about this GPU's role on the host and whether a guest can be
/// given its driver files: the display, BAR1, GSP firmware, the staged share.
fn gpu_host_lines(r: &mut Report) {
    if let Some(card) = crate::protect::nvidia_card(Path::new("/sys/class/drm")) {
        if card.drives_display() {
            let shown: Vec<&str> = card
                .connectors
                .iter()
                .filter(|c| c.connected)
                .map(|c| {
                    c.name
                        .trim_start_matches(|ch: char| ch != '-')
                        .trim_start_matches('-')
                })
                .collect();
            let plan = crate::protect::Plan::current();
            let cap = plan.vram_limit_mib;
            r.line(
                Level::Warn,
                "Display GPU",
                &format!(
                    "the NVIDIA GPU has a monitor connected ({}{}); a guest shares its memory with your desktop",
                    shown.join(", "),
                    if card.display_active() { ", in use" } else { "" }
                ),
                &format!(
                    "{}\n{}",
                    match cap {
                        Some(m) => format!("Guests are capped at {m} MiB of video memory (conduit config set gpu.vram_limit_mib N|off to change)."),
                        None => "No video-memory cap is set: a guest can fill the card your desktop runs on.\nSet one: conduit config set gpu.vram_limit_mib auto  (or a number of MiB)".to_string(),
                    },
                    if plan.safe_mode {
                        "Safe mode is on (see the Safe mode line)."
                    } else {
                        "For a first run: conduit config set gpu.safe_mode true (a cap of at most 2 GiB and 1 s limits on blocking GPU calls)."
                    }
                ),
            );
        } else {
            r.line(
                Level::Ok,
                "Display GPU",
                "no monitor is connected to the NVIDIA GPU",
                "",
            );
        }
        match card.bar1_mib() {
            Some(256) => r.line(
                Level::Warn,
                "BAR1",
                "256 MiB (Resizable BAR is off): guest mappings of GPU memory go through a small window",
                "Turn on Above 4G Decoding and Resizable BAR in the firmware settings for large guest workloads.",
            ),
            Some(m) => r.line(Level::Ok, "BAR1", &format!("{m} MiB"), ""),
            None => {}
        }
    }
    if std::fs::read_to_string("/proc/driver/nvidia/params").is_ok_and(|p| gpu_firmware_off(&p)) {
        r.line(
            Level::Warn,
            "GPU firmware",
            "NVreg_EnableGpuFirmware=0: the driver runs without the GSP firmware",
            "Conduit's forwarding rules assume GSP firmware (the default on current GPUs). Remove\nthe NVreg_EnableGpuFirmware=0 module option.",
        );
    }
    match crate::paths::Tool::Userspace.find() {
        None => r.line(
            Level::Warn,
            "Driver files",
            "conduit-userspace not found, so the files a guest needs were not checked",
            "Reinstall the conduit package (in a source checkout: cargo build --release in host/backend)",
        ),
        Some(tool) => {
            let out = std::process::Command::new(tool)
                .args(["--caps", "graphics,video,utility,compute"])
                .output();
            let (lvl, detail, fix) = match out {
                Ok(o) => staging_verdict(
                    o.status.success(),
                    &String::from_utf8_lossy(&o.stdout),
                    &String::from_utf8_lossy(&o.stderr),
                ),
                Err(e) => (Level::Warn, format!("could not run conduit-userspace: {e}"), String::new()),
            };
            r.line(lvl, "Driver files", &detail, &fix);
        }
    }
}

/// Whether `/proc/driver/nvidia/params` shows GPU firmware switched off.
fn gpu_firmware_off(params: &str) -> bool {
    params.lines().any(|l| {
        l.split_once(':')
            .is_some_and(|(k, v)| k.trim() == "EnableGpuFirmware" && v.trim() == "0")
    })
}

/// What `conduit-userspace` (plan only) says about staging the driver files:
/// a failure when it cannot plan, a warning naming the files that are listed
/// but not installed.
fn staging_verdict(ok: bool, stdout: &str, stderr: &str) -> (Level, String, String) {
    if !ok {
        let why = stderr.lines().last().unwrap_or("").trim().to_string();
        return (
            Level::Fail,
            format!("the driver files for a guest cannot be listed: {why}"),
            "A VM cannot start without them. Install the driver's userspace packages for the\nloaded kernel module (same version), then run conduit doctor again.".into(),
        );
    }
    let mut missing = Vec::new();
    let mut in_missing = false;
    for l in stdout.lines() {
        if l.contains("listed but not installed") {
            in_missing = true;
        } else if in_missing && l.starts_with("  ") {
            missing.push(l.trim().to_string());
        } else {
            in_missing = false;
        }
    }
    let wanted = stdout
        .lines()
        .find(|l| l.contains("wanted here"))
        .unwrap_or("")
        .trim()
        .to_string();
    if missing.is_empty() {
        (
            Level::Ok,
            format!("ready to stage ({wanted})"),
            String::new(),
        )
    } else {
        (
            Level::Warn,
            format!("{wanted}; not installed here: {}", missing.join(", ")),
            "Guests lack these. Install the matching driver packages if you need that feature\n(video decode, Wayland EGL...).".into(),
        )
    }
}

/// Every host check, in the order `conduit doctor` prints them.
pub fn host_checks() -> Vec<Check> {
    let mut r = Report::new(false);
    collect_host(&mut r);
    r.checks
}

#[cfg(test)]
pub fn driver_checks_for_test(driver: Option<host::Driver>, supported: &[String]) -> Vec<Check> {
    let mut r = Report::new(false);
    driver_checks(&mut r, driver, supported, "test");
    r.checks
}

/// The driver lines: what is loaded, whether Conduit knows it, safe mode.
fn driver_checks(r: &mut Report, driver: Option<host::Driver>, supported: &[String], from: &str) {
    match driver {
        None => r.line_remedy("nvidia-driver", Level::Fail, "NVIDIA driver", "not loaded",
            Some(Remedy::explain("Install NVIDIA's driver with the OPEN kernel modules, version 580 or newer\n(Ubuntu: sudo apt install nvidia-driver-580-open), then restart.", driver_guide(None, supported)))),
        Some(d) => {
            let known = supported.iter().any(|v| host::same_release(v, &d.version));
            let (lvl, detail, fix) = module_verdict(&d, known);
            r.line_remedy("nvidia-driver", lvl, "NVIDIA driver", &detail,
                (!fix.is_empty()).then(|| Remedy::explain(fix, driver_guide(Some(&d), supported))));
            r.line(Level::Ok, "Safe mode", &safe_mode_report(&d), "");
            if known {
                r.line(Level::Ok, "Driver support", &format!("Conduit knows driver {}", d.version), "");
            } else {
                r.line_remedy("driver-support", Level::Fail, "Driver support", &format!("driver {} is not one Conduit supports yet ({from}: {})", d.version, supported.join(", ")),
                    Some(Remedy::explain("Each NVIDIA driver release needs a matching Conduit update. Update Conduit,\nor install one of the listed driver versions.", driver_guide(Some(&d), supported))));
            }
        }
    }
}

fn collect_host(r: &mut Report) {
    // KVM
    if !Path::new("/dev/kvm").exists() {
        r.line(Level::Fail, "KVM", "not available (/dev/kvm is missing)",
            "Turn on virtualization in your BIOS/UEFI settings (called VT-x, VT-d, AMD-V or SVM),\nthen restart. If it is on, load the module: sudo modprobe kvm_intel  (or kvm_amd)");
    } else if !may_open_rw("/dev/kvm") {
        r.line_remedy(
            "kvm-access",
            Level::Fail,
            "KVM",
            "present, but you may not use it",
            Some(Remedy::sudo(
                "Add yourself to the kvm group, then log out and back in:",
                &["usermod", "-aG", "kvm", &paths::username()],
            )),
        );
    } else {
        r.line(Level::Ok, "KVM", "available", "");
    }

    // NVIDIA driver
    let (supported, from) = host::supported_drivers();
    driver_checks(r, host::driver(), &supported, from);
    // Which GPU. Informational: any Turing or later GPU works (docs/GPU-SUPPORT.md). Whether
    // Resizable BAR is on is the BAR1 line's (gpu_host_lines).
    for g in host::gpus() {
        let name = g
            .model
            .clone()
            .unwrap_or_else(|| format!("NVIDIA device {:#06x}", g.device));
        let bar1 = g.bar1.map(ui::human_bytes).unwrap_or_else(|| "?".into());
        r.line(
            Level::Ok,
            "GPU",
            &format!("{name} at {}, BAR1 {bar1}", g.addr),
            "",
        );
    }
    for dev in ["/dev/nvidiactl", "/dev/nvidia-uvm"] {
        if Path::new(dev).exists() && !may_open_rw(dev) {
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

    if host::driver().is_some() {
        gpu_host_lines(r);
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
        r.line_remedy(
            "tools",
            Level::Fail,
            "Tools",
            &format!("missing: {}", missing.join(", ")),
            Some(Remedy::install(
                format!("Ubuntu: sudo apt install {}", TOOL_PACKAGES_APT.join(" ")),
                TOOL_PACKAGES_APT,
                TOOL_PACKAGES_DNF,
                TOOL_PACKAGES_PACMAN,
            )),
        );
    }

    // Conduit's own parts
    // The VM runner: the bundled QEMU, or the built-in one as a fallback.
    let qemu = Tool::BundledQemu.find();
    match (&qemu, crate::qemu::virtiofsd(), Tool::Vmm.find()) {
        (Some(q), Some(_), _) => r.line(Level::Ok, "VM runner", &format!("QEMU {}", q.display()), ""),
        (Some(_), None, _) => r.line_remedy("virtiofsd", Level::Fail, "VM runner", "QEMU found, but virtiofsd is missing",
            Some(Remedy::install("Ubuntu/Debian: sudo apt install virtiofsd   Fedora: sudo dnf install virtiofsd", &["virtiofsd"], &["virtiofsd"], &["virtiofsd"]))),
        (None, _, Some(v)) => r.line(Level::Warn, "VM runner",
            &format!("bundled QEMU missing; only the built-in runner ({})", v.display()),
            "The built-in runner has no sound and cannot boot a VM's own (stock) kernel. Reinstall the conduit package, or build host/qemu."),
        (None, _, None) => r.line_remedy("conduit-part", Level::Fail, "VM runner", "neither the bundled QEMU nor the built-in runner was found",
            Some(conduit_part_remedy("Reinstall the conduit package. In a source checkout: host/qemu/build-qemu.sh"))),
    }

    for t in [Tool::Backend, Tool::Viewer, Tool::GuestDeb, Tool::Userspace] {
        match t.find() {
            Some(p) => r.line(Level::Ok, t.label(), &p.display().to_string(), ""),
            None => r.line_remedy("conduit-part", Level::Fail, t.label(), "not found",
                Some(conduit_part_remedy(&format!("Reinstall the conduit package. In a source checkout, build it first.\n(looked in {} and the source checkout; or point to it with {}=/path)", paths::prefix().display(), t.env_var())))),
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

    // Conduit's QEMU and libvirt (virt-manager / virsh)
    match Tool::BundledQemu.find() {
        Some(q) => {
            let v = sys::output(&q.to_string_lossy(), &["--version"])
                .ok()
                .and_then(|o| host::parse_qemu_version(&o));
            match v {
                Some(v) if v >= (11, 1) => r.line(
                    Level::Ok,
                    "Conduit's QEMU",
                    &format!("{} ({}.{})", q.display(), v.0, v.1),
                    "",
                ),
                _ => r.line(
                    Level::Warn,
                    "Conduit's QEMU",
                    &format!("{} is not QEMU 11.1 or newer", q.display()),
                    "Reinstall the conduit package (source checkout: host/qemu/build-qemu.sh)",
                ),
            }
        }
        None => r.line(
            Level::Warn,
            "Conduit's QEMU",
            "missing (VMs fall back to the built-in runner, without sound or libvirt)",
            "Reinstall the conduit package (in a source checkout: host/qemu/build-qemu.sh)",
        ),
    }
    if !sys::have("virsh") {
        r.line(
            Level::Ok,
            "libvirt",
            "not installed (optional: manage VMs from virt-manager)",
            "",
        );
    } else if crate::virt::session_available() {
        r.line(
            Level::Ok,
            "libvirt",
            "user session (qemu:///session) reachable: `conduit create` registers VMs there",
            "",
        );
    } else {
        r.line(
            Level::Warn,
            "libvirt",
            "installed, but the user session (qemu:///session) does not answer",
            "Run: virsh -c qemu:///session list   to see why",
        );
    }
}

pub fn run() -> i32 {
    let mut r = Report::new(true);
    println!("Checking this computer for Conduit…\n");
    collect_host(&mut r);
    println!();
    let (fails, warns) = (r.fails(), r.warns());
    if fails == 0 && warns == 0 {
        println!("All good. Next: conduit create myvm");
    } else if fails == 0 {
        println!("Ready, with {warns} warning(s) above.");
    } else {
        println!("{fails} problem(s) to fix first; see the lines marked FAIL.");
    }
    if fails > 0 {
        1
    } else {
        0
    }
}

/// `conduit doctor NAME`: one VM's whole chain, in the order a start uses it.
pub fn run_vm(name: &str) -> i32 {
    use crate::units;
    use crate::virt::{self, Kind, Link};
    let mut r = Report::new(true);
    println!("Checking the VM {name}…\n");
    let cfg = crate::vm::VmConfig::load(name).ok();
    let link = Link::load(name);
    if cfg.is_none() && link.is_none() {
        r.line(
            Level::Fail,
            "VM",
            "unknown",
            "See `conduit list`; `conduit attach NAME` adds Conduit to a virt-manager VM",
        );
        return 1;
    }
    if let Some(c) = &cfg {
        let disk = c.disk_path();
        if disk.is_file() {
            r.line(Level::Ok, "Disk", &disk.display().to_string(), "");
        } else {
            r.line(
                Level::Fail,
                "Disk",
                &format!("missing: {}", disk.display()),
                "Re-create or `conduit import` the VM",
            );
        }
        match &c.kernel {
            Some(k) if !k.is_file() => r.line(
                Level::Fail,
                "Kernel",
                &format!("missing: {}", k.display()),
                "Fix \"kernel\" in vm.json",
            ),
            Some(k) => r.line(Level::Ok, "Kernel", &k.display().to_string(), ""),
            None => r.line(
                Level::Ok,
                "Kernel",
                "the one installed on the VM's disk (copied out at each start)",
                "",
            ),
        }
    }
    for (t, fix) in [
        (
            Tool::Backend,
            "Reinstall the conduit package, or build it in a checkout",
        ),
        (
            Tool::BundledQemu,
            "Reinstall the conduit package (source checkout: host/qemu/build-qemu.sh)",
        ),
    ] {
        // A libvirt VM names its QEMU in the domain (checked below).
        if t == Tool::BundledQemu && Link::load(name).is_some() {
            continue;
        }
        match t.find() {
            Some(p) => r.line(Level::Ok, t.label(), &p.display().to_string(), ""),
            None => r.line(Level::Fail, t.label(), "missing", fix),
        }
    }
    match crate::qemu::virtiofsd() {
        Some(p) => r.line(Level::Ok, "virtiofsd", &p.display().to_string(), ""),
        None => r.line(
            Level::Fail,
            "virtiofsd",
            "missing",
            "sudo apt install virtiofsd",
        ),
    }
    let Some(link) = link else {
        r.line(
            Level::Ok,
            "libvirt",
            "not a libvirt VM: `conduit up/view` run it directly",
            "",
        );
        println!("\n(`conduit libvirt enable {name}` makes it a virt-manager VM)");
        return if r.fails() > 0 { 1 } else { 0 };
    };
    let v = link.virsh();
    if let Err(e) = v.reachable() {
        r.line(Level::Fail, "libvirt", &format!("{e:#}"), "");
        return 1;
    }
    r.line(
        Level::Ok,
        "libvirt",
        &format!(
            "{} ({})",
            link.uri,
            if link.kind == Kind::Managed {
                "made by Conduit"
            } else {
                "attached VM"
            }
        ),
        "",
    );
    let repair = if link.kind == Kind::Managed {
        format!("conduit libvirt enable {name}")
    } else {
        format!("conduit attach {name}")
    };
    let Ok(xml) = v.inactive_xml(&link.domain) else {
        r.line(
            Level::Fail,
            "Domain",
            "not defined in libvirt any more",
            &format!("Define it again: {repair}"),
        );
        return 1;
    };
    let scope = match link.scope() {
        Ok(s) => s,
        Err(e) => {
            r.line(Level::Fail, "Units", &format!("{e:#}"), "");
            return 1;
        }
    };
    let gpu = units::socket_path(&scope, name, "backend");
    let vfs = units::socket_path(&scope, name, "virtiofsd");
    let checks = [
        ("Conduit metadata", virt::is_ours(&xml)),
        (
            "memfd shared memory",
            xml.contains("<source type='memfd'/>") && xml.contains("<access mode='shared'/>"),
        ),
        (
            "GPU device",
            xml.contains("vhost-user-test-device-pci")
                && xml.contains(&format!("path={}", gpu.display())),
        ),
        (
            "NVIDIA share",
            xml.contains(&format!("socket='{}'", vfs.display())),
        ),
        ("Host address width", xml.contains("maxphysaddr")),
    ];
    for (what, ok) in checks {
        if ok {
            r.line(Level::Ok, &format!("Domain: {what}"), "present", "");
        } else {
            r.line(
                Level::Fail,
                &format!("Domain: {what}"),
                "missing or stale",
                &format!("Repair: {repair}"),
            );
        }
    }
    let emu = xml
        .split("<emulator>")
        .nth(1)
        .and_then(|s| s.split("</emulator>").next())
        .unwrap_or("")
        .to_string();
    let emu_ok = std::path::Path::new(&emu).is_file();
    if !emu_ok {
        r.line(
            Level::Fail,
            "Domain: emulator",
            &format!("{emu} does not exist"),
            &format!("Repair: {repair}"),
        );
    } else if !virt::libvirtd_may_exec(std::path::Path::new(&emu)) {
        r.line(
            Level::Fail,
            "Domain: emulator",
            &format!("{emu}: libvirt's AppArmor profile does not allow it"),
            &format!("Repair: {repair} (adds the rule)"),
        );
    } else {
        r.line(Level::Ok, "Domain: emulator", &emu, "");
    }
    for h in units::HELPERS {
        let u = units::unit(h, name, "socket");
        if units::active(&scope, name, h, "socket") {
            r.line(Level::Ok, &format!("Socket {u}"), "listening", "");
        } else {
            r.line(
                Level::Fail,
                &format!("Socket {u}"),
                "not listening",
                &format!("Repair: {repair}"),
            );
        }
    }
    if let Some(c) = &cfg {
        if units::net_installed(name) {
            r.line(Level::Ok, "Network", &crate::net::describe(c), "");
        } else {
            r.line(
                Level::Warn,
                "Network",
                "conduit-net unit missing (`conduit up` sets the tap up with sudo)",
                &format!("Repair: {repair}"),
            );
        }
    }
    let state = v.state(&link.domain).unwrap_or_default();
    r.line(Level::Ok, "State", &state, "");
    let logs = crate::lvrun::logs_dir(name);
    if virt::state_is_up(&state) {
        if units::active(&scope, name, "backend", "service") {
            r.line(Level::Ok, "GPU backend", "running", "");
        } else {
            r.line(Level::Fail, "GPU backend", "not running although the VM is",
                &format!("Its log: conduit logs {name} backend\nRestart the VM (conduit down {name}; conduit up {name})"));
        }
        let ver = if let Some(c) = &cfg {
            crate::run::ssh_cmd(c, "root")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=3",
                    crate::guest::VERSION_PROBE,
                ])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            crate::guest::driver_version_via_agent(&link)
        };
        let host_ver = Tool::GuestDeb.find().and_then(|d| {
            sys::output("dpkg-deb", &["-f", &d.to_string_lossy(), "Version"])
                .ok()
                .map(|s| s.trim().to_string())
        });
        match ver.filter(|v| !v.is_empty()) {
            Some(v) => {
                let same = host_ver
                    .as_deref()
                    .is_some_and(|h| v.starts_with(h.split('-').next().unwrap_or(h)));
                let lvl = if same || host_ver.is_none() {
                    Level::Ok
                } else {
                    Level::Warn
                };
                r.line(lvl, "Guest driver", &format!("{v}{}", host_ver.map(|h| format!(" (host ships {h})")).unwrap_or_default()),
                    &format!("Update it inside the VM: conduit attach {name}  (or install the host's conduit-guest package there)"));
            }
            None => r.line(
                Level::Warn,
                "Guest driver",
                "could not ask the VM (no ssh / guest agent answer)",
                "",
            ),
        }
    } else if let Some(c) = &cfg {
        let avail = crate::mem::available_mib().unwrap_or(0);
        let need = c.ram_mib + crate::mem::overhead_mib(c.ram_mib) + 2048;
        if avail >= need {
            r.line(
                Level::Ok,
                "Memory",
                &format!("{avail} MiB free, the VM needs about {need} MiB"),
                "",
            );
        } else {
            r.line(
                Level::Warn,
                "Memory",
                &format!("only {avail} MiB free, the VM needs about {need} MiB"),
                "Close programs or stop another VM first",
            );
        }
    }
    let blog = logs.join("backend.log");
    if blog.is_file() {
        let t = sys::tail(&blog, 200);
        if let Some(l) = t
            .lines()
            .rev()
            .find(|l| l.contains("ERROR") || l.contains("error:"))
        {
            r.line(
                Level::Warn,
                "Backend log",
                l.trim(),
                &format!("Full log: conduit logs {name} backend"),
            );
        }
    }
    println!();
    if r.fails() == 0 {
        println!(
            "{name}: the chain looks complete{}.",
            if r.warns() > 0 {
                " (see the warnings)"
            } else {
                ""
            }
        );
        0
    } else {
        println!(
            "{name}: {} problem(s); fix the FAIL lines first.",
            r.fails()
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chk(id: &str, level: Level, title: &str, detail: &str, hint: &str) -> Check {
        Check {
            id: id.into(),
            level,
            title: title.into(),
            detail: detail.into(),
            remedy: (!hint.is_empty()).then(|| Remedy::guide(hint)),
        }
    }

    /// The text `conduit doctor` has always printed for these lines.
    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("conduit-doctor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn access_probe_follows_the_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("modes");
        let f = d.join("node");
        std::fs::write(&f, b"").unwrap();
        let p = f.to_str().unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(may_open_rw(p));
        if !is_root() {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(!may_open_rw(p), "read-only is not read-write");
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o200)).unwrap();
            assert!(!may_open_rw(p), "write-only is not read-write");
        }
        assert!(!may_open_rw(d.join("missing").to_str().unwrap()));
        assert!(
            !may_open_rw("/dev/null\0x"),
            "an embedded NUL is not a path"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The probe must not open the node. A directory cannot be opened
    /// read-write (EISDIR) yet access(2) says its permissions allow it, so
    /// only a probe that never calls open() answers true here; the old
    /// open-based one answered false.
    #[test]
    fn access_probe_does_not_open_the_node() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("noopen");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        let opened = std::fs::OpenOptions::new().read(true).write(true).open(&d);
        assert!(
            opened.is_err(),
            "the premise: open(O_RDWR) on a directory fails"
        );
        assert!(may_open_rw(d.to_str().unwrap()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_printed_hint_is_derived_from_the_remedy() {
        let kvm = Remedy::sudo(
            "Add yourself to the kvm group:",
            &["usermod", "-aG", "kvm", "ole"],
        );
        assert_eq!(
            kvm.hint(),
            "Add yourself to the kvm group:\n  sudo usermod -aG kvm ole"
        );
        let c = Check {
            remedy: Some(kvm),
            ..chk("kvm-access", Level::Fail, "KVM", "no", "")
        };
        assert_eq!(
            c.hint(),
            "Add yourself to the kvm group:\n  sudo usermod -aG kvm ole"
        );
        assert!(render_check(&c).ends_with(
            "         Add yourself to the kvm group:\n           sudo usermod -aG kvm ole\n"
        ));
        assert_eq!(chk("a", Level::Ok, "A", "d", "").hint(), "");
    }

    #[test]
    fn printer_output_is_the_established_text() {
        let checks = [
            chk("kvm", Level::Ok, "KVM", "available", ""),
            chk(
                "sudo",
                Level::Warn,
                "sudo",
                "may not be allowed",
                "Ask an administrator\nto add you.",
            ),
            chk(
                "tools",
                Level::Fail,
                "Tools",
                "missing: ip",
                "Ubuntu: sudo apt install iproute2",
            ),
            chk(
                "x",
                Level::Ok,
                "Safe mode",
                "off",
                "a hint that is never shown for ok",
            ),
        ];
        let text: String = checks.iter().map(render_check).collect();
        assert_eq!(
            text,
            "[  ok  ] KVM: available\n\
             [ warn ] sudo: may not be allowed\n         Ask an administrator\n         to add you.\n\
             [ FAIL ] Tools: missing: ip\n         Ubuntu: sudo apt install iproute2\n\
             [  ok  ] Safe mode: off\n"
        );
        assert_eq!(
            (count(&checks, Level::Warn), count(&checks, Level::Fail)),
            (1, 1)
        );
    }

    #[test]
    fn ids_are_slugs_of_titles_unless_given() {
        assert_eq!(slug("NVIDIA driver"), "nvidia-driver");
        assert_eq!(slug("Driver support"), "driver-support");
        assert_eq!(slug("Conduit's QEMU"), "conduit-s-qemu");
        let mut r = Report::new(false);
        r.line(Level::Ok, "KVM", "available", "");
        r.line_remedy("kvm-access", Level::Fail, "KVM", "x", None);
        let ids: Vec<&str> = r.checks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["kvm", "kvm-access"]);
        assert_eq!((r.fails(), r.warns()), (1, 0));
    }

    #[test]
    fn firmware_off_is_read_from_the_params_file() {
        assert!(gpu_firmware_off(
            "ModifyDeviceFiles: 1\nEnableGpuFirmware: 0\n"
        ));
        assert!(!gpu_firmware_off("EnableGpuFirmware: 18\n"));
        assert!(!gpu_firmware_off("EnableGpuFirmwareLogs: 0\n"));
        assert!(!gpu_firmware_off(""));
    }

    #[test]
    fn staging_verdict_fails_without_a_plan_and_names_what_is_missing() {
        let (l, d, _) = staging_verdict(false, "", "Error: no driver manifest\n");
        assert_eq!(l, Level::Fail);
        assert!(d.contains("no driver manifest"), "{d}");
        let ok = "40 entries in the file list, 38 wanted here, 500.0 MiB\n\n2 listed but not installed on this host:\n  libvdpau_nvidia.so.565.77 (lib)\n  10_nvidia_wayland.json (json)\n\nNothing written.\n";
        let (l, d, _) = staging_verdict(true, ok, "");
        assert_eq!(l, Level::Warn);
        assert!(
            d.contains("libvdpau_nvidia.so.565.77 (lib), 10_nvidia_wayland.json (json)"),
            "{d}"
        );
        let (l, _, _) = staging_verdict(
            true,
            "40 entries in the file list, 40 wanted here, 1 MiB\n",
            "",
        );
        assert_eq!(l, Level::Ok);
    }

    fn drv(version: &str, open: bool) -> host::Driver {
        host::Driver {
            version: version.into(),
            open,
        }
    }

    #[test]
    fn open_580_or_newer_is_ok_either_way() {
        assert_eq!(module_verdict(&drv("615.71.09", true), true).0, Level::Ok);
        assert_eq!(module_verdict(&drv("615.71.09", true), false).0, Level::Ok);
    }

    /// The host this was written for: closed 565.77, which has tables.
    #[test]
    fn closed_or_old_with_tables_is_a_warning_that_says_untested() {
        for (d, what) in [
            (
                drv("565.77", false),
                "closed kernel modules and a branch older than 580",
            ),
            (drv("595.104.02", false), "the closed kernel modules"),
            (drv("535.129.03", true), "a branch older than 580"),
        ] {
            let (lvl, detail, fix) = module_verdict(&d, true);
            assert_eq!(lvl, Level::Warn, "{}", d.version);
            assert!(
                detail.contains(what) && detail.contains("untested"),
                "{detail}"
            );
            assert!(!fix.is_empty());
        }
    }

    #[test]
    fn safe_mode_line_says_whether_and_why() {
        use crate::config::SafeSetting::*;
        let closed = safe_mode_text(None, Auto, &drv("565.77", false));
        assert!(
            closed.starts_with(
                "ON, decided by auto, from the driver: 565.77 (the closed kernel modules"
            ),
            "{closed}"
        );
        assert!(closed.contains("gpu.safe_mode false"));
        let proven = safe_mode_text(None, Auto, &drv("610.57.04", true));
        assert!(
            proven.starts_with("off, decided by auto, from the driver: 610.57.04"),
            "{proven}"
        );
        assert!(safe_mode_text(None, On, &drv("610.57.04", true))
            .starts_with("ON, decided by the setting (gpu.safe_mode is true)"));
        assert!(safe_mode_text(None, Off, &drv("565.77", false))
            .starts_with("off, decided by the setting (gpu.safe_mode is false)"));
        // The shell decided: say so, and say what a libvirt start gets instead.
        let shell = safe_mode_text(Some("1"), Auto, &drv("610.57.04", true));
        assert!(
            shell.starts_with("ON, decided by the environment (CONDUIT_SAFE_MODE=1 in this shell)"),
            "{shell}"
        );
        assert!(
            shell.contains("libvirt or virt-manager start does not see this shell")
                && shell.contains("safe mode off (auto, from the driver"),
            "{shell}"
        );
        assert!(
            !proven.contains("libvirt"),
            "no note when the shell did not decide"
        );
    }

    #[test]
    fn closed_or_old_without_tables_still_fails() {
        assert_eq!(module_verdict(&drv("565.78", false), false).0, Level::Fail);
        assert_eq!(
            module_verdict(&drv("595.104.02", false), false).0,
            Level::Fail
        );
        assert_eq!(
            module_verdict(&drv("550.54.14", true), false).0,
            Level::Fail
        );
    }
}
