//! `conduit setup` in a real pseudo-terminal: it must end, not spin, when
//! the terminal goes away or a termination signal arrives.
//!
//! The binary reaches its Welcome screen after the doctor's host checks,
//! which only look (access(2), /proc, /sys, `--version` runs); nothing is
//! changed and no GPU device is opened.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const WELCOME: &[u8] = b"Press Enter to begin";

struct Session {
    child: Child,
    master: Option<OwnedFd>,
    out: Vec<u8>,
    home: std::path::PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// `conduit setup` on a fresh 100x30 pty as its controlling terminal, with
/// HOME and the XDG dirs in a temp dir, once it shows the Welcome screen.
fn start(name: &str) -> Session {
    let home = std::env::temp_dir().join(format!("conduit-pty-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // Held until the spawn: another test's child must not inherit this
    // pty's fds before they are close-on-exec, or closing the master here
    // would not hang the terminal up.
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let (mut m, mut s) = (0, 0);
    let ws = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let r = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), &ws) };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    for fd in [m, s] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_conduit"));
    cmd.arg("setup")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_RUNTIME_DIR", &home)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            // Its own session, with the pty as the controlling terminal: a
            // hangup then signals it as a closing terminal window would.
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    drop(cmd); // the parent's copies of the slave
    drop(lock);
    let mut s = Session {
        child,
        master: Some(master),
        out: Vec::new(),
        home,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !contains(&s.out, WELCOME) {
        assert!(
            Instant::now() < deadline,
            "no Welcome screen; output: {}",
            String::from_utf8_lossy(&s.out)
        );
        assert!(s.child.try_wait().unwrap().is_none(), "setup exited early");
        s.pump(Duration::from_millis(50));
    }
    s
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl Session {
    /// Read what the program wrote, waiting at most `wait`.
    fn pump(&mut self, wait: Duration) {
        let Some(m) = &self.master else { return };
        let mut p = libc::pollfd {
            fd: m.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, wait.as_millis() as i32) } <= 0 {
            return;
        }
        let mut buf = [0u8; 65536];
        let n = unsafe { libc::read(m.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            self.out.extend_from_slice(&buf[..n as usize]);
        } else {
            // EIO: the slave side is closed (the program is gone).
            std::thread::sleep(wait);
        }
    }

    /// CPU ticks (user + system) the program used so far.
    fn cpu_ticks(&self) -> Option<u64> {
        let s = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).ok()?;
        let f: Vec<&str> = s[s.rfind(')')? + 2..].split_whitespace().collect();
        Some(f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?)
    }

    /// Its exit within `limit`, reading its output meanwhile; on a timeout,
    /// the CPU it burnt in the last second, to tell a spin from a hang.
    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                // What it wrote just before exiting.
                for _ in 0..5 {
                    self.pump(Duration::from_millis(20));
                }
                return st;
            }
            if Instant::now() >= deadline {
                let t0 = self.cpu_ticks();
                std::thread::sleep(Duration::from_secs(1));
                let t1 = self.cpu_ticks();
                panic!(
                    "setup still runs {limit:?} later; CPU ticks in the last second: {:?}",
                    t0.zip(t1).map(|(a, b)| b - a)
                );
            }
            self.pump(Duration::from_millis(20));
        }
    }
}

#[test]
fn closing_the_terminal_ends_setup_with_the_hangup_status() {
    let mut s = start("hup");
    s.master = None; // the terminal window goes away
    let st = s.exit_within(Duration::from_secs(3));
    assert_eq!(st.code(), Some(129), "{st:?}");
}

#[test]
fn sigterm_ends_setup_with_its_status_and_restores_the_terminal() {
    let mut s = start("term");
    let before = s.out.len();
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    let st = s.exit_within(Duration::from_secs(3));
    assert_eq!(st.code(), Some(143), "{st:?} (signal {:?})", st.signal());
    assert!(
        contains(&s.out[before..], b"\x1b[?1049l"),
        "no leave-alternate-screen after SIGTERM: {:?}",
        String::from_utf8_lossy(&s.out[before..])
    );
}
