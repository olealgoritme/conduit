//! `conduit` with no command: a live dashboard of every VM, the host and its
//! GPU, with the everyday actions one key (or click) away.
//!
//! Actions run this same program (`conduit view NAME`, `conduit shutdown
//! NAME`, …) as child processes, so the dashboard does exactly what the
//! commands do. Every one runs in its own session with its output in a log
//! file in the VM's runtime folder, so it finishes (and a window opened from
//! here stays open) after the dashboard quits; the dashboard tails the short
//! commands' logs into its activity list.

mod apps;
mod data;
mod draw;
pub(crate) mod nvml;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

pub use data::Snapshot;

/// History length of the charts, in samples (one a second).
pub const HIST: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Dash,
    Logs,
    Doctor,
    Apps,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    View,
    Up,
    Shutdown,
    Reboot,
    Reset,
    Poweroff,
    Pause,
    Mode,
    Shares,
    Logs,
    Doctor,
    Help,
    Quit,
}

impl Act {
    pub fn verb(self) -> &'static str {
        match self {
            Act::View => "view",
            Act::Up => "up",
            Act::Shutdown => "shutdown",
            Act::Reboot => "reboot",
            Act::Reset => "reset",
            Act::Poweroff => "poweroff",
            Act::Pause => "pause",
            Act::Mode => "mode",
            Act::Shares => "shares",
            Act::Logs => "logs",
            Act::Doctor => "doctor",
            Act::Help => "help",
            Act::Quit => "quit",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Err,
}

pub struct Note {
    pub at: String,
    pub level: Level,
    pub text: String,
}

pub enum Modal {
    Confirm {
        act: Act,
        vm: String,
    },
    Mode {
        vm: String,
        items: Vec<(String, String)>,
        idx: usize,
    },
    /// The VM's shared folders; `input` is the path being typed for `a`.
    Shares {
        vm: String,
        items: Vec<crate::shares::Share>,
        idx: usize,
        input: Option<String>,
    },
    Help,
}

struct Job {
    vm: String,
    label: String,
    child: Child,
    log: PathBuf,
    /// A short command (shutdown, pause, share …): its log is tailed into
    /// the activity list and its VM shows it as busy. Otherwise a detached
    /// `view` / `up`, whose log is shown only if it fails.
    tail: Option<LogTail>,
    started: Instant,
}

/// The new complete lines of a log file, as they are written.
struct LogTail {
    file: std::fs::File,
    part: Vec<u8>,
}

impl LogTail {
    fn open(p: &Path) -> Option<LogTail> {
        Some(LogTail {
            file: std::fs::File::open(p).ok()?,
            part: Vec::new(),
        })
    }

    /// Lines finished since the last call; with `end`, the unfinished last
    /// line too. Each is what a terminal would show of it (colours dropped,
    /// the last `\r`-separated piece).
    fn lines(&mut self, end: bool) -> Vec<String> {
        let _ = self.file.read_to_end(&mut self.part);
        let mut out = Vec::new();
        let mut take = |l: &[u8]| {
            let t = strip_ansi(&String::from_utf8_lossy(l));
            let t = t.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("");
            if !t.trim().is_empty() {
                out.push(t.trim_end().to_string());
            }
        };
        while let Some(i) = self.part.iter().position(|&b| b == b'\n') {
            let l: Vec<u8> = self.part.drain(..=i).collect();
            take(&l[..i]);
        }
        if end && !self.part.is_empty() {
            let l = std::mem::take(&mut self.part);
            take(&l);
        }
        out
    }
}

/// Start `exe ARGS…` in its own session (no terminal signal reaches it and
/// it outlives the dashboard), stdout and stderr to `log`.
fn spawn_logged(exe: &str, args: &[String], log: &Path) -> std::io::Result<Child> {
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let file = std::fs::File::create(log)?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stderr(file.try_clone()?)
        .stdout(file);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
}

/// The program to run for actions: the running binary; once an upgrade
/// replaced it (Linux then reports "PATH (deleted)"), the new file at that
/// path; else `conduit` on PATH.
fn exe_path(cur: Option<PathBuf>) -> String {
    let Some(p) = cur else {
        return "conduit".into();
    };
    if p.exists() {
        return p.to_string_lossy().into_owned();
    }
    let s = p.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(orig) if Path::new(orig).is_file() => orig.to_string(),
        _ => "conduit".into(),
    }
}

/// The panic hook's part: only the thread that owns the terminal restores
/// it (and reports the panic); a panic on a worker thread is caught there
/// and shown as a note, and printing it would scribble over the screen.
fn on_panic(main: std::thread::ThreadId, restore: &dyn Fn(), report: &dyn Fn()) {
    if std::thread::current().id() == main {
        restore();
        report();
    }
}

pub struct App {
    pub snap: Snapshot,
    pub sel: usize,
    pub tab: Tab,
    pub tick: u64,
    /// Frames of the opening animation still to show (any key skips it).
    pub intro: u64,
    pub gpu_util: VecDeque<u64>,
    pub gpu_temp: VecDeque<u64>,
    pub gpu_power: VecDeque<u64>,
    pub gpu_vram: VecDeque<u64>,
    pub host_cpu: VecDeque<u64>,
    pub vm_cpu: HashMap<String, VecDeque<u64>>,
    pub notes: VecDeque<Note>,
    pub modal: Option<Modal>,
    pub modes: HashMap<String, String>,
    pub logs: Vec<String>,
    pub logs_which: usize,
    pub logs_scroll: u16,
    pub doctor: Vec<String>,
    pub doctor_scroll: u16,
    pub capturing: Option<Tab>,
    pub apps: apps::AppsTab,
    pub rows: Vec<(Rect, usize)>,
    pub buttons: Vec<(Rect, Act)>,
    pub tabs: Vec<(Rect, Tab)>,
    jobs: Vec<Job>,
    cap_tx: mpsc::Sender<(Tab, Vec<String>)>,
    cap_rx: mpsc::Receiver<(Tab, Vec<String>)>,
    logs_at: Option<Instant>,
    last_seq: u64,
    poke: mpsc::Sender<()>,
    quit: bool,
    /// Something changed since the last frame.
    dirty: bool,
    /// The sampler's last error, noted once.
    last_error: Option<String>,
    /// mtime of the open Shares modal's `shares.json`.
    shares_stamp: Option<SystemTime>,
    /// Commands the tests' App would have run: (vm, args).
    #[cfg(test)]
    spawned: Vec<(String, Vec<String>)>,
}

pub const LOG_SOURCES: [&str; 5] = ["all", "backend", "vm", "venus", "viewer"];

impl App {
    pub fn vm(&self) -> Option<&data::Vm> {
        self.snap.vms.get(self.sel)
    }

    /// The label of a command running for `vm`, for its state column.
    pub fn busy(&self, vm: &str) -> Option<&str> {
        self.jobs
            .iter()
            .rev()
            .find(|j| j.vm == vm && j.tail.is_some())
            .map(|j| j.label.as_str())
    }

    pub fn note(&mut self, level: Level, text: impl Into<String>) {
        self.dirty = true;
        self.notes.push_back(Note {
            at: clock(),
            level,
            text: text.into(),
        });
        while self.notes.len() > 200 {
            self.notes.pop_front();
        }
    }

    fn exe() -> String {
        exe_path(std::env::current_exe().ok())
    }

    /// Runs a short `conduit ARGS…` (shutdown, pause, share …); its output
    /// lines land in the activity list.
    fn run(&mut self, vm: &str, label: &str, args: Vec<String>) {
        self.start_job(vm, label, args, true);
    }

    /// Runs a long `conduit ARGS…` (`view`, `up`); its log is shown only
    /// if it fails.
    fn detach(&mut self, vm: &str, label: &str, args: Vec<String>) {
        self.start_job(vm, label, args, false);
        let _ = self.poke.send(());
    }

    fn start_job(&mut self, vm: &str, label: &str, args: Vec<String>, tail: bool) {
        #[cfg(test)]
        {
            let _ = tail;
            self.note(Level::Info, format!("{label} {vm}"));
            self.spawned.push((vm.into(), args));
        }
        #[cfg(not(test))]
        {
            let verb = args.first().map(String::as_str).unwrap_or("job");
            let log = crate::paths::run_dir(vm).join(format!("dashboard-{verb}.log"));
            match spawn_logged(&Self::exe(), &args, &log) {
                Ok(child) => {
                    let ellipsis = if tail { "…" } else { "" };
                    self.note(Level::Info, format!("{label} {vm}{ellipsis}"));
                    self.jobs.push(Job {
                        vm: vm.into(),
                        label: label.into(),
                        child,
                        tail: if tail { LogTail::open(&log) } else { None },
                        log,
                        started: Instant::now(),
                    });
                }
                Err(e) => self.note(Level::Err, format!("could not run conduit: {e}")),
            }
        }
    }

    fn venus_args(&self, vm: &data::Vm, args: &mut Vec<String>) {
        if vm.windows {
            args.push("--venus".into());
        }
    }

    /// Act on the selected VM.
    pub fn act(&mut self, a: Act) {
        match a {
            Act::Quit => {
                self.quit = true;
                return;
            }
            Act::Help => {
                self.modal = Some(Modal::Help);
                return;
            }
            Act::Doctor => {
                self.tab = Tab::Doctor;
                self.capture(Tab::Doctor);
                return;
            }
            Act::Logs => {
                self.tab = Tab::Logs;
                self.logs_scroll = 0;
                self.capture(Tab::Logs);
                return;
            }
            _ => {}
        }
        let Some(vm) = self.vm().cloned() else {
            self.note(
                Level::Warn,
                "no VM selected (create one with `conduit create NAME`)",
            );
            return;
        };
        self.act_on(a, vm, false);
    }

    /// Act on `vm`; `confirmed` once the confirmation for a stop was given
    /// (for the VM it was asked about, whatever is selected by then).
    fn act_on(&mut self, a: Act, vm: data::Vm, confirmed: bool) {
        let name = vm.name.clone();
        match a {
            Act::Mode => {
                let mut items = Vec::new();
                let (m, src) = crate::mode::detect();
                items.push((m.to_string(), format!("your monitor ({src})")));
                for p in [
                    "1920x1080@240",
                    "2560x1440@240",
                    "3440x1440@240",
                    "3840x2160@240",
                    "5120x1440@240",
                    "1920x1080@144",
                    "2560x1440@144",
                    "1920x1080@60",
                ] {
                    if !items.iter().any(|(x, _)| x == p) {
                        items.push((p.to_string(), String::new()));
                    }
                }
                let cur = self.modes.get(&name).cloned().or(vm.mode.clone());
                let idx = cur
                    .and_then(|c| items.iter().position(|(x, _)| *x == c))
                    .unwrap_or(0);
                self.modal = Some(Modal::Mode {
                    vm: name,
                    items,
                    idx,
                });
            }
            Act::Shares => {
                if vm.libvirt.is_none() {
                    self.note(
                        Level::Warn,
                        format!("{name} is not a libvirt VM (`conduit attach {name}` first)"),
                    );
                    return;
                }
                self.modal = Some(Modal::Shares {
                    items: crate::shares::load(&name).unwrap_or_default(),
                    vm: name,
                    idx: 0,
                    input: None,
                });
            }
            Act::View => {
                if vm.viewer {
                    self.note(
                        Level::Warn,
                        format!("{name} already has a viewer window open"),
                    );
                    return;
                }
                let mut args = vec!["view".to_string(), name.clone()];
                if let Some(m) = self.modes.get(&name) {
                    args.push(m.clone());
                }
                self.venus_args(&vm, &mut args);
                self.detach(&name, "opening a window on", args);
            }
            Act::Up => {
                if vm.up() {
                    self.note(Level::Warn, format!("{name} is already {}", vm.state));
                    return;
                }
                let mut args = vec!["up".to_string(), name.clone()];
                if let Some(m) = self.modes.get(&name) {
                    args.push("--display".into());
                    args.push(m.clone());
                }
                self.venus_args(&vm, &mut args);
                self.detach(&name, "starting", args);
            }
            Act::Pause => {
                let (verb, label) = if vm.state == "paused" {
                    ("resume", "resuming")
                } else {
                    ("pause", "pausing")
                };
                self.run(&name, label, vec![verb.into(), name.clone()]);
            }
            Act::Shutdown | Act::Reboot | Act::Reset | Act::Poweroff => {
                if !vm.up() {
                    self.note(Level::Warn, format!("{name} is not running"));
                    return;
                }
                if !confirmed {
                    self.modal = Some(Modal::Confirm { act: a, vm: name });
                    return;
                }
                let label = match a {
                    Act::Shutdown => "shutting down",
                    Act::Reboot => "rebooting",
                    Act::Reset => "resetting",
                    _ => "forcing off",
                };
                self.modal = None;
                self.run(&name, label, vec![a.verb().into(), name.clone()]);
            }
            _ => {}
        }
        let _ = self.poke.send(());
    }

    /// The Apps tab for the selected VM.
    fn show_apps(&mut self) {
        self.tab = Tab::Apps;
        self.apps.reset_view();
    }

    /// Fills the Logs or Doctor tab from `conduit logs`/`conduit doctor`.
    fn capture(&mut self, tab: Tab) {
        if self.capturing == Some(tab) {
            return;
        }
        let args: Vec<String> = match tab {
            Tab::Doctor => vec!["doctor".into()],
            Tab::Logs => {
                let Some(vm) = self.vm() else { return };
                let mut a = vec!["logs".into(), vm.name.clone()];
                if self.logs_which > 0 {
                    a.push(LOG_SOURCES[self.logs_which].into());
                }
                a.extend(["-n".into(), "400".into()]);
                a
            }
            Tab::Dash | Tab::Apps => return,
        };
        self.capturing = Some(tab);
        let tx = self.cap_tx.clone();
        let exe = Self::exe();
        std::thread::spawn(move || {
            let out = Command::new(exe).args(&args).stdin(Stdio::null()).output();
            let lines = match out {
                Ok(o) => {
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                    strip_ansi(&s).lines().map(str::to_string).collect()
                }
                Err(e) => vec![format!("could not run conduit: {e}")],
            };
            let _ = tx.send((tab, lines));
        });
    }

    fn reap(&mut self) {
        let mut done = Vec::new();
        let mut lines = Vec::new();
        for (i, j) in self.jobs.iter_mut().enumerate() {
            let st = j.child.try_wait().ok().flatten();
            if let Some(t) = &mut j.tail {
                lines.extend(t.lines(st.is_some()));
            }
            if let Some(st) = st {
                done.push((i, st));
            }
        }
        for l in lines {
            self.note(Level::Info, l);
        }
        for (i, st) in done.into_iter().rev() {
            let j = self.jobs.remove(i);
            let secs = j.started.elapsed().as_secs_f32();
            let log = j.tail.is_none().then_some(&j.log);
            match (log, st.success()) {
                // `conduit view` returns at once when it hands the window
                // to a VM that keeps running (a libvirt VM).
                (Some(_), true) if j.label.starts_with("opening") && secs < 10.0 => {
                    self.note(Level::Ok, format!("{}: window open", j.vm))
                }
                (Some(_), true) if j.label.starts_with("opening") => {
                    self.note(Level::Info, format!("{}: window closed", j.vm))
                }
                (_, true) => self.note(
                    Level::Ok,
                    format!("{} {}: done ({secs:.1} s)", j.label, j.vm),
                ),
                (Some(log), false) => {
                    let tail = std::fs::read_to_string(log).unwrap_or_default();
                    let last: Vec<&str> = tail.lines().rev().take(3).collect();
                    for l in last.into_iter().rev() {
                        self.note(Level::Err, strip_ansi(l));
                    }
                    self.note(
                        Level::Err,
                        format!("{} {} failed ({})", j.label, j.vm, log.display()),
                    );
                }
                (None, false) => self.note(Level::Err, format!("{} {} failed", j.label, j.vm)),
            }
            let _ = self.poke.send(());
        }
    }

    fn absorb(&mut self, s: Snapshot) {
        if s.seq == self.last_seq {
            return;
        }
        self.last_seq = s.seq;
        self.dirty = true;
        if s.error != self.last_error {
            if let Some(e) = &s.error {
                self.note(Level::Warn, e.clone());
            }
            self.last_error = s.error.clone();
        }
        let push = |q: &mut VecDeque<u64>, v: u64| {
            q.push_back(v);
            while q.len() > HIST {
                q.pop_front();
            }
        };
        if let Some(g) = &s.gpu {
            push(&mut self.gpu_util, g.util_gpu.unwrap_or(0) as u64);
            push(&mut self.gpu_temp, g.temp_c.unwrap_or(0) as u64);
            push(&mut self.gpu_power, (g.power_mw.unwrap_or(0) / 1000) as u64);
            if let (Some(u), Some(t)) = (g.vram_used, g.vram_total) {
                push(&mut self.gpu_vram, if t > 0 { u * 100 / t } else { 0 });
            }
        }
        push(&mut self.host_cpu, s.host.cpu_pct.round() as u64);
        for v in &s.vms {
            let q = self.vm_cpu.entry(v.name.clone()).or_default();
            push(q, v.cpu_pct.round() as u64);
        }
        let sel_name = self.vm().map(|v| v.name.clone());
        self.snap = s;
        if let Some(n) = sel_name {
            if let Some(i) = self.snap.vms.iter().position(|v| v.name == n) {
                self.sel = i;
            }
        }
        self.sel = self.sel.min(self.snap.vms.len().saturating_sub(1));
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.dirty = true;
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        let mut todo: Option<(String, String, Vec<String>)> = None;
        if let Some(m) = &mut self.modal {
            match m {
                Modal::Help => self.modal = None,
                Modal::Shares {
                    vm,
                    items,
                    idx,
                    input,
                } => {
                    let cmd = |what: &str, rest: Vec<String>| {
                        let mut a = vec!["share".to_string(), what.to_string(), vm.clone()];
                        a.extend(rest);
                        (vm.clone(), format!("{what} shared folder on"), a)
                    };
                    if let Some(text) = input {
                        match code {
                            KeyCode::Esc => *input = None,
                            KeyCode::Backspace => {
                                text.pop();
                            }
                            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                                text.push(c)
                            }
                            KeyCode::Enter => {
                                let p = text.trim().to_string();
                                *input = None;
                                if !p.is_empty() {
                                    todo = Some(cmd("add", vec![expand_home(&p)]));
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match code {
                            KeyCode::Up | KeyCode::Char('k') => *idx = idx.saturating_sub(1),
                            KeyCode::Down | KeyCode::Char('j') => {
                                *idx = (*idx + 1).min(items.len().saturating_sub(1))
                            }
                            KeyCode::Char('a') => *input = Some(String::new()),
                            KeyCode::Char('d') => {
                                if let Some(s) = items.get(*idx) {
                                    todo = Some(cmd("rm", vec![s.name.clone()]));
                                }
                            }
                            KeyCode::Char('o') => {
                                if let Some(s) = items.get(*idx) {
                                    let state = if s.read_only { "off" } else { "on" };
                                    todo = Some(cmd("ro", vec![s.name.clone(), state.into()]));
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('F') => {
                                self.modal = None
                            }
                            _ => {}
                        }
                    }
                }
                Modal::Confirm { act, vm } => match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        // The VM asked about, not whatever is selected now
                        // (the sampler may have reordered the list).
                        let (a, name) = (*act, vm.clone());
                        self.modal = None;
                        match self.snap.vms.iter().find(|v| v.name == name).cloned() {
                            Some(v) => self.act_on(a, v, true),
                            None => self.note(Level::Warn, format!("{name} is gone")),
                        }
                    }
                    _ => {
                        self.modal = None;
                        self.note(Level::Info, "cancelled");
                    }
                },
                Modal::Mode { vm, items, idx } => match code {
                    KeyCode::Up | KeyCode::Char('k') => *idx = idx.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *idx = (*idx + 1).min(items.len() - 1),
                    KeyCode::Enter => {
                        let (vm, m) = (vm.clone(), items[*idx].0.clone());
                        self.modal = None;
                        self.note(Level::Ok, format!("{vm}: {m} on the next view/start"));
                        self.modes.insert(vm, m);
                    }
                    _ => self.modal = None,
                },
            }
            if let Some((vm, label, args)) = todo {
                self.run(&vm, &label, args);
            }
            return;
        }
        if self.tab == Tab::Apps {
            // Left/Right pick another VM; the list starts at the top.
            if !self.apps.typing && matches!(code, KeyCode::Left | KeyCode::Right) {
                let n = self.snap.vms.len();
                if n > 0 {
                    self.sel = if code == KeyCode::Left {
                        (self.sel + n - 1) % n
                    } else {
                        (self.sel + 1) % n
                    };
                    self.apps.reset_view();
                }
                return;
            }
            let vm = self.snap.vms.get(self.sel);
            if self.apps.key(code, vm) {
                return;
            }
        }
        let n = self.snap.vms.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.tab != Tab::Dash {
                    self.tab = Tab::Dash
                } else {
                    self.quit = true
                }
            }
            KeyCode::Up | KeyCode::Char('k') => match self.tab {
                Tab::Dash => self.sel = self.sel.saturating_sub(1),
                Tab::Logs => self.logs_scroll = self.logs_scroll.saturating_add(1),
                Tab::Doctor => self.doctor_scroll = self.doctor_scroll.saturating_sub(1),
                Tab::Apps => {}
            },
            KeyCode::Down | KeyCode::Char('j') => match self.tab {
                Tab::Dash => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
                Tab::Logs => self.logs_scroll = self.logs_scroll.saturating_sub(1),
                Tab::Doctor => self.doctor_scroll = self.doctor_scroll.saturating_add(1),
                Tab::Apps => {}
            },
            KeyCode::PageUp => self.logs_scroll = self.logs_scroll.saturating_add(20),
            KeyCode::PageDown => self.logs_scroll = self.logs_scroll.saturating_sub(20),
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Dash => Tab::Logs,
                    Tab::Logs => Tab::Doctor,
                    Tab::Doctor => Tab::Apps,
                    Tab::Apps => Tab::Dash,
                };
                if !matches!(self.tab, Tab::Dash | Tab::Apps) {
                    let t = self.tab;
                    self.capture(t);
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Dash,
            KeyCode::Char('2') => self.act(Act::Logs),
            KeyCode::Char('3') => self.act(Act::Doctor),
            KeyCode::Char('4') => self.show_apps(),
            KeyCode::Left | KeyCode::Right if self.tab == Tab::Logs => {
                let k = LOG_SOURCES.len();
                self.logs_which = if code == KeyCode::Left {
                    (self.logs_which + k - 1) % k
                } else {
                    (self.logs_which + 1) % k
                };
                self.logs_scroll = 0;
                self.capture(Tab::Logs);
            }
            KeyCode::Enter | KeyCode::Char('v') => self.act(Act::View),
            KeyCode::Char('s') => self.act(Act::Up),
            KeyCode::Char('d') => self.act(Act::Shutdown),
            KeyCode::Char('R') => self.act(Act::Reboot),
            KeyCode::Char('r') => self.act(Act::Reset),
            KeyCode::Char('f') => self.act(Act::Poweroff),
            KeyCode::Char('p') => self.act(Act::Pause),
            KeyCode::Char('m') => self.act(Act::Mode),
            KeyCode::Char('F') => self.act(Act::Shares),
            KeyCode::Char('l') => self.act(Act::Logs),
            KeyCode::Char('D') => self.act(Act::Doctor),
            KeyCode::Char('?') | KeyCode::F(1) => self.act(Act::Help),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16) {
        self.dirty = true;
        let p = Position { x, y };
        if self.modal.is_some() {
            if matches!(self.modal, Some(Modal::Help)) {
                self.modal = None;
            }
            return;
        }
        if let Some((_, a)) = self.buttons.iter().find(|(r, _)| r.contains(p)).copied() {
            self.act(a);
            return;
        }
        if let Some((_, t)) = self.tabs.iter().find(|(r, _)| r.contains(p)).copied() {
            match t {
                Tab::Dash => self.tab = Tab::Dash,
                Tab::Logs => self.act(Act::Logs),
                Tab::Doctor => self.act(Act::Doctor),
                Tab::Apps => self.show_apps(),
            }
            return;
        }
        if let Some((_, i)) = self.rows.iter().find(|(r, _)| r.contains(p)).copied() {
            if self.sel == i && self.tab == Tab::Dash {
                self.act(Act::View);
            }
            self.sel = i;
        }
    }
}

impl App {
    /// Something on screen moves on its own (a spinner, the intro), so it is
    /// drawn at the animation rate; otherwise only when something changes.
    fn animating(&self) -> bool {
        self.intro > 0
            || self.capturing.is_some()
            || self.jobs.iter().any(|j| j.tail.is_some())
            || (self.tab == Tab::Doctor && self.doctor.is_empty())
            || (self.tab == Tab::Apps && self.apps.vms.values().any(|e| e.loading))
    }

    /// The open Shares modal follows `shares.json` when it changes (a
    /// `conduit share` child rewrites it).
    fn follow_shares(&mut self) {
        let Some(Modal::Shares { vm, items, idx, .. }) = &mut self.modal else {
            self.shares_stamp = None;
            return;
        };
        let stamp = std::fs::metadata(crate::shares::file(vm))
            .and_then(|m| m.modified())
            .ok();
        if stamp != self.shares_stamp {
            self.shares_stamp = stamp;
            *items = crate::shares::load(vm).unwrap_or_default();
            *idx = (*idx).min(items.len().saturating_sub(1));
            self.dirty = true;
        }
    }
}

pub fn clock() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        crossterm::event::DisableFocusChange,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// A typed path: a leading `~/` is the home folder.
fn expand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => crate::paths::home().join(rest).display().to_string(),
        None if p == "~" => crate::paths::home().display().to_string(),
        None => p.to_string(),
    }
}

/// `conduit` alone.
pub fn run() -> Result<()> {
    let prev = std::panic::take_hook();
    let main = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        on_panic(main, &restore, &|| prev(info));
    }));
    for s in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe {
            libc::signal(
                s,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    enable_raw_mode()?;
    let _g = Guard;
    crossterm::execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::event::EnableFocusChange
    )?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    // NVML is let go while the terminal reports the dashboard unfocused.
    let gpu_wanted = Arc::new(AtomicBool::new(true));
    let (shared, poke) = data::start(gpu_wanted.clone());
    let (cap_tx, cap_rx) = mpsc::channel();
    let mut app = App {
        snap: Snapshot::default(),
        sel: 0,
        tab: Tab::Dash,
        tick: 0,
        intro: 26,
        gpu_util: VecDeque::new(),
        gpu_temp: VecDeque::new(),
        gpu_power: VecDeque::new(),
        gpu_vram: VecDeque::new(),
        host_cpu: VecDeque::new(),
        vm_cpu: HashMap::new(),
        notes: VecDeque::new(),
        modal: None,
        modes: HashMap::new(),
        logs: Vec::new(),
        logs_which: 0,
        logs_scroll: 0,
        doctor: Vec::new(),
        doctor_scroll: 0,
        capturing: None,
        apps: apps::AppsTab::new(),
        rows: Vec::new(),
        buttons: Vec::new(),
        tabs: Vec::new(),
        jobs: Vec::new(),
        cap_tx,
        cap_rx,
        logs_at: None,
        last_seq: 0,
        poke,
        quit: false,
        dirty: true,
        last_error: None,
        shares_stamp: None,
        #[cfg(test)]
        spawned: vec![],
    };
    // Select the most recently used VM.
    let first = shared.lock().map(|s| s.clone()).unwrap_or_default();
    app.absorb(first);
    app.note(Level::Info, "welcome to Conduit · ? for help");

    while !app.quit && SIGNAL.load(Ordering::SeqCst) == 0 {
        let snap = shared.lock().map(|s| s.clone()).ok();
        if let Some(s) = snap {
            let first_fill = app.last_seq == 0 && s.seq > 0;
            app.absorb(s);
            if first_fill {
                if let Some(i) = crate::lvrun::recent_vm()
                    .and_then(|n| app.snap.vms.iter().position(|v| v.name == n))
                {
                    app.sel = i;
                }
            }
        }
        while let Ok((tab, lines)) = app.cap_rx.try_recv() {
            match tab {
                Tab::Logs => {
                    app.logs = lines;
                    app.logs_at = Some(Instant::now());
                }
                Tab::Doctor => app.doctor = lines,
                Tab::Dash | Tab::Apps => {}
            }
            if app.capturing == Some(tab) {
                app.capturing = None;
            }
        }
        if app.tab == Tab::Logs
            && app
                .logs_at
                .is_some_and(|t| t.elapsed() > Duration::from_secs(2))
        {
            app.logs_at = None;
            app.capture(Tab::Logs);
        }
        app.follow_shares();
        for (lvl, t) in app.apps.pump() {
            app.note(lvl, t);
        }
        if app.tab == Tab::Apps {
            app.apps.tick(app.snap.vms.get(app.sel));
        }
        app.reap();
        let animating = app.animating();
        if app.dirty || animating {
            term.draw(|f| draw::draw(f, &mut app))?;
            app.dirty = false;
            app.tick = app.tick.wrapping_add(1);
            app.intro = app.intro.saturating_sub(1);
        }
        let frame = match (app.intro > 0, animating) {
            (true, _) => 45,
            (false, true) => 100,
            // Nothing moves: wake for input, a new snapshot or a job's line.
            (false, false) => 250,
        };
        if event::poll(Duration::from_millis(frame))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press && app.intro > 0 => app.intro = 0,
                Event::Key(k) if k.kind == KeyEventKind::Press => app.key(k.code, k.modifiers),
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => app.click(m.column, m.row),
                    MouseEventKind::ScrollUp => app.key(KeyCode::Up, KeyModifiers::NONE),
                    MouseEventKind::ScrollDown => app.key(KeyCode::Down, KeyModifiers::NONE),
                    _ => {}
                },
                Event::FocusLost => {
                    gpu_wanted.store(false, Ordering::SeqCst);
                    let _ = app.poke.send(());
                }
                Event::FocusGained => {
                    gpu_wanted.store(true, Ordering::SeqCst);
                    let _ = app.poke.send(());
                }
                Event::Resize(..) => app.dirty = true,
                _ => {}
            }
        }
    }
    let running = app.jobs.iter().filter(|j| j.tail.is_some()).count();
    drop(term);
    drop(_g);
    if running > 0 {
        eprintln!(
            "conduit: {running} command(s) started from the dashboard still running; their output is in dashboard-*.log in the VM's runtime folder"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn app_pub() -> App {
        app()
    }
    fn app() -> App {
        let (poke, _) = mpsc::channel();
        let (cap_tx, cap_rx) = mpsc::channel();
        let mut snap = Snapshot {
            seq: 1,
            ..Default::default()
        };
        snap.vms = vec![
            data::Vm {
                name: "win11".into(),
                state: "running".into(),
                windows: true,
                libvirt: Some("win11".into()),
                mode: Some("5120x1440@240".into()),
                cpus: Some(16),
                ram_mib: Some(32768),
                pid: Some(1),
                cpu_pct: 812.0,
                uptime: Some(Duration::from_secs(7000)),
                vram: 3 << 30,
                ..Default::default()
            },
            data::Vm {
                name: "lab".into(),
                state: "stopped".into(),
                ..Default::default()
            },
        ];
        snap.gpu = Some(nvml::Sample {
            name: "NVIDIA GeForce RTX 5090".into(),
            temp_c: Some(61),
            util_gpu: Some(87),
            vram_used: Some(12 << 30),
            vram_total: Some(32 << 30),
            power_mw: Some(430_000),
            power_limit_mw: Some(575_000),
            pstate: Some(0),
            ..Default::default()
        });
        App {
            snap,
            sel: 0,
            tab: Tab::Dash,
            tick: 0,
            intro: 0,
            gpu_util: (0..120).map(|i| i % 100).collect(),
            gpu_temp: VecDeque::new(),
            gpu_power: VecDeque::new(),
            gpu_vram: VecDeque::new(),
            host_cpu: VecDeque::new(),
            vm_cpu: HashMap::new(),
            notes: VecDeque::new(),
            modal: None,
            modes: HashMap::new(),
            logs: vec!["error: x".into(); 50],
            logs_which: 0,
            logs_scroll: 0,
            doctor: vec!["ok".into()],
            doctor_scroll: 0,
            capturing: None,
            apps: apps::AppsTab::new(),
            rows: vec![],
            buttons: vec![],
            tabs: vec![],
            jobs: vec![],
            cap_tx,
            cap_rx,
            logs_at: None,
            last_seq: 1,
            poke,
            quit: false,
            dirty: true,
            last_error: None,
            shares_stamp: None,
            spawned: vec![],
        }
    }

    #[test]
    fn draws_every_tab_and_modal_at_any_size() {
        use ratatui::backend::TestBackend;
        for (w, h) in [(20, 8), (80, 24), (120, 40), (240, 70)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut a = app();
            for tab in [Tab::Dash, Tab::Logs, Tab::Doctor, Tab::Apps] {
                a.tab = tab;
                t.draw(|f| draw::draw(f, &mut a)).unwrap();
            }
            a.tab = Tab::Dash;
            for m in [
                Modal::Help,
                Modal::Confirm {
                    act: Act::Poweroff,
                    vm: "win11".into(),
                },
                Modal::Mode {
                    vm: "win11".into(),
                    items: vec![("1920x1080@240".into(), String::new()); 9],
                    idx: 3,
                },
                Modal::Shares {
                    vm: "win11".into(),
                    items: vec![crate::shares::default_share("win11")],
                    idx: 0,
                    input: Some("~/Doc".into()),
                },
            ] {
                a.modal = Some(m);
                t.draw(|f| draw::draw(f, &mut a)).unwrap();
            }
        }
    }

    #[test]
    fn draws_the_apps_tab_in_every_state() {
        use ratatui::backend::TestBackend;
        let mk = |n: &str| conduit_ctl::App {
            name: n.into(),
            source: "steam".into(),
            ..Default::default()
        };
        for (w, h) in [(20, 8), (80, 24), (170, 46)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut a = app();
            a.tab = Tab::Apps;
            let name = a.snap.vms[0].name.clone();
            a.snap.vms[0].state = "running".into();
            a.snap.vms[0].libvirt = Some("qemu:///session".into());
            // Nothing yet, loading, failed, listed, listed but stale.
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
            let e = a.apps.vms.entry(name.clone()).or_default();
            e.loading = true;
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
            let e = a.apps.vms.get_mut(&name).unwrap();
            e.loading = false;
            e.error = Some("no answer\nstart the agent".into());
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
            let e = a.apps.vms.get_mut(&name).unwrap();
            e.list = (0..300)
                .map(|i| mk(&format!("App número {i} 日本")))
                .collect();
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
            a.apps.sel = 299;
            a.apps.filter = "99".into();
            a.apps.typing = true;
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
            assert!(a.apps.sel < 300);
            a.snap.vms[0].state = "stopped".into();
            t.draw(|f| draw::draw(f, &mut a)).unwrap();
        }
    }

    #[test]
    fn a_confirmed_stop_acts_on_the_vm_it_asked_about() {
        let mut a = app();
        a.sel = 0;
        a.key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            matches!(&a.modal, Some(Modal::Confirm { act: Act::Shutdown, vm }) if vm == "win11")
        );
        assert!(a.spawned.is_empty(), "nothing runs before the answer");
        // The list changes under the open question: a new VM sorts first and
        // the selection now points at another one.
        let mut other = a.snap.vms[0].clone();
        other.name = "aaa".into();
        a.snap.vms.insert(0, other);
        a.sel = 0;
        a.key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(a.modal.is_none());
        assert_eq!(
            a.spawned,
            [(
                "win11".to_string(),
                vec!["shutdown".to_string(), "win11".to_string()]
            )]
        );
        // A VM that went away meanwhile is not acted on.
        let mut a = app();
        a.modal = Some(Modal::Confirm {
            act: Act::Poweroff,
            vm: "gone".into(),
        });
        a.key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(a.spawned.is_empty());
        assert!(a.notes.iter().any(|n| n.text.contains("gone")));
        // Anything else cancels.
        a.modal = Some(Modal::Confirm {
            act: Act::Reset,
            vm: "win11".into(),
        });
        a.key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(a.modal.is_none() && a.spawned.is_empty());
    }

    #[test]
    fn only_the_terminal_thread_restores_it_on_a_panic() {
        use std::cell::Cell;
        let main = std::thread::current().id();
        let (restored, reported) = (Cell::new(0), Cell::new(0));
        on_panic(main, &|| restored.set(restored.get() + 1), &|| {
            reported.set(reported.get() + 1)
        });
        assert_eq!((restored.get(), reported.get()), (1, 1));
        let n = std::thread::spawn(move || {
            let hits = Cell::new(0);
            on_panic(main, &|| hits.set(hits.get() + 1), &|| {
                hits.set(hits.get() + 1)
            });
            hits.get()
        })
        .join()
        .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_sampler_failure_is_a_note_once() {
        let mut a = app();
        let snap = |seq, e: Option<&str>| Snapshot {
            seq,
            error: e.map(String::from),
            ..a.snap.clone()
        };
        let (s2, s3, s4) = (
            snap(2, Some("the sampler failed: x")),
            snap(3, Some("the sampler failed: x")),
            snap(4, None),
        );
        a.absorb(s2);
        a.absorb(s3);
        a.absorb(s4);
        assert_eq!(
            a.notes
                .iter()
                .filter(|n| n.text.contains("sampler failed"))
                .count(),
            1
        );
    }

    #[test]
    fn the_program_after_an_upgrade_replaced_it() {
        let d = std::env::temp_dir().join(format!("conduit-exe-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let bin = d.join("conduit");
        std::fs::write(&bin, b"").unwrap();
        let s = bin.display().to_string();
        assert_eq!(exe_path(Some(bin.clone())), s);
        assert_eq!(exe_path(Some(PathBuf::from(format!("{s} (deleted)")))), s);
        assert_eq!(exe_path(Some(d.join("gone (deleted)"))), "conduit");
        assert_eq!(exe_path(None), "conduit");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn jobs_run_in_their_own_session_and_their_log_is_tailed() {
        let d = std::env::temp_dir().join(format!("conduit-job-{}", std::process::id()));
        let log = d.join("dashboard-x.log");
        let mut child = spawn_logged(
            "sh",
            &[
                "-c".into(),
                "echo \"sid $(ps -o sid= -p $$ | tr -d ' ') pid $$\"; printf 'a\\033[1mb\\033[0m\\nprog 1\\rprog 2\\nerr\\n' >&2; printf 'no newline'".into(),
            ],
            &log,
        )
        .unwrap();
        let mut t = LogTail::open(&log).unwrap();
        assert!(child.wait().unwrap().success());
        let lines = t.lines(false);
        let (sid, pid) = {
            let w: Vec<&str> = lines[0].split_whitespace().collect();
            (w[1].to_string(), w[3].to_string())
        };
        assert_eq!(sid, pid, "own session: {lines:?}");
        assert_eq!(lines[1..], ["ab", "prog 2", "err"]);
        assert_eq!(t.lines(true), ["no newline"]);
        assert!(t.lines(true).is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn idle_frames_are_not_redrawn() {
        let mut a = app();
        a.intro = 0;
        assert!(!a.animating());
        a.capturing = Some(Tab::Doctor);
        assert!(a.animating());
        a.capturing = None;
        a.tab = Tab::Doctor;
        a.doctor.clear();
        assert!(a.animating(), "checking… spinner");
        a.tab = Tab::Dash;
        a.dirty = false;
        let s = Snapshot {
            seq: 9,
            ..a.snap.clone()
        };
        a.absorb(s.clone());
        assert!(a.dirty, "a new snapshot is drawn");
        a.dirty = false;
        a.absorb(s);
        assert!(!a.dirty, "the same snapshot is not");
        a.key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(a.dirty);
    }

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[0m done"), "ok done");
    }
}

#[cfg(test)]
mod preview {
    #[test]
    #[ignore]
    fn print_frame() {
        use ratatui::backend::TestBackend;
        let mut t = super::Terminal::new(TestBackend::new(170, 46)).unwrap();
        let mut a = super::tests::app_pub();
        t.draw(|f| super::draw::draw(f, &mut a)).unwrap();
        let b = t.backend().buffer().clone();
        for y in 0..b.area.height {
            let mut s = String::new();
            for x in 0..b.area.width {
                s.push_str(b[(x, y)].symbol());
            }
            println!("{s}");
        }
    }
}
