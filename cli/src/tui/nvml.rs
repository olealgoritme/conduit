//! Host GPU readings through NVML (libnvidia-ml.so.1, part of every NVIDIA
//! Linux driver), loaded at run time so `conduit` has no link-time NVIDIA
//! dependency. A failing call makes that reading `None`; a library that cannot
//! be opened is an error that says which step failed and NVML's own text for it.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

type Dev = *mut c_void;
type Ret = c_int;

#[repr(C)]
#[derive(Default)]
struct Util {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Default)]
struct Mem {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Proc {
    pid: c_uint,
    used: u64,
    gi: c_uint,
    ci: c_uint,
}

/// One GPU's readings at one moment.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    /// NVML's index of the device.
    pub index: u32,
    pub name: String,
    pub temp_c: Option<u32>,
    pub util_gpu: Option<u32>,
    pub util_mem: Option<u32>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub clk_gfx: Option<u32>,
    pub clk_gfx_max: Option<u32>,
    pub clk_mem: Option<u32>,
    pub clk_mem_max: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub fan_pct: Option<u32>,
    pub pstate: Option<u32>,
    pub pcie_tx_kbs: Option<u32>,
    pub pcie_rx_kbs: Option<u32>,
    pub enc_util: Option<u32>,
    /// (pid, VRAM bytes) of every process with memory on the GPU.
    pub procs: Vec<(u32, u64)>,
}

/// An open NVML. It keeps the NVIDIA device files open, which stops the GPU
/// from being unbound from its driver; drop it to let go.
pub struct Nvml {
    lib: *mut c_void,
    devs: Vec<Dev>,
    pub driver: String,
    /// `nvmlInit_v2` succeeded, so `nvmlShutdown` is owed.
    inited: bool,
    /// Core reads of the sample being taken: how many ran, and the first
    /// failure (`what`, code) among them.
    core_calls: Cell<u32>,
    core_fails: Cell<u32>,
    first_fail: Cell<Option<(&'static str, Ret)>>,
}

// The handle is only used from the sampler thread that made it.
unsafe impl Send for Nvml {}

macro_rules! sym {
    ($s:expr, $name:literal, $ty:ty) => {{
        let p = unsafe { libc::dlsym($s.lib, concat!($name, "\0").as_ptr() as *const c_char) };
        if p.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute::<*mut c_void, $ty>(p) })
        }
    }};
}

impl Nvml {
    /// Opens NVML and takes the device handles. The error names the step that
    /// failed and carries NVML's own text for the code.
    pub fn open() -> Result<Nvml, String> {
        let lib = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if lib.is_null() {
            let why = unsafe { libc::dlerror() };
            let why = if why.is_null() {
                "unknown reason".into()
            } else {
                unsafe { CStr::from_ptr(why) }
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(format!("cannot load libnvidia-ml.so.1: {why}"));
        }
        let mut n = Nvml {
            lib,
            devs: Vec::new(),
            driver: String::new(),
            inited: false,
            core_calls: Cell::new(0),
            core_fails: Cell::new(0),
            first_fail: Cell::new(None),
        };
        let init = sym!(n, "nvmlInit_v2", extern "C" fn() -> Ret)
            .ok_or("libnvidia-ml.so.1 has no nvmlInit_v2")?;
        let r = init();
        if r != 0 {
            return Err(n.describe("nvmlInit_v2", r));
        }
        n.inited = true;
        let by_index = sym!(
            n,
            "nvmlDeviceGetHandleByIndex_v2",
            extern "C" fn(c_uint, *mut Dev) -> Ret
        )
        .ok_or("libnvidia-ml.so.1 has no nvmlDeviceGetHandleByIndex_v2")?;
        let mut count: c_uint = 1;
        if let Some(f) = sym!(
            n,
            "nvmlDeviceGetCount_v2",
            extern "C" fn(*mut c_uint) -> Ret
        ) {
            if f(&mut count) != 0 {
                count = 1;
            }
        }
        let mut last = None;
        for i in 0..count.min(64) {
            let mut d: Dev = std::ptr::null_mut();
            match by_index(i, &mut d) {
                0 => n.devs.push(d),
                r => last = Some(n.describe("nvmlDeviceGetHandleByIndex_v2", r)),
            }
        }
        if n.devs.is_empty() {
            return Err(last.unwrap_or_else(|| "NVML sees no GPU".into()));
        }
        if let Some(f) = sym!(
            n,
            "nvmlSystemGetDriverVersion",
            extern "C" fn(*mut c_char, c_uint) -> Ret
        ) {
            let mut b = [0 as c_char; 96];
            if f(b.as_mut_ptr(), b.len() as c_uint) == 0 {
                n.driver = unsafe { CStr::from_ptr(b.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        Ok(n)
    }

    /// `what` failed with `r`: "nvmlInit_v2 failed: Driver Not Loaded (9)".
    fn describe(&self, what: &str, r: Ret) -> String {
        let text = sym!(self, "nvmlErrorString", extern "C" fn(Ret) -> *const c_char)
            .map(|f| f(r))
            .filter(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
        failure_text(what, r, text.as_deref())
    }

    /// A core read of the sample being taken (`what` for the error text).
    fn core(&self, what: &'static str, r: Ret) -> bool {
        self.core_calls.set(self.core_calls.get() + 1);
        if r != 0 {
            self.core_fails.set(self.core_fails.get() + 1);
            if self.first_fail.get().is_none() {
                self.first_fail.set(Some((what, r)));
            }
        }
        r == 0
    }

    /// Whether the handles went stale since the last call: every core read of
    /// the samples taken since then failed (the GPU was reset or lost, or the
    /// driver reloaded, and this NVML's session no longer works). The error
    /// text names the first failing read. Resets the count.
    pub fn take_stale(&self) -> Option<String> {
        let (calls, fails) = (self.core_calls.take(), self.core_fails.take());
        let first = self.first_fail.take();
        if !all_failed(calls, fails) {
            return None;
        }
        first.map(|(what, r)| self.describe(what, r))
    }

    /// How many GPUs NVML sees.
    pub fn count(&self) -> usize {
        self.devs.len()
    }

    /// The first GPU's readings, PCIe throughput included (the stats feed).
    pub fn sample(&self) -> Sample {
        let mut s = self.sample_dev(0);
        (s.pcie_tx_kbs, s.pcie_rx_kbs) = self.pcie(0);
        s
    }

    /// PCIe (tx, rx) in KB/s. Each call samples for about 20 ms, so the
    /// dashboard reads it apart from the rest.
    pub fn pcie(&self, i: usize) -> (Option<u32>, Option<u32>) {
        let Some(&dev) = self.devs.get(i) else {
            return (None, None);
        };
        type Get2 = extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret;
        let f = sym!(self, "nvmlDeviceGetPcieThroughput", Get2);
        (clock(dev, f, 0), clock(dev, f, 1))
    }

    /// One GPU's readings, without PCIe throughput (see [`Nvml::pcie`]).
    pub fn sample_dev(&self, i: usize) -> Sample {
        let mut s = Sample {
            index: i as u32,
            ..Sample::default()
        };
        let Some(&dev) = self.devs.get(i) else {
            return s;
        };
        let u32_of = |f: Option<extern "C" fn(Dev, *mut c_uint) -> Ret>| -> Option<u32> {
            let mut v: c_uint = 0;
            (f?(dev, &mut v) == 0).then_some(v)
        };
        let core_clock = |name: &'static str, kind: c_uint| -> Option<u32> {
            let f = sym!(
                self,
                "nvmlDeviceGetClockInfo",
                extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret
            )?;
            let mut v: c_uint = 0;
            self.core(name, f(dev, kind, &mut v)).then_some(v)
        };
        type Get = extern "C" fn(Dev, *mut c_uint) -> Ret;
        type Get2 = extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret;
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetName",
            extern "C" fn(Dev, *mut c_char, c_uint) -> Ret
        ) {
            let mut b = [0 as c_char; 96];
            let r = f(dev, b.as_mut_ptr(), b.len() as c_uint);
            if self.core("nvmlDeviceGetName", r) {
                s.name = unsafe { CStr::from_ptr(b.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        // Sensor 0: the GPU core.
        if let Some(f) = sym!(self, "nvmlDeviceGetTemperature", Get2) {
            let mut v: c_uint = 0;
            let r = f(dev, 0, &mut v);
            s.temp_c = self.core("nvmlDeviceGetTemperature", r).then_some(v);
        }
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetUtilizationRates",
            extern "C" fn(Dev, *mut Util) -> Ret
        ) {
            let mut u = Util::default();
            if self.core("nvmlDeviceGetUtilizationRates", f(dev, &mut u)) {
                s.util_gpu = Some(u.gpu);
                s.util_mem = Some(u.memory);
            }
        }
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetMemoryInfo",
            extern "C" fn(Dev, *mut Mem) -> Ret
        ) {
            let mut m = Mem::default();
            if self.core("nvmlDeviceGetMemoryInfo", f(dev, &mut m)) {
                s.vram_used = Some(m.used);
                s.vram_total = Some(m.total);
            }
        }
        let max = sym!(self, "nvmlDeviceGetMaxClockInfo", Get2);
        // 0 graphics, 2 memory.
        s.clk_gfx = core_clock("nvmlDeviceGetClockInfo", 0);
        s.clk_mem = core_clock("nvmlDeviceGetClockInfo", 2);
        s.clk_gfx_max = clock(dev, max, 0);
        s.clk_mem_max = clock(dev, max, 2);
        s.power_mw = u32_of(sym!(self, "nvmlDeviceGetPowerUsage", Get));
        s.power_limit_mw = u32_of(sym!(self, "nvmlDeviceGetEnforcedPowerLimit", Get));
        s.fan_pct = u32_of(sym!(self, "nvmlDeviceGetFanSpeed", Get));
        s.pstate = u32_of(sym!(self, "nvmlDeviceGetPerformanceState", Get));
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetEncoderUtilization",
            extern "C" fn(Dev, *mut c_uint, *mut c_uint) -> Ret
        ) {
            let (mut u, mut p) = (0, 0);
            if f(dev, &mut u, &mut p) == 0 {
                s.enc_util = Some(u);
            }
        }
        for name in [
            "nvmlDeviceGetGraphicsRunningProcesses_v3",
            "nvmlDeviceGetComputeRunningProcesses_v3",
        ] {
            let cname = std::ffi::CString::new(name).unwrap();
            let p = unsafe { libc::dlsym(self.lib, cname.as_ptr()) };
            if p.is_null() {
                continue;
            }
            let f: extern "C" fn(Dev, *mut c_uint, *mut Proc) -> Ret =
                unsafe { std::mem::transmute(p) };
            if let Some(list) = processes(|n, buf| f(dev, n, buf)) {
                for p in &list {
                    // NVML_VALUE_NOT_AVAILABLE is all ones.
                    let used = if p.used == u64::MAX { 0 } else { p.used };
                    match s.procs.iter_mut().find(|(pid, _)| *pid == p.pid) {
                        Some(e) => e.1 = e.1.max(used),
                        None => s.procs.push((p.pid, used)),
                    }
                }
            }
        }
        s
    }
}

fn clock(
    dev: Dev,
    f: Option<extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret>,
    kind: c_uint,
) -> Option<u32> {
    let mut v: c_uint = 0;
    (f?(dev, kind, &mut v) == 0).then_some(v)
}

/// "what failed: NVML's text (code)".
fn failure_text(what: &str, r: Ret, text: Option<&str>) -> String {
    match text {
        Some(t) => format!("{what} failed: {t} ({r})"),
        None => format!("{what} failed with NVML error {r}"),
    }
}

/// Whether a sample's core reads all failed (and there were some): the
/// handles are stale. One failing read is only that reading's trouble (a
/// sensor the board lacks).
fn all_failed(calls: u32, fails: u32) -> bool {
    calls > 0 && fails == calls
}

/// NVML_ERROR_INSUFFICIENT_SIZE: the buffer was too small; the count says
/// how many entries there are.
const INSUFFICIENT_SIZE: Ret = 7;

/// A running-process list through `call(&mut count, buffer)`, growing the
/// buffer as NVML asks (processes come and go between calls, so a few tries).
fn processes(mut call: impl FnMut(&mut c_uint, *mut Proc) -> Ret) -> Option<Vec<Proc>> {
    let mut cap = 128usize;
    for _ in 0..4 {
        let mut buf = vec![Proc::default(); cap];
        let mut n = cap as c_uint;
        match call(&mut n, buf.as_mut_ptr()) {
            0 => {
                buf.truncate((n as usize).min(cap));
                return Some(buf);
            }
            INSUFFICIENT_SIZE => cap = (n as usize + 32).max(cap * 2).min(1 << 16),
            _ => return None,
        }
    }
    None
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if self.inited {
            if let Some(f) = sym!(self, "nvmlShutdown", extern "C" fn() -> Ret) {
                f();
            }
        }
        unsafe { libc::dlclose(self.lib) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake NVML call over `have` processes, honest about sizes.
    fn fake(have: usize, calls: &mut usize) -> impl FnMut(&mut c_uint, *mut Proc) -> Ret + '_ {
        move |n, buf| {
            *calls += 1;
            if (*n as usize) < have {
                *n = have as c_uint;
                return INSUFFICIENT_SIZE;
            }
            for i in 0..have {
                // SAFETY: the caller's buffer holds *n >= have entries.
                unsafe {
                    *buf.add(i) = Proc {
                        pid: i as c_uint + 1,
                        used: 1,
                        gi: 0,
                        ci: 0,
                    }
                };
            }
            *n = have as c_uint;
            0
        }
    }

    #[test]
    fn a_failure_names_the_step_and_nvmls_text() {
        assert_eq!(
            failure_text("nvmlInit_v2", 9, Some("Driver Not Loaded")),
            "nvmlInit_v2 failed: Driver Not Loaded (9)"
        );
        assert_eq!(
            failure_text("nvmlInit_v2", 999, None),
            "nvmlInit_v2 failed with NVML error 999"
        );
    }

    #[test]
    fn handles_are_stale_only_when_every_core_read_failed() {
        assert!(!all_failed(0, 0), "nothing read");
        assert!(!all_failed(6, 0));
        assert!(!all_failed(6, 1), "one sensor the board lacks");
        assert!(all_failed(6, 6));
    }

    /// With a GPU: `cargo test -p conduit live_handles -- --ignored --nocapture`.
    /// Shuts NVML down behind the handle's back, as a reset or driver reload
    /// would leave it: the next sample must report the handle stale.
    #[test]
    #[ignore]
    fn live_handles_go_stale_after_a_shutdown() {
        let n = Nvml::open().expect("NVML");
        assert!(!n.sample().name.is_empty());
        assert_eq!(n.take_stale(), None);
        let shutdown = sym!(n, "nvmlShutdown", extern "C" fn() -> Ret).unwrap();
        assert_eq!(shutdown(), 0);
        let _ = n.sample();
        let why = n.take_stale().expect("stale");
        println!("{why}");
        assert!(why.contains("Uninitialized"), "{why}");
        // A fresh open works again.
        drop(n);
        let n = Nvml::open().expect("reopened");
        assert!(!n.sample().name.is_empty());
        assert_eq!(n.take_stale(), None);
    }

    #[test]
    fn a_long_process_list_is_read_whole() {
        let mut calls = 0;
        assert_eq!(processes(fake(5, &mut calls)).unwrap().len(), 5);
        assert_eq!(calls, 1);
        let mut calls = 0;
        let l = processes(fake(300, &mut calls)).unwrap();
        assert_eq!(l.len(), 300);
        assert_eq!(l[299].pid, 300);
        assert_eq!(calls, 2);
        // Any other error is no list, not a panic.
        assert!(processes(|_, _| 3).is_none());
    }
}
