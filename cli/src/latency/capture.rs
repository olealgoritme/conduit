//! The capture side of `conduit trace NAME latency`: find the VM's backend,
//! conduit-venus and QEMU, write a bpftrace program for their PIDs and
//! symbols, and run it as root for a few seconds. Everything is an observer
//! (uprobes, tracepoints): no process is stopped, signalled or changed.

use crate::run::Rt;
use crate::sys;
use crate::ui::oops;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The processes one VM's round trip crosses.
#[derive(Clone, Debug, PartialEq)]
pub struct Pids {
    pub backend: i32,
    pub venus: i32,
    /// None for the built-in runner or when it cannot be told: the vCPU
    /// wakeups and INTx are then left out.
    pub qemu: Option<i32>,
}

/// Is `value` the argument right after `flag` in `args`?
pub fn has_arg_pair(args: &[&[u8]], flag: &str, value: &Path) -> bool {
    let v = value.as_os_str().as_encoded_bytes();
    args.windows(2)
        .any(|w| w[0] == flag.as_bytes() && w[1] == v)
}

fn cmdline(pid: i32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/cmdline")).ok()
}

fn split_args(raw: &[u8]) -> Vec<&[u8]> {
    raw.split(|b| *b == 0).filter(|a| !a.is_empty()).collect()
}

/// The lowest pid whose command line has `flag value`. Command lines are
/// world-readable, also of a process that made itself non-dumpable.
fn pid_with_arg(flag: &str, value: &Path) -> Option<i32> {
    let mut pids: Vec<i32> = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();
    pids.into_iter()
        .find(|&p| cmdline(p).is_some_and(|raw| has_arg_pair(&split_args(&raw), flag, value)))
}

/// Find the VM's processes from what conduit started them with: the
/// backend is the process serving the VM's trace socket, conduit-venus the
/// one listening on its venus socket (both are in their command lines,
/// whether `conduit up` or libvirt's units started them), QEMU from its pid
/// file (or libvirt's).
pub fn find_pids(name: &str) -> Result<Pids> {
    let trace_sock = crate::trace::socket(name);
    let backend = pid_with_arg("--trace-socket", &trace_sock).ok_or_else(|| {
        oops(
            format!("{name}'s GPU backend is not running"),
            format!("Start the VM first (`conduit up {name} --venus`)."),
        )
    })?;
    let rt = Rt::new(name)?;
    let venus = pid_with_arg("--socket", &rt.venus_sock()).ok_or_else(|| {
        oops(
            format!("{name} runs without the Venus renderer (conduit-venus)"),
            "The latency capture follows the Windows guest's Venus copy; start the VM with --venus.",
        )
    })?;
    let qemu = match crate::virt::Link::load(name) {
        Some(l) => l.virsh().qemu_pid(&l.domain),
        None => {
            let st = rt.state();
            if st.vmm == "qemu" {
                rt.pid("vm", &st.vm_comm)
            } else {
                None
            }
        }
    }
    .filter(|&p| sys::alive(p));
    Ok(Pids {
        backend,
        venus,
        qemu,
    })
}

// ------------------------------------------------------------- privileges

/// How bpftrace gets to run as root. File capabilities are no way: bpftrace
/// up to at least 0.20 refuses to start as any other user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Priv {
    /// conduit itself runs as root.
    Root,
    /// `sudo -n` works (cached credentials or a NOPASSWD rule).
    Sudo,
}

pub fn bpftrace() -> Option<PathBuf> {
    sys::which("bpftrace")
}

pub fn sudo_works() -> bool {
    sys::have("sudo")
        && Command::new("sudo")
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

pub fn is_root() -> bool {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// How bpftrace would run now; None when it cannot.
pub fn privilege() -> Option<Priv> {
    if is_root() {
        Some(Priv::Root)
    } else if sudo_works() {
        Some(Priv::Sudo)
    } else {
        None
    }
}

pub const PRIV_HINT: &str =
    "bpftrace runs as root. Run `sudo -v` first: conduit then uses `sudo -n` \
     and never asks for a password itself. Or allow bpftrace without a password in sudoers \
     (docs/TRACING.md \"Latency capture\").";

fn privileged(p: Priv, prog: &Path) -> Command {
    match p {
        Priv::Sudo => {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(prog);
            c
        }
        Priv::Root => Command::new(prog),
    }
}

/// A file of a process that may be non-dumpable (the backend): read it
/// directly, else as root.
fn read_proc(pid: i32, what: &str, p: Priv) -> Option<String> {
    let path = format!("/proc/{pid}/{what}");
    if let Ok(s) = std::fs::read_to_string(&path) {
        return Some(s);
    }
    if p != Priv::Sudo {
        return None;
    }
    let out = Command::new("sudo")
        .args(["-n", "cat", &path])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The program a process runs: /proc/PID/exe, else (non-dumpable, no root)
/// the absolute argv[0] conduit started it with.
fn exe_of(pid: i32, p: Priv) -> Result<PathBuf> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .or_else(|| {
            if p != Priv::Sudo {
                return None;
            }
            let out = Command::new("sudo")
                .args(["-n", "readlink", &format!("/proc/{pid}/exe")])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
        });
    let exe = exe
        .or_else(|| {
            let raw = cmdline(pid)?;
            let a0 = split_args(&raw)
                .first()
                .map(|a| PathBuf::from(String::from_utf8_lossy(a).into_owned()))?;
            (a0.is_absolute() && a0.is_file()).then_some(a0)
        })
        .ok_or_else(|| {
            oops(
                format!("cannot tell which program process {pid} runs"),
                PRIV_HINT,
            )
        })?;
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return Err(oops(
            format!("{} was replaced since the VM started", exe.display()),
            "Its probes would attach to the new file. Restart the VM, then capture.",
        ));
    }
    Ok(exe)
}

/// The first mapping of a library whose path contains `lib`.
pub fn lib_from_maps(maps: &str, lib: &str) -> Option<PathBuf> {
    maps.lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .find(|p| p.contains(lib))
        .map(PathBuf::from)
}

/// The NVIDIA interrupt lines in /proc/interrupts.
pub fn nvidia_irqs(interrupts: &str) -> Vec<u32> {
    interrupts
        .lines()
        .filter(|l| l.contains("nvidia"))
        .filter_map(|l| l.split(':').next()?.trim().parse().ok())
        .collect()
}

// ------------------------------------------------------------ ELF symbols

fn rd<const N: usize>(d: &[u8], at: usize) -> Option<[u8; N]> {
    d.get(at..at + N)?.try_into().ok()
}
fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    rd::<2>(d, at).map(u16::from_le_bytes)
}
fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    rd::<4>(d, at).map(u32::from_le_bytes)
}
fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    rd::<8>(d, at).map(u64::from_le_bytes)
}

/// The defined function symbols of a little-endian ELF64 file (`.symtab`
/// and `.dynsym`), sorted by name: what `nm` lists, without needing it.
pub fn elf_functions(d: &[u8]) -> Option<Vec<String>> {
    if d.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = u64_at(d, 0x28)? as usize;
    let shentsize = u16_at(d, 0x3a)? as usize;
    let shnum = u16_at(d, 0x3c)? as usize;
    let sh = |i: usize| shoff + i * shentsize;
    let mut out = Vec::new();
    for i in 0..shnum {
        let ty = u32_at(d, sh(i) + 4)?;
        if ty != 2 && ty != 11 {
            continue; // SHT_SYMTAB, SHT_DYNSYM
        }
        let (off, size) = (
            u64_at(d, sh(i) + 24)? as usize,
            u64_at(d, sh(i) + 32)? as usize,
        );
        let link = u32_at(d, sh(i) + 40)? as usize;
        let str_off = u64_at(d, sh(link) + 24)? as usize;
        let str_size = u64_at(d, sh(link) + 32)? as usize;
        let strtab = d.get(str_off..str_off + str_size)?;
        for s in d.get(off..off + size)?.chunks_exact(24) {
            let name = u32_at(s, 0)? as usize;
            let (info, shndx) = (s[4], u16_at(s, 6)?);
            if name == 0 || shndx == 0 || info & 0xf != 2 {
                continue; // unnamed, undefined, not STT_FUNC
            }
            let n = strtab.get(name..)?;
            let n = &n[..n.iter().position(|&b| b == 0).unwrap_or(n.len())];
            out.push(String::from_utf8_lossy(n).into_owned());
        }
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// Which symbol a probe goes on: `^` anchors the first part at the start;
/// the parts must appear in order (the regexes capture.sh gave `nm | awk`).
#[derive(Clone, Copy, Debug)]
pub struct Sym(pub &'static str);

impl Sym {
    pub fn matches(&self, name: &str) -> bool {
        let (anchored, pat) = match self.0.strip_prefix('^') {
            Some(p) => (true, p),
            None => (false, self.0),
        };
        let mut rest = name;
        for (i, part) in pat.split(".*").enumerate() {
            let Some(at) = rest.find(part) else {
                return false;
            };
            if i == 0 && anchored && at != 0 {
                return false;
            }
            rest = &rest[at + part.len()..];
        }
        true
    }

    /// The first matching name in a sorted list (what `nm | awk` took).
    pub fn find<'a>(&self, names: &'a [String]) -> Option<&'a str> {
        names.iter().find(|n| self.matches(n)).map(String::as_str)
    }
}

// ----------------------------------------------------------------- script

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bin {
    Backend,
    Venus,
    Virgl,
}

/// One uprobe of the capture.
pub struct Probe {
    pub bin: Bin,
    pub sym: Sym,
    pub ret: bool,
    /// Only in a full capture.
    pub full: bool,
    pub body: &'static str,
}

const fn p(bin: Bin, sym: &'static str, ret: bool, full: bool, body: &'static str) -> Probe {
    Probe {
        bin,
        sym: Sym(sym),
        ret,
        full,
        body,
    }
}

/// The uprobes, as the former host/latency/capture.sh had them. A light capture keeps
/// three (`Venus::dispatch`, the vkr timeline write, the fence callback):
/// under 15 µs of probe cost per round trip, against about 30 for all.
pub const PROBES: &[Probe] = &[
    p(
        Bin::Backend,
        "^_ZN6device5venus5Venus8dispatch17",
        false,
        false,
        r#"$h = arg1; printf("D0 %llu %d %u %u %llu %u %u\n", nsecs, tid, *(uint32*)$h, *(uint32*)($h+4), *(uint64*)($h+8), *(uint32*)($h+16), *(uint8*)($h+20));"#,
    ),
    p(
        Bin::Virgl,
        "^render_context_update_timeline",
        false,
        false,
        r#"printf("T %llu %d %u %u\n", nsecs, tid, arg1, arg2);"#,
    ),
    p(
        Bin::Venus,
        "virgl19write_context_fence17",
        false,
        false,
        r#"printf("WF %llu %d %u %u %llu\n", nsecs, tid, arg1, arg2, arg3);"#,
    ),
    p(
        Bin::Backend,
        "^_ZN6device5venus5Venus8dispatch17",
        true,
        true,
        r#"printf("D1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*6submit17",
        false,
        true,
        r#"printf("S0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*6submit17",
        true,
        true,
        r#"printf("S1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*13submit_fenced17",
        false,
        true,
        r#"printf("S0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*13submit_fenced17",
        true,
        true,
        r#"printf("F1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*12create_fence17",
        false,
        true,
        r#"printf("F0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "IpcClient.*Renderer.*12create_fence17",
        true,
        true,
        r#"printf("F1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "^_ZN15conduit_backend19deliver_completions17",
        false,
        true,
        r#"printf("C0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "^_ZN15conduit_backend13return_chains17",
        false,
        true,
        r#"printf("C0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Backend,
        "VringRwLock.*17signal_used_queue17",
        false,
        true,
        r#"printf("U0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Venus,
        "Virgl.*Renderer.*6submit17",
        false,
        true,
        r#"printf("VS0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Venus,
        "Virgl.*Renderer.*6submit17",
        true,
        true,
        r#"printf("VS1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Venus,
        "Virgl.*Renderer.*12create_fence17",
        false,
        true,
        r#"printf("VF0 %llu %d %u %u %llu\n", nsecs, tid, arg2, arg3, arg4);"#,
    ),
    p(
        Bin::Venus,
        "Virgl.*Renderer.*12create_fence17",
        true,
        true,
        r#"printf("VF1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Virgl,
        "^render_context_dispatch_submit_cmd",
        false,
        true,
        r#"printf("RC0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Virgl,
        "^vkr_dispatch_vkQueueSubmit",
        false,
        true,
        r#"printf("Q0 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Virgl,
        "^vkr_dispatch_vkQueueSubmit",
        true,
        true,
        r#"printf("Q1 %llu %d\n", nsecs, tid);"#,
    ),
    p(
        Bin::Virgl,
        "^vkr_queue_sync_submit",
        false,
        true,
        r#"printf("Y0 %llu %d %u %llu\n", nsecs, tid, arg2, arg3);"#,
    ),
    p(
        Bin::Virgl,
        "^vkr_queue_sync_submit",
        true,
        true,
        r#"printf("Y1 %llu %d\n", nsecs, tid);"#,
    ),
];

/// What the program is written for: the PIDs, and per binary its path and
/// function names.
pub struct Target {
    pub pids: Pids,
    pub bins: Vec<(Bin, PathBuf, Vec<String>)>,
    pub nvidia_irqs: Vec<u32>,
    pub secs: u64,
    pub full: bool,
}

/// The bpftrace program. Probes whose symbol a binary lacks are left out
/// (and listed in the second value); bpftrace refuses a program with a
/// probe it cannot attach.
pub fn script(t: &Target) -> (String, Vec<String>) {
    let (bp, vp) = (t.pids.backend, t.pids.venus);
    let mut s = String::new();
    let _ = writeln!(s, "config = {{ max_strlen = 16 }}");
    let _ = writeln!(s, r#"BEGIN {{ printf("START %llu 0\n", nsecs); }}"#);
    let _ = writeln!(s, "interval:s:{} {{ exit(); }}", t.secs);
    let _ = writeln!(
        s,
        r#"END {{ printf("STOP %llu 0\n", nsecs); clear(@cst); }}"#
    );
    let qemu_cpu = t
        .pids
        .qemu
        .map(|q| format!(" || ($g == {q} && strncmp($p->comm, \"CPU\", 3) == 0)"))
        .unwrap_or_default();
    let _ = write!(
        s,
        r#"rawtracepoint:sched_wakeup, rawtracepoint:sched_wakeup_new {{
  $p = (struct task_struct *)arg0; $g = $p->tgid;
  if ($g == {bp} || $g == {vp}{qemu_cpu}) {{
    $c = $p->thread_info.cpu;
    printf("WK %llu %d %d %d %d %s\n", nsecs, $p->pid, $c, @cst[$c], tid, comm);
  }}
}}
rawtracepoint:sched_waking {{
  $p = (struct task_struct *)arg0; $g = $p->tgid;
  if ($g == {bp} || $g == {vp}) {{ printf("WG %llu %d %d\n", nsecs, tid, $p->pid); }}
}}
rawtracepoint:sched_switch {{
  $n = (struct task_struct *)arg2; $g = $n->tgid;
  if ($g == {bp} || $g == {vp}) {{ printf("RN %llu %d %d\n", nsecs, $n->pid, cpu); }}
}}
tracepoint:power:cpu_idle {{ @cst[args.cpu_id] = args.state == 4294967295 ? 0 : args.state + 1; }}
tracepoint:kvm:kvm_msi_set_irq /pid == {bp}/ {{ printf("MSI %llu %d %llu\n", nsecs, tid, args.address); }}
"#
    );
    if let Some(q) = t.pids.qemu {
        let _ = writeln!(
            s,
            r#"tracepoint:kvm:kvm_set_irq /pid == {q} && tid == {q} && args.level == 1/ {{ printf("IRQ %llu %d %u\n", nsecs, tid, args.gsi); }}"#
        );
    }
    if !t.nvidia_irqs.is_empty() {
        let pred: Vec<String> = t
            .nvidia_irqs
            .iter()
            .map(|i| format!("args.irq == {i}"))
            .collect();
        let _ = writeln!(
            s,
            r#"tracepoint:irq:irq_handler_entry /{}/ {{ printf("NI %llu %d %d\n", nsecs, tid, cpu); }}"#,
            pred.join(" || ")
        );
    }
    let mut missing = Vec::new();
    for pr in PROBES.iter().filter(|pr| t.full || !pr.full) {
        let bin = t.bins.iter().find(|b| b.0 == pr.bin);
        let Some(((_, path, _), sym)) = bin.and_then(|b| Some((b, pr.sym.find(&b.2)?))) else {
            let m = pr.sym.0.trim_start_matches('^').to_string();
            if !missing.contains(&m) {
                missing.push(m);
            }
            continue;
        };
        let pid = if pr.bin == Bin::Backend { bp } else { vp };
        let _ = writeln!(
            s,
            "{}:{}:\"{sym}\" /pid == {pid}/ {{ {} }}",
            if pr.ret { "uretprobe" } else { "uprobe" },
            path.display(),
            pr.body
        );
    }
    (s, missing)
}

/// Look up the binaries and write the program for this VM.
pub fn prepare(pids: Pids, secs: u64, full: bool, p: Priv) -> Result<(String, Vec<String>)> {
    let backend = exe_of(pids.backend, p)?;
    let venus = exe_of(pids.venus, p)?;
    let maps = read_proc(pids.venus, "maps", p);
    let virgl = maps
        .as_deref()
        .and_then(|m| lib_from_maps(m, "libvirglrenderer"));
    let mut bins = Vec::new();
    for (bin, path) in [
        (Bin::Backend, Some(backend)),
        (Bin::Venus, Some(venus)),
        (Bin::Virgl, virgl),
    ] {
        let Some(path) = path else { continue };
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let names = elf_functions(&data).unwrap_or_default();
        bins.push((bin, path, names));
    }
    let irqs = nvidia_irqs(&std::fs::read_to_string("/proc/interrupts").unwrap_or_default());
    Ok(script(&Target {
        pids,
        bins,
        nvidia_irqs: irqs,
        secs,
        full,
    }))
}

/// A Ctrl-C during the capture ends bpftrace (it gets the signal too, and
/// sudo passes it on) and this process goes on to the table.
extern "C" fn ignore_interrupt(_: libc::c_int) {}

/// Run the program; its events go to `out`. An error carries bpftrace's
/// own messages.
pub fn run(script_file: &Path, out: &Path, p: Priv, bpftrace: &Path) -> Result<()> {
    let f = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    // SAFETY: a handler that does nothing; exec resets it in the child.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = ignore_interrupt as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
    let o = privileged(p, bpftrace)
        .arg("-q")
        .arg(script_file)
        .stdin(Stdio::null())
        .stdout(f)
        .stderr(Stdio::piped())
        .output();
    // SAFETY: back to the default disposition.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    let o = o.with_context(|| format!("could not run {}", bpftrace.display()))?;
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    let wrote = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    if !o.status.success() && wrote == 0 {
        let hint = if err.contains("password") || err.contains("sudo") {
            PRIV_HINT.to_string()
        } else if err.contains("BTF") || err.contains("task_struct") {
            "bpftrace needs the kernel's BTF (/sys/kernel/btf/vmlinux) for the scheduler probes."
                .into()
        } else {
            format!("The program is in {}.", script_file.display())
        };
        return Err(oops(
            format!("bpftrace failed: {}", err.lines().last().unwrap_or("")),
            format!("{err}\n{hint}"),
        ));
    }
    Ok(())
}

/// `TID NAME` of every thread of the processes, read when the capture ends.
pub fn threads(pids: &[i32]) -> String {
    let mut s = String::new();
    for pid in pids {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        let mut tids: Vec<i64> = rd
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        tids.sort_unstable();
        for t in tids {
            if let Ok(n) = std::fs::read_to_string(format!("/proc/{pid}/task/{t}/comm")) {
                let _ = writeln!(s, "{t} {}", n.trim());
            }
        }
    }
    s
}

/// The tracing prerequisites, for `conduit doctor`: (all present, what).
/// sudo is not tried here (a failed `sudo -n` is logged); the capture
/// uses it unless conduit runs as root.
pub fn prerequisites() -> (bool, String) {
    let btf = Path::new("/sys/kernel/btf/vmlinux").exists();
    let Some(bt) = bpftrace() else {
        return (
            false,
            format!(
                "bpftrace not installed{} (optional: `conduit trace NAME latency`)",
                if btf { "" } else { ", no kernel BTF" }
            ),
        );
    };
    if !btf {
        return (
            false,
            format!(
                "{}, but no kernel BTF (/sys/kernel/btf/vmlinux): the scheduler probes need it",
                bt.display()
            ),
        );
    }
    let how = if is_root() {
        "running as root"
    } else if sys::have("sudo") {
        "root through `sudo -n` at capture time (run `sudo -v` first)"
    } else {
        return (
            false,
            format!(
                "{} and kernel BTF, but no sudo to run it as root",
                bt.display()
            ),
        );
    };
    (true, format!("{}, kernel BTF, {how}", bt.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_pairs_find_the_vm_s_processes() {
        let raw = b"/opt/conduit/bin/conduit-backend\0--socket\0/run/user/1/conduit/a/gpu.sock\0--trace-socket\0/run/user/1/conduit/a/trace.sock\0";
        let args = split_args(raw);
        assert!(has_arg_pair(
            &args,
            "--trace-socket",
            Path::new("/run/user/1/conduit/a/trace.sock")
        ));
        assert!(!has_arg_pair(
            &args,
            "--trace-socket",
            Path::new("/run/user/1/conduit/b/trace.sock")
        ));
        assert!(!has_arg_pair(
            &args,
            "--socket",
            Path::new("/run/user/1/conduit/a/trace.sock")
        ));
    }

    #[test]
    fn symbol_patterns_are_ordered_parts() {
        let s = Sym("IpcClient.*Renderer.*6submit17");
        assert!(s.matches("_ZN83_$LT$device..venus..IpcClient$u20$as$u20$device..venus..Renderer$GT$6submit17h0123E"));
        assert!(!s.matches("_ZN8Renderer9IpcClient6submit17h0E"));
        assert!(!s.matches("_ZN9IpcClient8Renderer13submit_fenced17h0E"));
        let a = Sym("^_ZN6device5venus5Venus8dispatch17");
        assert!(a.matches("_ZN6device5venus5Venus8dispatch17hb2c5962d928d33a3E"));
        assert!(!a.matches("x_ZN6device5venus5Venus8dispatch17h0E"));
        let names = vec![
            "a_render_context_update_timeline".to_string(),
            "render_context_update_timeline".to_string(),
        ];
        assert_eq!(
            Sym("^render_context_update_timeline").find(&names),
            Some("render_context_update_timeline")
        );
    }

    #[test]
    fn maps_and_interrupts() {
        let maps = "7f00-7f10 r-xp 00000000 103:03 42 /opt/conduit/lib/libvirglrenderer.so.1\n7f20-7f30 r-xp 00000000 103:03 43 /usr/lib/libc.so.6\n";
        assert_eq!(
            lib_from_maps(maps, "libvirglrenderer"),
            Some(PathBuf::from("/opt/conduit/lib/libvirglrenderer.so.1"))
        );
        assert_eq!(lib_from_maps(maps, "libnope"), None);
        let ints = "           CPU0       CPU1\n 226:   1  2  IR-PCI-MSIX-0000:01:00.0    0-edge      nvidia\n 227:  0 0 IR-PCI-MSIX-0000:01:00.0 1-edge nvidia\n NMI:  0 0 Non-maskable interrupts\n  16: 0 0 IO-APIC 16-fasteoi i801_smbus\n";
        assert_eq!(nvidia_irqs(ints), [226, 227]);
    }

    /// A minimal ELF64 with one .symtab: a function, an object, an
    /// undefined function.
    fn tiny_elf() -> Vec<u8> {
        let strtab = b"\0render_context_update_timeline\0some_data\0undef_fn\0".to_vec();
        let mut syms = vec![0u8; 24]; // the null symbol
        let sym = |name: u32, info: u8, shndx: u16| {
            let mut s = vec![0u8; 24];
            s[0..4].copy_from_slice(&name.to_le_bytes());
            s[4] = info;
            s[6..8].copy_from_slice(&shndx.to_le_bytes());
            s
        };
        syms.extend(sym(1, 0x12, 1)); // GLOBAL FUNC, defined
        syms.extend(sym(32, 0x11, 1)); // GLOBAL OBJECT
        syms.extend(sym(42, 0x12, 0)); // undefined
        let mut d = vec![0u8; 64];
        d[..6].copy_from_slice(b"\x7fELF\x02\x01");
        let sym_off = d.len();
        d.extend(&syms);
        let str_off = d.len();
        d.extend(&strtab);
        let shoff = d.len();
        d[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        d[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        d[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes());
        let sh = |ty: u32, off: usize, size: usize, link: u32| {
            let mut h = vec![0u8; 64];
            h[4..8].copy_from_slice(&ty.to_le_bytes());
            h[24..32].copy_from_slice(&(off as u64).to_le_bytes());
            h[32..40].copy_from_slice(&(size as u64).to_le_bytes());
            h[40..44].copy_from_slice(&link.to_le_bytes());
            h
        };
        d.extend(sh(0, 0, 0, 0));
        d.extend(sh(2, sym_off, syms.len(), 2));
        d.extend(sh(3, str_off, strtab.len(), 0));
        d
    }

    #[test]
    fn elf_symbols_are_the_defined_functions() {
        assert_eq!(
            elf_functions(&tiny_elf()).unwrap(),
            ["render_context_update_timeline"]
        );
        assert!(elf_functions(b"not an elf").is_none());
    }

    fn target(full: bool, qemu: Option<i32>) -> Target {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Target {
            pids: Pids {
                backend: 10,
                venus: 20,
                qemu,
            },
            bins: vec![
                (
                    Bin::Backend,
                    PathBuf::from("/opt/conduit/bin/conduit-backend"),
                    names(&[
                        "_ZN6device5venus5Venus8dispatch17hb2c5E",
                        "_ZN15conduit_backend19deliver_completions17h1E",
                    ]),
                ),
                (
                    Bin::Venus,
                    PathBuf::from("/opt/conduit/bin/conduit-venus"),
                    names(&["_ZN13conduit_venus5virgl19write_context_fence17h36E"]),
                ),
                (
                    Bin::Virgl,
                    PathBuf::from("/opt/conduit/lib/libvirglrenderer.so.1"),
                    names(&[
                        "render_context_update_timeline",
                        "vkr_dispatch_vkQueueSubmit",
                    ]),
                ),
            ],
            nvidia_irqs: vec![226, 227],
            secs: 5,
            full,
        }
    }

    #[test]
    fn the_light_program_has_three_uprobes_and_the_tracepoints() {
        let (s, missing) = script(&target(false, Some(30)));
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(s.matches("uprobe:").count(), 3, "{s}");
        assert!(s.contains("uprobe:/opt/conduit/bin/conduit-backend:\"_ZN6device5venus5Venus8dispatch17hb2c5E\" /pid == 10/ { $h = arg1;"));
        assert!(s.contains("uprobe:/opt/conduit/lib/libvirglrenderer.so.1:\"render_context_update_timeline\" /pid == 20/"));
        assert!(s.contains(
            "if ($g == 10 || $g == 20 || ($g == 30 && strncmp($p->comm, \"CPU\", 3) == 0))"
        ));
        assert!(
            s.contains("tracepoint:kvm:kvm_set_irq /pid == 30 && tid == 30 && args.level == 1/")
        );
        assert!(s.contains("/args.irq == 226 || args.irq == 227/"));
        assert!(s.contains("interval:s:5 { exit(); }"));
        assert!(s.contains("clear(@cst)"));
    }

    #[test]
    fn the_full_program_skips_what_the_binaries_lack() {
        let (s, missing) = script(&target(true, None));
        assert!(s.contains("uretprobe:/opt/conduit/bin/conduit-backend:\"_ZN6device5venus5Venus8dispatch17hb2c5E\""));
        assert!(s.contains("C0 %llu"));
        assert!(s.contains("Q0 %llu") && s.contains("Q1 %llu"));
        assert!(
            missing.contains(&"VringRwLock.*17signal_used_queue17".to_string()),
            "{missing:?}"
        );
        assert!(
            missing.contains(&"vkr_queue_sync_submit".to_string()),
            "{missing:?}"
        );
        // No QEMU: no vCPU wakeups, no INTx.
        assert!(!s.contains("kvm_set_irq"));
        assert!(s.contains("if ($g == 10 || $g == 20) {\n    $c"));
    }
}
