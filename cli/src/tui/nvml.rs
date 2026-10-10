//! Host GPU readings through NVML (libnvidia-ml.so.1, part of every NVIDIA
//! Linux driver), loaded at run time so `conduit` has no link-time NVIDIA
//! dependency. Missing library or a failing call: that reading is `None`.

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
    pub fn open() -> Option<Nvml> {
        let lib = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if lib.is_null() {
            return None;
        }
        let mut n = Nvml {
            lib,
            devs: Vec::new(),
            driver: String::new(),
        };
        let init = sym!(n, "nvmlInit_v2", extern "C" fn() -> Ret)?;
        if init() != 0 {
            return None;
        }
        let by_index = sym!(
            n,
            "nvmlDeviceGetHandleByIndex_v2",
            extern "C" fn(c_uint, *mut Dev) -> Ret
        )?;
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
        for i in 0..count.min(64) {
            let mut d: Dev = std::ptr::null_mut();
            if by_index(i, &mut d) == 0 {
                n.devs.push(d);
            }
        }
        if n.devs.is_empty() {
            return None;
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
        Some(n)
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
        type Get = extern "C" fn(Dev, *mut c_uint) -> Ret;
        type Get2 = extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret;
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetName",
            extern "C" fn(Dev, *mut c_char, c_uint) -> Ret
        ) {
            let mut b = [0 as c_char; 96];
            if f(dev, b.as_mut_ptr(), b.len() as c_uint) == 0 {
                s.name = unsafe { CStr::from_ptr(b.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        // Sensor 0: the GPU core.
        s.temp_c = clock(dev, sym!(self, "nvmlDeviceGetTemperature", Get2), 0);
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetUtilizationRates",
            extern "C" fn(Dev, *mut Util) -> Ret
        ) {
            let mut u = Util::default();
            if f(dev, &mut u) == 0 {
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
            if f(dev, &mut m) == 0 {
                s.vram_used = Some(m.used);
                s.vram_total = Some(m.total);
            }
        }
        let clk = sym!(self, "nvmlDeviceGetClockInfo", Get2);
        let max = sym!(self, "nvmlDeviceGetMaxClockInfo", Get2);
        // 0 graphics, 2 memory.
        s.clk_gfx = clock(dev, clk, 0);
        s.clk_mem = clock(dev, clk, 2);
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
        if let Some(f) = sym!(self, "nvmlShutdown", extern "C" fn() -> Ret) {
            f();
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
