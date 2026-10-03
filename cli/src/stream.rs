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
