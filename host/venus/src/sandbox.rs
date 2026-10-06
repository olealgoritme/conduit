// SPDX-License-Identifier: Apache-2.0
//
// conduit-venus's sandbox (docs/SECURITY.md, docs/VENUS.md).
//
// The renderer runs the host NVIDIA Vulkan driver on command streams a guest
// wrote, so it gets the same posture as conduit-backend: refuse root and
// CAP_SYS_ADMIN, drop capabilities, no_new_privs, then Landlock and seccomp
// before the first request is read. It cannot get the backend's *policy*: the
// driver is a few hundred MiB of userspace that loads libraries, reads its
// configuration, probes sysfs and writes a shader cache, and does all of that
// lazily -- on the guest's vkCreateInstance / vkCreateDevice, long after
// `virgl_renderer_init`. So the rules below were found by tracing a real
// render (strace -f over `examples/venus-render`), not derived from the
// driver's own needs, and they are wider than the backend's.
//
// What is allowed (everything else on the filesystem is not):
//
//   read-only    /usr/lib, /lib, /usr/lib64, /lib64 and the directory
//                libvirglrenderer was loaded from: the driver dlopens its ICD
//                (libGLX_nvidia, libnvidia-glcore, -glvkspirv, -gpucomp, ...)
//                when the guest creates an instance. /etc/ld.so.cache for
//                that search. /usr/share/vulkan, /etc/vulkan: the ICD json.
//                /usr/share/nvidia, /etc/nvidia: application profiles.
//                /usr/share/glvnd, /etc/glvnd, /usr/share/egl, /etc/egl: with
//                no DISPLAY the driver comes up headless through EGL, and
//                without the glvnd vendor json vkCreateInstance fails with
//                VK_ERROR_INCOMPATIBLE_DRIVER. This binary itself (the driver
//                reads it to match application profiles).
//                /proc/driver/nvidia: driver params, GPU NUMA status.
//                /proc/self, /proc/{cpuinfo,meminfo,stat,filesystems},
//                /proc/sys/vm/overcommit_memory, /sys/devices/system/{cpu,
//                memory,node}: what the driver and glibc size themselves by.
//                The sysfs directories of the PCI devices behind /dev/dri and
//                of the NVIDIA GPUs: libdrm's device enumeration.
//   read-write   /dev/nvidiactl, /dev/nvidia[0-15], /dev/nvidia-modeset and
//   + ioctl      /dev/dri: the GPU. /dev/nvidia-uvm: a device created with
//                VK_KHR_acceleration_structure brings up libcuda inside the
//                driver (acceleration-structure builds), which opens it;
//                without it vkCreateDevice fails with
//                VK_ERROR_INITIALIZATION_FAILED, and every D3D12 device
//                (vkd3d-proton enables DXR) with it. Not
//                /dev/nvidia-uvm-tools, not /dev/udmabuf.
//   read-write   $XDG_CACHE_HOME/conduit/venus/VM (one per VM; the driver's
//   + create     shader cache, pointed at by __GL_SHADER_DISK_CACHE_PATH).
//
// Nothing may be executed (Landlock's EXECUTE right is granted nowhere, and
// seccomp refuses execve). With Landlock ABI 4+ TCP bind/connect are refused,
// with ABI 6+ abstract unix sockets and signals to other processes too.
//
// Before the driver loads, the environment is narrowed so that what it loads
// is what was traced: VK_DRIVER_FILES names the NVIDIA ICD alone (the loader
// otherwise dlopens every ICD on the host -- radv, anv, lavapipe and LLVM --
// and Mesa's ICDs then reach for $HOME caches and the X server), layers are
// disabled (an implicit layer is code from wherever its json says, $HOME
// included, injected into this process), and DISPLAY / WAYLAND_DISPLAY are
// removed: the renderer has no window system. A variable already set by the
// caller is left as it is, for debugging, and logged.
//
// seccomp is a denylist, unlike the backend's allowlist. The NVIDIA userspace
// driver's syscall set is not ours to enumerate and changes per release; a
// call an allowlist forgot would fail inside the driver as a GPU error with no
// name on it. It also needs `mprotect(PROT_READ|PROT_WRITE|PROT_EXEC)` on its
// own pages (libnvidia-glcore patches itself at load), so the backend's "no
// executable mappings" rule cannot hold here. What the list refuses is the
// way out of the process: exec, fork, ptrace and the cross-process memory
// calls, bpf/perf/io_uring/userfaultfd/keyctl, mounts and namespaces, module
// and kexec loading; and any socket but AF_UNIX stream/seqpacket, with
// connect and accept refused outright, so there is no way to reach a named
// socket (the X server, D-Bus, journald) after the sandbox is on -- the one
// connection the renderer serves is accepted before it. bind and listen are
// allowed: the libcuda that acceleration structures bring up binds and
// listens on an abstract socket (cuda-uvmfd-NS-PID, to hand its UVM
// descriptor to CUDA IPC peers) and fails its initialization if either is
// refused. Landlock grants MAKE_SOCK nowhere (but the socket's own directory
// on ABI < 8), so a bind can only name an abstract socket, and with accept
// refused nothing that connects to it is ever served.
//
// Ordering. Landlock restricts the calling thread and threads it creates
// later, and before ABI 8 nothing else. virglrenderer's render server is a
// thread started by `virgl_renderer_init`, and the driver starts more. So:
//
//   * Landlock ABI >= 8 (LANDLOCK_RESTRICT_SELF_TSYNC): `prepare` runs at the
//     top of main (single-threaded: posture, environment, cache directory);
//     virglrenderer initializes; the backend's connection is accepted; then
//     `enter` applies Landlock to every thread and seccomp (TSYNC) to every
//     thread, before the first request is read.
//   * Landlock ABI 1..7: Landlock goes on in `prepare`, before virglrenderer
//     starts its thread, with the right to bind and remove the listening
//     socket in its directory; seccomp still waits for `enter`.
//
// Entering late buys less than it sounds: `virgl_renderer_init` with
// RENDER_SERVER does not touch Vulkan, the driver loads when the guest's first
// context creates an instance, and that is after the sandbox in either order.
// So the policy must allow everything the driver opens at instance and device
// creation whichever way round it goes; what the late order does buy is
// nothing to grant for the socket, and one policy for all threads.

use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Why the sandbox could not be prepared or entered.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);

type Result<T> = std::result::Result<T, Error>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

/// Landlock ABI with LANDLOCK_RESTRICT_SELF_TSYNC.
const ABI_TSYNC: i64 = 8;

/// The sandbox between `prepare` and `enter`.
#[derive(Debug)]
pub struct Sandbox {
    abi: i64,
    rules: Rules,
    /// Landlock is already in force (ABI < 8, see the module comment).
    landlocked: bool,
    cache_dir: Option<PathBuf>,
}

/// What `enter` managed, for the start-up log line.
#[derive(Debug, Clone)]
pub struct Report {
    pub landlock_abi: i64,
    pub landlock_all_threads: bool,
    pub seccomp_insns: usize,
    pub cache_dir: Option<PathBuf>,
}

/// Paths and the rights they get.
#[derive(Debug, Default, Clone)]
pub struct Rules {
    /// Read-only trees or files.
    pub read_only: Vec<PathBuf>,
    /// Device nodes or directories of them: read, write, ioctl.
    pub devices: Vec<PathBuf>,
    /// Read, write, create and remove beneath: the shader cache.
    pub read_write: Vec<PathBuf>,
    /// Directories a listening socket may be created and removed in (only
    /// when Landlock goes on before the socket is bound).
    pub sockets: Vec<PathBuf>,
}

/// The renderer's posture, and what it can do without a sandbox: refuse
/// root and CAP_SYS_ADMIN, drop capabilities, set no_new_privs, and point the
/// shader cache at the VM's directory. Run with `--no-sandbox` too.
///
/// RM takes a caller's privilege from its credentials, so a root renderer
/// would make the guest's Vulkan calls an RM administrator's; there is no
/// flag to go on as root.
pub fn posture(vm: &str) -> Result<Option<PathBuf>> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let (eff, prm) = caps()?;
    if euid == 0 {
        return err("refusing to start: conduit-venus runs as root. Run it as the VM's unprivileged user");
    }
    if (eff | prm) & (1 << CAP_SYS_ADMIN) != 0 {
        return err(format!(
            "refusing to start: conduit-venus has CAP_SYS_ADMIN (effective {eff:#x}, permitted {prm:#x})"
        ));
    }
    drop_caps().map_err(|e| Error(format!("dropping capabilities: {e}")))?;
    single_threaded("the environment is set")?;
    Ok(shader_cache(vm))
}

impl Sandbox {
    /// After [`posture`], at the top of `main`, while the process has one
    /// thread: check the kernel, narrow the environment the driver will read,
    /// and on a kernel without Landlock TSYNC apply Landlock now.
    ///
    /// `socket` is the path the renderer will listen on.
    pub fn prepare(socket: &Path, cache_dir: Option<PathBuf>) -> Result<Sandbox> {
        single_threaded("the sandbox is prepared")?;
        let abi = landlock::abi()?;
        seccomp::probe()?;
        narrow_env();

        let mut rules = rules(cache_dir.as_deref());
        let mut landlocked = false;
        if abi < ABI_TSYNC {
            if let Some(dir) = socket.parent() {
                rules.sockets.push(if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir.into() });
            }
            landlock::restrict(abi, &rules, false)?;
            landlocked = true;
        }
        Ok(Sandbox { abi, rules, landlocked, cache_dir })
    }

    /// After virglrenderer is up and the backend's connection is accepted,
    /// before the first request: Landlock (if not on yet) and seccomp, on
    /// every thread.
    pub fn enter(self) -> Result<Report> {
        if !self.landlocked {
            landlock::restrict(self.abi, &self.rules, true)?;
        }
        let insns = seccomp::install()?;
        harden();
        Ok(Report {
            landlock_abi: self.abi,
            landlock_all_threads: self.abi >= ABI_TSYNC,
            seccomp_insns: insns,
            cache_dir: self.cache_dir,
        })
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }
}

/// The paths the NVIDIA Vulkan driver and virglrenderer were seen to need.
/// Paths this host does not have are skipped when the rules are added.
pub fn rules(cache_dir: Option<&Path>) -> Rules {
    let p = PathBuf::from;
    let mut r = Rules::default();
    r.read_only.extend(
        [
            "/usr/lib",
            "/lib",
            "/usr/lib64",
            "/lib64",
            "/etc/ld.so.cache",
            "/usr/share/vulkan",
            "/etc/vulkan",
            "/usr/share/nvidia",
            "/etc/nvidia",
            "/usr/share/glvnd",
            "/etc/glvnd",
            "/usr/share/egl",
            "/etc/egl",
            "/proc/driver/nvidia",
            "/proc/self",
            "/proc/cpuinfo",
            "/proc/meminfo",
            "/proc/stat",
            "/proc/filesystems",
            "/proc/sys/vm/overcommit_memory",
            "/sys/devices/system/cpu",
            "/sys/devices/system/memory",
            "/sys/devices/system/node",
        ]
        .map(p),
    );
    if let Some(dir) = loaded_from("libvirglrenderer") {
        r.read_only.push(dir);
    }
    // The driver reads its own process's binary (application profiles).
    r.read_only.extend(std::env::current_exe().ok());
    r.read_only.extend(gpu_sysfs());

    // nvidia-uvm: libcuda, for acceleration structures (see the module comment).
    r.devices.extend(["/dev/nvidiactl", "/dev/nvidia-modeset", "/dev/nvidia-uvm", "/dev/dri"].map(p));
    r.devices.extend((0..16).map(|n| PathBuf::from(format!("/dev/nvidia{n}"))));

    r.read_write.extend(cache_dir.map(Path::to_path_buf));
    r
}

/// The directory a library mapped into this process was loaded from, if it
/// is not under a tree already allowed: a local virglrenderer build.
fn loaded_from(lib: &str) -> Option<PathBuf> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let path = maps.lines().find_map(|l| l.split_whitespace().nth(5).filter(|p| p.contains(lib)))?;
    Path::new(path).parent().map(Path::to_path_buf)
}

/// The sysfs directories of the PCI devices behind the DRM nodes and of the
/// NVIDIA GPUs, canonical (Landlock rules name inodes, and the paths the
/// driver opens go through /sys/dev/char and /sys/bus/pci symlinks to them).
fn gpu_sysfs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::fs::read_dir("/sys/class/drm") {
        for e in d.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let node = name
                .strip_prefix("card")
                .or_else(|| name.strip_prefix("renderD"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if node && let Ok(dev) = std::fs::canonicalize(e.path().join("device")) {
                v.push(dev);
            }
        }
    }
    if let Ok(d) = std::fs::read_dir("/proc/driver/nvidia/gpus") {
        for e in d.flatten() {
            if let Ok(dev) = std::fs::canonicalize(Path::new("/sys/bus/pci/devices").join(e.file_name())) {
                v.push(dev);
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// `$XDG_CACHE_HOME/conduit/venus/VM` (or `~/.cache/...`), created 0700 and
/// set as the NVIDIA shader cache. `None` (and the disk cache off) when there
/// is no home to put it in or it cannot be made.
fn shader_cache(vm: &str) -> Option<PathBuf> {
    // A caller's own choice wins; it must then be inside the rules to work,
    // which it is not unless it is this directory.
    if let Some(p) = std::env::var_os("__GL_SHADER_DISK_CACHE_PATH") {
        eprintln!("conduit-venus: __GL_SHADER_DISK_CACHE_PATH set by the caller ({}), kept", Path::new(&p).display());
        return Some(PathBuf::from(p));
    }
    let base =
        std::env::var_os("XDG_CACHE_HOME").filter(|v| Path::new(v).is_absolute()).map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").filter(|v| Path::new(v).is_absolute()).map(|h| Path::new(&h).join(".cache"))
        });
    let dir = base.map(|b| b.join("conduit/venus").join(vm));
    let made = dir.filter(|d| {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d).is_ok()
    });
    // SAFETY: single-threaded (checked by `posture`), so nothing reads the
    // environment concurrently.
    unsafe {
        match &made {
            Some(d) => std::env::set_var("__GL_SHADER_DISK_CACHE_PATH", d),
            None => std::env::set_var("__GL_SHADER_DISK_CACHE", "0"),
        }
    }
    made
}

/// The VM name for the cache directory: a single path component.
pub fn vm_name(explicit: Option<&str>, socket: &Path) -> String {
    let raw = explicit.map(str::to_owned).unwrap_or_else(|| {
        // The CLI puts the socket in the VM's own runtime directory.
        socket.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    });
    let clean: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    if clean.is_empty() || clean.chars().all(|c| c == '.') { "default".into() } else { clean }
}

/// The Vulkan loader's ICD json for the NVIDIA driver, wherever this distro
/// puts it.
fn nvidia_icd() -> Option<PathBuf> {
    ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d", "/usr/local/share/vulkan/icd.d"]
        .iter()
        .map(|d| Path::new(d).join("nvidia_icd.json"))
        .find(|p| p.is_file())
}

/// See the module comment. Single-threaded, checked by the caller.
fn narrow_env() {
    let set_default = |k: &str, v: OsString| {
        if let Some(old) = std::env::var_os(k) {
            eprintln!("conduit-venus: {k}={} set by the caller, kept", old.to_string_lossy());
        } else {
            // SAFETY: single-threaded, see `prepare`.
            unsafe { std::env::set_var(k, v) };
        }
    };
    if let Some(icd) = nvidia_icd() {
        set_default("VK_DRIVER_FILES", icd.clone().into());
        // Loaders before 1.3.207 know only the old name.
        set_default("VK_ICD_FILENAMES", icd.into());
    }
    set_default("VK_LOADER_LAYERS_DISABLE", "~all~".into());
    for k in ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(k) };
    }
}

fn single_threaded(what: &str) -> Result<()> {
    let n = std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0);
    if n == 1 { Ok(()) } else { err(format!("{what} before this process starts a thread, and it has {n}")) }
}

/// CAP_SYS_ADMIN's bit (linux/capability.h).
const CAP_SYS_ADMIN: u32 = 21;

/// (effective, permitted) from /proc/self/status.
fn caps() -> Result<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/self/status").map_err(|e| Error(format!("/proc/self/status: {e}")))?;
    Ok(parse_caps(&s))
}

fn parse_caps(status: &str) -> (u64, u64) {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
            .unwrap_or(0)
    };
    (field("CapEff:"), field("CapPrm:"))
}

/// Empty the ambient, inheritable, effective and permitted sets and set
/// no_new_privs, which Landlock and seccomp both require of an unprivileged
/// caller. As the backend's posture does.
fn drop_caps() -> io::Result<()> {
    // SAFETY: prctl with integer arguments.
    let r = unsafe { libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0) };
    if r != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
        return Err(io::Error::last_os_error());
    }
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header { version: 0x2008_0522, pid: 0 };
    let data = [Data::default(); 2];
    // SAFETY: header and two data words live for the call.
    if unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Raise the soft open-file limit to the hard one. Every guest GPU buffer
/// holds a descriptor or two (shared memory, a dma-buf, the NVIDIA driver's
/// own handle), and a Windows desktop with one 3D application open passes
/// the usual soft limit of 1024; past it, buffer creation fails and the
/// guest's Vulkan driver aborts. Nothing here uses select(), so descriptors
/// above FD_SETSIZE are fine. Best effort: the old limit stays on failure.
pub fn raise_nofile() -> u64 {
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit/setrlimit with a valid, owned struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) != 0 {
            return 0;
        }
        if l.rlim_cur < l.rlim_max {
            let raised = libc::rlimit { rlim_cur: l.rlim_max, rlim_max: l.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                l = raised;
            }
        }
    }
    l.rlim_cur
}

/// No core dumps, private files by default. Best effort.
///
/// Not PR_SET_DUMPABLE 0, unlike the backend: it makes /proc/self/* owned by
/// root, and the driver names its threads through /proc/self/task/N/comm.
fn harden() {
    // SAFETY: plain integer arguments.
    unsafe {
        libc::umask(0o077);
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
    }
}

/// Open a path for Landlock to name. Follows symlinks: /lib, /proc/self and
/// sysfs device links name their targets.
fn open_path(p: &Path) -> Option<(OwnedFd, bool)> {
    let c = CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a NUL-terminated path; the result is checked before use.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: open returned a descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fstat into a local.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let dir = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFDIR;
    Some((fd, dir))
}

mod landlock {
    use super::*;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;
    const RESTRICT_SELF_TSYNC: u32 = 1 << 3; // ABI 8

    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const FS_REFER: u64 = 1 << 13; // ABI 2
    const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
    const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5

    /// Rights that apply to a file rather than a directory.
    const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

    const NET_BIND_TCP: u64 = 1 << 0; // ABI 4
    const NET_CONNECT_TCP: u64 = 1 << 1;

    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0; // ABI 6
    const SCOPE_SIGNAL: u64 = 1 << 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    pub fn abi() -> Result<i64> {
        // SAFETY: the version query takes no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return err(format!(
                "refusing to start: this kernel has no Landlock ({}), so conduit-venus cannot confine what \
                 the GPU driver opens. Landlock is in Linux 5.13 and later; check that it is in the lsm= list. \
                 --no-sandbox runs without it, for debugging only",
                io::Error::last_os_error()
            ));
        }
        Ok(abi)
    }

    fn fs_rights(abi: i64) -> u64 {
        let mut r = FS_EXECUTE
            | FS_WRITE_FILE
            | FS_READ_FILE
            | FS_READ_DIR
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM;
        if abi >= 2 {
            r |= FS_REFER;
        }
        if abi >= 3 {
            r |= FS_TRUNCATE;
        }
        if abi >= 5 {
            r |= FS_IOCTL_DEV;
        }
        r
    }

    pub fn restrict(abi: i64, rules: &Rules, all_threads: bool) -> Result<()> {
        let handled = fs_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: if abi >= 4 { NET_BIND_TCP | NET_CONNECT_TCP } else { 0 },
            scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
        };
        let size = match abi {
            1..=3 => 8,
            4 | 5 => 16,
            _ => size_of::<RulesetAttr>(),
        };
        // SAFETY: attr outlives the call and `size` does not exceed it.
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size, 0u32) };
        if fd < 0 {
            return err(format!("landlock_create_ruleset: {}", io::Error::last_os_error()));
        }
        // SAFETY: the syscall returned a descriptor nothing else owns.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };

        let ro = FS_READ_FILE | FS_READ_DIR;
        let dev = FS_READ_FILE | FS_WRITE_FILE | FS_READ_DIR | FS_IOCTL_DEV;
        let rw = FS_READ_FILE
            | FS_WRITE_FILE
            | FS_READ_DIR
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_REFER
            | FS_TRUNCATE;
        let sock = FS_MAKE_SOCK | FS_REMOVE_FILE | FS_READ_DIR;
        for (list, rights) in
            [(&rules.read_only, ro), (&rules.devices, dev), (&rules.read_write, rw), (&rules.sockets, sock)]
        {
            for p in list {
                let Some((parent, is_dir)) = open_path(p) else { continue };
                let rights = rights & handled & if is_dir { !0 } else { FILE_RIGHTS };
                if rights == 0 {
                    continue;
                }
                let rule = PathBeneathAttr { allowed_access: rights, parent_fd: parent.as_raw_fd() };
                // SAFETY: rule outlives the call.
                let r = unsafe {
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        ruleset.as_raw_fd(),
                        RULE_PATH_BENEATH,
                        &rule as *const PathBeneathAttr,
                        0u32,
                    )
                };
                if r != 0 {
                    return err(format!("landlock_add_rule {}: {}", p.display(), io::Error::last_os_error()));
                }
            }
        }
        let flags = if all_threads { RESTRICT_SELF_TSYNC } else { 0 };
        // SAFETY: a ruleset descriptor and flags this ABI knows.
        let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), flags) };
        if r != 0 {
            return err(format!("landlock_restrict_self: {}", io::Error::last_os_error()));
        }
        Ok(())
    }
}

mod seccomp {
    use super::*;

    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
    const AND_K: u16 = 0x54; // BPF_ALU | BPF_AND | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K

    const SET_MODE_FILTER: u32 = 1;
    const FILTER_FLAG_TSYNC: u32 = 1;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    pub(super) const RET_ALLOW: u32 = 0x7fff_0000;

    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const X32_BIT: u32 = 0x4000_0000;

    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const fn arg(n: u32) -> u32 {
        16 + 8 * n
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Insn {
        pub code: u16,
        pub jt: u8,
        pub jf: u8,
        pub k: u32,
    }

    #[repr(C)]
    struct Prog {
        len: u16,
        filter: *const Insn,
    }

    const fn stmt(code: u16, k: u32) -> Insn {
        Insn { code, jt: 0, jf: 0, k }
    }
    const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
        Insn { code, jt, jf, k }
    }
    const fn ret(k: u32) -> Insn {
        stmt(RET_K, k)
    }
    pub(super) const fn errno(e: i32) -> u32 {
        RET_ERRNO | (e as u32 & 0xffff)
    }

    /// Refused whatever their arguments: the ways out of this process and
    /// into kernel surface a renderer has no use for. See the module comment.
    pub(super) fn denied() -> Vec<libc::c_long> {
        let mut v = vec![
            // running something else
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_fork,
            libc::SYS_vfork,
            // other processes' memory
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_process_madvise,
            libc::SYS_pidfd_getfd,
            libc::SYS_kcmp,
            // kernel surface
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_modify_ldt,
            libc::SYS_iopl,
            libc::SYS_ioperm,
            libc::SYS_uselib,
            libc::SYS_personality,
            libc::SYS_syslog,
            libc::SYS_lookup_dcookie,
            libc::SYS_vhangup,
            libc::SYS_quotactl,
            libc::SYS_open_by_handle_at,
            libc::SYS_name_to_handle_at,
            // mounts and namespaces
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_move_mount,
            libc::SYS_open_tree,
            libc::SYS_mount_setattr,
            // the machine
            libc::SYS_kexec_load,
            libc::SYS_kexec_file_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_reboot,
            libc::SYS_swapon,
            libc::SYS_swapoff,
            libc::SYS_acct,
            libc::SYS_settimeofday,
            libc::SYS_clock_settime,
            libc::SYS_clock_adjtime,
            libc::SYS_adjtimex,
            libc::SYS_sethostname,
            libc::SYS_setdomainname,
            // named sockets: the one connection is accepted before this.
            // Not bind and listen: libcuda's abstract socket (module comment).
            libc::SYS_connect,
            libc::SYS_accept,
            libc::SYS_accept4,
        ];
        v.sort_unstable();
        v.dedup();
        v
    }

    pub(super) fn program() -> Vec<Insn> {
        let mut p = vec![
            stmt(LD_W_ABS, ARCH),
            jump(JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
            ret(RET_KILL_PROCESS),
            stmt(LD_W_ABS, NR),
            jump(JSET_K, X32_BIT, 0, 1),
            ret(RET_KILL_PROCESS),
        ];

        // Threads, not processes.
        p.extend([
            stmt(LD_W_ABS, NR),
            jump(JEQ_K, libc::SYS_clone as u32, 0, 4),
            stmt(LD_W_ABS, arg(0)),
            jump(JSET_K, libc::CLONE_THREAD as u32, 1, 0),
            ret(errno(libc::EPERM)),
            ret(RET_ALLOW),
        ]);
        // clone3's flags are behind a pointer; ENOSYS makes glibc fall back
        // to clone.
        p.extend([stmt(LD_W_ABS, NR), jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1), ret(errno(libc::ENOSYS))]);

        // AF_UNIX stream or seqpacket only. No datagram socket: sendto(2) on
        // one reaches any named socket by path, which Landlock (to ABI 8)
        // does not cover and connect being refused does not stop.
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            p.extend([
                stmt(LD_W_ABS, NR),
                jump(JEQ_K, nr as u32, 0, 8),
                stmt(LD_W_ABS, arg(0)),
                jump(JEQ_K, libc::AF_UNIX as u32, 0, 5),
                stmt(LD_W_ABS, arg(1)),
                stmt(AND_K, 0xf),
                jump(JEQ_K, libc::SOCK_STREAM as u32, 1, 0),
                jump(JEQ_K, libc::SOCK_SEQPACKET as u32, 0, 1),
                ret(RET_ALLOW),
                ret(errno(libc::EAFNOSUPPORT)),
            ]);
        }

        let list = denied();
        let n = list.len();
        p.push(stmt(LD_W_ABS, NR));
        for (i, nr) in list.iter().enumerate() {
            // A match jumps over the rest and the ALLOW to the refusal.
            p.push(jump(JEQ_K, *nr as u32, (n - i) as u8, 0));
        }
        p.push(ret(RET_ALLOW));
        p.push(ret(errno(libc::EPERM)));
        p
    }

    pub fn probe() -> Result<()> {
        // SAFETY: a null program: EFAULT if filtering exists, EINVAL if not.
        let r = unsafe { libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, 0, std::ptr::null::<Prog>()) };
        let why = io::Error::last_os_error();
        if r == 0 || why.raw_os_error() != Some(libc::EFAULT) {
            return err(format!("refusing to start: this kernel has no seccomp filtering ({why})"));
        }
        Ok(())
    }

    pub fn install() -> Result<usize> {
        let prog = program();
        if prog.len() > 4096 {
            return err("seccomp program too long");
        }
        let fprog = Prog { len: prog.len() as u16, filter: prog.as_ptr() };
        // TSYNC: every thread of the process, virglrenderer's and the
        // driver's included, or fail.
        // SAFETY: fprog and the program outlive the call.
        let r = unsafe { libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FILTER_FLAG_TSYNC, &fprog as *const Prog) };
        if r != 0 {
            return err(format!("seccomp(SET_MODE_FILTER, TSYNC): {} ({r})", io::Error::last_os_error()));
        }
        Ok(prog.len())
    }
}

/// Checked from inside the sandbox (`conduit-venus --sandbox-selftest`):
/// what must be refused is, and what the driver needs is not. Returns the
/// checks that did not hold, each with what happened.
pub fn selftest(rules: &Rules) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |what: &str, ok: bool, detail: String| {
        eprintln!(
            "  {} {what}{}",
            if ok { "ok  " } else { "FAIL" },
            if detail.is_empty() { detail } else { format!(" ({detail})") }
        );
        if !ok {
            bad.push(what.to_string());
        }
    };
    let res = |r: io::Result<()>| match r {
        Ok(()) => "allowed".to_string(),
        Err(e) => e.to_string(),
    };
    let open_rw = |p: &str| std::fs::OpenOptions::new().read(true).write(true).open(p).map(drop);

    // Must be refused.
    if let Some(home) = std::env::var_os("HOME") {
        let r = std::fs::read_dir(&home).map(drop);
        check("refuse listing $HOME", r.is_err(), res(r));
        let r = std::fs::write(Path::new(&home).join(".conduit-venus-selftest"), b"x");
        check("refuse writing a file in $HOME", r.is_err(), res(r));
    }
    let r = std::fs::read("/etc/hostname").map(drop);
    check("refuse reading /etc/hostname", r.is_err(), res(r));
    let r = std::fs::read("/etc/passwd").map(drop);
    check("refuse reading /etc/passwd", r.is_err(), res(r));
    let r = std::fs::write("/tmp/conduit-venus-selftest", b"x");
    check("refuse writing in /tmp", r.is_err(), res(r));
    let r = open_rw("/dev/nvidia-uvm-tools");
    check("refuse opening /dev/nvidia-uvm-tools", r.is_err(), res(r));
    let r = std::process::Command::new("/bin/true").status().map(drop);
    check("refuse spawning /bin/true", r.is_err(), res(r));
    // SAFETY: execve with NUL-terminated strings; if it were allowed this
    // process would become /bin/true and the self-test would end without PASS.
    let r = unsafe {
        let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null::<libc::c_char>()];
        libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), envp.as_ptr())
    };
    check("refuse execve of /bin/true", r < 0, io::Error::last_os_error().to_string());
    // SAFETY: syscalls with plain arguments; results only compared.
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        check("refuse an AF_INET socket", s < 0, res(if s < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }));
        if s >= 0 {
            libc::close(s);
        }
        let s = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        check(
            "refuse an AF_UNIX datagram socket",
            s < 0,
            res(if s < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }),
        );
        if s >= 0 {
            libc::close(s);
        }
        let r = libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0);
        check("refuse ptrace", r < 0, String::new());
        let r = libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_ulong, 0, 0, 0, 0);
        if r == 0 {
            libc::_exit(0);
        }
        check("refuse fork (clone without CLONE_THREAD)", r < 0, String::new());
    }
    let r = std::os::unix::net::UnixStream::connect("/run/dbus/system_bus_socket").map(drop);
    check("refuse connecting to the D-Bus system socket", r.is_err(), res(r));
    if let Some(cache) = rules.read_write.first() {
        let r = std::os::unix::net::UnixListener::bind(cache.join("selftest.sock")).map(drop);
        check("refuse binding a named socket in the shader cache", r.is_err(), res(r));
    }

    // Must be allowed.
    let r = open_rw("/dev/nvidiactl");
    check("open /dev/nvidiactl read-write", r.is_ok(), res(r));
    if Path::new("/dev/nvidia-uvm").exists() {
        let r = open_rw("/dev/nvidia-uvm");
        check("open /dev/nvidia-uvm read-write", r.is_ok(), res(r));
    }
    if let Some(icd) = nvidia_icd() {
        let r = std::fs::read(&icd).map(drop);
        check("read the NVIDIA ICD json", r.is_ok(), res(r));
    }
    let r = std::fs::read("/proc/self/maps").map(drop);
    check("read /proc/self/maps", r.is_ok(), res(r));
    if let Some(cache) = rules.read_write.first() {
        let f = cache.join("selftest");
        let r = std::fs::write(&f, b"x").and_then(|()| std::fs::remove_file(&f));
        check("write and remove a file in the shader cache", r.is_ok(), res(r));
    }
    // virglrenderer's render server thread existed before `enter`; TSYNC
    // must have reached it.
    let tasks: Vec<String> = std::fs::read_dir("/proc/self/task")
        .map(|d| d.flatten().filter_map(|t| std::fs::read_to_string(t.path().join("status")).ok()).collect())
        .unwrap_or_default();
    let filtered = tasks.iter().filter(|s| s.lines().any(|l| l.split_whitespace().eq(["Seccomp:", "2"]))).count();
    check(
        "seccomp on every thread, including ones started before the sandbox",
        tasks.len() > 1 && filtered == tasks.len(),
        format!("{filtered} of {} threads", tasks.len()),
    );
    let r = std::thread::spawn(|| std::fs::read("/etc/hostname").is_err()).join();
    check("a new thread is confined too", r.unwrap_or(false), String::new());
    bad
}

#[cfg(test)]
mod tests {
    use super::seccomp::*;
    use super::*;

    const RET: u16 = 0x06;

    #[test]
    fn every_jump_lands_inside_the_program() {
        let p = program();
        for (i, insn) in p.iter().enumerate() {
            if matches!(insn.code, 0x15 | 0x45) {
                for off in [insn.jt, insn.jf] {
                    assert!(i + 1 + (off as usize) < p.len(), "instruction {i} jumps out");
                }
            }
        }
    }

    /// The tail is ALLOW (nothing matched) then EPERM (a denied call), and
    /// every comparison of the denylist reaches exactly the EPERM.
    #[test]
    fn the_denylist_jumps_to_the_refusal() {
        let p = program();
        let n = p.len();
        assert_eq!((p[n - 2].code, p[n - 2].k), (RET, RET_ALLOW));
        assert_eq!((p[n - 1].code, p[n - 1].k), (RET, errno(libc::EPERM)));
        let list = denied();
        let first = n - 2 - list.len();
        for (i, nr) in list.iter().enumerate() {
            let insn = p[first + i];
            assert_eq!(insn.k, *nr as u32);
            assert_eq!(first + i + 1 + insn.jt as usize, n - 1);
            assert_eq!(insn.jf, 0);
        }
    }

    #[test]
    fn the_ways_out_are_denied() {
        let list = denied();
        for nr in [
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_writev,
            libc::SYS_bpf,
            libc::SYS_io_uring_setup,
            libc::SYS_unshare,
            libc::SYS_connect,
            libc::SYS_accept,
            libc::SYS_accept4,
        ] {
            assert!(list.contains(&nr), "{nr} not denied");
        }
        // libcuda (acceleration structures) binds and listens on an abstract
        // socket; refusing either fails vkCreateDevice.
        for nr in [libc::SYS_bind, libc::SYS_listen] {
            assert!(!list.contains(&nr), "{nr} denied");
        }
        assert!(list.len() < 250, "jump offsets are u8");
    }

    #[test]
    fn vm_names_are_one_component() {
        assert_eq!(vm_name(Some("win11"), Path::new("/x/y.sock")), "win11");
        assert_eq!(vm_name(Some("../etc"), Path::new("/x/y.sock")), ".._etc");
        assert_eq!(vm_name(Some(".."), Path::new("/x/y.sock")), "default");
        assert_eq!(vm_name(None, Path::new("/run/user/1000/conduit/win11/venus.sock")), "win11");
        assert_eq!(vm_name(None, Path::new("venus.sock")), "default");
    }

    #[test]
    fn capability_sets_are_read_from_the_status_file() {
        let s = "CapPrm:\t000001ffffffffff\nCapEff:\t0000000000200000\n";
        assert_eq!(parse_caps(s), (1 << CAP_SYS_ADMIN, 0x1ff_ffff_ffff));
        assert_eq!(parse_caps(""), (0, 0));
    }
}
