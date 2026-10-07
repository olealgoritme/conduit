//! Drawing only: `draw` turns an [`App`] into widgets. No decisions here.

use super::plan::{first_run_text, fix_text, staged_text, WELCOME};
use super::{command_line, App, CheckResult, Dialog, Fix, Row, Screen};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const SCREENS: [(Screen, &str); 6] = [
    (Screen::Welcome, "Welcome"),
    (Screen::HostCheck, "This computer"),
    (Screen::ChooseGuest, "Choose a guest"),
    (Screen::Recipe, "Steps"),
    (Screen::FirstRun, "First run"),
    (Screen::Done, "Done"),
];

pub fn status_tag(s: &CheckResult) -> (&'static str, Color) {
    match s {
        CheckResult::Done(_) => ("  ok  ", Color::Green),
        CheckResult::Warn(_) => (" warn ", Color::Yellow),
        CheckResult::Todo(_) => (" TODO ", Color::Red),
    }
}

fn keys(app: &App) -> &'static str {
    if app.running.is_some() {
        return "Esc: stop the command (while it runs)   any key: close the output (when it ended)";
    }
    match (&app.dialog, app.screen) {
        (Some(Dialog::Confirm { .. }), _) => "Enter: run it   Esc: cancel",
        (Some(Dialog::Guide { .. }), _) => "Enter: done / close   Esc: close",
        (Some(Dialog::Input { .. }), _) => "type, then Enter   Esc: cancel",
        (_, Screen::Welcome) => "Enter: start   q: quit",
        (_, Screen::HostCheck) => "Up/Down: choose   Enter: fix   r: check again   n: next   c: next anyway   Esc: back   q: quit",
        (_, Screen::ChooseGuest) => "Up/Down: choose   Enter: select   Esc: back   q: quit",
        (_, Screen::Recipe) => "Up/Down: choose   Enter: do this step   r: check again   n: next   c: next anyway   Esc: back",
        (_, Screen::FirstRun) => "Enter: next   Esc: back   q: quit",
        (_, Screen::Done) => "Enter or q: quit",
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(f.area());
    f.render_widget(Paragraph::new(header(app)), head);
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(app.message.clone(), Style::new().fg(Color::Yellow)),
            Line::styled(keys(app), Style::new().add_modifier(Modifier::DIM)),
        ]),
        foot,
    );
    let body = if app.running.is_some() {
        let [top, pane] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(12)]).areas(body);
        draw_output(f, app, pane);
        top
    } else {
        body
    };
    match app.screen {
        Screen::Welcome => para(f, body, "Welcome to Conduit", WELCOME.to_string()),
        Screen::HostCheck => draw_rows(f, app, body, "This computer", &app.host),
        Screen::ChooseGuest => draw_guests(f, app, body),
        Screen::Recipe => draw_rows(f, app, body, app.recipe().name, &app.steps),
        Screen::FirstRun => para(
            f,
            body,
            "Before the first start",
            first_run_text(&app.env, &app.env.vm_name),
        ),
        Screen::Done => para(f, body, "Done. Now test in stages", staged_text()),
    }
    if let Some(d) = &app.dialog {
        draw_dialog(f, d);
    }
}

fn header(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " conduit setup ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )];
    for (s, name) in SCREENS {
        let st = if s == app.screen {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!(" {name} "), st));
    }
    Line::from(spans)
}

fn para(f: &mut Frame, area: Rect, title: &str, text: String) {
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).title(title.to_string())),
        area,
    );
}

fn draw_rows(f: &mut Frame, app: &App, area: Rect, title: &str, rows: &[Row]) {
    let [list, detail] = Layout::vertical([Constraint::Min(4), Constraint::Length(9)]).areas(area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let (tag, color) = status_tag(&r.status);
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{tag}] "), Style::new().fg(color)),
                Span::raw(format!("{}: {}", r.step.title, r.status.detail())),
            ]))
        })
        .collect();
    let mut st = ListState::default().with_selected(Some(app.cursor));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::new().borders(Borders::ALL).title(title.to_string()))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list,
        &mut st,
    );
    let text = rows
        .get(app.cursor)
        .map(|r| {
            let mut t = r.step.explanation.clone();
            if let Some(fix) = &r.step.fix {
                if !r.status.is_done() {
                    t.push_str(&format!("\n\nEnter: {}", fix_summary(fix)));
                }
            }
            t
        })
        .unwrap_or_default();
    para(f, detail, "About this step", text);
}

fn fix_summary(f: &Fix) -> String {
    match f {
        Fix::Run { .. } => format!("run  {}", fix_text(f).trim_start_matches("$ ")),
        Fix::Guide { .. } => "read what to do".into(),
        Fix::Open { url } => format!("open {url}"),
        Fix::Ask { field } => format!("type {}", field.label()),
    }
}

fn draw_guests(f: &mut Frame, app: &App, area: Rect) {
    let [list, detail] = Layout::vertical([Constraint::Min(4), Constraint::Length(7)]).areas(area);
    let items: Vec<ListItem> = app
        .recipes
        .iter()
        .map(|r| {
            ListItem::new(format!(
                "{}{}",
                r.name,
                if r.recommended {
                    "   (recommended)"
                } else {
                    ""
                }
            ))
        })
        .collect();
    let mut st = ListState::default().with_selected(Some(app.cursor));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::new().borders(Borders::ALL).title("Which guest?"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list,
        &mut st,
    );
    para(
        f,
        detail,
        "About this guest",
        app.recipes[app.cursor].description.to_string(),
    );
}

fn draw_output(f: &mut Frame, app: &App, area: Rect) {
    let Some(r) = &app.running else { return };
    let title = match r.exit {
        None => format!("running: {}", r.command),
        Some(0) => format!("finished, exit code 0: {}", r.command),
        Some(c) => format!("FAILED, exit code {c}: {}", r.command),
    };
    let h = area.height.saturating_sub(2) as usize;
    let skip = app.output.len().saturating_sub(h);
    let lines: Vec<Line> = app
        .output
        .iter()
        .skip(skip)
        .map(|l| Line::raw(l.clone()))
        .collect();
    let color = match r.exit {
        Some(0) => Color::Green,
        Some(_) => Color::Red,
        None => Color::Cyan,
    };
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(color))
                .title(title),
        ),
        area,
    );
}

fn draw_dialog(f: &mut Frame, d: &Dialog) {
    let (title, text) = match d {
        Dialog::Confirm { cmd, needs_sudo } => (
            "Run this command?".to_string(),
            format!(
                "{}\n\n{}",
                command_line(cmd, *needs_sudo),
                if *needs_sudo {
                    "It needs administrator rights: sudo asks for your password first."
                } else {
                    ""
                }
            ),
        ),
        Dialog::Guide { title, text } => (title.clone(), text.clone()),
        Dialog::Input { field, buf } => (field.label().to_string(), format!("{buf}_")),
    };
    let a = f.area();
    let w = a.width.saturating_sub(8).min(90);
    let h = (text.lines().count() as u16 + 4).clamp(6, a.height.saturating_sub(2));
    let r = Rect::new(
        a.x + (a.width - w) / 2,
        a.y + (a.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(title),
        ),
        r,
    );
}
