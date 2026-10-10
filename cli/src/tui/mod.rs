//! `conduit` with no command: a live dashboard of every VM, the host and its
//! GPU, with the everyday actions one key (or click) away.
//!
//! Actions run this same program (`conduit view NAME`, `conduit shutdown
//! NAME`, …) as child processes, so the dashboard does exactly what the
//! commands do. `view` and `up` are detached and log to the VM's runtime
//! folder, so a window opened from here outlives the dashboard.

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
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use data::Snapshot;

/// History length of the charts, in samples (one a second).
pub const HIST: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Dash,
    Logs,
    Doctor,
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
    Help,
}

struct Job {
    vm: String,
    label: String,
    child: Child,
    log: Option<std::path::PathBuf>,
    started: Instant,
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
    pub rows: Vec<(Rect, usize)>,
    pub buttons: Vec<(Rect, Act)>,
    pub tabs: Vec<(Rect, Tab)>,
    jobs: Vec<Job>,
    lines_tx: mpsc::Sender<(Level, String)>,
    lines_rx: mpsc::Receiver<(Level, String)>,
    cap_tx: mpsc::Sender<(Tab, Vec<String>)>,
    cap_rx: mpsc::Receiver<(Tab, Vec<String>)>,
    logs_at: Option<Instant>,
    last_seq: u64,
    poke: mpsc::Sender<()>,
    quit: bool,
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
            .find(|j| j.vm == vm && j.log.is_none())
            .map(|j| j.label.as_str())
    }

    pub fn note(&mut self, level: Level, text: impl Into<String>) {
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
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "conduit".into())
    }

    /// Runs `conduit ARGS…`; output lines land in the activity log.
    fn run(&mut self, vm: &str, label: &str, args: Vec<String>) {
        let mut cmd = Command::new(Self::exe());
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                for r in [
                    child
                        .stdout
                        .take()
                        .map(|r| Box::new(r) as Box<dyn Read + Send>),
                    child
                        .stderr
                        .take()
                        .map(|r| Box::new(r) as Box<dyn Read + Send>),
                ]
                .into_iter()
                .flatten()
                {
                    let tx = self.lines_tx.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(r).split(b'\n').map_while(Result::ok) {
                            let t = strip_ansi(&String::from_utf8_lossy(&l));
                            let t = t.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("");
                            if !t.trim().is_empty() {
                                let _ = tx.send((Level::Info, t.trim_end().to_string()));
                            }
                        }
                    });
                }
                self.note(Level::Info, format!("{label} {vm}…"));
                self.jobs.push(Job {
                    vm: vm.into(),
                    label: label.into(),
                    child,
                    log: None,
                    started: Instant::now(),
                });
            }
            Err(e) => self.note(Level::Err, format!("could not run conduit: {e}")),
        }
    }

    /// Runs `conduit ARGS…` in its own session with output in a log file,
    /// so it keeps running after the dashboard quits (`view`, `up`).
    fn detach(&mut self, vm: &str, label: &str, args: Vec<String>) {
        let log = crate::paths::run_dir(vm).join(format!("dashboard-{}.log", args[0]));
        let _ = std::fs::create_dir_all(crate::paths::run_dir(vm));
        let file = match std::fs::File::create(&log) {
            Ok(f) => f,
            Err(e) => {
                self.note(Level::Err, format!("{}: {e}", log.display()));
                return;
            }
        };
        let err = file.try_clone();
        let mut cmd = Command::new(Self::exe());
        cmd.args(&args).stdin(Stdio::null()).stdout(file);
        if let Ok(e) = err {
            cmd.stderr(e);
        }
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                self.note(Level::Info, format!("{label} {vm}"));
                self.jobs.push(Job {
                    vm: vm.into(),
                    label: label.into(),
                    child,
                    log: Some(log),
                    started: Instant::now(),
                });
            }
            Err(e) => self.note(Level::Err, format!("could not run conduit: {e}")),
        }
        let _ = self.poke.send(());
    }

    fn venus_args(&self, vm: &data::Vm, args: &mut Vec<String>) {
        if vm.windows {
            args.push("--venus".into());
        }
    }

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
                if self.modal.is_none() {
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
            Tab::Dash => return,
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
        for (i, j) in self.jobs.iter_mut().enumerate() {
            if let Ok(Some(st)) = j.child.try_wait() {
                done.push((i, st));
            }
        }
        for (i, st) in done.into_iter().rev() {
            let j = self.jobs.remove(i);
            let secs = j.started.elapsed().as_secs_f32();
            match (&j.log, st.success()) {
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
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if let Some(m) = &mut self.modal {
            match m {
                Modal::Help => self.modal = None,
                Modal::Confirm { act, .. } => match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        let a = *act;
                        self.act(a);
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
            return;
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
            },
            KeyCode::Down | KeyCode::Char('j') => match self.tab {
                Tab::Dash => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
                Tab::Logs => self.logs_scroll = self.logs_scroll.saturating_sub(1),
                Tab::Doctor => self.doctor_scroll = self.doctor_scroll.saturating_add(1),
            },
            KeyCode::PageUp => self.logs_scroll = self.logs_scroll.saturating_add(20),
            KeyCode::PageDown => self.logs_scroll = self.logs_scroll.saturating_sub(20),
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Dash => Tab::Logs,
                    Tab::Logs => Tab::Doctor,
                    Tab::Doctor => Tab::Dash,
                };
                if self.tab != Tab::Dash {
                    let t = self.tab;
                    self.capture(t);
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Dash,
            KeyCode::Char('2') => self.act(Act::Logs),
            KeyCode::Char('3') => self.act(Act::Doctor),
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
            KeyCode::Char('l') => self.act(Act::Logs),
            KeyCode::Char('D') => self.act(Act::Doctor),
            KeyCode::Char('?') | KeyCode::F(1) => self.act(Act::Help),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16) {
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

/// `conduit` alone.
pub fn run() -> Result<()> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
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
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let (shared, poke) = data::start();
    let (lines_tx, lines_rx) = mpsc::channel();
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
        rows: Vec::new(),
        buttons: Vec::new(),
        tabs: Vec::new(),
        jobs: Vec::new(),
        lines_tx,
        lines_rx,
        cap_tx,
        cap_rx,
        logs_at: None,
        last_seq: 0,
        poke,
        quit: false,
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
        while let Ok((lvl, l)) = app.lines_rx.try_recv() {
            app.note(lvl, l);
        }
        while let Ok((tab, lines)) = app.cap_rx.try_recv() {
            match tab {
                Tab::Logs => {
                    app.logs = lines;
                    app.logs_at = Some(Instant::now());
                }
                Tab::Doctor => app.doctor = lines,
                Tab::Dash => {}
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
        app.reap();
        term.draw(|f| draw::draw(f, &mut app))?;
        app.tick = app.tick.wrapping_add(1);
        app.intro = app.intro.saturating_sub(1);
        let frame = if app.intro > 0 { 45 } else { 100 };
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
                _ => {}
            }
        }
    }
    drop(term);
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
        let (lines_tx, lines_rx) = mpsc::channel();
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
            rows: vec![],
            buttons: vec![],
            tabs: vec![],
            jobs: vec![],
            lines_tx,
            lines_rx,
            cap_tx,
            cap_rx,
            logs_at: None,
            last_seq: 1,
            poke,
            quit: false,
        }
    }

    #[test]
    fn draws_every_tab_and_modal_at_any_size() {
        use ratatui::backend::TestBackend;
        for (w, h) in [(20, 8), (80, 24), (120, 40), (240, 70)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut a = app();
            for tab in [Tab::Dash, Tab::Logs, Tab::Doctor] {
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
            ] {
                a.modal = Some(m);
                t.draw(|f| draw::draw(f, &mut a)).unwrap();
            }
        }
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
