//! The host CPU topology, read from sysfs: physical cores and their SMT
//! threads, L3 domains (an AMD CCD or CCX; the larger one on an X3D part),
//! Intel hybrid P- and E-cores, the firmware's fastest-core ranking and NUMA
//! nodes. Everything is read below a root directory, so tests run on fixture
//! trees ([`fixture`]).

use super::cpus::CpuSet;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

/// Intel hybrid core type (`/sys/devices/cpu_core`, `cpu_atom`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKind {
    Performance,
    Efficient,
    /// Not a hybrid CPU.
    Uniform,
}

/// One physical core: its SMT threads and where it sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Core {
    pub cpus: CpuSet,
    /// Index into [`Topology::l3`], when the CPU reports an L3.
    pub l3: Option<usize>,
    pub node: u32,
    pub kind: CoreKind,
    /// The firmware's preference for this core: AMD's prefcore ranking, else
    /// ACPI CPPC highest_perf, else cpu_capacity. Higher is faster.
    pub rank: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L3 {
    pub cpus: CpuSet,
    pub size_kib: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    pub vendor: Vendor,
    pub online: CpuSet,
    /// Ordered by their lowest CPU number.
    pub cores: Vec<Core>,
    /// Ordered by their lowest CPU number.
    pub l3: Vec<L3>,
    /// NUMA node number -> its online CPUs (nodes without CPUs left out).
    pub nodes: BTreeMap<u32, CpuSet>,
}

/// How the policy splits this CPU ([`Topology::domains`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    /// Several NUMA nodes (multi-socket): by node.
    Numa,
    /// Intel hybrid: P-cores and E-cores.
    Hybrid,
    /// Several L3 domains (AMD CCDs/CCXs): by L3.
    Cache,
    /// One L3, one node: by core count.
    Monolithic,
}

/// A group of cores the policy places a VM in, best first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// For people: `CCD1`, `P-cores`, `node 1`, `L3 #2`.
    pub label: String,
    /// Indices into [`Topology::cores`], in placement order.
    pub cores: Vec<usize>,
    pub cpus: CpuSet,
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_num(p: &Path) -> Option<u64> {
    read(p)?.parse().ok()
}

/// `32768K`, `96M`, `1024` (KiB) as KiB.
fn parse_size_kib(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('K') {
        n.parse().ok()
    } else if let Some(n) = s.strip_suffix('M') {
        n.parse::<u64>().ok().map(|n| n * 1024)
    } else {
        s.parse().ok()
    }
}

impl Topology {
    /// Read the topology below `root` (`/` on the host).
    pub fn read(root: &Path) -> Option<Topology> {
        let cpu_dir = root.join("sys/devices/system/cpu");
        let online = CpuSet::parse(&read(&cpu_dir.join("online"))?)?;
        if online.is_empty() {
            return None;
        }
        let vendor = match read(&root.join("proc/cpuinfo"))
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("vendor_id"))
            .and_then(|l| l.split(':').nth(1))
            .map(str::trim)
        {
            Some("AuthenticAMD") | Some("HygonGenuine") => Vendor::Amd,
            Some("GenuineIntel") => Vendor::Intel,
            _ => Vendor::Other,
        };
        let devices = root.join("sys/devices");
        let p_cores = read(&devices.join("cpu_core/cpus")).and_then(|s| CpuSet::parse(&s));
        let e_cores = read(&devices.join("cpu_atom/cpus")).and_then(|s| CpuSet::parse(&s));
        let hybrid =
            matches!((&p_cores, &e_cores), (Some(p), Some(e)) if !p.is_empty() && !e.is_empty());

        // NUMA nodes.
        let mut nodes: BTreeMap<u32, CpuSet> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(root.join("sys/devices/system/node")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(n) = name
                    .strip_prefix("node")
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                if let Some(c) = read(&e.path().join("cpulist")).and_then(|s| CpuSet::parse(&s)) {
                    let c = c.intersect(&online);
                    if !c.is_empty() {
                        nodes.insert(n, c);
                    }
                }
            }
        }
        if nodes.is_empty() {
            nodes.insert(0, online.clone());
        }
        let node_of = |cpu: u32| {
            nodes
                .iter()
                .find(|(_, c)| c.contains(cpu))
                .map_or(0, |(n, _)| *n)
        };

        // Cores (by their sibling list) and L3 domains (by shared_cpu_list).
        let mut l3: Vec<L3> = Vec::new();
        let mut cores: Vec<Core> = Vec::new();
        for cpu in online.iter() {
            let d = cpu_dir.join(format!("cpu{cpu}"));
            let siblings = read(&d.join("topology/thread_siblings_list"))
                .and_then(|s| CpuSet::parse(&s))
                .map(|s| s.intersect(&online))
                .filter(|s| s.contains(cpu))
                .unwrap_or_else(|| [cpu].into_iter().collect());
            let l3_idx = l3_of(&d).map(|(cpus, size_kib)| {
                let cpus = cpus.intersect(&online);
                match l3.iter().position(|x| x.cpus == cpus) {
                    Some(i) => i,
                    None => {
                        l3.push(L3 { cpus, size_kib });
                        l3.len() - 1
                    }
                }
            });
            let rank = read_num(&d.join("cpufreq/amd_pstate_prefcore_ranking"))
                .or_else(|| read_num(&d.join("acpi_cppc/highest_perf")))
                .or_else(|| read_num(&d.join("cpu_capacity")))
                .map(|r| r as u32);
            let kind = if !hybrid {
                CoreKind::Uniform
            } else if p_cores.as_ref().is_some_and(|p| p.contains(cpu)) {
                CoreKind::Performance
            } else {
                CoreKind::Efficient
            };
            match cores.iter_mut().find(|c| c.cpus == siblings) {
                Some(c) => c.rank = c.rank.max(rank),
                None => cores.push(Core {
                    cpus: siblings,
                    l3: l3_idx,
                    node: node_of(cpu),
                    kind,
                    rank,
                }),
            }
        }
        cores.sort_by_key(|c| c.cpus.first());
        // L3 indices in CPU order, so CCD0 is the one with CPU 0.
        let mut order: Vec<usize> = (0..l3.len()).collect();
        order.sort_by_key(|i| l3[*i].cpus.first());
        let remap: Vec<usize> = (0..l3.len())
            .map(|old| order.iter().position(|o| *o == old).unwrap())
            .collect();
        for c in &mut cores {
            c.l3 = c.l3.map(|i| remap[i]);
        }
        let l3 = order.into_iter().map(|i| l3[i].clone()).collect();
        Some(Topology {
            vendor,
            online,
            cores,
            l3,
            nodes,
        })
    }

    /// The host's own topology.
    pub fn host() -> Option<Topology> {
        Topology::read(Path::new("/"))
    }

    pub fn smt(&self) -> bool {
        self.cores.iter().any(|c| c.cpus.len() > 1)
    }

    /// Every thread of the cores that hold any of `cpus`.
    pub fn whole_cores(&self, cpus: &CpuSet) -> CpuSet {
        self.cores
            .iter()
            .filter(|c| c.cpus.overlaps(cpus))
            .fold(CpuSet::new(), |a, c| a.union(&c.cpus))
    }

    /// How many physical cores lie entirely inside `cpus`.
    pub fn cores_in(&self, cpus: &CpuSet) -> usize {
        self.cores.iter().filter(|c| c.cpus.is_subset(cpus)).count()
    }

    pub fn split(&self) -> Split {
        if self.nodes.len() > 1 {
            Split::Numa
        } else if self.cores.iter().any(|c| c.kind == CoreKind::Efficient)
            && self.cores.iter().any(|c| c.kind == CoreKind::Performance)
        {
            Split::Hybrid
        } else if self.l3.len() > 1 {
            Split::Cache
        } else {
            Split::Monolithic
        }
    }

    /// The groups a VM is placed in, best first, each with its cores in
    /// placement order. Best: for NUMA the node without CPU 0 (the host's
    /// boot CPU and its default interrupt target); for hybrid the P-cores;
    /// for L3 domains the largest L3 (X3D), then the firmware's fastest
    /// cores, then the one without CPU 0. Inside a domain cores are taken
    /// from the low end, except in the domain holding CPU 0, where they are
    /// taken from the top so the host keeps CPU 0.
    pub fn domains(&self) -> (Split, Vec<Domain>) {
        let split = self.split();
        let cpu0 = self.online.first().unwrap_or(0);
        // (label, cores, preference: higher first)
        type Group = (String, Vec<usize>, (i64, i64, i64));
        let mut groups: Vec<Group> = match split {
            Split::Numa => self
                .nodes
                .iter()
                .map(|(n, cpus)| {
                    let cores = self.cores_where(|c| c.cpus.is_subset(cpus));
                    (
                        format!("node {n}"),
                        cores,
                        (0, 0, !cpus.contains(cpu0) as i64),
                    )
                })
                .collect(),
            Split::Hybrid => vec![
                (
                    "P-cores".to_string(),
                    self.cores_where(|c| c.kind == CoreKind::Performance),
                    (1, 0, 0),
                ),
                (
                    "E-cores".to_string(),
                    self.cores_where(|c| c.kind == CoreKind::Efficient),
                    (0, 0, 0),
                ),
            ],
            Split::Cache => self
                .l3
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let cores = self.cores_where(|c| c.l3 == Some(i));
                    let ranks: Vec<u32> =
                        cores.iter().filter_map(|c| self.cores[*c].rank).collect();
                    let mean = if ranks.is_empty() {
                        0
                    } else {
                        ranks.iter().map(|r| *r as i64).sum::<i64>() / ranks.len() as i64
                    };
                    let label = if self.vendor == Vendor::Amd {
                        format!("CCD{i}")
                    } else {
                        format!("L3 #{i}")
                    };
                    (
                        label,
                        cores,
                        (l.size_kib as i64, mean, !l.cpus.contains(cpu0) as i64),
                    )
                })
                .collect(),
            Split::Monolithic => vec![(
                "cores".to_string(),
                (0..self.cores.len()).collect(),
                (0, 0, 0),
            )],
        };
        groups.retain(|g| !g.1.is_empty());
        // Stable: equal scores keep CPU order.
        groups.sort_by(|a, b| b.2.cmp(&a.2));
        let doms = groups
            .into_iter()
            .map(|(label, mut cores, _)| {
                let cpus = cores
                    .iter()
                    .fold(CpuSet::new(), |a, c| a.union(&self.cores[*c].cpus));
                if cpus.contains(cpu0) {
                    cores.reverse();
                }
                Domain { label, cores, cpus }
            })
            .collect();
        (split, doms)
    }

    fn cores_where(&self, f: impl Fn(&Core) -> bool) -> Vec<usize> {
        (0..self.cores.len())
            .filter(|i| f(&self.cores[*i]))
            .collect()
    }

    /// The domains `cpus` touches, by label.
    pub fn labels_of(&self, cpus: &CpuSet) -> Vec<String> {
        self.domains()
            .1
            .into_iter()
            .filter(|d| d.cpus.overlaps(cpus))
            .map(|d| d.label)
            .collect()
    }
}

/// A CPU's L3: its sharing list and size. The index whose `level` is 3
/// (index3 on every x86 CPU, but the number is not fixed).
fn l3_of(cpu: &Path) -> Option<(CpuSet, u64)> {
    let rd = std::fs::read_dir(cpu.join("cache")).ok()?;
    let mut idx: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("index"))
        })
        .collect();
    idx.sort();
    idx.into_iter().find_map(|p| {
        if read(&p.join("level"))? != "3" {
            return None;
        }
        let cpus = CpuSet::parse(&read(&p.join("shared_cpu_list"))?)?;
        let size = read(&p.join("size"))
            .and_then(|s| parse_size_kib(&s))
            .unwrap_or(0);
        Some((cpus, size))
    })
}

/// Fixture sysfs trees of real CPUs, for tests here and in the policy and
/// apply modules.
#[cfg(test)]
pub mod fixture {
    use super::*;

    /// A scratch directory removed on drop.
    pub struct Tree(pub PathBuf);

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn scratch(tag: &str) -> Tree {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "conduit-hostiso-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tree(d)
    }

    pub fn put(root: &Path, rel: &str, v: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{v}\n")).unwrap();
    }

    /// One CPU's description.
    pub struct C {
        pub siblings: String,
        pub l3: Option<(String, &'static str)>,
        pub rank: Option<u32>,
    }

    /// Write a tree: `cpus[i]` is CPU i.
    pub fn write(
        tag: &str,
        vendor: &str,
        cpus: &[C],
        nodes: &[&str],
        hybrid: Option<(&str, &str)>,
    ) -> Tree {
        let t = scratch(tag);
        let r = &t.0;
        put(
            r,
            "sys/devices/system/cpu/online",
            &format!("0-{}", cpus.len() - 1),
        );
        put(
            r,
            "proc/cpuinfo",
            &format!("processor\t: 0\nvendor_id\t: {vendor}\n"),
        );
        for (i, c) in cpus.iter().enumerate() {
            let d = format!("sys/devices/system/cpu/cpu{i}");
            put(
                r,
                &format!("{d}/topology/thread_siblings_list"),
                &c.siblings,
            );
            put(r, &format!("{d}/cache/index0/level"), "1");
            put(r, &format!("{d}/cache/index0/shared_cpu_list"), &c.siblings);
            if let Some((l3, size)) = &c.l3 {
                put(r, &format!("{d}/cache/index3/level"), "3");
                put(r, &format!("{d}/cache/index3/shared_cpu_list"), l3);
                put(r, &format!("{d}/cache/index3/size"), size);
            }
            if let Some(rank) = c.rank {
                put(
                    r,
                    &format!("{d}/cpufreq/amd_pstate_prefcore_ranking"),
                    &rank.to_string(),
                );
            }
        }
        for (n, list) in nodes.iter().enumerate() {
            put(r, &format!("sys/devices/system/node/node{n}/cpulist"), list);
        }
        if let Some((p, e)) = hybrid {
            put(r, "sys/devices/cpu_core/cpus", p);
            put(r, "sys/devices/cpu_atom/cpus", e);
        }
        t
    }

    /// `cores` cores with SMT numbered like Linux on AMD and Intel desktop
    /// parts: CPU c and c + cores are core c's threads. `ccds` lists each
    /// L3's core range and size; `rank(core)` its prefcore ranking.
    fn smt_part(
        tag: &str,
        vendor: &str,
        cores: u32,
        ccds: &[(u32, u32, &'static str)],
        rank: impl Fn(u32) -> Option<u32>,
    ) -> Tree {
        let mut cpus = Vec::new();
        for i in 0..cores * 2 {
            let core = i % cores;
            let (a, b, size) = *ccds
                .iter()
                .find(|(a, b, _)| (*a..=*b).contains(&core))
                .unwrap();
            cpus.push(C {
                siblings: format!("{core},{}", core + cores),
                l3: Some((format!("{a}-{b},{}-{}", a + cores, b + cores), size)),
                rank: rank(core),
            });
        }
        write(tag, vendor, &cpus, &[&format!("0-{}", cores * 2 - 1)], None)
    }

    /// Ryzen 9 7950X: 16 cores, two CCDs of 8 with 32 MiB L3 each; this
    /// fixture ranks CCD1's cores faster.
    pub fn r7950x() -> Tree {
        smt_part(
            "7950x",
            "AuthenticAMD",
            16,
            &[(0, 7, "32768K"), (8, 15, "32768K")],
            |c| Some(if c < 8 { 170 + c } else { 210 + c }),
        )
    }

    /// Ryzen 7 7800X3D: 8 cores, one CCD with 96 MiB L3.
    pub fn r7800x3d() -> Tree {
        smt_part("7800x3d", "AuthenticAMD", 8, &[(0, 7, "98304K")], |c| {
            Some(200 + c)
        })
    }

    /// Ryzen 9 7950X3D: CCD0 has the 96 MiB L3, CCD1 32 MiB and the higher
    /// clocks (ranked faster).
    pub fn r7950x3d() -> Tree {
        smt_part(
            "7950x3d",
            "AuthenticAMD",
            16,
            &[(0, 7, "98304K"), (8, 15, "32768K")],
            |c| Some(if c < 8 { 180 } else { 230 }),
        )
    }

    /// An 8-core CPU without SMT and with one L3 (Core i7-9700K).
    pub fn mono8() -> Tree {
        let cpus = (0..8)
            .map(|i| C {
                siblings: i.to_string(),
                l3: Some(("0-7".into(), "12288K")),
                rank: None,
            })
            .collect::<Vec<_>>();
        write("mono8", "GenuineIntel", &cpus, &["0-7"], None)
    }

    /// Core i9-13900K: 8 P-cores with HT (CPUs 0-15, siblings adjacent) and
    /// 16 E-cores (CPUs 16-31), one shared L3.
    pub fn i13900k() -> Tree {
        let mut cpus = Vec::new();
        for i in 0..32u32 {
            let siblings = if i < 16 {
                format!("{},{}", i & !1, (i & !1) + 1)
            } else {
                i.to_string()
            };
            cpus.push(C {
                siblings,
                l3: Some(("0-31".into(), "36864K")),
                rank: None,
            });
        }
        write(
            "13900k",
            "GenuineIntel",
            &cpus,
            &["0-31"],
            Some(("0-15", "16-31")),
        )
    }

    /// Two 10-core Xeons with HT: node 0 = CPUs 0-9,20-29, node 1 = 10-19,30-39,
    /// one L3 per socket.
    pub fn xeon2s() -> Tree {
        let mut cpus = Vec::new();
        for i in 0..40u32 {
            let core = i % 20;
            let l3 = if core < 10 {
                "0-9,20-29"
            } else {
                "10-19,30-39"
            };
            cpus.push(C {
                siblings: format!("{core},{}", core + 20),
                l3: Some((l3.into(), "25600K")),
                rank: None,
            });
        }
        write(
            "xeon2s",
            "GenuineIntel",
            &cpus,
            &["0-9,20-29", "10-19,30-39"],
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    fn set(s: &str) -> CpuSet {
        CpuSet::parse(s).unwrap()
    }

    #[test]
    fn ryzen_7950x_has_two_ccds_and_prefers_the_faster_one() {
        let t = r7950x();
        let topo = Topology::read(&t.0).unwrap();
        assert_eq!(topo.vendor, Vendor::Amd);
        assert_eq!(topo.cores.len(), 16);
        assert!(topo.smt());
        assert_eq!(topo.l3.len(), 2);
        assert_eq!(topo.l3[0].cpus, set("0-7,16-23"));
        assert_eq!(topo.cores[8].cpus, set("8,24"));
        assert_eq!(topo.split(), Split::Cache);
        let (_, d) = topo.domains();
        assert_eq!(d[0].label, "CCD1");
        assert_eq!(d[0].cpus, set("8-15,24-31"));
        assert_eq!(topo.cores[d[0].cores[0]].cpus, set("8,24"));
        assert_eq!(d[1].label, "CCD0");
        // CCD0 holds CPU 0: placement there starts at the top.
        assert_eq!(topo.cores[d[1].cores[0]].cpus, set("7,23"));
        assert_eq!(topo.whole_cores(&set("8,9")), set("8-9,24-25"));
        assert_eq!(topo.cores_in(&set("8-15,24-31")), 8);
        assert_eq!(topo.labels_of(&set("7-8")), vec!["CCD1", "CCD0"]);
    }

    #[test]
    fn ryzen_7950x3d_prefers_the_cache_ccd() {
        let t = r7950x3d();
        let topo = Topology::read(&t.0).unwrap();
        let (split, d) = topo.domains();
        assert_eq!(split, Split::Cache);
        assert_eq!(d[0].label, "CCD0");
        assert_eq!(topo.l3[0].size_kib, 98304);
    }

    #[test]
    fn single_ccd_and_monolithic_parts() {
        let t = r7800x3d();
        let topo = Topology::read(&t.0).unwrap();
        assert_eq!(topo.split(), Split::Monolithic);
        let (_, d) = topo.domains();
        assert_eq!(d.len(), 1);
        assert_eq!(topo.cores[d[0].cores[0]].cpus, set("7,15"));

        let t = mono8();
        let topo = Topology::read(&t.0).unwrap();
        assert!(!topo.smt());
        assert_eq!(topo.split(), Split::Monolithic);
        assert_eq!(topo.cores.len(), 8);
    }

    #[test]
    fn intel_hybrid_splits_p_and_e_cores() {
        let t = i13900k();
        let topo = Topology::read(&t.0).unwrap();
        assert_eq!(topo.split(), Split::Hybrid);
        assert_eq!(topo.cores.len(), 24);
        let (_, d) = topo.domains();
        assert_eq!(d[0].label, "P-cores");
        assert_eq!(d[0].cpus, set("0-15"));
        assert_eq!(d[1].label, "E-cores");
        assert_eq!(d[1].cpus, set("16-31"));
    }

    #[test]
    fn two_sockets_split_by_node() {
        let t = xeon2s();
        let topo = Topology::read(&t.0).unwrap();
        assert_eq!(topo.split(), Split::Numa);
        let (_, d) = topo.domains();
        assert_eq!(d[0].label, "node 1");
        assert_eq!(d[0].cpus, set("10-19,30-39"));
    }

    #[test]
    fn sizes_and_missing_trees() {
        assert_eq!(parse_size_kib("32768K"), Some(32768));
        assert_eq!(parse_size_kib("96M"), Some(98304));
        let t = scratch("empty");
        assert!(Topology::read(&t.0).is_none());
    }
}
