//! The host end of the guest control channel (`org.conduit.ctl.0`, protocol
//! in `host/ctl`): a client for `conduit run`, `apps`, `cp`, `app` and the
//! dashboard's Apps tab.
//!
//! QEMU binds `ctl.sock` and takes one connection at a time, so a client
//! holds an advisory lock (`ctl.lock` in the VM's runtime folder) for as long
//! as it is connected, connects, and every request waits for its response
//! with a timeout. Nothing here keeps a connection between commands: a stale
//! or half-written exchange cannot outlive the command that made it.

use crate::paths;
use crate::sys;
use crate::ui::{self, oops};
use crate::units;
use crate::virt::{self, Link};
use anyhow::Result;
use conduit_ctl::{
    b64_decode, b64_encode, App, Apps, Chunk, GetArgs, Icon, Listing, Op, Pong, PutArgs, Request,
    Response, RunArgs, Sha256, Started, Written, CHUNK, MAX_FILE,
};
use serde::de::DeserializeOwned;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long each kind of request may take.
pub const T_PING: Duration = Duration::from_secs(5);
pub const T_RUN: Duration = Duration::from_secs(20);
pub const T_APPS: Duration = Duration::from_secs(45);
pub const T_ICON: Duration = Duration::from_secs(20);
pub const T_FILE: Duration = Duration::from_secs(30);

/// How long a client waits for another client of the same VM to finish.
const LOCK_WAIT: Duration = Duration::from_secs(20);

/// Why a request failed.
#[derive(Debug, PartialEq)]
pub enum CtlError {
    /// The guest did not answer in time.
    Timeout(Duration),
    /// The channel closed: the guest agent went away, or the VM stopped.
    Closed,
    /// The guest answered with an error.
    Guest(String),
    /// The guest's answer made no sense, or a local file failed.
    Failed(String),
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlError::Timeout(d) => write!(
                f,
                "the guest did not answer within {} s",
                d.as_secs().max(1)
            ),
            CtlError::Closed => f.write_str("the guest closed the control channel"),
            CtlError::Guest(e) | CtlError::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for CtlError {}

type R<T> = std::result::Result<T, CtlError>;

fn io_err(e: std::io::Error) -> CtlError {
    use std::io::ErrorKind::*;
    match e.kind() {
        WouldBlock | TimedOut => CtlError::Timeout(Duration::ZERO),
        BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof => CtlError::Closed,
        _ => CtlError::Failed(e.to_string()),
    }
}

/// The hint that goes with a failure to get an answer from the agent.
const AGENT_HINT: &str = "The guest agent must be running in the VM: on Windows the Conduit GPU tray app (check its tray icon), on Linux `systemctl --user status conduit-ctl-agent`.\nThe VM must have been restarted after `conduit attach` added the channel.";

/// A friendly error for the command line.
pub fn friendly(vm: &str, e: CtlError) -> anyhow::Error {
    match e {
        CtlError::Timeout(_) => oops(format!("{vm}: {e}"), AGENT_HINT),
        CtlError::Closed => oops(
            format!("{vm}: {e}"),
            "The agent restarted or the VM stopped; run the command again",
        ),
        CtlError::Guest(m) => oops(format!("{vm}: {m}"), ""),
        CtlError::Failed(m) => oops(m, ""),
    }
}

/// A connection to one VM's control channel.
pub struct Client {
    s: UnixStream,
    next: u64,
    reader: conduit_ctl::LineReader,
    lines: VecDeque<String>,
    /// Never wait longer than this (tests).
    cap: Option<Duration>,
    _lock: Option<sys::Lock>,
}

impl Client {
    /// Connect to `vm`'s channel, or say precisely why that is not possible:
    /// not a libvirt VM, not running, running without the channel.
    pub fn connect(vm: &str) -> Result<Client> {
        let link = Link::load(vm).ok_or_else(|| {
            oops(
                format!("{vm} is not a libvirt VM of Conduit's"),
                format!("The control channel needs one: `conduit attach {vm}`"),
            )
        })?;
        let scope = virt::scope_for(&link.uri)?;
        let sock = units::ctl_path(&scope, vm);
        let state = link.virsh().state(&link.domain).unwrap_or_default();
        if !virt::state_is_up(&state) {
            return Err(oops(
                format!("{vm} is not running"),
                format!("Start it with `conduit view {vm}`"),
            ));
        }
        if state == "paused" {
            return Err(oops(
                format!("{vm} is paused"),
                format!("`conduit resume {vm}`"),
            ));
        }
        let dir = paths::run_dir(vm);
        std::fs::create_dir_all(&dir)?;
        let lock = sys::lock(&dir.join("ctl.lock"), LOCK_WAIT).map_err(|_| {
            oops(
                format!("another conduit command is using {vm}'s control channel"),
                "Wait for it (a file copy, say) to finish, then try again",
            )
        })?;
        let s = UnixStream::connect(&sock).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => oops(
                format!("{vm} has no control channel (QEMU is not listening on {})", sock.display()),
                format!("Add it with `conduit attach {vm}`, then restart the VM (`conduit down {vm}`, `conduit view {vm}`)"),
            ),
            _ => oops(format!("cannot open {}: {e}", sock.display()), ""),
        })?;
        let mut c = Client::from_stream(s);
        // A late answer to an earlier command's request must not pass for
        // ours: ids grow across commands (microseconds since the epoch).
        c.next = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64);
        c._lock = Some(lock);
        Ok(c)
    }

    pub fn from_stream(s: UnixStream) -> Client {
        Client {
            s,
            next: 0,
            reader: conduit_ctl::LineReader::default(),
            lines: VecDeque::new(),
            cap: None,
            _lock: None,
        }
    }

    #[cfg(test)]
    fn with_cap(mut self, d: Duration) -> Client {
        self.cap = Some(d);
        self
    }

    fn limit(&self, d: Duration) -> Duration {
        self.cap.map_or(d, |c| c.min(d))
    }

    /// Send a request and wait for its response. Responses to earlier,
    /// abandoned requests and lines that are not responses are skipped.
    pub fn call(&mut self, op: Op, limit: Duration) -> R<Response> {
        let limit = self.limit(limit);
        let deadline = Instant::now() + limit;
        self.next += 1;
        let id = self.next;
        let line = Request::new(id, op).to_line();
        self.s.set_write_timeout(Some(limit)).map_err(io_err)?;
        self.s
            .write_all(line.as_bytes())
            .map_err(|e| match io_err(e) {
                CtlError::Timeout(_) => CtlError::Timeout(limit),
                e => e,
            })?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            while let Some(l) = self.lines.pop_front() {
                if let Some(r) = Response::parse(&l) {
                    if r.id == id {
                        return Ok(r);
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.reader.reset();
                return Err(CtlError::Timeout(limit));
            }
            self.s.set_read_timeout(Some(left)).map_err(io_err)?;
            match self.s.read(&mut buf) {
                Ok(0) => return Err(CtlError::Closed),
                Ok(n) => self.lines.extend(self.reader.push(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(match io_err(e) {
                        CtlError::Timeout(_) => CtlError::Timeout(limit),
                        e => e,
                    })
                }
            }
        }
    }

    fn ask<T: DeserializeOwned>(&mut self, op: Op, limit: Duration) -> R<T> {
        let r = self.call(op, limit)?;
        if !r.ok {
            return Err(CtlError::Guest(
                r.error.unwrap_or_else(|| "the guest refused".into()),
            ));
        }
        r.into_body().map_err(CtlError::Failed)
    }

    pub fn ping(&mut self) -> R<Pong> {
        self.ask(Op::Ping, T_PING)
    }

    pub fn run(&mut self, a: RunArgs) -> R<Started> {
        self.ask(Op::Run(a), T_RUN)
    }

    pub fn apps(&mut self) -> R<Vec<App>> {
        let a: Apps = self.ask(Op::Apps, T_APPS)?;
        Ok(a.apps)
    }

    pub fn icon(&mut self, key: &str) -> R<Icon> {
        self.ask(Op::Icon { key: key.into() }, T_ICON)
    }

    pub fn ls(&mut self, path: &str) -> R<Listing> {
        self.ask(Op::Ls { path: path.into() }, T_FILE)
    }

    /// Copy a local file to the guest; returns where it landed. `done` is
    /// told (bytes so far, total) after every piece.
    pub fn put_file(
        &mut self,
        local: &Path,
        remote: &str,
        force: bool,
        done: &mut dyn FnMut(u64, u64),
    ) -> R<Written> {
        let meta = std::fs::metadata(local)
            .map_err(|e| CtlError::Failed(format!("{}: {e}", local.display())))?;
        if meta.is_dir() {
            return Err(CtlError::Failed(format!(
                "{} is a folder; `conduit cp` copies files (share a folder with `conduit share`)",
                local.display()
            )));
        }
        let size = meta.len();
        if size > MAX_FILE {
            return Err(CtlError::Failed(format!(
                "{} is {} (the most `conduit cp` moves is {}); share its folder with `conduit share`",
                local.display(),
                ui::human_bytes(size),
                ui::human_bytes(MAX_FILE)
            )));
        }
        let mut f = std::fs::File::open(local)
            .map_err(|e| CtlError::Failed(format!("{}: {e}", local.display())))?;
        let mut hash = Sha256::default();
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            let want = (size - offset).min(CHUNK as u64) as usize;
            f.read_exact(&mut buf[..want]).map_err(|_| {
                CtlError::Failed(format!("{} changed while it was copied", local.display()))
            })?;
            hash.update(&buf[..want]);
            let last = offset + want as u64 == size;
            let sum = last.then(|| std::mem::take(&mut hash).finish_hex());
            let w: Written = self.ask(
                Op::Put(PutArgs {
                    path: remote.into(),
                    offset,
                    data: b64_encode(&buf[..want]),
                    size: (offset == 0).then_some(size),
                    done: last,
                    sha256: sum.clone(),
                    force,
                }),
                T_FILE,
            )?;
            offset += want as u64;
            if w.size != offset {
                return Err(CtlError::Failed(format!(
                    "the guest has {} bytes where {offset} were sent",
                    w.size
                )));
            }
            done(offset, size);
            if last {
                if sum.as_deref() != Some(w.sha256.as_str()) {
                    return Err(CtlError::Failed(
                        "the copy in the guest does not match (checksum differs); it was not kept"
                            .into(),
                    ));
                }
                return Ok(w);
            }
        }
    }

    /// Copy a guest file to `dst` (a file path). Written to a temporary file
    /// next to it and renamed once the size and checksum agree.
    pub fn get_file(
        &mut self,
        remote: &str,
        dst: &Path,
        force: bool,
        done: &mut dyn FnMut(u64, u64),
    ) -> R<u64> {
        if dst.exists() && !force {
            return Err(CtlError::Failed(format!(
                "{} exists (--force replaces it)",
                dst.display()
            )));
        }
        let mut tmp = dst.as_os_str().to_owned();
        tmp.push(".conduit-part");
        let tmp = PathBuf::from(tmp);
        let r = self.get_into(remote, &tmp, done);
        let r = r.and_then(|n| {
            std::fs::rename(&tmp, dst)
                .map_err(|e| CtlError::Failed(format!("{}: {e}", dst.display())))?;
            Ok(n)
        });
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }

    fn get_into(&mut self, remote: &str, tmp: &Path, done: &mut dyn FnMut(u64, u64)) -> R<u64> {
        let mut out = std::fs::File::create(tmp)
            .map_err(|e| CtlError::Failed(format!("{}: {e}", tmp.display())))?;
        let mut hash = Sha256::default();
        let mut offset = 0u64;
        loop {
            let c: Chunk = self.ask(
                Op::Get(GetArgs {
                    path: remote.into(),
                    offset,
                    len: CHUNK,
                }),
                T_FILE,
            )?;
            if c.size > MAX_FILE {
                return Err(CtlError::Failed(format!(
                    "{remote} is {} (the most `conduit cp` moves is {})",
                    ui::human_bytes(c.size),
                    ui::human_bytes(MAX_FILE)
                )));
            }
            let data = b64_decode(&c.data)
                .ok_or_else(|| CtlError::Failed("the guest sent damaged data".into()))?;
            if data.len() > CHUNK || (data.is_empty() && !c.eof) {
                return Err(CtlError::Failed("the guest sent a bad piece".into()));
            }
            out.write_all(&data)
                .map_err(|e| CtlError::Failed(format!("{}: {e}", tmp.display())))?;
            hash.update(&data);
            offset += data.len() as u64;
            done(offset, c.size);
            if c.eof {
                if offset != c.size {
                    return Err(CtlError::Failed(format!(
                        "received {offset} of {} bytes",
                        c.size
                    )));
                }
                let want = c.sha256.unwrap_or_default();
                if hash.finish_hex() != want {
                    return Err(CtlError::Failed(
                        "the received file does not match the guest's (checksum differs)".into(),
                    ));
                }
                out.sync_all().ok();
                return Ok(offset);
            }
        }
    }
}

// ------------------------------------------------------------ app naming

/// The app a user named: exactly (ignoring case), else the one whose name
/// starts with it, else the one that contains it. Several matches or none is
/// an error that lists the candidates.
pub fn pick_app<'a>(apps: &'a [App], want: &str) -> Result<&'a App> {
    let w = want.trim().to_lowercase();
    let tiers: [&dyn Fn(&str) -> bool; 3] =
        [&|n| n == w, &|n| n.starts_with(&w), &|n| n.contains(&w)];
    for t in tiers {
        let hits: Vec<&App> = apps.iter().filter(|a| t(&a.name.to_lowercase())).collect();
        match hits.as_slice() {
            [] => {}
            [one] => return Ok(one),
            many => {
                // The same app listed twice (Start Menu and desktop) is one.
                let first = many[0];
                if many
                    .iter()
                    .all(|a| a.name.eq_ignore_ascii_case(&first.name))
                {
                    return Ok(first);
                }
                let mut names: Vec<&str> = many.iter().map(|a| a.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                let more = names.len().saturating_sub(8);
                names.truncate(8);
                return Err(oops(
                    format!(
                        "\"{want}\" matches several apps: {}{}",
                        names.join(", "),
                        if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        }
                    ),
                    "Give more of the name",
                ));
            }
        }
    }
    Err(oops(
        format!("no app called \"{want}\" in the guest"),
        "List them with `conduit apps VM`",
    ))
}

pub fn source_label(s: &str) -> &str {
    match s {
        "startmenu" => "Start Menu",
        "steam" => "Steam",
        "desktop" => "desktop",
        "flatpak" => "Flatpak",
        "snap" => "Snap",
        other => other,
    }
}

// ------------------------------------------------------------ cp endpoints

#[derive(Debug, PartialEq)]
pub enum Endpoint {
    Local(PathBuf),
    Remote { vm: String, path: String },
}

impl Endpoint {
    /// `VM:path` is a path in a VM; anything else is local. The VM name is at
    /// least two characters (so `C:\x` is a local path) and has no slash (so
    /// `./a:b` is local).
    pub fn parse(s: &str) -> Endpoint {
        if let Some((vm, path)) = s.split_once(':') {
            if vm.len() >= 2 && crate::vm::check_name(vm).is_ok() {
                return Endpoint::Remote {
                    vm: vm.into(),
                    path: path.into(),
                };
            }
        }
        Endpoint::Local(PathBuf::from(s))
    }
}

/// The last component of a guest path, either separator.
pub fn remote_basename(p: &str) -> &str {
    p.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
}

/// Where a file lands in the guest: `VM:` or a path ending in a separator
/// takes the source's name; a bare name goes to Downloads (the guest decides);
/// else the path as given.
pub fn remote_target(dst: &str, src: &Path) -> String {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dst.is_empty() {
        name
    } else if dst.ends_with(['/', '\\']) {
        format!("{dst}{name}")
    } else {
        dst.to_string()
    }
}

/// Where a guest file lands here: an existing folder (or a path ending in `/`)
/// takes the guest file's name.
pub fn local_target(dst: &Path, remote: &str) -> PathBuf {
    let ends_slash = dst.as_os_str().to_string_lossy().ends_with('/');
    if dst.is_dir() || ends_slash {
        dst.join(remote_basename(remote))
    } else {
        dst.to_path_buf()
    }
}

fn progress(what: &str) -> impl FnMut(u64, u64) + '_ {
    use std::io::IsTerminal;
    let tty = std::io::stderr().is_terminal();
    let start = Instant::now();
    let mut last = Instant::now() - Duration::from_secs(1);
    move |n, total| {
        if !tty || (last.elapsed() < Duration::from_millis(100) && n < total) {
            return;
        }
        last = Instant::now();
        let secs = start.elapsed().as_secs_f64().max(0.001);
        let pct = if total > 0 { n * 100 / total } else { 100 };
        eprint!(
            "\r{what}: {} / {} ({pct}%) {}/s   ",
            ui::human_bytes(n),
            ui::human_bytes(total),
            ui::human_bytes((n as f64 / secs) as u64)
        );
        if n >= total {
            eprintln!();
        }
    }
}

/// `conduit cp SRC DST`.
pub fn cp(src: &str, dst: &str, force: bool) -> Result<()> {
    match (Endpoint::parse(src), Endpoint::parse(dst)) {
        (Endpoint::Local(from), Endpoint::Remote { vm, path }) => {
            let mut c = Client::connect(&vm)?;
            let mut target = remote_target(&path, &from);
            // An existing folder in the guest takes the name too.
            if !target.ends_with(['/', '\\'])
                && !path.is_empty()
                && c.ls(&target).is_ok()
            {
                target = remote_target(&format!("{target}/"), &from);
            }
            let t = Instant::now();
            let w = c
                .put_file(&from, &target, force, &mut progress("copying"))
                .map_err(|e| friendly(&vm, e))?;
            ui::info(format!(
                "copied {} to {vm}:{} ({}, {:.1} s, checksum verified)",
                from.display(),
                w.path,
                ui::human_bytes(w.size),
                t.elapsed().as_secs_f64()
            ));
            Ok(())
        }
        (Endpoint::Remote { vm, path }, Endpoint::Local(to)) => {
            if path.is_empty() {
                return Err(oops("name the file in the VM, e.g. win11:C:\\Users\\me\\a.txt", ""));
            }
            let mut c = Client::connect(&vm)?;
            let target = local_target(&to, &path);
            let t = Instant::now();
            let n = c
                .get_file(&path, &target, force, &mut progress("copying"))
                .map_err(|e| friendly(&vm, e))?;
            ui::info(format!(
                "copied {vm}:{path} to {} ({}, {:.1} s, checksum verified)",
                target.display(),
                ui::human_bytes(n),
                t.elapsed().as_secs_f64()
            ));
            Ok(())
        }
        (Endpoint::Local(_), Endpoint::Local(_)) => Err(oops(
            "one side must be in a VM: `conduit cp FILE VM:PATH` or `conduit cp VM:PATH FILE`",
            "Between host folders use cp; folders you share with `conduit share` are in both places",
        )),
        (Endpoint::Remote { .. }, Endpoint::Remote { .. }) => Err(oops(
            "copying between two VMs is not supported",
            "Copy to this computer first",
        )),
    }
}

// ------------------------------------------------------------ run / apps

/// Start `conduit ARGS…` detached in its own session, its output in the VM's
/// runtime folder.
pub fn spawn_detached(vm: &str, args: &[&str]) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let dir = paths::run_dir(vm);
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::File::create(dir.join(format!("ctl-{}.log", args[0])))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(log.try_clone()?)
        .stdout(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(cmd.spawn()?)
}

fn has_desktop() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn viewer_open(vm: &str) -> bool {
    sys::read_pid(&paths::run_dir(vm).join("viewer.pid"))
        .is_some_and(|p| unsafe { libc::kill(p, 0) } == 0)
}

/// Connect and ping, waiting up to `wait` for the agent to come up.
fn connect_ready(vm: &str, wait: Duration) -> Result<(Client, Pong)> {
    let t = Instant::now();
    loop {
        let last = match Client::connect(vm) {
            Ok(mut c) => match c.ping() {
                Ok(p) => return Ok((c, p)),
                Err(e) => friendly(vm, e),
            },
            Err(e) => e,
        };
        if t.elapsed() >= wait {
            return Err(last);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

pub struct RunOpts {
    pub vm: String,
    pub cwd: Option<String>,
    pub env: Vec<String>,
    pub app: Option<String>,
    pub cmd: Vec<String>,
    /// Start the VM (with a window) if it is off, and wait for the guest.
    pub start: bool,
    pub no_view: bool,
}

/// `conduit run`.
pub fn run(o: RunOpts) -> Result<()> {
    use std::io::IsTerminal;
    let r = run_inner(&o);
    if let Err(e) = &r {
        if !std::io::stderr().is_terminal() {
            // Started from a desktop shortcut: nobody sees stderr.
            let _ = std::process::Command::new("notify-send")
                .args([
                    "-a",
                    "conduit",
                    "-i",
                    "dialog-error",
                    &format!("conduit: {e}"),
                ])
                .status();
        }
    }
    r
}

fn run_inner(o: &RunOpts) -> Result<()> {
    let vm = &o.vm;
    if o.app.is_none() && o.cmd.is_empty() {
        return Err(oops(
            "nothing to run",
            "conduit run VM -- PROGRAM [ARGS…]   or   conduit run VM --app \"App name\"",
        ));
    }
    let link = Link::load(vm).ok_or_else(|| {
        oops(
            format!("{vm} is not a libvirt VM of Conduit's"),
            format!("The control channel needs one: `conduit attach {vm}`"),
        )
    })?;
    let up = link
        .virsh()
        .state(&link.domain)
        .is_some_and(|s| virt::state_is_up(&s));
    let mut wait = Duration::ZERO;
    if !up {
        if !o.start {
            return Err(oops(
                format!("{vm} is not running"),
                format!(
                    "Start it with `conduit view {vm}`, or run with --start to start it and wait"
                ),
            ));
        }
        ui::info(format!("starting {vm}…"));
        spawn_detached(vm, &["view", vm])?;
        wait = Duration::from_secs(240);
    } else if !o.no_view && !viewer_open(vm) && has_desktop() {
        ui::info(format!("opening a window on {vm}…"));
        spawn_detached(vm, &["view", vm])?;
    }
    let (mut c, pong) = connect_ready(vm, wait)?;
    let mut args = RunArgs {
        cwd: o.cwd.clone().unwrap_or_default(),
        ..RunArgs::default()
    };
    for kv in &o.env {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| oops(format!("--env wants NAME=VALUE, not \"{kv}\""), ""))?;
        args.env.insert(k.into(), v.into());
    }
    let label = match &o.app {
        Some(name) => {
            let apps = c.apps().map_err(|e| friendly(vm, e))?;
            let a = pick_app(&apps, name)?;
            args.cmd = a.target.clone();
            args.args = a.args.clone();
            a.name.clone()
        }
        None => {
            args.cmd = o.cmd[0].clone();
            args.args = o.cmd[1..].to_vec();
            o.cmd[0].clone()
        }
    };
    if !pong.session {
        ui::warn(format!(
            "{vm} has no desktop session yet (nobody is logged in); the program may not appear"
        ));
    }
    let s = c.run(args).map_err(|e| friendly(vm, e))?;
    if s.pid > 0 {
        ui::info(format!("started {label} in {vm} (pid {})", s.pid));
    } else {
        ui::info(format!("started {label} in {vm}"));
    }
    Ok(())
}

/// Start `app` in the VM (the dashboard's Enter): no output, no waiting for
/// the guest beyond the request timeout.
pub fn run_app(vm: &str, app: &App) -> Result<Started> {
    let mut c = Client::connect(vm)?;
    c.run(RunArgs {
        cmd: app.target.clone(),
        args: app.args.clone(),
        ..RunArgs::default()
    })
    .map_err(|e| friendly(vm, e))
}

/// The apps of a running VM, sorted (the dashboard's Apps tab).
pub fn list_apps(vm: &str) -> Result<Vec<App>> {
    let mut c = Client::connect(vm)?;
    c.ping().map_err(|e| friendly(vm, e))?;
    let mut apps = c.apps().map_err(|e| friendly(vm, e))?;
    sort_apps(&mut apps);
    Ok(apps)
}

/// `conduit apps VM`.
pub fn apps(vm: &str, json: bool) -> Result<()> {
    let mut c = Client::connect(vm)?;
    let mut apps = c.apps().map_err(|e| friendly(vm, e))?;
    sort_apps(&mut apps);
    if json {
        println!("{}", serde_json::to_string_pretty(&Apps { apps })?);
        return Ok(());
    }
    if apps.is_empty() {
        ui::info(format!("{vm} reports no apps"));
        return Ok(());
    }
    let w = apps
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(48);
    for a in &apps {
        let name: String = a.name.chars().take(48).collect();
        println!("{name:<w$}  {:<10}  {}", source_label(&a.source), a.target);
    }
    Ok(())
}

pub fn sort_apps(apps: &mut [App]) {
    apps.sort_by_key(|a| (a.name.to_lowercase(), a.source.clone()));
}

// ------------------------------------------------------------ host shortcuts

/// `ab c/d` -> `ab-c-d`: file-name safe, lowercase.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let t = out.trim_end_matches('-');
    if t.is_empty() {
        "app".into()
    } else {
        t.into()
    }
}

fn data_home() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => paths::home().join(".local/share"),
    }
}

pub fn applications_dir() -> PathBuf {
    data_home().join("applications")
}

pub fn icons_dir() -> PathBuf {
    data_home().join("icons/conduit")
}

fn desktop_file(dir: &Path, vm: &str, name: &str) -> PathBuf {
    dir.join(format!("conduit-{vm}-{}.desktop", slug(name)))
}

/// A string for a Desktop Entry `Exec=` line (Desktop Entry spec, "The Exec key").
pub fn exec_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=+,:@".contains(c));
    if plain {
        return s.into();
    }
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                o.push('\\');
                o.push(c);
            }
            '%' => o.push_str("%%"),
            _ => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The text of a host launcher for a guest app.
pub fn desktop_entry(conduit: &Path, vm: &str, name: &str, icon: Option<&Path>) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('\n', " ");
    let mut s = String::from("[Desktop Entry]\nType=Application\n");
    s.push_str(&format!("Name={}\n", esc(name)));
    s.push_str(&format!("Comment=Runs in the VM {vm} (Conduit)\n"));
    s.push_str(&format!(
        "Exec={} run {} --start --app {}\n",
        exec_quote(&conduit.display().to_string()),
        exec_quote(vm),
        exec_quote(name)
    ));
    if let Some(i) = icon {
        s.push_str(&format!("Icon={}\n", i.display()));
    }
    s.push_str("Terminal=false\nStartupNotify=false\nCategories=Conduit;\n");
    s.push_str(&format!("X-Conduit-VM={vm}\nX-Conduit-App={}\n", esc(name)));
    s
}

/// Shortcuts of this VM: (app name, .desktop path).
pub fn shortcuts(dir: &Path, vm: &str) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let fname = e.file_name().to_string_lossy().into_owned();
        if !fname.starts_with(&format!("conduit-{vm}-")) || !fname.ends_with(".desktop") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
        };
        if field("X-Conduit-VM") == Some(vm) {
            if let Some(n) = field("X-Conduit-App") {
                out.push((n.to_string(), p));
            }
        }
    }
    out.sort();
    out
}

fn refresh_desktop_db(dir: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Make the host launcher for `name`; returns its path and what could not be
/// done on the way (no icon, say).
pub fn add_shortcut(vm: &str, name: &str) -> Result<(PathBuf, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut c = Client::connect(vm)?;
    let apps = c.apps().map_err(|e| friendly(vm, e))?;
    let app = pick_app(&apps, name)?.clone();
    let mut icon_path = None;
    if !app.icon.is_empty() {
        match c.icon(&app.icon) {
            Ok(i) => {
                let ext = if i.format == "svg" { "svg" } else { "png" };
                match b64_decode(&i.data) {
                    Some(bytes) if !bytes.is_empty() => {
                        let dir = icons_dir();
                        std::fs::create_dir_all(&dir)?;
                        let p = dir.join(format!("conduit-{vm}-{}.{ext}", slug(&app.name)));
                        std::fs::write(&p, bytes)?;
                        icon_path = Some(p);
                    }
                    _ => {
                        warnings.push("the guest sent an empty icon; the launcher has none".into())
                    }
                }
            }
            Err(e) => warnings.push(format!("no icon: {e}; the launcher has none")),
        }
    }
    let dir = applications_dir();
    std::fs::create_dir_all(&dir)?;
    let p = desktop_file(&dir, vm, &app.name);
    std::fs::write(
        &p,
        desktop_entry(
            &std::env::current_exe()?,
            vm,
            &app.name,
            icon_path.as_deref(),
        ),
    )?;
    refresh_desktop_db(&dir);
    Ok((p, warnings))
}

pub fn app_add(vm: &str, name: &str) -> Result<()> {
    let (p, warnings) = add_shortcut(vm, name)?;
    for w in warnings {
        ui::warn(w);
    }
    ui::info(format!(
        "added {} (starts the app in {vm}, and the VM if it is off)",
        p.display()
    ));
    Ok(())
}

pub fn app_rm(vm: &str, name: &str) -> Result<()> {
    let dir = applications_dir();
    let list = shortcuts(&dir, vm);
    let w = name.to_lowercase();
    let hit = list
        .iter()
        .find(|(n, _)| n.to_lowercase() == w)
        .or_else(|| {
            let m: Vec<_> = list
                .iter()
                .filter(|(n, _)| n.to_lowercase().contains(&w))
                .collect();
            (m.len() == 1).then(|| m[0])
        })
        .ok_or_else(|| {
            oops(
                format!("no shortcut for \"{name}\" in {vm}"),
                format!("List them with `conduit app list {vm}`"),
            )
        })?;
    let _ = std::fs::remove_file(icons_dir().join(format!("conduit-{vm}-{}.png", slug(&hit.0))));
    let _ = std::fs::remove_file(icons_dir().join(format!("conduit-{vm}-{}.svg", slug(&hit.0))));
    std::fs::remove_file(&hit.1)?;
    refresh_desktop_db(&dir);
    ui::info(format!("removed the shortcut for {}", hit.0));
    Ok(())
}

pub fn app_list(vm: &str) -> Result<()> {
    let list = shortcuts(&applications_dir(), vm);
    if list.is_empty() {
        ui::info(format!(
            "no host shortcuts for {vm} (`conduit app add {vm} \"App name\"`)"
        ));
    }
    for (n, p) in list {
        println!("{n}  {}", p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
