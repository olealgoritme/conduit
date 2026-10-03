//! The local control socket: `conduit-stream pair PIN`, `status`.
//!
//! `$XDG_RUNTIME_DIR/conduit-stream/NAME.sock`, owner only (0600, and the
//! peer's uid is checked). One line in, one line out.

use crate::gamestream::nvhttp;
use crate::host::Host;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        // SAFETY: getuid cannot fail.
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/conduit-{}", unsafe { libc::getuid() })));
    base.join("conduit-stream")
}

pub fn sock_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.sock"))
}

pub fn serve(host: Arc<Host>, name: &str) -> Result<()> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    let p = sock_path(name);
    let _ = std::fs::remove_file(&p);
    let l = UnixListener::bind(&p).with_context(|| format!("binding {}", p.display()))?;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let host = host.clone();
            std::thread::spawn(move || {
                let _ = handle(&host, c);
            });
        }
    });
    Ok(())
}

fn handle(host: &Host, c: UnixStream) -> Result<()> {
    c.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(&c).read_line(&mut line)?;
    let mut w = &c;
    let mut words = line.split_whitespace();
    match words.next() {
        Some("pair") => {
            let pin = words.next().unwrap_or("");
            if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
                writeln!(w, "error the PIN is 4 digits")?;
                return Ok(());
            }
            match nvhttp::enter_pin(host, pin) {
                Some(dev) => writeln!(w, "ok {dev}")?,
                None => writeln!(w, "none")?,
            }
        }
        Some("pending") => writeln!(w, "{}", nvhttp::pending_pairs(host).join(","))?,
        Some("status") => {
            let s = host.current_session();
            let v = serde_json::json!({
                "hostname": host.hostname,
                "app": host.app_name,
                "port": host.ports.http,
                "backend_connected": host.broker.connected(),
                "session": s.as_ref().map(|s| serde_json::json!({
                    "client": s.launch.client_name,
                    "mode": format!("{}x{}@{}", s.cfg.width, s.cfg.height, s.cfg.fps),
                    "codec": s.cfg.codec.name(),
                    "bitrate_kbps": s.cfg.bitrate_kbps,
                    "frames": s.frames_sent.load(std::sync::atomic::Ordering::Relaxed),
                    "seconds": s.started.elapsed().as_secs(),
                })),
                "pending_pairs": nvhttp::pending_pairs(host),
            });
            writeln!(w, "{v}")?;
        }
        _ => writeln!(w, "error unknown command")?,
    }
    Ok(())
}

fn ask(path: &PathBuf, cmd: &str) -> Result<String> {
    let mut s = UnixStream::connect(path).with_context(|| format!("{}", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(s, "{cmd}")?;
    let mut out = String::new();
    BufReader::new(&s).read_line(&mut out)?;
    Ok(out.trim().to_string())
}

pub fn running() -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sock") && UnixStream::connect(&p).is_ok() {
                let n = p.file_stem().unwrap().to_string_lossy().into_owned();
                v.push((n, p));
            }
        }
    }
    v.sort();
    v
}

/// Give `pin` to whichever running stream host has a client waiting,
/// waiting up to `wait` for one to ask.
pub fn pair(pin: &str, wait: Duration) -> Result<String> {
    let deadline = Instant::now() + wait;
    loop {
        let hosts = running();
        if hosts.is_empty() {
            bail!("no stream host is running (start one with `conduit stream NAME`)");
        }
        for (name, p) in &hosts {
            let r = ask(p, &format!("pair {pin}"))?;
            if let Some(dev) = r.strip_prefix("ok ") {
                return Ok(format!("{dev} (via {name})"));
            }
            if let Some(e) = r.strip_prefix("error ") {
                bail!("{e}");
            }
        }
        if Instant::now() >= deadline {
            bail!("no Moonlight client is waiting to pair; in Moonlight, add or select this host first, then enter the PIN it shows");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn status() -> Result<Vec<String>> {
    running().iter().map(|(_, p)| ask(p, "status")).collect()
}
