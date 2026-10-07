//! The terminal side: raw mode behind a guard, the key loop, and running the
//! commands the state machine asks for with their output streamed back.

use super::{effective_argv, plan, view, Action, App, Env, Key};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// `conduit setup [--plan]`.
pub fn run(plan_only: bool) -> Result<()> {
    let env = Env::detect();
    if plan_only || !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        print!("{}", plan::plan_text(&env));
        return Ok(());
    }
    interactive(env)
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Puts the terminal back however the program ends: return, `?`, panic.
struct Guard;

impl Guard {
    fn enter() -> Result<Guard> {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        enable_raw_mode()?;
        let g = Guard;
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
        Ok(g)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// SIGTERM/SIGHUP/SIGINT from outside end the loop, so the guard runs.
fn catch_signals() {
    for s in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe {
            libc::signal(
                s,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

fn map_key(k: event::KeyEvent) -> Option<Key> {
    if k.kind != KeyEventKind::Press {
        return None;
    }
    Some(match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlC,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    })
}

enum Msg {
    Line(String),
}

struct Job {
    child: Child,
    readers: Vec<std::thread::JoinHandle<()>>,
}

fn spawn_reader(
    r: impl Read + Send + 'static,
    tx: mpsc::Sender<Msg>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for l in BufReader::new(r).split(b'\n').map_while(Result::ok) {
            // A progress bar redraws with \r: show its last state.
            let text = String::from_utf8_lossy(&l);
            let last = text.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
            let _ = tx.send(Msg::Line(last.to_string()));
        }
    })
}

fn start(argv: &[String], needs_sudo: bool, tx: &mpsc::Sender<Msg>) -> std::io::Result<Job> {
    let mut full = effective_argv(argv, needs_sudo);
    // `conduit` means this very program, whatever PATH says.
    if full[0] == "conduit" {
        if let Ok(exe) = std::env::current_exe() {
            full[0] = exe.to_string_lossy().into_owned();
        }
    }
    let mut child = Command::new(&full[0])
        .args(&full[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let readers = vec![
        spawn_reader(child.stdout.take().unwrap(), tx.clone()),
        spawn_reader(child.stderr.take().unwrap(), tx.clone()),
    ];
    Ok(Job { child, readers })
}

/// Ask for the sudo password on the real terminal, then come back.
fn warm_sudo<B: ratatui::backend::Backend>(t: &mut Terminal<B>) -> bool {
    if crate::sys::quiet("sudo", &["-n", "true"]) {
        return true;
    }
    restore();
    println!("\nsudo needs your password for the next step.");
    let ok = Command::new("sudo")
        .args(["-v", "-p", "[sudo] password for %u: "])
        .status()
        .is_ok_and(|s| s.success());
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = t.clear();
    ok
}

fn interactive(env: Env) -> Result<()> {
    let _guard = Guard::enter()?;
    catch_signals();
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut app = App::new(env);
    let (tx, rx) = mpsc::channel();
    let mut job: Option<Job> = None;
    let mut pending = Action::None;
    loop {
        term.draw(|f| view::draw(f, &app))?;
        if SIGNALLED.load(Ordering::SeqCst) {
            break;
        }
        while let Ok(Msg::Line(l)) = rx.try_recv() {
            app.push_line(l);
        }
        if let Some(j) = &mut job {
            if let Some(st) = j.child.try_wait()? {
                for h in j.readers.drain(..) {
                    let _ = h.join();
                }
                while let Ok(Msg::Line(l)) = rx.try_recv() {
                    app.push_line(l);
                }
                job = None;
                pending = app.run_finished(st.code().unwrap_or(-1));
            }
        }
        let mut action = std::mem::replace(&mut pending, Action::None);
        if matches!(action, Action::None) && event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if let Some(key) = map_key(k) {
                    action = app.key(key);
                }
            }
        }
        match action {
            Action::None => {}
            Action::Quit => break,
            Action::Kill => {
                if let Some(j) = &job {
                    unsafe { libc::kill(j.child.id() as i32, libc::SIGTERM) };
                }
            }
            Action::Open(url) => {
                let ok = Command::new("xdg-open")
                    .arg(&url)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .is_ok();
                app.message = if ok {
                    format!("opened {url} in your browser")
                } else {
                    format!("could not open a browser; visit {url}")
                };
            }
            Action::Refresh => {
                app.message = "checking…".into();
                term.draw(|f| view::draw(f, &app))?;
                let mut env = app.env.clone();
                env.refresh();
                app.set_env(env);
                app.message.clear();
            }
            Action::Spawn { argv, needs_sudo } => {
                if needs_sudo && !warm_sudo(&mut term) {
                    app.push_line("sudo did not succeed; nothing was run".into());
                    pending = app.run_finished(1);
                    continue;
                }
                match start(&argv, needs_sudo, &tx) {
                    Ok(j) => job = Some(j),
                    Err(e) => {
                        app.push_line(format!("could not start: {e}"));
                        pending = app.run_finished(127);
                    }
                }
            }
        }
    }
    if let Some(mut j) = job {
        let _ = j.child.kill();
    }
    let _ = std::io::stdout().flush();
    Ok(())
}
