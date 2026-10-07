use super::data::*;
use super::env::{not_installed, Distro, Env, Family};
use super::*;
use crate::doctor::{Check, Level, Remedy};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn chk(id: &str, level: Level, title: &str, detail: &str, hint: &str) -> Check {
    Check {
        id: id.into(),
        level,
        title: title.into(),
        detail: detail.into(),
        remedy: (!hint.is_empty()).then(|| Remedy::guide(hint)),
    }
}

/// A check with the remedy the doctor really builds for it.
fn chk_with(id: &str, level: Level, title: &str, detail: &str, remedy: Remedy) -> Check {
    Check {
        remedy: Some(remedy),
        ..chk(id, level, title, detail, "")
    }
}

fn tools_remedy() -> Remedy {
    Remedy::install(
        "Ubuntu: sudo apt install iproute2",
        crate::doctor::TOOL_PACKAGES_APT,
        &["iproute", "iptables"],
        &["iproute2", "iptables"],
    )
}

/// A host with one passing check, a failing Tools check and a failing driver.
fn sick_env() -> Env {
    let mut e = Env::fixture();
    e.checks = vec![
        chk("kvm", Level::Ok, "KVM", "available", ""),
        chk_with("tools", Level::Fail, "Tools", "missing: ip, curl", tools_remedy()),
        chk_with(
            "nvidia-driver",
            Level::Fail,
            "NVIDIA driver",
            "not loaded",
            Remedy::explain(
                "Install NVIDIA's driver with the OPEN kernel modules.",
                "No NVIDIA driver is loaded now.\n\nConduit never installs or changes a driver for you. OPEN modules, closed, restart. Releases Conduit knows: 580.95.05",
            ),
        ),
        chk(
            "safe-mode",
            Level::Ok,
            "Safe mode",
            "off, because 580.95.05 on the open kernel modules is what Conduit is tested on",
            "",
        ),
    ];
    e
}

fn app() -> App {
    App::new(sick_env())
}

fn press(a: &mut App, keys: &[Key]) -> Vec<Action> {
    keys.iter().map(|k| a.key(*k)).collect()
}

fn at_host(a: &mut App) {
    assert_eq!(a.key(Key::Enter), Action::None);
    assert_eq!(a.screen, Screen::HostCheck);
}

// ------------------------------------------------------------ distro and fixes

#[test]
fn os_release_decides_the_family() {
    let fam = |t: &str| Distro::parse(t).family();
    assert_eq!(fam("ID=ubuntu\nID_LIKE=debian\n"), Family::Debian);
    assert_eq!(
        fam("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
        Family::Debian
    );
    assert_eq!(fam("ID=debian\n"), Family::Debian);
    assert_eq!(fam("ID=fedora\n"), Family::Fedora);
    assert_eq!(
        fam("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
        Family::Fedora
    );
    assert_eq!(fam("ID=arch\n"), Family::Arch);
    assert_eq!(fam("ID=cachyos\nID_LIKE=arch\n"), Family::Arch);
    assert_eq!(fam("ID=nixos\n"), Family::Other);
    assert_eq!(fam(""), Family::Other);
}

fn script_packages(script: &str, arm: &str, installer: &str) -> Vec<String> {
    let mut lines = script.lines().skip_while(|l| l.trim() != arm);
    lines.next();
    let mut out = Vec::new();
    let mut on = false;
    for l in lines {
        if l.trim() == ";;" {
            break;
        }
        if l.contains(installer) {
            on = true;
        }
        if on {
            out.extend(
                l.split_whitespace()
                    .filter(|w| !w.starts_with('-') && *w != "\\")
                    .skip(if l.contains(installer) {
                        installer.split_whitespace().count()
                    } else {
                        0
                    })
                    .map(String::from),
            );
        }
    }
    out
}

#[test]
fn package_lists_equal_the_ones_build_sh_installs() {
    let sh = include_str!("../../../packaging/build.sh");
    let want = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        script_packages(sh, "deb)", "apt-get install"),
        want(BUILD_DEPS_APT)
    );
    assert_eq!(
        script_packages(sh, "rpm)", "dnf install"),
        want(BUILD_DEPS_DNF)
    );
    assert_eq!(
        script_packages(sh, "arch)", "pacman -Syu"),
        want(BUILD_DEPS_PACMAN)
    );
}

#[test]
fn dpkg_status_lines_say_what_is_missing() {
    let out = "git ii \ncurl rc \nmeson ii \n";
    assert_eq!(
        not_installed(&["git", "curl", "meson", "ninja-build"], out),
        ["curl", "ninja-build"]
    );
}

#[test]
fn debian_gets_commands_and_other_distros_get_the_same_steps_as_words() {
    let tools = chk_with("tools", Level::Fail, "Tools", "missing", tools_remedy());
    let deb = hostfix::fix_for(&tools, &Env::fixture()).unwrap();
    let Fix::Run { cmd, needs_sudo } = &deb else {
        panic!("{deb:?}")
    };
    assert!(needs_sudo);
    assert_eq!(&cmd[..3], ["apt-get", "install", "-y"]);
    assert!(cmd.contains(&"iproute2".to_string()));
    assert_eq!(deb.command_line().unwrap().split(' ').next(), Some("sudo"));

    let mut fed = Env::fixture();
    fed.distro = Distro::parse("ID=fedora\n");
    let Fix::Guide { text } = hostfix::fix_for(&tools, &fed).unwrap() else {
        panic!()
    };
    assert!(
        text.contains("sudo dnf install") && text.contains("iproute "),
        "{text}"
    );

    let mut arch = Env::fixture();
    arch.distro = Distro::parse("ID=arch\n");
    let Fix::Guide { text } = hostfix::fix_for(&tools, &arch).unwrap() else {
        panic!()
    };
    assert!(text.contains("sudo pacman -S --needed") && text.contains("iproute2"));

    let kvm = chk_with(
        "kvm-access",
        Level::Fail,
        "KVM",
        "present, but",
        Remedy::sudo("Add yourself:", &["usermod", "-aG", "kvm", "ole"]),
    );
    let Some(Fix::Run { cmd, .. }) = hostfix::fix_for(&kvm, &Env::fixture()) else {
        panic!("kvm on Debian is a command")
    };
    assert_eq!(cmd, ["usermod", "-aG", "kvm", "ole"]);
    assert!(matches!(
        hostfix::fix_for(&kvm, &arch),
        Some(Fix::Guide { .. })
    ));
}

#[test]
fn a_check_without_a_remedy_has_no_fix_and_a_plain_hint_is_its_own_guide() {
    let none = chk("x", Level::Warn, "X", "d", "");
    assert_eq!(hostfix::fix_for(&none, &Env::fixture()), None);
    let words = chk("y", Level::Warn, "Y", "d", "Do this.");
    assert_eq!(
        hostfix::fix_for(&words, &Env::fixture()),
        Some(Fix::guide("Do this."))
    );
    assert!(!words.remedy.as_ref().unwrap().says_more_than_hint());
}

#[test]
fn the_driver_is_never_changed_only_explained() {
    let loaded = crate::host::Driver {
        version: "570.1".into(),
        open: false,
    };
    let supported = vec!["580.95.05".to_string()];
    let checks = crate::doctor::driver_checks_for_test(Some(loaded), &supported);
    assert!(checks.iter().any(|c| c.level != Level::Ok));
    for c in checks.iter().filter(|c| c.level != Level::Ok) {
        let Some(Fix::Guide { text }) = hostfix::fix_for(c, &Env::fixture()) else {
            panic!("{}: not a guide", c.id)
        };
        assert!(
            text.contains("580.95.05"),
            "lists supported releases: {text}"
        );
        assert!(text.contains("OPEN") && text.contains("closed") && text.contains("restart"));
        assert!(text.contains("never installs or changes a driver"));
        assert!(text.contains("570.1"));
    }
}

#[test]
fn source_checkouts_get_build_dependency_and_nfpm_steps() {
    let mut e = Env::fixture();
    assert!(hostfix::host_steps(&e).iter().all(|s| s.id != "nfpm"));
    e.source_checkout = true;
    e.build_deps_missing = vec!["meson".into(), "flex".into()];
    let steps = hostfix::host_steps(&e);
    let deps = steps.iter().find(|s| s.id == "build-deps").unwrap();
    assert_eq!(
        deps.fix.as_ref().unwrap().command_line().unwrap(),
        "sudo apt-get install -y meson flex"
    );
    let nfpm = steps.iter().find(|s| s.id == "nfpm").unwrap();
    assert_eq!(
        nfpm.fix.as_ref().unwrap().command_line().unwrap(),
        "conduit setup fetch-nfpm"
    );
}

#[test]
fn conduit_is_never_run_as_root_but_others_are() {
    let c = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        effective_argv(&c(&["conduit", "create", "x"]), true),
        c(&["conduit", "create", "x"])
    );
    assert_eq!(
        effective_argv(&c(&["apt-get", "update"]), true),
        c(&["sudo", "apt-get", "update"])
    );
    assert_eq!(
        effective_argv(&c(&["virsh", "list"]), false),
        c(&["virsh", "list"])
    );
    assert_eq!(command_line(&c(&["echo", "a b"]), false), "echo 'a b'");
}

// ------------------------------------------------------------ the state machine

#[test]
fn welcome_leads_to_the_host_check_and_quits_on_q() {
    let mut a = app();
    assert_eq!(a.screen, Screen::Welcome);
    assert_eq!(a.key(Key::Char('q')), Action::Quit);
    at_host(&mut a);
    assert_eq!(a.host.len(), 4);
    assert_eq!(App::failing(&a.host), 2);
}

#[test]
fn failing_steps_block_next_until_continue_anyway() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Char('n'));
    assert_eq!(a.screen, Screen::HostCheck);
    assert!(a.message.contains("2 step(s) still to do"), "{}", a.message);
    a.key(Key::Char('c'));
    assert_eq!(a.screen, Screen::ChooseGuest);
    a.key(Key::Esc);
    assert_eq!(a.screen, Screen::HostCheck);

    let mut healthy = Env::fixture();
    healthy.checks = vec![chk("kvm", Level::Ok, "KVM", "available", "")];
    let mut b = App::new(healthy);
    at_host(&mut b);
    b.key(Key::Char('n'));
    assert_eq!(b.screen, Screen::ChooseGuest);
}

#[test]
fn a_command_runs_only_after_enter_in_the_confirm_dialog() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down); // Tools
    assert_eq!(a.key(Key::Enter), Action::None);
    let Some(Dialog::Confirm { cmd, needs_sudo }) = a.dialog.clone() else {
        panic!("{:?}", a.dialog)
    };
    assert!(needs_sudo && cmd[0] == "apt-get");
    assert!(a.running.is_none(), "nothing runs before Enter");
    // Esc cancels.
    assert_eq!(a.key(Key::Esc), Action::None);
    assert!(a.dialog.is_none() && a.running.is_none());
    // Enter, Enter: runs.
    a.key(Key::Enter);
    let act = a.key(Key::Enter);
    assert_eq!(
        act,
        Action::Spawn {
            argv: cmd,
            needs_sudo: true
        }
    );
    assert!(a
        .running
        .as_ref()
        .unwrap()
        .command
        .starts_with("sudo apt-get install -y"));
}

#[test]
fn while_a_command_runs_only_stop_is_accepted_and_the_exit_code_is_kept() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    a.key(Key::Enter);
    assert_eq!(
        press(&mut a, &[Key::Down, Key::Char('q'), Key::Enter]),
        [Action::None, Action::None, Action::None]
    );
    assert_eq!(a.key(Key::Esc), Action::Kill);
    a.push_line("E: Unable to locate package".into());
    assert_eq!(a.run_finished(100), Action::Refresh);
    assert_eq!(a.running.as_ref().unwrap().exit, Some(100));
    assert_eq!(a.output, ["E: Unable to locate package"]);
    // The next key only closes the output.
    assert_eq!(a.key(Key::Char('q')), Action::None);
    assert!(a.running.is_none());
    assert_eq!(a.key(Key::Char('q')), Action::Quit);
}

#[test]
fn guide_fixes_only_show_words() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Down); // NVIDIA driver
    assert_eq!(a.key(Key::Enter), Action::None);
    let Some(Dialog::Guide { text, .. }) = &a.dialog else {
        panic!()
    };
    assert!(text.contains("never installs or changes a driver"));
    a.key(Key::Enter);
    assert!(a.dialog.is_none() && a.running.is_none());
}

#[test]
fn choosing_a_guest_sets_up_its_steps_and_windows_is_only_explained() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Char('c'));
    assert_eq!(a.screen, Screen::ChooseGuest);
    // win11 is the fourth.
    press(&mut a, &[Key::Down, Key::Down, Key::Down]);
    a.key(Key::Enter);
    assert_eq!(a.screen, Screen::ChooseGuest);
    let Some(Dialog::Guide { text, .. }) = &a.dialog else {
        panic!()
    };
    assert!(text.contains("docs/WINDOWS.md") && text.contains("does not set this one up"));
    a.key(Key::Esc);
    press(&mut a, &[Key::Up, Key::Up, Key::Up]);
    assert_eq!(a.key(Key::Enter), Action::Refresh);
    assert_eq!(a.screen, Screen::Recipe);
    assert_eq!(a.env.vm_name, "ubuntu");
    let ids: Vec<_> = a.steps.iter().map(|r| r.step.id.as_str()).collect();
    assert_eq!(ids, ["vm-name", "create", "doctor-vm"]);
    assert_eq!(
        a.steps[1]
            .step
            .fix
            .as_ref()
            .unwrap()
            .command_line()
            .unwrap(),
        "conduit create ubuntu"
    );
}

#[test]
fn an_iso_path_is_typed_and_used_by_the_define_command() {
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    a.cursor = 1;
    a.key(Key::Enter); // omarchy
    assert_eq!(a.env.vm_name, "omarchy");
    let iso_row = a.steps.iter().position(|r| r.step.id == "iso").unwrap();
    assert!(a.steps[iso_row].status.is_blocking());
    let define_row = a.steps.iter().position(|r| r.step.id == "define").unwrap();
    assert!(
        a.steps[define_row].step.fix.is_none(),
        "no ISO, no define command"
    );
    a.cursor = iso_row;
    a.key(Key::Enter);
    for c in "/tmp/x.iso".chars() {
        a.key(Key::Char(c));
    }
    a.key(Key::Backspace);
    a.key(Key::Char('o'));
    a.key(Key::Enter);
    assert_eq!(a.env.iso.as_deref(), Some("/tmp/x.iso"));
    assert_eq!(
        a.steps[define_row]
            .step
            .fix
            .as_ref()
            .unwrap()
            .command_line()
            .unwrap(),
        "conduit setup define omarchy --iso /tmp/x.iso"
    );
}

#[test]
fn an_exit_code_zero_proves_an_exit0_step_and_a_guide_confirms_a_confirm_step() {
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    a.key(Key::Enter); // ubuntu
    a.cursor = 2; // doctor-vm
    assert!(!a.steps[2].status.is_done());
    a.key(Key::Enter);
    a.key(Key::Enter);
    a.run_finished(1);
    a.key(Key::Esc);
    assert!(!a.steps[2].status.is_done(), "a failing run proves nothing");
    a.key(Key::Enter);
    a.key(Key::Enter);
    a.run_finished(0);
    a.set_env(a.env.clone());
    assert!(a.steps[2].status.is_done());

    let mut b = app();
    b.screen = Screen::ChooseGuest;
    b.cursor = 1;
    b.key(Key::Enter);
    b.cursor = b
        .steps
        .iter()
        .position(|r| r.step.id == "install-os")
        .unwrap();
    b.key(Key::Enter);
    assert!(matches!(b.dialog, Some(Dialog::Guide { .. })));
    b.key(Key::Enter);
    assert!(b.steps[b.cursor].status.is_done());
}

#[test]
fn recipe_steps_lead_to_first_run_then_done() {
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    a.key(Key::Enter);
    a.key(Key::Char('c'));
    assert_eq!(a.screen, Screen::FirstRun);
    a.key(Key::Enter);
    assert_eq!(a.screen, Screen::Done);
    assert_eq!(a.key(Key::Enter), Action::Quit);
    assert_eq!(a.key(Key::CtrlC), Action::Quit);
}

// ------------------------------------------------------------ plain text

#[test]
fn first_run_page_states_safe_mode_the_ssh_advice_and_the_launch_command() {
    let mut e = sick_env();
    e.protection = crate::protect::Protection::Untested;
    let t = plan::first_run_text(&e, "omarchy");
    assert!(t.contains("Safe mode: off, because 580.95.05"));
    assert!(t.contains("untested with Conduit"));
    assert!(t.contains("ssh session"));
    assert!(t.contains("journalctl -k -f | grep -E 'NVRM|Xid'"));
    assert!(t.contains("conduit view omarchy"));
    e.protection = crate::protect::Protection::Proven;
    assert!(plan::first_run_text(&e, "x").contains("what Conduit is tested on"));
}

#[test]
fn the_plan_has_every_section_and_commands_only_on_debian() {
    let deb = plan::plan_text(&sick_env());
    for want in [
        "1. Check this computer",
        "[ FAIL ] Tools: missing: ip, curl",
        "$ sudo apt-get install -y iproute2",
        "2. Choose a guest",
        "[ubuntu]",
        "3. Steps for Omarchy (guided install)",
        "$ conduit create ubuntu",
        "$ conduit attach omarchy",
        "4. First run, safely",
        "5. Staged checks",
        "vkcube",
        "page kind 6/2",
        "https://omarchy.org",
    ] {
        assert!(deb.contains(want), "missing {want:?}\n{deb}");
    }
    assert!(!deb.contains("Windows 11 (experimental) steps"));
    // The hint is printed once, not again as "what to do".
    assert_eq!(
        deb.matches("Install NVIDIA's driver with the OPEN kernel modules.")
            .count(),
        1
    );
    assert!(!deb.lines().any(|l| l.ends_with(' ')), "no trailing spaces");

    let mut e = sick_env();
    e.distro = Distro::parse("ID=fedora\n");
    let fed = plan::plan_text(&e);
    assert!(fed.contains("sudo dnf install"));
    assert!(!fed.contains("$ sudo apt-get"));
}

// ------------------------------------------------------------ rendering

use super::layout::{MAX_W, MIN_H, MIN_W};
use super::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

fn buffer(app: &App, w: u16, h: u16, t: &Theme) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| view::draw_with(f, app, t)).unwrap();
    term.backend().buffer().clone()
}

fn text(buf: &Buffer) -> String {
    let a = buf.area;
    (0..a.height)
        .map(|y| {
            (0..a.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn dump(app: &App, w: u16, h: u16) -> String {
    text(&buffer(app, w, h, &Theme::color()))
}

fn last_row(s: &str) -> &str {
    s.lines().last().unwrap_or("")
}

/// A host with `n` extra passing checks, for scrolling.
fn long_host(n: usize) -> App {
    let mut a = app();
    at_host(&mut a);
    for i in 0..n {
        a.host.push(Row {
            step: Step::new(
                &format!("extra-{i}"),
                &format!("Extra check {i}"),
                "",
                |_| CheckResult::Done("fine".into()),
                None,
                Verify::Recheck,
            ),
            status: CheckResult::Done(format!("fine number {i}")),
        });
    }
    a
}

/// Every screen and overlay, with the name of the step the header marks and
/// the first key the footer must show.
fn every_screen() -> Vec<(&'static str, App, &'static str, &'static str)> {
    let mut v = Vec::new();
    v.push(("welcome", app(), "Welcome", "Enter begin"));
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    v.push(("this-computer", a, "This computer", "↑↓ choose"));
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    v.push(("confirm", a, "This computer", "Enter run it"));
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    a.key(Key::Enter);
    for i in 0..40 {
        a.push_line(format!("Reading package lists... {i}"));
    }
    a.run_finished(100);
    v.push(("output-failed", a, "This computer", "any key close"));
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Down);
    a.key(Key::Enter);
    v.push(("guide", a, "This computer", "Enter done / close"));
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    v.push(("choose-guest", a, "Choose a guest", "↑↓ choose"));
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    a.key(Key::Enter);
    v.push(("steps", a, "Steps", "↑↓ choose"));
    let mut a = app();
    a.screen = Screen::FirstRun;
    v.push(("first-run", a, "First run", "Enter next"));
    let mut a = app();
    a.screen = Screen::Done;
    v.push(("done", a, "Done", "Enter/q quit"));
    v.push((
        "this-computer-long",
        long_host(32),
        "This computer",
        "↑↓ choose",
    ));
    v
}

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 75)];

#[test]
fn every_screen_has_the_progress_header_the_footer_and_a_bounded_column() {
    for (name, a, step, key) in every_screen() {
        for (w, h) in SIZES {
            let s = dump(&a, w, h);
            let rows: Vec<&str> = s.lines().collect();
            assert_eq!(rows.len(), h as usize, "{name} {w}x{h}");
            assert!(
                rows[0].contains(&format!("● {step}")) && rows[0].contains(" ─ "),
                "{name} {w}x{h}: header\n{s}"
            );
            assert!(last_row(&s).contains(key), "{name} {w}x{h}: footer\n{s}");
            let x0 = (w - w.min(MAX_W)) / 2;
            for (y, r) in rows.iter().enumerate() {
                let cols: Vec<usize> = r
                    .chars()
                    .enumerate()
                    .filter(|(_, c)| *c != ' ')
                    .map(|(i, _)| i)
                    .collect();
                if let (Some(&first), Some(&last)) = (cols.first(), cols.last()) {
                    assert!(
                        first >= x0 as usize && last < (x0 + MAX_W) as usize,
                        "{name} {w}x{h}: row {y} outside the column: {r}"
                    );
                }
            }
            if matches!(
                name,
                "this-computer" | "choose-guest" | "steps" | "this-computer-long"
            ) {
                assert!(s.contains("▶ "), "{name} {w}x{h}: selection marker\n{s}");
            }
        }
    }
}

/// The owner's 200x75 screenshot showed no key hints: they were there, but
/// faint (SGR 2) and nothing else. The keys are now the accent colour and
/// bold, never faint, on the last row.
#[test]
fn footer_keys_are_on_the_last_row_in_the_key_style_not_faint() {
    for (w, h) in SIZES {
        let buf = buffer(&app(), w, h, &Theme::color());
        let s = text(&buf);
        let y = h - 1;
        let row = s.lines().nth(y as usize).unwrap();
        let x = row.find("Enter").expect("hint on the last row") as u16;
        let cell = &buf[(x, y)];
        assert_eq!(cell.fg, Color::Cyan, "{w}x{h}");
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(!cell.modifier.contains(Modifier::DIM));
        let action = &buf[(x + 6, y)];
        assert!(
            !action.modifier.contains(Modifier::DIM),
            "{w}x{h}: action faint"
        );
    }
}

#[test]
fn a_resize_lays_the_frame_out_again() {
    let a = app();
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| view::draw_with(f, &a, &Theme::color()))
        .unwrap();
    assert!(last_row(&text(term.backend().buffer())).contains("Enter begin"));
    term.backend_mut().resize(200, 75);
    term.draw(|f| view::draw_with(f, &a, &Theme::color()))
        .unwrap();
    let s = text(term.backend().buffer());
    assert_eq!(s.lines().count(), 75);
    assert!(last_row(&s).contains("Enter begin"), "{s}");
    assert!(!s.lines().nth(23).unwrap().contains("Enter begin"));
    term.backend_mut().resize(60, 15);
    term.draw(|f| view::draw_with(f, &a, &Theme::color()))
        .unwrap();
    assert!(text(term.backend().buffer()).contains("Terminal too small"));
}

#[test]
fn render_welcome() {
    let s = dump(&app(), 80, 24);
    println!("{s}");
    assert!(s.contains("conduit setup") || s.contains("● Welcome"));
    assert!(s.contains("Welcome to Conduit"));
    assert!(s.contains("┏━╸┏━┓┏┓╻"));
    assert!(s.contains("Conduit shares your NVIDIA graphics"));
    assert!(s.contains("Ubuntu is fully automatic"));
    assert!(s.contains("What happens next") && s.contains("3  First run, safely"));
    assert!(s.contains("Press Enter to begin"));
    assert!(last_row(&s).contains("Enter begin   q quit"));
}

#[test]
fn welcome_is_centred_on_a_large_terminal() {
    let s = dump(&app(), 200, 75);
    println!("{s}");
    let rows: Vec<&str> = s.lines().collect();
    let y = rows
        .iter()
        .position(|r| r.contains("Welcome to Conduit"))
        .unwrap();
    assert!((25..45).contains(&y), "row {y}");
    let x = rows[y].find("Welcome to Conduit").unwrap();
    assert!((55..80).contains(&x), "col {x}");
    assert!(rows[0].contains("conduit setup"));
}

#[test]
fn render_step_list_with_a_failing_step() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    let s = dump(&a, 80, 24);
    println!("{s}");
    assert!(s.contains("[ ok ] KVM"));
    assert!(s.contains("▶ [TODO] Tools") && s.contains("missing: ip, curl"));
    assert!(s.contains("[TODO] NVIDIA driver") && s.contains("not loaded"));
    assert!(s.contains("Enter run this command:"));
    assert!(s.contains("$ sudo apt-get install -y"));
    assert!(s.contains("2 done · 0 warn · 2 to do"));
}

#[test]
fn long_lists_scroll_and_keep_the_selection_visible() {
    let mut a = long_host(32);
    let n = a.host.len();
    for (target, up) in [
        (0, false),
        (20, false),
        (n - 1, false),
        (10, true),
        (0, true),
    ] {
        while a.cursor != target {
            a.key(if up { Key::Up } else { Key::Down });
        }
        let s = dump(&a, 80, 24);
        let title = &a.host[target].step.title;
        assert!(
            s.lines()
                .any(|l| l.contains("▶ ") && l.contains(title.as_str())),
            "{target}: {title}\n{s}"
        );
    }
}

#[test]
fn render_confirm_dialog_shows_the_exact_command() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    let s = dump(&a, 100, 24);
    println!("{s}");
    assert!(s.contains("Run this command?"));
    assert!(s.contains("$ sudo apt-get install -y iproute2 iptables openssh-client curl"));
    assert!(s.contains("sudo asks for your password"));
    assert!(s.contains("Enter run it   Esc cancel"));
    assert!(last_row(&s).contains("Enter run it   Esc cancel"));
}

#[test]
fn render_running_output_and_exit_code() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    a.key(Key::Enter);
    a.push_line("Reading package lists...".into());
    let s = dump(&a, 100, 30);
    assert!(s.contains("running: sudo apt-get install"));
    assert!(last_row(&s).contains("Esc stop the command"));
    a.run_finished(100);
    for (w, h) in SIZES {
        let s = dump(&a, w, h);
        println!("{s}");
        assert!(s.contains("FAILED, exit code 100") && s.contains("Reading package lists..."));
    }
}

#[test]
fn render_guest_choice_and_done() {
    let mut a = app();
    a.screen = Screen::ChooseGuest;
    let s = dump(&a, 100, 24);
    println!("{s}");
    assert!(s.contains("Ubuntu 24.04 (automatic)   (recommended)"));
    assert!(s.contains("Windows 11 (experimental)"));
    a.screen = Screen::Done;
    let s = dump(&a, 100, 40);
    println!("{s}");
    assert!(s.contains("1. Module load only") && s.contains("3. vkcube"));
}

#[test]
fn small_terminals_say_so_and_do_not_panic() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    a.key(Key::Enter);
    for (w, h) in [
        (10, 3),
        (40, 8),
        (1, 1),
        (0, 0),
        (79, 24),
        (80, 23),
        (200, 60),
    ] {
        dump(&a, w, h);
    }
    let s = dump(&a, 60, 15);
    println!("{s}");
    assert!(s.contains("Terminal too small"));
    assert!(s.contains(&format!("{MIN_W}x{MIN_H}")) && s.contains("Ctrl-C"));
    assert!(dump(&a, 80, 24).contains("Run this command?"));
}

#[test]
fn no_color_keeps_the_selection_but_drops_every_colour() {
    let mut a = app();
    at_host(&mut a);
    a.key(Key::Down);
    let buf = buffer(&a, 120, 40, &Theme::plain());
    assert!(buf
        .content()
        .iter()
        .all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
    let s = text(&buf);
    let (y, row) = s
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("▶ [TODO] Tools"))
        .unwrap();
    let x = row.find("[TODO]").unwrap();
    let x = row[..x].chars().count() as u16;
    assert!(buf[(x, y as u16)].modifier.contains(Modifier::REVERSED));
}

/// Not a check: writes every screen at every size as text, for review.
/// `CONDUIT_DUMP_DIR=dir cargo test -p conduit write_render_dumps -- --ignored`
#[test]
#[ignore]
fn write_render_dumps() {
    let dir = std::path::PathBuf::from(std::env::var("CONDUIT_DUMP_DIR").unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    for (name, a, _, _) in every_screen() {
        for (w, h) in SIZES.iter().copied().chain([(60, 15)]) {
            std::fs::write(
                dir.join(format!("{name}-{w}x{h}.txt")),
                dump(&a, w, h) + "\n",
            )
            .unwrap();
        }
    }
}

#[test]
fn links_are_one_table_and_nothing_else_spells_a_url() {
    for f in [
        "mod.rs",
        "recipes.rs",
        "hostfix.rs",
        "plan.rs",
        "view.rs",
        "layout.rs",
        "theme.rs",
        "term.rs",
        "env.rs",
        "domain.rs",
    ] {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/setup")
                .join(f),
        )
        .unwrap();
        let code = src.split("#[cfg(test)]").next().unwrap();
        for l in code.lines().filter(|l| !l.trim_start().starts_with("//")) {
            assert!(
                !l.contains("https://") && !l.contains("http://"),
                "{f}: {l}"
            );
        }
    }
}
