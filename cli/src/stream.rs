//! `conduit stream`: put a VM on the network for Moonlight clients and Conduit
//! viewers (host/stream, docs/STREAMING.md).
//!
//!   conduit stream NAME              stream it (starting it if needed) until Ctrl+C
//!   conduit stream NAME --service    the same as a systemd user service (restarts, starts at login)
//!   conduit stream NAME --stop       stop that service
//!   conduit stream pair PIN          enter the PIN a Moonlight client shows
//!   conduit stream clients | unpair CLIENT | status
//!
//! The stream host takes the VM's display socket, the one the local viewer
//! would use, so a VM is either viewed here or streamed.

use crate::mode::{self, Mode};
use crate::paths::{self, Tool};
use crate::run;
use crate::sys;
use crate::ui::{self, oops};
use crate::vm::VmConfig;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub struct Opts {
    pub preset: String,
    pub port: u16,
    pub display: Option<Mode>,
    pub service: bool,
    pub stop: bool,
    pub link: bool,
    pub video_encryption: bool,
    pub link_mbps: Option<u32>,
}

/// Words that are actions rather than VM names.
pub const ACTIONS: &[&str] = &["pair", "clients", "unpair", "status", "token"];

fn unit_name(vm: &str) -> String {
    format!("conduit-stream-{vm}.service")
}

fn unit_path(vm: &str) -> PathBuf {
    paths::config_dir()
        .parent()
        .map(|p| p.join("systemd/user"))
        .unwrap_or_else(|| paths::home().join(".config/systemd/user"))
        .join(unit_name(vm))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("could not run systemctl")?;
    if !st.success() {
        return Err(oops(
            format!("systemctl --user {} failed", args.join(" ")),
            "Is a systemd user session running? (`systemctl --user status`)",
        ));
    }
    Ok(())
}

/// Pass an action through to conduit-stream.
pub fn action(word: &str, rest: &[String]) -> Result<()> {
    let bin = Tool::Stream.require()?;
    let mut cmd = Command::new(bin);
    match word {
        "pair" => {
            let pin = rest.first().ok_or_else(|| {
                oops(
                    "which PIN?",
                    "Moonlight shows a 4-digit PIN when you add or select this computer: `conduit stream pair 1234`",
                )
            })?;
            cmd.args(["pair", pin]);
        }
        "unpair" => {
            let who = rest
                .first()
                .ok_or_else(|| oops("unpair which client?", "See `conduit stream clients`"))?;
            cmd.args(["unpair", who]);
        }
        "clients" | "status" | "token" => {
            cmd.arg(word);
        }
        _ => unreachable!(),
    }
    let st = cmd.status().context("could not run conduit-stream")?;
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
    Ok(())
}

fn install_service(name: &str, o: &Opts) -> Result<()> {
    let me = std::env::current_exe()?;
    let mut args = vec![
        "stream".to_string(),
        name.to_string(),
        "--keep-vm".into(),
        "--preset".into(),
        o.preset.clone(),
        "--port".into(),
        o.port.to_string(),
    ];
    if let Some(m) = o.display {
        args.push("--display".into());
        args.push(m.to_string());
    }
    if o.link {
        args.push("--link".into());
    }
    if o.video_encryption {
        args.push("--video-encryption".into());
    }
    if let Some(l) = o.link_mbps {
        args.push("--link-mbps".into());
        args.push(l.to_string());
    }
    let exec = std::iter::once(me.to_string_lossy().into_owned())
        .chain(args)
        .map(|a| ui::shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let unit = format!(
        "# Written by `conduit stream {name} --service`; `conduit stream {name} --stop` removes it.\n\
         [Unit]\n\
         Description=Conduit: stream the VM {name} (Moonlight / conduit viewer)\n\
         After=graphical-session.target network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    let p = unit_path(name);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, unit).with_context(|| format!("writing {}", p.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &unit_name(name)])?;
    ui::info(format!(
        "streaming {name} as a service ({}); it restarts if it fails and starts at login",
        unit_name(name)
    ));
    ui::info(format!(
        "logs: `journalctl --user -u {}`; stop: `conduit stream {name} --stop`",
        unit_name(name)
    ));
    Ok(())
}

fn remove_service(name: &str) -> Result<()> {
    let p = unit_path(name);
    if !p.exists() {
        return Err(oops(
            format!("{name} is not streamed as a service"),
            "A `conduit stream NAME` in a terminal stops with Ctrl+C",
        ));
    }
    let _ = systemctl(&["disable", "--now", &unit_name(name)]);
    std::fs::remove_file(&p)?;
    let _ = systemctl(&["daemon-reload"]);
    ui::info(format!(
        "stopped streaming {name}; the VM keeps running (`conduit down {name}` stops it)"
    ));
    Ok(())
}

/// The GameStream ports (and the link's) must be free before the VM boots.
fn ports_free(http: u16, link: bool) -> Result<()> {
    let mut tcp = vec![http, http.wrapping_sub(5), http.wrapping_add(21)];
    if link {
        tcp.push(48100);
    }
    let udp = [
        http.wrapping_add(9),
        http.wrapping_add(10),
        http.wrapping_add(11),
    ];
    let busy = tcp
        .iter()
        .filter(|&&p| std::net::TcpListener::bind(("0.0.0.0", p)).is_err())
        .map(|p| format!("TCP {p}"))
        .chain(
            udp.iter()
                .filter(|&&p| std::net::UdpSocket::bind(("0.0.0.0", p)).is_err())
                .map(|p| format!("UDP {p}")),
        )
        .collect::<Vec<_>>();
    if busy.is_empty() {
        return Ok(());
    }
    Err(oops(
        format!("the streaming ports are in use ({})", busy.join(", ")),
        "Another `conduit stream` (or Sunshine) runs; stop it, or use --port 48089 (then add the host in Moonlight as IP:48089)",
    ))
}

pub fn stream(name: &str, o: &Opts, keep_vm: bool) -> Result<()> {
    if o.stop {
        return remove_service(name);
    }
    let c = VmConfig::load(name)?;
    if o.service {
        Tool::Stream.require()?;
        return install_service(name, o);
    }
    let bin = Tool::Stream.require()?;
    let sock = paths::run_dir(name).join("display.sock");
    if sock.exists() && sys::socket_live(&sock) {
        return Err(oops(
            format!("{name}'s screen is already taken (a viewer window, or another stream)"),
            format!("Close the viewer window, or check `conduit stream status`; then run `conduit stream {name}` again"),
        ));
    }
    let _ = std::fs::remove_file(&sock);
    ports_free(o.port, o.link)?;

    // The VM: start it with a display if it is not running. Its guest
    // follows the client's resolution once a client connects.
    let started = if run::is_running(name) {
        false
    } else {
        let m = o.display.unwrap_or_else(|| mode::detect().0);
        run::up(name, Some(m), false, None)?;
        true
    };

    let mut cmd = Command::new(&bin);
    cmd.args(["serve", "--name", name, "--socket"])
        .arg(&sock)
        .args(["--preset", &o.preset, "--port", &o.port.to_string()]);
    if o.video_encryption {
        cmd.arg("--video-encryption");
    }
    if let Some(l) = o.link_mbps {
        cmd.args(["--link-mbps", &l.to_string()]);
    }
    if o.link {
        cmd.arg("--link");
    }
    ui::info(format!(
        "streaming {name}: in Moonlight add this computer{}, then `conduit stream pair PIN` with the PIN it shows. Ctrl+C stops.",
        if o.port == 47989 {
            String::new()
        } else {
            format!(" as IP:{}", o.port)
        }
    ));
    let _ = c;
    let mut child = cmd.spawn().context("could not run conduit-stream")?;
    // Ctrl+C reaches the stream host too (same terminal); we only wait for it,
    // then clean up.
    // SAFETY: changing our own SIGINT disposition after the child exists.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let st = child.wait()?;
    if started && !keep_vm {
        ui::info(format!("streaming ended; shutting {name} down"));
        let _ = run::down(name);
    }
    if !st.success() && st.code().is_some() {
        return Err(oops(
            "the stream host stopped with an error",
            "Its messages are above; `conduit doctor` checks the GPU and driver",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- conduit remote

pub struct RemoteOpts {
    pub host: String,
    pub token: Option<String>,
    pub lossless: bool,
    pub codec: Option<String>,
    pub bitrate: Option<String>,
    pub fps: Option<u32>,
    pub fullscreen: bool,
    pub yuv444: bool,
}

/// "300M", "80m", "50000k", "2G" → kbit/s.
pub fn parse_bitrate(s: &str) -> Result<u32> {
    let t = s.trim().to_ascii_lowercase();
    let t = t
        .trim_end_matches("bit/s")
        .trim_end_matches("bps")
        .trim_end_matches('b');
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1.0),
        Some('m') => (&t[..t.len() - 1], 1000.0),
        Some('g') => (&t[..t.len() - 1], 1_000_000.0),
        _ => (t, 1000.0), // a bare number is Mbit/s
    };
    let v: f64 = num.parse().map_err(|_| {
        oops(
            format!("\"{s}\" is not a bitrate"),
            "Write it like 300M or 2G",
        )
    })?;
    let k = (v * mult).round();
    if !(500.0..=10_000_000.0).contains(&k) {
        return Err(oops(
            format!("bitrate {s} is out of range"),
            "Between 0.5M and 10G",
        ));
    }
    Ok(k as u32)
}

fn tokens_path() -> PathBuf {
    paths::config_dir().join("remote-tokens")
}

fn remembered_token(host: &str) -> Option<String> {
    let s = std::fs::read_to_string(tokens_path()).ok()?;
    s.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(h, _)| *h == host)
        .map(|(_, t)| t.trim().to_string())
}

fn remember_token(host: &str, token: &str) {
    let p = tokens_path();
    let mut lines: Vec<String> = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split_once(' ').map(|(h, _)| h) != Some(host))
        .map(str::to_string)
        .collect();
    lines.push(format!("{host} {token}"));
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&p)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", lines.join("\n"));
    }
}

/// Show a VM streamed by another computer in the local viewer.
pub fn remote(o: &RemoteOpts) -> Result<()> {
    let viewer = Tool::Viewer.require()?;
    let bin = Tool::Stream.require()?;
    let token = match (&o.token, remembered_token(&o.host)) {
        (Some(t), _) => t.clone(),
        (None, Some(t)) => t,
        (None, None) => {
            return Err(oops(
                format!("no link token for {}", o.host),
                format!(
                    "On {} run `conduit stream token`, then `conduit remote {} --token TOKEN` (remembered after that)",
                    o.host, o.host
                ),
            ))
        }
    };
    let safe: String = o
        .host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = paths::xdg_runtime().join("conduit");
    std::fs::create_dir_all(&dir)?;
    let sock = dir.join(format!("remote-{safe}.sock"));
    if sock.exists() && sys::socket_live(&sock) {
        return Err(oops(
            format!("a remote window for {} is already open", o.host),
            "Look for its window, or close it first",
        ));
    }
    let _ = std::fs::remove_file(&sock);
    let size = mode::detect().0;
    let mut v = Command::new(&viewer);
    let session = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else {
        "x11"
    };
    v.args(["--backend", session, "--socket"]).arg(&sock).args([
        "--size",
        &size.size(),
        "--title",
        &format!("{} - Conduit (remote)", o.host),
        "--present-mode=native",
        "--scale",
        "aspect",
        "--stats",
    ]);
    if o.fullscreen {
        v.arg("--fullscreen");
    }
    let log = paths::cache_dir().join(format!("remote-{safe}-viewer.log"));
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let vpid = sys::spawn_detached(&mut v, &log, false)? as i32;
    if !sys::wait_for(std::time::Duration::from_secs(5), || {
        sock.exists() || !sys::alive(vpid)
    }) || !sys::alive(vpid)
    {
        return Err(oops(
            "the viewer window could not open",
            format!(
                "Its log ({}) ends with:\n{}",
                log.display(),
                sys::tail(&log, 6)
            ),
        ));
    }
    let mut c = Command::new(&bin);
    c.args(["connect", &o.host, "--token", &token, "--socket"])
        .arg(&sock);
    if o.lossless {
        c.arg("--lossless");
    }
    if o.yuv444 {
        c.arg("--yuv444");
    }
    if let Some(codec) = &o.codec {
        c.args(["--codec", codec]);
    }
    if let Some(b) = &o.bitrate {
        c.args(["--bitrate-kbps", &parse_bitrate(b)?.to_string()]);
    }
    if let Some(f) = o.fps {
        c.args(["--fps", &f.to_string()]);
    }
    ui::info(format!(
        "connecting to {}{}; close the window to disconnect",
        o.host,
        if o.lossless { " (lossless)" } else { "" }
    ));
    let mut child = c.spawn().context("could not run conduit-stream")?;
    // SAFETY: our own SIGINT disposition, after the child exists.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let st = child.wait()?;
    // SAFETY: plain kill of the viewer we started.
    unsafe { libc::kill(vpid, libc::SIGTERM) };
    if st.success() {
        remember_token(&o.host, &token);
        Ok(())
    } else {
        Err(oops(
            format!("the link to {} ended with an error", o.host),
            "Its message is above. Is `conduit stream NAME --link` running there?",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrates() {
        assert_eq!(parse_bitrate("300M").unwrap(), 300_000);
        assert_eq!(parse_bitrate("2G").unwrap(), 2_000_000);
        assert_eq!(parse_bitrate("50000k").unwrap(), 50_000);
        assert_eq!(parse_bitrate("80").unwrap(), 80_000);
        assert_eq!(parse_bitrate("1.5Mbit/s").unwrap(), 1_500);
        assert!(parse_bitrate("fast").is_err());
        assert!(parse_bitrate("100k").is_err());
    }
}
