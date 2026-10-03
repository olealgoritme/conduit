//! Conduit VMs as libvirt domains: start, stop, pause and resume them from
//! virt-manager or virsh like any VM, while Conduit's GPU, display and
//! clipboard keep working.
//!
//! Two kinds of domain carry Conduit's GPU:
//!   * managed: a VM `conduit create`/`import` made, defined by
//!     `conduit libvirt enable NAME` in the session daemon (qemu:///session,
//!     QEMU runs as you). Conduit writes the whole domain.
//!   * attached: an existing virt-manager VM `conduit attach NAME` added the GPU
//!     to (session or system daemon). Its original definition is backed up and
//!     `conduit detach NAME` puts it back.
//!
//! Either way the GPU backend and virtiofsd are socket-activated systemd units
//! (units.rs) that start when QEMU connects and stop when it goes, and
//! `vms/NAME/libvirt.json` (a [`Link`]) records the domain.

use crate::boot;
use crate::paths::{self, Tool};
use crate::qemu;
use crate::sys;
use crate::ui::{self, oops};
use crate::units::{self, Scope};
use crate::vm::VmConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const SESSION: &str = "qemu:///session";
pub const SYSTEM: &str = "qemu:///system";
/// Namespace of Conduit's <metadata> element in a domain.
pub const META_NS: &str = "https://github.com/olealgoritme/conduit/libvirt/1";
pub const QEMU_NS: &str = "http://libvirt.org/schemas/domain/qemu/1.0";
/// chardev id of the GPU in <qemu:commandline>.
pub const CHARDEV_ID: &str = "conduit-gpu";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Managed,
    Attached,
}

/// `vms/NAME/libvirt.json`: this VM is a libvirt domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub uri: String,
    pub domain: String,
    pub kind: Kind,
    /// Attached: the definition from before `conduit attach` (restored by detach).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    /// The emulator the domain uses (for doctor).
    #[serde(default)]
    pub emulator: PathBuf,
}

impl Link {
    pub fn file(name: &str) -> PathBuf {
        paths::vm_dir(name).join("libvirt.json")
    }

    pub fn load(name: &str) -> Option<Link> {
        crate::vm::check_name(name).ok()?;
        let s = std::fs::read_to_string(Link::file(name)).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn save(&self, name: &str) -> Result<()> {
        let f = Link::file(name);
        std::fs::create_dir_all(f.parent().unwrap())?;
        let tmp = f.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, &f)?;
        Ok(())
    }

    pub fn is_system(&self) -> bool {
        is_system_uri(&self.uri)
    }

    pub fn virsh(&self) -> Virsh {
        Virsh::new(&self.uri)
    }

    pub fn scope(&self) -> Result<Scope> {
        scope_for(&self.uri)
    }
}

pub fn is_system_uri(uri: &str) -> bool {
    uri.starts_with("qemu:///system") || uri.starts_with("qemu+unix:///system")
}

/// The units' scope for a libvirt connection.
pub fn scope_for(uri: &str) -> Result<Scope> {
    if !is_system_uri(uri) {
        return Ok(Scope::User);
    }
    let qemu_user = ["libvirt-qemu", "qemu"]
        .into_iter()
        .find(|u| sys::quiet("id", &["-u", u]))
        .ok_or_else(|| {
            oops(
                "cannot tell which user the system libvirt runs QEMU as",
                "Neither libvirt-qemu nor qemu exists; use the session daemon (-c qemu:///session)",
            )
        })?;
    Ok(Scope::System {
        user: paths::username(),
        uid: paths::uid(),
        home: paths::home(),
        qemu_user: qemu_user.into(),
    })
}

// ---------------------------------------------------------------- virsh

pub struct Virsh {
    pub uri: String,
}

impl Virsh {
    pub fn new(uri: &str) -> Virsh {
        Virsh { uri: uri.into() }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new("virsh");
        c.env("LC_ALL", "C")
            .args(["-q", "-c", &self.uri])
            .args(args);
        c
    }

    /// Run; the error carries virsh's own message.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .cmd(args)
            .stdin(Stdio::null())
            .output()
            .context("could not run virsh")?;
        if !out.status.success() {
            let e = String::from_utf8_lossy(&out.stderr);
            let e = e
                .trim()
                .trim_start_matches("error: ")
                .replace("\nerror: ", "; ");
            anyhow::bail!("virsh {}: {e}", args.first().unwrap_or(&""));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Can we talk to this libvirt at all? Explains how to fix it if not.
    pub fn reachable(&self) -> Result<()> {
        if !sys::have("virsh") {
            return Err(oops(
                "libvirt is not installed (virsh is missing)",
                "Install it: sudo apt install libvirt-daemon-system libvirt-clients  (Fedora: sudo dnf install libvirt-daemon-kvm libvirt-client)",
            ));
        }
        self.run(&["uri"]).map(|_| ()).map_err(|e| {
            oops(
                format!("cannot connect to libvirt at {}: {e:#}", self.uri),
                if is_system_uri(&self.uri) {
                    "Start it (sudo systemctl enable --now libvirtd) and make sure you are in the libvirt group"
                } else {
                    "Make sure libvirt is installed (the session daemon starts on demand): sudo apt install libvirt-daemon-system"
                },
            )
        })
    }

    pub fn exists(&self, dom: &str) -> bool {
        self.run(&["domuuid", dom]).is_ok()
    }

    /// "running", "paused", "shut off", "in shutdown", "crashed", "pmsuspended"; None if undefined.
    pub fn state(&self, dom: &str) -> Option<String> {
        self.run(&["domstate", dom])
            .ok()
            .map(|s| s.trim().to_string())
    }

    pub fn inactive_xml(&self, dom: &str) -> Result<String> {
        self.run(&["dumpxml", "--inactive", "--security-info", dom])
    }

    /// Define a whole domain in one step (libvirt validates it first, so a
    /// refused definition changes nothing).
    pub fn define(&self, xml: &str, name: &str) -> Result<()> {
        let dir = paths::vm_dir(name);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(".libvirt-define.xml");
        std::fs::write(&tmp, xml)?;
        let r = self.run(&["define", "--validate", tmp.to_str().unwrap()]);
        let _ = std::fs::remove_file(&tmp);
        r.map(|_| ())
    }

    /// QEMU's pid, when this is the session daemon and the domain runs.
    pub fn qemu_pid(&self, dom: &str) -> Option<i32> {
        let p = if is_system_uri(&self.uri) {
            PathBuf::from(format!("/run/libvirt/qemu/{dom}.pid"))
        } else {
            paths::xdg_runtime().join(format!("libvirt/qemu/run/{dom}.pid"))
        };
        sys::read_pid(&p)
    }
}

/// Running in the sense of "has a QEMU": running, paused, shutting down.
pub fn state_is_up(s: &str) -> bool {
    !matches!(s, "shut off" | "crashed")
}

/// Does this domain carry Conduit's metadata (we made or attached it)?
pub fn is_ours(xml: &str) -> bool {
    xml.contains(META_NS)
}

// ---------------------------------------------------------------- emulator & AppArmor

/// The Conduit QEMU (11.1 + patches): stock QEMU cannot run the GPU device.
pub fn emulator() -> Result<PathBuf> {
    let q = Tool::BundledQemu.require()?;
    // libvirt (and the system QEMU user) need a real path, not a symlink chain.
    Ok(q.canonicalize().unwrap_or(q))
}

const AA_LIBVIRTD: &str = "/etc/apparmor.d/usr.sbin.libvirtd";
const AA_LIBVIRTD_LOCAL: &str = "/etc/apparmor.d/local/usr.sbin.libvirtd";
const AA_MARK: &str = "# added by conduit";

fn apparmor_on() -> bool {
    Path::new("/sys/kernel/security/apparmor").is_dir() && Path::new(AA_LIBVIRTD).is_file()
}

/// Paths libvirtd's own AppArmor profile already lets it execute (PUx).
fn aa_allows_exec(p: &Path) -> bool {
    let s = p.to_string_lossy();
    ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/"]
        .iter()
        .any(|d| s.starts_with(d) && !s[d.len()..].contains('/'))
}

/// The rule that lets libvirtd (session or system) start this QEMU.
pub fn aa_rule(emulator: &Path) -> String {
    format!("\"{}\" PUx, {AA_MARK}", emulator.display())
}

/// Is libvirtd allowed to run `emulator`? (Ubuntu confines libvirtd, the
/// session daemon too, and its profile only knows /usr/bin's QEMU.)
pub fn libvirtd_may_exec(emulator: &Path) -> bool {
    if !apparmor_on() || aa_allows_exec(emulator) {
        return true;
    }
    std::fs::read_to_string(AA_LIBVIRTD_LOCAL)
        .map(|s| s.contains(&aa_rule(emulator)))
        .unwrap_or(false)
}

pub fn allow_libvirtd_exec(emulator: &Path) -> Result<()> {
    if libvirtd_may_exec(emulator) {
        return Ok(());
    }
    sys::sudo_ready(&format!(
        "libvirt's AppArmor profile does not let it start Conduit's QEMU ({}); adding one rule for it to {AA_LIBVIRTD_LOCAL}.",
        emulator.display()
    ))?;
    let line = aa_rule(emulator);
    sys::sudo(
        "sh",
        &[
            "-c",
            &format!(
                "printf '%s\\n' {} >> {AA_LIBVIRTD_LOCAL} && apparmor_parser -r {AA_LIBVIRTD}",
                ui::shell_quote(&line)
            ),
        ],
    )
    .context("could not update libvirt's AppArmor profile")?;
    Ok(())
}

// ---------------------------------------------------------------- managed domain XML

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

/// Everything the managed domain points at.
pub struct Managed<'a> {
    pub emulator: &'a Path,
    pub kernel: &'a Path,
    pub initrd: Option<&'a Path>,
    pub gpu_sock: &'a Path,
    pub vfs_sock: &'a Path,
    pub console_log: &'a Path,
    pub audio: Option<qemu::Audio>,
    pub runtime_dir: &'a Path,
}

/// The GPU's -device value, pinned to a PCI slot libvirt leaves alone.
pub fn gpu_device_arg(bus: &str, slot: u8) -> String {
    format!(
        "{},bus={bus},addr={slot:#04x}",
        qemu::gpu_device(CHARDEV_ID)
    )
}

pub fn gpu_chardev_arg(sock: &Path) -> String {
    format!("socket,id={CHARDEV_ID},path={}", sock.display())
}

/// The libvirt domain of a Conduit VM (same machine as `conduit up` runs).
pub fn managed_xml(c: &VmConfig, m: &Managed) -> String {
    let n = c.net();
    let p = |x: &Path| esc(&x.display().to_string());
    let mut os = format!("    <kernel>{}</kernel>\n", p(m.kernel));
    if let Some(i) = m.initrd {
        os += &format!("    <initrd>{}</initrd>\n", p(i));
    }
    os += &format!("    <cmdline>{}</cmdline>\n", esc(&qemu::kernel_args(c)));
    let mut args = vec![
        "-chardev".to_string(),
        gpu_chardev_arg(m.gpu_sock),
        "-device".into(),
        gpu_device_arg("pcie.0", 0x10),
    ];
    if let Some(a) = m.audio {
        args.extend([
            "-audiodev".into(),
            format!(
                "{},id=conduit-snd,out.name=conduit-{n},in.name=conduit-{n}",
                a.driver(),
                n = c.name
            ),
            "-device".into(),
            // streams=2: speakers and microphone.
            "virtio-sound-pci,audiodev=conduit-snd,streams=2,bus=pcie.0,addr=0x11".into(),
        ]);
    }
    let mut cl: String = args
        .iter()
        .map(|a| format!("    <qemu:arg value='{}'/>\n", esc(a)))
        .collect();
    // libvirt hands QEMU a scrubbed environment: point it at the desktop's
    // sound server (PipeWire / PulseAudio live in the runtime dir).
    let rt = p(m.runtime_dir);
    cl += &format!("    <qemu:env name='XDG_RUNTIME_DIR' value='{rt}'/>\n");
    cl += &format!("    <qemu:env name='PIPEWIRE_RUNTIME_DIR' value='{rt}'/>\n");
    cl += &format!("    <qemu:env name='PULSE_SERVER' value='unix:{rt}/pulse/native'/>\n");
    format!(
        r#"<domain type='kvm' xmlns:qemu='{QEMU_NS}'>
  <name>{name}</name>
  <title>{name} (Conduit)</title>
  <description>Conduit VM. GPU, display (`conduit view {name}`) and clipboard come from Conduit. Settings live in {dir}/vm.json; `conduit libvirt enable {name}` rewrites this definition from them.</description>
  <metadata>
    <conduit:vm xmlns:conduit='{META_NS}' name='{name}' kind='managed'/>
  </metadata>
  <memory unit='MiB'>{ram}</memory>
  <currentMemory unit='MiB'>{ram}</currentMemory>
  <memoryBacking>
    <source type='memfd'/>
    <access mode='shared'/>
  </memoryBacking>
  <vcpu placement='static'>{cpus}</vcpu>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
{os}  </os>
  <features>
    <acpi/>
    <apic/>
  </features>
  <cpu mode='host-passthrough' check='none' migratable='off'>
    <maxphysaddr mode='passthrough'/>
  </cpu>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <emulator>{emu}</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='raw' cache='none' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <interface type='ethernet'>
      <mac address='{mac}'/>
      <target dev='{tap}' managed='no'/>
      <model type='virtio'/>
    </interface>
    <filesystem type='mount'>
      <driver type='virtiofs' queue='1024'/>
      <source socket='{vfs}'/>
      <target dir='nvidia'/>
    </filesystem>
    <serial type='pty'>
      <log file='{log}' append='off'/>
      <target port='0'/>
    </serial>
    <console type='pty'>
      <target type='serial' port='0'/>
    </console>
    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
    <video>
      <model type='none'/>
    </video>
    <memballoon model='none'/>
  </devices>
  <qemu:commandline>
{cl}  </qemu:commandline>
</domain>
"#,
        name = esc(&c.name),
        dir = p(&c.dir()),
        ram = c.ram_mib,
        cpus = c.cpus,
        emu = p(m.emulator),
        disk = p(&c.disk_path()),
        mac = n.mac,
        tap = n.tap,
        vfs = p(m.vfs_sock),
        log = p(m.console_log),
    )
}

/// Build the managed domain's XML for this VM now (refreshes boot files).
pub fn build_managed(c: &VmConfig, emulator: &Path) -> Result<String> {
    let b = boot::resolve(c)?;
    let gpu = units::socket_path(&Scope::User, &c.name, "backend");
    let vfs = units::socket_path(&Scope::User, &c.name, "virtiofsd");
    let log = c.logs_dir().join("vm.log");
    let audio = qemu::pick_audio(emulator);
    Ok(managed_xml(
        c,
        &Managed {
            emulator,
            kernel: b.kernel(),
            initrd: b.initrd(),
            gpu_sock: &gpu,
            vfs_sock: &vfs,
            console_log: &log,
            audio,
            runtime_dir: &paths::xdg_runtime(),
        },
    ))
}

// ---------------------------------------------------------------- desktop entry

fn desktop_file(name: &str) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| paths::home().join(".local/share"))
        .join("applications")
        .join(format!("conduit-{name}.desktop"))
}

pub fn desktop_entry(name: &str, conduit: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={name} (Conduit VM)\nComment=Open the VM {name} in a window (starts it if needed)\n\
         Exec={} view {name}\nIcon=computer\nTerminal=false\nCategories=System;Emulator;\nKeywords=VM;Conduit;\nStartupNotify=true\n",
        conduit.display()
    )
}

pub fn install_desktop_entry(name: &str, conduit: &Path) {
    let f = desktop_file(name);
    if std::fs::create_dir_all(f.parent().unwrap()).is_ok()
        && std::fs::write(&f, desktop_entry(name, conduit)).is_ok()
    {
        let _ = sys::quiet(
            "update-desktop-database",
            &[&f.parent().unwrap().to_string_lossy()],
        );
    }
}

pub fn remove_desktop_entry(name: &str) {
    let _ = std::fs::remove_file(desktop_file(name));
}

// ---------------------------------------------------------------- enable / disable

pub fn conduit_exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("cannot find the conduit program")?;
    Ok(me.canonicalize().unwrap_or(me))
}

/// Can `conduit create`/`import` register VMs with libvirt here?
pub fn session_available() -> bool {
    sys::have("virsh") && Virsh::new(SESSION).reachable().is_ok()
}

/// `conduit libvirt enable NAME`: define (or refresh) the managed domain.
pub fn enable(name: &str) -> Result<()> {
    let c = VmConfig::load(name)?;
    if let Some(l) = Link::load(name) {
        if l.kind == Kind::Attached {
            return Err(oops(
                format!("{name} is an attached libvirt VM"),
                format!("Use `conduit attach {name}` / `conduit detach {name}` for it"),
            ));
        }
    }
    let v = Virsh::new(SESSION);
    // ---- validate everything before changing anything
    v.reachable()?;
    units::check(&Scope::User)?;
    if crate::run::is_running_unmanaged(name) {
        return Err(oops(
            format!("{name} is running (started by `conduit up`/`view`)"),
            format!("Stop it first: conduit down {name}"),
        ));
    }
    let old = v.inactive_xml(name).ok();
    if let Some(x) = &old {
        if !is_ours(x) {
            return Err(oops(
                format!("libvirt already has a VM called \"{name}\" that Conduit did not make"),
                format!("To give that VM Conduit's GPU use `conduit attach {name}`; otherwise rename one of them"),
            ));
        }
        if v.state(name).is_some_and(|s| state_is_up(&s)) {
            return Err(oops(
                format!("{name} is running"),
                format!("Shut it down first (conduit down {name}), then run this again"),
            ));
        }
    }
    let emu = emulator()?;
    Tool::Backend.require()?;
    crate::run::need_virtiofsd()?;
    if !c.disk_path().is_file() {
        return Err(oops(
            format!("the VM's disk is missing: {}", c.disk_path().display()),
            "Re-create the VM with `conduit create`, or `conduit import` a disk",
        ));
    }
    let xml = build_managed(&c, &emu)?;
    let me = conduit_exe()?;
    std::fs::create_dir_all(c.logs_dir())?;

    // ---- apply: host plumbing first, the domain last (one atomic define)
    allow_libvirtd_exec(&emu)?;
    units::install_net(&c)?;
    units::install(&Scope::User, name, &me)?;
    if let Err(e) = v.define(&xml, name) {
        if old.is_none() {
            units::remove(&Scope::User, name);
        }
        return Err(oops(
            format!("libvirt refused {name}'s definition: {e:#}"),
            "Nothing was changed in libvirt. Run `conduit doctor` and report this if it persists",
        ));
    }
    Link {
        uri: SESSION.into(),
        domain: name.into(),
        kind: Kind::Managed,
        backup: None,
        emulator: emu,
    }
    .save(name)?;
    install_desktop_entry(name, &me);
    println!(
        "{name} is a libvirt VM now ({SESSION}). Start, pause and stop it from virt-manager or virsh:"
    );
    println!("  virt-manager: File > Add Connection > QEMU/KVM user session, then {name} > Run");
    println!("  virsh -c {SESSION} start {name}     (or: conduit up {name})");
    println!("  its screen:   conduit view {name}   (or the \"{name} (Conduit VM)\" app entry)");
    Ok(())
}

/// `conduit libvirt disable NAME`: remove the managed domain (the VM stays).
pub fn disable(name: &str) -> Result<()> {
    let link = Link::load(name).ok_or_else(|| {
        oops(
            format!("{name} is not a libvirt VM"),
            format!("Nothing to do. `conduit libvirt enable {name}` makes it one"),
        )
    })?;
    if link.kind == Kind::Attached {
        return Err(oops(
            format!("{name} was attached to an existing libvirt VM"),
            format!("Use `conduit detach {name}` to restore its original definition"),
        ));
    }
    let v = link.virsh();
    v.reachable()?;
    if v.state(name).is_some_and(|s| state_is_up(&s)) {
        return Err(oops(
            format!("{name} is running"),
            format!("Shut it down first: conduit down {name}"),
        ));
    }
    if v.exists(name) {
        let x = v.inactive_xml(name)?;
        if !is_ours(&x) {
            return Err(oops(
                format!("libvirt's \"{name}\" is not the one Conduit made; leaving it alone"),
                "Remove it yourself if you want to (virsh undefine)",
            ));
        }
        v.run(&["undefine", name])
            .context("could not remove the libvirt definition")?;
    }
    units::remove(&Scope::User, name);
    if let Ok(c) = VmConfig::load(name) {
        if let Err(e) = units::remove_net(&c.name) {
            ui::warn(format!("{e:#}"));
        }
    }
    remove_desktop_entry(name);
    let _ = std::fs::remove_file(Link::file(name));
    println!("{name} is no longer a libvirt VM. `conduit up/view {name}` run it directly again.");
    Ok(())
}

// ---------------------------------------------------------------- lifecycle

/// Start the domain. Errors carry libvirt's message and where its log is.
pub fn start(name: &str, link: &Link) -> Result<()> {
    let v = link.virsh();
    match v.state(&link.domain).as_deref() {
        Some("paused") => {
            v.run(&["resume", &link.domain])?;
            return Ok(());
        }
        Some(s) if state_is_up(s) => return Ok(()),
        _ => {}
    }
    if let Ok(scope) = link.scope() {
        if !units::ensure_listening(&scope, name) {
            return Err(oops(
                format!("{name}'s GPU backend socket is not listening"),
                format!(
                    "Repair it with `conduit {} {name}`",
                    if link.kind == Kind::Managed {
                        "libvirt enable"
                    } else {
                        "attach"
                    }
                ),
            ));
        }
    }
    v.run(&["start", &link.domain]).map_err(|e| {
        let log = if link.is_system() {
            format!("/var/log/libvirt/qemu/{}.log", link.domain)
        } else {
            paths::home()
                .join(format!(".cache/libvirt/qemu/log/{}.log", link.domain))
                .display()
                .to_string()
        };
        oops(
            format!("libvirt could not start {name}: {e:#}"),
            format!(
                "QEMU's log: {log}\nThe GPU backend's log: conduit logs {name} backend\nCheck the whole chain: conduit doctor {name}"
            ),
        )
    })?;
    Ok(())
}

/// ACPI shutdown, then force it off after `grace`. True when it went down cleanly.
pub fn shutdown(link: &Link, grace: Duration) -> Result<bool> {
    let v = link.virsh();
    let d = &link.domain;
    if !v.state(d).is_some_and(|s| state_is_up(&s)) {
        return Ok(true);
    }
    if v.state(d).as_deref() == Some("paused") {
        let _ = v.run(&["resume", d]);
    }
    let _ = v.run(&["shutdown", d]);
    let clean = sys::wait_for(grace, || !v.state(d).is_some_and(|s| state_is_up(&s)));
    if !clean {
        v.run(&["destroy", d]).context("could not stop the VM")?;
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        let mut c = VmConfig::new("t-1", 4096, 4, 3, "me", "none");
        c.disk = PathBuf::from("/vms/t-1/disk.img");
        managed_xml(
            &c,
            &Managed {
                emulator: Path::new("/opt/conduit/bin/qemu-system-x86_64"),
                kernel: Path::new("/vms/t-1/boot/vmlinuz"),
                initrd: Some(Path::new("/vms/t-1/boot/initrd.img")),
                gpu_sock: Path::new("/run/user/1000/conduit/t-1/gpu-libvirt.sock"),
                vfs_sock: Path::new("/run/user/1000/conduit/t-1/vfs-libvirt.sock"),
                console_log: Path::new("/vms/t-1/logs/vm.log"),
                audio: Some(qemu::Audio::PipeWire),
                runtime_dir: Path::new("/run/user/1000"),
            },
        )
    }

    #[test]
    fn managed_domain_has_everything_conduit_up_has() {
        let x = sample();
        let root = xmltree::Element::parse(x.as_bytes()).expect("well-formed XML");
        assert_eq!(root.get_child("name").unwrap().get_text().unwrap(), "t-1");
        for want in [
            "<source type='memfd'/>",
            "<access mode='shared'/>",
            "<memory unit='MiB'>4096</memory>",
            "<kernel>/vms/t-1/boot/vmlinuz</kernel>",
            "<initrd>/vms/t-1/boot/initrd.img</initrd>",
            "<cmdline>console=ttyS0 root=/dev/vda rw</cmdline>",
            "<maxphysaddr mode='passthrough'/>",
            "<emulator>/opt/conduit/bin/qemu-system-x86_64</emulator>",
            "<source file='/vms/t-1/disk.img'/>",
            "<target dev='conduit3' managed='no'/>",
            "<mac address='02:00:00:00:03:01'/>",
            "<source socket='/run/user/1000/conduit/t-1/vfs-libvirt.sock'/>",
            "<target dir='nvidia'/>",
            "socket,id=conduit-gpu,path=/run/user/1000/conduit/t-1/gpu-libvirt.sock",
            "vhost-user-test-device-pci,chardev=conduit-gpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036,bus=pcie.0,addr=0x10",
            "pipewire,id=conduit-snd,out.name=conduit-t-1,in.name=conduit-t-1",
            "virtio-sound-pci,audiodev=conduit-snd,streams=2",
            "<qemu:env name='XDG_RUNTIME_DIR' value='/run/user/1000'/>",
            META_NS,
        ] {
            assert!(x.contains(want), "missing {want:?} in\n{x}");
        }
        assert!(is_ours(&x));
    }

    #[test]
    fn escapes_paths() {
        assert_eq!(esc("a&b<'c'>"), "a&amp;b&lt;&apos;c&apos;&gt;");
    }

    #[test]
    fn link_roundtrip() {
        let l = Link {
            uri: SESSION.into(),
            domain: "x".into(),
            kind: Kind::Attached,
            backup: Some("/b.xml".into()),
            emulator: "/q".into(),
        };
        let s = serde_json::to_string(&l).unwrap();
        assert!(s.contains("\"kind\":\"attached\""));
        assert_eq!(serde_json::from_str::<Link>(&s).unwrap(), l);
        assert!(!is_system_uri(SESSION));
        assert!(is_system_uri(SYSTEM));
    }

    #[test]
    fn apparmor_rule_quotes_the_path() {
        assert_eq!(
            aa_rule(Path::new("/opt/conduit/bin/qemu-system-x86_64")),
            "\"/opt/conduit/bin/qemu-system-x86_64\" PUx, # added by conduit"
        );
        assert!(aa_allows_exec(Path::new("/usr/bin/qemu-system-x86_64")));
        assert!(!aa_allows_exec(Path::new(
            "/opt/conduit/bin/qemu-system-x86_64"
        )));
    }

    #[test]
    fn up_states() {
        assert!(state_is_up("running"));
        assert!(state_is_up("paused"));
        assert!(!state_is_up("shut off"));
    }
}
