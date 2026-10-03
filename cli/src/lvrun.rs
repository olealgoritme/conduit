//! Running VMs that are libvirt domains: `conduit up/view/down/status` drive
//! libvirt, and the helpers systemd starts around the domain (`_backend`,
//! `_virtiofsd`, `_stopped`) live here.
//!
//! The backend's display mode is chosen when it starts: `conduit up --display`
//! / `conduit view` leave the wanted mode in the runtime folder
//! (`libvirt-next-mode`, "none" = headless) right before starting the domain;
//! a start from virt-manager or virsh uses the monitor's mode. The backend
//! records what it got in `libvirt-mode`, which `conduit view` reads to size a
//! window for a VM that is already running.

use crate::boot;
use crate::config;
use crate::hypr;
use crate::mode::{self, Mode};
use crate::net;
use crate::paths::{self, Tool};
use crate::run::{self, Rt, State};
use crate::sys;
use crate::ui::{self, oops};
use crate::units::{self, Scope};
use crate::virt::{self, Kind, Link};
use crate::vm::VmConfig;
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const NEXT_MODE: &str = "libvirt-next-mode";
const MODE: &str = "libvirt-mode";
/// How long an ACPI shutdown may take before the VM is forced off.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(60);

/// Conduit's VMs: its own, and libvirt VMs `conduit attach` added the GPU to.
pub fn all_names() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(paths::vms_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path().join("vm.json").is_file() || e.path().join("libvirt.json").is_file()
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn logs_dir(name: &str) -> PathBuf {
    paths::vm_dir(name).join("logs")
}

fn up_state(link: &Link) -> Option<String> {
    link.virsh().state(&link.domain)
}

fn is_up(link: &Link) -> bool {
    up_state(link).is_some_and(|s| virt::state_is_up(&s))
}

/// Checks and host-side preparation before the domain starts.
fn before_start(name: &str, link: &Link) -> Result<()> {
    let scope = link.scope()?;
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
    Tool::Backend.require()?;
    if link.kind == Kind::Managed {
        let c = VmConfig::load(name)?;
        run::check_memory(&c)?;
        // Boot the newest kernel installed in the VM (the domain points at
        // these copies).
        if c.kernel.is_none() {
            boot::resolve(&c)?;
        }
        // Normally conduit-net-NAME.service keeps it up; fix it if not.
        net::up(&c)?;
    }
    Ok(())
}

fn write_next_mode(rt: &Rt, mode: Option<Mode>) -> Result<()> {
    let v = mode.map(|m| m.to_string()).unwrap_or_else(|| "none".into());
    std::fs::write(rt.p(NEXT_MODE), v + "\n")?;
    Ok(())
}

/// The running backend's mode: Some(Some(m)) display, Some(None) headless.
fn running_mode(rt: &Rt) -> Option<Option<Mode>> {
    let s = std::fs::read_to_string(rt.p(MODE)).ok()?;
    let s = s.trim();
    if s == "none" {
        Some(None)
    } else {
        s.parse().ok().map(Some)
    }
}

pub fn up(name: &str, link: &Link, display: Option<Mode>, headless: bool) -> Result<()> {
    link.virsh().reachable()?;
    if up_state(link).as_deref() == Some("paused") {
        link.virsh().run(&["resume", &link.domain])?;
        ui::info(format!("{name} was paused; resumed it"));
        return Ok(());
    }
    if is_up(link) {
        ui::info(format!(
            "{name} is already running (`conduit status {name}`)"
        ));
        return Ok(());
    }
    before_start(name, link)?;
    let rt = Rt::new(name)?;
    let mode = if headless {
        None
    } else {
        Some(display.unwrap_or_else(|| mode::detect().0))
    };
    write_next_mode(&rt, mode)?;
    virt::start(name, link)?;
    ui::info(format!(
        "{name} is starting (libvirt {}){}.",
        link.uri,
        mode.map(|m| format!(" with a {m} display"))
            .unwrap_or_default()
    ));
    if mode.is_some() {
        ui::info(format!("open its screen with `conduit view {name}`"));
    }
    if let Ok(c) = VmConfig::load(name) {
        ui::info(format!(
            "in about 20 seconds: `conduit ssh {name}` (VM address {})",
            c.net().guest_ip
        ));
    }
    Ok(())
}

pub fn view(
    name: &str,
    link: &Link,
    req: Option<Mode>,
    tune_hyprland: bool,
    fullscreen: bool,
    keep_running: bool,
) -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return Err(oops(
            "no desktop session found (neither WAYLAND_DISPLAY nor DISPLAY is set)",
            "Run `conduit view` from a terminal inside your desktop (Wayland or X11), not over ssh or a text console",
        ));
    }
    let viewer = Tool::Viewer.require()?;
    link.virsh().reachable()?;
    let rt = Rt::new(name)?;
    let logs = logs_dir(name);
    std::fs::create_dir_all(&logs)?;
    let _lock = sys::lock(&rt.p("lock"), Duration::from_secs(60))?;
    let running = is_up(link);
    let mode = if running {
        if up_state(link).as_deref() == Some("paused") {
            ui::info(format!(
                "{name} is paused; the window shows its last frame until `virsh resume {}`",
                link.domain
            ));
        }
        // The backend starts a moment after QEMU: give it time to say its mode.
        sys::wait_for(Duration::from_secs(10), || running_mode(&rt).is_some());
        match running_mode(&rt) {
            Some(Some(m)) => {
                if let Some(r) = req.filter(|r| *r != m) {
                    ui::warn(format!(
                        "{name} is already running at {m}; showing that instead of {r} (`conduit down {name}` first to change it)"
                    ));
                }
                m
            }
            Some(None) => {
                return Err(oops(
                    format!("{name} was started without a display (`up --headless`)"),
                    format!("Run `conduit down {name}`, then `conduit view {name}`"),
                ))
            }
            None => {
                return Err(oops(
                    format!("{name} is running but its GPU backend has not started"),
                    format!("See `conduit logs {name} backend` and `conduit doctor {name}`"),
                ))
            }
        }
    } else {
        let m = match req {
            Some(m) => m,
            None => {
                let (m, src) = mode::detect();
                ui::info(format!("VM display: {m} (from {src})"));
                m
            }
        };
        before_start(name, link)?;
        write_next_mode(&rt, Some(m))?;
        m
    };
    let close_stops_vm = !running && !keep_running && config::close_stops_vm();
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
        if !running {
            virt::start(name, link)?;
        }
        let whole_run = run::start_viewer(
            name,
            &logs,
            &rt,
            &viewer,
            mode,
            tune.as_deref(),
            fullscreen,
            close_stops_vm,
        )?;
        if whole_run {
            hypr::hook("on", &rt.hypr_state())?;
        }
        let st = State {
            mode: Some(mode.to_string()),
            viewer_comm: paths::comm_of(&viewer),
            watcher_comm: run::self_comm(),
            close_stops_vm,
            ..Default::default()
        };
        rt.save_state(&st)?;
        run::start_watcher(name, &logs, &rt)
    })();
    if let Err(e) = result {
        stop_viewer(&rt);
        if !running {
            let _ = virt::shutdown(link, Duration::from_secs(5));
        }
        return Err(e);
    }
    ui::info(format!(
        "{name} is {}. Ctrl+Alt+F fullscreen, Ctrl+Alt+G capture mouse. {}",
        if running {
            "running; viewer opened"
        } else {
            "starting"
        },
        run::close_note(close_stops_vm, name)
    ));
    Ok(())
}

fn stop_viewer(rt: &Rt) {
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
    if !st.viewer_comm.is_empty() {
        sys::stop_pid(
            &rt.p("viewer.pid"),
            &st.viewer_comm,
            "viewer",
            Duration::from_secs(5),
        );
    }
    for f in ["viewer.pid", "watcher.pid", "state.json"] {
        let _ = std::fs::remove_file(rt.p(f));
    }
    if rt.hypr_state().exists() {
        let _ = hypr::hook("restore", &rt.hypr_state());
    }
    let _ = sys::clear_stale_socket(&rt.display_sock());
}

/// Background: the viewer closed or the domain stopped.
pub fn watch(name: &str, link: &Link) -> Result<()> {
    let rt = Rt::new(name)?;
    let st = rt.state();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let viewer = rt.pid("viewer", &st.viewer_comm).is_some();
        if !is_up(link) {
            ui::info(format!("{name} stopped; closing its window"));
            break;
        }
        if !viewer {
            if st.close_stops_vm {
                ui::info(format!("viewer window closed; shutting {name} down"));
                virt::shutdown(link, SHUTDOWN_GRACE)?;
            } else {
                ui::info(format!("viewer window closed; {name} keeps running"));
            }
            break;
        }
    }
    stop_viewer(&rt);
    Ok(())
}

pub fn down(name: &str, link: &Link) -> Result<()> {
    link.virsh().reachable()?;
    let rt = Rt::new(name)?;
    if is_up(link) {
        ui::info(format!("shutting down {name} cleanly…"));
        if !virt::shutdown(link, SHUTDOWN_GRACE)? {
            ui::warn(format!(
                "{name} did not shut down within a minute; it was powered off"
            ));
        }
    }
    stop_viewer(&rt);
    // The backend exits when QEMU disconnects; make sure.
    if let Ok(scope) = link.scope() {
        let gone = sys::wait_for(Duration::from_secs(10), || {
            !units::HELPERS
                .iter()
                .any(|h| units::active(&scope, name, h, "service"))
        });
        if !gone {
            ui::warn("the GPU backend did not exit with the VM; stopping it");
            for h in units::HELPERS {
                let u = units::unit(h, name, "service");
                let _ = match scope {
                    Scope::User => sys::output("systemctl", &["--user", "stop", &u]).map(|_| ()),
                    Scope::System { .. } => sys::sudo("systemctl", &["stop", &u]),
                };
            }
        }
    }
    ui::info(format!("{name} is stopped"));
    Ok(())
}

pub fn status(name: &str, link: &Link) -> Result<()> {
    let state = up_state(link).unwrap_or_else(|| "not defined in libvirt".into());
    println!("{name}: {state}");
    println!(
        "  libvirt  {} domain \"{}\" on {}",
        if link.kind == Kind::Managed {
            "Conduit"
        } else {
            "attached"
        },
        link.domain,
        link.uri
    );
    if let Ok(scope) = link.scope() {
        for h in units::HELPERS {
            let sock = if units::active(&scope, name, h, "socket") {
                "listening"
            } else {
                "NOT listening"
            };
            let svc = if units::active(&scope, name, h, "service") {
                "running"
            } else {
                "not running"
            };
            println!("  {h:<9} {svc}, socket {sock}");
        }
    }
    let rt = Rt::new(name)?;
    if virt::state_is_up(&state) {
        match running_mode(&rt) {
            Some(Some(m)) => println!("  display  {m}"),
            Some(None) => println!("  display  none (headless)"),
            None => {}
        }
    }
    let st = rt.state();
    if let Some(p) = rt.pid("viewer", &st.viewer_comm) {
        println!(
            "  viewer   open (pid {p}); closing it {}",
            if st.close_stops_vm {
                "shuts the VM down"
            } else {
                "leaves the VM running"
            }
        );
    }
    if let Ok(c) = VmConfig::load(name) {
        println!("  network  {}", net::describe(&c));
        if virt::state_is_up(&state) && state != "paused" {
            let ok = run::ssh_cmd(&c, "root")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "true"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            println!(
                "  guest    {}",
                if ok {
                    "reachable over ssh"
                } else {
                    "not reachable yet (it may still be booting)"
                }
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- unit helpers

/// stdout and stderr to the VM's log (the unit's own output goes to the journal).
fn log_to(file: &Path, append: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(file.parent().unwrap())?;
    let mut o = std::fs::OpenOptions::new();
    o.create(true);
    if append {
        o.append(true);
    } else {
        o.write(true).truncate(true);
    }
    let f = o
        .open(file)
        .with_context(|| format!("opening {}", file.display()))?;
    // SAFETY: dup2 onto the standard descriptors of this process.
    unsafe {
        libc::dup2(f.as_raw_fd(), 1);
        libc::dup2(f.as_raw_fd(), 2);
    }
    Ok(())
}

fn need_link(name: &str) -> Result<Link> {
    Link::load(name).ok_or_else(|| {
        oops(
            format!("{name} is not a libvirt VM of Conduit's"),
            format!("Remove the stale unit: systemctl --user disable --now conduit-backend@{name}.socket"),
        )
    })
}

/// `conduit _backend NAME` (conduit-backend@NAME.service): pick the display,
/// then become the backend, keeping systemd's listening socket (fd 3).
pub fn backend_exec(name: &str) -> Result<()> {
    let _link = need_link(name)?;
    let rt = Rt::new(name)?;
    log_to(&logs_dir(name).join("backend.log"), false)?;
    let next = std::fs::read_to_string(rt.p(NEXT_MODE)).ok();
    let _ = std::fs::remove_file(rt.p(NEXT_MODE));
    let mode: Option<Mode> = match next.as_deref().map(str::trim) {
        Some("none") => None,
        Some(s) if s.parse::<Mode>().is_ok() => s.parse().ok(),
        _ => Some(mode::detect().0),
    };
    std::fs::write(
        rt.p(MODE),
        mode.map(|m| m.to_string()).unwrap_or_else(|| "none".into()) + "\n",
    )?;
    let backend = Tool::Backend.require()?;
    let mut cmd = Command::new(&backend);
    // With socket activation the backend serves fd 3; --socket only names the
    // folder its sandbox may reach (the viewer's display socket is there).
    cmd.arg("--socket")
        .arg(rt.p("gpu-libvirt.sock"))
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
    eprintln!(
        "conduit: starting the GPU backend for {name} (socket activation), display {}",
        mode.map(|m| m.to_string()).unwrap_or_else(|| "none".into())
    );
    let e = cmd.exec();
    Err(e).with_context(|| format!("could not run {}", backend.display()))
}

/// `conduit _virtiofsd NAME` (conduit-virtiofsd@NAME.service): stage the
/// NVIDIA share if needed, then become virtiofsd on systemd's socket (fd 3).
pub fn virtiofsd_exec(name: &str) -> Result<()> {
    let link = need_link(name)?;
    let logs = logs_dir(name);
    log_to(&logs.join("virtiofsd.log"), false)?;
    let share = match (link.kind, VmConfig::load(name)) {
        (Kind::Managed, Ok(c)) => run::ensure_share(&c)?,
        _ => run::ensure_share_for(None, &Link::file(name), &logs)?,
    };
    let vfsd = run::need_virtiofsd()?;
    let mut cmd = Command::new(&vfsd);
    cmd.arg("--fd=3")
        .arg(format!("--shared-dir={}", share.display()))
        .args(["--sandbox=none", "--cache=auto", "--log-level=warn"]);
    if crate::qemu::supports_readonly(&vfsd) {
        cmd.arg("--readonly");
    } else {
        eprintln!(
            "conduit: this virtiofsd has no --readonly; the NVIDIA share is writable by the VM"
        );
    }
    let e = cmd.exec();
    Err(e).with_context(|| format!("could not run {}", vfsd.display()))
}

/// `conduit _stopped NAME` (after the backend): refresh the boot files so a
/// kernel updated inside the VM boots next time; forget the run's display.
pub fn stopped(name: &str) -> Result<()> {
    log_to(&logs_dir(name).join("backend.log"), true)?;
    if let Ok(rt) = Rt::new(name) {
        let _ = std::fs::remove_file(rt.p(MODE));
    }
    if let Some(l) = Link::load(name) {
        if l.kind == Kind::Managed {
            if let Ok(c) = VmConfig::load(name) {
                if c.kernel.is_none() {
                    match boot::resolve(&c) {
                        Ok(b) => eprintln!("conduit: next boot: {}", b.describe()),
                        Err(e) => eprintln!("conduit: could not refresh the boot files: {e:#}"),
                    }
                }
            }
        }
    }
    eprintln!("conduit: {name}'s GPU backend stopped with the VM");
    Ok(())
}

/// Every Conduit VM that libvirt knows, with its state (for `conduit list`).
pub fn state_word(name: &str) -> String {
    match Link::load(name) {
        Some(l) => l
            .virsh()
            .state(&l.domain)
            .map(|s| if s == "shut off" { "stopped".into() } else { s })
            .unwrap_or_else(|| "undefined".into()),
        None => {
            if run::is_running(name) {
                "running".into()
            } else {
                "stopped".into()
            }
        }
    }
}
