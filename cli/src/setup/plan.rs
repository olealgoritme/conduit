//! The whole setup as plain text: what `conduit setup --plan` prints, and
//! what a non-interactive `conduit setup` prints instead of a screen.

use super::data::{LOG_LINES, STAGES, WATCH_KERNEL_LOG};
use super::{hostfix, recipes, Env, Fix, Step};
use crate::doctor::{render_check, Level};

pub const WELCOME: &str = "Conduit shares your NVIDIA graphics card with a virtual machine, so the VM's desktop, Vulkan and CUDA programs run on the real GPU. \
This guide checks your computer, helps you fix what is missing and makes a first VM, showing every command before it runs. \
A Linux guest is the recommended first one: Ubuntu is fully automatic.";

/// The text of a fix: the command, or the words.
pub fn fix_text(f: &Fix) -> String {
    match f {
        Fix::Run { .. } => format!("$ {}", f.command_line().unwrap_or_default()),
        Fix::Guide { text } => text.clone(),
        Fix::Open { url } => format!("open {url}"),
        Fix::Ask { field } => format!("you type {}", field.label()),
    }
}

fn indent(text: &str, by: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                "\n".to_string()
            } else {
                format!("{by}{l}\n")
            }
        })
        .collect()
}

fn step_lines(steps: &[Step], env: &Env, out: &mut String) {
    for (i, s) in steps.iter().enumerate() {
        let st = (s.check)(env);
        out.push_str(&format!(
            "  {}. {} [{}]\n",
            i + 1,
            s.title,
            if st.is_done() { "done" } else { "to do" }
        ));
        if !s.explanation.is_empty() {
            out.push_str(&indent(&s.explanation, "       "));
        }
        if let Some(f) = s.fix.as_ref().filter(|_| !st.is_done()) {
            out.push_str(&indent(&fix_text(f), "       "));
        }
    }
}

pub fn plan_text(env: &Env) -> String {
    let mut o = String::new();
    o.push_str("Conduit setup\n=============\n\n");
    o.push_str(WELCOME);
    o.push_str("\n\n1. Check this computer\n\n");
    for c in &env.checks {
        o.push_str(&indent(render_check(c).trim_end(), "  "));
        if c.level != Level::Ok {
            // Words that only repeat the hint printed above add nothing.
            if let Some(f) = c
                .remedy
                .as_ref()
                .filter(|r| r.says_more_than_hint())
                .map(|r| hostfix::fix_of(r, env))
            {
                o.push_str(&indent("what to do:", "       "));
                o.push_str(&indent(&fix_text(&f), "         "));
            }
        }
    }
    let extras: Vec<Step> = hostfix::host_steps(env)
        .into_iter()
        .filter(|s| env.check(&s.id).is_none())
        .collect();
    if !extras.is_empty() {
        o.push_str("\n  Building from a source checkout:\n");
        step_lines(&extras, env, &mut o);
    }
    o.push_str("\n2. Choose a guest\n\n");
    for r in recipes::all() {
        o.push_str(&format!(
            "  [{}] {}{}\n{}",
            r.id,
            r.name,
            if r.recommended { "  (recommended)" } else { "" },
            indent(r.description, "     ")
        ));
    }
    for r in recipes::all()
        .into_iter()
        .filter(|r| r.experimental_doc.is_none())
    {
        let mut e = env.clone();
        e.vm_name = if r.default_name.is_empty() {
            "NAME"
        } else {
            r.default_name
        }
        .to_string();
        if e.iso.is_none() {
            e.iso = Some("/path/to/the.iso".into());
        }
        o.push_str(&format!("\n3. Steps for {}\n\n", r.name));
        step_lines(&(r.steps)(&e), &e, &mut o);
    }
    o.push_str("\n4. First run, safely\n\n");
    o.push_str(&indent(&first_run_text(env, "NAME"), "  "));
    o.push_str("\n5. Staged checks\n\n");
    o.push_str(&indent(&staged_text(), "  "));
    o
}

/// The first-run safety page: safe mode state, what to open, the launch command.
pub fn first_run_text(env: &Env, vm: &str) -> String {
    use crate::protect::Protection::*;
    let safe = env
        .check("safe-mode")
        .map(|c| c.detail.as_str())
        .unwrap_or("unknown: no NVIDIA driver is loaded");
    format!(
        "Safe mode: {safe}\n\
         Driver status: {}\n\n\
         Before the first start:\n\
         - Open an ssh session to this computer from another device, or a text console (Ctrl+Alt+F3), so you can stop the VM if the screen freezes.\n\
         - In a terminal watch the kernel log:\n    {WATCH_KERNEL_LOG}\n\
         - For a first run keep safe mode on: `conduit config set gpu.safe_mode true`.\n\n\
         Start it from a terminal inside your Wayland desktop:\n    conduit view {vm}\n\n\
         If it misbehaves: `conduit poweroff {vm}` from the ssh session.\n",
        match env.protection {
            Proven => "the open kernel modules, release 580 or newer: what Conduit is tested on",
            Untested => "untested with Conduit (closed modules, an older branch, or no driver): safe mode defaults to on",
        }
    )
}

pub fn staged_text() -> String {
    let mut o = String::new();
    for s in STAGES {
        o.push_str(&format!(
            "{}\n    run:    {}\n    expect: {}\n",
            s.title, s.command, s.expect
        ));
    }
    o.push_str("\nWhat log lines mean:\n");
    for l in LOG_LINES {
        o.push_str(&format!("  {}: {}\n", l.needle, l.meaning));
    }
    o
}
