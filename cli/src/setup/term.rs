//! The terminal side: raw mode behind a guard, the key loop, and running the
//! commands the state machine asks for with their output streamed back.

use super::{effective_argv, layout, plan, view, Action, App, Env, Key};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// The termination signal that arrived, 0 for none.
static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
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

/// Where events come from: the terminal, or a test's script.
trait Input {
    /// The next event within `timeout`, `Ok(None)` when none came. An error
    /// means the terminal cannot be read any more.
    fn next(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

/// The real terminal (crossterm reads stdin, which `run` made sure is one).
struct Tty;

impl Input for Tty {
    fn next(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if event::poll(timeout)? {
            return event::read().map(Some);
        }
        // crossterm reports end of input as "no event"; a hung-up terminal
        // says so in poll(2)'s revents, which are reported even unasked.
        let mut p = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: 0,
            revents: 0,
        };
        let hung = unsafe { libc::poll(&mut p, 1, 0) } > 0
            && p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        if hung {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the terminal hung up",
            ));
        }
        Ok(None)
    }
}

/// Why the key loop ended.
#[derive(Debug)]
enum End {
    Quit,
    /// SIGHUP, SIGTERM or SIGINT arrived.
    Signal(i32),
    /// Reading the terminal or drawing on it failed: it is gone.
    Failed(io::Error),
}

impl End {
    fn exit_code(&self) -> i32 {
        match self {
            End::Quit => 0,
            End::Signal(s) => 128 + s,
            End::Failed(_) => 1,
        }
    }
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

fn start(argv: &[String], needs_sudo: bool, tx: &mpsc::Sender<Msg>) -> io::Result<Job> {
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

/// Ends a running command: SIGTERM, a moment to clean up, then SIGKILL.
fn stop(mut job: Job) {
    let pid = job.child.id() as i32;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if !matches!(job.child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = job.child.kill();
    let _ = job.child.wait();
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
    let guard = Guard::enter()?;
    catch_signals();
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let end = run_loop(&mut term, &mut Tty, &mut App::new(env), &SIGNAL);
    let _ = io::stdout().flush();
    drop(guard);
    match end {
        End::Quit => Ok(()),
        End::Signal(_) => std::process::exit(end.exit_code()),
        End::Failed(ref e) => {
            // The terminal may be gone: eprintln! would panic on EIO.
            let _ = writeln!(io::stderr(), "conduit: error: the terminal failed: {e}");
            std::process::exit(end.exit_code())
        }
    }
}

/// The key loop. One rule for how it ends: at the first termination signal
/// (looked at before every turn and after every wait) or the first error
/// reading or drawing the terminal, never retried, and any running command
/// is stopped on the way out.
fn run_loop<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    input: &mut impl Input,
    app: &mut App,
    signal: &AtomicI32,
) -> End {
    let mut job = None;
    let end = turns(term, input, app, signal, &mut job).unwrap_or_else(End::Failed);
    if let Some(j) = job {
        stop(j);
    }
    // A hangup both signals and breaks the terminal: the signal says why.
    match signal.load(Ordering::SeqCst) {
        0 => end,
        s => End::Signal(s),
    }
}

fn turns<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    input: &mut impl Input,
    app: &mut App,
    signal: &AtomicI32,
    job: &mut Option<Job>,
) -> io::Result<End> {
    let signalled = || match signal.load(Ordering::SeqCst) {
        0 => None,
        s => Some(End::Signal(s)),
    };
    let (tx, rx) = mpsc::channel();
    let mut pending = Action::None;
    loop {
        if let Some(end) = signalled() {
            return Ok(end);
        }
        term.draw(|f| view::draw(f, app))?;
        while let Ok(Msg::Line(l)) = rx.try_recv() {
            app.push_line(l);
        }
        if let Some(j) = job {
            if let Some(st) = j.child.try_wait()? {
                for h in j.readers.drain(..) {
                    let _ = h.join();
                }
                while let Ok(Msg::Line(l)) = rx.try_recv() {
                    app.push_line(l);
                }
                *job = None;
                pending = app.run_finished(st.code().unwrap_or(-1));
            }
        }
        let mut action = std::mem::replace(&mut pending, Action::None);
        if matches!(action, Action::None) {
            if let Some(Event::Key(k)) = input.next(Duration::from_millis(50))? {
                if let Some(end) = signalled() {
                    return Ok(end);
                }
                if let Some(key) = map_key(k) {
                    if layout::key_allowed(term.size()?, key) {
                        action = app.key(key);
                    }
                }
            }
        }
        match action {
            Action::None => {}
            Action::Quit => return Ok(End::Quit),
            Action::Kill => {
                if let Some(j) = job {
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
                term.draw(|f| view::draw(f, app))?;
                let mut env = app.env.clone();
                env.refresh();
                app.set_env(env);
                app.message.clear();
            }
            Action::Spawn { argv, needs_sudo } => {
                if needs_sudo {
                    let ok = warm_sudo(term);
                    if let Some(end) = signalled() {
                        return Ok(end);
                    }
                    if !ok {
                        app.push_line("sudo did not succeed; nothing was run".into());
                        pending = app.run_finished(1);
                        continue;
                    }
                }
                match start(&argv, needs_sudo, &tx) {
                    Ok(j) => *job = Some(j),
                    Err(e) => {
                        app.push_line(format!("could not start: {e}"));
                        pending = app.run_finished(127);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::backend::{TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    /// Events from a script: call `n` (from 0) gets `f(n)`.
    struct Script<F>(usize, F);

    impl<F: FnMut(usize) -> io::Result<Option<Event>>> Input for Script<F> {
        fn next(&mut self, _: Duration) -> io::Result<Option<Event>> {
            self.0 += 1;
            (self.1)(self.0 - 1)
        }
    }

    fn eio() -> io::Error {
        io::Error::from_raw_os_error(libc::EIO)
    }

    fn enter() -> Event {
        Event::Key(KeyEvent::from(KeyCode::Enter))
    }

    fn term() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(100, 30)).unwrap()
    }

    /// A terminal whose writes fail after `ok` flushes, like a hung-up tty.
    struct Dying {
        inner: TestBackend,
        ok: usize,
    }

    impl ratatui::backend::Backend for Dying {
        fn draw<'a, I: Iterator<Item = (u16, u16, &'a Cell)>>(&mut self, c: I) -> io::Result<()> {
            self.inner.draw(c)
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
            self.inner.set_cursor_position(p)
        }
        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }
        fn size(&self) -> io::Result<Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.ok == 0 {
                return Err(eio());
            }
            self.ok -= 1;
            Ok(())
        }
    }

    #[test]
    fn a_terminal_that_cannot_be_read_ends_the_loop_without_a_retry() {
        let mut calls = 0;
        let mut input = Script(0, |_| {
            calls += 1;
            Err(eio())
        });
        let end = run_loop(
            &mut term(),
            &mut input,
            &mut App::new(Env::fixture()),
            &AtomicI32::new(0),
        );
        assert!(matches!(end, End::Failed(_)), "{end:?}");
        assert_eq!(end.exit_code(), 1);
        assert_eq!(calls, 1, "an input error is never retried");
    }

    #[test]
    fn a_terminal_that_cannot_be_drawn_on_ends_the_loop_without_a_retry() {
        let mut calls = 0;
        let mut input = Script(0, |_| {
            calls += 1;
            Ok(None)
        });
        let backend = Dying {
            inner: TestBackend::new(100, 30),
            ok: 3,
        };
        let end = run_loop(
            &mut Terminal::new(backend).unwrap(),
            &mut input,
            &mut App::new(Env::fixture()),
            &AtomicI32::new(0),
        );
        assert!(matches!(end, End::Failed(_)), "{end:?}");
        assert_eq!(calls, 3, "the turn whose draw failed is the last one");
    }

    #[test]
    fn a_signal_ends_the_loop_and_stops_the_running_command() {
        let dir = std::env::temp_dir().join(format!("conduit-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let mut app = App::new(Env::fixture());
        app.dialog = Some(super::super::Dialog::Confirm {
            cmd: vec![
                "sh".into(),
                "-c".into(),
                format!("echo $$ > {}; exec sleep 30", pidfile.display()),
            ],
            needs_sudo: false,
        });
        let signal = AtomicI32::new(0);
        let started = Instant::now();
        let mut input = Script(0, |n| {
            if n == 0 {
                return Ok(Some(enter()));
            }
            // Once the command runs, the terminal is closed (SIGHUP).
            if std::fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n')) {
                signal.store(libc::SIGHUP, Ordering::SeqCst);
            }
            assert!(n < 400, "the loop went on after the signal");
            std::thread::sleep(Duration::from_millis(5));
            Ok(None)
        });
        let end = run_loop(&mut term(), &mut input, &mut app, &signal);
        assert!(matches!(end, End::Signal(libc::SIGHUP)), "{end:?}");
        assert_eq!(end.exit_code(), 129);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Stopped and reaped: the pid is gone, not a zombie.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the command (pid {pid}) is still there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_signal_wins_over_the_terminal_error_it_caused() {
        let signal = AtomicI32::new(0);
        let mut input = Script(0, |_| {
            signal.store(libc::SIGTERM, Ordering::SeqCst);
            Err(eio())
        });
        let end = run_loop(
            &mut term(),
            &mut input,
            &mut App::new(Env::fixture()),
            &signal,
        );
        assert!(matches!(end, End::Signal(libc::SIGTERM)), "{end:?}");
        assert_eq!(end.exit_code(), 143);
    }
}
