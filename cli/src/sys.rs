//! Running programs, processes by pid file, sudo, locks.
//!
//! Processes are only ever found through Conduit's own pid files, and a pid is
//! only signalled after checking its kernel name (`comm`) still matches, so a
//! reused pid is never hit. Nothing here matches processes by command line.

use crate::ui::{self, oops};
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub fn have(cmd: &str) -> bool {
    which(cmd).is_some()
}

pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(PathBuf::from(cmd)).filter(|p| p.is_file());
    }
    let path = std::env::var_os("PATH")?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    // sbin is often missing from a user's PATH but tools like ip/mkfs live there.
    for d in ["/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.into_iter().map(|d| d.join(cmd)).find(|p| p.is_file())
}

/// Run and capture stdout; error if it fails.
pub fn output(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("could not run {cmd}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run quietly; true when it exited 0.
pub fn quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- sudo

static SUDO_EXPLAINED: AtomicBool = AtomicBool::new(false);
static SUDO_NONINTERACTIVE: AtomicBool = AtomicBool::new(false);

/// Background helpers (the viewer watcher) have no terminal: never prompt there.
pub fn sudo_noninteractive() {
    SUDO_NONINTERACTIVE.store(true, Ordering::SeqCst);
}

/// Make sure sudo will work, explaining why we need it before any password prompt.
pub fn sudo_ready(why: &str) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    if !have("sudo") {
        return Err(oops(
            "this step needs administrator rights, but `sudo` is not installed",
            "Install sudo, or run the command as root",
        ));
    }
    if quiet("sudo", &["-n", "true"]) {
        return Ok(());
    }
    if SUDO_NONINTERACTIVE.load(Ordering::SeqCst) {
        anyhow::bail!("sudo needs a password and there is no terminal to ask on");
    }
    if !SUDO_EXPLAINED.swap(true, Ordering::SeqCst) {
        ui::info(format!(
            "{why}\n         This needs administrator rights, so sudo will ask for your password."
        ));
    }
    let ok = Command::new("sudo")
        .args(["-v", "-p", "[sudo] password for %u: "])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err(oops(
            "sudo did not succeed",
            "Check your password, and that your user may use sudo (it should be in the 'sudo' or 'wheel' group)",
        ));
    }
    Ok(())
}

fn sudo_cmd(cmd: &str, args: &[&str]) -> Command {
    let root = unsafe { libc::geteuid() } == 0;
    let mut c = if root {
        Command::new(cmd)
    } else {
        Command::new("sudo")
    };
    if !root {
        c.arg("-n").arg(
            which(cmd)
                .map(|p| p.display().to_string())
                .unwrap_or(cmd.into()),
        );
    }
    c.args(args);
    c
}

/// Run as root; error with the tool's message when it fails.
pub fn sudo(cmd: &str, args: &[&str]) -> Result<()> {
    let out = sudo_cmd(cmd, args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {cmd}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run as root; true when it exited 0 (used for "is this rule there" checks).
pub fn sudo_ok(cmd: &str, args: &[&str]) -> bool {
    sudo_cmd(cmd, args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- processes

pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub fn comm(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// When the process started (clock ticks after boot, field 22 of
/// /proc/PID/stat). With the pid it names one process, even once the pid is
/// reused.
pub fn start_time(pid: i32) -> Option<u64> {
    parse_start_time(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_start_time(stat: &str) -> Option<u64> {
    // The command name (field 2) is in parentheses and may itself hold spaces
    // or parentheses, so count from the last ')': field 3 comes right after.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

pub fn read_pid(file: &Path) -> Option<i32> {
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// The pid in `file` if that process is alive and still the program we started.
pub fn live_pid(file: &Path, expect_comm: &str) -> Option<i32> {
    let pid = read_pid(file)?;
    if !alive(pid) {
        return None;
    }
    match comm(pid) {
        Some(c) if c == expect_comm => Some(pid),
        _ => None,
    }
}

pub fn write_pid(file: &Path, pid: u32) -> Result<()> {
    fs::write(file, format!("{pid}\n")).with_context(|| format!("writing {}", file.display()))
}

/// SIGTERM, wait up to `grace`, then SIGKILL. Removes the pid file.
pub fn stop_pid(file: &Path, expect_comm: &str, what: &str, grace: Duration) {
    if let Some(pid) = live_pid(file, expect_comm) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let t = Instant::now();
        while alive(pid) && t.elapsed() < grace {
            sleep(Duration::from_millis(100));
        }
        if alive(pid) && comm(pid).as_deref() == Some(expect_comm) {
            ui::info(format!("{what} (pid {pid}) did not stop, forcing it"));
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let t = Instant::now();
            while alive(pid) && t.elapsed() < Duration::from_secs(2) {
                sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = fs::remove_file(file);
}

/// Start a program in its own session, output to `log`, and return its pid.
pub fn spawn_detached(cmd: &mut Command, log: &Path, append: bool) -> Result<u32> {
    let mut opts = OpenOptions::new();
    opts.create(true).mode(0o644);
    if append {
        opts.append(true);
    } else {
        opts.write(true).truncate(true);
    }
    let out = opts
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let err = out.try_clone()?;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .with_context(|| format!("could not start {:?}", cmd.get_program()))?;
    Ok(child.id())
}

/// Start one of a VM's processes (backend, runner, virtiofsd) like
/// [`spawn_detached`], inside the VM's slice when there is one, and marked as
/// an early OOM victim so the kernel kills a VM before the desktop.
pub fn spawn_vm_part(
    cmd: &mut Command,
    log: &Path,
    append: bool,
    slice: Option<&crate::scope::Slice>,
) -> Result<u32> {
    let mut wrapped = slice.map(|s| s.wrap(cmd));
    let cmd = wrapped.as_mut().unwrap_or(cmd);
    unsafe {
        cmd.pre_exec(|| {
            // Raising one's own score needs no privilege; inherited across exec.
            let path = c"/proc/self/oom_score_adj";
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd >= 0 {
                let v = crate::scope::OOM_SCORE_ADJ.as_bytes();
                libc::write(fd, v.as_ptr().cast(), v.len());
                libc::close(fd);
            }
            Ok(())
        });
    }
    spawn_detached(cmd, log, append)
}

/// Wait until `cond` is true or `timeout` passes; false on timeout.
pub fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if t.elapsed() >= timeout {
            return false;
        }
        sleep(Duration::from_millis(100));
    }
}

/// Last `n` lines of a file, for error messages.
pub fn tail(file: &Path, n: usize) -> String {
    let s = fs::read_to_string(file).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// ---------------------------------------------------------------- sockets

/// Is somebody listening on this unix socket?
pub fn socket_live(p: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(p).is_ok()
}

/// Remove a socket file nobody serves any more. Errors if it is in use.
pub fn clear_stale_socket(p: &Path) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    if socket_live(p) {
        return Err(oops(
            format!("{} is still in use by another program", p.display()),
            "Another copy of this VM may be running. Try `conduit down NAME` first.",
        ));
    }
    fs::remove_file(p).with_context(|| format!("removing stale {}", p.display()))
}

// ---------------------------------------------------------------- lock

/// An exclusive lock on a file, released on drop. Not inherited by children (O_CLOEXEC).
pub struct Lock(#[allow(dead_code)] File);

/// A VM's lock. If another conduit command holds it (starting or stopping the
/// VM), say so and wait up to `timeout` instead of hanging silently.
pub fn lock_vm(path: &Path, vm: &str, timeout: Duration) -> Result<Lock> {
    if let Ok(l) = lock(path, Duration::ZERO) {
        return Ok(l);
    }
    ui::info(format!(
        "{vm} is busy: another conduit command is starting or stopping it; waiting up to {} s…",
        timeout.as_secs()
    ));
    lock(path, timeout).map_err(|_| {
        oops(
            format!("{vm} is still busy after {} s", timeout.as_secs()),
            format!("See `conduit status {vm}`; `conduit poweroff {vm}` forces it off"),
        )
    })
}

pub fn lock(path: &Path, timeout: Duration) -> Result<Lock> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let t = Instant::now();
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Lock(f));
        }
        if t.elapsed() > timeout {
            return Err(oops(
                "another conduit command is busy with this VM",
                "Wait for it to finish, then try again",
            ));
        }
        sleep(Duration::from_millis(200));
    }
}

/// Free bytes for an unprivileged user on the filesystem holding `p`.
#[allow(clippy::unnecessary_cast)] // field types differ between glibc and musl
pub fn free_bytes(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().to_str()?).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_time_counts_from_the_last_paren() {
        let stat = "4242 (qemu (x) 1) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 9 0 \
                    123456 1000000 500 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(123456));
        assert_eq!(parse_start_time("garbage"), None);
    }

    #[test]
    fn start_time_of_this_process() {
        assert!(start_time(std::process::id() as i32).is_some());
    }
}
