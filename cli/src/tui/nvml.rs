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

pub struct Nvml {
    lib: *mut c_void,
    dev: Dev,
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
            dev: std::ptr::null_mut(),
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
        if by_index(0, &mut n.dev) != 0 {
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

    fn u32_of(&self, f: Option<extern "C" fn(Dev, *mut c_uint) -> Ret>) -> Option<u32> {
        let mut v: c_uint = 0;
        (f?(self.dev, &mut v) == 0).then_some(v)
    }

    fn clock(
        &self,
        f: Option<extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret>,
        kind: c_uint,
    ) -> Option<u32> {
        let mut v: c_uint = 0;
        (f?(self.dev, kind, &mut v) == 0).then_some(v)
    }

    pub fn sample(&self) -> Sample {
        let mut s = Sample::default();
        type Get = extern "C" fn(Dev, *mut c_uint) -> Ret;
        type Get2 = extern "C" fn(Dev, c_uint, *mut c_uint) -> Ret;
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetName",
            extern "C" fn(Dev, *mut c_char, c_uint) -> Ret
        ) {
            let mut b = [0 as c_char; 96];
            if f(self.dev, b.as_mut_ptr(), b.len() as c_uint) == 0 {
                s.name = unsafe { CStr::from_ptr(b.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        // Sensor 0: the GPU core.
        s.temp_c = self.clock(sym!(self, "nvmlDeviceGetTemperature", Get2), 0);
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetUtilizationRates",
            extern "C" fn(Dev, *mut Util) -> Ret
        ) {
            let mut u = Util::default();
            if f(self.dev, &mut u) == 0 {
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
            if f(self.dev, &mut m) == 0 {
                s.vram_used = Some(m.used);
                s.vram_total = Some(m.total);
            }
        }
        let clk = sym!(self, "nvmlDeviceGetClockInfo", Get2);
        let max = sym!(self, "nvmlDeviceGetMaxClockInfo", Get2);
        // 0 graphics, 2 memory.
        s.clk_gfx = self.clock(clk, 0);
        s.clk_mem = self.clock(clk, 2);
        s.clk_gfx_max = self.clock(max, 0);
        s.clk_mem_max = self.clock(max, 2);
        s.power_mw = self.u32_of(sym!(self, "nvmlDeviceGetPowerUsage", Get));
        s.power_limit_mw = self.u32_of(sym!(self, "nvmlDeviceGetEnforcedPowerLimit", Get));
        s.fan_pct = self.u32_of(sym!(self, "nvmlDeviceGetFanSpeed", Get));
        s.pstate = self.u32_of(sym!(self, "nvmlDeviceGetPerformanceState", Get));
        let pcie = sym!(self, "nvmlDeviceGetPcieThroughput", Get2);
        s.pcie_tx_kbs = self.clock(pcie, 0);
        s.pcie_rx_kbs = self.clock(pcie, 1);
        if let Some(f) = sym!(
            self,
            "nvmlDeviceGetEncoderUtilization",
            extern "C" fn(Dev, *mut c_uint, *mut c_uint) -> Ret
        ) {
            let (mut u, mut p) = (0, 0);
            if f(self.dev, &mut u, &mut p) == 0 {
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
            let mut buf = [Proc::default(); 128];
            let mut n = buf.len() as c_uint;
            if f(self.dev, &mut n, buf.as_mut_ptr()) == 0 {
                for p in &buf[..(n as usize).min(buf.len())] {
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

impl Drop for Nvml {
    fn drop(&mut self) {
        if let Some(f) = sym!(self, "nvmlShutdown", extern "C" fn() -> Ret) {
            f();
        }
        unsafe { libc::dlclose(self.lib) };
    }
}
