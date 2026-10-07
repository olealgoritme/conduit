//! `conduit setup`: a guided first run. A pure state machine ([`App`]) over
//! plain data ([`Step`], [`Fix`], [`Recipe`]); `view` only draws it and `term`
//! only feeds it keys and runs the commands it asks for.
//!
//! One rule per thing: host findings are `doctor::Check`s (nothing is checked
//! twice), package lists are in `data` (mirrored from packaging/build.sh by a
//! test), and every action the wizard takes is a command line shown in full
//! and run only after Enter.

pub mod data;
pub mod domain;
pub mod env;
pub mod hostfix;
pub mod nfpm;
pub mod plan;
pub mod recipes;
pub mod term;
pub mod view;

use crate::doctor::Level;
pub use env::{Env, Family};
pub use recipes::Recipe;

/// What the wizard can do about a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Fix {
    /// A command, shown exactly, run after Enter. `needs_sudo` puts `sudo`
    /// in front (and asks for the password on the real terminal first).
    Run { cmd: Vec<String>, needs_sudo: bool },
    /// Words only: what to do by hand (the wizard never does these for you).
    Guide { text: String },
    /// A page to read.
    Open { url: String },
    /// A value only the user knows.
    Ask { field: Field },
}

impl Fix {
    pub fn run(cmd: &[&str], needs_sudo: bool) -> Fix {
        Fix::Run {
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            needs_sudo,
        }
    }

    pub fn guide(text: impl Into<String>) -> Fix {
        Fix::Guide { text: text.into() }
    }

    /// The command line as the user would type it.
    pub fn command_line(&self) -> Option<String> {
        match self {
            Fix::Run { cmd, needs_sudo } => Some(command_line(cmd, *needs_sudo)),
            _ => None,
        }
    }
}

/// What actually runs. `needs_sudo` puts `sudo` in front, except for
/// `conduit` itself, which asks for sudo only where it needs it (and must not
/// run as root: its files would be root's).
pub fn effective_argv(cmd: &[String], needs_sudo: bool) -> Vec<String> {
    let mut v = Vec::new();
    if needs_sudo && cmd.first().map(String::as_str) != Some("conduit") {
        v.push("sudo".to_string());
    }
    v.extend(cmd.iter().cloned());
    v
}

/// The command as typed in a shell: what the dialog shows and the runner runs.
pub fn command_line(cmd: &[String], needs_sudo: bool) -> String {
    effective_argv(cmd, needs_sudo)
        .iter()
        .map(|w| crate::ui::shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    VmName,
    IsoPath,
    IsoSha256,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::VmName => "the VM's name",
            Field::IsoPath => "the path of the installer ISO",
            Field::IsoSha256 => "the ISO's sha256 (empty to skip)",
        }
    }
}

/// How a step is known to be done after its fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Check again (the check looks at the real world).
    Recheck,
    /// The command exiting 0 is the proof.
    Exit0,
    /// Only the user can tell: they confirm it.
    Confirm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    Done(String),
    /// Not done, but nothing stops the next step.
    Warn(String),
    Todo(String),
}

impl CheckResult {
    pub fn detail(&self) -> &str {
        match self {
            CheckResult::Done(d) | CheckResult::Warn(d) | CheckResult::Todo(d) => d,
        }
    }

    pub fn from_level(level: Level, detail: &str) -> CheckResult {
        match level {
            Level::Ok => CheckResult::Done(detail.into()),
            Level::Warn => CheckResult::Warn(detail.into()),
            Level::Fail => CheckResult::Todo(detail.into()),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, CheckResult::Done(_))
    }

    pub fn is_blocking(&self) -> bool {
        matches!(self, CheckResult::Todo(_))
    }
}

pub struct Step {
    pub id: String,
    pub title: String,
    pub explanation: String,
    pub check: Box<dyn Fn(&Env) -> CheckResult>,
    pub fix: Option<Fix>,
    pub verify: Verify,
}

impl Step {
    pub fn new(
        id: &str,
        title: &str,
        explanation: &str,
        check: impl Fn(&Env) -> CheckResult + 'static,
        fix: Option<Fix>,
        verify: Verify,
    ) -> Step {
        Step {
            id: id.into(),
            title: title.into(),
            explanation: explanation.into(),
            check: Box::new(check),
            fix,
            verify,
        }
    }
}

pub struct Row {
    pub step: Step,
    pub status: CheckResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Welcome,
    HostCheck,
    ChooseGuest,
    Recipe,
    FirstRun,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// "Run this?" with the exact command.
    Confirm { cmd: Vec<String>, needs_sudo: bool },
    /// Read this; Enter closes (and marks a `Verify::Confirm` step done).
    Guide { title: String, text: String },
    /// Type a value.
    Input { field: Field, buf: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Esc,
    Up,
    Down,
    Backspace,
    Char(char),
    CtrlC,
}

/// What the runner must do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Spawn {
        argv: Vec<String>,
        needs_sudo: bool,
    },
    Open(String),
    /// Re-read the computer (checks, VMs) and call `App::set_env`.
    Refresh,
    Kill,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub command: String,
    pub exit: Option<i32>,
}

pub struct App {
    pub env: Env,
    pub recipes: Vec<Recipe>,
    pub screen: Screen,
    pub host: Vec<Row>,
    pub guests: usize,
    pub steps: Vec<Row>,
    pub cursor: usize,
    pub dialog: Option<Dialog>,
    pub output: Vec<String>,
    pub running: Option<Running>,
    pub message: String,
    /// The step a running command or an open guide belongs to.
    pending: Option<(String, Verify)>,
}

impl App {
    pub fn new(env: Env) -> App {
        let mut a = App {
            env,
            recipes: recipes::all(),
            screen: Screen::Welcome,
            host: Vec::new(),
            guests: 0,
            steps: Vec::new(),
            cursor: 0,
            dialog: None,
            output: Vec::new(),
            running: None,
            message: String::new(),
            pending: None,
        };
        a.rebuild();
        a
    }

    pub fn recipe(&self) -> &Recipe {
        &self.recipes[self.guests]
    }

    /// Recompute every step from `env`.
    fn rebuild(&mut self) {
        let eval = |steps: Vec<Step>, env: &Env| -> Vec<Row> {
            steps
                .into_iter()
                .map(|step| {
                    let status = (step.check)(env);
                    Row { step, status }
                })
                .collect()
        };
        self.host = eval(hostfix::host_steps(&self.env), &self.env);
        self.steps = eval((self.recipes[self.guests].steps)(&self.env), &self.env);
        let n = self.rows().len();
        self.cursor = self.cursor.min(n.saturating_sub(1));
    }

    pub fn set_env(&mut self, env: Env) {
        self.env = env;
        self.rebuild();
    }

    pub fn rows(&self) -> &[Row] {
        match self.screen {
            Screen::HostCheck => &self.host,
            Screen::Recipe => &self.steps,
            _ => &[],
        }
    }

    pub fn failing(rows: &[Row]) -> usize {
        rows.iter().filter(|r| r.status.is_blocking()).count()
    }

    fn goto(&mut self, s: Screen) {
        self.screen = s;
        self.cursor = 0;
        self.message.clear();
        if s == Screen::Recipe {
            self.rebuild();
        }
    }

    pub fn key(&mut self, k: Key) -> Action {
        if self.running.as_ref().is_some_and(|r| r.exit.is_none()) {
            return match k {
                Key::Esc | Key::CtrlC => Action::Kill,
                _ => Action::None,
            };
        }
        if self.running.is_some() {
            // The finished command's output was on screen; this key only closes it.
            self.running = None;
            return Action::None;
        }
        if k == Key::CtrlC {
            return Action::Quit;
        }
        if self.dialog.is_some() {
            return self.dialog_key(k);
        }
        match self.screen {
            Screen::Welcome => match k {
                Key::Enter => {
                    self.goto(Screen::HostCheck);
                    Action::None
                }
                Key::Esc | Key::Char('q') => Action::Quit,
                _ => Action::None,
            },
            Screen::HostCheck | Screen::Recipe => self.list_key(k),
            Screen::ChooseGuest => self.guest_key(k),
            Screen::FirstRun => match k {
                Key::Enter => {
                    self.goto(Screen::Done);
                    Action::None
                }
                Key::Esc => {
                    self.goto(Screen::Recipe);
                    Action::None
                }
                Key::Char('q') => Action::Quit,
                _ => Action::None,
            },
            Screen::Done => match k {
                Key::Enter | Key::Esc | Key::Char('q') => Action::Quit,
                _ => Action::None,
            },
        }
    }

    fn move_cursor(&mut self, k: Key) -> bool {
        let n = if self.screen == Screen::ChooseGuest {
            self.recipes.len()
        } else {
            self.rows().len()
        };
        match k {
            Key::Up if self.cursor > 0 => self.cursor -= 1,
            Key::Down if self.cursor + 1 < n => self.cursor += 1,
            Key::Char('k') if self.cursor > 0 => self.cursor -= 1,
            Key::Char('j') if self.cursor + 1 < n => self.cursor += 1,
            _ => return matches!(k, Key::Up | Key::Down | Key::Char('j') | Key::Char('k')),
        }
        true
    }

    fn list_key(&mut self, k: Key) -> Action {
        if self.move_cursor(k) {
            return Action::None;
        }
        let host = self.screen == Screen::HostCheck;
        let blocked = Self::failing(self.rows());
        match k {
            Key::Char('r') => Action::Refresh,
            Key::Enter => self.open_fix(),
            Key::Char('n') | Key::Char('c') => {
                if blocked > 0 && k == Key::Char('n') {
                    self.message = format!(
                        "{blocked} step(s) still to do; fix them, or press c to continue anyway"
                    );
                    return Action::None;
                }
                self.goto(if host {
                    Screen::ChooseGuest
                } else {
                    Screen::FirstRun
                });
                Action::None
            }
            Key::Esc => {
                self.goto(if host {
                    Screen::Welcome
                } else {
                    Screen::ChooseGuest
                });
                Action::None
            }
            Key::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    fn guest_key(&mut self, k: Key) -> Action {
        if self.move_cursor(k) {
            return Action::None;
        }
        match k {
            Key::Enter => {
                let r = &self.recipes[self.cursor];
                if let Some(doc) = r.experimental_doc {
                    self.dialog = Some(Dialog::Guide {
                        title: format!("{} (experimental)", r.name),
                        text: format!(
                            "Conduit does not set this one up for you. Follow {doc} in the Conduit sources: it lists the exact steps, the Helios driver, and what is known not to work."
                        ),
                    });
                    return Action::None;
                }
                let default = r.default_name;
                // A name the user typed stays; a recipe's own default follows the recipe.
                let untouched = self.env.vm_name.is_empty()
                    || self
                        .recipes
                        .iter()
                        .any(|r| r.default_name == self.env.vm_name);
                if untouched {
                    self.env.vm_name = default.to_string();
                }
                self.guests = self.cursor;
                self.goto(Screen::Recipe);
                Action::Refresh
            }
            Key::Esc => {
                self.goto(Screen::HostCheck);
                Action::None
            }
            Key::Char('q') => Action::Quit,
            _ => Action::None,
        }
    }

    fn open_fix(&mut self) -> Action {
        let Some(row) = self.rows().get(self.cursor) else {
            return Action::None;
        };
        let (id, verify, title) = (row.step.id.clone(), row.step.verify, row.step.title.clone());
        let fix = row.step.fix.clone();
        let expl = row.step.explanation.clone();
        let Some(fix) = fix else {
            self.message = "nothing to do here".into();
            return Action::None;
        };
        self.pending = Some((id, verify));
        match fix {
            Fix::Run { cmd, needs_sudo } => {
                self.dialog = Some(Dialog::Confirm { cmd, needs_sudo });
                Action::None
            }
            Fix::Guide { text } => {
                let text = if expl.is_empty() || text.contains(&expl) {
                    text
                } else {
                    format!("{expl}\n\n{text}")
                };
                self.dialog = Some(Dialog::Guide { title, text });
                Action::None
            }
            Fix::Open { url } => {
                self.message = format!("opening {url}");
                Action::Open(url)
            }
            Fix::Ask { field } => {
                let buf = self.env.field(field).unwrap_or_default();
                self.dialog = Some(Dialog::Input { field, buf });
                Action::None
            }
        }
    }

    fn dialog_key(&mut self, k: Key) -> Action {
        let Some(d) = self.dialog.clone() else {
            return Action::None;
        };
        match (d, k) {
            (Dialog::Confirm { cmd, needs_sudo }, Key::Enter) => {
                self.dialog = None;
                self.output.clear();
                self.running = Some(Running {
                    command: command_line(&cmd, needs_sudo),
                    exit: None,
                });
                Action::Spawn {
                    argv: cmd,
                    needs_sudo,
                }
            }
            (Dialog::Guide { .. }, Key::Enter) => {
                self.dialog = None;
                if let Some((id, Verify::Confirm)) = self.pending.take() {
                    self.env.confirm(&id);
                    self.rebuild();
                }
                Action::None
            }
            (Dialog::Input { field, buf }, Key::Enter) => {
                self.dialog = None;
                self.env.set_field(field, buf.trim());
                self.rebuild();
                Action::None
            }
            (Dialog::Input { field, mut buf }, Key::Char(c)) => {
                buf.push(c);
                self.dialog = Some(Dialog::Input { field, buf });
                Action::None
            }
            (Dialog::Input { field, mut buf }, Key::Backspace) => {
                buf.pop();
                self.dialog = Some(Dialog::Input { field, buf });
                Action::None
            }
            (_, Key::Esc) => {
                self.dialog = None;
                self.pending = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.output.push(line);
        if self.output.len() > 2000 {
            self.output.drain(..500);
        }
    }

    /// The runner reports the command's end.
    pub fn run_finished(&mut self, code: i32) -> Action {
        if let Some(r) = &mut self.running {
            r.exit = Some(code);
        }
        if code == 0 {
            if let Some((id, Verify::Exit0)) = self.pending.take() {
                self.env.confirm(&id);
            }
        }
        self.pending = None;
        Action::Refresh
    }
}

#[cfg(test)]
mod tests;
