//! Request tracing: where records go once a request has been served.
//!
//! The serving thread does as little as possible. It checks [`enabled`] (one
//! relaxed atomic load) once per batch of requests; when that is false
//! nothing else here runs and no record is made. When it is true, each
//! request costs two clock reads around it, two around each host ioctl, and a
//! [`emit`] -- a `try_send` into a bounded lock-free queue that never blocks
//! and never makes a syscall. A full queue drops the record and counts it.
//!
//! One writer thread drains the queue every couple of milliseconds and does
//! the encoding and the I/O: to the `--trace` file, and to every reader
//! connected to the control socket (`conduit trace NAME`). Tracing is on
//! while either has somewhere to write; the writer turns [`enabled`] on and
//! off as readers come and go.
//!
//! The file is opened by the caller before the sandbox goes on, because the
//! sandbox lets the backend create no file. The control socket is bound in
//! the directory the sandbox allows sockets in. See docs/TRACING.md.

use conduit_trace::read::{Format, Writer};
use conduit_trace::{DriverVersion, Header, Record};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, channel, sync_channel};
use std::time::Duration;

pub use conduit_trace as format;

static ENABLED: AtomicBool = AtomicBool::new(false);
static QUEUE: OnceLock<SyncSender<Record>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// The host driver release, packed, for the trace header; 0 until known.
static DRIVER: AtomicU64 = AtomicU64::new(0);
/// Set by SIGUSR1: flip the trace file on or off.
static FILE_TOGGLE: AtomicBool = AtomicBool::new(false);
/// Set by the control socket: 1 = file on, 2 = file off.
static FILE_REQUEST: AtomicU8 = AtomicU8::new(0);
/// For `status`: the writer's view of things.
static FILE_ON: AtomicBool = AtomicBool::new(false);
static HAS_FILE: AtomicBool = AtomicBool::new(false);
static READERS: AtomicUsize = AtomicUsize::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static DROPPED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Records the queue holds before the next one is dropped: about 4.7 MB,
/// a few hundred milliseconds of the busiest workload measured.
const QUEUE_LEN: usize = 1 << 16;
/// Most records the writer takes per pass.
const BATCH: usize = 4096;

/// Whether requests are being traced. The one check the serving path makes.
#[inline(always)]
pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// `CLOCK_MONOTONIC` in nanoseconds (a vDSO call, no syscall).
#[inline]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid clock id and a live timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Hand a finished record to the writer. Never blocks.
#[inline]
pub fn emit(r: Record) {
    if let Some(q) = QUEUE.get()
        && q.try_send(r).is_err()
    {
        DROPPED.fetch_add(1, Relaxed);
    }
}

/// The host driver release, for the header readers get (NVKMS command names
/// depend on it). Call before [`start`].
pub fn set_driver(v: DriverVersion) {
    DRIVER.store(
        (v.major as u64) << 42 | (v.minor as u64 & 0x1f_ffff) << 21 | (v.patch as u64 & 0x1f_ffff),
        Relaxed,
    );
}

fn header() -> Header {
    let p = DRIVER.load(Relaxed);
    Header {
        driver: (p != 0).then(|| {
            DriverVersion::new(
                (p >> 42) as u32,
                (p >> 21 & 0x1f_ffff) as u32,
                (p & 0x1f_ffff) as u32,
            )
        }),
    }
}

/// Where traces go.
#[derive(Default)]
pub struct Options {
    /// An open file and the format to write it in (`--trace PATH`). Tracing
    /// starts on at once; SIGUSR1 or `file off` on the socket pauses it.
    pub file: Option<(std::fs::File, Format)>,
    /// The control socket `conduit trace` connects to.
    pub socket: Option<PathBuf>,
}

type Sink = Writer<BufWriter<Box<dyn Write + Send>>>;

/// Start the writer thread, and the control socket's if one is asked for.
/// With neither a file nor a socket, does nothing and tracing stays off.
pub fn start(opts: Options) -> io::Result<()> {
    if opts.file.is_none() && opts.socket.is_none() {
        return Ok(());
    }
    let (tx, rx) = sync_channel(QUEUE_LEN);
    if QUEUE.set(tx).is_err() {
        return Err(io::Error::other("tracing was already started"));
    }
    let (sub_tx, sub_rx) = channel::<Sink>();

    if let Some(path) = &opts.socket {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)
            .map_err(|e| io::Error::other(format!("trace socket {}: {e}", path.display())))?;
        std::thread::Builder::new()
            .name("trace-control".into())
            .spawn(move || control(listener, sub_tx))?;
        log::info!("trace: control socket {}", path.display());
    }

    let file = match opts.file {
        Some((f, format)) => {
            let w: Box<dyn Write + Send> = Box::new(f);
            HAS_FILE.store(true, Relaxed);
            FILE_ON.store(true, Relaxed);
            Some(Writer::new(
                BufWriter::with_capacity(1 << 16, w),
                format,
                header(),
            )?)
        }
        None => None,
    };
    install_sigusr1();
    std::thread::Builder::new()
        .name("trace-writer".into())
        .spawn(move || writer(rx, sub_rx, file))?;
    Ok(())
}

extern "C" fn on_sigusr1(_: libc::c_int) {
    FILE_TOGGLE.store(true, Relaxed);
}

fn install_sigusr1() {
    // SAFETY: a zeroed sigaction with a handler that only stores to an atomic.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigusr1 as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) != 0 {
            log::warn!(
                "trace: SIGUSR1 handler: {}; the file can still be paused from the control socket",
                io::Error::last_os_error()
            );
        }
    }
}

fn writer(rx: Receiver<Record>, subs_rx: Receiver<Sink>, mut file: Option<Sink>) {
    let mut file_on = file.is_some();
    let mut subs: Vec<Sink> = Vec::new();
    let mut batch: Vec<Record> = Vec::with_capacity(BATCH);
    loop {
        while let Ok(s) = subs_rx.try_recv() {
            subs.push(s);
        }
        let mut want = None;
        if FILE_TOGGLE.swap(false, Relaxed) {
            want = Some(!file_on);
        }
        match FILE_REQUEST.swap(0, Relaxed) {
            1 => want = Some(true),
            2 => want = Some(false),
            _ => {}
        }
        if let Some(on) = want
            && file.is_some()
            && on != file_on
        {
            file_on = on;
            log::info!("trace: file {}", if on { "resumed" } else { "paused" });
        }
        FILE_ON.store(file_on && file.is_some(), Relaxed);
        READERS.store(subs.len(), Relaxed);

        let active = (file_on && file.is_some()) || !subs.is_empty();
        if ENABLED.swap(active, Relaxed) != active {
            log::info!("trace: {}", if active { "on" } else { "off" });
        }

        batch.clear();
        loop {
            match rx.try_recv() {
                Ok(r) => {
                    batch.push(r);
                    if batch.len() == BATCH {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let lost = DROPPED.swap(0, Relaxed);
        if lost > 0 {
            DROPPED_TOTAL.fetch_add(lost, Relaxed);
            batch.push(Record::dropped(now_ns(), lost));
        }

        // Records made just before tracing went off have nowhere to go.
        if !batch.is_empty() {
            if file_on && let Some(f) = file.as_mut() {
                let ok = batch
                    .iter()
                    .try_for_each(|r| f.write(r))
                    .and_then(|()| f.flush());
                if let Err(e) = ok {
                    log::warn!("trace: writing the trace file: {e}; tracing to it stops");
                    file = None;
                }
            }
            subs.retain_mut(|s| {
                batch
                    .iter()
                    .try_for_each(|r| s.write(r))
                    .and_then(|()| s.flush())
                    .is_ok()
            });
            WRITTEN.fetch_add(batch.len() as u64, Relaxed);
        }

        if batch.len() < BATCH {
            // Not waiting on the queue is what keeps `emit` free of syscalls:
            // with no receiver parked, a send never has anyone to wake.
            std::thread::sleep(if active {
                Duration::from_millis(2)
            } else {
                Duration::from_millis(100)
            });
        }
    }
}

/// The control socket: one command per connection.
///
/// * `stream` or `stream bin` -- a header, then binary records until the
///   reader hangs up. `stream json` -- the same as JSON Lines.
/// * `status` -- one line saying what is being traced.
/// * `file on` / `file off` -- resume or pause the `--trace` file.
fn control(listener: UnixListener, subs: Sender<Sink>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if let Err(e) = serve_control(conn, &subs) {
            log::debug!("trace: control connection: {e}");
        }
    }
}

fn serve_control(conn: UnixStream, subs: &Sender<Sink>) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(io::Read::take(conn.try_clone()?, 256)).read_line(&mut line)?;
    let reply = |s: String| (&conn).write_all(s.as_bytes());
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["stream"] | ["stream", "bin" | "binary"] | ["stream", "json"] => {
            let format = if line.contains("json") {
                Format::Json
            } else {
                Format::Binary
            };
            // A reader that stops reading is dropped rather than allowed to
            // stall the writer, and with it every other reader.
            let out = conn.try_clone()?;
            out.set_write_timeout(Some(Duration::from_secs(1)))?;
            let w: Box<dyn Write + Send> = Box::new(out);
            let sink = Writer::new(BufWriter::with_capacity(1 << 16, w), format, header())?;
            let _ = subs.send(sink);
            log::info!("trace: a reader connected ({format:?})");
            Ok(())
        }
        ["status"] => reply(format!(
            "tracing {}; file {}; {} live reader(s); {} record(s) written, {} dropped\n",
            if enabled() { "on" } else { "off" },
            match (HAS_FILE.load(Relaxed), FILE_ON.load(Relaxed)) {
                (false, _) => "none",
                (true, true) => "on",
                (true, false) => "paused",
            },
            READERS.load(Relaxed),
            WRITTEN.load(Relaxed),
            DROPPED_TOTAL.load(Relaxed),
        )),
        ["file", on @ ("on" | "off")] => {
            if !HAS_FILE.load(Relaxed) {
                return reply("no trace file: start the backend with --trace PATH\n".into());
            }
            FILE_REQUEST.store(if *on == "on" { 1 } else { 2 }, Relaxed);
            reply("ok\n".into())
        }
        _ => reply(format!("unknown command {:?}\n", line.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_survives_packing() {
        let v = DriverVersion::new(580, 178, 4);
        set_driver(v);
        assert_eq!(header().driver, Some(v));
    }

    #[test]
    fn the_clock_moves_forward() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a && a > 0);
    }
}
