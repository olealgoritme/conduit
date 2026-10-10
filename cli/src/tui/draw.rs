//! Drawing the dashboard. Truecolor throughout; gradients are per cell.

use super::{data, Act, App, Level, Modal, Tab, HIST, LOG_SOURCES};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row,
    Sparkline, Table, TableState, Wrap,
};
use ratatui::Frame;

const BG: Color = Color::Rgb(13, 15, 20);
const PANEL: Color = Color::Rgb(20, 23, 31);
const EDGE: Color = Color::Rgb(52, 58, 76);
const DIM: Color = Color::Rgb(110, 118, 140);
const FG: Color = Color::Rgb(220, 225, 235);
const GREEN: Color = Color::Rgb(118, 185, 0);
const CYAN: Color = Color::Rgb(0, 200, 220);
const BLUE: Color = Color::Rgb(80, 150, 255);
const VIOLET: Color = Color::Rgb(170, 110, 255);
const AMBER: Color = Color::Rgb(255, 190, 40);
const RED: Color = Color::Rgb(255, 80, 90);
const PINK: Color = Color::Rgb(255, 100, 200);

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn rgb(c: Color) -> (f32, f32, f32) {
    match c {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (255.0, 255.0, 255.0),
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    Color::Rgb(
        (ar + (br - ar) * t) as u8,
        (ag + (bg - ag) * t) as u8,
        (ab + (bb - ab) * t) as u8,
    )
}

/// A colour along a stops list, t in 0..=1.
fn ramp(stops: &[Color], t: f32) -> Color {
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (t.floor() as usize).min(stops.len() - 2);
    mix(stops[i], stops[i + 1], t - i as f32)
}

const HEAT: [Color; 4] = [GREEN, AMBER, Color::Rgb(255, 120, 40), RED];
const COOL: [Color; 3] = [GREEN, CYAN, BLUE];
const NEON: [Color; 3] = [CYAN, VIOLET, PINK];

fn panel<'a>(title: &'a str, accent: Color) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(EDGE))
        .style(Style::new().bg(PANEL).fg(FG))
        .title(Line::from(vec![
            Span::styled(" ◆ ", Style::new().fg(accent)),
            Span::styled(title, Style::new().fg(FG).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
}

/// A smooth bar: eighth blocks, coloured cell by cell along `stops`.
fn bar(width: u16, frac: f64, stops: &[Color]) -> Vec<Span<'static>> {
    const PART: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let w = width as usize;
    let f = frac.clamp(0.0, 1.0) * w as f64;
    let full = f.floor() as usize;
    let part = ((f - full as f64) * 8.0).round() as usize;
    let mut v = Vec::with_capacity(w);
    for i in 0..w {
        let c = ramp(stops, i as f32 / w.max(2) as f32);
        if i < full {
            v.push(Span::styled("█", Style::new().fg(c)));
        } else if i == full && part > 0 && part < 8 {
            v.push(Span::styled(PART[part], Style::new().fg(c)));
        } else {
            v.push(Span::styled("━", Style::new().fg(Color::Rgb(38, 42, 55))));
        }
    }
    v
}

fn human(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 10.0 {
        format!("{g:.0} GiB")
    } else if g >= 1.0 {
        format!("{g:.1} GiB")
    } else {
        format!("{} MiB", b >> 20)
    }
}

fn dur(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86400, (s % 86400) / 3600),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::new().bg(BG)), area);
    if app.intro > 0 {
        splash(f, app, area);
        return;
    }
    app.rows.clear();
    app.buttons.clear();
    app.tabs.clear();
    let [head, tabs, body, foot] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .areas(area);
    header(f, app, head);
    tab_bar(f, app, tabs);
    match app.tab {
        Tab::Dash => dashboard(f, app, body),
        Tab::Logs => logs(f, app, body),
        Tab::Doctor => doctor(f, app, body),
        Tab::Apps => apps_tab(f, app, body),
    }
    footer(f, app, foot);
    if let Some(m) = &app.modal {
        modal(f, app, m, area);
    }
}

const LOGO: [&str; 3] = [
    "▄▀▀▀ ▄▀▀▄ █▄  █ █▀▀▄ █  █ █ ▀█▀",
    "█    █  █ █ ▀▄█ █  █ █  █ █  █ ",
    "▀▄▄▄ ▀▄▄▀ ▀   ▀ ▀▄▄▀ ▀▄▄▀ ▀  ▀ ",
];

fn header(f: &mut Frame, app: &App, area: Rect) {
    let [logo, info] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);
    let shift = (app.tick as f32 * 0.03).sin() * 0.5 + 0.5;
    let mut lines = vec![Line::raw("")];
    for row in LOGO {
        let n = row.chars().count().max(1) as f32;
        let spans: Vec<Span> = row
            .chars()
            .enumerate()
            .map(|(i, c)| {
                let t = (i as f32 / n + shift * 0.6) % 1.0;
                Span::styled(
                    c.to_string(),
                    Style::new()
                        .fg(ramp(&[GREEN, CYAN, VIOLET, GREEN], t))
                        .bold(),
                )
            })
            .collect();
        let mut l = vec![Span::raw("  ")];
        l.extend(spans);
        lines.push(Line::from(l));
    }
    f.render_widget(Paragraph::new(lines), logo);

    let s = &app.snap;
    let h = &s.host;
    let gpu = s
        .gpu
        .as_ref()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "GPU: NVML not available".into());
    let running = s.vms.iter().filter(|v| v.up()).count();
    let mem_frac = if h.mem_total > 0 {
        h.mem_used as f64 / h.mem_total as f64
    } else {
        0.0
    };
    let bw = info.width.saturating_sub(48).clamp(8, 30);
    let mut cpu_line = vec![Span::styled(" cpu  ", Style::new().fg(DIM))];
    cpu_line.extend(bar(bw, h.cpu_pct / 100.0, &COOL));
    cpu_line.push(Span::styled(
        format!(
            " {:>3.0}%  load {:.2}  {} threads",
            h.cpu_pct, h.load1, h.cores
        ),
        Style::new().fg(FG),
    ));
    let mut mem_line = vec![Span::styled(" ram  ", Style::new().fg(DIM))];
    mem_line.extend(bar(bw, mem_frac, &COOL));
    mem_line.push(Span::styled(
        format!(
            " {:>3.0}%  {} / {}",
            mem_frac * 100.0,
            human(h.mem_used),
            human(h.mem_total)
        ),
        Style::new().fg(FG),
    ));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", h.hostname),
                Style::new().fg(BG).bg(GREEN).bold(),
            ),
            Span::styled(format!(" linux {} ", h.kernel), Style::new().fg(DIM)),
            Span::styled("│", Style::new().fg(EDGE)),
            Span::styled(format!(" {gpu} "), Style::new().fg(FG).bold()),
            Span::styled(
                if s.driver.is_empty() {
                    String::new()
                } else {
                    format!("driver {} ", s.driver)
                },
                Style::new().fg(DIM),
            ),
            Span::styled("│", Style::new().fg(EDGE)),
            Span::styled(
                format!(" {running}/{} VMs up ", s.vms.len()),
                Style::new().fg(if running > 0 { GREEN } else { DIM }),
            ),
            Span::styled("│ ", Style::new().fg(EDGE)),
            Span::styled(super::clock(), Style::new().fg(CYAN).bold()),
        ]),
        Line::from(cpu_line),
        Line::from(mem_line),
    ];
    let inner = Rect {
        y: info.y + 1,
        height: info.height.saturating_sub(1),
        ..info
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn tab_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let mut x = area.x + 2;
    let mut spans = vec![Span::raw("  ")];
    for (t, label, key) in [
        (Tab::Dash, "Dashboard", "1"),
        (Tab::Logs, "Logs", "2"),
        (Tab::Doctor, "Doctor", "3"),
        (Tab::Apps, "Apps", "4"),
    ] {
        let on = app.tab == t;
        let text = format!(" {key} {label} ");
        let w = text.chars().count() as u16;
        spans.push(Span::styled(
            text,
            if on {
                Style::new().fg(BG).bg(CYAN).bold()
            } else {
                Style::new().fg(DIM).bg(PANEL)
            },
        ));
        spans.push(Span::raw(" "));
        app.tabs.push((
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            t,
        ));
        x += w + 1;
    }
    if let Some(vm) = app.vm() {
        spans.push(Span::styled("  ", Style::new()));
        spans.push(Span::styled(
            format!(" {} ", vm.name),
            Style::new().fg(GREEN).bg(PANEL).bold(),
        ));
        spans.push(Span::raw(" "));
    }
    let used: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
    let rest = area.width.saturating_sub(used + 1) as usize;
    let phase = app.tick as f32 * 0.02;
    for i in 0..rest {
        let t = ((i as f32 / rest.max(1) as f32) - phase).rem_euclid(1.0);
        let c = ramp(&[EDGE, GREEN, CYAN, VIOLET, EDGE], t);
        spans.push(Span::styled("─", Style::new().fg(c)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn state_span(app: &App, v: &data::Vm) -> Span<'static> {
    if let Some(b) = app.busy(&v.name) {
        let s = SPIN[(app.tick as usize) % SPIN.len()];
        return Span::styled(format!("{s} {b}"), Style::new().fg(PINK).bold());
    }
    match v.state.as_str() {
        "running" => {
            let pulse = (app.tick as f32 * 0.15).sin() * 0.5 + 0.5;
            Span::styled(
                "● running".to_string(),
                Style::new()
                    .fg(mix(GREEN, Color::Rgb(200, 255, 120), pulse))
                    .bold(),
            )
        }
        "paused" => Span::styled("‖ paused".to_string(), Style::new().fg(AMBER).bold()),
        "stopped" => Span::styled("○ stopped".to_string(), Style::new().fg(DIM)),
        other => Span::styled(format!("◌ {other}"), Style::new().fg(VIOLET)),
    }
}

fn dashboard(f: &mut Frame, app: &mut App, area: Rect) {
    let vm_h = (app.snap.vms.len() as u16 + 4).clamp(8, 14);
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(vm_h),
        Constraint::Min(16),
        Constraint::Length(7),
    ])
    .areas(area);
    let [list, card] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(top);
    vm_table(f, app, list);
    vm_card(f, app, card);
    let [gauges, chart] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid);
    gpu_panel(f, app, gauges);
    gpu_chart(f, app, chart);
    activity(f, app, bottom);
}

fn vm_table(f: &mut Frame, app: &mut App, area: Rect) {
    let block = panel("Virtual machines", GREEN);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.snap.vms.is_empty() {
        let t = if app.snap.seq == 0 {
            "  looking for VMs…"
        } else {
            "  No VMs yet. Create one with `conduit create NAME` or `conduit setup`."
        };
        f.render_widget(Paragraph::new(t).fg(DIM), inner);
        return;
    }
    let header = Row::new([
        "", "NAME", "STATE", "OS", "DISPLAY", "CPU", "RAM", "VRAM", "UP",
    ])
    .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))
    .bottom_margin(0);
    let rows: Vec<Row> = app
        .snap
        .vms
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let sel = i == app.sel;
            let os = if v.windows {
                Span::styled("⊞ Windows", Style::new().fg(BLUE))
            } else if v.libvirt.is_some() || v.ram_mib.is_some() {
                Span::styled("◆ Linux", Style::new().fg(AMBER))
            } else {
                Span::raw("")
            };
            let mode = if v.up() {
                v.mode.clone().unwrap_or_else(|| "headless".into())
            } else {
                app.modes
                    .get(&v.name)
                    .map(|m| format!("→ {m}"))
                    .unwrap_or_default()
            };
            let cpu_t = (v.cpu_pct / (100.0 * v.cpus.unwrap_or(1).max(1) as f64)) as f32;
            Row::new(vec![
                Cell::from(Span::styled(
                    if sel { "▶" } else { " " },
                    Style::new().fg(GREEN).bold(),
                )),
                Cell::from(Span::styled(
                    v.name.clone(),
                    Style::new().fg(if sel { Color::White } else { FG }).bold(),
                )),
                Cell::from(state_span(app, v)),
                Cell::from(os),
                Cell::from(Span::styled(mode, Style::new().fg(CYAN))),
                Cell::from(if v.pid.is_some() {
                    let mut l = bar(6, cpu_t as f64, &COOL);
                    l.push(Span::styled(
                        format!("{:>5.0}%", v.cpu_pct),
                        Style::new().fg(ramp(&HEAT, cpu_t)),
                    ));
                    Line::from(l)
                } else {
                    Line::styled("      -", Style::new().fg(DIM))
                }),
                Cell::from(
                    v.ram_mib
                        .map(|m| human(m << 20))
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(Span::styled(
                    if v.vram > 0 {
                        human(v.vram)
                    } else {
                        "-".into()
                    },
                    Style::new().fg(VIOLET),
                )),
                Cell::from(Span::styled(
                    v.uptime.map(dur).unwrap_or_else(|| "-".into()),
                    Style::new().fg(DIM),
                )),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(18),
        Constraint::Length(9),
        Constraint::Length(15),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(7),
    ];
    for i in 0..app.snap.vms.len() {
        let y = inner.y + 1 + i as u16;
        if y < inner.y + inner.height {
            app.rows.push((
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                i,
            ));
        }
    }
    let mut st = TableState::default().with_selected(Some(app.sel));
    let t = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(Color::Rgb(32, 40, 30)));
    f.render_stateful_widget(t, inner, &mut st);
}

fn kv(k: &str, v: impl Into<String>, c: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {k:<9}"), Style::new().fg(DIM)),
        Span::styled(v.into(), Style::new().fg(c)),
    ])
}

fn vm_card(f: &mut Frame, app: &App, area: Rect) {
    let Some(v) = app.vm() else {
        f.render_widget(panel("Details", CYAN), area);
        return;
    };
    let title = v.name.clone();
    let block = panel(&title, CYAN);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [text, spark] = Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).areas(inner);
    let mut lines = vec![Line::from(vec![Span::raw(" "), state_span(app, v)])];
    lines.push(kv(
        "managed",
        match &v.libvirt {
            Some(d) => format!("libvirt domain {d}"),
            None => "conduit up/view".into(),
        },
        FG,
    ));
    if let (Some(c), Some(m)) = (v.cpus, v.ram_mib) {
        lines.push(kv("size", format!("{c} vCPUs · {}", human(m << 20)), FG));
    }
    if v.up() {
        lines.push(kv(
            "display",
            format!(
                "{}{}",
                v.mode.clone().unwrap_or_else(|| "headless".into()),
                if v.viewer { " · window open" } else { "" }
            ),
            CYAN,
        ));
        if let Some(p) = v.pid {
            lines.push(kv(
                "process",
                format!("pid {p} · {} resident", human(v.rss)),
                FG,
            ));
        }
        let helpers: Vec<String> = v.helpers.iter().map(|(w, p)| format!("{w} {p}")).collect();
        if !helpers.is_empty() {
            lines.push(kv("helpers", helpers.join(" · "), DIM));
        }
    }
    if let Some(m) = app.modes.get(&v.name) {
        lines.push(kv("next", format!("{m} on view/start"), AMBER));
    }
    if v.windows {
        lines.push(kv("gpu path", "NVK on RM · Venus on", GREEN));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
    let data: Vec<u64> = app
        .vm_cpu
        .get(&v.name)
        .map(|q| q.iter().copied().collect())
        .unwrap_or_default();
    let cap = 100 * v.cpus.unwrap_or(1).max(1) as u64;
    let take = spark.width.saturating_sub(12) as usize;
    let tail: Vec<u64> = data.iter().rev().take(take).rev().copied().collect();
    let [lab, sp] = Layout::horizontal([Constraint::Length(11), Constraint::Min(4)]).areas(spark);
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(" cpu", Style::new().fg(DIM)),
            Line::styled(
                format!(" {:>4.0}%", v.cpu_pct),
                Style::new().fg(GREEN).bold(),
            ),
        ]),
        lab,
    );
    f.render_widget(
        Sparkline::default()
            .data(&tail)
            .max(cap)
            .style(Style::new().fg(GREEN).bg(PANEL)),
        sp,
    );
}

fn gauge_line(label: &str, frac: f64, text: String, stops: &[Color], w: u16) -> Line<'static> {
    let mut l = vec![Span::styled(format!(" {label:<6}"), Style::new().fg(DIM))];
    l.extend(bar(w, frac, stops));
    l.push(Span::styled(format!(" {text}"), Style::new().fg(FG).bold()));
    Line::from(l)
}

/// A 3-row block font for the big readouts.
fn glyph(c: char) -> [&'static str; 3] {
    match c {
        '0' => ["█▀█", "█ █", "█▄█"],
        '1' => ["▀█ ", " █ ", "▄█▄"],
        '2' => ["▀▀█", "█▀▀", "█▄▄"],
        '3' => ["▀▀█", " ▀█", "▄▄█"],
        '4' => ["█ █", "▀▀█", "  █"],
        '5' => ["█▀▀", "▀▀█", "▄▄█"],
        '6' => ["█▀▀", "█▀█", "█▄█"],
        '7' => ["▀▀█", "  █", "  █"],
        '8' => ["█▀█", "█▀█", "█▄█"],
        '9' => ["█▀█", "▀▀█", "▄▄█"],
        '%' => ["█ ▄", " ▄▀", "▀ █"],
        'W' => ["█   █", "█ █ █", "▀▄▀▄▀"],
        '°' => ["▄▀▄", " ▀ ", "   "],
        'C' => ["█▀▀", "█  ", "█▄▄"],
        'G' => ["█▀▀", "█ █", "█▄█"],
        'H' => ["█ █", "█▀█", "█ █"],
        'z' => ["   ", "▀▀█", "█▄▄"],
        '.' => [" ", " ", "▄"],
        _ => ["  ", "  ", "  "],
    }
}

/// `text` in the big font, each column coloured along `stops` from `t0`.
fn big(text: &str, stops: &[Color], t0: f32) -> Vec<Line<'static>> {
    let mut rows: [Vec<Span<'static>>; 3] = [vec![], vec![], vec![]];
    let total: usize = text.chars().map(|c| glyph(c)[0].chars().count() + 1).sum();
    let mut col = 0usize;
    for c in text.chars() {
        let g = glyph(c);
        let w = g[0].chars().count();
        for (r, row) in rows.iter_mut().enumerate() {
            for (i, ch) in g[r].chars().enumerate() {
                let t = t0 + (col + i) as f32 / total.max(1) as f32 * 0.35;
                row.push(Span::styled(
                    ch.to_string(),
                    Style::new().fg(ramp(stops, t)).bold(),
                ));
            }
            row.push(Span::raw(" "));
        }
        col += w + 1;
    }
    rows.into_iter().map(Line::from).collect()
}

/// One hero tile: a dim label over a big number.
fn hero(f: &mut Frame, area: Rect, label: &str, value: &str, frac: f64, stops: &[Color]) {
    let mut lines = vec![Line::styled(
        format!(" {label}"),
        Style::new().fg(DIM).bold(),
    )];
    for l in big(value, stops, (frac as f32 * 0.8).clamp(0.0, 0.65)) {
        let mut v = vec![Span::raw(" ")];
        v.extend(l.spans);
        lines.push(Line::from(v));
    }
    f.render_widget(Paragraph::new(lines), area);
}

const VM_COLOURS: [Color; 5] = [GREEN, CYAN, VIOLET, PINK, AMBER];

/// VRAM split by VM: one stacked bar plus its legend.
fn vram_split(app: &App, g: &super::nvml::Sample, w: u16) -> Vec<Line<'static>> {
    let total = g.vram_total.unwrap_or(0).max(1);
    let used = g.vram_used.unwrap_or(0);
    let mut segs: Vec<(String, u64, Color)> = app
        .snap
        .vms
        .iter()
        .filter(|v| v.vram > 0)
        .enumerate()
        .map(|(i, v)| (v.name.clone(), v.vram, VM_COLOURS[i % VM_COLOURS.len()]))
        .collect();
    let vm_sum: u64 = segs.iter().map(|s| s.1).sum();
    segs.push((
        "host".into(),
        used.saturating_sub(vm_sum),
        Color::Rgb(90, 100, 130),
    ));
    let mut bar_spans = vec![Span::styled(" split ", Style::new().fg(DIM))];
    let mut legend = vec![Span::raw("       ")];
    let mut cells = 0u16;
    for (name, bytes, c) in &segs {
        let n = ((*bytes as f64 / total as f64) * w as f64).round() as u16;
        let n = n.min(w - cells);
        if n > 0 {
            bar_spans.push(Span::styled("█".repeat(n as usize), Style::new().fg(*c)));
            cells += n;
        }
        legend.push(Span::styled("■ ", Style::new().fg(*c)));
        legend.push(Span::styled(
            format!("{name} {}  ", human(*bytes)),
            Style::new().fg(FG),
        ));
    }
    bar_spans.push(Span::styled(
        "━".repeat((w - cells) as usize),
        Style::new().fg(Color::Rgb(38, 42, 55)),
    ));
    legend.push(Span::styled(
        format!("free {}", human(total.saturating_sub(used))),
        Style::new().fg(DIM),
    ));
    vec![Line::from(bar_spans), Line::from(legend)]
}

fn gpu_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = panel("GPU", GREEN);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(g) = &app.snap.gpu else {
        let msg = if app.snap.seq == 0 {
            "  reading the GPU…"
        } else {
            "  NVML (libnvidia-ml.so.1) is not available: is the NVIDIA driver loaded?"
        };
        f.render_widget(Paragraph::new(msg).fg(DIM), inner);
        return;
    };
    let [heroes, rest] = Layout::vertical([Constraint::Length(5), Constraint::Min(1)]).areas(inner);
    let tiles = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(heroes);
    let util = g.util_gpu.unwrap_or(0);
    let watts = g.power_mw.unwrap_or(0) / 1000;
    let limit = g.power_limit_mw.unwrap_or(600_000) / 1000;
    let tc = g.temp_c.unwrap_or(0);
    hero(
        f,
        tiles[0],
        "GPU LOAD",
        &format!("{util}%"),
        util as f64 / 100.0,
        &NEON,
    );
    hero(
        f,
        tiles[1],
        "BOARD POWER",
        &format!("{watts}W"),
        watts as f64 / limit.max(1) as f64,
        &HEAT,
    );
    hero(
        f,
        tiles[2],
        "CORE TEMP",
        &format!("{tc}°C"),
        tc as f64 / 95.0,
        &HEAT,
    );
    let inner = rest;
    let w = inner.width.saturating_sub(26).max(6);
    let vram_frac = match (g.vram_used, g.vram_total) {
        (Some(u), Some(t)) if t > 0 => u as f64 / t as f64,
        _ => 0.0,
    };
    let pw = g.power_mw.unwrap_or(0) as f64 / 1000.0;
    let pl = g.power_limit_mw.unwrap_or(600_000) as f64 / 1000.0;
    let temp = g.temp_c.unwrap_or(0) as f64;
    let ratio = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(a), Some(b)) if b > 0 => a as f64 / b as f64,
        _ => 0.0,
    };
    let mut lines = vec![
        gauge_line(
            "vram",
            vram_frac,
            format!(
                "{} / {}",
                human(g.vram_used.unwrap_or(0)),
                human(g.vram_total.unwrap_or(0))
            ),
            &COOL,
            w,
        ),
        gauge_line(
            "core",
            ratio(g.clk_gfx, g.clk_gfx_max),
            format!("{} MHz", g.clk_gfx.unwrap_or(0)),
            &COOL,
            w,
        ),
        gauge_line(
            "mem",
            ratio(g.clk_mem, g.clk_mem_max),
            format!("{} MHz", g.clk_mem.unwrap_or(0)),
            &COOL,
            w,
        ),
        gauge_line("power", pw / pl, format!("{pw:.0} / {pl:.0} W"), &HEAT, w),
        gauge_line("temp", temp / 95.0, format!("{temp:.0} °C"), &HEAT, w),
    ];
    if let Some(fan) = g.fan_pct {
        lines.push(gauge_line(
            "fan",
            fan as f64 / 100.0,
            format!("{fan:>3}%"),
            &COOL,
            w,
        ));
    }
    let mut extra = vec![Span::styled(" ", Style::new())];
    if let Some(p) = g.pstate {
        extra.push(Span::styled(
            format!(" P{p} "),
            Style::new().fg(BG).bg(VIOLET).bold(),
        ));
        extra.push(Span::raw(" "));
    }
    if let (Some(tx), Some(rx)) = (g.pcie_tx_kbs, g.pcie_rx_kbs) {
        extra.push(Span::styled("pcie ", Style::new().fg(DIM)));
        extra.push(Span::styled(
            format!("↑{:.1} ↓{:.1} GB/s", tx as f64 / 1e6, rx as f64 / 1e6),
            Style::new().fg(CYAN),
        ));
        extra.push(Span::raw("  "));
    }
    if let Some(e) = g.enc_util {
        extra.push(Span::styled("nvenc ", Style::new().fg(DIM)));
        extra.push(Span::styled(format!("{e}%"), Style::new().fg(PINK)));
        extra.push(Span::raw("  "));
    }
    if let Some(u) = g.util_mem {
        extra.push(Span::styled("mem ctrl ", Style::new().fg(DIM)));
        extra.push(Span::styled(format!("{u}%"), Style::new().fg(AMBER)));
    }
    lines.push(Line::from(extra));
    lines.push(Line::raw(""));
    lines.extend(vram_split(app, g, inner.width.saturating_sub(9).max(4)));
    f.render_widget(Paragraph::new(lines), inner);
}

fn gpu_chart(f: &mut Frame, app: &App, area: Rect) {
    let block = panel("Last two minutes", VIOLET);
    let pts = |q: &std::collections::VecDeque<u64>| -> Vec<(f64, f64)> {
        let off = HIST.saturating_sub(q.len()) as f64;
        q.iter()
            .enumerate()
            .map(|(i, v)| (off + i as f64, *v as f64))
            .collect()
    };
    let util = pts(&app.gpu_util);
    let temp = pts(&app.gpu_temp);
    let vram = pts(&app.gpu_vram);
    let cpu = pts(&app.host_cpu);
    let ds = vec![
        Dataset::default()
            .name("gpu load %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(PINK))
            .data(&util),
        Dataset::default()
            .name("temp °C")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(AMBER))
            .data(&temp),
        Dataset::default()
            .name("vram %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(VIOLET))
            .data(&vram),
        Dataset::default()
            .name("host cpu %")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(CYAN))
            .data(&cpu),
    ];
    let chart = Chart::new(ds)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([0.0, HIST as f64])
                .style(Style::new().fg(EDGE)),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, 100.0])
                .labels(vec![
                    Span::styled("0", Style::new().fg(DIM)),
                    Span::styled("50", Style::new().fg(DIM)),
                    Span::styled("100", Style::new().fg(DIM)),
                ])
                .style(Style::new().fg(EDGE)),
        )
        .legend_position(Some(ratatui::widgets::LegendPosition::TopLeft))
        .hidden_legend_constraints((Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)));
    f.render_widget(chart, area);
}

fn level_style(l: Level) -> (Span<'static>, Color) {
    match l {
        Level::Info => (Span::styled("•", Style::new().fg(CYAN)), FG),
        Level::Ok => (Span::styled("✔", Style::new().fg(GREEN).bold()), GREEN),
        Level::Warn => (Span::styled("▲", Style::new().fg(AMBER).bold()), AMBER),
        Level::Err => (Span::styled("✖", Style::new().fg(RED).bold()), RED),
    }
}

fn activity(f: &mut Frame, app: &App, area: Rect) {
    let block = panel("Activity", PINK);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let n = inner.height as usize;
    let lines: Vec<Line> = app
        .notes
        .iter()
        .rev()
        .take(n)
        .rev()
        .map(|note| {
            let (icon, c) = level_style(note.level);
            Line::from(vec![
                Span::styled(format!(" {} ", note.at), Style::new().fg(DIM)),
                icon,
                Span::raw(" "),
                Span::styled(note.text.clone(), Style::new().fg(c)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn colour_log(l: &str) -> Line<'static> {
    let low = l.to_ascii_lowercase();
    let c = if low.contains("error") || low.contains("fail") || low.contains("panic") {
        RED
    } else if low.contains("warn") {
        AMBER
    } else if low.starts_with("==>") || low.starts_with("--") {
        CYAN
    } else {
        FG
    };
    Line::styled(l.to_string(), Style::new().fg(c))
}

fn logs(f: &mut Frame, app: &mut App, area: Rect) {
    let name = app.vm().map(|v| v.name.clone()).unwrap_or_default();
    let title = format!("Logs · {name}");
    let block = panel(&title, AMBER);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [srcs, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let mut s = vec![Span::styled(" source ", Style::new().fg(DIM))];
    for (i, src) in LOG_SOURCES.iter().enumerate() {
        s.push(Span::styled(
            format!(" {src} "),
            if i == app.logs_which {
                Style::new().fg(BG).bg(AMBER).bold()
            } else {
                Style::new().fg(DIM)
            },
        ));
    }
    s.push(Span::styled(
        "   ←/→ source · ↑/↓ PgUp/PgDn scroll · refreshes every 2 s",
        Style::new().fg(EDGE),
    ));
    if app.capturing == Some(Tab::Logs) {
        s.push(Span::styled(
            format!("  {}", SPIN[(app.tick as usize) % SPIN.len()]),
            Style::new().fg(PINK),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(s)), srcs);
    let h = body.height as usize;
    let total = app.logs.len();
    let max_scroll = total.saturating_sub(h) as u16;
    app.logs_scroll = app.logs_scroll.min(max_scroll);
    let end = total.saturating_sub(app.logs_scroll as usize);
    let start = end.saturating_sub(h);
    let lines: Vec<Line> = app.logs[start..end].iter().map(|l| colour_log(l)).collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn doctor(f: &mut Frame, app: &mut App, area: Rect) {
    let block = panel("Doctor · this computer", GREEN);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.doctor.is_empty() {
        let s = SPIN[(app.tick as usize) % SPIN.len()];
        f.render_widget(
            Paragraph::new(format!("  {s} checking this computer…")).fg(PINK),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = app
        .doctor
        .iter()
        .map(|l| {
            let t = l.trim_start();
            let c = if t.starts_with("✓") || t.starts_with("[ok]") || t.starts_with("ok") {
                GREEN
            } else if t.starts_with("✗") || t.starts_with("[fail]") || t.contains("FAIL") {
                RED
            } else if t.starts_with('!') || t.starts_with("[warn]") {
                AMBER
            } else {
                FG
            };
            Line::styled(l.clone(), Style::new().fg(c))
        })
        .collect();
    let max = (lines.len() as u16).saturating_sub(inner.height);
    app.doctor_scroll = app.doctor_scroll.min(max);
    f.render_widget(Paragraph::new(lines).scroll((app.doctor_scroll, 0)), inner);
}

/// The Apps tab: the selected VM's installed apps.
fn apps_tab(f: &mut Frame, app: &mut App, area: Rect) {
    use super::apps::{filtered, unavailable};
    let Some(vm) = app.snap.vms.get(app.sel).cloned() else {
        let block = panel("Apps", CYAN);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new("  No VM selected.").fg(DIM), inner);
        return;
    };
    let spin = SPIN[(app.tick as usize) % SPIN.len()];
    let title = format!("Apps · {}", vm.name);
    let block = panel(&title, CYAN);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let note = |f: &mut Frame, text: &str, color: Color| {
        let mut lines = Vec::new();
        for (i, l) in text.lines().enumerate() {
            lines.push(Line::styled(
                format!("  {l}"),
                if i == 0 {
                    Style::new().fg(color).bold()
                } else {
                    Style::new().fg(DIM)
                },
            ));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    };
    if let Some(why) = unavailable(&vm) {
        note(f, &why, AMBER);
        return;
    }
    let entry = app.apps.vms.get(&vm.name);
    let (loading, error, loaded, count) = entry.map_or((true, None, None, 0), |e| {
        (e.loading, e.error.clone(), e.loaded, e.list.len())
    });
    if count == 0 {
        match (&error, loading) {
            (Some(e), _) => {
                let retry = if loading {
                    format!("{spin} trying again…")
                } else {
                    "trying again every few seconds · r to retry now".to_string()
                };
                note(f, &format!("{e}\n{retry}"), RED);
            }
            (None, true) => note(
                f,
                &format!("{spin} loading the app list from {}…", vm.name),
                PINK,
            ),
            (None, false) if loaded.is_some() => {
                note(f, &format!("{} reports no apps", vm.name), AMBER)
            }
            _ => note(f, &format!("{spin} loading…"), PINK),
        }
        return;
    }
    let stale = loading || error.is_some();
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let list = &app.apps.vms[&vm.name].list;
    let shown = filtered(list, &app.apps.filter);
    let mut h = vec![Span::styled(
        format!(" {} of {} apps ", shown.len(), list.len()),
        Style::new().fg(DIM),
    )];
    if app.apps.typing || !app.apps.filter.is_empty() {
        h.push(Span::styled(
            format!(
                " /{}{} ",
                app.apps.filter,
                if app.apps.typing { "▏" } else { "" }
            ),
            Style::new().fg(BG).bg(AMBER).bold(),
        ));
    }
    if loading {
        h.push(Span::styled(
            format!("  {spin} refreshing"),
            Style::new().fg(PINK),
        ));
    } else if let Some(e) = &error {
        let first = e.lines().next().unwrap_or("");
        h.push(Span::styled(
            format!("  ! {first} (showing the last list)"),
            Style::new().fg(RED),
        ));
    } else if let Some(t) = loaded {
        h.push(Span::styled(
            format!("  updated {} ago", dur(t.elapsed())),
            Style::new().fg(EDGE),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(h)), head);
    let rows = body.height as usize;
    let sel = app.apps.sel.min(shown.len().saturating_sub(1));
    app.apps.sel = sel;
    let start = sel
        .saturating_sub(rows / 2)
        .min(shown.len().saturating_sub(rows));
    let w = body.width as usize;
    let src_w = 11usize;
    let name_w = w.saturating_sub(src_w + 4).max(8);
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(i, a)| {
            let name: String = a.name.chars().take(name_w).collect();
            let src = crate::ctl::source_label(&a.source);
            let (fg, src_fg) = if stale {
                (DIM, EDGE)
            } else {
                (FG, source_color(&a.source))
            };
            let mut st = Style::new().fg(fg);
            let mut sst = Style::new().fg(src_fg);
            if i == sel {
                st = st.bg(Color::Rgb(34, 44, 60)).bold();
                sst = sst.bg(Color::Rgb(34, 44, 60));
            }
            Line::from(vec![
                Span::styled(if i == sel { " ▸ " } else { "   " }, st.fg(GREEN)),
                Span::styled(format!("{name:<name_w$}"), st),
                Span::styled(format!(" {src:<src_w$}"), sst),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}

fn source_color(s: &str) -> Color {
    match s {
        "steam" => BLUE,
        "startmenu" => CYAN,
        "flatpak" => VIOLET,
        "snap" => PINK,
        _ => GREEN,
    }
}

fn footer(f: &mut Frame, app: &mut App, area: Rect) {
    if app.tab == Tab::Apps {
        let items = [
            ("⏎", "run"),
            ("a", "add launcher"),
            ("/", "filter"),
            ("r", "refresh"),
            ("←→", "VM"),
            ("Tab", "tabs"),
            ("?", "help"),
            ("q", "back"),
        ];
        let mut spans = Vec::new();
        let mut used = 0u16;
        for (k, label) in items {
            let (ks, ls) = (format!(" {k} "), format!(" {label} "));
            let w = (ks.chars().count() + ls.chars().count() + 1) as u16;
            if used + w > area.width {
                break;
            }
            used += w;
            spans.push(Span::styled(ks, Style::new().fg(BG).bg(CYAN).bold()));
            spans.push(Span::styled(
                ls,
                Style::new().fg(FG).bg(Color::Rgb(34, 38, 50)),
            ));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let items: &[(&str, &str, Act)] = &[
        ("⏎", "view", Act::View),
        ("s", "start", Act::Up),
        ("d", "shutdown", Act::Shutdown),
        ("R", "reboot", Act::Reboot),
        ("r", "reset", Act::Reset),
        ("f", "force off", Act::Poweroff),
        ("p", "pause", Act::Pause),
        ("m", "mode", Act::Mode),
        ("F", "shares", Act::Shares),
        ("l", "logs", Act::Logs),
        ("D", "doctor", Act::Doctor),
        ("?", "help", Act::Help),
        ("q", "quit", Act::Quit),
    ];
    let mut x = area.x;
    let mut spans = Vec::new();
    for (k, label, a) in items {
        let danger = matches!(a, Act::Reset | Act::Poweroff);
        let key_bg = if danger { RED } else { GREEN };
        let ks = format!(" {k} ");
        let ls = format!(" {label} ");
        let w = (ks.chars().count() + ls.chars().count()) as u16;
        if x + w > area.x + area.width {
            break;
        }
        spans.push(Span::styled(ks, Style::new().fg(BG).bg(key_bg).bold()));
        spans.push(Span::styled(
            ls,
            Style::new().fg(FG).bg(Color::Rgb(34, 38, 50)),
        ));
        spans.push(Span::raw(" "));
        app.buttons.push((
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            *a,
        ));
        x += w + 1;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn modal(f: &mut Frame, app: &App, m: &Modal, area: Rect) {
    match m {
        Modal::Confirm { act, vm } => {
            let r = centered(area, 56, 8);
            f.render_widget(Clear, r);
            let (what, warn) = match act {
                Act::Poweroff => (
                    "Force off",
                    "Like pulling the plug: unsaved work in the VM is lost.",
                ),
                Act::Reset => (
                    "Reset",
                    "Like the reset button: unsaved work in the VM is lost.",
                ),
                Act::Reboot => ("Reboot", "Asks the guest to restart."),
                _ => (
                    "Shut down",
                    "Presses the power button and waits for the guest.",
                ),
            };
            let danger = matches!(act, Act::Poweroff | Act::Reset);
            let accent = if danger { RED } else { AMBER };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(accent))
                .style(Style::new().bg(Color::Rgb(26, 20, 24)).fg(FG))
                .title(Line::styled(
                    format!(" {what} {vm}? "),
                    Style::new().fg(accent).bold(),
                ))
                .title_alignment(Alignment::Center);
            let t = Text::from(vec![
                Line::raw(""),
                Line::styled(warn, Style::new().fg(FG)).centered(),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" y ", Style::new().fg(BG).bg(accent).bold()),
                    Span::styled(format!(" {what} "), Style::new().fg(FG)),
                    Span::raw("     "),
                    Span::styled(" any other key ", Style::new().fg(BG).bg(DIM).bold()),
                    Span::styled(" cancel ", Style::new().fg(FG)),
                ])
                .centered(),
            ]);
            f.render_widget(Paragraph::new(t).block(block), r);
        }
        Modal::Mode { vm, items, idx } => {
            let r = centered(area, 50, items.len() as u16 + 5);
            f.render_widget(Clear, r);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(CYAN))
                .style(Style::new().bg(PANEL).fg(FG))
                .title(Line::styled(
                    format!(" Display mode for {vm} "),
                    Style::new().fg(CYAN).bold(),
                ));
            let mut lines = vec![Line::styled(
                " used the next time you view or start it",
                Style::new().fg(DIM),
            )];
            for (i, (m, note)) in items.iter().enumerate() {
                let on = i == *idx;
                lines.push(Line::from(vec![
                    Span::styled(
                        if on { " ▶ " } else { "   " },
                        Style::new().fg(GREEN).bold(),
                    ),
                    Span::styled(
                        format!("{m:<16}"),
                        if on {
                            Style::new().fg(BG).bg(CYAN).bold()
                        } else {
                            Style::new().fg(FG)
                        },
                    ),
                    Span::styled(format!(" {note}"), Style::new().fg(DIM)),
                ]));
            }
            lines.push(Line::styled(
                " ↑/↓ choose · ⏎ set · Esc cancel",
                Style::new().fg(EDGE),
            ));
            f.render_widget(Paragraph::new(lines).block(block), r);
        }
        Modal::Shares {
            vm,
            items,
            idx,
            input,
        } => {
            let r = centered(area, 78, items.len().max(1) as u16 + 7);
            f.render_widget(Clear, r);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(CYAN))
                .style(Style::new().bg(PANEL).fg(FG))
                .title(Line::styled(
                    format!(" Shared folders of {vm} "),
                    Style::new().fg(CYAN).bold(),
                ));
            let mut lines = vec![Line::styled(
                " drives in Windows, /mnt/conduit/NAME in Linux",
                Style::new().fg(DIM),
            )];
            if items.is_empty() {
                lines.push(Line::styled("   none yet: press a", Style::new().fg(DIM)));
            }
            let inner = r.width.saturating_sub(2) as usize;
            for (i, s) in items.iter().enumerate() {
                let on = i == *idx && input.is_none();
                let room = inner.saturating_sub(24);
                let path = if s.path.chars().count() > room {
                    let tail: String = s.path.chars().rev().take(room.saturating_sub(1)).collect();
                    format!("…{}", tail.chars().rev().collect::<String>())
                } else {
                    s.path.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if on { " ▶ " } else { "   " },
                        Style::new().fg(GREEN).bold(),
                    ),
                    Span::styled(
                        format!("{:<16}", s.name),
                        if on {
                            Style::new().fg(BG).bg(CYAN).bold()
                        } else {
                            Style::new().fg(FG)
                        },
                    ),
                    Span::styled(
                        if s.read_only { " ro " } else { " rw " },
                        Style::new().fg(if s.read_only { AMBER } else { GREEN }),
                    ),
                    Span::styled(format!(" {path}"), Style::new().fg(DIM)),
                ]));
            }
            lines.push(Line::raw(""));
            match input {
                Some(t) => {
                    lines.push(Line::from(vec![
                        Span::styled(" folder to share: ", Style::new().fg(GREEN).bold()),
                        Span::styled(t.clone(), Style::new().fg(FG)),
                        Span::styled("▌", Style::new().fg(GREEN)),
                    ]));
                    lines.push(Line::styled(
                        " type a path · ⏎ share · Esc cancel",
                        Style::new().fg(EDGE),
                    ));
                }
                None => {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        " ↑/↓ choose · a add · d remove · o read-only on/off · Esc close",
                        Style::new().fg(EDGE),
                    ));
                }
            }
            f.render_widget(Paragraph::new(lines).block(block), r);
        }
        Modal::Help => {
            let r = centered(area, 64, 25);
            f.render_widget(Clear, r);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(GREEN))
                .style(Style::new().bg(PANEL).fg(FG))
                .title(Line::styled(
                    " Conduit · keys ",
                    Style::new().fg(GREEN).bold(),
                ));
            let k = |key: &str, what: &str| {
                Line::from(vec![
                    Span::styled(format!(" {key:>9} "), Style::new().fg(BG).bg(GREEN).bold()),
                    Span::styled(format!("  {what}"), Style::new().fg(FG)),
                ])
            };
            let lines = vec![
                Line::raw(""),
                k("↑ ↓ j k", "select a VM (or click it)"),
                k("⏎ v", "open it in a window (starts it if needed)"),
                k("s", "start it without a window"),
                k("d", "shut down (power button)"),
                k("R", "reboot"),
                k("r", "reset (hard)"),
                k("f", "force off (pull the plug)"),
                k("p", "pause / resume"),
                k("m", "display mode for the next view/start"),
                k("F", "shared folders (a add, d remove, o read-only)"),
                k("l  2", "logs of the selected VM"),
                k("D  3", "doctor: check this computer"),
                k("4", "apps in the VM (⏎ run, a add a launcher, / filter)"),
                k("Tab 1", "switch tabs / back to the dashboard"),
                k("q Esc", "back / quit (VMs keep running)"),
                Line::raw(""),
                Line::styled(
                    "  Windows VMs get --venus automatically.",
                    Style::new().fg(DIM),
                ),
                Line::styled(
                    "  Each action runs the matching `conduit` command.",
                    Style::new().fg(DIM),
                ),
                Line::raw(""),
                Line::styled("  any key to close", Style::new().fg(EDGE)),
            ];
            f.render_widget(Paragraph::new(lines).block(block), r);
            let _ = app;
        }
    }
}

/// The opening: the logo wipes in on a gradient, the tagline types itself.
fn splash(f: &mut Frame, app: &App, area: Rect) {
    let frame = 26u64.saturating_sub(app.intro) as usize;
    let logo_w = LOGO[0].chars().count() * 2;
    let reveal = frame * logo_w / 12;
    let tag = "share your NVIDIA GPU with a virtual machine";
    let typed = (frame.saturating_sub(10) * 4).min(tag.len());
    let h = 9u16;
    let top = area.y + area.height.saturating_sub(h) / 2;
    let mut lines = Vec::new();
    for row in LOGO {
        let mut spans = Vec::new();
        for (i, c) in row.chars().flat_map(|c| [c, c]).enumerate() {
            let t = (i as f32 / logo_w as f32 + frame as f32 * 0.04).rem_euclid(1.0);
            let col = ramp(&[GREEN, CYAN, VIOLET, PINK, GREEN], t);
            let c = if i < reveal {
                c
            } else if i < reveal + 3 && c != ' ' {
                '░'
            } else {
                ' '
            };
            let shown = if c == ' ' {
                ' '.to_string()
            } else {
                c.to_string()
            };
            spans.push(Span::styled(shown, Style::new().fg(col).bold()));
        }
        lines.push(Line::from(spans).centered());
    }
    lines.push(Line::raw(""));
    let mut t = vec![Span::styled(&tag[..typed], Style::new().fg(FG))];
    if typed < tag.len() || (app.tick / 4).is_multiple_of(2) {
        t.push(Span::styled("▌", Style::new().fg(GREEN)));
    }
    lines.push(Line::from(t).centered());
    lines.push(Line::raw(""));
    let gpu = app
        .snap
        .gpu
        .as_ref()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "looking for the GPU".into());
    let s = SPIN[(app.tick as usize) % SPIN.len()];
    lines.push(
        Line::from(vec![
            Span::styled(format!("{s} "), Style::new().fg(PINK)),
            Span::styled(gpu, Style::new().fg(DIM)),
            Span::styled(
                format!(" · {} VMs", app.snap.vms.len()),
                Style::new().fg(DIM),
            ),
        ])
        .centered(),
    );
    let r = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: h.min(area.height),
    };
    f.render_widget(Paragraph::new(lines), r);
}
