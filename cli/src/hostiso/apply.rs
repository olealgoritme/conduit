//! Applying and reverting a split: cgroup CPU sets through systemd
//! (`AllowedCPUs=`), device interrupt affinity, the unbound workqueue mask,
//! and irqbalance. Every value is saved before its first change, in a ledger
//! that also lists the active isolations, so a revert is exact, an apply or
//! revert can be repeated, and several VMs share one host set (the host gets
//! everything no running VM holds; the saved values come back when the last
//! one stops).
//!
//! Two ledgers use this: the root helper's (`/run/conduit-isolation`), which
//! covers the system manager's units, interrupts and workqueues, and each
//! user's (`$XDG_RUNTIME_DIR/conduit/isolation.json`), which covers the units
//! of the user's own systemd manager. Neither ever touches a cgroup holding a
//! VM's QEMU (or the libvirt daemon that starts them): those are walked into,
//! never restricted.

use super::cpus::CpuSet;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One active isolation in a ledger.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The VM's CPUs.
    pub guest: String,
    /// User ledger: the root helper's instance (`UID_8-15_24-31`), when it ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance: String,
    /// User ledger: what the start said, for `conduit doctor` and the dashboard.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// User ledger: why the system part did not apply, when it did not.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system_note: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Root: instance -> entry. User: VM name -> entry.
    pub active: BTreeMap<String, Entry>,
    /// Values from before the first change: `unit:NAME` (AllowedCPUs),
    /// `irq:N` (smp_affinity_list), `irq-default` (default_smp_affinity),
    /// `workqueue` (cpumask), `irqbalance` (`active` when it was stopped).
    pub saved: BTreeMap<String, String>,
    /// Root: IRQs that refused a new affinity (per-CPU or kernel-managed),
    /// so `conduit doctor` does not count them.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub unmovable: std::collections::BTreeSet<u32>,
}

impl Ledger {
    pub fn load(file: &Path) -> Ledger {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Written whole and renamed into place; removed when nothing is left.
    pub fn save(&self, file: &Path) -> Result<()> {
        if self.active.is_empty() && self.saved.is_empty() && self.unmovable.is_empty() {
            let _ = std::fs::remove_file(file);
            return Ok(());
        }
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, file).with_context(|| format!("saving {}", file.display()))
    }

    /// The union of every active isolation's CPUs, `except` one key.
    pub fn guests(&self, except: Option<&str>) -> CpuSet {
        self.active
            .iter()
            .filter(|(k, _)| Some(k.as_str()) != except)
            .filter_map(|(_, e)| CpuSet::parse(&e.guest))
            .fold(CpuSet::new(), |a, c| a.union(&c))
    }
}

/// A systemd manager's units: `AllowedCPUs=` and start/stop.
pub trait Units {
    fn allowed_cpus(&self, unit: &str) -> Result<String>;
    fn set_allowed_cpus(&self, unit: &str, cpus: &str) -> Result<()>;
    fn is_active(&self, unit: &str) -> bool;
    fn stop(&self, unit: &str) -> Result<()>;
    fn start(&self, unit: &str) -> Result<()>;
}

/// The places read and written, below a root (`/` on the host).
#[derive(Clone, Debug)]
pub struct Roots {
    pub proc: PathBuf,
    pub sys: PathBuf,
    pub cgroup: PathBuf,
}

impl Roots {
    pub fn under(root: &Path) -> Roots {
        Roots {
            proc: root.join("proc"),
            sys: root.join("sys"),
            cgroup: root.join("sys/fs/cgroup"),
        }
    }

    pub fn host() -> Roots {
        Roots::under(Path::new("/"))
    }
}

/// Processes whose cgroup must never be restricted: QEMU (its vCPU threads
/// are pinned to the VM's CPUs, and a cgroup CPU set would override that),
/// Conduit's built-in VM runner, and the libvirt daemons that start QEMU (a
/// VM started later is born in their cgroup).
pub fn is_protected_comm(comm: &str) -> bool {
    comm.starts_with("qemu-system")
        || comm.starts_with("qemu-kvm")
        || comm == "conduit-vmm"
        || comm == "virtqemud"
        || comm == "libvirtd"
}

/// The cgroups (`/user.slice/...`) of every protected process.
pub fn protected_cgroups(r: &Roots) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&r.proc) else {
        return v;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if !is_protected_comm(comm.trim()) {
            continue;
        }
        let cg = std::fs::read_to_string(e.path().join("cgroup")).unwrap_or_default();
        if let Some(p) = cg.lines().find_map(|l| l.strip_prefix("0::")) {
            let p = p.trim().to_string();
            if !v.contains(&p) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// What a walk found below a cgroup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// Units to restrict to the host's CPUs.
    pub restrict: Vec<String>,
    /// Units left alone because a VM's QEMU (or libvirt) runs inside.
    pub kept: Vec<String>,
}

/// Walk the units below `base` (a cgroup path relative to the cgroup root,
/// `""` for the root): a unit without protected processes is restricted as a
/// whole; a slice holding one is walked into; any other unit holding one is
/// left alone. `skip` names units never touched at all.
pub fn walk(r: &Roots, base: &str, protected: &[String], skip: &dyn Fn(&str) -> bool) -> Walk {
    let mut w = Walk::default();
    walk_into(r, base, protected, skip, &mut w);
    w
}

fn walk_into(
    r: &Roots,
    base: &str,
    protected: &[String],
    skip: &dyn Fn(&str) -> bool,
    w: &mut Walk,
) {
    let dir = r.cgroup.join(base.trim_start_matches('/'));
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".slice") || n.ends_with(".scope") || n.ends_with(".service"))
        .collect();
    names.sort();
    for name in names {
        if skip(&name) {
            continue;
        }
        let rel = format!("{}/{name}", base.trim_end_matches('/'));
        let holds = protected
            .iter()
            .any(|p| p == &rel || p.starts_with(&format!("{rel}/")));
        if !holds {
            w.restrict.push(name);
        } else if name.ends_with(".slice") {
            walk_into(r, &rel, protected, skip, w);
        } else {
            w.kept.push(name);
        }
    }
}

/// The system manager's walk: from the cgroup root, never init.scope (PID 1)
/// or machine.slice (the system libvirt's VMs).
pub fn system_walk(r: &Roots, protected: &[String]) -> Walk {
    walk(r, "", protected, &|n| {
        n == "init.scope" || n == "machine.slice"
    })
}

/// A user manager's walk: below `user@UID.service`, never its init.scope.
pub fn user_walk(r: &Roots, uid: u32, protected: &[String]) -> Walk {
    walk(
        r,
        &format!("/user.slice/user-{uid}.slice/user@{uid}.service"),
        protected,
        &|n| n == "init.scope",
    )
}

/// What an apply or revert changed, for its one log line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub units: Vec<String>,
    pub kept: Vec<String>,
    pub irqs: usize,
    pub workqueue: bool,
    pub irqbalance_stopped: bool,
}

impl Changes {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.units.is_empty() {
            parts.push(self.units.join(", "));
        }
        if self.irqs > 0 {
            parts.push(format!("{} IRQs", self.irqs));
        }
        if self.workqueue {
            parts.push("workqueues".into());
        }
        if self.irqbalance_stopped {
            parts.push("irqbalance stopped".into());
        }
        let mut s = if parts.is_empty() {
            "nothing to change".to_string()
        } else {
            parts.join("; ")
        };
        if !self.kept.is_empty() {
            s += &format!(" (left alone, QEMU inside: {})", self.kept.join(", "));
        }
        s
    }
}

/// Restrict `walk.restrict` to `host`, saving each unit's value first.
pub fn restrict_units(
    units: &dyn Units,
    l: &mut Ledger,
    w: &Walk,
    host: &CpuSet,
    ch: &mut Changes,
) {
    let h = host.to_string();
    for u in &w.restrict {
        let key = format!("unit:{u}");
        if !l.saved.contains_key(&key) {
            match units.allowed_cpus(u) {
                Ok(v) => {
                    l.saved.insert(key.clone(), v);
                }
                Err(_) => continue,
            }
        }
        if units.set_allowed_cpus(u, &h).is_ok() {
            ch.units.push(u.clone());
        }
    }
    // Units restricted by an earlier apply that the walk no longer lists
    // (gone, or a VM now runs inside) get their value back.
    let listed: Vec<String> = w.restrict.iter().map(|u| format!("unit:{u}")).collect();
    let stale: Vec<String> = l
        .saved
        .keys()
        .filter(|k| k.starts_with("unit:") && !listed.contains(k))
        .cloned()
        .collect();
    for k in stale {
        let v = l.saved.remove(&k).unwrap_or_default();
        let _ = units.set_allowed_cpus(&k["unit:".len()..], &v);
    }
    ch.kept = w.kept.clone();
}

/// Put every saved unit value back and forget them.
pub fn restore_units(units: &dyn Units, l: &mut Ledger, ch: &mut Changes) {
    let keys: Vec<String> = l
        .saved
        .keys()
        .filter(|k| k.starts_with("unit:"))
        .cloned()
        .collect();
    for k in keys {
        let v = l.saved.remove(&k).unwrap_or_default();
        let u = &k["unit:".len()..];
        if units.set_allowed_cpus(u, &v).is_ok() {
            ch.units.push(u.to_string());
        }
    }
}

/// Device IRQs: every `/proc/irq/N` with a handler. Per-CPU and
/// kernel-managed ones (NVMe queues) refuse a new affinity; they are left
/// as they are and not recorded.
fn irqs(r: &Roots) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(r.proc.join("irq"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
        .filter(|n: &u32| {
            // No handler: nothing to move. Older kernels lack the file.
            match std::fs::read_to_string(r.sys.join(format!("kernel/irq/{n}/actions"))) {
                Ok(a) => !a.trim().is_empty(),
                Err(_) => true,
            }
        })
        .collect();
    v.sort();
    v
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The root helper's part beyond units: interrupts, the workqueue mask and
/// irqbalance, all to `host`.
pub fn restrict_system(
    units: &dyn Units,
    r: &Roots,
    l: &mut Ledger,
    host: &CpuSet,
    ch: &mut Changes,
) {
    // irqbalance would move the interrupts back.
    if !l.saved.contains_key("irqbalance")
        && units.is_active("irqbalance.service")
        && units.stop("irqbalance.service").is_ok()
    {
        l.saved.insert("irqbalance".into(), "active".into());
        ch.irqbalance_stopped = true;
    }
    let list = host.to_string();
    for n in irqs(r) {
        let f = r.proc.join(format!("irq/{n}/smp_affinity_list"));
        let key = format!("irq:{n}");
        let fresh = !l.saved.contains_key(&key);
        if fresh {
            let Some(prev) = read_trim(&f) else { continue };
            if prev == list {
                continue;
            }
            l.saved.insert(key.clone(), prev);
        }
        if std::fs::write(&f, &list).is_ok() {
            ch.irqs += 1;
        } else {
            l.unmovable.insert(n);
            if fresh {
                l.saved.remove(&key);
            }
        }
    }
    let mask = host.mask();
    for (key, rel) in [
        ("irq-default", "irq/default_smp_affinity"),
        ("workqueue", "devices/virtual/workqueue/cpumask"),
    ] {
        let f = if key == "workqueue" {
            r.sys.join(rel)
        } else {
            r.proc.join(rel)
        };
        if !l.saved.contains_key(key) {
            let Some(prev) = read_trim(&f) else { continue };
            l.saved.insert(key.into(), prev);
        }
        if std::fs::write(&f, &mask).is_ok() && key == "workqueue" {
            ch.workqueue = true;
        }
    }
}

/// Put interrupts, the workqueue mask and irqbalance back as saved.
pub fn restore_system(units: &dyn Units, r: &Roots, l: &mut Ledger, ch: &mut Changes) {
    let keys: Vec<String> = l.saved.keys().cloned().collect();
    for k in keys {
        let v = l.saved[&k].clone();
        let f = if let Some(n) = k.strip_prefix("irq:") {
            r.proc.join(format!("irq/{n}/smp_affinity_list"))
        } else if k == "irq-default" {
            r.proc.join("irq/default_smp_affinity")
        } else if k == "workqueue" {
            r.sys.join("devices/virtual/workqueue/cpumask")
        } else {
            continue;
        };
        // A device that went away: nothing to restore.
        if std::fs::write(&f, &v).is_ok() {
            if k.starts_with("irq:") {
                ch.irqs += 1;
            } else if k == "workqueue" {
                ch.workqueue = true;
            }
        }
        l.saved.remove(&k);
    }
    if l.saved.remove("irqbalance").is_some() {
        let _ = units.start("irqbalance.service");
    }
    l.unmovable.clear();
}

#[cfg(test)]
pub mod tests {
    use super::super::topo::fixture::{put, scratch};
    use super::*;
    use std::cell::RefCell;

    /// A systemd stand-in: AllowedCPUs per unit, active units, a call log.
    #[derive(Default)]
    pub struct FakeUnits {
        pub allowed: RefCell<BTreeMap<String, String>>,
        pub active: RefCell<Vec<String>>,
        pub log: RefCell<Vec<String>>,
    }

    impl Units for FakeUnits {
        fn allowed_cpus(&self, unit: &str) -> Result<String> {
            Ok(self.allowed.borrow().get(unit).cloned().unwrap_or_default())
        }
        fn set_allowed_cpus(&self, unit: &str, cpus: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("set {unit}={cpus}"));
            self.allowed.borrow_mut().insert(unit.into(), cpus.into());
            Ok(())
        }
        fn is_active(&self, unit: &str) -> bool {
            self.active.borrow().iter().any(|u| u == unit)
        }
        fn stop(&self, unit: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("stop {unit}"));
            self.active.borrow_mut().retain(|u| u != unit);
            Ok(())
        }
        fn start(&self, unit: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("start {unit}"));
            self.active.borrow_mut().push(unit.into());
            Ok(())
        }
    }

    /// A host like the reference one: Hyprland's session scope, system.slice,
    /// QEMU in a terminal's scope inside the user manager, conduit's slices,
    /// three IRQs (one per-CPU one that refuses a new affinity).
    pub fn host_tree() -> super::super::topo::fixture::Tree {
        let t = scratch("apply");
        let r = &t.0;
        let cg = "sys/fs/cgroup";
        for d in [
            "init.scope",
            "system.slice/irqbalance.service",
            "system.slice/libvirtd.service",
            "machine.slice",
            "user.slice/user-1000.slice/session-4.scope",
            "user.slice/user-1000.slice/user@1000.service/init.scope",
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-foot.scope",
            "user.slice/user-1000.slice/user@1000.service/conduit.slice/conduit-win11.slice/conduit-backend@win11.service",
            "user.slice/user-1000.slice/user@1000.service/session.slice",
            "user.slice/user-1000.slice/user@1000.service/tmux-spawn-1.scope",
            "user.slice/user-1000.slice/user@1000.service/tmux-spawn-2.scope",
        ] {
            std::fs::create_dir_all(r.join(cg).join(d)).unwrap();
        }
        // QEMU and the session libvirt daemon in tmux-spawn-1.scope.
        for (pid, comm) in [
            ("100", "qemu-system-x86"),
            ("101", "virtqemud"),
            ("200", "Hyprland"),
        ] {
            put(r, &format!("proc/{pid}/comm"), comm);
        }
        put(
            r,
            "proc/100/cgroup",
            "0::/user.slice/user-1000.slice/user@1000.service/tmux-spawn-1.scope",
        );
        put(
            r,
            "proc/101/cgroup",
            "0::/user.slice/user-1000.slice/user@1000.service/tmux-spawn-1.scope",
        );
        put(
            r,
            "proc/200/cgroup",
            "0::/user.slice/user-1000.slice/session-4.scope",
        );
        for (n, aff, actions) in [
            ("24", "0-31", "nvidia"),
            ("30", "0-31", "xhci_hcd"),
            ("40", "3", ""),
        ] {
            put(r, &format!("proc/irq/{n}/smp_affinity_list"), aff);
            put(r, &format!("sys/kernel/irq/{n}/actions"), actions);
        }
        put(r, "proc/irq/default_smp_affinity", "ffffffff");
        put(r, "sys/devices/virtual/workqueue/cpumask", "ffffffff");
        t
    }

    fn set(s: &str) -> CpuSet {
        CpuSet::parse(s).unwrap()
    }

    #[test]
    fn the_walk_never_restricts_qemu_or_libvirt() {
        let t = host_tree();
        let r = Roots::under(&t.0);
        let p = protected_cgroups(&r);
        assert_eq!(
            p,
            vec!["/user.slice/user-1000.slice/user@1000.service/tmux-spawn-1.scope"]
        );
        let sys = system_walk(&r, &p);
        assert_eq!(sys.restrict, vec!["system.slice", "session-4.scope"]);
        assert_eq!(sys.kept, vec!["user@1000.service"]);
        let user = user_walk(&r, 1000, &p);
        assert_eq!(
            user.restrict,
            vec![
                "app.slice",
                "conduit.slice",
                "session.slice",
                "tmux-spawn-2.scope"
            ]
        );
        assert_eq!(user.kept, vec!["tmux-spawn-1.scope"]);
    }

    #[test]
    fn a_qemu_inside_a_slice_is_walked_into() {
        let t = host_tree();
        let r = Roots::under(&t.0);
        // The built-in runner's layout: QEMU in its own scope in the VM's slice.
        std::fs::create_dir_all(r.cgroup.join(
            "user.slice/user-1000.slice/user@1000.service/conduit.slice/conduit-win11.slice/run-1.scope",
        ))
        .unwrap();
        let p = vec![
            "/user.slice/user-1000.slice/user@1000.service/conduit.slice/conduit-win11.slice/run-1.scope"
                .to_string(),
        ];
        let w = user_walk(&r, 1000, &p);
        assert!(w
            .restrict
            .contains(&"conduit-backend@win11.service".to_string()));
        assert!(!w.restrict.contains(&"conduit.slice".to_string()));
        assert_eq!(w.kept, vec!["run-1.scope"]);
    }

    #[test]
    fn apply_and_revert_are_exact_and_repeatable() {
        let t = host_tree();
        let r = Roots::under(&t.0);
        let u = FakeUnits::default();
        u.allowed
            .borrow_mut()
            .insert("system.slice".into(), "".into());
        u.allowed
            .borrow_mut()
            .insert("session-4.scope".into(), "0-31".into());
        u.active.borrow_mut().push("irqbalance.service".into());
        let p = protected_cgroups(&r);
        let w = system_walk(&r, &p);
        let host = set("0-7,16-23");
        let mut l = Ledger::default();
        let mut ch = Changes::default();
        restrict_units(&u, &mut l, &w, &host, &mut ch);
        restrict_system(&u, &r, &mut l, &host, &mut ch);
        assert_eq!(ch.units, vec!["system.slice", "session-4.scope"]);
        assert_eq!(ch.irqs, 2, "IRQ 40 has no handler and is skipped");
        assert!(ch.irqbalance_stopped && ch.workqueue);
        let rd = |p: &str| {
            std::fs::read_to_string(t.0.join(p))
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(rd("proc/irq/24/smp_affinity_list"), "0-7,16-23");
        assert_eq!(rd("sys/devices/virtual/workqueue/cpumask"), "00ff00ff");
        assert_eq!(rd("proc/irq/default_smp_affinity"), "00ff00ff");
        assert_eq!(u.allowed.borrow()["system.slice"], "0-7,16-23");
        assert!(!u.is_active("irqbalance.service"));

        // Again (a second VM, or a repeated apply): the saved values stay the
        // ones from before the first change.
        let host2 = set("0-3,16-19");
        let mut ch2 = Changes::default();
        restrict_units(&u, &mut l, &w, &host2, &mut ch2);
        restrict_system(&u, &r, &mut l, &host2, &mut ch2);
        assert_eq!(l.saved["unit:session-4.scope"], "0-31");
        assert_eq!(l.saved["irq:24"], "0-31");
        assert_eq!(l.saved["workqueue"], "ffffffff");
        assert_eq!(rd("proc/irq/30/smp_affinity_list"), "0-3,16-19");

        let mut back = Changes::default();
        restore_units(&u, &mut l, &mut back);
        restore_system(&u, &r, &mut l, &mut back);
        assert!(l.saved.is_empty());
        assert_eq!(u.allowed.borrow()["system.slice"], "");
        assert_eq!(u.allowed.borrow()["session-4.scope"], "0-31");
        assert_eq!(rd("proc/irq/24/smp_affinity_list"), "0-31");
        assert_eq!(rd("proc/irq/default_smp_affinity"), "ffffffff");
        assert_eq!(rd("sys/devices/virtual/workqueue/cpumask"), "ffffffff");
        assert!(u.is_active("irqbalance.service"));
        // Reverting again changes nothing.
        let mut again = Changes::default();
        restore_units(&u, &mut l, &mut again);
        restore_system(&u, &r, &mut l, &mut again);
        assert_eq!(again, Changes::default());
    }

    #[test]
    fn irqs_that_refuse_an_affinity_are_noted_not_saved() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions. Root writes through 0444.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let t = host_tree();
        let r = Roots::under(&t.0);
        let f = t.0.join("proc/irq/30/smp_affinity_list");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
        let u = FakeUnits::default();
        let mut l = Ledger::default();
        let mut ch = Changes::default();
        restrict_system(&u, &r, &mut l, &set("0-7"), &mut ch);
        assert_eq!(ch.irqs, 1);
        assert!(l.unmovable.contains(&30));
        assert!(!l.saved.contains_key("irq:30"));
        let mut back = Changes::default();
        restore_system(&u, &r, &mut l, &mut back);
        assert!(l.unmovable.is_empty() && l.saved.is_empty());
    }

    #[test]
    fn units_dropped_from_the_walk_get_their_value_back() {
        let u = FakeUnits::default();
        let mut l = Ledger::default();
        let mut ch = Changes::default();
        let w = Walk {
            restrict: vec!["a.scope".into(), "b.scope".into()],
            kept: vec![],
        };
        restrict_units(&u, &mut l, &w, &set("0-3"), &mut ch);
        let w2 = Walk {
            restrict: vec!["a.scope".into()],
            kept: vec!["b.scope".into()],
        };
        restrict_units(&u, &mut l, &w2, &set("0-3"), &mut ch);
        assert_eq!(u.allowed.borrow()["b.scope"], "");
        assert!(!l.saved.contains_key("unit:b.scope"));
    }

    #[test]
    fn ledger_round_trip_and_union() {
        let t = scratch("ledger");
        let f = t.0.join("state.json");
        let mut l = Ledger::default();
        l.active.insert(
            "1000_8-15_24-31".into(),
            Entry {
                guest: "8-15,24-31".into(),
                ..Default::default()
            },
        );
        l.active.insert(
            "1000_4-7_20-23".into(),
            Entry {
                guest: "4-7,20-23".into(),
                ..Default::default()
            },
        );
        l.saved.insert("workqueue".into(), "ffffffff".into());
        l.save(&f).unwrap();
        let back = Ledger::load(&f);
        assert_eq!(back, l);
        assert_eq!(back.guests(None), set("4-15,20-31"));
        assert_eq!(back.guests(Some("1000_4-7_20-23")), set("8-15,24-31"));
        Ledger::default().save(&f).unwrap();
        assert!(!f.exists(), "an empty ledger is removed");
    }
}
