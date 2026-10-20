//! `conduit _sleep` (`conduit-sleep.service`, a user unit): running VMs survive
//! the host's suspend and continue where they were (docs/SUSPEND.md).
//!
//! It holds a logind (or elogind) "delay" inhibitor for sleep and listens for
//! `PrepareForSleep`. Before the host sleeps it pauses every running VM
//! (`virsh suspend`, QMP `stop`), has each VM's GPU backend hold guest
//! requests and drain the GPU work in flight (the backend's power socket,
//! `device::power`), writes down which VMs it paused and lets go of the
//! inhibitor, all within [`BUDGET`]. After the host wakes it waits for the
//! NVIDIA driver's own resume, has each backend check the GPU and serve
//! again, continues exactly the VMs it paused, takes the inhibitor again
//! and sets the guests' clocks.
//!
//! A VM is paused only when its backend can keep the guest's GPU objects
//! across the pause (it answered `keep`): an older backend would release
//! them when the VM continues, which is worse than not pausing at all.

use crate::dbus::{self, Bus, LOGIN1, LOGIN1_MANAGER, LOGIN1_PATH};
use crate::paths;
use crate::qemu::Qmp;
use crate::run::Rt;
use crate::sys;
use crate::virt::{Link, Virsh};
use crate::vm::VmConfig;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Everything before sleep, within logind's default `InhibitDelayMaxSec` (5 s).
pub const BUDGET: Duration = Duration::from_millis(4000);
/// One VM's pause (`virsh suspend`, QMP `stop`).
const PAUSE_LIMIT: Duration = Duration::from_millis(1500);
/// A backend's `keep` answer, before the VM is paused.
const KEEP_LIMIT: Duration = Duration::from_millis(500);
/// The longest drain a backend is asked for.
const HOLD_WAIT_MAX: Duration = Duration::from_millis(2000);
/// What the hold's answer may take beyond its wait.
const HOLD_SLACK: Duration = Duration::from_millis(300);
/// The NVIDIA driver's own resume (nvidia-resume.service) may take a while
/// with a lot of video memory to restore.
const GPU_RESUME_WAIT: Duration = Duration::from_secs(30);
/// A backend's `release`: its GPU check waits on RM, which waits for the GPU.
const RELEASE_LIMIT: Duration = Duration::from_secs(10);
/// One VM's continue (`virsh resume`, QMP `cont`).
const CONT_LIMIT: Duration = Duration::from_secs(5);

/// How the VM runs, which says how it is paused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "runner", rename_all = "lowercase")]
pub enum Runner {
    Libvirt {
        uri: String,
        domain: String,
    },
    /// `conduit up`/`view` under QEMU: QMP.
    Qemu,
    /// The built-in runner: its vCPUs cannot be paused.
    Builtin,
}

/// A VM that is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vm {
    pub name: String,
    pub runner: Runner,
    /// Its vCPUs run; false when it was already paused (by you, or by
    /// QEMU on an I/O error): such a VM is left exactly as it is.
    pub running: bool,
}

/// What the hook did to a VM before sleep, and so what it undoes after.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub name: String,
    pub runner: Runner,
    /// Its vCPUs were paused by the hook.
    pub vcpus: bool,
    /// Its backend holds guest requests.
    pub held: bool,
}

/// What the plan needs from the world: QEMU, libvirt, the backends, systemd.
/// The real one is [`Real`]; tests use a fake.
pub trait Host {
    fn vms(&mut self) -> Vec<Vm>;
    /// The backend's `keep`: its answer, or why there was none.
    fn keep(&mut self, name: &str, limit: Duration) -> Result<String>;
    fn pause(&mut self, vm: &Vm, limit: Duration) -> Result<()>;
    /// `hold MS` to every backend at once; the answers in the same order.
    fn hold(&mut self, names: &[String], wait: Duration) -> Vec<Result<String>>;
    /// Wait for the NVIDIA driver's resume; false when it did not finish in time.
    fn gpu_ready(&mut self, limit: Duration) -> bool;
    /// `release` to every backend at once.
    fn release(&mut self, names: &[String], limit: Duration) -> Vec<Result<String>>;
    /// Continue a VM the hook paused. `Ok(false)`: it was running already.
    fn resume(&mut self, p: &Paused, limit: Duration) -> Result<bool>;
    fn log(&mut self, line: String);
    /// A desktop notification, for what needs the user.
    fn notify(&mut self, msg: String);
}

/// Before sleep: pause the running VMs, then hold their backends. What was
/// done comes back, to be undone after the wake.
pub fn before_sleep(h: &mut impl Host, budget: Duration) -> Vec<Paused> {
    let start = Instant::now();
    let mut paused: Vec<Paused> = Vec::new();
    for vm in h.vms() {
        let name = vm.name.clone();
        if !vm.running {
            h.log(format!("{name}: already paused; left as it is"));
            continue;
        }
        let left = budget.saturating_sub(start.elapsed());
        if left.is_zero() {
            h.log(format!("{name}: out of time before sleep; left running"));
            continue;
        }
        // First the backend: a VM whose GPU objects would not survive the
        // pause is better left running (the kernel freezes it for the sleep).
        match h.keep(&name, KEEP_LIMIT.min(left)) {
            Ok(r) if r.trim() == "kept" => {}
            Ok(r) => {
                h.log(format!(
                    "{name}: the GPU backend answered {:?} to keep; left running",
                    r.trim()
                ));
                continue;
            }
            Err(e) => {
                h.log(format!(
                    "{name}: no GPU backend power socket ({e:#}); left running (a VM started \
                     by an older conduit gets one at its next start)"
                ));
                continue;
            }
        }
        let vcpus = match vm.runner {
            Runner::Builtin => {
                h.log(format!(
                    "{name}: the built-in runner cannot pause vCPUs; only its GPU backend is held"
                ));
                false
            }
            _ => {
                let left = budget.saturating_sub(start.elapsed());
                match h.pause(&vm, PAUSE_LIMIT.min(left)) {
                    Ok(()) => true,
                    Err(e) => {
                        h.log(format!("{name}: could not pause ({e:#}); left running"));
                        continue;
                    }
                }
            }
        };
        paused.push(Paused {
            name,
            runner: vm.runner,
            vcpus,
            held: false,
        });
    }
    if !paused.is_empty() {
        let left = budget.saturating_sub(start.elapsed());
        let wait = HOLD_WAIT_MAX.min(left.saturating_sub(HOLD_SLACK));
        let names: Vec<String> = paused.iter().map(|p| p.name.clone()).collect();
        let answers = h.hold(&names, wait);
        for (p, a) in paused.iter_mut().zip(answers) {
            match a {
                Ok(line) => {
                    p.held = line.starts_with("held");
                    h.log(format!("{}: GPU backend: {}", p.name, line.trim()));
                }
                Err(e) => h.log(format!(
                    "{}: GPU backend did not answer the hold: {e:#}",
                    p.name
                )),
            }
        }
    }
    h.log(format!(
        "before sleep: {} VM(s) paused in {} ms",
        paused.iter().filter(|p| p.vcpus).count(),
        start.elapsed().as_millis()
    ));
    paused
}

/// After the wake: check the GPU, serve again, continue what was paused.
/// The VMs continued come back (their clocks want setting).
pub fn after_wake(h: &mut impl Host, paused: &[Paused]) -> Vec<Paused> {
    if paused.is_empty() {
        return Vec::new();
    }
    let start = Instant::now();
    if !h.gpu_ready(GPU_RESUME_WAIT) {
        h.log(format!(
            "the NVIDIA driver's resume did not finish within {} s; going on",
            GPU_RESUME_WAIT.as_secs()
        ));
    }
    // Every backend that was told to keep, held or not: `release` checks the
    // GPU and starts the window in which the VM's restart is kept.
    let names: Vec<String> = paused.iter().map(|p| p.name.clone()).collect();
    let answers = h.release(&names, RELEASE_LIMIT);
    for (p, a) in paused.iter().zip(answers) {
        match a {
            Ok(line) if line.starts_with("released lost") => {
                h.log(format!("{}: GPU backend: {}", p.name, line.trim()));
                h.notify(format!(
                    "{}: the GPU did not come back from sleep. Programs using it in the VM \
                     fail; restart them, or the VM",
                    p.name
                ));
            }
            Ok(line) => h.log(format!("{}: GPU backend: {}", p.name, line.trim())),
            Err(e) => h.log(format!(
                "{}: GPU backend did not answer the release: {e:#}",
                p.name
            )),
        }
    }
    let mut continued = Vec::new();
    for p in paused.iter().filter(|p| p.vcpus) {
        match h.resume(p, CONT_LIMIT) {
            Ok(true) => continued.push(p.clone()),
            Ok(false) => h.log(format!("{}: was already running again", p.name)),
            Err(e) => {
                h.log(format!("{}: could not continue: {e:#}", p.name));
                h.notify(format!(
                    "{} is still paused after sleep: `conduit resume {}`",
                    p.name, p.name
                ));
            }
        }
    }
    h.log(format!(
        "after wake: {} VM(s) continued in {} ms",
        continued.len(),
        start.elapsed().as_millis()
    ));
    continued
}

/// From `systemctl show -p ActiveState -p Job nvidia-resume.service`: is the
/// driver's resume still to run or running?
pub fn gpu_resume_pending(show: &str) -> bool {
    show.lines().any(|l| match l.split_once('=') {
        Some(("ActiveState", v)) => matches!(v.trim(), "activating" | "deactivating"),
        Some(("Job", v)) => !v.trim().is_empty(),
        _ => false,
    })
}

// ---------------------------------------------------------------- what was paused

fn paused_file() -> PathBuf {
    paths::xdg_runtime()
        .join("conduit")
        .join("sleep-paused.json")
}

fn save_paused(p: &[Paused]) {
    let f = paused_file();
    if p.is_empty() {
        let _ = std::fs::remove_file(&f);
        return;
    }
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(s) = serde_json::to_string_pretty(p) {
        let _ = sys::write_private(&f, &s);
    }
}

fn load_paused() -> Vec<Paused> {
    std::fs::read_to_string(paused_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the backend's socket

/// The backend's power socket (`--power-socket`), beside its trace socket.
pub fn power_socket(name: &str) -> PathBuf {
    paths::run_dir(name).join("power.sock")
}

/// One command to a VM's backend; its one-line answer.
pub fn backend_ask(name: &str, cmd: &str, limit: Duration) -> Result<String> {
    let mut s = UnixStream::connect(power_socket(name))
        .with_context(|| format!("connecting to {}", power_socket(name).display()))?;
    s.set_write_timeout(Some(limit))?;
    s.set_read_timeout(Some(limit))?;
    writeln!(s, "{cmd}")?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    if out.trim().is_empty() {
        bail!("no answer");
    }
    Ok(out.trim().to_string())
}

/// One command to several backends at once: each works on its own, so the
/// slowest sets the time, not their sum.
fn ask_all(names: &[String], cmd: &str, limit: Duration) -> Vec<Result<String>> {
    let socks: Vec<PathBuf> = names.iter().map(|n| power_socket(n)).collect();
    ask_all_at(&socks, cmd, limit)
}

fn ask_all_at(socks: &[PathBuf], cmd: &str, limit: Duration) -> Vec<Result<String>> {
    let deadline = Instant::now() + limit;
    let sent: Vec<Result<UnixStream>> = socks
        .iter()
        .map(|p| -> Result<UnixStream> {
            let mut s =
                UnixStream::connect(p).with_context(|| format!("connecting to {}", p.display()))?;
            s.set_write_timeout(Some(Duration::from_millis(500)))?;
            writeln!(s, "{cmd}")?;
            Ok(s)
        })
        .collect();
    sent.into_iter()
        .map(|s| {
            let mut s = s?;
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10));
            s.set_read_timeout(Some(left))?;
            let mut out = String::new();
            s.read_to_string(&mut out).context("no answer in time")?;
            if out.trim().is_empty() {
                bail!("no answer");
            }
            Ok(out.trim().to_string())
        })
        .collect()
}

// ---------------------------------------------------------------- the real world

/// The real host.
pub struct Real;

fn virsh_of(uri: &str) -> Virsh {
    Virsh::new(uri)
}

fn qmp_path(name: &str) -> PathBuf {
    paths::run_dir(name).join("qmp.sock")
}

impl Host for Real {
    fn vms(&mut self) -> Vec<Vm> {
        let mut out = Vec::new();
        for name in crate::lvrun::all_names() {
            if let Some(l) = Link::load(&name) {
                let state = l
                    .virsh()
                    .run_within(&["domstate", &l.domain], Duration::from_secs(3))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let running = match state.as_str() {
                    "running" => true,
                    "paused" => false,
                    _ => continue,
                };
                out.push(Vm {
                    name,
                    runner: Runner::Libvirt {
                        uri: l.uri,
                        domain: l.domain,
                    },
                    running,
                });
            } else if crate::run::is_running_unmanaged(&name) {
                let Ok(rt) = Rt::new(&name) else { continue };
                if rt.state().vmm == "qemu" {
                    let running = Qmp::connect_within(&qmp_path(&name), Duration::from_secs(1))
                        .and_then(|mut q| q.running())
                        .unwrap_or(true);
                    out.push(Vm {
                        name,
                        runner: Runner::Qemu,
                        running,
                    });
                } else {
                    out.push(Vm {
                        name,
                        runner: Runner::Builtin,
                        running: true,
                    });
                }
            }
        }
        out
    }

    fn keep(&mut self, name: &str, limit: Duration) -> Result<String> {
        backend_ask(name, "keep", limit)
    }

    fn pause(&mut self, vm: &Vm, limit: Duration) -> Result<()> {
        match &vm.runner {
            Runner::Libvirt { uri, domain } => virsh_of(uri)
                .run_within(&["suspend", domain], limit)
                .map(|_| ())
                .context("virsh suspend failed or did not finish in time"),
            Runner::Qemu => {
                Qmp::connect_within(&qmp_path(&vm.name), limit)?.exec("stop")?;
                Ok(())
            }
            Runner::Builtin => bail!("the built-in runner cannot pause"),
        }
    }

    fn hold(&mut self, names: &[String], wait: Duration) -> Vec<Result<String>> {
        ask_all(
            names,
            &format!("hold {}", wait.as_millis()),
            wait + HOLD_SLACK,
        )
    }

    fn gpu_ready(&mut self, limit: Duration) -> bool {
        if !sys::have("systemctl") {
            return true;
        }
        sys::wait_for(limit, || {
            let show = sys::output_timeout(
                std::process::Command::new("systemctl").args([
                    "show",
                    "-p",
                    "ActiveState",
                    "-p",
                    "Job",
                    "nvidia-resume.service",
                ]),
                Duration::from_secs(2),
            );
            !show.as_deref().is_some_and(gpu_resume_pending)
        })
    }

    fn release(&mut self, names: &[String], limit: Duration) -> Vec<Result<String>> {
        ask_all(names, "release", limit)
    }

    fn resume(&mut self, p: &Paused, limit: Duration) -> Result<bool> {
        match &p.runner {
            Runner::Libvirt { uri, domain } => {
                let v = virsh_of(uri);
                let state = v
                    .run_within(&["domstate", domain], limit)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if state != "paused" {
                    return Ok(false);
                }
                v.run_within(&["resume", domain], limit)
                    .map(|_| true)
                    .context("virsh resume failed or did not finish in time")
            }
            Runner::Qemu => {
                let mut q = Qmp::connect_within(&qmp_path(&p.name), limit)?;
                if q.running()? {
                    return Ok(false);
                }
                q.exec("cont")?;
                Ok(true)
            }
            Runner::Builtin => Ok(false),
        }
    }

    fn log(&mut self, line: String) {
        eprintln!("conduit: sleep: {line}");
    }

    fn notify(&mut self, msg: String) {
        if sys::have("notify-send") {
            let _ = sys::output_timeout(
                std::process::Command::new("notify-send").args(crate::ctl::notify_args(&msg)),
                Duration::from_secs(3),
            );
        }
    }
}

/// After the wake, the guest's clock is behind by the time it slept. Windows
/// guests: the Conduit tray sets it (`settime` on the control channel).
/// Other libvirt guests: the QEMU guest agent, when installed (`virsh domtime
/// --sync`). VMs `conduit up` runs: `hwclock --hctosys` over ssh (the
/// emulated RTC follows the host). All best effort, each bounded.
fn sync_clock(p: &Paused) {
    let now_ms = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    };
    let done = match &p.runner {
        Runner::Libvirt { uri, domain } => {
            let agent = crate::ctl::Client::connect_waiting(&p.name, Duration::from_secs(2))
                .map_err(|e| format!("{e:#}"))
                .and_then(|mut c| {
                    c.set_time(now_ms(), Duration::from_secs(3))
                        .map_err(|e| format!("{e:?}"))
                });
            match agent {
                Ok(()) => "set by the Conduit agent".to_string(),
                Err(why) => match virsh_of(uri)
                    .run_within(&["domtime", domain, "--sync"], Duration::from_secs(5))
                {
                    Some(_) => "set by the QEMU guest agent".to_string(),
                    None => format!(
                        "not set (no Conduit agent that sets it: {why}; no QEMU guest agent); \
                         the guest's NTP corrects it"
                    ),
                },
            }
        }
        Runner::Qemu => match VmConfig::load(&p.name) {
            Ok(c) => {
                let mut cmd = crate::run::ssh_cmd(&c, "root");
                cmd.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=3",
                    "hwclock --hctosys",
                ]);
                match sys::output_timeout(&mut cmd, Duration::from_secs(6)) {
                    Some(_) => "set from the RTC over ssh".to_string(),
                    None => "not set (ssh as root failed); the guest's NTP corrects it".to_string(),
                }
            }
            Err(e) => format!("not set: {e:#}"),
        },
        Runner::Builtin => return,
    };
    eprintln!("conduit: sleep: {}: clock {done}", p.name);
}

/// Continue what a previous run of the hook paused and never saw wake (it
/// was stopped or crashed in between): if it is running now, the host is awake.
fn resume_leftovers() {
    let left = load_paused();
    if left.is_empty() {
        return;
    }
    eprintln!(
        "conduit: sleep: continuing {} VM(s) a previous run paused",
        left.len()
    );
    let continued = after_wake(&mut Real, &left);
    save_paused(&[]);
    std::thread::spawn(move || continued.iter().for_each(sync_clock));
}

fn inhibit(bus: &mut Bus) -> Result<OwnedFd> {
    let r = bus.call(
        LOGIN1,
        LOGIN1_PATH,
        LOGIN1_MANAGER,
        "Inhibit",
        &[
            "sleep",
            "Conduit",
            "Pausing running VMs and draining their GPU work",
            "delay",
        ],
        Duration::from_secs(5),
    )?;
    if r.kind == dbus::ERROR {
        bail!("Inhibit: {}", r.error_text());
    }
    r.fds
        .into_iter()
        .next()
        .context("Inhibit answered without a file descriptor")
}

/// One connection's worth of the hook; an error ends it (the caller reconnects).
fn serve() -> Result<()> {
    resume_leftovers();
    let mut bus = Bus::system()?;
    if !bus.fds_ok {
        bail!("the system bus does not pass file descriptors; no inhibitor can be taken");
    }
    bus.add_match(&dbus::sleep_match_rule())?;
    let mut owner = bus
        .name_owner(LOGIN1)?
        .context("no login manager (systemd-logind or elogind) on the system bus")?;
    eprintln!("conduit: sleep: listening to {LOGIN1} ({owner})");
    let mut lock: Option<OwnedFd> = None;
    let mut said_off = false;
    let mut said_no_lock = false;
    // Between PrepareForSleep(true) and (false): no new inhibitor, or logind
    // would wait for it until its delay runs out.
    let mut asleep = false;
    loop {
        let enabled = crate::config::pause_on_suspend();
        if enabled && lock.is_none() && !asleep {
            match inhibit(&mut bus) {
                Ok(fd) => {
                    lock = Some(fd);
                    said_off = false;
                    said_no_lock = false;
                }
                Err(e) if !said_no_lock => {
                    said_no_lock = true;
                    eprintln!("conduit: sleep: no sleep inhibitor (asked again every 30 s): {e:#}");
                }
                Err(_) => {}
            }
        }
        if !enabled && !said_off {
            lock = None;
            said_off = true;
            eprintln!("conduit: sleep: vm.pause_on_suspend is false: VMs are not paused at sleep");
        }
        let Some(sig) = bus.next_signal(Duration::from_secs(30))? else {
            continue;
        };
        let Some(sleeping) = dbus::prepare_for_sleep(&sig) else {
            continue;
        };
        if sig.sender.as_deref() != Some(owner.as_str()) {
            // logind restarted, or someone else sent it.
            match bus.name_owner(LOGIN1)? {
                Some(o) if sig.sender.as_deref() == Some(o.as_str()) => owner = o,
                _ => continue,
            }
        }
        if sleeping {
            if crate::config::pause_on_suspend() {
                if lock.is_none() {
                    eprintln!(
                        "conduit: sleep: no inhibitor held; the host may sleep before this finishes"
                    );
                }
                let paused = before_sleep(&mut Real, BUDGET);
                save_paused(&paused);
            }
            // Let the host sleep.
            lock = None;
            asleep = true;
        } else {
            asleep = false;
            let paused = load_paused();
            let continued = after_wake(&mut Real, &paused);
            save_paused(&[]);
            std::thread::spawn(move || continued.iter().for_each(sync_clock));
        }
    }
}

/// `conduit _sleep`: run until stopped, reconnecting to the bus as needed.
pub fn run() -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        let t = Instant::now();
        if let Err(e) = serve() {
            eprintln!("conduit: sleep: {e:#}; again in {} s", backoff.as_secs());
        }
        if t.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Install and start the sleep hook (`conduit-sleep.service`) when this
/// session has a systemd user manager. Best effort: VMs work without it.
pub fn ensure_hook() {
    let Ok(exe) = crate::virt::conduit_exe() else {
        return;
    };
    if crate::units::check(&crate::units::Scope::User).is_err() {
        return;
    }
    if let Err(e) = crate::units::ensure_sleep_hook(&exe) {
        crate::ui::warn(format!(
            "could not start the sleep hook ({e:#}): VMs will not be paused when this computer sleeps"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// A world on paper: which VMs exist, how their backends and runners
    /// answer, and a record of everything done to them.
    #[derive(Default)]
    struct Fake {
        vms: Vec<Vm>,
        /// Backends without a power socket (older), or answering otherwise.
        keep_answer: HashMap<String, Result<String, String>>,
        pause_fails: HashSet<String>,
        hold_answer: HashMap<String, Result<String, String>>,
        release_answer: HashMap<String, Result<String, String>>,
        /// VMs someone continued while the host slept.
        resumed_meanwhile: HashSet<String>,
        resume_fails: HashSet<String>,
        gpu_ready: bool,
        /// A slow pause eats the budget.
        pause_takes: Duration,
        done: Vec<String>,
        notes: Vec<String>,
    }

    fn vm(name: &str, runner: Runner, running: bool) -> Vm {
        Vm {
            name: name.into(),
            runner,
            running,
        }
    }

    fn lv(d: &str) -> Runner {
        Runner::Libvirt {
            uri: "qemu:///session".into(),
            domain: d.into(),
        }
    }

    fn answer(m: &HashMap<String, Result<String, String>>, n: &str, dflt: &str) -> Result<String> {
        match m.get(n) {
            Some(Ok(s)) => Ok(s.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Ok(dflt.into()),
        }
    }

    impl Host for Fake {
        fn vms(&mut self) -> Vec<Vm> {
            self.vms.clone()
        }
        fn keep(&mut self, name: &str, _: Duration) -> Result<String> {
            self.done.push(format!("keep {name}"));
            answer(&self.keep_answer, name, "kept")
        }
        fn pause(&mut self, vm: &Vm, limit: Duration) -> Result<()> {
            assert!(limit <= PAUSE_LIMIT);
            std::thread::sleep(self.pause_takes);
            if self.pause_fails.contains(&vm.name) {
                bail!("QMP stop: timed out");
            }
            self.done.push(format!("pause {}", vm.name));
            Ok(())
        }
        fn hold(&mut self, names: &[String], wait: Duration) -> Vec<Result<String>> {
            assert!(wait <= HOLD_WAIT_MAX);
            self.done.push(format!("hold {}", names.join(",")));
            names
                .iter()
                .map(|n| {
                    answer(
                        &self.hold_answer,
                        n,
                        "held idle fences=0 chains=0 waited_ms=1",
                    )
                })
                .collect()
        }
        fn gpu_ready(&mut self, _: Duration) -> bool {
            self.done.push("gpu_ready".into());
            self.gpu_ready
        }
        fn release(&mut self, names: &[String], _: Duration) -> Vec<Result<String>> {
            self.done.push(format!("release {}", names.join(",")));
            names
                .iter()
                .map(|n| answer(&self.release_answer, n, "released healthy tsgs=2"))
                .collect()
        }
        fn resume(&mut self, p: &Paused, _: Duration) -> Result<bool> {
            if self.resume_fails.contains(&p.name) {
                bail!("virsh resume failed");
            }
            if self.resumed_meanwhile.contains(&p.name) {
                return Ok(false);
            }
            self.done.push(format!("resume {}", p.name));
            Ok(true)
        }
        fn log(&mut self, _line: String) {}
        fn notify(&mut self, msg: String) {
            self.notes.push(msg);
        }
    }

    #[test]
    fn running_vms_are_paused_and_held_then_continued() {
        let mut f = Fake {
            vms: vec![
                vm("win11", lv("win11"), true),
                vm("omarchy", Runner::Qemu, true),
            ],
            gpu_ready: true,
            ..Fake::default()
        };
        let paused = before_sleep(&mut f, BUDGET);
        assert_eq!(
            f.done,
            [
                "keep win11",
                "pause win11",
                "keep omarchy",
                "pause omarchy",
                "hold win11,omarchy"
            ]
        );
        assert!(paused.iter().all(|p| p.vcpus && p.held));
        f.done.clear();
        let continued = after_wake(&mut f, &paused);
        assert_eq!(
            f.done,
            [
                "gpu_ready",
                "release win11,omarchy",
                "resume win11",
                "resume omarchy"
            ],
            "the GPU is checked before any VM continues"
        );
        assert_eq!(continued.len(), 2);
        assert!(f.notes.is_empty());
    }

    #[test]
    fn only_vms_the_hook_paused_are_continued() {
        let mut f = Fake {
            vms: vec![
                vm("mine", lv("mine"), false), // paused by the user
                vm("off-socket", Runner::Qemu, true),
                vm("stuck", Runner::Qemu, true),
                vm("ok", Runner::Qemu, true),
            ],
            keep_answer: [(
                "off-socket".to_string(),
                Err("connecting to power.sock: No such file".to_string()),
            )]
            .into(),
            pause_fails: ["stuck".to_string()].into(),
            gpu_ready: true,
            ..Fake::default()
        };
        let paused = before_sleep(&mut f, BUDGET);
        let names: Vec<&str> = paused.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["ok"], "{:?}", f.done);
        assert!(
            !f.done.iter().any(|d| d == "pause off-socket"),
            "an older backend's VM is never paused"
        );
        assert!(!f.done.iter().any(|d| d.contains("keep mine")));
        f.done.clear();
        after_wake(&mut f, &paused);
        assert_eq!(f.done, ["gpu_ready", "release ok", "resume ok"]);
    }

    #[test]
    fn a_vm_continued_meanwhile_is_left_alone_and_a_failed_continue_is_reported() {
        let paused = vec![
            Paused {
                name: "a".into(),
                runner: lv("a"),
                vcpus: true,
                held: true,
            },
            Paused {
                name: "b".into(),
                runner: Runner::Qemu,
                vcpus: true,
                held: true,
            },
        ];
        let mut f = Fake {
            resumed_meanwhile: ["a".to_string()].into(),
            resume_fails: ["b".to_string()].into(),
            gpu_ready: false,
            ..Fake::default()
        };
        let continued = after_wake(&mut f, &paused);
        assert!(continued.is_empty());
        assert_eq!(f.notes.len(), 1);
        assert!(f.notes[0].contains("conduit resume b"));
        assert!(
            f.done.contains(&"release a,b".to_string()),
            "a slow GPU resume does not stop the release"
        );
    }

    #[test]
    fn the_budget_bounds_the_pause_and_the_hold() {
        let mut f = Fake {
            vms: vec![
                vm("a", Runner::Qemu, true),
                vm("b", Runner::Qemu, true),
                vm("c", Runner::Qemu, true),
            ],
            pause_takes: Duration::from_millis(60),
            ..Fake::default()
        };
        let t = Instant::now();
        let paused = before_sleep(&mut f, Duration::from_millis(100));
        assert!(t.elapsed() < Duration::from_millis(400));
        // Two pauses fit in 100 ms of 60 ms each at most; the third is left running.
        assert!(paused.len() < 3, "{:?}", f.done);
        assert!(!paused.is_empty());
    }

    #[test]
    fn a_busy_or_silent_backend_is_not_held_but_the_vm_stays_paused() {
        let mut f = Fake {
            vms: vec![vm("a", Runner::Qemu, true), vm("b", Runner::Qemu, true)],
            hold_answer: [
                (
                    "a".to_string(),
                    Ok("busy request in flight; not held".to_string()),
                ),
                ("b".to_string(), Err("no answer in time".to_string())),
            ]
            .into(),
            gpu_ready: true,
            ..Fake::default()
        };
        let paused = before_sleep(&mut f, BUDGET);
        assert_eq!(paused.len(), 2);
        assert!(paused.iter().all(|p| p.vcpus && !p.held));
        // Released anyway: that starts the keep window for their restart.
        f.done.clear();
        after_wake(&mut f, &paused);
        assert_eq!(f.done, ["gpu_ready", "release a,b", "resume a", "resume b"]);
    }

    #[test]
    fn a_lost_gpu_is_reported_and_the_vm_still_continues() {
        let paused = vec![Paused {
            name: "win11".into(),
            runner: lv("win11"),
            vcpus: true,
            held: true,
        }];
        let mut f = Fake {
            release_answer: [(
                "win11".to_string(),
                Ok("released lost tsgs=3 lost=3 status=0xf".to_string()),
            )]
            .into(),
            gpu_ready: true,
            ..Fake::default()
        };
        let continued = after_wake(&mut f, &paused);
        assert_eq!(
            continued.len(),
            1,
            "it continues: its fences end in errors, not hangs"
        );
        assert_eq!(f.notes.len(), 1);
        assert!(f.notes[0].contains("did not come back"));
    }

    #[test]
    fn the_builtin_runner_is_held_but_never_paused_or_continued() {
        let mut f = Fake {
            vms: vec![vm("lab", Runner::Builtin, true)],
            gpu_ready: true,
            ..Fake::default()
        };
        let paused = before_sleep(&mut f, BUDGET);
        assert_eq!(f.done, ["keep lab", "hold lab"]);
        assert_eq!(paused.len(), 1);
        assert!(!paused[0].vcpus && paused[0].held);
        f.done.clear();
        let continued = after_wake(&mut f, &paused);
        assert_eq!(f.done, ["gpu_ready", "release lab"]);
        assert!(continued.is_empty());
    }

    #[test]
    fn nothing_running_does_nothing() {
        let mut f = Fake::default();
        assert!(before_sleep(&mut f, BUDGET).is_empty());
        assert!(f.done.is_empty());
        assert!(after_wake(&mut f, &[]).is_empty());
        assert!(f.done.is_empty(), "no GPU wait without a VM");
    }

    #[test]
    fn paused_records_round_trip() {
        let p = vec![
            Paused {
                name: "w".into(),
                runner: lv("w"),
                vcpus: true,
                held: false,
            },
            Paused {
                name: "o".into(),
                runner: Runner::Qemu,
                vcpus: true,
                held: true,
            },
        ];
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"runner\":\"libvirt\""));
        let back: Vec<Paused> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn the_drivers_resume_is_waited_for() {
        assert!(gpu_resume_pending("ActiveState=activating\nJob=\n"));
        assert!(gpu_resume_pending("ActiveState=inactive\nJob=812\n"));
        assert!(!gpu_resume_pending("ActiveState=inactive\nJob=\n"));
        assert!(!gpu_resume_pending("ActiveState=active\n"));
        assert!(!gpu_resume_pending(""));
    }

    #[test]
    fn backends_are_asked_at_once() {
        let dir = std::env::temp_dir().join(format!("conduit-sleep-ask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Two fake backends answering after 150 ms each: together well under 300.
        let mut socks = Vec::new();
        for n in ["a", "b"] {
            let sock = dir.join(format!("{n}.sock"));
            let _ = std::fs::remove_file(&sock);
            let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line).unwrap();
                std::thread::sleep(Duration::from_millis(150));
                writeln!(
                    s,
                    "held idle fences=0 chains=0 waited_ms=150 ({})",
                    line.trim()
                )
                .unwrap();
            });
            socks.push(sock);
        }
        let t = Instant::now();
        let r = ask_all_at(&socks, "hold 150", Duration::from_secs(2));
        assert!(
            t.elapsed() < Duration::from_millis(290),
            "{:?}",
            t.elapsed()
        );
        assert!(r
            .iter()
            .all(|a| a.as_ref().is_ok_and(|s| s.contains("hold 150"))));
        let missing = ask_all_at(
            &[dir.join("nope.sock")],
            "hold 1",
            Duration::from_millis(50),
        );
        assert!(missing[0].is_err());
        assert!(power_socket("win11").ends_with("conduit/win11/power.sock"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
