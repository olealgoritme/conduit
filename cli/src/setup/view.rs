//! Drawing only: `draw` turns an [`App`] into widgets. No decisions here.
//!
//! One layout rule for every screen: the frame is a centred column at most
//! [`super::layout::MAX_W`] wide with the progress header on top and the message line and
//! key hints on the last two rows; screens draw inside what is left, sized to
//! their content. Below [`MIN_W`] x [`MIN_H`] only "terminal too small" shows.

use super::layout::{centered, column, fits, truncate, width, wrap, MIN_H, MIN_W};
use super::plan::{first_run_text, fix_text, staged_text, WELCOME};
use super::theme::Theme;
use super::{command_line, App, CheckResult, Dialog, Fix, Row, Screen};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Padding,
    Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

const SCREENS: [(Screen, &str); 6] = [
    (Screen::Welcome, "Welcome"),
    (Screen::HostCheck, "This computer"),
    (Screen::ChooseGuest, "Choose a guest"),
    (Screen::Recipe, "Steps"),
    (Screen::FirstRun, "First run"),
    (Screen::Done, "Done"),
];

/// The three-row wordmark on the welcome page.
const WORDMARK: [&str; 3] = [
    "┏━╸┏━┓┏┓╻╺┳┓╻ ╻╻╺┳╸",
    "┃  ┃ ┃┃┗┫ ┃┃┃ ┃┃ ┃ ",
    "┗━╸┗━┛╹ ╹╺┻┛┗━┛╹ ╹ ",
];

/// What happens after Welcome, in the order the screens come.
const NEXT: [(&str, &str); 3] = [
    (
        "Check this computer",
        "driver, KVM and tools; fix what is missing",
    ),
    (
        "Choose a guest",
        "then follow its steps; Ubuntu is automatic",
    ),
    ("First run, safely", "safe mode on, then test in stages"),
];

/// The badge text of a status, always six columns.
pub fn badge(s: &CheckResult) -> &'static str {
    match s {
        CheckResult::Done(_) => "[ ok ]",
        CheckResult::Warn(_) => "[warn]",
        CheckResult::Todo(_) => "[TODO]",
    }
}

fn badge_style(s: &CheckResult, t: &Theme) -> Style {
    match s {
        CheckResult::Done(_) => t.ok,
        CheckResult::Warn(_) => t.warn,
        CheckResult::Todo(_) => t.fail,
    }
}

/// One `key action` hint. `keep` hints stay when the line is too narrow.
struct Hint {
    key: &'static str,
    action: &'static str,
    keep: bool,
}

const fn h(key: &'static str, action: &'static str) -> Hint {
    Hint {
        key,
        action,
        keep: true,
    }
}

const fn opt(key: &'static str, action: &'static str) -> Hint {
    Hint {
        key,
        action,
        keep: false,
    }
}

fn hints(app: &App) -> Vec<Hint> {
    if let Some(r) = &app.running {
        return match r.exit {
            None => vec![h("Esc", "stop the command")],
            Some(_) => vec![h("any key", "close the output")],
        };
    }
    match (&app.dialog, app.screen) {
        (Some(Dialog::Confirm { .. }), _) => vec![h("Enter", "run it"), h("Esc", "cancel")],
        (Some(Dialog::Guide { .. }), _) => vec![h("Enter", "done / close"), h("Esc", "close")],
        (Some(Dialog::Input { .. }), _) => vec![h("type", "then Enter"), h("Esc", "cancel")],
        (_, Screen::Welcome) => vec![h("Enter", "begin"), h("q", "quit")],
        (_, Screen::HostCheck) => vec![
            h("↑↓", "choose"),
            h("Enter", "fix"),
            h("r", "recheck"),
            h("n", "next"),
            h("c", "next anyway"),
            h("Esc", "back"),
            opt("q", "quit"),
        ],
        (_, Screen::ChooseGuest) => vec![
            h("↑↓", "choose"),
            h("Enter", "select"),
            h("Esc", "back"),
            opt("q", "quit"),
        ],
        (_, Screen::Recipe) => vec![
            h("↑↓", "choose"),
            h("Enter", "do this step"),
            h("r", "recheck"),
            h("n", "next"),
            h("c", "next anyway"),
            h("Esc", "back"),
            opt("q", "quit"),
        ],
        (_, Screen::FirstRun) => vec![h("Enter", "next"), h("Esc", "back"), h("q", "quit")],
        (_, Screen::Done) => vec![h("Enter/q", "quit")],
    }
}

const GAP: &str = "   ";

/// The hints as one line of at most `w` columns: optional ones go first.
fn hint_line(mut hs: Vec<Hint>, w: usize, t: &Theme) -> Line<'static> {
    let len = |hs: &[Hint]| -> usize {
        hs.iter()
            .map(|h| width(h.key) + 1 + width(h.action))
            .sum::<usize>()
            + GAP.len() * hs.len().saturating_sub(1)
    };
    while len(&hs) > w {
        match hs.iter().rposition(|h| !h.keep) {
            Some(i) => {
                hs.remove(i);
            }
            None => break,
        }
    }
    let mut spans = Vec::new();
    for (i, x) in hs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(GAP));
        }
        spans.push(Span::styled(x.key, t.key));
        spans.push(Span::raw(format!(" {}", x.action)));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, app: &App) {
    draw_with(f, app, Theme::detect());
}

pub fn draw_with(f: &mut Frame, app: &App, t: &Theme) {
    let area = f.area();
    if !fits(area.as_size()) {
        too_small(f, area, t);
        return;
    }
    let col = column(area);
    let [head, rule, body, msg, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(col);
    f.render_widget(Paragraph::new(header(app, head.width as usize, t)), head);
    f.render_widget(
        Paragraph::new(Line::styled("─".repeat(rule.width as usize), t.border)),
        rule,
    );
    f.render_widget(
        Paragraph::new(Line::styled(
            format!(" {}", truncate(&app.message, msg.width as usize - 1)),
            t.warn,
        )),
        msg,
    );
    f.render_widget(
        Paragraph::new(hint_line(hints(app), keys.width as usize - 1, t))
            .block(Block::new().padding(Padding::left(1))),
        keys,
    );
    let body = if app.running.is_some() {
        let pane_h = (body.height / 2).clamp(8, 16);
        let [top, pane] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(pane_h)]).areas(body);
        draw_output(f, app, pane, t);
        top
    } else {
        body
    };
    match app.screen {
        Screen::Welcome => draw_welcome(f, body, t),
        Screen::HostCheck => draw_rows(f, app, body, "This computer", &app.host, t),
        Screen::ChooseGuest => draw_guests(f, app, body, t),
        Screen::Recipe => draw_rows(f, app, body, app.recipe().name, &app.steps, t),
        Screen::FirstRun => page(
            f,
            body,
            "Before the first start",
            &first_run_text(&app.env, &app.env.vm_name),
            t,
        ),
        Screen::Done => page(f, body, "Done. Now test in stages", &staged_text(), t),
    }
    if let Some(d) = &app.dialog {
        draw_dialog(f, d, col, t);
    }
}

fn too_small(f: &mut Frame, area: Rect, t: &Theme) {
    let w = area.width.saturating_sub(2).max(1) as usize;
    let mut lines = vec![Line::styled("Terminal too small", t.title), Line::raw("")];
    let text = format!(
        "This window is {}x{}; conduit setup needs at least {MIN_W}x{MIN_H}. Make the window larger, or press Ctrl-C to quit.",
        area.width, area.height
    );
    lines.extend(wrap(&text, w).into_iter().map(Line::raw));
    let r = centered(area, (w as u16).min(60) + 2, lines.len() as u16 + 2);
    let lines: Vec<Line> = lines.into_iter().map(|l| l.centered()).collect();
    f.render_widget(Paragraph::new(lines), r.inner(Margin::new(1, 1)));
}

/// `● Welcome ─ ● This computer ─ ○ Choose a guest ─ …`; the brand in front
/// when it fits, the names of the other steps dropped when they do not.
fn header(app: &App, w: usize, t: &Theme) -> Line<'static> {
    let cur = SCREENS
        .iter()
        .position(|(s, _)| *s == app.screen)
        .unwrap_or(0);
    let build = |names: bool| -> Vec<Span<'static>> {
        let mut v = Vec::new();
        for (i, (_, name)) in SCREENS.iter().enumerate() {
            if i > 0 {
                v.push(Span::styled(" ─ ", t.dim));
            }
            let (mark, st) = match i.cmp(&cur) {
                std::cmp::Ordering::Less => ("●", Style::new()),
                std::cmp::Ordering::Equal => ("●", t.title),
                std::cmp::Ordering::Greater => ("○", t.dim),
            };
            if names || i == cur {
                v.push(Span::styled(format!("{mark} {name}"), st));
            } else {
                v.push(Span::styled(mark, st));
            }
        }
        v
    };
    let len = |v: &[Span]| v.iter().map(|s| s.width()).sum::<usize>();
    let mut steps = build(true);
    if len(&steps) + 1 > w {
        steps = build(false);
    }
    let brand = "conduit setup";
    let mut spans = vec![Span::raw(" ")];
    let used = 1 + len(&steps);
    if used + width(brand) + 4 <= w {
        spans.push(Span::styled(brand, t.title));
        spans.push(Span::raw(" ".repeat(w - used - width(brand))));
    }
    spans.extend(steps);
    Line::from(spans)
}

fn panel<'a>(title: impl Into<String>, t: &Theme) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.border)
        .title(Span::styled(format!(" {} ", title.into()), t.title))
        .padding(Padding::horizontal(1))
}

fn draw_welcome(f: &mut Frame, area: Rect, t: &Theme) {
    let w = area.width.saturating_sub(4).min(74) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let tall = area.height >= 18;
    if tall {
        lines.extend(WORDMARK.iter().map(|l| Line::styled(*l, t.title)));
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(vec![
        Span::styled("Welcome to Conduit", t.strong),
        Span::styled("  ·  guided first run", t.dim),
    ]));
    lines.push(Line::raw(""));
    lines.extend(wrap(WELCOME, w).into_iter().map(Line::raw));
    lines.push(Line::raw(""));
    lines.push(Line::styled("What happens next", t.title));
    let nw = NEXT.iter().map(|(n, _)| width(n)).max().unwrap_or(0);
    for (i, (name, what)) in NEXT.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}  ", i + 1), t.key),
            Span::styled(format!("{name:<nw$}"), t.strong),
            Span::styled(format!("   {what}"), t.dim),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" ▶ Press Enter to begin ", t.cta),
        Span::styled("   q to quit", t.dim),
    ]));
    let r = centered(area, w as u16, lines.len() as u16);
    f.render_widget(Paragraph::new(lines), r);
}

/// Where the list starts so that row `sel` of `n` shows, near the middle.
fn list_offset(n: usize, sel: usize, visible: usize) -> usize {
    if n <= visible {
        return 0;
    }
    sel.saturating_sub(visible / 2).min(n - visible)
}

/// A list box sized to its rows and a detail box sized to its text below it,
/// both bounded by `area`; the list scrolls when it has more rows than room.
struct ListView<'a> {
    title: String,
    summary: String,
    items: Vec<ListItem<'a>>,
    cursor: usize,
    detail_title: &'static str,
    detail: Vec<Line<'a>>,
}

fn draw_list(f: &mut Frame, area: Rect, v: ListView, t: &Theme) {
    let n = v.items.len();
    let want_list = n as u16 + 2;
    let want_detail = v.detail.len() as u16 + 2;
    let detail_h = if want_list + 1 + want_detail <= area.height {
        want_detail
    } else {
        want_detail.min((area.height * 2 / 5).max(6))
    };
    let list_h = want_list
        .min(area.height.saturating_sub(detail_h + 1))
        .max(3);
    let [list, _, detail, _] = Layout::vertical([
        Constraint::Length(list_h),
        Constraint::Length(1),
        Constraint::Length(detail_h),
        Constraint::Min(0),
    ])
    .areas(area);
    let visible = list_h.saturating_sub(2) as usize;
    let mut st = ListState::default()
        .with_offset(list_offset(n, v.cursor, visible))
        .with_selected(Some(v.cursor));
    let block =
        panel(v.title, t).title(Line::styled(format!(" {} ", v.summary), t.dim).right_aligned());
    f.render_stateful_widget(
        List::new(v.items)
            .block(block)
            .highlight_style(t.selected)
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always),
        list,
        &mut st,
    );
    if n > visible {
        let mut ss = ScrollbarState::new(n.saturating_sub(visible))
            .position(st.offset())
            .viewport_content_length(visible);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .symbols(ratatui::symbols::scrollbar::VERTICAL)
                .begin_symbol(None)
                .end_symbol(None)
                .track_style(t.border)
                .thumb_style(t.title),
            list.inner(Margin::new(0, 1)),
            &mut ss,
        );
    }
    let inner = detail.height.saturating_sub(2) as usize;
    let mut lines = v.detail;
    if lines.len() > inner && inner > 0 {
        lines.truncate(inner - 1);
        lines.push(Line::styled("… (make the window taller to read on)", t.dim));
    }
    f.render_widget(
        Paragraph::new(lines).block(panel(v.detail_title, t)),
        detail,
    );
}

fn draw_rows(f: &mut Frame, app: &App, area: Rect, title: &str, rows: &[Row], t: &Theme) {
    // Inside the box: border + padding on each side, then the "▶ " column.
    let inner = area.width.saturating_sub(4) as usize;
    let tw = rows
        .iter()
        .map(|r| width(&r.step.title))
        .max()
        .unwrap_or(0)
        .min(24);
    let room = inner.saturating_sub(2 + 6 + 1 + tw + 2);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            ListItem::new(Line::from(vec![
                Span::styled(badge(&r.status), badge_style(&r.status, t)),
                Span::raw(format!(" {:<tw$}  ", truncate(&r.step.title, tw))),
                Span::raw(truncate(r.status.detail(), room)),
            ]))
        })
        .collect();
    let todo = App::failing(rows);
    let warn = rows
        .iter()
        .filter(|r| matches!(r.status, CheckResult::Warn(_)))
        .count();
    let summary = format!(
        "{} done · {warn} warn · {todo} to do",
        rows.len() - todo - warn
    );
    let dw = inner;
    let detail = rows
        .get(app.cursor)
        .map(|r| {
            let mut v: Vec<Line> = Vec::new();
            let first = wrap(r.status.detail(), dw.saturating_sub(7));
            for (i, l) in first.into_iter().enumerate() {
                let lead = if i == 0 {
                    Span::styled(badge(&r.status), badge_style(&r.status, t))
                } else {
                    Span::raw("      ")
                };
                v.push(Line::from(vec![
                    lead,
                    Span::raw(" "),
                    Span::styled(l, t.strong),
                ]));
            }
            if !r.step.explanation.is_empty() {
                v.push(Line::raw(""));
                v.extend(wrap(&r.step.explanation, dw).into_iter().map(Line::raw));
            }
            if let Some(fix) = r.step.fix.as_ref().filter(|_| !r.status.is_done()) {
                v.push(Line::raw(""));
                v.extend(fix_lines(fix, dw, t));
            }
            v
        })
        .unwrap_or_default();
    draw_list(
        f,
        area,
        ListView {
            title: title.to_string(),
            summary,
            items,
            cursor: app.cursor,
            detail_title: "About this step",
            detail,
        },
        t,
    );
}

/// What Enter does on a row: the key, then the command or the words.
fn fix_lines(fix: &Fix, w: usize, t: &Theme) -> Vec<Line<'static>> {
    let (what, rest) = match fix {
        Fix::Run { .. } => ("run this command:", Some(fix_text(fix))),
        Fix::Guide { .. } => ("read what to do", None),
        Fix::Open { url } => return vec![enter_line(&format!("open {url}"), t)],
        Fix::Ask { field } => return vec![enter_line(&format!("type {}", field.label()), t)],
    };
    let mut v = vec![enter_line(what, t)];
    if let Some(cmd) = rest {
        v.extend(
            wrap(&cmd, w)
                .into_iter()
                .map(|l| Line::styled(l, t.command)),
        );
    }
    v
}

fn enter_line(what: &str, t: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("Enter", t.key),
        Span::raw(format!(" {what}")),
    ])
}

fn draw_guests(f: &mut Frame, app: &App, area: Rect, t: &Theme) {
    let items: Vec<ListItem> = app
        .recipes
        .iter()
        .map(|r| {
            let mut s = vec![Span::raw(r.name.to_string())];
            if r.recommended {
                s.push(Span::styled("   (recommended)", t.title));
            }
            ListItem::new(Line::from(s))
        })
        .collect();
    let dw = area.width.saturating_sub(4) as usize;
    let detail = wrap(app.recipes[app.cursor].description, dw)
        .into_iter()
        .map(Line::raw)
        .collect();
    draw_list(
        f,
        area,
        ListView {
            title: "Which guest?".into(),
            summary: format!("{} guests", app.recipes.len()),
            items,
            cursor: app.cursor,
            detail_title: "About this guest",
            detail,
        },
        t,
    );
}

/// A box of text, as tall as its text (cut with a note when the window is not).
fn page(f: &mut Frame, area: Rect, title: &str, text: &str, t: &Theme) {
    let w = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = wrap(text.trim_end(), w)
        .into_iter()
        .map(Line::raw)
        .collect();
    let inner = area.height.saturating_sub(2) as usize;
    if lines.len() > inner && inner > 0 {
        lines.truncate(inner - 1);
        lines.push(Line::styled(
            "… more below: make the window taller, or read `conduit setup --plan`",
            t.dim,
        ));
    }
    let r = Rect {
        height: (lines.len() as u16 + 2).min(area.height),
        ..area
    };
    f.render_widget(Paragraph::new(lines).block(panel(title, t)), r);
}

fn draw_output(f: &mut Frame, app: &App, area: Rect, t: &Theme) {
    let Some(r) = &app.running else { return };
    let (title, st) = match r.exit {
        None => (format!("running: {}", r.command), t.title),
        Some(0) => (format!("finished, exit code 0: {}", r.command), t.ok),
        Some(c) => (format!("FAILED, exit code {c}: {}", r.command), t.fail),
    };
    let h = area.height.saturating_sub(2) as usize;
    let skip = app.output.len().saturating_sub(h);
    let lines: Vec<Line> = app
        .output
        .iter()
        .skip(skip)
        .map(|l| Line::raw(l.clone()))
        .collect();
    let title = truncate(&title, area.width.saturating_sub(4) as usize);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(st)
                .title(Span::styled(format!(" {title} "), st))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

fn draw_dialog(f: &mut Frame, d: &Dialog, col: Rect, t: &Theme) {
    let w = col.width.saturating_sub(8).min(76);
    let tw = w.saturating_sub(4) as usize;
    let (title, mut lines, keys): (String, Vec<Line>, Vec<Hint>) = match d {
        Dialog::Confirm { cmd, needs_sudo } => {
            let mut v: Vec<Line> = wrap(&format!("$ {}", command_line(cmd, *needs_sudo)), tw)
                .into_iter()
                .map(|l| Line::styled(l, t.command))
                .collect();
            if *needs_sudo {
                v.push(Line::raw(""));
                v.extend(
                    wrap(
                        "It needs administrator rights: sudo asks for your password first.",
                        tw,
                    )
                    .into_iter()
                    .map(|l| Line::styled(l, t.warn)),
                );
            }
            (
                "Run this command?".into(),
                v,
                vec![h("Enter", "run it"), h("Esc", "cancel")],
            )
        }
        Dialog::Guide { title, text } => (
            title.clone(),
            wrap(text, tw).into_iter().map(Line::raw).collect(),
            vec![h("Enter", "done / close"), h("Esc", "close")],
        ),
        Dialog::Input { field, buf } => (
            field.label().to_string(),
            vec![Line::from(vec![
                Span::styled("> ", t.key),
                Span::styled(format!("{buf}_"), t.strong),
            ])],
            vec![h("Enter", "save"), h("Esc", "cancel")],
        ),
    };
    lines.push(Line::raw(""));
    lines.push(hint_line(keys, tw, t));
    let r = centered(col, w, lines.len() as u16 + 4);
    let inner = r.height.saturating_sub(4) as usize;
    if lines.len() > inner && inner > 2 {
        // Keep the key line; cut the text above it.
        let keyline = lines.pop().unwrap();
        lines.truncate(inner - 2);
        lines.push(Line::styled("…", t.dim));
        lines.push(keyline);
    }
    // Everything behind the dialog goes faint, so the dialog is what you read.
    f.buffer_mut().set_style(col, t.dim);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(t.dialog)
                .title(Span::styled(format!(" {title} "), t.title))
                .padding(Padding::uniform(1)),
        ),
        r,
    );
}
