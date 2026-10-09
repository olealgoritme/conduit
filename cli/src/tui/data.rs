//! What the dashboard shows, gathered on a background thread once a second:
//! every VM's state and its processes' load, the host's CPU and memory, and
//! the GPU through NVML. virsh and NVML calls are slow next to a frame, so
//! the UI thread only ever reads the latest [`Snapshot`].

use super::nvml::{Nvml, Sample};
use crate::virt::{self, Link};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Vm {
    pub name: String,
    /// "running", "paused", "stopped", "undefined", …
    pub state: String,
    pub libvirt: Option<String>,
    pub windows: bool,
    pub mode: Option<String>,
    pub viewer: bool,
    pub ram_mib: Option<u64>,
    pub cpus: Option<u32>,
    pub pid: Option<i32>,
    /// The VM process's CPU use, in percent of one core.
    pub cpu_pct: f64,
    pub rss: u64,
    pub uptime: Option<Duration>,
    /// GPU memory of the VM's host processes (QEMU, backend, Venus).
    pub vram: u64,
    pub helpers: Vec<(String, i32)>,
}

impl Vm {
    /// Has a running VM process: running, paused, shutting down.
    pub fn up(&self) -> bool {
        !matches!(self.state.as_str(), "stopped" | "undefined" | "")
            && virt::state_is_up(&self.state)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Host {
    pub hostname: String,
    pub kernel: String,
    pub cpu_pct: f64,
    pub cores: usize,
    pub mem_used: u64,
    pub mem_total: u64,
    pub load1: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub vms: Vec<Vm>,
    pub host: Host,
    pub gpu: Option<Sample>,
    pub driver: String,
    /// Bumped on every refresh.
    pub seq: u64,
}

pub type Shared = Arc<Mutex<Snapshot>>;

/// Starts the sampler; `poke` makes it refresh at once (after an action).
pub fn start() -> (Shared, std::sync::mpsc::Sender<()>) {
    let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let out = shared.clone();
    std::thread::spawn(move || {
        let nvml = Nvml::open();
        let mut c = Collector::default();
        loop {
            let snap = c.collect(nvml.as_ref());
            if let Ok(mut s) = out.lock() {
                let seq = s.seq + 1;
                *s = Snapshot { seq, ..snap };
            }
            match rx.recv_timeout(Duration::from_millis(1000)) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                _ => {}
            }
        }
    });
    (shared, tx)
}

#[derive(Default)]
struct Collector {
    /// pid → (cpu ticks, when) of the last sample.
    ticks: HashMap<i32, (u64, Instant)>,
    cpu_prev: Option<(u64, u64)>,
    /// Facts from a domain's XML, read once: (windows, MiB, vCPUs).
    xml: HashMap<String, (bool, Option<u64>, Option<u32>)>,
}

fn read(p: impl AsRef<std::path::Path>) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn clk_tck() -> u64 {
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64
}

fn boot_time() -> u64 {
    read("/proc/stat")
        .lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// (utime + stime, starttime) in clock ticks.
fn proc_ticks(pid: i32) -> Option<(u64, u64)> {
    let s = read(format!("/proc/{pid}/stat"));
    // The command name may hold spaces; fields resume after its ')'.
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ut: u64 = f.get(11)?.parse().ok()?;
    let st: u64 = f.get(12)?.parse().ok()?;
    let start: u64 = f.get(19)?.parse().ok()?;
    Some((ut + st, start))
}

fn rss(pid: i32) -> u64 {
    read(format!("/proc/{pid}/status"))
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0)
        * 1024
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

fn xml_num(xml: &str, tag: &str) -> Option<(u64, String)> {
    let i = xml.find(&format!("<{tag}"))?;
    let rest = &xml[i..];
    let gt = rest.find('>')?;
    let head = &rest[..gt];
    let unit = head
        .split("unit='")
        .nth(1)
        .or_else(|| head.split("unit=\"").nth(1))
        .and_then(|u| u.split(['\'', '"']).next())
        .unwrap_or("")
        .to_string();
    let end = rest.find('<').and_then(|_| rest[1..].find('<'))? + 1;
    rest[gt + 1..end].trim().parse().ok().map(|n| (n, unit))
}

impl Collector {
    fn collect(&mut self, nvml: Option<&Nvml>) -> Snapshot {
        let gpu = nvml.map(|n| n.sample());
        let tck = clk_tck();
        let btime = boot_time();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut vms = Vec::new();
        let mut seen = Vec::new();
        for name in crate::lvrun::all_names() {
            let mut vm = Vm {
                name: name.clone(),
                ..Default::default()
            };
            let link = Link::load(&name);
            vm.state = crate::lvrun::state_word(&name);
            let rt = crate::run::Rt::new(&name).ok();
            let st = rt.as_ref().map(|r| r.state()).unwrap_or_default();
            if let Some(l) = &link {
                vm.libvirt = Some(l.domain.clone());
                let facts = self.xml.entry(name.clone()).or_insert_with(|| {
                    let xml = l.virsh().inactive_xml(&l.domain).unwrap_or_default();
                    let mem = xml_num(&xml, "memory").map(|(n, u)| match u.as_str() {
                        "GiB" | "G" => n * 1024,
                        "MiB" | "M" => n,
                        "B" | "bytes" => n >> 20,
                        _ => n / 1024,
                    });
                    let cpus = xml_num(&xml, "vcpu").map(|(n, _)| n as u32);
                    (crate::libvirt::is_windows(&xml), mem, cpus)
                });
                vm.windows = facts.0;
                vm.ram_mib = facts.1;
                vm.cpus = facts.2;
                if vm.up() {
                    vm.pid = l.virsh().qemu_pid(&l.domain);
                }
            }
            if let Ok(c) = crate::vm::VmConfig::load(&name) {
                vm.ram_mib = Some(c.ram_mib);
                vm.cpus = Some(c.cpus);
            }
            if let Some(r) = &rt {
                if vm.pid.is_none() {
                    vm.pid = r.pid("vm", &st.vm_comm);
                }
                vm.viewer = r.pid("viewer", &st.viewer_comm).is_some();
                for (what, comm) in [
                    ("backend", &st.backend_comm),
                    ("venus", &st.venus_comm),
                    ("viewer", &st.viewer_comm),
                ] {
                    if let Some(p) = r.pid(what, comm) {
                        vm.helpers.push((what.to_string(), p));
                    }
                }
                if vm.up() {
                    let m = read(r.p("libvirt-mode"));
                    let m = m.trim();
                    vm.mode = if !m.is_empty() {
                        Some(m.to_string())
                    } else {
                        st.mode.clone()
                    };
                }
            }
            if let Some(pid) = vm.pid {
                if let Some((t, start)) = proc_ticks(pid) {
                    let now = Instant::now();
                    if let Some((pt, pw)) = self.ticks.get(&pid) {
                        let dt = now.duration_since(*pw).as_secs_f64();
                        if dt > 0.0 {
                            vm.cpu_pct = (t.saturating_sub(*pt)) as f64 / tck as f64 / dt * 100.0;
                        }
                    }
                    self.ticks.insert(pid, (t, now));
                    seen.push(pid);
                    let started = btime + start / tck;
                    vm.uptime = Some(Duration::from_secs(now_unix.saturating_sub(started)));
                }
                vm.rss = rss(pid);
            }
            vms.push(vm);
        }
        self.ticks.retain(|p, _| seen.contains(p));
        if let Some(g) = &gpu {
            for (pid, used) in &g.procs {
                let cl = cmdline(*pid);
                for vm in vms.iter_mut() {
                    let tag = format!("/conduit/{}/", vm.name);
                    let guest = format!("guest={},", vm.name);
                    if vm.pid == Some(*pid as i32) || cl.contains(&tag) || cl.contains(&guest) {
                        vm.vram += used;
                        break;
                    }
                }
            }
        }
        Snapshot {
            vms,
            host: self.host(),
            driver: nvml.map(|n| n.driver.clone()).unwrap_or_default(),
            gpu,
            seq: 0,
        }
    }

    fn host(&mut self) -> Host {
        let mut h = Host {
            hostname: read("/proc/sys/kernel/hostname").trim().to_string(),
            kernel: read("/proc/sys/kernel/osrelease").trim().to_string(),
            ..Default::default()
        };
        let stat = read("/proc/stat");
        h.cores = stat
            .lines()
            .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
            .count();
        if let Some(l) = stat.lines().next() {
            let v: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .filter_map(|x| x.parse().ok())
                .collect();
            let total: u64 = v.iter().sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            if let Some((pt, pi)) = self.cpu_prev {
                let dt = total.saturating_sub(pt);
                if dt > 0 {
                    h.cpu_pct = 100.0 * (dt - idle.saturating_sub(pi).min(dt)) as f64 / dt as f64;
                }
            }
            self.cpu_prev = Some((total, idle));
        }
        let mi = read("/proc/meminfo");
        let kib = |k: &str| -> u64 {
            mi.lines()
                .find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse().ok())
                .unwrap_or(0)
                * 1024
        };
        h.mem_total = kib("MemTotal:");
        h.mem_used = h.mem_total.saturating_sub(kib("MemAvailable:"));
        h.load1 = read("/proc/loadavg")
            .split_whitespace()
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0.0);
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_memory_and_vcpus_from_domain_xml() {
        let xml = "<domain><memory unit='KiB'>16777216</memory><vcpu placement='static'>8</vcpu></domain>";
        assert_eq!(xml_num(xml, "memory"), Some((16777216, "KiB".into())));
        assert_eq!(xml_num(xml, "vcpu").map(|x| x.0), Some(8));
    }

    #[test]
    fn reads_the_own_process_ticks() {
        let me = std::process::id() as i32;
        assert!(proc_ticks(me).is_some());
        assert!(rss(me) > 0);
    }
}
