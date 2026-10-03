//! `conduit trace`: record, watch and summarise the GPU requests a VM makes.
//!
//! The VM's GPU backend listens on a control socket in the VM's runtime
//! folder (trace.sock). Connecting to it turns tracing on; the backend
//! streams binary records for as long as someone is reading, and turns
//! tracing off again when the last reader goes. See docs/TRACING.md.

use crate::paths;
use crate::ui::oops;
use anyhow::{Context, Result};
use conduit_trace::filter::Filter;
use conduit_trace::read::{Format, Reader, Writer};
use conduit_trace::summary::Summary;
use conduit_trace::{Header, pretty};
use std::io::{BufReader, BufWriter, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// What to do with the records of a live trace.
pub struct Opts {
    pub follow: bool,
    pub summary: bool,
    pub output: Option<PathBuf>,
    pub format: Option<String>,
    pub filter: Vec<String>,
    pub duration: Option<u64>,
}

pub fn socket(name: &str) -> PathBuf {
    paths::run_dir(name).join("trace.sock")
}

fn connect(name: &str) -> Result<UnixStream> {
    let sock = socket(name);
    UnixStream::connect(&sock).map_err(|e| {
        oops(
            format!("cannot reach {name}'s GPU backend for tracing ({e})"),
            format!(
                "Is {name} running? Start it with `conduit up {name}`. A VM started by an \
                 older conduit has no trace socket ({}); restart it.",
                sock.display()
            ),
        )
    })
}

/// One command, one line of answer.
fn command(name: &str, cmd: &str) -> Result<String> {
    let mut s = connect(name)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(s, "{cmd}")?;
    let mut out = String::new();
    std::io::Read::read_to_string(&mut s, &mut out)?;
    Ok(out.trim_end().to_string())
}

/// `conduit trace NAME status | on | off`.
pub fn action(name: &str, what: &str) -> Result<()> {
    let reply = match what {
        "status" => command(name, "status")?,
        "on" => command(name, "file on")?,
        "off" => command(name, "file off")?,
        _ => unreachable!("checked by the caller"),
    };
    println!("{reply}");
    if reply.starts_with("no trace file") {
        std::process::exit(1);
    }
    Ok(())
}

/// The socket being read, so Ctrl-C can end the stream cleanly: shutting it
/// down makes the read loop see the end of the stream and print what it has.
static STREAM_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_interrupt(_: libc::c_int) {
    let fd = STREAM_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: shutdown(2) is async-signal-safe; the fd is ours.
        unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
    }
}

fn stop_on_interrupt(fd: i32) {
    STREAM_FD.store(fd, Ordering::Relaxed);
    // SAFETY: a handler that only reads an atomic and calls shutdown(2).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn parse_filter(filter: &[String]) -> Result<Filter> {
    Filter::parse(&filter.join(",")).map_err(|e| oops(e, ""))
}

fn default_output(name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("conduit-trace-{name}-{now}.jsonl"))
}

/// `conduit trace NAME [--follow] [--summary] [-o FILE]`: stream records from
/// the running VM until Ctrl-C (or --duration).
pub fn live(name: &str, o: Opts) -> Result<()> {
    let filter = parse_filter(&o.filter)?;
    // With nothing else asked for, record to a file.
    let output = match (&o.output, o.follow || o.summary) {
        (Some(p), _) => Some(p.clone()),
        (None, false) => Some(default_output(name)),
        (None, true) => None,
    };
    let format = match &o.format {
        Some(f) => Format::parse(f)
            .ok_or_else(|| oops(format!("unknown trace format {f:?}"), "Use json or bin"))?,
        None => output
            .as_deref()
            .map(Format::for_path)
            .unwrap_or(Format::Json),
    };

    let mut s = connect(name)?;
    writeln!(s, "stream bin")?;
    stop_on_interrupt(s.as_raw_fd());
    if let Some(secs) = o.duration {
        let fd = s.as_raw_fd();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            // SAFETY: shutting down our own socket; the reader sees the end.
            unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
        });
    }
    let mut reader = Reader::new(BufReader::with_capacity(1 << 16, &s))
        .with_context(|| format!("reading {name}'s trace stream"))?;
    let header = reader.header();

    let mut file = match &output {
        Some(p) => {
            let f = std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?;
            Some(Writer::new(BufWriter::new(f), format, header)?)
        }
        None => None,
    };
    let what = match (&output, o.follow) {
        (Some(p), _) => format!("recording to {}", p.display()),
        (None, true) => "following".to_string(),
        (None, false) => "collecting".to_string(),
    };
    let until = match o.duration {
        Some(d) => format!("for {d}s"),
        None => "until Ctrl-C".to_string(),
    };
    eprintln!("conduit: tracing {name}: {what}, {until}");

    let color = color();
    let mut summary = Summary::new(header.driver);
    let mut t0 = None;
    let mut written = 0u64;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for rec in reader.by_ref() {
        // A read error here is the stream ending; what was read stands.
        let Ok(rec) = rec else { break };
        if !filter.matches(&rec) {
            continue;
        }
        if let Some(f) = file.as_mut() {
            f.write(&rec)?;
            written += 1;
        }
        if o.summary {
            summary.add(&rec);
        }
        if o.follow {
            let t = *t0.get_or_insert(rec.ts_ns);
            if writeln!(out, "{}", pretty::line(&rec, t, header.driver, color)).is_err() {
                break; // stdout closed (| head)
            }
        }
    }
    drop(out);
    if let Some(mut f) = file {
        f.flush()?;
        if let Some(p) = &output {
            eprintln!(
                "conduit: {written} records in {}; `conduit trace analyze {}` summarises them",
                p.display(),
                p.display()
            );
        }
    }
    if o.summary {
        print!("{}", summary.render());
    }
    Ok(())
}

/// `conduit trace analyze FILE`: the summary of a recorded trace, or its
/// records one per line with --follow.
pub fn analyze(path: &Path, filter: &[String], lines: bool) -> Result<()> {
    let filter = parse_filter(filter)?;
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = Reader::new(BufReader::with_capacity(1 << 16, f))
        .with_context(|| format!("reading {}", path.display()))?;
    let Header { driver } = reader.header();
    let mut summary = Summary::new(driver);
    let color = color();
    let mut t0 = None;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for rec in reader.by_ref() {
        let rec = rec.with_context(|| format!("reading {}", path.display()))?;
        if !filter.matches(&rec) {
            continue;
        }
        if lines {
            let t = *t0.get_or_insert(rec.ts_ns);
            if writeln!(out, "{}", pretty::line(&rec, t, driver, color)).is_err() {
                return Ok(());
            }
        } else {
            summary.add(&rec);
        }
    }
    if !lines {
        write!(out, "{}", summary.render())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use conduit_trace::{Call, Kind, Record};

    fn sample_file(dir: &Path, format: Format) -> PathBuf {
        let p = dir.join(match format {
            Format::Json => "t.jsonl",
            Format::Binary => "t.bin",
        });
        let f = std::fs::File::create(&p).unwrap();
        let mut w = Writer::new(f, format, Header::default()).unwrap();
        for i in 0..10u64 {
            w.write(&Record {
                ts_ns: i * 1000,
                kind: Kind::Ioctl,
                call: if i < 7 { Call::Control } else { Call::Alloc },
                sub: Some(0x2080_0102),
                reply_ns: 10_000 + i,
                ..Default::default()
            })
            .unwrap();
        }
        p
    }

    #[test]
    fn analyze_reads_both_formats() {
        let dir = std::env::temp_dir().join(format!("conduit-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for format in [Format::Json, Format::Binary] {
            let p = sample_file(&dir, format);
            analyze(&p, &[], false).unwrap();
            analyze(&p, &["control".into(), "slow:>5us".into()], true).unwrap();
            assert!(analyze(&p, &["nonsense".into()], false).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_socket_lives_in_the_runtime_folder() {
        assert!(socket("lab").ends_with("conduit/lab/trace.sock"));
    }
}
