//! Which host CPUs a VM gets and which stay the host's: pure functions of the
//! topology, the `host.isolation` setting, the VM's vCPU count and pinning,
//! and the CPUs other running VMs already hold.
//!
//! `auto`:
//! - a VM whose vCPUs are all pinned 1:1 keeps that pinning: the VM gets
//!   those cores (every SMT thread of them), the host the rest. When the
//!   pinning spans more domains (CCDs, nodes, P/E) than the VM needs, or
//!   splits cores, a re-pin is proposed for the next start;
//! - otherwise the VM is placed: in the best domain ([`Topology::domains`]:
//!   the CCD with the larger L3 or faster cores, the P-cores, the node without
//!   CPU 0), whole cores, spilling into the next domain only when it does not
//!   fit;
//! - the host always keeps at least [`host_min_cores`] whole cores.
//!
//! A CPU list: the VM gets exactly those CPUs (plus any it is pinned to
//! outside them, so no vCPU shares a CPU with host work).

use super::cpus::CpuSet;
use super::topo::{Split, Topology};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    Auto,
    Off,
    Cpus(CpuSet),
}

impl Setting {
    pub fn parse(s: &str) -> Option<Setting> {
        match s.trim() {
            "auto" => Some(Setting::Auto),
            "off" => Some(Setting::Off),
            v => CpuSet::parse(v)
                .filter(|c| !c.is_empty())
                .map(Setting::Cpus),
        }
    }
}

impl std::fmt::Display for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Setting::Auto => write!(f, "auto"),
            Setting::Off => write!(f, "off"),
            Setting::Cpus(c) => write!(f, "{c}"),
        }
    }
}

/// The fewest whole cores the host keeps under `auto`: two, or a quarter of
/// the cores, whichever is more (never all of them).
pub fn host_min_cores(t: &Topology) -> usize {
    let n = t.cores.len();
    2.max(n.div_ceil(4)).min(n.saturating_sub(1)).max(1)
}

/// The fewest whole cores the host keeps whatever the setting (an explicit
/// list may give a VM more than `auto` would). The root helper refuses
/// anything below it.
pub const HOST_FLOOR_CORES: usize = 1;

/// What the VM looks like to the policy.
#[derive(Clone, Debug, Default)]
pub struct Vm {
    pub vcpus: u32,
    /// The domain's `<vcpupin>` per vCPU (index = vCPU), `None` = unpinned.
    pub pins: Vec<Option<CpuSet>>,
}

impl Vm {
    pub fn pinned_any(&self) -> bool {
        self.pins.iter().any(Option::is_some)
    }

    /// Every vCPU pinned to exactly one CPU, no two to the same one.
    fn one_to_one(&self) -> Option<Vec<u32>> {
        if self.pins.len() != self.vcpus as usize {
            return None;
        }
        let v: Vec<u32> = self
            .pins
            .iter()
            .map(|p| p.as_ref().filter(|c| c.len() == 1).and_then(CpuSet::first))
            .collect::<Option<_>>()?;
        let distinct: CpuSet = v.iter().copied().collect();
        (distinct.len() == v.len()).then_some(v)
    }

    fn pinned_union(&self) -> CpuSet {
        self.pins
            .iter()
            .flatten()
            .fold(CpuSet::new(), |a, p| a.union(p))
    }
}

/// How the domain's own pinning relates to the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pins {
    /// No `<vcpupin>`: the start pins the vCPUs live ([`Isolation::pinning`]),
    /// and `conduit attach` can make it permanent.
    None,
    Agrees,
    /// Why not; [`Isolation::proposal`] is the re-pin.
    Disagrees(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Isolation {
    /// The VM's CPUs while it runs: no host work or device interrupt there.
    pub guest: CpuSet,
    /// Everything else that is online (minus other VMs' CPUs).
    pub host: CpuSet,
    /// Where the VM sits, for people (`CCD1`, `P-cores`, `node 1`); empty on
    /// a CPU with one domain.
    pub label: String,
    /// vCPU i -> host CPU: the domain's own pinning when it agrees, else the
    /// policy's.
    pub pinning: Vec<u32>,
    pub pins: Pins,
    /// The re-pin for the next start when the pinning disagrees (vCPU i ->
    /// CPU), with the CPUs it would give the VM.
    pub proposal: Option<(Vec<u32>, CpuSet)>,
}

impl Isolation {
    /// `VM on CCD1 (8-15,24-31), host on 0-7,16-23`.
    pub fn describe(&self) -> String {
        let at = if self.label.is_empty() {
            self.guest.to_string()
        } else {
            format!("{} ({})", self.label, self.guest)
        };
        format!("VM on {at}, host on {}", self.host)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    Off,
    Isolate(Isolation),
    /// It cannot be done; why, for people.
    Refused(String),
}

/// The plan for one VM. `taken`: the CPUs other running VMs hold.
pub fn plan(t: &Topology, setting: &Setting, vm: &Vm, taken: &CpuSet) -> Plan {
    let free = t.online.minus(taken);
    match setting {
        Setting::Off => Plan::Off,
        Setting::Cpus(list) => {
            let list = list.intersect(&t.online);
            if list.is_empty() {
                return Plan::Refused("none of host.isolation's CPUs is online".into());
            }
            let pinned = vm.pinned_union();
            let guest = list.union(&pinned);
            if guest.overlaps(taken) {
                return Plan::Refused(format!(
                    "CPUs {} are another running VM's",
                    guest.intersect(taken)
                ));
            }
            let host = free.minus(&guest);
            if t.cores_in(&host) < HOST_FLOOR_CORES {
                return Plan::Refused(format!("that leaves the host no whole core ({host})"));
            }
            let proposal = core_order(t, &list);
            let fits = proposal.len() >= vm.vcpus as usize;
            let proposal: Vec<u32> = proposal.into_iter().take(vm.vcpus as usize).collect();
            let pins = if !vm.pinned_any() {
                Pins::None
            } else if !pinned.is_subset(&list) {
                Pins::Disagrees(format!("vCPUs are pinned to {pinned}, outside {list}"))
            } else if vm.one_to_one().is_none() {
                Pins::Disagrees("vCPUs are not pinned one to one".into())
            } else {
                Pins::Agrees
            };
            let pinning = match (&pins, vm.one_to_one()) {
                (Pins::Agrees, Some(p)) => p,
                _ => proposal.clone(),
            };
            let proposal = matches!(pins, Pins::Disagrees(_))
                .then(|| fits.then(|| (proposal, list.clone())))
                .flatten();
            Plan::Isolate(Isolation {
                label: labels(t, &guest),
                guest,
                host,
                pinning,
                pins,
                proposal,
            })
        }
        Setting::Auto => {
            let placed = match place(t, vm.vcpus, taken) {
                Ok(p) => p,
                Err(e) => return Plan::Refused(e),
            };
            let Some(one) = vm.one_to_one() else {
                if vm.pinned_any() {
                    // Some vCPUs pinned, or several to a CPU: keep clear of
                    // all of them now, propose the placement for next time.
                    let guest = placed.1.union(&t.whole_cores(&vm.pinned_union()));
                    if guest.overlaps(taken) {
                        return Plan::Refused(format!(
                            "its pinning uses CPUs {} of another running VM",
                            guest.intersect(taken)
                        ));
                    }
                    let host = free.minus(&guest);
                    if t.cores_in(&host) < host_min_cores(t) {
                        return Plan::Refused(format!(
                            "its pinning ({}) leaves the host fewer than {} cores",
                            vm.pinned_union(),
                            host_min_cores(t)
                        ));
                    }
                    return Plan::Isolate(Isolation {
                        label: labels(t, &guest),
                        guest,
                        host,
                        pinning: placed.0.clone(),
                        pins: Pins::Disagrees(
                            if vm.pins.iter().all(Option::is_some) {
                                "vCPUs are not pinned one to one"
                            } else {
                                "not every vCPU is pinned"
                            }
                            .into(),
                        ),
                        proposal: Some(placed),
                    });
                }
                let guest = placed.1.clone();
                return Plan::Isolate(Isolation {
                    label: labels(t, &guest),
                    host: free.minus(&guest),
                    guest,
                    pinning: placed.0,
                    pins: Pins::None,
                    proposal: None,
                });
            };
            // Pinned 1:1: the VM gets those cores.
            let pinned: CpuSet = one.iter().copied().collect();
            let guest = t.whole_cores(&pinned);
            if guest.overlaps(taken) {
                return Plan::Refused(format!(
                    "its vCPUs are pinned to CPUs {} that another running VM holds",
                    guest.intersect(taken)
                ));
            }
            let host = free.minus(&guest);
            if t.cores_in(&host) < host_min_cores(t) {
                return Plan::Refused(format!(
                    "its vCPUs are pinned to {pinned}, which leaves the host fewer than {} whole cores",
                    host_min_cores(t)
                ));
            }
            let need = domains_touched(t, &placed.1).len();
            let have = domains_touched(t, &guest);
            let split = t.split() != Split::Monolithic;
            let pins = if split && have.len() > need {
                Pins::Disagrees(format!("vCPUs span {}", have.join(" and ")))
            } else if t.smt() && guest.len() > pinned.len() + pinned.len() / 2 {
                // More than half the VM's cores have a thread the VM does
                // not use: those threads idle (host work is kept off them).
                Pins::Disagrees(format!(
                    "vCPUs use {} of the {} threads of their cores",
                    pinned.len(),
                    guest.len()
                ))
            } else {
                Pins::Agrees
            };
            let proposal = matches!(pins, Pins::Disagrees(_)).then_some(placed);
            Plan::Isolate(Isolation {
                label: labels(t, &guest),
                guest,
                host,
                pinning: one,
                pins,
                proposal,
            })
        }
    }
}

/// Place `vcpus` on free whole cores, best domain first: (vCPU i -> CPU, the
/// cores' CPUs). The host keeps at least [`host_min_cores`].
pub fn place(t: &Topology, vcpus: u32, taken: &CpuSet) -> Result<(Vec<u32>, CpuSet), String> {
    let (_, doms) = t.domains();
    let mut cpus: Vec<u32> = Vec::new();
    let mut guest = CpuSet::new();
    'outer: for d in &doms {
        for c in &d.cores {
            if cpus.len() >= vcpus as usize {
                break 'outer;
            }
            let core = &t.cores[*c];
            if core.cpus.overlaps(taken) {
                continue;
            }
            cpus.extend(core.cpus.iter());
            guest = guest.union(&core.cpus);
        }
    }
    let host = t.online.minus(taken).minus(&guest);
    let min = host_min_cores(t);
    if cpus.len() < vcpus as usize || t.cores_in(&host) < min {
        let total = t.online.minus(taken);
        let most = total.len().saturating_sub(
            t.cores
                .iter()
                .filter(|c| c.cpus.is_subset(&total))
                .take(min)
                .map(|c| c.cpus.len())
                .sum::<usize>(),
        );
        return Err(format!(
            "{vcpus} vCPUs do not fit: the host keeps at least {min} whole cores, so a VM gets at most {most} CPUs here{}",
            if taken.is_empty() {
                String::new()
            } else {
                format!(" (other running VMs hold {taken})")
            }
        ));
    }
    cpus.truncate(vcpus as usize);
    Ok((cpus, guest))
}

/// A list's CPUs in core order (each core's threads together), for pinning.
fn core_order(t: &Topology, list: &CpuSet) -> Vec<u32> {
    let mut v = Vec::new();
    for c in &t.cores {
        v.extend(c.cpus.iter().filter(|x| list.contains(*x)));
    }
    v
}

fn domains_touched(t: &Topology, cpus: &CpuSet) -> Vec<String> {
    t.labels_of(cpus)
}

/// The label of the domains a VM's CPUs lie in; none on a CPU with one.
fn labels(t: &Topology, guest: &CpuSet) -> String {
    if t.split() == Split::Monolithic {
        return String::new();
    }
    domains_touched(t, guest).join("+")
}

#[cfg(test)]
mod tests {
    use super::super::topo::fixture::*;
    use super::*;

    fn set(s: &str) -> CpuSet {
        CpuSet::parse(s).unwrap()
    }

    fn topo(t: &Tree) -> Topology {
        Topology::read(&t.0).unwrap()
    }

    fn vm(vcpus: u32) -> Vm {
        Vm {
            vcpus,
            pins: Vec::new(),
        }
    }

    fn pinned(cpus: &[u32]) -> Vm {
        Vm {
            vcpus: cpus.len() as u32,
            pins: cpus
                .iter()
                .map(|c| Some([*c].into_iter().collect()))
                .collect(),
        }
    }

    fn iso(p: Plan) -> Isolation {
        match p {
            Plan::Isolate(i) => i,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_parse() {
        assert_eq!(Setting::parse("auto"), Some(Setting::Auto));
        assert_eq!(Setting::parse("off"), Some(Setting::Off));
        assert_eq!(
            Setting::parse("8-15,24-31"),
            Some(Setting::Cpus(set("8-15,24-31")))
        );
        assert_eq!(Setting::parse(""), None);
        assert_eq!(Setting::parse("fast"), None);
    }

    #[test]
    fn r7950x_vm_pinned_to_ccd1_keeps_it() {
        let t = r7950x();
        let t = topo(&t);
        // The reference host's pinning: vCPU 2k/2k+1 = core 8+k's threads.
        let pins: Vec<u32> = (8..16).flat_map(|c| [c, c + 16]).collect();
        let i = iso(plan(&t, &Setting::Auto, &pinned(&pins), &CpuSet::new()));
        assert_eq!(i.guest, set("8-15,24-31"));
        assert_eq!(i.host, set("0-7,16-23"));
        assert_eq!(i.label, "CCD1");
        assert_eq!(i.pins, Pins::Agrees);
        assert_eq!(i.pinning, pins);
        assert!(i.proposal.is_none());
        assert_eq!(i.describe(), "VM on CCD1 (8-15,24-31), host on 0-7,16-23");
    }

    #[test]
    fn r7950x_unpinned_vm_goes_to_the_faster_ccd() {
        let t = r7950x();
        let t = topo(&t);
        let i = iso(plan(&t, &Setting::Auto, &vm(16), &CpuSet::new()));
        assert_eq!(i.guest, set("8-15,24-31"));
        assert_eq!(i.pins, Pins::None);
        assert_eq!(&i.pinning[..4], &[8, 24, 9, 25]);
        // A small VM takes only the cores it needs.
        let i = iso(plan(&t, &Setting::Auto, &vm(4), &CpuSet::new()));
        assert_eq!(i.guest, set("8-9,24-25"));
        assert_eq!(i.host, set("0-7,10-23,26-31"));
    }

    #[test]
    fn r7950x_pinning_across_ccds_gets_a_proposal() {
        let t = r7950x();
        let t = topo(&t);
        let pins: Vec<u32> = (4..12).collect();
        let i = iso(plan(&t, &Setting::Auto, &pinned(&pins), &CpuSet::new()));
        // Kept clear of the pinned cores now...
        assert_eq!(i.guest, set("4-11,20-27"));
        assert!(matches!(&i.pins, Pins::Disagrees(w) if w.contains("CCD1") && w.contains("CCD0")));
        // ...and one CCD next time.
        let (p, cpus) = i.proposal.unwrap();
        assert_eq!(cpus, set("8-11,24-27"));
        assert_eq!(p, vec![8, 24, 9, 25, 10, 26, 11, 27]);
    }

    #[test]
    fn half_used_cores_are_flagged() {
        let t = r7950x();
        let t = topo(&t);
        // One thread per core on 8 cores of CCD1.
        let i = iso(plan(
            &t,
            &Setting::Auto,
            &pinned(&[8, 9, 10, 11, 12, 13, 14, 15]),
            &CpuSet::new(),
        ));
        assert_eq!(i.guest, set("8-15,24-31"));
        assert!(matches!(i.pins, Pins::Disagrees(_)));
    }

    #[test]
    fn a_second_vm_gets_disjoint_cpus() {
        let t = r7950x();
        let t = topo(&t);
        let first = set("8-15,24-31");
        let i = iso(plan(&t, &Setting::Auto, &vm(8), &first));
        assert_eq!(i.guest, set("4-7,20-23"), "CCD0 from the top");
        assert!(!i.guest.overlaps(&first));
        assert_eq!(i.host, set("0-3,16-19"));
        // A third that does not fit is refused.
        let both = first.union(&i.guest);
        assert!(matches!(
            plan(&t, &Setting::Auto, &vm(4), &both),
            Plan::Refused(w) if w.contains("whole cores")
        ));
        // Pinned onto another VM's CPUs: refused.
        assert!(matches!(
            plan(&t, &Setting::Auto, &pinned(&[8, 24]), &first),
            Plan::Refused(_)
        ));
    }

    #[test]
    fn r7800x3d_single_ccd_leaves_the_host_two_cores() {
        let t = r7800x3d();
        let t = topo(&t);
        assert_eq!(host_min_cores(&t), 2);
        let i = iso(plan(&t, &Setting::Auto, &vm(12), &CpuSet::new()));
        assert_eq!(i.guest, set("2-7,10-15"));
        assert_eq!(i.host, set("0-1,8-9"));
        assert_eq!(i.label, "");
        assert_eq!(i.describe(), "VM on 2-7,10-15, host on 0-1,8-9");
        assert!(matches!(
            plan(&t, &Setting::Auto, &vm(16), &CpuSet::new()),
            Plan::Refused(w) if w.contains("at most 12 CPUs")
        ));
    }

    #[test]
    fn r7950x3d_games_go_to_the_cache_ccd() {
        let t = r7950x3d();
        let t = topo(&t);
        let i = iso(plan(&t, &Setting::Auto, &vm(16), &CpuSet::new()));
        assert_eq!(i.label, "CCD0");
        assert_eq!(i.guest, set("0-7,16-23"));
        assert_eq!(i.host, set("8-15,24-31"));
    }

    #[test]
    fn mono8_without_smt() {
        let t = mono8();
        let t = topo(&t);
        let i = iso(plan(&t, &Setting::Auto, &vm(4), &CpuSet::new()));
        assert_eq!(i.guest, set("4-7"));
        assert_eq!(i.host, set("0-3"));
        assert!(matches!(
            plan(&t, &Setting::Auto, &vm(7), &CpuSet::new()),
            Plan::Refused(_)
        ));
    }

    #[test]
    fn i13900k_vm_on_p_cores_host_on_e_cores() {
        let t = i13900k();
        let t = topo(&t);
        let i = iso(plan(&t, &Setting::Auto, &vm(16), &CpuSet::new()));
        assert_eq!(i.label, "P-cores");
        assert_eq!(i.guest, set("0-15"));
        assert_eq!(i.host, set("16-31"));
        // Bigger than the P-cores: spills into E-cores, host keeps 6 cores.
        let i = iso(plan(&t, &Setting::Auto, &vm(20), &CpuSet::new()));
        assert_eq!(i.label, "P-cores+E-cores");
        assert_eq!(t.cores_in(&i.host), 12);
    }

    #[test]
    fn xeon_two_sockets_vm_on_node_1() {
        let t = xeon2s();
        let t = topo(&t);
        let i = iso(plan(&t, &Setting::Auto, &vm(20), &CpuSet::new()));
        assert_eq!(i.label, "node 1");
        assert_eq!(i.guest, set("10-19,30-39"));
        assert_eq!(i.host, set("0-9,20-29"));
    }

    #[test]
    fn explicit_lists_and_off() {
        let t = r7950x();
        let t = topo(&t);
        assert_eq!(plan(&t, &Setting::Off, &vm(4), &CpuSet::new()), Plan::Off);
        let list = Setting::Cpus(set("8-15,24-31"));
        let i = iso(plan(&t, &list, &vm(16), &CpuSet::new()));
        assert_eq!(i.host, set("0-7,16-23"));
        assert_eq!(&i.pinning[..2], &[8, 24]);
        assert_eq!(i.pins, Pins::None);
        // Pinned outside the list: kept clear of both, re-pin proposed.
        let i = iso(plan(&t, &list, &pinned(&[0, 1]), &CpuSet::new()));
        assert_eq!(i.guest, set("0-1,8-15,24-31"));
        assert!(matches!(i.pins, Pins::Disagrees(_)));
        assert_eq!(i.proposal.unwrap().0, vec![8, 24]);
        // Nothing left for the host.
        assert!(matches!(
            plan(&t, &Setting::Cpus(set("0-31")), &vm(4), &CpuSet::new()),
            Plan::Refused(_)
        ));
    }
}
