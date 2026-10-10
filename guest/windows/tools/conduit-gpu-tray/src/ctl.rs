//! The host<->guest control channel on Windows: the `org.conduit.ctl.0`
//! virtio-serial port, one JSON request per line in, one reply line out.
//! The request handling is `gpu_tray::ctl_logic`; this is the port I/O.
//!
//! The reader blocks on overlapped I/O while a host client is connected;
//! each request runs on its own short-lived thread so a slow op never holds
//! up the others, and replies are written one at a time under a lock.
//! Nothing here runs until the port exists: before that a slow retry loop is
//! all there is.
//!
//! The VirtIO serial driver fails reads and writes with
//! `ERROR_NO_SYSTEM_RESOURCES` while no host client is connected to the
//! port's socket (the CLI connects per request). That is not an error of the
//! port: it stays open, so the host sees the guest end as connected, and the
//! reader asks again shortly. Closing it instead would drop whatever the
//! host sends while the port is closed.

use crate::apps::{self, ComGuard};
use crate::launch;
use conduit_ctl::{LineReader, Request, Response, RunArgs, CHANNEL};
use gpu_tray::ctl_logic::{Agent, Backend};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::UI::Shell::FOLDERID_Downloads;
use windows_sys::Win32::UI::WindowsAndMessaging::GetShellWindow;

/// Requests running at once; more are answered "busy".
const MAX_INFLIGHT: usize = 8;
/// How often the reader asks again while no host client is connected.
const HOST_POLL: Duration = Duration::from_millis(100);
/// A reply that cannot be written for this long is abandoned.
const WRITE_TIMEOUT_MS: u32 = 30_000;

struct WinBackend;

impl Backend for WinBackend {
    fn user(&self) -> String {
        let mut buf = [0u16; 257];
        let mut n = buf.len() as u32;
        if unsafe { GetUserNameW(buf.as_mut_ptr(), &mut n) } == 0 || n == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())])
    }

    fn session(&self) -> bool {
        !unsafe { GetShellWindow() }.is_null()
    }

    fn downloads(&self) -> Option<PathBuf> {
        let _com = ComGuard::new();
        apps::known_folder(&FOLDERID_Downloads)
    }

    fn drives(&self) -> Vec<String> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:\\", (b'A' + i) as char))
            .collect()
    }

    fn run(&self, a: &RunArgs) -> Result<u32, String> {
        launch::run(a)
    }

    fn stop(&self, pid: u32) -> Result<(), String> {
        launch::stop(pid)
    }

    fn apps(&self) -> Result<Vec<conduit_ctl::App>, String> {
        apps::list()
    }

    fn icon(&self, key: &str) -> Result<Vec<u8>, String> {
        apps::icon(key)
    }

    fn as_user<R>(&self, f: impl FnOnce() -> R) -> Result<R, String> {
        launch::as_user(f)
    }
}

/// A manual-reset event.
struct Event(HANDLE);

impl Event {
    fn new() -> Option<Event> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        (!h.is_null()).then_some(Event(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// One open port. Closed when the reader and every running request are done.
struct Conn {
    h: HANDLE,
    /// Serializes replies; holds the write event.
    writer: Mutex<Event>,
    broken: AtomicBool,
}

unsafe impl Send for Conn {}
unsafe impl Sync for Conn {}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

impl Conn {
    fn open() -> Result<Conn, u32> {
        let path = crate::gfx::wide(&format!(r"\\.\Global\{CHANNEL}"));
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(unsafe { GetLastError() });
        }
        match Event::new() {
            Some(ev) => Ok(Conn {
                h,
                writer: Mutex::new(ev),
                broken: AtomicBool::new(false),
            }),
            None => {
                unsafe { CloseHandle(h) };
                Err(ERROR_NOT_ENOUGH_MEMORY)
            }
        }
    }

    /// One overlapped read or write; `Err` is a Win32 error code.
    fn io(&self, ev: &Event, write: bool, buf: *mut u8, len: u32) -> Result<u32, u32> {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev.0;
        unsafe { ResetEvent(ev.0) };
        let ok = unsafe {
            if write {
                WriteFile(self.h, buf, len, null_mut(), &mut ov)
            } else {
                ReadFile(self.h, buf, len, null_mut(), &mut ov)
            }
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                return Err(e);
            }
            if write && unsafe { WaitForSingleObject(ev.0, WRITE_TIMEOUT_MS) } == WAIT_TIMEOUT {
                // The host is not reading: give up, and wait out the cancel
                // so `ov` and the buffer stay valid until the driver is done.
                unsafe { CancelIoEx(self.h, &ov) };
                let mut n = 0u32;
                unsafe { GetOverlappedResult(self.h, &ov, &mut n, 1) };
                return Err(WAIT_TIMEOUT);
            }
        }
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.h, &ov, &mut n, 1) } == 0 {
            return Err(unsafe { GetLastError() });
        }
        Ok(n)
    }

    fn read(&self, ev: &Event, buf: &mut [u8]) -> Result<u32, u32> {
        self.io(ev, false, buf.as_mut_ptr(), buf.len() as u32)
    }

    /// Writes the whole line; false when the port is gone.
    fn write_line(&self, line: &str) -> bool {
        let ev = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if self.broken.load(Relaxed) {
            return false;
        }
        let mut data = line.as_bytes();
        while !data.is_empty() {
            let chunk = data.len().min(1 << 20);
            match self.io(&ev, true, data.as_ptr() as *mut u8, chunk as u32) {
                Ok(n) if n > 0 => data = &data[(n as usize).min(data.len())..],
                Err(ERROR_NO_SYSTEM_RESOURCES) => {
                    // The host client left before its reply: drop the reply.
                    return false;
                }
                _ => {
                    // The port is unusable: end the reader too (its read
                    // completes as cancelled), so `serve` opens it afresh
                    // instead of listening on a port that never replies.
                    self.broken.store(true, Relaxed);
                    unsafe { CancelIoEx(self.h, null()) };
                    return false;
                }
            }
        }
        true
    }
}

struct Inflight<'a>(&'a AtomicUsize);

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Reads requests until the port fails; true when it carried any.
fn connection(agent: &Arc<Agent<WinBackend>>, conn: Arc<Conn>, ended: &mut u32) -> bool {
    let Some(ev) = Event::new() else {
        return false;
    };
    let mut lines = LineReader::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut any = false;
    loop {
        let n = match conn.read(&ev, &mut buf) {
            Ok(n) if n > 0 => n as usize,
            Ok(_) => break,
            Err(ERROR_NO_SYSTEM_RESOURCES) => {
                // No host client right now. Whatever half-line the last one
                // left must not run into the next one's first request.
                lines = LineReader::default();
                std::thread::sleep(HOST_POLL);
                continue;
            }
            Err(e) => {
                *ended = e;
                break;
            }
        };
        any = true;
        for line in lines.push(&buf[..n]) {
            if INFLIGHT.fetch_add(1, Relaxed) >= MAX_INFLIGHT {
                INFLIGHT.fetch_sub(1, Relaxed);
                let id = match Request::parse(&line) {
                    Ok(r) => r.id,
                    Err((id, _)) => id.unwrap_or(0),
                };
                conn.write_line(&Response::err(id, "busy").to_line());
                continue;
            }
            let (a, c) = (agent.clone(), conn.clone());
            let spawned = std::thread::Builder::new()
                .name("ctl-request".into())
                .spawn(move || {
                    let _slot = Inflight(&INFLIGHT);
                    let reply = a.handle_line(&line);
                    c.write_line(&reply);
                });
            if spawned.is_err() {
                INFLIGHT.fetch_sub(1, Relaxed);
            }
        }
    }
    any
}

/// Appends one line to `%ProgramData%\Conduit\ctl.log` (restarted when it
/// passes 64 KiB). Only state changes are logged, never requests.
pub(crate) fn log(msg: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("ProgramData") else {
        return;
    };
    let path = PathBuf::from(dir).join("Conduit").join("ctl.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 64 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {msg}", stamp());
    }
}

/// Seconds since the tray started, for the log.
fn stamp() -> String {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    format!(
        "[+{:.1}s]",
        START.get_or_init(Instant::now).elapsed().as_secs_f32()
    )
}

/// Serves the channel for good: the port may not exist yet, may be taken,
/// may go away (the host side reconnecting, the VM resuming); start over
/// with a growing pause, up to 15 seconds.
pub fn serve() {
    let agent = Arc::new(Agent::new(
        WinBackend,
        format!("conduit-tray {}", env!("CARGO_PKG_VERSION")),
    ));
    stamp();
    let mut pause = Duration::from_secs(1);
    let mut last_open_err = 0u32;
    loop {
        match Conn::open() {
            Ok(conn) => {
                log("port opened");
                last_open_err = 0;
                let t0 = Instant::now();
                let mut ended = 0u32;
                let any = connection(&agent, Arc::new(conn), &mut ended);
                log(&format!(
                    "port closed after {:.1}s (carried requests: {any}, read error {ended})",
                    t0.elapsed().as_secs_f32()
                ));
                if any || t0.elapsed() > Duration::from_secs(10) {
                    pause = Duration::from_secs(1);
                }
            }
            Err(e) => {
                if e != last_open_err {
                    log(&format!("opening the port failed: error {e}"));
                    last_open_err = e;
                }
            }
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(15));
    }
}
