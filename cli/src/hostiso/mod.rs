//! Host isolation: while a libvirt VM runs, host work and device interrupts
//! stay off its CPUs.
//!
//! The split comes from the policy ([`policy`]): the VM's CPUs (its pinning,
//! or a placement on the CPU's topology: a CCD, the P-cores, a NUMA node) and
//! the host's (everything else). Applying it has two halves:
//!
//! - **System** (root): `conduit-isolation@UID_CPUS.service`, a oneshot unit
//!   with `RemainAfterExit=yes` that runs `conduit _isolation apply|revert`.
//!   It sets `AllowedCPUs=` on system.slice and the login session scopes,
//!   moves every device interrupt (and the default for new ones) to the
//!   host's CPUs, sets the unbound workqueue mask, and stops irqbalance for
//!   as long as any isolation is active. A polkit rule lets local users
//!   start and stop exactly these units, so no password is asked.
//! - **User**: `AllowedCPUs=` on the units of your own systemd manager
//!   (apps, terminals, Conduit's own slices with the backend), through
//!   `systemctl --user`.
//!
//! A cgroup that holds a VM's QEMU (or the libvirt daemon that starts them)
//! is never restricted: QEMU's vCPU threads are pinned by libvirt
//! (`<vcpupin>`, or live at the start for a domain without pinning) and its
//! own threads by `<emulatorpin>`.
//!
//! When: `conduit _backend` (the VM's backend unit, started by every VM start
//! however it was started) runs `conduit _isolation start NAME` in the
//! background, which waits for the domain to run and applies; `_stopped`
//! (after the backend) and `conduit down` revert. Both ends are idempotent,
//! and a start first reverts what a VM that is no longer running left
//! behind, so a crash costs nothing past the next start.

pub mod apply;
pub mod cpus;
pub mod domain;
pub mod policy;
pub mod topo;

use crate::config;
use crate::paths;
use crate::sys;
use crate::virt::{self, Link};
use anyhow::{bail, Context, Result};
use apply::{Changes, Entry, Ledger, Roots, Units};
use cpus::CpuSet;
use policy::{Pins, Plan, Setting};
use std::path::{Path, PathBuf};
use std::time::Duration;
use topo::Topology;

/// The root helper's state (world-readable: users read the CPUs other VMs hold).
pub const STATE_DIR: &str = "/run/conduit-isolation";
/// The template unit the packages install.
pub const UNIT_TEMPLATE: &str = "conduit-isolation@.service";
/// Where the packages put the unit (the tarball: /etc/systemd/system).
const UNIT_FILES: [&str; 3] = [
    "/usr/lib/systemd/system/conduit-isolation@.service",
    "/lib/systemd/system/conduit-isolation@.service",
    "/etc/systemd/system/conduit-isolation@.service",
];

const SYSTEMCTL_LIMIT: Duration = Duration::from_secs(30);
const VIRSH_LIMIT: Duration = Duration::from_secs(10);
/// How long `start` waits for the domain to run after its backend started.
const RUN_WAIT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------- systemd

/// `systemctl` (or `systemctl --user`), every call with a deadline.
pub struct Systemctl {
    pub user: bool,
}

impl Systemctl {
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut a: Vec<&str> = Vec::new();
        if self.user {
            a.push("--user");
        }
        a.push("--no-ask-password");
        a.extend_from_slice(args);
        sys::output_within("systemctl", &a, SYSTEMCTL_LIMIT)
    }
}

impl Units for Systemctl {
    fn allowed_cpus(&self, unit: &str) -> Result<String> {
        Ok(self
            .run(&["show", "-p", "AllowedCPUs", "--value", unit])?
            .trim()
            .to_string())
    }
    fn set_allowed_cpus(&self, unit: &str, cpus: &str) -> Result<()> {
        self.run(&[
            "set-property",
            "--runtime",
            unit,
            &format!("AllowedCPUs={cpus}"),
        ])
        .map(|_| ())
    }
    fn is_active(&self, unit: &str) -> bool {
        self.run(&["is-active", "--quiet", unit]).is_ok()
    }
    fn stop(&self, unit: &str) -> Result<()> {
        self.run(&["stop", unit]).map(|_| ())
    }
    fn start(&self, unit: &str) -> Result<()> {
        self.run(&["start", unit]).map(|_| ())
    }
}

// ---------------------------------------------------------------- instances

/// The root helper's instance for a user's VM CPUs: `1000_8-15_24-31`.
pub fn instance(uid: u32, guest: &CpuSet) -> String {
    format!("{uid}_{}", guest.instance_form())
}

pub fn unit_of(instance: &str) -> String {
    format!("conduit-isolation@{instance}.service")
}

/// `UID_RANGES` back to (uid, CPUs); strict, since it reaches root.
pub fn parse_instance(s: &str) -> Option<(u32, CpuSet)> {
    if s.is_empty()
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return None;
    }
    let (uid, ranges) = s.split_once('_')?;
    let uid: u32 = uid.parse().ok()?;
    let set = CpuSet::parse(&ranges.replace('_', ","))?;
    (!set.is_empty()).then_some((uid, set))
}

// ---------------------------------------------------------------- root helper

/// `conduit _isolation apply|revert INSTANCE`, as root from
/// conduit-isolation@.service.
pub fn helper(action: &str, inst: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("this runs as root from conduit-isolation@.service; use `conduit doctor NAME` to see host isolation");
    }
    let r = Roots::host();
    let topo = Topology::host().context("cannot read the CPU topology from /sys")?;
    let dir = Path::new(STATE_DIR);
    std::fs::create_dir_all(dir)?;
    let _lock = sys::lock(&dir.join("lock"), Duration::from_secs(20))?;
    let file = dir.join("state.json");
    let mut l = Ledger::load(&file);
    let (line, res) = helper_with(&Systemctl { user: false }, &r, &topo, &mut l, action, inst);
    l.save(&file)?;
    println!("{line}");
    res
}

/// The helper's body, on any roots and systemd (tests use fakes). Returns
/// the log line and whether it worked.
pub fn helper_with(
    units: &dyn Units,
    r: &Roots,
    topo: &Topology,
    l: &mut Ledger,
    action: &str,
    inst: &str,
) -> (String, Result<()>) {
    let Some((uid, guest)) = parse_instance(inst) else {
        return (
            format!("host isolation: refused instance {inst:?} (expected UID_CPUS)"),
            Err(anyhow::anyhow!("bad instance {inst:?}")),
        );
    };
    let mut ch = Changes::default();
    match action {
        "apply" => {
            if !guest.is_subset(&topo.online) {
                let line = format!(
                    "host isolation: refused {inst}: CPUs {} are not online",
                    guest.minus(&topo.online)
                );
                return (line.clone(), Err(anyhow::anyhow!(line)));
            }
            let others = l.guests(Some(inst));
            if guest.overlaps(&others) {
                let line = format!(
                    "host isolation: refused {inst}: CPUs {} belong to another running VM",
                    guest.intersect(&others)
                );
                return (line.clone(), Err(anyhow::anyhow!(line)));
            }
            let host = topo.online.minus(&others.union(&guest));
            if topo.cores_in(&host) < policy::HOST_FLOOR_CORES {
                let line = format!(
                    "host isolation: refused {inst}: the host would keep no whole core ({host})"
                );
                return (line.clone(), Err(anyhow::anyhow!(line)));
            }
            l.active.insert(
                inst.to_string(),
                Entry {
                    guest: guest.to_string(),
                    ..Default::default()
                },
            );
            enforce_system(units, r, l, &host, &mut ch);
            (
                format!(
                    "host isolation applied for uid {uid}: VM CPUs {guest}, host on {host}: {}",
                    ch.describe()
                ),
                Ok(()),
            )
        }
        "revert" => {
            l.active.remove(inst);
            if l.active.is_empty() {
                apply::restore_units(units, l, &mut ch);
                apply::restore_system(units, r, l, &mut ch);
                (
                    format!(
                        "host isolation reverted for uid {uid} (CPUs {guest}): everything back as it was: {}",
                        ch.describe()
                    ),
                    Ok(()),
                )
            } else {
                let host = topo.online.minus(&l.guests(None));
                enforce_system(units, r, l, &host, &mut ch);
                (
                    format!(
                        "host isolation reverted for uid {uid} (CPUs {guest}); other VMs still isolated, host on {host}: {}",
                        ch.describe()
                    ),
                    Ok(()),
                )
            }
        }
        other => (
            format!("host isolation: unknown action {other:?}"),
            Err(anyhow::anyhow!(
                "unknown action {other:?} (apply or revert)"
            )),
        ),
    }
}

fn enforce_system(units: &dyn Units, r: &Roots, l: &mut Ledger, host: &CpuSet, ch: &mut Changes) {
    let protected = apply::protected_cgroups(r);
    let w = apply::system_walk(r, &protected);
    apply::restrict_units(units, l, &w, host, ch);
    apply::restrict_system(units, r, l, host, ch);
}

// ---------------------------------------------------------------- user side

fn ledger_file() -> PathBuf {
    paths::xdg_runtime().join("conduit/isolation.json")
}

fn ledger_lock() -> Result<sys::Lock> {
    let f = paths::xdg_runtime().join("conduit/isolation.lock");
    std::fs::create_dir_all(f.parent().unwrap())?;
    sys::lock(&f, Duration::from_secs(60))
}

/// The root helper's ledger, read-only.
fn system_ledger() -> Ledger {
    Ledger::load(&Path::new(STATE_DIR).join("state.json"))
}

/// Is the system part installed (the unit file the packages ship)?
pub fn system_part_installed() -> bool {
    UNIT_FILES.iter().any(|f| Path::new(f).is_file())
}

fn virsh(uri: &str, args: &[&str]) -> Result<String> {
    let mut a = vec!["-q", "-c", uri];
    a.extend_from_slice(args);
    sys::output_within("virsh", &a, VIRSH_LIMIT)
}

fn domain_up(link: &Link) -> bool {
    virsh(&link.uri, &["domstate", &link.domain])
        .map(|s| virt::state_is_up(s.trim()))
        .unwrap_or(false)
}

/// The VM's setting.
pub fn setting(name: &str) -> (Setting, String) {
    let (v, src) = config::host_isolation(name);
    (Setting::parse(&v).unwrap_or(Setting::Auto), src)
}

/// CPUs other running VMs hold, from both ledgers: not `name`'s.
fn taken(user: &Ledger, name: &str) -> CpuSet {
    let mine = user.active.get(name).map(|e| e.instance.clone());
    let sys = system_ledger();
    let theirs = sys
        .active
        .iter()
        .filter(|(k, _)| Some(*k) != mine.as_ref())
        .filter_map(|(_, e)| CpuSet::parse(&e.guest))
        .fold(CpuSet::new(), |a, c| a.union(&c));
    user.guests(Some(name)).union(&theirs)
}

/// The pinning `conduit attach` and `conduit libvirt enable` write into a
/// definition so it agrees with host isolation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repin {
    /// vCPU i -> host CPU.
    pub vcpus: Vec<u32>,
    /// QEMU's own threads (`<emulatorpin>`, `<iothreadpin>`).
    pub host: CpuSet,
    /// What attach says about it.
    pub note: String,
}

/// For a definition with these CPU facts: the pinning to write (`None` when
/// its own agrees, or isolation is off), and a note when isolation cannot
/// apply to it at all. Planned as if no other VM ran: a definition outlives
/// one run.
pub fn definition_pinning(name: &str, d: &domain::DomainCpus) -> (Option<Repin>, Option<String>) {
    let (s, _) = setting(name);
    let Some(t) = Topology::host() else {
        return (None, None);
    };
    repin_of(&t, &s, d)
}

pub fn repin_of(
    t: &Topology,
    s: &Setting,
    d: &domain::DomainCpus,
) -> (Option<Repin>, Option<String>) {
    match policy::plan(t, s, &d.vm(), &CpuSet::new()) {
        Plan::Off => (None, None),
        Plan::Refused(why) => (None, Some(format!("Host isolation cannot apply to this VM: {why}."))),
        Plan::Isolate(i) => match &i.pins {
            Pins::Agrees => (None, None),
            Pins::None => (
                Some(Repin {
                    note: format!(
                        "vCPUs pinned 1:1 to {}, QEMU's own threads to {} (host isolation; `conduit config set host.isolation off` turns it off).",
                        where_(&i.label, &i.guest),
                        i.host
                    ),
                    vcpus: i.pinning.clone(),
                    host: i.host.clone(),
                }),
                None,
            ),
            Pins::Disagrees(why) => match &i.proposal {
                Some((p, cpus)) => {
                    let host = t.online.minus(cpus);
                    let label = t.labels_of(cpus);
                    let label = if t.split() == topo::Split::Monolithic {
                        String::new()
                    } else {
                        label.join("+")
                    };
                    (
                        Some(Repin {
                            note: format!(
                                "Re-pinned the vCPUs for host isolation ({why}): now 1:1 to {}, QEMU's own threads to {host}.",
                                where_(&label, cpus)
                            ),
                            vcpus: p.clone(),
                            host,
                        }),
                        None,
                    )
                }
                None => (None, Some(format!("Host isolation: {why}; no re-pin fits."))),
            },
        },
    }
}

fn where_(label: &str, cpus: &CpuSet) -> String {
    if label.is_empty() {
        cpus.to_string()
    } else {
        format!("{label} ({cpus})")
    }
}

/// Called by `conduit _backend` before it becomes the backend: start the
/// isolation in the background (it waits for the domain to run), output to
/// the backend's log.
pub fn spawn_start(name: &str, log: &Path) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let mut c = std::process::Command::new(me);
    c.args(["_isolation", "start", name]);
    if let Err(e) = sys::spawn_detached(&mut c, log, true) {
        eprintln!("conduit: host isolation not started: {e:#}");
    }
}

/// `conduit _isolation start NAME`: apply the VM's split once it runs.
pub fn start(name: &str) -> Result<()> {
    let Some(link) = Link::load(name) else {
        return Ok(());
    };
    let (s, src) = setting(name);
    if s == Setting::Off {
        stop(name);
        eprintln!("conduit: host isolation: off ({src})");
        return Ok(());
    }
    if !sys::wait_for(RUN_WAIT, || {
        virsh(&link.uri, &["domstate", &link.domain]).is_ok_and(|s| s.trim() == "running")
    }) {
        eprintln!(
            "conduit: host isolation: {name} did not reach \"running\" within {} s; not applied",
            RUN_WAIT.as_secs()
        );
        return Ok(());
    }
    let xml = virsh(&link.uri, &["dumpxml", &link.domain])?;
    let topo = Topology::host().context("cannot read the CPU topology")?;
    let d = domain::read(&xml).context("the domain names no vCPU count")?;
    let _lock = ledger_lock()?;
    let file = ledger_file();
    let mut l = Ledger::load(&file);
    reconcile(&mut l);
    // Still running (it may have stopped while this waited for the lock)?
    if !domain_up(&link) {
        l.save(&file)?;
        return Ok(());
    }
    let p = policy::plan(&topo, &s, &d.vm(), &taken(&l, name));
    let i = match p {
        Plan::Off => return Ok(()),
        Plan::Refused(why) => {
            eprintln!("conduit: host isolation: not applied for {name}: {why}");
            l.active.insert(
                name.into(),
                Entry {
                    summary: format!("not applied: {why}"),
                    ..Default::default()
                },
            );
            return l.save(&file);
        }
        Plan::Isolate(i) => i,
    };
    // A domain without pinning: pin it now, for this run (libvirt sets the
    // threads' affinity; the definition is not changed).
    if i.pins == Pins::None {
        for (n, cpu) in i.pinning.iter().enumerate() {
            let _ = virsh(
                &link.uri,
                &[
                    "vcpupin",
                    &link.domain,
                    &n.to_string(),
                    &cpu.to_string(),
                    "--live",
                ],
            );
        }
        if d.emulatorpin.is_none() {
            let _ = virsh(
                &link.uri,
                &["emulatorpin", &link.domain, &i.host.to_string(), "--live"],
            );
        }
    }
    let inst = instance(paths::uid(), &i.guest);
    // A previous run of this VM on other CPUs (a start repeated after a
    // change of setting): its system part goes first.
    if let Some(old) = l
        .active
        .get(name)
        .filter(|e| !e.instance.is_empty() && e.instance != inst)
    {
        let _ = (Systemctl { user: false }).stop(&unit_of(&old.instance));
    }
    let system_note = if !system_part_installed() {
        format!("the system part is not installed ({UNIT_TEMPLATE}); only your own processes were moved")
    } else {
        match (Systemctl { user: false }).start(&unit_of(&inst)) {
            Ok(()) => String::new(),
            Err(e) => format!("{} did not start: {e:#}", unit_of(&inst)),
        }
    };
    let mut ch = Changes::default();
    let all_guests = taken(&l, name).union(&i.guest);
    let host = topo.online.minus(&all_guests);
    let r = Roots::host();
    let w = apply::user_walk(&r, paths::uid(), &apply::protected_cgroups(&r));
    apply::restrict_units(&Systemctl { user: true }, &mut l, &w, &host, &mut ch);
    let summary = format!(
        "{} → {}{}",
        if src == "default" {
            "auto".to_string()
        } else {
            s.to_string()
        },
        i.describe(),
        if i.pins == Pins::None {
            "; vCPUs pinned for this run"
        } else {
            ""
        }
    );
    eprintln!(
        "conduit: host isolation: {summary}; applied (yours: {}){}",
        ch.describe(),
        if system_note.is_empty() {
            format!("; system part: {}", unit_of(&inst))
        } else {
            format!("; {system_note}")
        }
    );
    l.active.insert(
        name.into(),
        Entry {
            guest: i.guest.to_string(),
            instance: if system_note.is_empty() {
                inst
            } else {
                String::new()
            },
            summary,
            system_note,
        },
    );
    l.save(&file)
}

/// Revert a VM's isolation (idempotent): `_stopped`, `conduit down`.
pub fn stop(name: &str) {
    let Ok(_lock) = ledger_lock() else {
        return;
    };
    let file = ledger_file();
    let mut l = Ledger::load(&file);
    let Some(e) = l.active.remove(name) else {
        return;
    };
    if let Some(line) = stop_entry(&mut l, name, &e) {
        eprintln!("conduit: {line}");
    }
    stop_strays(&l);
    if let Err(err) = l.save(&file) {
        eprintln!("conduit: host isolation: {err:#}");
    }
}

/// Undo one entry already taken out of the ledger; the log line.
fn stop_entry(l: &mut Ledger, name: &str, e: &Entry) -> Option<String> {
    if e.guest.is_empty() {
        return None;
    }
    let mut note = String::new();
    if !e.instance.is_empty() {
        if let Err(err) = (Systemctl { user: false }).stop(&unit_of(&e.instance)) {
            note = format!("; {} did not stop: {err:#}", unit_of(&e.instance));
        }
    }
    let mut ch = Changes::default();
    let units = Systemctl { user: true };
    if l.active.values().all(|x| x.guest.is_empty()) {
        apply::restore_units(&units, l, &mut ch);
    } else if let Some(t) = Topology::host() {
        let host = t.online.minus(&taken(l, name));
        let r = Roots::host();
        let w = apply::user_walk(&r, paths::uid(), &apply::protected_cgroups(&r));
        apply::restrict_units(&units, l, &w, &host, &mut ch);
    }
    Some(format!(
        "host isolation reverted for {name} (CPUs {}; yours: {}){note}",
        e.guest,
        ch.describe()
    ))
}

/// Revert what VMs that are no longer running left behind.
fn reconcile(l: &mut Ledger) {
    let gone: Vec<String> = l
        .active
        .keys()
        .filter(|n| !Link::load(n).is_some_and(|k| domain_up(&k)))
        .cloned()
        .collect();
    for n in gone {
        if let Some(e) = l.active.remove(&n) {
            if let Some(line) = stop_entry(l, &n, &e) {
                eprintln!("conduit: {line} (left from an earlier run)");
            }
        }
    }
    stop_strays(l);
}

/// Your root-helper instances that no ledger entry names (a start killed
/// half way): stopped.
fn stop_strays(l: &Ledger) {
    if !system_part_installed() {
        return;
    }
    let pattern = format!("conduit-isolation@{}_*", paths::uid());
    let Ok(out) = (Systemctl { user: false }).run(&[
        "list-units",
        "--plain",
        "--no-legend",
        "--state=active",
        &pattern,
    ]) else {
        return;
    };
    let known: Vec<String> = l.active.values().map(|e| unit_of(&e.instance)).collect();
    for unit in out.lines().filter_map(|x| x.split_whitespace().next()) {
        if unit.starts_with("conduit-isolation@")
            && !known.iter().any(|k| k == unit)
            && (Systemctl { user: false }).stop(unit).is_ok()
        {
            eprintln!("conduit: host isolation: stopped {unit}, left from an earlier run");
        }
    }
}

/// Your VMs' isolation now, by VM (the dashboard reads it each refresh).
pub fn entries() -> std::collections::BTreeMap<String, Entry> {
    Ledger::load(&ledger_file()).active
}

/// Is a root-helper instance active?
pub fn system_active(inst: &str) -> bool {
    !inst.is_empty() && (Systemctl { user: false }).is_active(&unit_of(inst))
}

/// The dashboard's short form: `CCD1 8-15,24-31`, `8-15,24-31`, or why not.
pub fn short(e: &Entry) -> String {
    if e.guest.is_empty() {
        return "not applied".into();
    }
    let at = e
        .summary
        .split("VM on ")
        .nth(1)
        .and_then(|s| s.split(", host").next())
        .unwrap_or(&e.guest)
        .replace(" (", " ")
        .replace(')', "");
    if e.system_note.is_empty() {
        at
    } else {
        format!("{at} (partial)")
    }
}

/// Device IRQs whose effective affinity reaches `cpus`, by name. Left out:
/// the ones the root helper found it cannot move (kernel-managed queue
/// interrupts, spread over all CPUs by their driver, such as NVMe and
/// multi-queue NIC queues; they fire on a CPU only for work that CPU
/// submitted), and per-queue NVMe ones when it has not run.
pub fn irqs_on(cpus: &CpuSet) -> Vec<String> {
    let unmovable = system_ledger().unmovable;
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc/irq") else {
        return v;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.bytes().all(|b| b.is_ascii_digit())
            || n.parse::<u32>().is_ok_and(|x| unmovable.contains(&x))
        {
            continue;
        }
        let actions = std::fs::read_to_string(format!("/sys/kernel/irq/{n}/actions"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if actions.is_empty() || is_managed_queue(&actions) {
            continue;
        }
        let eff = std::fs::read_to_string(e.path().join("effective_affinity_list"))
            .ok()
            .and_then(|s| CpuSet::parse(&s));
        if eff.is_some_and(|c| c.overlaps(cpus)) && !v.contains(&actions) {
            v.push(actions);
        }
    }
    v.sort();
    v
}

/// `nvme0q3`: a per-CPU NVMe queue (`nvme0q0` is the admin queue).
fn is_managed_queue(actions: &str) -> bool {
    actions.split(',').all(|a| {
        a.strip_prefix("nvme")
            .and_then(|r| r.split_once('q'))
            .is_some_and(|(d, q)| {
                d.bytes().all(|b| b.is_ascii_digit()) && q.parse::<u32>().is_ok_and(|q| q > 0)
            })
    })
}

/// A cgroup's effective CPUs (`system.slice`), when readable.
pub fn effective_cpus(cgroup: &str) -> Option<CpuSet> {
    std::fs::read_to_string(format!("/sys/fs/cgroup/{cgroup}/cpuset.cpus.effective"))
        .ok()
        .and_then(|s| CpuSet::parse(&s))
}

/// One `conduit doctor NAME` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub warn: bool,
    pub title: String,
    pub detail: String,
    pub fix: String,
}

fn ok(title: &str, detail: String) -> Finding {
    Finding {
        warn: false,
        title: title.into(),
        detail,
        fix: String::new(),
    }
}

fn warn(title: &str, detail: String, fix: String) -> Finding {
    Finding {
        warn: true,
        title: title.into(),
        detail,
        fix,
    }
}

/// `conduit doctor NAME`'s host isolation lines. `xml`: the persistent
/// definition; `link`: to read the live one while it runs; `repair`: the
/// command that rewrites the definition (`conduit attach NAME`).
pub fn findings(name: &str, xml: &str, link: &Link, running: bool, repair: &str) -> Vec<Finding> {
    let mut v = Vec::new();
    const T: &str = "Host isolation";
    let (s, src) = setting(name);
    let shown = match src.as_str() {
        "default" => "auto".to_string(),
        "host.isolation" => s.to_string(),
        other => format!("{s} ({other})"),
    };
    let Some(t) = Topology::host() else {
        v.push(warn(
            T,
            "cannot read the CPU topology from /sys".into(),
            String::new(),
        ));
        return v;
    };
    let back_on = if src.starts_with("host.isolation.") {
        format!("conduit config unset {src}")
    } else {
        "conduit config set host.isolation auto".into()
    };
    if s == Setting::Off {
        let (split, doms) = t.domains();
        if split != topo::Split::Monolithic {
            let what = match split {
                topo::Split::Cache => format!(
                    "{} L3 domains ({})",
                    doms.len(),
                    doms.iter()
                        .map(|d| d.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                topo::Split::Hybrid => "P- and E-cores".into(),
                _ => format!("{} NUMA nodes", doms.len()),
            };
            v.push(warn(
                T,
                format!("{shown} on a CPU with {what}: host work and device interrupts can run on the VM's CPUs"),
                back_on,
            ));
        } else {
            v.push(ok(T, shown));
        }
        return v;
    }
    let Some(d) = domain::read(xml) else {
        v.push(warn(
            T,
            "the domain names no vCPU count".into(),
            String::new(),
        ));
        return v;
    };
    let user = Ledger::load(&ledger_file());
    let plan = policy::plan(&t, &s, &d.vm(), &taken(&user, name));
    let i = match plan {
        Plan::Off => return v,
        Plan::Refused(why) => {
            v.push(warn(
                T,
                format!("{shown}: cannot apply: {why}"),
                format!("Give the VM fewer vCPUs, or name its CPUs: conduit config set host.isolation.{name} LIST (or `conduit config set host.isolation.{name} off`)"),
            ));
            return v;
        }
        Plan::Isolate(i) => i,
    };
    let entry = user.active.get(name).cloned();
    let installed = system_part_installed();
    if running {
        match &entry {
            Some(e) if !e.guest.is_empty() && e.system_note.is_empty() && system_active(&e.instance) => {
                v.push(ok(T, format!("{}; applied ✓", e.summary)));
            }
            Some(e) if !e.guest.is_empty() => v.push(warn(
                T,
                format!(
                    "{}; applied to your own processes only: {}",
                    e.summary,
                    if e.system_note.is_empty() {
                        format!("{} is not active", unit_of(&e.instance))
                    } else {
                        e.system_note.clone()
                    }
                ),
                format!("Restart the VM; if it stays, see `systemctl status {}` and `conduit logs {name} backend`", if e.instance.is_empty() { "conduit-isolation@*".to_string() } else { unit_of(&e.instance) }),
            )),
            Some(e) => v.push(warn(
                T,
                format!("{shown} → {}; {}", i.describe(), e.summary),
                "See `conduit logs NAME backend`".replace("NAME", name),
            )),
            None => v.push(warn(
                T,
                format!("{shown} → {}; not applied to this run", i.describe()),
                format!("It applies at each start: restart the VM (`conduit down {name}`, then start it). If it still is not applied, see `conduit logs {name} backend`"),
            )),
        }
    } else {
        v.push(ok(
            T,
            format!("{shown} → {}; applies when the VM starts", i.describe()),
        ));
    }
    if !installed {
        v.push(warn(
            "Host isolation: system part",
            format!("{UNIT_TEMPLATE} is not installed: device interrupts, system services and your login session are not moved off the VM's CPUs"),
            "Install Conduit from its package (it ships the unit and its polkit rule)".into(),
        ));
    }
    match &i.pins {
        Pins::Agrees => v.push(ok("vCPU pinning", "1:1 on the VM's cores, as host isolation wants".into())),
        Pins::None => v.push(ok(
            "vCPU pinning",
            format!("none in the definition: pinned 1:1 at each start; `{repair}` writes it into the definition"),
        )),
        Pins::Disagrees(why) => v.push(warn(
            "vCPU pinning",
            format!(
                "{why}{}",
                i.proposal
                    .as_ref()
                    .map(|(_, c)| format!("; proposed: 1:1 on {}", where_(&t.labels_of(c).join("+"), c)))
                    .unwrap_or_default()
            ),
            format!("Re-pin (backs the definition up; applies at the next start): {repair}"),
        )),
    }
    if let Some(b) = config::backend_cpus().and_then(|c| CpuSet::parse(&c)) {
        if b.overlaps(&i.guest) {
            v.push(warn(
                "backend.cpus",
                format!("{b} overlaps the VM's CPUs {}", i.guest),
                format!("conduit config set backend.cpus LIST with CPUs from {} (or unset it: the backend follows host isolation)", i.host),
            ));
        }
    }
    if running {
        // What really runs where: the live pinning, the cgroups, the IRQs.
        let live = virsh(&link.uri, &["dumpxml", &link.domain])
            .ok()
            .and_then(|x| domain::read(&x));
        let vcpu_cpus = live
            .map(|l| {
                l.pins
                    .iter()
                    .flatten()
                    .fold(CpuSet::new(), |a, c| a.union(c))
            })
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| i.guest.clone());
        let fix = if entry.as_ref().is_some_and(|e| !e.guest.is_empty()) {
            format!("Restart the VM to apply host isolation again; `conduit logs {name} backend` says what it did")
        } else {
            format!("Restart the VM: host isolation applies at each start ({back_on} if it is off)")
        };
        let mut shared = Vec::new();
        let mut cgs = vec!["system.slice".to_string()];
        let us = format!("user.slice/user-{}.slice", paths::uid());
        if let Ok(rd) = std::fs::read_dir(format!("/sys/fs/cgroup/{us}")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("session-") && n.ends_with(".scope") {
                    cgs.push(format!("{us}/{n}"));
                }
            }
        }
        for cg in cgs {
            if let Some(c) = effective_cpus(&cg).filter(|c| c.overlaps(&vcpu_cpus)) {
                shared.push(format!("{} ({c})", cg.rsplit('/').next().unwrap_or(&cg)));
            }
        }
        if !shared.is_empty() {
            v.push(warn(
                "Host work on the VM's CPUs",
                format!(
                    "{} may run on the VM's vCPU CPUs {vcpu_cpus}",
                    shared.join(", ")
                ),
                fix.clone(),
            ));
        }
        let irqs = irqs_on(&vcpu_cpus);
        if !irqs.is_empty() {
            v.push(warn(
                "Device interrupts on the VM's CPUs",
                format!(
                    "{} IRQ(s) can fire on {vcpu_cpus}: {}",
                    irqs.len(),
                    irqs.join(", ")
                ),
                fix,
            ));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::apply::tests::{host_tree, FakeUnits};
    use super::topo::fixture::r7950x;
    use super::*;

    fn set(s: &str) -> CpuSet {
        CpuSet::parse(s).unwrap()
    }

    #[test]
    fn instances_round_trip_and_are_strict() {
        let g = set("8-15,24-31");
        let i = instance(1000, &g);
        assert_eq!(i, "1000_8-15_24-31");
        assert_eq!(unit_of(&i), "conduit-isolation@1000_8-15_24-31.service");
        assert_eq!(parse_instance(&i), Some((1000, g)));
        for bad in [
            "",
            "1000",
            "1000_",
            "x_1-2",
            "1000_1-2;rm",
            "1000_8-15,24-31",
            "1000_../x",
        ] {
            assert!(parse_instance(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn helper_applies_shares_and_reverts_across_two_vms() {
        let tree = host_tree();
        let r = Roots::under(&tree.0);
        let cpu = r7950x();
        let topo = Topology::read(&cpu.0).unwrap();
        let u = FakeUnits::default();
        u.allowed
            .borrow_mut()
            .insert("session-4.scope".into(), "".into());
        let mut l = Ledger::default();
        let (line, res) = helper_with(&u, &r, &topo, &mut l, "apply", "1000_8-15_24-31");
        assert!(res.is_ok(), "{line}");
        assert!(line.contains("host on 0-7,16-23"), "{line}");
        assert_eq!(u.allowed.borrow()["system.slice"], "0-7,16-23");
        // Applying the same instance again is harmless.
        let (_, res) = helper_with(&u, &r, &topo, &mut l, "apply", "1000_8-15_24-31");
        assert!(res.is_ok());
        // An overlapping second VM is refused; a disjoint one shrinks the host.
        let (line, res) = helper_with(&u, &r, &topo, &mut l, "apply", "1000_15_31");
        assert!(
            res.is_err() && line.contains("another running VM"),
            "{line}"
        );
        let (_, res) = helper_with(&u, &r, &topo, &mut l, "apply", "1000_4-7_20-23");
        assert!(res.is_ok());
        assert_eq!(u.allowed.borrow()["session-4.scope"], "0-3,16-19");
        // The first stops: the host grows back, still isolated from the second.
        let (line, _) = helper_with(&u, &r, &topo, &mut l, "revert", "1000_8-15_24-31");
        assert!(line.contains("other VMs still isolated"), "{line}");
        assert_eq!(u.allowed.borrow()["session-4.scope"], "0-3,8-19,24-31");
        // The last stops: everything as it was.
        let (_, res) = helper_with(&u, &r, &topo, &mut l, "revert", "1000_4-7_20-23");
        assert!(res.is_ok());
        assert_eq!(u.allowed.borrow()["session-4.scope"], "");
        assert!(l.active.is_empty() && l.saved.is_empty());
        let wq =
            std::fs::read_to_string(tree.0.join("sys/devices/virtual/workqueue/cpumask")).unwrap();
        assert_eq!(wq.trim(), "ffffffff");
        // Reverting what is not there is fine.
        let (_, res) = helper_with(&u, &r, &topo, &mut l, "revert", "1000_4-7_20-23");
        assert!(res.is_ok());
    }

    #[test]
    fn helper_refuses_bad_requests() {
        let tree = host_tree();
        let r = Roots::under(&tree.0);
        let cpu = r7950x();
        let topo = Topology::read(&cpu.0).unwrap();
        let u = FakeUnits::default();
        let mut l = Ledger::default();
        for (inst, why) in [
            ("1000_0-31", "no whole core"),
            ("1000_30-40", "not online"),
            ("bogus", "refused instance"),
        ] {
            let (line, res) = helper_with(&u, &r, &topo, &mut l, "apply", inst);
            assert!(res.is_err() && line.contains(why), "{inst}: {line}");
        }
        assert!(l.active.is_empty());
        assert!(u.log.borrow().is_empty(), "nothing was touched");
    }

    #[test]
    fn short_forms_and_managed_queues() {
        let e = Entry {
            guest: "8-15,24-31".into(),
            summary: "auto → VM on CCD1 (8-15,24-31), host on 0-7,16-23".into(),
            ..Default::default()
        };
        assert_eq!(short(&e), "CCD1 8-15,24-31");
        let mono = Entry {
            guest: "2-7".into(),
            summary: "auto → VM on 2-7, host on 0-1".into(),
            system_note: "x".into(),
            ..Default::default()
        };
        assert_eq!(short(&mono), "2-7 (partial)");
        assert_eq!(short(&Entry::default()), "not applied");
        assert!(is_managed_queue("nvme0q3"));
        assert!(!is_managed_queue("nvme0q0"));
        assert!(!is_managed_queue("nvidia"));
        assert!(!is_managed_queue("xhci_hcd"));
    }
}
