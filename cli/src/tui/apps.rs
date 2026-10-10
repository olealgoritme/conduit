//! The dashboard's Apps tab: the apps installed in the selected VM, read over
//! the guest control channel ([`crate::ctl`]) on a background thread, so the
//! dashboard never waits on a guest. Each VM keeps its last list; a refresh
//! shows it dimmed, and a failure keeps it on screen under the reason.

use super::{data, Level};
use crate::ctl;
use conduit_ctl::App;
use crossterm::event::KeyCode;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Reload a list this often while the tab is open.
const REFRESH: Duration = Duration::from_secs(60);
/// Try again this often after a failure (the VM or its agent may be coming up).
const RETRY: Duration = Duration::from_secs(8);
/// A load that has not answered after this long is abandoned.
const GIVE_UP: Duration = Duration::from_secs(90);

#[derive(Default)]
pub struct Entry {
    /// The last list that loaded, sorted by name.
    pub list: Vec<App>,
    pub loaded: Option<Instant>,
    pub loading: bool,
    /// Why the last load failed (message, then the fix on further lines).
    pub error: Option<String>,
    tried: Option<Instant>,
}

enum Msg {
    Loaded(String, Result<Vec<App>, String>),
    Note(Level, String),
}

pub struct AppsTab {
    pub sel: usize,
    pub filter: String,
    pub typing: bool,
    pub vms: HashMap<String, Entry>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

/// An error and its fix as lines for the screen.
fn explain(e: &anyhow::Error) -> String {
    let mut s = format!("{e}");
    if let Some(h) = crate::ui::hint_of(e) {
        s.push('\n');
        s.push_str(&h);
    }
    s
}

/// Why the tab has nothing to ask for, if so.
pub fn unavailable(vm: &data::Vm) -> Option<String> {
    if vm.libvirt.is_none() {
        Some(format!(
            "{0} is not a libvirt VM of Conduit's\nThe control channel needs one: `conduit attach {0}`",
            vm.name
        ))
    } else if !vm.up() {
        Some(format!(
            "{0} is not running\nStart it with s (or ⏎ for a window); its apps appear when the guest is up",
            vm.name
        ))
    } else if vm.state == "paused" {
        Some(format!("{0} is paused\nResume it with p", vm.name))
    } else {
        None
    }
}

/// Entries whose name or source contains `filter` (ignoring case).
pub fn filtered<'a>(list: &'a [App], filter: &str) -> Vec<&'a App> {
    let f = filter.to_lowercase();
    list.iter()
        .filter(|a| {
            f.is_empty()
                || a.name.to_lowercase().contains(&f)
                || ctl::source_label(&a.source).to_lowercase().contains(&f)
        })
        .collect()
}

impl AppsTab {
    pub fn new() -> AppsTab {
        let (tx, rx) = mpsc::channel();
        AppsTab {
            sel: 0,
            filter: String::new(),
            typing: false,
            vms: HashMap::new(),
            tx,
            rx,
        }
    }

    /// Take in finished loads; returns messages for the activity log.
    pub fn pump(&mut self) -> Vec<(Level, String)> {
        let mut notes = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Loaded(vm, r) => {
                    let e = self.vms.entry(vm).or_default();
                    e.loading = false;
                    match r {
                        Ok(list) => {
                            e.list = list;
                            e.loaded = Some(Instant::now());
                            e.error = None;
                        }
                        Err(msg) => e.error = Some(msg),
                    }
                }
                Msg::Note(l, t) => notes.push((l, t)),
            }
        }
        notes
    }

    /// Start a load when the tab is showing a VM that needs one.
    pub fn tick(&mut self, vm: Option<&data::Vm>) {
        let Some(vm) = vm else { return };
        if unavailable(vm).is_some() {
            return;
        }
        let e = self.vms.entry(vm.name.clone()).or_default();
        let age = |t: Option<Instant>| t.map(|t| t.elapsed());
        if e.loading && age(e.tried).is_some_and(|a| a < GIVE_UP) {
            return;
        }
        let due = match (&e.error, e.loaded) {
            (_, None) if e.tried.is_none() => true,
            (Some(_), _) => age(e.tried).is_none_or(|a| a >= RETRY),
            (None, Some(t)) => t.elapsed() >= REFRESH,
            (None, None) => age(e.tried).is_none_or(|a| a >= RETRY),
        };
        if due {
            self.load(&vm.name.clone());
        }
    }

    /// Reload now.
    pub fn load(&mut self, vm: &str) {
        let e = self.vms.entry(vm.to_string()).or_default();
        e.loading = true;
        e.tried = Some(Instant::now());
        let (tx, name) = (self.tx.clone(), vm.to_string());
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| ctl::list_apps(&name))
                .map_err(|_| "internal error while reading the app list".to_string())
                .and_then(|r| r.map_err(|e| explain(&e)));
            let _ = tx.send(Msg::Loaded(name, r));
        });
    }

    fn picked(&self, vm: &str) -> Option<App> {
        let e = self.vms.get(vm)?;
        filtered(&e.list, &self.filter)
            .get(self.sel)
            .map(|a| (*a).clone())
    }

    fn spawn(&self, what: impl FnOnce() -> (Level, String) + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (l, t) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(what))
                .unwrap_or((Level::Err, "internal error".into()));
            let _ = tx.send(Msg::Note(l, t));
        });
    }

    /// Enter: start the selected app.
    pub fn run_selected(&mut self, vm: &str) {
        let Some(app) = self.picked(vm) else { return };
        let vm = vm.to_string();
        self.spawn(move || match ctl::run_app(&vm, &app) {
            Ok(_) => (Level::Ok, format!("{vm}: started {}", app.name)),
            Err(e) => (Level::Err, explain(&e).replace('\n', " · ")),
        });
    }

    /// `a`: add a host launcher for the selected app.
    pub fn add_selected(&mut self, vm: &str) {
        let Some(app) = self.picked(vm) else { return };
        let vm = vm.to_string();
        self.spawn(move || match ctl::add_shortcut(&vm, &app.name) {
            Ok((p, w)) => {
                let extra = if w.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", w.join("; "))
                };
                (
                    Level::Ok,
                    format!(
                        "{vm}: launcher added for {}: {}{extra}",
                        app.name,
                        p.display()
                    ),
                )
            }
            Err(e) => (Level::Err, explain(&e).replace('\n', " · ")),
        });
    }

    /// Keys of the tab; true when consumed. `vm` is the selected VM, if any.
    pub fn key(&mut self, code: KeyCode, vm: Option<&data::Vm>) -> bool {
        let n = vm
            .and_then(|v| self.vms.get(&v.name))
            .map_or(0, |e| filtered(&e.list, &self.filter).len());
        if self.typing {
            match code {
                KeyCode::Esc => {
                    self.typing = false;
                    self.filter.clear();
                    self.sel = 0;
                }
                KeyCode::Enter => self.typing = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sel = 0;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.sel = 0;
                }
                KeyCode::Up => self.sel = self.sel.saturating_sub(1),
                KeyCode::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = n.saturating_sub(1),
            KeyCode::Char('/') => {
                self.typing = true;
                self.filter.clear();
                self.sel = 0;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.sel = 0;
            }
            KeyCode::Enter => {
                if let Some(v) = vm {
                    self.run_selected(&v.name);
                }
            }
            KeyCode::Char('a') => {
                if let Some(v) = vm {
                    self.add_selected(&v.name);
                }
            }
            KeyCode::Char('r') => {
                if let Some(v) = vm.filter(|v| unavailable(v).is_none()) {
                    self.load(&v.name);
                }
            }
            _ => return false,
        }
        true
    }

    /// Switching VMs starts the list at the top.
    pub fn reset_view(&mut self) {
        self.sel = 0;
        self.filter.clear();
        self.typing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(n: &str, s: &str) -> App {
        App {
            name: n.into(),
            source: s.into(),
            target: format!("t:{n}"),
            ..App::default()
        }
    }

    fn vm(state: &str, libvirt: bool) -> data::Vm {
        data::Vm {
            name: "win11".into(),
            state: state.into(),
            libvirt: libvirt.then(|| "qemu:///session".into()),
            ..data::Vm::default()
        }
    }

    #[test]
    fn says_why_a_vm_has_no_apps() {
        assert!(unavailable(&vm("running", false))
            .unwrap()
            .contains("conduit attach win11"));
        assert!(unavailable(&vm("stopped", true))
            .unwrap()
            .contains("not running"));
        assert!(unavailable(&vm("paused", true)).unwrap().contains("paused"));
        assert!(unavailable(&vm("running", true)).is_none());
    }

    #[test]
    fn filter_matches_name_and_source() {
        let l = vec![
            app("Notepad", "startmenu"),
            app("Portal", "steam"),
            app("Gimp", "desktop"),
        ];
        assert_eq!(filtered(&l, "").len(), 3);
        assert_eq!(filtered(&l, "PORT")[0].name, "Portal");
        assert_eq!(filtered(&l, "steam")[0].name, "Portal");
        assert_eq!(filtered(&l, "start menu")[0].name, "Notepad");
        assert!(filtered(&l, "zzz").is_empty());
    }

    #[test]
    fn keys_move_filter_and_leave_other_keys_alone() {
        let mut t = AppsTab::new();
        let v = vm("running", true);
        t.vms.insert(
            "win11".into(),
            Entry {
                list: vec![app("A", "steam"), app("B", "steam"), app("C", "steam")],
                ..Entry::default()
            },
        );
        assert!(t.key(KeyCode::Down, Some(&v)));
        assert!(t.key(KeyCode::Char('j'), Some(&v)));
        assert!(t.key(KeyCode::Char('j'), Some(&v)));
        assert_eq!(t.sel, 2, "clamped to the last");
        assert!(t.key(KeyCode::Home, Some(&v)));
        assert_eq!(t.sel, 0);
        assert!(
            !t.key(KeyCode::Char('3'), Some(&v)),
            "tab switching is not ours"
        );
        assert!(!t.key(KeyCode::Char('q'), Some(&v)));
        t.key(KeyCode::Char('/'), Some(&v));
        assert!(t.typing);
        t.key(KeyCode::Char('q'), Some(&v));
        assert!(t.key(KeyCode::Char('x'), Some(&v)), "typing eats keys");
        assert_eq!(t.filter, "qx");
        t.key(KeyCode::Esc, Some(&v));
        assert!(!t.typing && t.filter.is_empty());
        // No VM selected: nothing to act on, nothing panics.
        assert!(t.key(KeyCode::Enter, None));
        assert!(t.key(KeyCode::Char('a'), None));
    }

    #[test]
    fn a_failed_reload_keeps_the_old_list() {
        let mut t = AppsTab::new();
        t.vms.entry("win11".into()).or_default().list = vec![app("A", "steam")];
        t.vms.get_mut("win11").unwrap().loading = true;
        t.tx.send(Msg::Loaded(
            "win11".into(),
            Err("agent gone\nfix it".into()),
        ))
        .unwrap();
        t.pump();
        let e = &t.vms["win11"];
        assert_eq!(e.list.len(), 1);
        assert!(!e.loading);
        assert!(e.error.as_deref().unwrap().contains("agent gone"));
        t.tx.send(Msg::Loaded(
            "win11".into(),
            Ok(vec![app("B", "steam"), app("C", "steam")]),
        ))
        .unwrap();
        t.pump();
        let e = &t.vms["win11"];
        assert_eq!(e.list.len(), 2);
        assert!(e.error.is_none() && e.loaded.is_some());
    }

    #[test]
    fn tick_does_not_load_for_a_vm_that_cannot_answer() {
        let mut t = AppsTab::new();
        t.tick(Some(&vm("stopped", true)));
        t.tick(Some(&vm("running", false)));
        t.tick(None);
        assert!(t.vms.is_empty(), "no thread, no entry");
    }
}
