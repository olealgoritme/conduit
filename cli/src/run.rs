//! Starting and stopping a VM: network, GPU backend, VM runner, viewer.
//!
//! Two VM runners: Conduit's QEMU (the default; adds sound and an ACPI
//! shutdown, see qemu.rs) and the small built-in one (host/vmm), used when the
//! bundled QEMU is missing or with `--vmm builtin`.
//!
//! Runtime files live in /run/user/$UID/conduit/NAME/ (pid files, sockets, the
//! generated runner config); logs in the VM's folder, logs/{backend,vm,viewer}.log.

use crate::hypr;
use crate::mem;
use crate::mode::{self, Mode};
use crate::net;
use crate::paths::{self, comm_of, Tool};
use crate::qemu;
use crate::scope::Slice;
use crate::sys::{self, live_pid};
use crate::ui::{self, oops, shell_quote};
use crate::vm::{self, VmConfig};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Which program runs the VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum VmmKind {
    /// Conduit's QEMU 11.1 (sound, ACPI shutdown)
    Qemu,
    /// The small built-in runner (no sound)
    Builtin,
}

impl VmmKind {
    fn as_str(self) -> &'static str {
        match self {
            VmmKind::Qemu => "qemu",
            VmmKind::Builtin => "builtin",
        }
    }
}

/// What the running VM was started with (state.json in the runtime folder).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub(crate) mode: Option<String>,
    pub(crate) backend_comm: String,
    pub(crate) vm_comm: String,
    pub(crate) viewer_comm: String,
    pub(crate) watcher_comm: String,
    /// "qemu" or "builtin" (empty: started by an older conduit = builtin).
    #[serde(default)]
    pub(crate) vmm: String,
    /// QEMU only: the virtiofsd serving the NVIDIA share.
    #[serde(default)]
    pub(crate) virtiofsd_comm: String,
    /// QEMU only: the sound server the VM plays through.
    #[serde(default)]
    pub(crate) audio: Option<String>,
    /// Closing the viewer window shuts the VM down (`conduit view` booted it
    /// and neither --keep-running nor `view.close_stops_vm false` said otherwise).
    #[serde(default)]
    pub(crate) close_stops_vm: bool,
}

pub(crate) struct Rt {
    pub(crate) dir: PathBuf,
}

impl Rt {
    pub(crate) fn new(name: &str) -> Result<Rt> {
        let dir = paths::run_dir(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Rt { dir })
    }
    pub(crate) fn p(&self, f: &str) -> PathBuf {
        self.dir.join(f)
    }
    fn gpu_sock(&self) -> PathBuf {
        self.p("gpu.sock")
    }
    pub(crate) fn display_sock(&self) -> PathBuf {
        self.p("display.sock")
    }
    fn vfs_sock(&self) -> PathBuf {
        self.p("vfs.sock")
    }
    fn qmp_sock(&self) -> PathBuf {
        self.p("qmp.sock")
    }
    pub(crate) fn hypr_state(&self) -> PathBuf {
        self.p("hypr.saved")
    }
    pub(crate) fn state(&self) -> State {
        std::fs::read_to_string(self.p("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub(crate) fn save_state(&self, s: &State) -> Result<()> {
        std::fs::write(self.p("state.json"), serde_json::to_string_pretty(s)?)?;
        Ok(())
    }
    pub(crate) fn pid(&self, what: &str, comm: &str) -> Option<i32> {
        if comm.is_empty() {
            return None;
        }
        live_pid(&self.p(&format!("{what}.pid")), comm)
    }
}

pub(crate) fn self_comm() -> String {
    std::env::current_exe()
        .map(|p| comm_of(&p))
        .unwrap_or_else(|_| "conduit".into())
}

/// Running in any way: as a libvirt domain (also paused) or under `conduit up`.
pub fn is_running(name: &str) -> bool {
    match crate::virt::Link::load(name) {
        Some(l) => l
            .virsh()
            .state(&l.domain)
            .is_some_and(|s| crate::virt::state_is_up(&s)),
        None => is_running_unmanaged(name),
    }
}

/// Started by `conduit up`/`view` directly (not through libvirt).
pub(crate) fn is_running_unmanaged(name: &str) -> bool {
    let Ok(rt) = Rt::new(name) else { return false };
    let st = rt.state();
    rt.pid("vm", &st.vm_comm).is_some()
}

// ---------------------------------------------------------------- driver share

/// The NVIDIA user-space folder the guest mounts. It must be the exact build of
/// the host's loaded driver, so by default it is staged from the host itself.
pub(crate) fn ensure_share(c: &VmConfig) -> Result<PathBuf> {
    ensure_share_for(c.share.as_deref(), &c.dir().join("vm.json"), &c.logs_dir())
}

/// The share for a VM with this `share` setting (None: staged from the host
/// driver); `settings` is where the setting lives, `logs` the VM's log folder.
pub(crate) fn ensure_share_for(
    share: Option<&Path>,
    settings: &Path,
    logs: &Path,
) -> Result<PathBuf> {
    if let Some(s) = share {
        if s.is_dir() {
            return Ok(s.to_path_buf());
        }
        return Err(oops(
            format!(
                "the NVIDIA share folder set for this VM is missing: {}",
                s.display()
            ),
            format!("Fix or remove \"share\" in {}", settings.display()),
        ));
    }
    let drv = crate::host::driver().ok_or_else(|| {
        oops(
            "the NVIDIA driver is not loaded on this computer",
            "Run `conduit doctor` for help",
        )
    })?;
    let dir = paths::cache_dir().join("nvidia-share").join(&drv.version);
    if dir.join(".conduit-staged").is_file() {
        return Ok(dir);
    }
    let tool = Tool::Userspace.require()?;
    ui::info(format!(
        "preparing the NVIDIA {} files for VMs (once per driver version)",
        drv.version
    ));
    // Not with_extension: "610.57.04" would become "610.57.partial".
    let tmp = dir.with_file_name(format!("{}.partial", drv.version));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.parent().unwrap())?;
    std::fs::create_dir_all(logs)?;
    let log = logs.join("share.log");
    let out = Command::new(&tool)
        .arg("--stage")
        .arg(&tmp)
        .args(["--caps", "graphics,video,utility,compute"])
        .output()
        .context("could not run the driver share tool")?;
    let _ = std::fs::write(
        &log,
        [out.stdout.as_slice(), out.stderr.as_slice()].concat(),
    );
    if !out.status.success() {
        return Err(oops(
            "could not prepare the NVIDIA files for the VM",
            format!("Details: {}\n{}", log.display(), sys::tail(&log, 5)),
        ));
    }
    std::fs::write(tmp.join(".conduit-staged"), format!("{}\n", drv.version))?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------- start pieces

fn start_backend(c: &VmConfig, rt: &Rt, p: &Parts, mode: Option<Mode>) -> Result<()> {
    let backend = &p.backend;
    let sock = rt.gpu_sock();
    sys::clear_stale_socket(&sock)?;
    let mut cmd = Command::new(backend);
    cmd.arg("--socket")
        .arg(&sock)
        .args(["--caps", "graphics,video,utility,compute"]);
    if let Some(m) = mode {
        cmd.arg("--display")
            .arg(m.to_string())
            .arg("--display-socket")
            .arg(rt.display_sock());
    }
    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        if mode.is_some() {
            "info,device::display=debug".into()
        } else {
            "info".into()
        }
    });
    cmd.env("RUST_LOG", level);
    let log = c.logs_dir().join("backend.log");
    let pid = sys::spawn_vm_part(&mut cmd, &log, false, p.slice.as_ref())?;
    sys::write_pid(&rt.p("backend.pid"), pid)?;
    if let Some(s) = &p.slice {
        s.group_oom();
    }
    let pid = pid as i32;
    let ok = sys::wait_for(Duration::from_secs(10), || {
        sock.exists() || !sys::alive(pid)
    });
    if !ok || !sock.exists() {
        return Err(oops(
            "the GPU backend did not start",
            format!(
                "Its log ({}) ends with:\n{}",
                log.display(),
                sys::tail(&log, 6)
            ),
        ));
    }
    Ok(())
}

fn check_disk(c: &VmConfig) -> Result<()> {
    let disk = c.disk_path();
    if !disk.is_file() {
        return Err(oops(
            format!("the VM's disk is missing: {}", disk.display()),
            "Re-create the VM with `conduit create`, or `conduit import` a disk",
        ));
    }
    Ok(())
}

fn vm_failed(log: &Path) -> anyhow::Error {
    oops(
        "the VM failed to start",
        format!(
            "Its console log ({}) ends with:\n{}",
            log.display(),
            sys::tail(log, 6)
        ),
    )
}

/// The NVIDIA share for QEMU: a virtiofsd QEMU connects to.
fn start_virtiofsd(c: &VmConfig, rt: &Rt, p: &Parts, vfsd: &Path) -> Result<()> {
    let share = &p.share;
    let sock = rt.vfs_sock();
    sys::clear_stale_socket(&sock)?;
    let (mut cmd, ro) = qemu::virtiofsd_cmd(vfsd, &sock, share);
    let log = c.logs_dir().join("virtiofsd.log");
    let pid = sys::spawn_vm_part(&mut cmd, &log, false, p.slice.as_ref())?;
    sys::write_pid(&rt.p("virtiofsd.pid"), pid)?;
    if !ro {
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "conduit: this virtiofsd has no --readonly; the NVIDIA share is writable by the VM")
            });
    }
    let pid = pid as i32;
    let ok = sys::wait_for(Duration::from_secs(10), || {
        sock.exists() || !sys::alive(pid)
    });
    if !ok || !sock.exists() {
        return Err(oops(
            "the NVIDIA share (virtiofsd) did not start",
            format!(
                "Its log ({}) ends with:\n{}",
                log.display(),
                sys::tail(&log, 6)
            ),
        ));
    }
    Ok(())
}

fn start_qemu(c: &VmConfig, rt: &Rt, p: &Parts) -> Result<()> {
    check_disk(c)?;
    let log = c.logs_dir().join("vm.log");
    let qmp = rt.qmp_sock();
    let _ = std::fs::remove_file(&qmp);
    let args = qemu::args(
        c,
        &qemu::Paths {
            kernel: p.boot.kernel(),
            initrd: p.boot.initrd(),
            gpu_sock: &rt.gpu_sock(),
            vfs_sock: &rt.vfs_sock(),
            qmp_sock: &qmp,
            console_log: &log,
        },
        p.audio,
    );
    // QEMU's own messages and the serial console share vm.log: both append.
    std::fs::write(&log, "")?;
    std::fs::write(
        rt.p("qemu.args"),
        args.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" \\\n  "),
    )?;
    let mut cmd = Command::new(&p.vmm);
    cmd.args(&args);
    let pid = sys::spawn_vm_part(&mut cmd, &log, true, p.slice.as_ref())?;
    sys::write_pid(&rt.p("vm.pid"), pid)?;
    let pid = pid as i32;
    // QMP greets only once the machine is built and the main loop runs, so a
    // greeting means every device (tap, sockets, audio) came up.
    let ready = sys::wait_for(Duration::from_secs(20), || {
        !sys::alive(pid) || qemu::Qmp::connect(&qmp).is_ok()
    });
    if !sys::alive(pid) {
        return Err(vm_failed(&log));
    }
    if !ready {
        return Err(oops(
            "QEMU started but did not answer within 20 seconds",
            format!(
                "Its log ({}) ends with:\n{}",
                log.display(),
                sys::tail(&log, 6)
            ),
        ));
    }
    Ok(())
}

fn start_vm(c: &VmConfig, rt: &Rt, p: &Parts) -> Result<()> {
    let (vmm, share, kernel) = (&p.vmm, &p.share, p.boot.kernel());
    check_disk(c)?;
    let cfg = vm::vmm_config(c, kernel, &rt.gpu_sock(), share);
    let cfg_path = rt.p("vmm.json");
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;
    let log = c.logs_dir().join("vm.log");
    let mut cmd = Command::new(vmm);
    cmd.arg(&cfg_path);
    let pid = sys::spawn_vm_part(&mut cmd, &log, false, p.slice.as_ref())?;
    sys::write_pid(&rt.p("vm.pid"), pid)?;
    std::thread::sleep(Duration::from_secs(1));
    if !sys::alive(pid as i32) {
        return Err(vm_failed(&log));
    }
    Ok(())
}

/// Clipboard sharing for the viewer (`conduit view --clipboard`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Clipboard {
    /// Both ways: the VM gets your clipboard when its window is focused, and
    /// its copies reach yours while focused
    Both,
    /// Only copies made in the VM reach your clipboard
    ToHost,
    /// Only your clipboard reaches the VM
    ToGuest,
    /// Nothing crosses
    Off,
}

impl Clipboard {
    /// The viewer's `--clipboard` value.
    pub fn viewer_arg(self) -> &'static str {
        match self {
            Clipboard::Both => "both",
            Clipboard::ToHost => "guest-to-host",
            Clipboard::ToGuest => "host-to-guest",
            Clipboard::Off => "off",
        }
    }
}

static CLIPBOARD: std::sync::OnceLock<Clipboard> = std::sync::OnceLock::new();

pub fn set_clipboard(c: Clipboard) {
    let _ = CLIPBOARD.set(c);
}

fn clipboard() -> Clipboard {
    CLIPBOARD.get().copied().unwrap_or(Clipboard::Both)
}

/// The viewer backend for this desktop: Wayland when there is one, else X11.
fn viewer_session(wayland: bool, x11: bool) -> Option<&'static str> {
    if wayland {
        Some("wayland")
    } else if x11 {
        Some("x11")
    } else {
        None
    }
}

fn viewer_supports_hook(viewer: &Path) -> bool {
    Command::new(viewer)
        .arg("--help")
        .output()
        .map(|o| {
            let s = [o.stdout, o.stderr].concat();
            String::from_utf8_lossy(&s).contains("--direct-hook")
        })
        .unwrap_or(false)
}

/// Returns true if Hyprland must be tuned for the whole run (old viewer without hook support).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_viewer(
    name: &str,
    logs: &Path,
    rt: &Rt,
    viewer: &Path,
    m: Mode,
    tune: Option<&str>,
    fullscreen: bool,
    close_stops_vm: bool,
) -> Result<bool> {
    let dsock = rt.display_sock();
    if dsock.exists() && sys::socket_live(&dsock) {
        return Err(oops(
            format!("a viewer for {name} is already open"),
            "Look for its window, or close it first",
        ));
    }
    let _ = std::fs::remove_file(&dsock);
    let mut cmd = Command::new(viewer);
    let session = viewer_session(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )
    .unwrap_or("wayland");
    cmd.args(["--backend", session, "--socket"])
        .arg(&dsock)
        .args(["--clipboard", clipboard().viewer_arg()])
        .args([
            "--size",
            &m.size(),
            "--title",
            &format!(
                "{name} - Conduit{}",
                if close_stops_vm {
                    " (closing shuts the VM down)"
                } else {
                    " (closing leaves the VM running)"
                }
            ),
        ])
        .args([
            "--present-mode=native",
            "--scale",
            "aspect",
            "--persist",
            "--stats",
        ]);
    if fullscreen || std::env::var("CONDUIT_FULLSCREEN").as_deref() == Ok("1") {
        cmd.arg("--fullscreen");
    }
    let mut whole_run = false;
    if let Some(sig) = tune {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
        if viewer_supports_hook(viewer) {
            let me = std::env::current_exe()?;
            let hook = format!(
                "{} hypr-hook --state {}",
                shell_quote(&me.to_string_lossy()),
                shell_quote(&rt.hypr_state().to_string_lossy())
            );
            cmd.arg("--direct-hook").arg(hook);
        } else {
            whole_run = true;
        }
    }
    let log = logs.join("viewer.log");
    let pid = sys::spawn_detached(&mut cmd, &log, false)?;
    sys::write_pid(&rt.p("viewer.pid"), pid)?;
    let pid = pid as i32;
    sys::wait_for(Duration::from_secs(5), || {
        dsock.exists() || !sys::alive(pid)
    });
    if !sys::alive(pid) {
        return Err(oops(
            "the viewer window could not open",
            format!(
                "Its log ({}) ends with:\n{}",
                log.display(),
                sys::tail(&log, 6)
            ),
        ));
    }
    Ok(whole_run)
}

pub(crate) fn start_watcher(name: &str, logs: &Path, rt: &Rt) -> Result<()> {
    let me = std::env::current_exe()?;
    let mut cmd = Command::new(me);
    cmd.args(["_watch", name]);
    let pid = sys::spawn_detached(&mut cmd, &logs.join("watcher.log"), true)?;
    sys::write_pid(&rt.p("watcher.pid"), pid)
}

fn prepare(c: &VmConfig) -> Result<(Rt, sys::Lock)> {
    std::fs::create_dir_all(c.logs_dir())?;
    let rt = Rt::new(&c.name)?;
    let lock = sys::lock_vm(&rt.p("lock"), &c.name, LOCK_WAIT)?;
    Ok((rt, lock))
}

/// Everything that can fail (or ask for a password) before anything is started.
struct Parts {
    backend: PathBuf,
    kind: VmmKind,
    /// QEMU or the built-in runner.
    vmm: PathBuf,
    /// QEMU only.
    virtiofsd: Option<PathBuf>,
    audio: Option<qemu::Audio>,
    share: PathBuf,
    /// The VM's systemd slice (memory limit), when there is a user systemd.
    slice: Option<Slice>,
    boot: crate::boot::Boot,
}

pub(crate) fn need_virtiofsd() -> Result<PathBuf> {
    qemu::virtiofsd().ok_or_else(|| {
        oops(
            "virtiofsd is not installed (QEMU needs it to share the NVIDIA files with the VM)",
            "Install it (Ubuntu/Debian: `sudo apt install virtiofsd`, Fedora: `sudo dnf install virtiofsd`), or use `--vmm builtin`",
        )
    })
}

/// QEMU unless asked otherwise; the built-in runner when QEMU is missing.
/// `own_kernel`: the VM boots the kernel on its disk, which only QEMU can do
/// (the built-in runner loads an uncompressed ELF vmlinux, with no initrd).
fn pick_vmm(
    want: Option<VmmKind>,
    own_kernel: bool,
) -> Result<(VmmKind, PathBuf, Option<PathBuf>)> {
    let builtin_cannot = || {
        oops(
            "the built-in VM runner cannot boot this VM: it boots the kernel installed on its own disk, which needs QEMU",
            "Use the bundled QEMU (reinstall the conduit package; in a source checkout run host/qemu/build-qemu.sh, and install virtiofsd)",
        )
    };
    match want {
        Some(VmmKind::Qemu) => Ok((
            VmmKind::Qemu,
            Tool::BundledQemu.require()?,
            Some(need_virtiofsd()?),
        )),
        Some(VmmKind::Builtin) if own_kernel => Err(builtin_cannot()),
        Some(VmmKind::Builtin) => Ok((VmmKind::Builtin, Tool::Vmm.require()?, None)),
        None => match (Tool::BundledQemu.find(), qemu::virtiofsd()) {
            (Some(q), Some(v)) => Ok((VmmKind::Qemu, q, Some(v))),
            (q, _) => {
                let why = if q.is_none() {
                    "Conduit's QEMU was not found"
                } else {
                    "virtiofsd is not installed"
                };
                if own_kernel {
                    let e = builtin_cannot();
                    return Err(oops(why, format!("{e:#}")));
                }
                let vmm = Tool::Vmm.require().map_err(|e| {
                    oops(
                        format!("{why}, and neither was the built-in VM runner"),
                        format!("{e:#}"),
                    )
                })?;
                ui::warn(format!(
                    "{why}; using the built-in VM runner instead (no sound in the VM). `conduit doctor` explains how to get QEMU."
                ));
                Ok((VmmKind::Builtin, vmm, None))
            }
        },
    }
}

/// `--no-mem-check`: start even when the host looks short of memory.
static NO_MEM_CHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_no_mem_check(on: bool) {
    NO_MEM_CHECK.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Refuse to start when the VM's RAM does not fit in what the host has free.
pub(crate) fn check_memory(c: &VmConfig) -> Result<()> {
    let others: Vec<(u64, i32)> = vm::all()
        .into_iter()
        .filter(|n| *n != c.name)
        .filter_map(|n| {
            let o = VmConfig::load(&n).ok()?;
            if let Some(l) = crate::virt::Link::load(&n) {
                return Some((o.ram_mib, l.virsh().qemu_pid(&l.domain)?));
            }
            let rt = Rt::new(&n).ok()?;
            let pid = rt.pid("vm", &rt.state().vm_comm)?;
            Some((o.ram_mib, pid))
        })
        .collect();
    mem::admit(
        &c.name,
        c.ram_mib,
        &others,
        NO_MEM_CHECK.load(std::sync::atomic::Ordering::SeqCst),
    )
}

fn preflight(c: &VmConfig, want: Option<VmmKind>) -> Result<Parts> {
    check_memory(c)?;
    let backend = Tool::Backend.require()?;
    let (kind, vmm, virtiofsd) = pick_vmm(want, c.kernel.is_none())?;
    let audio = match kind {
        VmmKind::Qemu => qemu::pick_audio(&vmm),
        VmmKind::Builtin => None,
    };
    match kind {
        VmmKind::Builtin => {
            ui::info("VM runner: built-in (it has no sound device; `--vmm qemu` adds one)")
        }
        VmmKind::Qemu => ui::info(format!(
            "VM runner: QEMU, sound {}",
            audio
                .map(|a| format!("through {}", a.driver()))
                .unwrap_or_else(|| "off (no PipeWire or PulseAudio found)".into())
        )),
    }
    check_disk(c)?;
    let boot = crate::boot::resolve(c)?;
    ui::info(format!("booting {}", boot.describe()));
    let share = ensure_share(c)?;
    net::up(c)?;
    Ok(Parts {
        backend,
        kind,
        vmm,
        virtiofsd,
        audio,
        share,
        slice: Slice::prepare(&c.name, c.ram_mib),
        boot,
    })
}

/// Backend + VM. `mode` None = no display at all (headless, no KMS).
fn boot(c: &VmConfig, rt: &Rt, st: &mut State, p: &Parts, mode: Option<Mode>) -> Result<()> {
    st.backend_comm = comm_of(&p.backend);
    st.vm_comm = comm_of(&p.vmm);
    st.vmm = p.kind.as_str().into();
    st.mode = mode.map(|m| m.to_string());
    st.audio = p.audio.map(|a| a.driver().to_string());
    if let Some(v) = &p.virtiofsd {
        st.virtiofsd_comm = comm_of(v);
    }
    rt.save_state(st)?;
    start_backend(c, rt, p, mode)?;
    match (p.kind, &p.virtiofsd) {
        (VmmKind::Qemu, Some(vfsd)) => {
            start_virtiofsd(c, rt, p, vfsd)?;
            start_qemu(c, rt, p)?;
        }
        _ => start_vm(c, rt, p)?,
    }
    Ok(())
}

// ---------------------------------------------------------------- commands

pub fn up(name: &str, display: Option<Mode>, headless: bool, vmm: Option<VmmKind>) -> Result<()> {
    if let Some(link) = crate::virt::Link::load(name) {
        if vmm == Some(VmmKind::Builtin) {
            return Err(oops(
                format!("{name} is a libvirt VM; it runs under Conduit's QEMU"),
                format!("`conduit libvirt disable {name}` first to use the built-in runner"),
            ));
        }
        return crate::lvrun::up(name, &link, display, headless);
    }
    let c = VmConfig::load(name)?;
    // First: a stop or start in progress finishes (we wait, and say so).
    let (rt, _lock) = prepare(&c)?;
    if is_running(name) {
        ui::info(format!(
            "{name} is already running (`conduit status {name}`)"
        ));
        return Ok(());
    }
    stop_leftovers(&c, &rt);
    // A display (with no window yet) lets `conduit view` attach later.
    let mode = if headless {
        None
    } else {
        Some(display.unwrap_or_else(|| mode::detect().0))
    };
    let mut st = State::default();
    if let Err(e) = preflight(&c, vmm).and_then(|p| boot(&c, &rt, &mut st, &p, mode)) {
        drop(_lock);
        let _ = down_inner(&c, true, false, Stop::Force);
        return Err(e);
    }
    let n = c.net();
    ui::info(format!(
        "{name} is starting{}. In about 20 seconds: `conduit ssh {name}` (VM address {})",
        mode.map(|m| format!(" with a {m} display"))
            .unwrap_or_default(),
        n.guest_ip
    ));
    if mode.is_some() {
        ui::info(format!("open its screen with `conduit view {name}`"));
    }
    Ok(())
}

pub fn view(
    name: &str,
    req: Option<Mode>,
    tune_hyprland: bool,
    fullscreen: bool,
    vmm: Option<VmmKind>,
    keep_running: bool,
) -> Result<()> {
    if let Some(link) = crate::virt::Link::load(name) {
        return crate::lvrun::view(name, &link, req, tune_hyprland, fullscreen, keep_running);
    }
    let c = VmConfig::load(name)?;
    if viewer_session(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )
    .is_none()
    {
        return Err(oops(
            "no desktop session found (neither WAYLAND_DISPLAY nor DISPLAY is set)",
            "Run `conduit view` from a terminal inside your desktop (Wayland or X11), not over ssh or a text console",
        ));
    }
    let viewer = Tool::Viewer.require()?;
    let (rt, lock) = prepare(&c)?;
    let mut st = rt.state();
    let running = rt.pid("vm", &st.vm_comm).is_some();
    // Only a VM this command boots stops with its window.
    let close_stops_vm = !running && !keep_running && crate::config::close_stops_vm();

    let mode = match (running, st.mode.as_deref()) {
        (true, Some(m)) => {
            let m: Mode = m.parse()?;
            let now = if st.vmm.is_empty() {
                "builtin"
            } else {
                st.vmm.as_str()
            };
            if let Some(k) = vmm.filter(|k| k.as_str() != now) {
                ui::warn(format!(
                    "{name} is already running under the {now} runner, not {} (`conduit down {name}` first to change it)",
                    k.as_str()
                ));
            }
            if let Some(r) = req.filter(|r| *r != m) {
                ui::warn(format!(
                    "{name} is already running at {m}; showing that instead of {r} (`conduit down {name}` first to change it)"
                ));
            }
            m
        }
        (true, None) => {
            return Err(oops(
                format!("{name} was started without a display (`up --headless`)"),
                format!("Run `conduit down {name}`, then `conduit view {name}`"),
            ));
        }
        (false, _) => match req {
            Some(m) => m,
            None => {
                let (m, src) = mode::detect();
                ui::info(format!("VM display: {m} (from {src})"));
                m
            }
        },
    };

    // Network, driver share etc. first: they may ask for a password, and
    // should fail before a window pops up.
    let parts = if running {
        None
    } else {
        stop_leftovers(&c, &rt);
        match preflight(&c, vmm) {
            Ok(p) => Some(p),
            Err(e) => {
                drop(lock);
                let _ = down_inner(&c, true, false, Stop::Force);
                return Err(e);
            }
        }
    };
    let tune = if tune_hyprland {
        match hypr::instance() {
            Some(sig) => {
                let _ = std::fs::remove_file(rt.hypr_state());
                Some(sig)
            }
            None => {
                ui::warn("--tune-hyprland: no running Hyprland found, ignoring it");
                None
            }
        }
    } else {
        None
    };

    let result = (|| -> Result<()> {
        // Viewer first: the backend connects to its socket.
        let whole_run = start_viewer(
            &c.name,
            &c.logs_dir(),
            &rt,
            &viewer,
            mode,
            tune.as_deref(),
            fullscreen,
            close_stops_vm,
        )?;
        st.viewer_comm = comm_of(&viewer);
        rt.save_state(&st)?;
        if whole_run {
            hypr::hook("on", &rt.hypr_state())?;
            ui::info("Hyprland tuned for the viewer (restored when it closes)");
        }
        if let Some(p) = &parts {
            boot(&c, &rt, &mut st, p, Some(mode))?;
            st.viewer_comm = comm_of(&viewer);
        }
        st.watcher_comm = self_comm();
        st.close_stops_vm = close_stops_vm;
        rt.save_state(&st)?;
        start_watcher(&c.name, &c.logs_dir(), &rt)
    })();
    if let Err(e) = result {
        drop(lock);
        let _ = down_inner(&c, true, false, Stop::Force);
        return Err(e);
    }
    ui::info(format!(
        "{name} is {}. Ctrl+Alt+F fullscreen, Ctrl+Alt+G capture mouse. {}",
        if running {
            "running; viewer opened"
        } else {
            "starting"
        },
        close_note(close_stops_vm, name)
    ));
    Ok(())
}

/// What closing the window will do, for `conduit view`'s output.
pub(crate) fn close_note(close_stops_vm: bool, name: &str) -> String {
    if close_stops_vm {
        format!("Closing the window shuts {name} down (--keep-running, or `conduit config set view.close_stops_vm false`, leaves it running).")
    } else {
        format!("Closing the window leaves {name} running: `conduit view {name}` reattaches, `conduit down {name}` stops it.")
    }
}

/// Clear pid files and sockets of processes that are gone (crash, reboot).
fn stop_leftovers(c: &VmConfig, rt: &Rt) {
    let st = rt.state();
    for (what, comm) in [
        ("viewer", &st.viewer_comm),
        ("backend", &st.backend_comm),
        ("vm", &st.vm_comm),
        ("virtiofsd", &st.virtiofsd_comm),
    ] {
        if rt.pid(what, comm).is_none() {
            let _ = std::fs::remove_file(rt.p(&format!("{what}.pid")));
        }
    }
    if rt.pid("backend", &st.backend_comm).is_none() {
        let _ = sys::clear_stale_socket(&rt.gpu_sock());
    }
    if rt.pid("virtiofsd", &st.virtiofsd_comm).is_none() {
        let _ = sys::clear_stale_socket(&rt.vfs_sock());
    }
    if rt.pid("vm", &st.vm_comm).is_none() {
        let _ = sys::clear_stale_socket(&rt.qmp_sock());
    }
    let _ = c;
}

pub fn ssh_cmd(c: &VmConfig, user: &str) -> Command {
    let mut cmd = Command::new("ssh");
    let key = paths::ssh_key();
    if key.is_file() {
        cmd.arg("-i").arg(key);
    }
    cmd.args([
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]);
    cmd.arg(format!("{user}@{}", c.net().guest_ip));
    cmd
}

/// How long a command waits for another one that is starting/stopping the VM
/// (longer than a whole stop: grace + force).
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(90);
/// How long a clean (ACPI) shutdown may take before the VM is forced off.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// How to stop a VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Power button (and `systemctl poweroff` over ssh); forced off after the grace.
    Soft(Duration),
    /// Pull the plug now.
    Force,
}

/// Ask the guest over ssh to power off (or reboot); true when it accepted.
pub(crate) fn guest_ask(c: &VmConfig, action: &str) -> bool {
    let ask = |user: &str, cmd: &str| {
        ssh_cmd(c, user)
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=3", cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    ask("root", &format!("sync; systemctl {action} --no-block"))
        || ask(
            &c.user,
            &format!("sync; sudo -n systemctl {action} --no-block"),
        )
}

/// Stop the VM process `v`: the power button, then ssh, within `grace`; then
/// QEMU `quit`, then signals. Reports a forced stop. True when it went cleanly.
fn stop_vm(c: &VmConfig, rt: &Rt, st: &State, v: i32, how: Stop, verbose: bool) -> bool {
    let qemu = st.vmm == "qemu";
    let clean = match how {
        Stop::Force => false,
        Stop::Soft(grace) => {
            if verbose {
                ui::info(format!(
                    "shutting down {} cleanly (forced off after {} s)…",
                    c.name,
                    grace.as_secs()
                ));
            }
            let t = std::time::Instant::now();
            let acpi = qemu && qemu::ask(&rt.qmp_sock(), "system_powerdown");
            // A desktop may ignore the power button: ask over ssh too, early,
            // inside the same time budget.
            let first = if acpi {
                grace.min(Duration::from_secs(10))
            } else {
                Duration::ZERO
            };
            sys::wait_for(first, || !sys::alive(v)) || {
                guest_ask(c, "poweroff");
                sys::wait_for(grace.saturating_sub(t.elapsed()), || !sys::alive(v))
            }
        }
    };
    if !clean && sys::alive(v) {
        if let Stop::Soft(grace) = how {
            ui::warn(format!(
                "{} did not power off within {} s (it ignored the power button and ssh); forcing it off",
                c.name,
                grace.as_secs()
            ));
        } else if verbose {
            ui::info(format!("forcing {} off", c.name));
        }
        if qemu {
            qemu::quit(&rt.qmp_sock(), v);
        }
    }
    sys::stop_pid(&rt.p("vm.pid"), &st.vm_comm, "VM", Duration::from_secs(5));
    clean
}

pub fn down(name: &str, how: Stop) -> Result<()> {
    if let Some(link) = crate::virt::Link::load(name) {
        return crate::lvrun::down(name, &link, how);
    }
    let c = VmConfig::load(name)?;
    down_inner(&c, true, true, how)
}

/// Stop everything for this VM. `interactive` false: background, no password prompts.
fn down_inner(c: &VmConfig, interactive: bool, verbose: bool, how: Stop) -> Result<()> {
    let rt = Rt::new(&c.name)?;
    let _lock = sys::lock_vm(&rt.p("lock"), &c.name, LOCK_WAIT)?;
    let st = rt.state();
    let me = std::process::id() as i32;
    if let Some(w) = rt.pid("watcher", &st.watcher_comm) {
        if w != me {
            sys::stop_pid(
                &rt.p("watcher.pid"),
                &st.watcher_comm,
                "watcher",
                Duration::from_secs(2),
            );
        }
    }
    let _ = std::fs::remove_file(rt.p("watcher.pid"));
    if !st.viewer_comm.is_empty() {
        sys::stop_pid(
            &rt.p("viewer.pid"),
            &st.viewer_comm,
            "viewer",
            Duration::from_secs(5),
        );
    }
    if let Some(v) = rt.pid("vm", &st.vm_comm) {
        stop_vm(c, &rt, &st, v, how, verbose);
    }
    if !st.virtiofsd_comm.is_empty() {
        sys::stop_pid(
            &rt.p("virtiofsd.pid"),
            &st.virtiofsd_comm,
            "virtiofsd",
            Duration::from_secs(3),
        );
    }
    if !st.backend_comm.is_empty() {
        sys::stop_pid(
            &rt.p("backend.pid"),
            &st.backend_comm,
            "GPU backend",
            Duration::from_secs(5),
        );
    }
    // Whatever is left in the VM's slice (a process that ignored SIGTERM, a
    // child): stopping the slice ends it, so the shared guest RAM is freed.
    Slice::stop(&c.name);
    let _ = sys::clear_stale_socket(&rt.gpu_sock());
    let _ = sys::clear_stale_socket(&rt.display_sock());
    let _ = sys::clear_stale_socket(&rt.vfs_sock());
    let _ = sys::clear_stale_socket(&rt.qmp_sock());
    if rt.hypr_state().exists() {
        let _ = hypr::hook("restore", &rt.hypr_state());
    }
    if let Err(e) = net::down(c, interactive) {
        ui::warn(format!("{e:#}"));
    }
    let _ = std::fs::remove_file(rt.p("state.json"));
    let _ = std::fs::remove_file(rt.p("vmm.json"));
    let _ = std::fs::remove_file(rt.p("qemu.args"));
    if verbose {
        ui::info(format!("{} is stopped", c.name));
    }
    Ok(())
}

/// Background: wait for the viewer window (or the VM) to go away, then stop everything.
pub fn watch(name: &str) -> Result<()> {
    sys::sudo_noninteractive();
    if let Some(link) = crate::virt::Link::load(name) {
        return crate::lvrun::watch(name, &link);
    }
    let c = VmConfig::load(name)?;
    let rt = Rt::new(name)?;
    let st = rt.state();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let viewer = rt.pid("viewer", &st.viewer_comm).is_some();
        let vm = rt.pid("vm", &st.vm_comm).is_some();
        if !viewer && vm && !st.close_stops_vm {
            ui::info(format!("viewer window closed; {name} keeps running"));
            let _ = std::fs::remove_file(rt.p("viewer.pid"));
            let _ = std::fs::remove_file(rt.p("watcher.pid"));
            if rt.hypr_state().exists() {
                let _ = hypr::hook("restore", &rt.hypr_state());
            }
            let _ = sys::clear_stale_socket(&rt.display_sock());
            return Ok(());
        }
        if !viewer || !vm {
            ui::info(format!(
                "{} closed; stopping {}",
                if !viewer { "viewer window" } else { "VM" },
                name
            ));
            break;
        }
    }
    down_inner(&c, false, true, Stop::Soft(SHUTDOWN_GRACE))
}

pub fn status(name: Option<&str>) -> Result<()> {
    if let Some(n) = name {
        if let Some(link) = crate::virt::Link::load(n) {
            return crate::lvrun::status(n, &link);
        }
    }
    let names = match name {
        Some(n) => {
            if crate::virt::Link::load(n).is_none() {
                VmConfig::load(n)?;
            }
            vec![n.to_string()]
        }
        None => crate::lvrun::all_names(),
    };
    if names.is_empty() {
        println!("No VMs yet. Create one with `conduit create myvm`.");
        return Ok(());
    }
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if let Some(link) = crate::virt::Link::load(n) {
            crate::lvrun::status(n, &link)?;
            continue;
        }
        let c = VmConfig::load(n)?;
        let rt = Rt::new(n)?;
        let st = rt.state();
        let vm_pid = rt.pid("vm", &st.vm_comm);
        println!(
            "{n}: {}",
            if vm_pid.is_some() {
                "running"
            } else {
                "stopped"
            }
        );
        if vm_pid.is_some() {
            let runner = if st.vmm.is_empty() {
                "builtin"
            } else {
                st.vmm.as_str()
            };
            let sound = match (runner, &st.audio) {
                ("qemu", Some(a)) => format!("sound through {a}"),
                ("qemu", None) => "no sound".into(),
                _ => "no sound (built-in runner)".into(),
            };
            println!("  runner   {runner}, {sound}");
        }
        for (what, comm) in [
            ("vm", &st.vm_comm),
            ("backend", &st.backend_comm),
            ("virtiofsd", &st.virtiofsd_comm),
            ("viewer", &st.viewer_comm),
            ("watcher", &st.watcher_comm),
        ] {
            if what == "virtiofsd" && comm.is_empty() {
                continue;
            }
            if let Some(p) = rt.pid(what, comm) {
                println!("  {what:<9} running (pid {p})");
            } else if vm_pid.is_some() || name.is_some() {
                println!("  {what:<9} not running");
            }
        }
        if let Some(m) = &st.mode {
            if vm_pid.is_some() {
                println!("  display  {m}");
            }
        }
        println!("  network  {}", net::describe(&c));
        if rt.hypr_state().exists() {
            println!("  hyprland tuned (restored when the viewer closes)");
        }
        if vm_pid.is_some() {
            let out = ssh_cmd(&c, "root")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2",
                       "echo \"up $(cut -d. -f1 /proc/uptime)s, desktop: $(systemctl is-active display-manager 2>/dev/null)\""])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    println!("  guest    {}", String::from_utf8_lossy(&o.stdout).trim())
                }
                _ => println!("  guest    not reachable yet (it may still be booting)"),
            }
        }
    }
    Ok(())
}

pub fn logs(name: &str, which: Option<&str>, follow: bool, lines: usize) -> Result<()> {
    let logs_dir = match VmConfig::load(name) {
        Ok(c) => c.logs_dir(),
        Err(_) if crate::virt::Link::load(name).is_some() => paths::vm_dir(name).join("logs"),
        Err(e) => return Err(e),
    };
    let all = ["backend", "vm", "viewer"];
    let pick: Vec<&str> = match which {
        None => all.to_vec(),
        Some(w) if all.contains(&w) || ["watcher", "share", "virtiofsd"].contains(&w) => vec![w],
        Some(w) => {
            return Err(oops(
                format!("there is no \"{w}\" log"),
                "Choose backend, vm or viewer",
            ));
        }
    };
    let files: Vec<PathBuf> = pick
        .iter()
        .map(|w| logs_dir.join(format!("{w}.log")))
        .collect();
    if follow {
        use std::os::unix::process::CommandExt;
        let err = Command::new("tail")
            .arg("-n")
            .arg(lines.to_string())
            .arg("-F")
            .args(&files)
            .exec();
        return Err(err).context("could not run tail");
    }
    for (w, f) in pick.iter().zip(&files) {
        if pick.len() > 1 {
            println!("==> {w} ({})", f.display());
        }
        if f.is_file() {
            println!("{}", sys::tail(f, lines));
        } else {
            println!("(no log yet)");
        }
    }
    Ok(())
}

pub fn ssh(name: &str, user: Option<&str>, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let c = VmConfig::load(name)?;
    if !is_running(name) {
        return Err(oops(
            format!("{name} is not running"),
            format!("Start it with `conduit up {name}` or `conduit view {name}`"),
        ));
    }
    let user = user.unwrap_or(&c.user).to_string();
    let err = ssh_cmd(&c, &user).args(args).exec();
    Err(err).context("could not run ssh (is openssh-client installed?)")
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn clipboard_maps_to_the_viewer_modes() {
        assert_eq!(Clipboard::Both.viewer_arg(), "both");
        assert_eq!(Clipboard::ToHost.viewer_arg(), "guest-to-host");
        assert_eq!(Clipboard::ToGuest.viewer_arg(), "host-to-guest");
        assert_eq!(Clipboard::Off.viewer_arg(), "off");
    }

    #[test]
    fn the_viewer_follows_the_desktop() {
        assert_eq!(viewer_session(true, true), Some("wayland"));
        assert_eq!(viewer_session(false, true), Some("x11"));
        assert_eq!(viewer_session(false, false), None);
    }
}
