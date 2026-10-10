//! The popup's drawing. All coordinates are logical pixels; the caller scales.

use crate::gfx::{argb, Align, Gfx, Rgb};
use conduit_stats::Reading;
use gpu_tray::{gib, heat, load_color, temp_color, vram_fraction};

pub const WIDTH: f32 = 380.0;
pub const HEIGHT_FULL: f32 = 612.0;
pub const HEIGHT_WAITING: f32 = 224.0;

const BG: Rgb = (16, 18, 20);
const CARD: Rgb = (24, 27, 31);
const EDGE: Rgb = (38, 42, 48);
const TRACK: Rgb = (36, 40, 46);
const TEXT: Rgb = (232, 234, 237);
const MUTED: Rgb = (139, 146, 154);
const GREEN: Rgb = (118, 185, 0);
const PAD: f32 = 16.0;

/// Why there is nothing to show.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    NoChannel,
    Denied,
    Quiet,
}

/// What the popup shows.
pub struct Snapshot {
    pub reading: Option<Reading>,
    pub wait: Wait,
    pub load: Vec<f32>,
    pub power_w: Vec<f32>,
    pub temp_c: Vec<f32>,
}

pub fn height(s: &Snapshot) -> f32 {
    if s.reading.is_some() {
        HEIGHT_FULL
    } else {
        HEIGHT_WAITING
    }
}

fn a(c: Rgb) -> u32 {
    argb(255, c)
}

fn or_dash<T: ToString>(v: Option<T>) -> String {
    v.map_or("\u{2013}".into(), |v| v.to_string())
}

pub fn draw(g: &Gfx, s: &Snapshot) {
    let h = height(s);
    g.clear(a(BG));
    g.stroke_rrect(argb(255, EDGE), 1.0, 0.5, 0.5, WIDTH - 1.0, h - 1.0, 8.0);
    match &s.reading {
        Some(r) => full(g, r, s),
        None => waiting(g, s.wait),
    }
}

fn waiting(g: &Gfx, why: Wait) {
    g.fill_rrect(a(GREEN), PAD, PAD, 4.0, 34.0, 2.0);
    g.text(
        "Conduit GPU",
        a(TEXT),
        15.0,
        true,
        Align::Left,
        false,
        (PAD + 12.0, PAD - 2.0, 260.0, 20.0),
    );
    g.text(
        "Host GPU monitor",
        a(MUTED),
        11.0,
        false,
        Align::Left,
        false,
        (PAD + 12.0, PAD + 20.0, 260.0, 16.0),
    );
    // A quiet ring with a green arc of dots.
    let (cx, cy) = (WIDTH / 2.0, 108.0);
    for i in 0..12 {
        let t = i as f32 / 12.0 * std::f32::consts::TAU;
        let alpha = 40 + (i * 18) as u8;
        g.fill_circle(
            argb(alpha.min(230), GREEN),
            cx + 20.0 * t.cos(),
            cy + 20.0 * t.sin(),
            2.6,
        );
    }
    g.text(
        "Waiting for Conduit host feed\u{2026}",
        a(TEXT),
        14.0,
        true,
        Align::Center,
        false,
        (PAD, 146.0, WIDTH - 2.0 * PAD, 20.0),
    );
    let sub = match why {
        Wait::NoChannel => "The feed starts with the VM. Restart it once after `conduit attach`.",
        Wait::Denied => {
            "Windows denied access to the stats channel. Run the app as administrator once."
        }
        Wait::Quiet => {
            "The host stopped sending readings. It resumes when the VM's helper is back."
        }
    };
    g.text(
        sub,
        a(MUTED),
        11.0,
        false,
        Align::Center,
        false,
        (PAD + 8.0, 172.0, WIDTH - 2.0 * PAD - 16.0, 40.0),
    );
}

fn full(g: &Gfx, r: &Reading, s: &Snapshot) {
    // Header.
    g.fill_rrect(a(GREEN), PAD, PAD, 4.0, 36.0, 2.0);
    g.text(
        &r.gpu.replace("NVIDIA ", ""),
        a(TEXT),
        16.0,
        true,
        Align::Left,
        false,
        (PAD + 12.0, PAD - 3.0, 250.0, 22.0),
    );
    let mut sub = format!("Driver {}", r.driver);
    if let Some(p) = r.pstate {
        sub += &format!("   P{p}");
    }
    if !r.host.is_empty() {
        sub += &format!("   {}", r.host);
    }
    g.text(
        &sub,
        a(MUTED),
        11.0,
        false,
        Align::Left,
        false,
        (PAD + 12.0, PAD + 21.0, 270.0, 16.0),
    );
    // LIVE pill.
    let (pw, py) = (54.0, PAD + 2.0);
    g.fill_rrect(argb(40, GREEN), WIDTH - PAD - pw, py, pw, 20.0, 10.0);
    g.fill_circle(a(GREEN), WIDTH - PAD - pw + 12.0, py + 10.0, 3.0);
    g.text(
        "LIVE",
        a(GREEN),
        10.0,
        true,
        Align::Left,
        true,
        (WIDTH - PAD - pw + 21.0, py, 32.0, 20.0),
    );

    // Big readouts.
    let gap = 10.0;
    let cw = (WIDTH - 2.0 * PAD - 2.0 * gap) / 3.0;
    let y = 68.0;
    let load = r.util_gpu;
    let power = r.power_mw.map(|p| (p as f32 / 1000.0).round() as u32);
    let temp = r.temp_c;
    let cards: [(&str, Option<u32>, &str, Rgb, String); 3] = [
        (
            "LOAD",
            load,
            "%",
            load.map_or(GREEN, load_color),
            format!("Memory {}%", or_dash(r.util_mem)),
        ),
        (
            "POWER",
            power,
            "W",
            match (r.power_mw, r.power_limit_mw) {
                (Some(p), Some(l)) if l > 0 => heat(p as f32 / l as f32 * 100.0, 20.0, 100.0),
                _ => GREEN,
            },
            r.power_limit_mw
                .map_or("no limit known".into(), |l| format!("of {} W", l / 1000)),
        ),
        (
            "TEMP",
            temp,
            "\u{b0}C",
            temp.map_or(GREEN, temp_color),
            format!(
                "Fan {}",
                r.fan_pct.map_or("\u{2013}".into(), |f| format!("{f}%"))
            ),
        ),
    ];
    for (i, (label, val, unit, col, subtxt)) in cards.iter().enumerate() {
        let x = PAD + i as f32 * (cw + gap);
        g.fill_rrect(a(CARD), x, y, cw, 88.0, 10.0);
        g.stroke_rrect(a(EDGE), 1.0, x + 0.5, y + 0.5, cw - 1.0, 87.0, 10.0);
        g.text(
            label,
            a(MUTED),
            10.0,
            true,
            Align::Left,
            false,
            (x + 12.0, y + 10.0, cw - 16.0, 14.0),
        );
        let v = or_dash(*val);
        g.text(
            &v,
            a(*col),
            32.0,
            true,
            Align::Left,
            false,
            (x + 10.0, y + 24.0, cw, 42.0),
        );
        let vw = g.text_width(&v, 32.0, true);
        g.text(
            unit,
            a(MUTED),
            13.0,
            false,
            Align::Left,
            false,
            (x + 12.0 + vw, y + 41.0, 30.0, 18.0),
        );
        g.text(
            subtxt,
            a(MUTED),
            10.0,
            false,
            Align::Left,
            false,
            (x + 12.0, y + 67.0, cw - 14.0, 14.0),
        );
    }

    // Sparklines.
    let mut y = 172.0;
    section(g, "LAST 60 SECONDS", y);
    y += 20.0;
    let limit_w = r.power_limit_mw.map(|l| l as f32 / 1000.0);
    let rows: [(&str, &[f32], (f32, f32), Rgb, String); 3] = [
        (
            "Load",
            &s.load,
            (0.0, 100.0),
            load.map_or(GREEN, load_color),
            format!("{}%", or_dash(load)),
        ),
        (
            "Power",
            &s.power_w,
            (
                0.0,
                limit_w
                    .unwrap_or_else(|| s.power_w.iter().copied().fold(100.0, f32::max))
                    .max(1.0),
            ),
            GREEN,
            format!("{} W", or_dash(power)),
        ),
        (
            "Temperature",
            &s.temp_c,
            (30.0, 95.0),
            temp.map_or(GREEN, temp_color),
            format!("{}\u{b0}C", or_dash(temp)),
        ),
    ];
    for (label, series, range, col, now) in rows {
        spark(g, label, &now, series, range, col, y);
        y += 72.0;
    }

    // Bars.
    y += 4.0;
    section(g, "DETAILS", y);
    y += 20.0;
    let vf = vram_fraction(r);
    let ratio = |v: Option<u32>, m: Option<u32>| match (v, m) {
        (Some(v), Some(m)) if m > 0 => Some((v as f32 / m as f32).clamp(0.0, 1.0)),
        _ => None,
    };
    let vram_txt = match (r.vram_used, r.vram_total) {
        (Some(u), Some(t)) => format!("{:.1} / {:.1} GiB", gib(u), gib(t)),
        _ => "\u{2013}".into(),
    };
    let power_f = ratio(r.power_mw, r.power_limit_mw);
    let bars: [(&str, String, Option<f32>, bool); 5] = [
        ("VRAM", vram_txt, vf, true),
        (
            "Core clock",
            format!("{} / {} MHz", or_dash(r.clk_gfx), or_dash(r.clk_gfx_max)),
            ratio(r.clk_gfx, r.clk_gfx_max),
            false,
        ),
        (
            "Memory clock",
            format!("{} / {} MHz", or_dash(r.clk_mem), or_dash(r.clk_mem_max)),
            ratio(r.clk_mem, r.clk_mem_max),
            false,
        ),
        (
            "Power",
            format!(
                "{} / {} W",
                or_dash(power),
                or_dash(r.power_limit_mw.map(|l| l / 1000))
            ),
            power_f,
            true,
        ),
        (
            "Fan",
            r.fan_pct.map_or("\u{2013}".into(), |f| format!("{f}%")),
            r.fan_pct.map(|f| (f as f32 / 100.0).min(1.0)),
            false,
        ),
    ];
    for (label, val, frac, warm) in bars {
        bar(g, label, &val, frac, warm, y);
        y += 34.0;
    }
}

fn section(g: &Gfx, title: &str, y: f32) {
    g.text(
        title,
        a(MUTED),
        10.0,
        true,
        Align::Left,
        false,
        (PAD + 2.0, y, 200.0, 14.0),
    );
}

fn spark(g: &Gfx, label: &str, now: &str, series: &[f32], (lo, hi): (f32, f32), col: Rgb, y: f32) {
    let (x, w, h) = (PAD, WIDTH - 2.0 * PAD, 64.0);
    g.fill_rrect(a(CARD), x, y, w, h, 10.0);
    g.stroke_rrect(a(EDGE), 1.0, x + 0.5, y + 0.5, w - 1.0, h - 1.0, 10.0);
    g.text(
        label,
        a(MUTED),
        10.5,
        true,
        Align::Left,
        false,
        (x + 12.0, y + 8.0, 150.0, 14.0),
    );
    g.text(
        now,
        a(col),
        12.0,
        true,
        Align::Right,
        false,
        (x + w - 112.0, y + 6.0, 100.0, 18.0),
    );
    let (gx, gy, gw, gh) = (x + 12.0, y + 26.0, w - 24.0, h - 34.0);
    g.line(
        argb(60, EDGE),
        1.0,
        gx,
        gy + gh / 2.0,
        gx + gw,
        gy + gh / 2.0,
    );
    g.line(argb(255, EDGE), 1.0, gx, gy + gh, gx + gw, gy + gh);
    if series.len() < 2 {
        return;
    }
    let n = gpu_tray::HISTORY as f32 - 1.0;
    // The newest sample sits at the right edge; a short history starts partway in.
    let off = gpu_tray::HISTORY - series.len();
    let pt = |i: usize, v: f32| {
        let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        (gx + (i + off) as f32 / n * gw, gy + gh - t * gh)
    };
    let pts: Vec<(f32, f32)> = series.iter().enumerate().map(|(i, &v)| pt(i, v)).collect();
    let mut poly = pts.clone();
    poly.push((pts[pts.len() - 1].0, gy + gh));
    poly.push((pts[0].0, gy + gh));
    g.fill_poly_vertical(argb(110, col), argb(0, col), gy, gy + gh, &poly);
    g.polyline(a(col), 1.8, &pts);
    let last = pts[pts.len() - 1];
    g.fill_circle(a(col), last.0, last.1, 2.6);
}

fn bar(g: &Gfx, label: &str, val: &str, frac: Option<f32>, warm: bool, y: f32) {
    let (x, w) = (PAD + 2.0, WIDTH - 2.0 * PAD - 4.0);
    g.text(
        label,
        a(MUTED),
        11.0,
        false,
        Align::Left,
        false,
        (x, y, 150.0, 16.0),
    );
    g.text(
        val,
        a(TEXT),
        11.0,
        true,
        Align::Right,
        false,
        (x + w - 220.0, y, 220.0, 16.0),
    );
    let by = y + 19.0;
    g.fill_rrect(a(TRACK), x, by, w, 7.0, 3.5);
    if let Some(f) = frac {
        let fw = (w * f).max(7.0);
        let end = if warm {
            heat(f * 100.0, 40.0, 100.0)
        } else {
            (166, 224, 40)
        };
        g.fill_rrect_grad(a(GREEN), a(end), x, by, fw, 7.0, 3.5);
    }
}
