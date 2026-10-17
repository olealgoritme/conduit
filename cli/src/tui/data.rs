//! What the dashboard shows, gathered on a background thread once a second:
//! every VM's state and its processes' load, the host's CPU and memory, and
//! the GPU through NVML. virsh and NVML calls are slow next to a frame, so
//! the UI thread only ever reads the latest [`Snapshot`].

use super::nvml::{Nvml, Sample};
use crate::virt::{self, Link};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The longest a virsh call of the sampler may take.
const VIRSH_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest wait between tries to open NVML again.
const NVML_RETRY_MAX: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Default)]
pub struct Vm {
    pub name: String,
    /// "running", "paused", "stopped", "undefined", "unknown" (libvirt did
    /// not answer), …
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
    /// Host isolation while it runs: `CCD1 8-15,24-31`, or why not
    /// (hostiso.rs); `None` when nothing is recorded.
    pub isolation: Option<String>,
}

impl Vm {
    /// Has a running VM process: running, paused, shutting down.
    pub fn up(&self) -> bool {
        !matches!(
            self.state.as_str(),
            "stopped" | "undefined" | "unknown" | ""
        ) && virt::state_is_up(&self.state)
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
    /// How many NVIDIA GPUs NVML sees (`gpu` is the one the VMs use).
    pub gpus: usize,
    /// NVML was let go (the sampler was told to release it).
    pub gpu_released: bool,
    /// Why there is no GPU reading: the failure NVML reported, while it is
    /// being opened again.
    pub gpu_error: Option<String>,
    pub driver: String,
    /// The sampler's last failure (a panic), for a note.
    pub error: Option<String>,
    /// Bumped on every refresh.
    pub seq: u64,
}

pub type Shared = Arc<Mutex<Snapshot>>;

/// The wait before the next try after `failures` failures in a row: 1 s,
/// doubling up to [`NVML_RETRY_MAX`]. NVML can refuse for a moment while a VM starts or stops
/// on the GPU; a try costs a dlopen and an init, so short waits are cheap.
fn retry_delay(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        n => Duration::from_secs(1)
            .saturating_mul(1u32 << (n - 1).min(10))
            .min(NVML_RETRY_MAX),
    }
}

/// A resource (the open NVML) kept while wanted and opened again, with a
/// growing wait, when opening fails or the open one goes stale.
struct Holder<T> {
    slot: Option<T>,
    /// Failures in a row since the last good reading.
    failures: u32,
    retry_at: Option<Instant>,
    /// The last failure, until a reading works again.
    error: Option<String>,
}

impl<T> Holder<T> {
    fn new() -> Self {
        Holder {
            slot: None,
            failures: 0,
            retry_at: None,
            error: None,
        }
    }

    /// Open while `want` (when no wait is pending), close otherwise.
    fn update(&mut self, want: bool, now: Instant, open: impl FnOnce() -> Result<T, String>) {
        if !want {
            *self = Self::new();
            return;
        }
        if self.slot.is_some() || self.retry_at.is_some_and(|t| now < t) {
            return;
        }
        match open() {
            Ok(v) => {
                self.slot = Some(v);
                self.retry_at = None;
            }
            Err(e) => self.fail(now, e),
        }
    }

    /// The open one no longer works (`why`): let go of it, to be opened again
    /// after the wait.
    fn stale(&mut self, now: Instant, why: String) {
        self.slot = None;
        self.fail(now, why);
    }

    /// A reading worked: forget the failures.
    fn healthy(&mut self) {
        self.failures = 0;
        self.retry_at = None;
        self.error = None;
    }

    fn fail(&mut self, now: Instant, why: String) {
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + retry_delay(self.failures));
        self.error = Some(why);
    }
}

/// What a panic carried, as text.
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Starts the sampler; `poke` makes it refresh at once (after an action).
/// NVML is held open only while `gpu_wanted`: an
/// open NVML keeps the NVIDIA device busy, so the GPU cannot be unbound.
pub fn start(gpu_wanted: Arc<AtomicBool>) -> (Shared, std::sync::mpsc::Sender<()>) {
    let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let out = shared.clone();
    std::thread::spawn(move || {
        let mut nvml: Holder<Nvml> = Holder::new();
        let mut c = Collector::default();
        loop {
            let want = gpu_wanted.load(Ordering::SeqCst);
            nvml.update(want, Instant::now(), Nvml::open);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.collect(nvml.slot.as_ref())
            }));
            // Every core read failing means the handles went stale (the GPU
            // was reset or lost, the driver reloaded): no reading, and NVML
            // is opened again.
            let r = r.map(|mut snap| {
                match nvml.slot.as_ref().map(|n| n.take_stale()) {
                    Some(Some(why)) => {
                        nvml.stale(Instant::now(), why);
                        snap.gpu = None;
                        snap.gpus = 0;
                        snap.driver.clear();
                    }
                    Some(None) => nvml.healthy(),
                    None => {}
                }
                snap
            });
            if let Ok(mut s) = out.lock() {
                let seq = s.seq + 1;
                *s = match r {
                    Ok(snap) => Snapshot {
                        seq,
                        gpu_released: !want,
                        gpu_error: nvml.error.clone(),
                        ..snap
                    },
                    Err(p) => {
                        c = Collector::default();
                        Snapshot {
                            seq,
                            error: Some(format!("the sampler failed: {}", panic_text(&*p))),
                            ..s.clone()
                        }
                    }
                };
            }
            // After publishing: PCIe throughput samples for ~40 ms and is
            // shown with the next snapshot.
            if let Some(n) = &nvml.slot {
                c.pcie = n.pcie(c.picked);
            }
            if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) =
                rx.recv_timeout(Duration::from_millis(1000))
            {
                break;
            }
        }
    });
    (shared, tx)
}

/// Domain states of one libvirt connection, from `virsh list --all` (one
/// call for every domain): name to state, `shut off` reported as `stopped`.
pub fn parse_virsh_list(text: &str) -> HashMap<String, String> {
    let mut lines = text.lines();
    let mut m = HashMap::new();
    let Some(head) = lines.find(|l| l.contains("Name") && l.contains("State")) else {
        return m;
    };
    let (Some(n), Some(st)) = (head.find("Name"), head.find("State")) else {
        return m;
    };
    for l in lines {
        if l.trim_start().starts_with('-') && l.trim().chars().all(|c| c == '-') {
            continue;
        }
        let (Some(name), Some(state)) = (l.get(n..st), l.get(st..)) else {
            continue;
        };
        let (name, state) = (name.trim(), state.trim());
        if name.is_empty() || state.is_empty() {
            continue;
        }
        let state = if state == "shut off" {
            "stopped"
        } else {
            state
        };
        m.insert(name.to_string(), state.to_string());
    }
    m
}

/// The GPU the VMs use: the one holding the most of their memory; else the
/// one picked before; else the first.
pub fn pick_gpu(vm_bytes: &[u64], prev: usize) -> usize {
    match vm_bytes.iter().enumerate().max_by_key(|(_, b)| **b) {
        Some((i, b)) if *b > 0 => i,
        _ if prev < vm_bytes.len() => prev,
        _ => 0,
    }
}

/// Where libvirt keeps a domain's persistent definition (its mtime says
/// when it was last defined).
fn domain_xml_file(uri: &str, dom: &str) -> PathBuf {
    if virt::is_system_uri(uri) {
        PathBuf::from(format!("/etc/libvirt/qemu/{dom}.xml"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| crate::paths::home().join(".config"))
            .join(format!("libvirt/qemu/{dom}.xml"))
    }
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn virsh(uri: &str, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new("virsh");
    c.env("LC_ALL", "C").args(["-c", uri]).args(args);
    crate::sys::output_timeout(&mut c, VIRSH_TIMEOUT)
}

/// Facts from a domain's XML: (windows, MiB, vCPUs).
type Facts = (bool, Option<u64>, Option<u32>);

fn facts_of(xml: &str) -> Facts {
    let mem = xml_num(xml, "memory").map(|(n, u)| match u.as_str() {
        "GiB" | "G" => n * 1024,
        "MiB" | "M" => n,
        "B" | "bytes" => n >> 20,
        _ => n / 1024,
    });
    let cpus = xml_num(xml, "vcpu").map(|(n, _)| n as u32);
    (crate::libvirt::is_windows(xml), mem, cpus)
}

/// A cached [`Facts`]: read again when the definition's (or the link's)
/// mtime changes, or every minute when it cannot be seen.
struct Cached {
    facts: Facts,
    stamp: (Option<SystemTime>, Option<SystemTime>),
    at: Instant,
}

impl Cached {
    fn fresh(&self, stamp: (Option<SystemTime>, Option<SystemTime>)) -> bool {
        stamp == self.stamp && (stamp.0.is_some() || self.at.elapsed() < Duration::from_secs(60))
    }
}

#[derive(Default)]
struct Collector {
    /// pid → (cpu ticks, when) of the last sample.
    ticks: HashMap<i32, (u64, Instant)>,
    cpu_prev: Option<(u64, u64)>,
    /// Facts from each domain's XML, kept until the definition changes.
    xml: HashMap<String, Cached>,
    /// The last states seen per connection, kept when virsh times out.
    states: HashMap<String, HashMap<String, String>>,
    /// The GPU shown, and its last PCIe (tx, rx).
    picked: usize,
    pcie: (Option<u32>, Option<u32>),
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
    /// Every connection's domain states, one `virsh list --all` each.
    fn refresh_states(&mut self, uris: &[String]) {
        for uri in uris {
            if let Some(t) = virsh(uri, &["list", "--all"]) {
                self.states.insert(uri.clone(), parse_virsh_list(&t));
            }
        }
        self.states.retain(|u, _| uris.contains(u));
    }

    fn facts(&mut self, name: &str, l: &Link) -> Facts {
        let stamp = (
            mtime(&domain_xml_file(&l.uri, &l.domain)),
            mtime(&Link::file(name)),
        );
        if let Some(c) = self.xml.get(name) {
            if c.fresh(stamp) {
                return c.facts;
            }
        }
        let Some(xml) = virsh(&l.uri, &["dumpxml", "--inactive", &l.domain]) else {
            // Keep what we had; try again next time.
            return self.xml.get(name).map(|c| c.facts).unwrap_or_default();
        };
        let facts = facts_of(&xml);
        self.xml.insert(
            name.to_string(),
            Cached {
                facts,
                stamp,
                at: Instant::now(),
            },
        );
        facts
    }

    fn collect(&mut self, nvml: Option<&Nvml>) -> Snapshot {
        let samples: Vec<Sample> = nvml
            .map(|n| (0..n.count()).map(|i| n.sample_dev(i)).collect())
            .unwrap_or_default();
        let names = crate::lvrun::all_names();
        let links: HashMap<String, Link> = names
            .iter()
            .filter_map(|n| Link::load(n).map(|l| (n.clone(), l)))
            .collect();
        let mut uris: Vec<String> = links.values().map(|l| l.uri.clone()).collect();
        uris.sort();
        uris.dedup();
        self.refresh_states(&uris);
        self.xml.retain(|n, _| links.contains_key(n));
        let tck = clk_tck();
        let btime = boot_time();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut vms = Vec::new();
        let mut seen = Vec::new();
        let isolation = crate::hostiso::entries();
        for name in names {
            let mut vm = Vm {
                name: name.clone(),
                ..Default::default()
            };
            let link = links.get(&name);
            vm.state = match link {
                Some(l) => self
                    .states
                    .get(&l.uri)
                    .map(|m| {
                        m.get(&l.domain)
                            .cloned()
                            .unwrap_or_else(|| "undefined".into())
                    })
                    .unwrap_or_else(|| "unknown".into()),
                None => crate::lvrun::state_word(&name),
            };
            let rt = crate::run::Rt::new(&name).ok();
            let st = rt.as_ref().map(|r| r.state()).unwrap_or_default();
            if let Some(l) = link {
                vm.libvirt = Some(l.domain.clone());
                let facts = self.facts(&name, l);
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
            if vm.up() {
                vm.isolation = isolation.get(&name).map(crate::hostiso::short);
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
        // Every GPU's processes count towards their VMs; the one holding
        // most of the VMs' memory is the one shown.
        let mut per_gpu = vec![0u64; samples.len()];
        let mut cmdlines: HashMap<u32, String> = HashMap::new();
        for (gi, g) in samples.iter().enumerate() {
            for (pid, used) in &g.procs {
                let cl = cmdlines.entry(*pid).or_insert_with(|| cmdline(*pid));
                for vm in vms.iter_mut() {
                    let tag = format!("/conduit/{}/", vm.name);
                    let guest = format!("guest={},", vm.name);
                    if vm.pid == Some(*pid as i32) || cl.contains(&tag) || cl.contains(&guest) {
                        vm.vram += used;
                        per_gpu[gi] += used;
                        break;
                    }
                }
            }
        }
        let gpus = samples.len();
        let pick = pick_gpu(&per_gpu, self.picked);
        if pick != self.picked {
            self.pcie = (None, None);
        }
        self.picked = pick;
        let gpu = samples.into_iter().nth(pick).map(|mut g| {
            (g.pcie_tx_kbs, g.pcie_rx_kbs) = self.pcie;
            g
        });
        Snapshot {
            vms,
            host: self.host(),
            driver: nvml.map(|n| n.driver.clone()).unwrap_or_default(),
            gpu,
            gpus,
            gpu_released: false,
            gpu_error: None,
            error: None,
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
    fn one_virsh_list_gives_every_state() {
        let t = " Id   Name          State\n------------------------------------\n 3    win11         running\n -    lab           shut off\n 7    my vm two     in shutdown\n -    p             paused\n\n";
        let m = parse_virsh_list(t);
        assert_eq!(m["win11"], "running");
        assert_eq!(m["lab"], "stopped");
        assert_eq!(m["my vm two"], "in shutdown");
        assert_eq!(m["p"], "paused");
        assert_eq!(m.len(), 4);
        assert!(parse_virsh_list("").is_empty());
        assert!(parse_virsh_list("error: failed to connect").is_empty());
    }

    #[test]
    fn the_shown_gpu_is_the_one_the_vms_use() {
        assert_eq!(pick_gpu(&[], 0), 0);
        assert_eq!(pick_gpu(&[0, 0], 1), 1);
        assert_eq!(pick_gpu(&[0, 0], 5), 0);
        assert_eq!(pick_gpu(&[0, 3 << 30], 0), 1);
        assert_eq!(pick_gpu(&[5, 3], 1), 0);
    }

    #[test]
    fn retries_wait_longer_up_to_a_cap() {
        let secs: Vec<u64> = (0..7).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [0, 1, 2, 4, 8, 8, 8]);
        assert_eq!(retry_delay(u32::MAX), NVML_RETRY_MAX);
    }

    #[test]
    fn nvml_is_held_only_while_wanted() {
        let t0 = Instant::now();
        let mut h = Holder::new();
        let opens = std::cell::Cell::new(0);
        let open = |ok: bool| {
            opens.set(opens.get() + 1);
            if ok {
                Ok(1)
            } else {
                Err("nvmlInit_v2 failed: Driver Not Loaded (9)".to_string())
            }
        };
        h.update(true, t0, || open(true));
        assert_eq!(h.slot, Some(1));
        h.update(true, t0, || open(true));
        assert_eq!(opens.get(), 1, "kept open, not opened again");
        // Focus lost: let go of the GPU, and forget any failure.
        h.stale(t0, "old".into());
        h.update(false, t0, || open(true));
        assert!(h.slot.is_none() && h.error.is_none() && h.retry_at.is_none());
        assert_eq!(opens.get(), 1, "not opened while unwanted");
        assert_eq!(h.failures, 0);
    }

    #[test]
    fn a_failed_open_keeps_its_error_and_is_tried_again_soon() {
        let t0 = Instant::now();
        let mut h: Holder<u8> = Holder::new();
        h.update(true, t0, || {
            Err("nvmlInit_v2 failed: Unknown Error (999)".into())
        });
        assert!(h.slot.is_none());
        assert_eq!(
            h.error.as_deref(),
            Some("nvmlInit_v2 failed: Unknown Error (999)")
        );
        // Within the wait: no try.
        h.update(true, t0 + Duration::from_millis(900), || {
            panic!("tried too early")
        });
        // After 1 s: a try; failing again doubles the wait.
        h.update(true, t0 + Duration::from_secs(1), || Err("again".into()));
        assert_eq!(h.error.as_deref(), Some("again"));
        h.update(true, t0 + Duration::from_secs(2), || {
            panic!("tried before the 2 s wait")
        });
        // Success: open, the error stays until a reading works.
        h.update(true, t0 + Duration::from_secs(3), || Ok(7));
        assert_eq!(h.slot, Some(7));
        h.healthy();
        assert!(h.error.is_none());
        assert_eq!(h.failures, 0);
    }

    #[test]
    fn a_stale_handle_is_closed_and_opened_again() {
        let t0 = Instant::now();
        let mut h = Holder::new();
        h.update(true, t0, || Ok(1));
        h.healthy();
        // The GPU was lost: the handle goes, the error says why.
        h.stale(t0, "nvmlDeviceGetName failed: GPU is lost (15)".into());
        assert!(h.slot.is_none());
        assert_eq!(
            h.error.as_deref(),
            Some("nvmlDeviceGetName failed: GPU is lost (15)")
        );
        h.update(true, t0, || panic!("tried at once"));
        h.update(true, t0 + Duration::from_secs(1), || Ok(2));
        assert_eq!(h.slot, Some(2), "reopened");
        // Stale again before any good reading: the wait grows.
        h.stale(t0 + Duration::from_secs(1), "x".into());
        h.update(true, t0 + Duration::from_secs(2), || panic!("too early"));
        h.update(true, t0 + Duration::from_secs(3), || Ok(3));
        assert_eq!(h.slot, Some(3));
        // A good reading resets the wait.
        h.healthy();
        h.stale(t0 + Duration::from_secs(3), "y".into());
        h.update(true, t0 + Duration::from_secs(4), || Ok(4));
        assert_eq!(h.slot, Some(4));
    }

    #[test]
    fn domain_facts_are_read_again_when_the_definition_changes() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t1 = t0 + Duration::from_secs(1);
        let c = Cached {
            facts: (true, Some(4096), Some(4)),
            stamp: (Some(t0), Some(t0)),
            at: Instant::now(),
        };
        assert!(c.fresh((Some(t0), Some(t0))));
        assert!(!c.fresh((Some(t1), Some(t0))), "redefined");
        assert!(!c.fresh((Some(t0), Some(t1))), "re-attached");
        let unseen = Cached {
            stamp: (None, Some(t0)),
            at: Instant::now() - Duration::from_secs(61),
            ..c
        };
        assert!(!unseen.fresh((None, Some(t0))), "unseen: once a minute");
        assert_eq!(
            facts_of("<domain><memory unit='GiB'>8</memory><vcpu>6</vcpu></domain>"),
            (false, Some(8192), Some(6))
        );
    }

    #[test]
    fn panics_become_text() {
        let p = std::panic::catch_unwind(|| panic!("boom {}", 1)).unwrap_err();
        assert_eq!(panic_text(&*p), "boom 1");
        let p = std::panic::catch_unwind(|| panic!("static")).unwrap_err();
        assert_eq!(panic_text(&*p), "static");
    }

    #[test]
    fn reads_the_own_process_ticks() {
        let me = std::process::id() as i32;
        assert!(proc_ticks(me).is_some());
        assert!(rss(me) > 0);
    }
}
