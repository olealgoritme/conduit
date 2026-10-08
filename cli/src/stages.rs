//! `conduit trace NAME stages` and `conduit trace stages DIR`: where each
//! frame's time goes, stage by stage (docs/TRACING.md "Frame stage timing").
//!
//! The backend and `conduit-venus` stamp every windowed-Present copy (a
//! fenced `SUBMIT_3D`, by its fence id) and every flip (`ScanoutFlip`, by its
//! `seq`) as it passes each stage, in `CLOCK_MONOTONIC` ns; the guest driver
//! stamps the same ids in its interrupt time (100 ns) and publishes its ring
//! as the registry value `StgRing`. This collects both for a while, puts the
//! guest's stamps on the host clock, joins them by id and prints, per path,
//! the mean, p50 and p99 of every interval between consecutive stages and its
//! share of the frame interval. The same joined data can be written as a
//! Chrome trace-event / Perfetto JSON file.

use crate::trace::socket;
use crate::ui::{info, oops};
use anyhow::{Context, Result};
use conduit_venus::stage::{self, guest, Rec};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Path numbers: the frame kinds of `conduit_venus::stage`.
const COPY: u8 = stage::KIND_FENCE;
const FLIP: u8 = stage::KIND_FLIP;

/// The order a copy passes its stages in: a causal chain, each stage after
/// the one ahead of it. Kept apart (`Frame::side`): `V_GPU`, which is not a
/// point in time, and `V_SUBMIT` / `V_SUBMIT_DONE`, vkr's `vkQueueSubmit` of
/// the stream, which runs on the render server thread concurrently with
/// conduit-venus's serving thread (before or after `R_SUBMITTED`, `R_FENCE`,
/// `H_FENCE_ASKED`) and always before `V_FENCE`; they get rows of their own.
const COPY_ORDER: &[u8] = &[
    stage::G_PRESENT,
    stage::G_DEFER,
    stage::G_SUBMIT,
    stage::H_KICK,
    stage::H_DECODED,
    stage::R_RECV,
    stage::R_SUBMITTED,
    stage::H_SUBMITTED,
    stage::R_FENCE,
    stage::H_FENCE_ASKED,
    stage::V_FENCE,
    stage::V_FENCE_DONE,
    stage::R_SIGNAL,
    stage::R_PUSH,
    stage::H_SIGNALLED,
    stage::H_USED,
    stage::H_IRQ,
    stage::G_ISR,
    stage::G_DONE,
    stage::G_NOTIFY,
];

/// The order a flip passes its stages in.
const FLIP_ORDER: &[u8] = &[
    stage::G_FLIP_DDI,
    stage::G_FLIP_SUBMIT,
    stage::H_KICK,
    stage::H_DECODED,
    stage::H_DISPLAY,
    stage::H_USED,
    stage::H_IRQ,
    stage::G_FLIP_ISR,
    stage::G_FLIP_ACK,
    stage::G_FLIP_RETIRE,
];

fn order(path: u8) -> &'static [u8] {
    if path == FLIP {
        FLIP_ORDER
    } else {
        COPY_ORDER
    }
}

fn rank(path: u8, s: u8) -> Option<usize> {
    order(path).iter().position(|&x| x == s)
}

fn path_name(path: u8) -> &'static str {
    if path == FLIP {
        "foreign flip"
    } else {
        "windowed copy"
    }
}

// ------------------------------------------------------------- collection

/// What `conduit trace NAME stages` was asked for.
pub struct Opts {
    pub duration: u64,
    pub guest_cmd: Option<String>,
    pub save: Option<PathBuf>,
    pub perfetto: Option<PathBuf>,
    pub etw: Option<PathBuf>,
}

/// Everything collected: the host's records and the guest's snapshots, in
/// the order they were taken.
#[derive(Default)]
pub struct Collected {
    pub host: Vec<Rec>,
    pub host_lost: u64,
    /// Raw `StgRing` bytes; the first is the baseline (its records predate
    /// the collection and are not used).
    pub guest: Vec<Vec<u8>>,
}

fn connect(name: &str) -> Result<UnixStream> {
    let sock = socket(name);
    UnixStream::connect(&sock).map_err(|e| {
        oops(
            format!("cannot reach {name}'s GPU backend ({e})"),
            format!("Is {name} running? ({})", sock.display()),
        )
    })
}

/// One command on the trace socket; the whole answer.
fn ask(name: &str, cmd: &str) -> Result<Vec<u8>> {
    let mut s = connect(name)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(s, "{cmd}")?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    if out.starts_with(b"unknown command") {
        return Err(oops(
            format!("{name}'s GPU backend has no stage timing"),
            "Update conduit (the backend and conduit-venus) and restart the VM.",
        ));
    }
    Ok(out)
}

fn ask_text(name: &str, cmd: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&ask(name, cmd)?)
        .trim_end()
        .to_string())
}

fn host_dump(name: &str, c: &mut Collected) -> Result<()> {
    let b = ask(name, "stages dump")?;
    let (recs, lost) = stage::decode_dump(&b).ok_or_else(|| {
        oops(
            "the backend's stage dump is not readable",
            String::from_utf8_lossy(&b[..b.len().min(120)]).to_string(),
        )
    })?;
    c.host.extend(recs);
    c.host_lost += lost;
    Ok(())
}

fn guest_snapshot(cmd: &str, c: &mut Collected) {
    let out = std::process::Command::new("sh").arg("-c").arg(cmd).output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            match guest::from_reg_query(&text) {
                Some(b) if guest::parse(&b).is_some() => c.guest.push(b),
                _ => info("guest: no StgRing value in the command's output (is StageTrace=1 set?)"),
            }
        }
        Ok(o) => info(format!(
            "guest: the command failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => info(format!("guest: cannot run the command: {e}")),
    }
}

/// `conduit trace NAME stages`.
pub fn live(name: &str, o: Opts) -> Result<()> {
    info(format!("collecting frame stages for {} s", o.duration));
    let end = Instant::now() + Duration::from_secs(o.duration);
    let c = collect(name, o.guest_cmd.as_deref(), &|| Instant::now() >= end)?;
    if let Some(dir) = &o.save {
        save(dir, &c)?;
        info(format!("saved to {}", dir.display()));
    }
    report(&c, o.perfetto.as_deref(), o.etw.as_deref(), Vec::new())
}

/// Turn stage stamping on (unless it was), collect every second until
/// `stop` says so, and turn it off again (if it was off).
pub fn collect(name: &str, guest_cmd: Option<&str>, stop: &dyn Fn() -> bool) -> Result<Collected> {
    // `stages status` begins "stages on" or "stages off".
    let status = ask_text(name, "stages status")?;
    let was_on = status
        .split_whitespace()
        .take(2)
        .any(|w| w == "on" || w == "on;" || w == "on,");
    let r = ask_text(name, "stages on")?;
    if !r.starts_with("ok") {
        return Err(oops(format!("stages on: {r}"), ""));
    }
    let mut c = Collected::default();
    // Whatever was stamped before now is not part of this collection.
    let mut stale = Collected::default();
    host_dump(name, &mut stale)?;
    if let Some(cmd) = guest_cmd {
        guest_snapshot(cmd, &mut c);
    }
    let run = (|| -> Result<()> {
        while !stop() {
            let tick = Instant::now() + Duration::from_secs(1);
            while !stop() && Instant::now() < tick {
                std::thread::sleep(Duration::from_millis(50));
            }
            host_dump(name, &mut c)?;
            if let Some(cmd) = guest_cmd {
                guest_snapshot(cmd, &mut c);
            }
        }
        // The renderer's records reach the backend on its fence pump.
        std::thread::sleep(Duration::from_millis(300));
        host_dump(name, &mut c)?;
        if let Some(cmd) = guest_cmd {
            guest_snapshot(cmd, &mut c);
        }
        Ok(())
    })();
    if !was_on {
        let _ = ask_text(name, "stages off");
    }
    run?;
    Ok(c)
}

pub fn save(dir: &Path, c: &Collected) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(
        dir.join("host.bin"),
        stage::encode_dump(&c.host, c.host_lost),
    )?;
    for (i, g) in c.guest.iter().enumerate() {
        std::fs::write(dir.join(format!("guest-{i:03}.bin")), g)?;
    }
    Ok(())
}

/// `conduit trace stages DIR`: a collection saved with `--save`.
pub fn offline(dir: &Path, perfetto: Option<&Path>, etw: Option<&Path>) -> Result<()> {
    let c = load(dir)?;
    if c.host.is_empty() && c.guest.is_empty() {
        return Err(oops(
            format!("nothing in {}", dir.display()),
            "Expected host.bin and guest-NNN.bin",
        ));
    }
    report(&c, perfetto, etw, Vec::new())
}

/// What `save` wrote to `dir` (empty when it holds no stage files).
pub fn load(dir: &Path) -> Result<Collected> {
    let mut c = Collected::default();
    let host = dir.join("host.bin");
    if host.exists() {
        let b = std::fs::read(&host)?;
        let (recs, lost) = stage::decode_dump(&b)
            .ok_or_else(|| oops(format!("{} is not a stage dump", host.display()), ""))?;
        c.host = recs;
        c.host_lost = lost;
    }
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|n| n.starts_with("guest-") && n.ends_with(".bin"))
        .collect();
    names.sort();
    for n in names {
        c.guest.push(std::fs::read(dir.join(n))?);
    }
    Ok(c)
}

/// The guest's records, each once, from overlapping snapshots; and how many
/// were lost (overwritten in the guest's ring between two snapshots). The
/// first snapshot is the baseline: its records predate the collection.
pub fn dedupe_guest(snaps: &[Vec<u8>]) -> (Vec<guest::GRec>, u64) {
    let mut out = Vec::new();
    let mut lost = 0;
    let mut seen: Option<u64> = None; // highest index taken
    for (i, b) in snaps.iter().enumerate() {
        let Some(s) = guest::parse(b) else { continue };
        if i == 0 {
            seen = s.head.checked_sub(1);
            continue;
        }
        let fresh: Vec<_> = s
            .recs
            .into_iter()
            .filter(|r| seen.is_none_or(|n| r.index > n))
            .collect();
        if let (Some(first), Some(n)) = (fresh.first(), seen) {
            lost += first.index.saturating_sub(n + 1);
        }
        if let Some(last) = fresh.last() {
            seen = Some(last.index);
        }
        out.extend(fresh);
    }
    (out, lost)
}

// ------------------------------------------------------- clock correlation

/// The guest's interrupt time (100 ns) on the host's `CLOCK_MONOTONIC`:
/// `host_ns = 100 * t_guest + offset`.
///
/// Method: every frame that went both ways bounds the offset from both
/// sides, by causality alone. The host can only decode a descriptor after
/// the guest stamped `G_SUBMIT` (taken before the descriptor is published),
/// so `offset <= H_DECODED - 100 * G_SUBMIT`; the guest can only find a
/// completion after the host put it on the used ring, so
/// `offset >= H_USED - 100 * G_DONE` (the same with `G_FLIP_SUBMIT` /
/// `G_FLIP_ACK` for flips). Within each 1 s window of host time the
/// feasible interval is [max of the lower bounds, min of the upper bounds];
/// the estimate is its midpoint and the error bound half its width (the
/// fastest round trip in the window sets it, typically tens of µs). The
/// estimate between windows is interpolated linearly, which follows a
/// slewing host clock; windows with only one-sided or contradictory bounds
/// are skipped. The drift is the slope of the window midpoints.
#[derive(Clone, Debug)]
pub struct Clock {
    /// (host ns at the window's samples' mean, offset midpoint, half width).
    pub points: Vec<(f64, f64, f64)>,
    /// Frames that contributed a bound.
    pub samples: usize,
    /// Over the whole collection, if consistent.
    pub global: Option<(f64, f64)>,
    pub drift_ppm: f64,
}

/// One bound on the offset: `(host ns it was seen at, value, is_upper)`.
pub type Bound = (f64, f64, bool);

const WINDOW_NS: f64 = 1e9;

impl Clock {
    pub fn from_bounds(bounds: &[Bound]) -> Option<Clock> {
        let (has_lo, has_hi) = (bounds.iter().any(|b| !b.2), bounds.iter().any(|b| b.2));
        if !has_lo || !has_hi {
            return None;
        }
        let t0 = bounds.iter().map(|b| b.0).fold(f64::INFINITY, f64::min);
        let mut win: BTreeMap<i64, (f64, f64, f64, usize)> = BTreeMap::new();
        for &(t, v, upper) in bounds {
            let w = win.entry(((t - t0) / WINDOW_NS).floor() as i64).or_insert((
                f64::NEG_INFINITY,
                f64::INFINITY,
                0.0,
                0,
            ));
            if upper {
                w.1 = w.1.min(v);
            } else {
                w.0 = w.0.max(v);
            }
            w.2 += t;
            w.3 += 1;
        }
        let points: Vec<_> = win
            .values()
            .filter(|w| w.0.is_finite() && w.1.is_finite() && w.0 <= w.1)
            .map(|w| (w.2 / w.3 as f64, (w.0 + w.1) / 2.0, (w.1 - w.0) / 2.0))
            .collect();
        let lo = bounds
            .iter()
            .filter(|b| !b.2)
            .map(|b| b.1)
            .fold(f64::NEG_INFINITY, f64::max);
        let hi = bounds
            .iter()
            .filter(|b| b.2)
            .map(|b| b.1)
            .fold(f64::INFINITY, f64::min);
        let global = (lo <= hi).then_some((lo, hi));
        let points = if points.is_empty() {
            // No window is consistent on its own: the whole run, if it is.
            let (lo, hi) = global?;
            vec![(t0, (lo + hi) / 2.0, (hi - lo) / 2.0)]
        } else {
            points
        };
        let drift_ppm = if points.len() >= 2 {
            let n = points.len() as f64;
            let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
            let my = points.iter().map(|p| p.1).sum::<f64>() / n;
            let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
            let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
            if sxx > 0.0 {
                sxy / sxx * 1e6
            } else {
                0.0
            }
        } else {
            0.0
        };
        Some(Clock {
            points,
            samples: bounds.len(),
            global,
            drift_ppm,
        })
    }

    /// The offset at host time `t`: linear between the windows around it,
    /// extended from the first or last two outside them.
    pub fn offset_at(&self, t: f64) -> f64 {
        let p = &self.points;
        if p.len() == 1 {
            return p[0].1;
        }
        let line =
            |a: &(f64, f64, f64), b: &(f64, f64, f64)| a.1 + (t - a.0) / (b.0 - a.0) * (b.1 - a.1);
        let i = p
            .windows(2)
            .position(|w| t <= w[1].0)
            .unwrap_or(p.len() - 2);
        line(&p[i], &p[i + 1])
    }

    /// A guest time (100 ns) in host ns.
    pub fn to_host(&self, t100: u64) -> f64 {
        let g = t100 as f64 * 100.0;
        let first = g + self.offset_at(g + self.points[0].1);
        g + self.offset_at(first)
    }

    /// The largest half-width of the windows used: the error bound.
    pub fn error_ns(&self) -> f64 {
        self.points.iter().map(|p| p.2).fold(0.0, f64::max)
    }

    fn median_offset(&self) -> f64 {
        let mut v: Vec<f64> = self.points.iter().map(|p| p.1).collect();
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    }
}

/// The bounds every frame present on both sides gives.
pub fn bounds(host: &[Rec], guest: &[guest::GRec]) -> Vec<Bound> {
    let mut g: HashMap<(u8, u32, u8), u64> = HashMap::new();
    for r in guest {
        g.entry((r.kind, r.id, r.stage)).or_insert(r.t);
    }
    let mut out = Vec::new();
    for r in host {
        let id = r.id as u32;
        let (submit, done) = match r.kind {
            COPY if r.ring == 1 => (stage::G_SUBMIT, stage::G_DONE),
            FLIP => (stage::G_FLIP_SUBMIT, stage::G_FLIP_ACK),
            _ => continue,
        };
        let h = r.ts_ns as f64;
        if r.stage == stage::H_DECODED {
            if let Some(&t) = g.get(&(r.kind, id, submit)) {
                out.push((h, h - 100.0 * t as f64, true));
            }
        } else if r.stage == stage::H_USED {
            if let Some(&t) = g.get(&(r.kind, id, done)) {
                out.push((h, h - 100.0 * t as f64, false));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ frames

/// One frame on one path: its stamps on the host clock, in canonical order.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub path: u8,
    pub id: u32,
    /// (stage, host ns), canonical order, each stage once.
    pub stamps: Vec<(u8, f64)>,
    /// The GPU duration from timestamp queries.
    pub gpu_ns: Option<u64>,
    /// Stamps off the chain (`V_SUBMIT`, `V_SUBMIT_DONE`), host ns.
    pub side: Vec<(u8, f64)>,
    pub has_host: bool,
    pub has_guest: bool,
}

/// Join the host's and the guest's stamps by id. Copies: host fences on
/// ring 1, and with guest data only the ids the guest submitted as copies.
/// Without a clock the guest's stamps cannot be placed and are left out.
pub fn join(host: &[Rec], guest: &[guest::GRec], clock: Option<&Clock>) -> Vec<Frame> {
    let guest_copies: std::collections::HashSet<u32> = guest
        .iter()
        .filter(|r| r.kind == COPY)
        .map(|r| r.id)
        .collect();
    let mut frames: BTreeMap<(u8, u32), Frame> = BTreeMap::new();
    fn get(frames: &mut BTreeMap<(u8, u32), Frame>, path: u8, id: u32) -> &mut Frame {
        frames.entry((path, id)).or_insert_with(|| Frame {
            path,
            id,
            ..Default::default()
        })
    }
    for r in host {
        let id = r.id as u32;
        let path = match r.kind {
            COPY if r.ring == 1 && (guest_copies.is_empty() || guest_copies.contains(&id)) => COPY,
            FLIP => FLIP,
            _ => continue,
        };
        let f = get(&mut frames, path, id);
        f.has_host = true;
        if r.stage == stage::V_GPU {
            f.gpu_ns.get_or_insert(r.aux);
        } else if r.stage == stage::V_SUBMIT || r.stage == stage::V_SUBMIT_DONE {
            if !f.side.iter().any(|x| x.0 == r.stage) {
                f.side.push((r.stage, r.ts_ns as f64));
            }
        } else if rank(path, r.stage).is_some() && !f.stamps.iter().any(|s| s.0 == r.stage) {
            f.stamps.push((r.stage, r.ts_ns as f64));
        }
    }
    if let Some(c) = clock {
        for r in guest {
            if r.kind != COPY && r.kind != FLIP {
                continue;
            }
            let f = get(&mut frames, r.kind, r.id);
            f.has_guest = true;
            if rank(r.kind, r.stage).is_some() && !f.stamps.iter().any(|s| s.0 == r.stage) {
                f.stamps.push((r.stage, c.to_host(r.t)));
            }
        }
    } else {
        for r in guest {
            if let Some(f) = frames.get_mut(&(r.kind, r.id)) {
                f.has_guest = true;
            } else if r.kind == COPY || r.kind == FLIP {
                frames.insert(
                    (r.kind, r.id),
                    Frame {
                        path: r.kind,
                        id: r.id,
                        has_guest: true,
                        ..Default::default()
                    },
                );
            }
        }
    }
    let mut out: Vec<Frame> = frames.into_values().collect();
    for f in &mut out {
        sort_stamps(f);
    }
    out
}

/// A frame's stamps in their causal order (`COPY_ORDER` / `FLIP_ORDER`):
/// each stage of the chain cannot happen before the one ahead of it, so an
/// interval is negative only by the clock correlation's error.
fn sort_stamps(f: &mut Frame) {
    let p = f.path;
    f.stamps.sort_by_key(|s| rank(p, s.0));
}

// ------------------------------------------------------------------- table

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p99_us: f64,
}

/// Nearest-rank percentiles of `v` (µs).
pub fn stats(v: &mut [f64]) -> Option<Stats> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| v[((p / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    Some(Stats {
        n: v.len(),
        mean_us: v.iter().sum::<f64>() / v.len() as f64,
        p50_us: at(50.0),
        p99_us: at(99.0),
    })
}

#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub stats: Stats,
    /// A breakdown of other rows, not part of their sum.
    pub detail: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PathReport {
    pub path: u8,
    pub frames: usize,
    pub rows: Vec<Row>,
    pub total: Option<Stats>,
    /// Mean time between consecutive frames' first stamps, µs.
    pub interval_us: Option<f64>,
    /// Intervals that came out negative (clamped to 0).
    pub reordered: usize,
}

/// Every interval of one frame: `(from, to, start ns, length ns)`, negatives
/// clamped; and how many were negative.
fn intervals(f: &Frame) -> (Vec<(u8, u8, f64, f64)>, usize) {
    let mut out = Vec::new();
    let mut neg = 0;
    for w in f.stamps.windows(2) {
        let d = w[1].1 - w[0].1;
        if d < 0.0 {
            neg += 1;
        }
        out.push((w[0].0, w[1].0, w[0].1, d.max(0.0)));
    }
    (out, neg)
}

fn interval_name(a: u8, b: u8) -> String {
    format!("{} -> {}", stage::name(a), stage::name(b))
}

pub fn path_report(frames: &[Frame], path: u8) -> PathReport {
    let fs: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.path == path && f.stamps.len() >= 2)
        .collect();
    let mut rep = PathReport {
        path,
        frames: fs.len(),
        ..Default::default()
    };
    if fs.is_empty() {
        return rep;
    }
    let mut by: BTreeMap<(usize, usize), (String, Vec<f64>)> = BTreeMap::new();
    let mut totals = Vec::new();
    let mut gpu = Vec::new();
    let mut wake = Vec::new();
    let mut decode = Vec::new();
    let mut vksubmit = Vec::new();
    for f in &fs {
        let (iv, neg) = intervals(f);
        rep.reordered += neg;
        for (a, b, _, d) in iv {
            by.entry((rank(path, a).unwrap(), rank(path, b).unwrap()))
                .or_insert_with(|| (interval_name(a, b), Vec::new()))
                .1
                .push(d / 1e3);
        }
        totals.push((f.stamps.last().unwrap().1 - f.stamps[0].1).max(0.0) / 1e3);
        let t = |s: u8| {
            f.stamps
                .iter()
                .chain(f.side.iter())
                .find(|x| x.0 == s)
                .map(|x| x.1)
        };
        if let (Some(a), Some(b)) = (t(stage::R_RECV), t(stage::V_SUBMIT)) {
            decode.push((b - a).max(0.0) / 1e3);
        }
        if let (Some(a), Some(b)) = (t(stage::V_SUBMIT), t(stage::V_SUBMIT_DONE)) {
            vksubmit.push((b - a).max(0.0) / 1e3);
        }
        if let Some(g) = f.gpu_ns {
            gpu.push(g as f64 / 1e3);
            if let (Some(a), Some(b)) = (t(stage::V_SUBMIT_DONE), t(stage::V_FENCE_DONE)) {
                let span = (b - a).max(0.0);
                wake.push((span - (g as f64).min(span)) / 1e3);
            }
        }
    }
    for (_, (name, mut v)) in by {
        rep.rows.push(Row {
            name,
            stats: stats(&mut v).unwrap(),
            detail: false,
        });
    }
    if let Some(s) = stats(&mut decode) {
        rep.rows.push(Row {
            name: "venus recv -> vkr submit (render server pickup + decode)".into(),
            stats: s,
            detail: true,
        });
    }
    if let Some(s) = stats(&mut vksubmit) {
        rep.rows.push(Row {
            name: "vkr submit -> vkr submit done (vkQueueSubmit call)".into(),
            stats: s,
            detail: true,
        });
    }
    if let Some(s) = stats(&mut gpu) {
        rep.rows.push(Row {
            name: "gpu copy (timestamps)".into(),
            stats: s,
            detail: true,
        });
    }
    if let Some(s) = stats(&mut wake) {
        rep.rows.push(Row {
            name: "vkr submit done -> fence done, minus gpu (queue/fence wake)".into(),
            stats: s,
            detail: true,
        });
    }
    rep.total = stats(&mut totals);
    let mut firsts: Vec<f64> = fs.iter().map(|f| f.stamps[0].1).collect();
    firsts.sort_by(|a, b| a.total_cmp(b));
    if firsts.len() >= 2 {
        rep.interval_us =
            Some((firsts[firsts.len() - 1] - firsts[0]) / (firsts.len() - 1) as f64 / 1e3);
    }
    rep
}

pub fn render(rep: &PathReport) -> String {
    let mut s = format!(
        "\n({}) {}: {} frame(s)",
        if rep.path == FLIP { 'b' } else { 'a' },
        path_name(rep.path),
        rep.frames
    );
    let Some(total) = &rep.total else {
        s.push_str(", nothing to show\n");
        return s;
    };
    let iv = rep.interval_us.unwrap_or(f64::NAN);
    s.push_str(&format!(
        ", frame interval {:.1} us ({:.1} fps)",
        iv,
        1e6 / iv
    ));
    if rep.reordered > 0 {
        s.push_str(&format!(
            ", {} interval(s) out of order (counted as 0)",
            rep.reordered
        ));
    }
    s.push('\n');
    let pct = |m: f64| {
        if iv.is_finite() && iv > 0.0 {
            format!("{:.1}", m / iv * 100.0)
        } else {
            "-".into()
        }
    };
    let w = rep
        .rows
        .iter()
        .map(|r| r.name.len() + 2)
        .max()
        .unwrap_or(10)
        .max(28);
    s.push_str(&format!(
        "{:<w$} {:>7} {:>9} {:>9} {:>9} {:>8}\n",
        "stage", "n", "mean us", "p50", "p99", "% frame"
    ));
    let line = |name: &str, st: &Stats| {
        format!(
            "{:<w$} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>8}\n",
            name,
            st.n,
            st.mean_us,
            st.p50_us,
            st.p99_us,
            pct(st.mean_us)
        )
    };
    for r in &rep.rows {
        let name = if r.detail {
            format!("  ({})", r.name)
        } else {
            r.name.clone()
        };
        s.push_str(&line(&name, &r.stats));
    }
    s.push_str(&line("total (first -> last)", total));
    if iv.is_finite() {
        let un = iv - total.mean_us;
        if un > 0.0 {
            s.push_str(&format!(
                "{:<w$} {:>7} {:>9.1} {:>9} {:>9} {:>8}\n",
                "unattributed",
                "",
                un,
                "",
                "",
                pct(un)
            ));
        }
    }
    s
}

/// The whole report: clock line, counts, both tables; and the optional
/// trace-event file, with `extra` events (host µs) added to it.
pub fn report(
    c: &Collected,
    perfetto: Option<&Path>,
    etw: Option<&Path>,
    mut extra: Vec<Value>,
) -> Result<()> {
    let (grecs, guest_lost) = dedupe_guest(&c.guest);
    let clock = Clock::from_bounds(&bounds(&c.host, &grecs));
    match &clock {
        Some(k) => {
            println!(
                "clock: guest->host offset {:.0} ns, error bound +-{:.1} us (from {} frames), drift {:.2} ppm",
                k.median_offset(),
                k.error_ns() / 1e3,
                k.samples,
                k.drift_ppm
            );
            match k.global {
                Some((lo, hi)) => println!(
                    "clock: one offset for the whole run would be {:.0} ns +-{:.1} us",
                    (lo + hi) / 2.0,
                    (hi - lo) / 2e3
                ),
                None => println!(
                    "clock: no single offset fits the whole run (drift); per-second windows used"
                ),
            }
        }
        None if grecs.is_empty() => println!("clock: no guest records; host stages only"),
        None => println!("clock: no frame seen on both sides; guest stamps left out"),
    }
    let frames = join(&c.host, &grecs, clock.as_ref());
    let joined = frames.iter().filter(|f| f.has_host && f.has_guest).count();
    let host_only = frames.iter().filter(|f| f.has_host && !f.has_guest).count();
    let guest_only = frames.iter().filter(|f| !f.has_host && f.has_guest).count();
    println!(
        "records: host {} ({} lost), guest {} ({} lost); frames: {joined} joined, {host_only} host only, {guest_only} guest only",
        c.host.len(),
        c.host_lost,
        grecs.len(),
        guest_lost
    );
    for path in [COPY, FLIP] {
        print!("{}", render(&path_report(&frames, path)));
    }
    if let Some(out) = perfetto {
        let etw_events = match (etw, &clock) {
            (Some(p), Some(k)) => Some(import_etw(&std::fs::read_to_string(p)?, k)?),
            (Some(_), None) => {
                info("--etw needs the guest/host clock correlation; ETW events left out");
                None
            }
            _ => None,
        };
        extra.extend(etw_events.unwrap_or_default());
        let v = trace_events(&frames, (!extra.is_empty()).then_some(extra.as_slice()));
        std::fs::write(out, serde_json::to_vec(&v)?)
            .with_context(|| format!("writing {}", out.display()))?;
        info(format!(
            "trace events written to {} (open in ui.perfetto.dev)",
            out.display()
        ));
    } else if etw.is_some() {
        info("--etw is only used with --perfetto");
    }
    Ok(())
}

// ---------------------------------------------------- trace-event / Perfetto

const P_GUEST: u32 = 1;
const P_DXGKRNL: u32 = 2;
const P_BACKEND: u32 = 3;
const P_VENUS: u32 = 4;
const P_GPU: u32 = 5;
const P_IRQ: u32 = 6;

/// The track an interval ending at `to` (from `from`) goes on.
fn track(from: u8, to: u8) -> u32 {
    match (stage::side(from), stage::side(to)) {
        (1, 0) => P_IRQ,
        (_, 0) => P_GUEST,
        (_, 1) => P_BACKEND,
        _ => P_VENUS,
    }
}

/// Chrome trace-event JSON: one slice per interval per frame, flows linking
/// a frame's slices across tracks, and `extra` events (ETW, the latency
/// capture's round trips; `ts` in absolute host µs) as given. Time 0 is the
/// earliest stamp or extra event.
pub fn trace_events(frames: &[Frame], extra: Option<&[Value]>) -> Value {
    let extra_ts = extra
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| e.get("ts").and_then(Value::as_f64))
        .map(|us| us * 1e3);
    let t0 = frames
        .iter()
        .filter_map(|f| f.stamps.first().map(|s| s.1))
        .chain(extra_ts)
        .fold(f64::INFINITY, f64::min);
    let t0 = if t0.is_finite() { t0 } else { 0.0 };
    let has_etw = extra
        .unwrap_or(&[])
        .iter()
        .any(|e| e.get("cat").and_then(Value::as_str) == Some("etw"));
    let us = |ns: f64| (ns - t0) / 1e3;
    let mut ev = Vec::new();
    for (pid, name) in [
        (P_GUEST, "guest KMD"),
        (P_DXGKRNL, "dxgkrnl (ETW)"),
        (P_BACKEND, "host backend"),
        (P_VENUS, "conduit-venus/virglrenderer"),
        (P_GPU, "host GPU"),
        (P_IRQ, "interrupt delivery"),
    ] {
        // Without frames (a latency capture alone) the stage tracks stay out.
        if (pid == P_DXGKRNL && !has_etw) || (pid != P_DXGKRNL && frames.is_empty()) {
            continue;
        }
        ev.push(json!({"ph": "M", "name": "process_name", "pid": pid, "tid": 0, "args": {"name": name}}));
        ev.push(json!({"ph": "M", "name": "process_sort_index", "pid": pid, "tid": 0, "args": {"sort_index": pid}}));
        for path in [COPY, FLIP] {
            ev.push(json!({"ph": "M", "name": "thread_name", "pid": pid, "tid": path, "args": {"name": path_name(path)}}));
        }
    }
    for (flow, f) in frames.iter().enumerate() {
        let (iv, _) = intervals(f);
        let mut slices: Vec<(u32, f64, f64, String, Value)> = iv
            .iter()
            .map(|&(a, b, start, d)| {
                (
                    track(a, b),
                    start,
                    d,
                    interval_name(a, b),
                    json!({"frame": f.id, "path": path_name(f.path)}),
                )
            })
            .collect();
        let side = |st: u8| f.side.iter().find(|x| x.0 == st).map(|x| x.1);
        if let (Some(a), Some(b)) = (side(stage::V_SUBMIT), side(stage::V_SUBMIT_DONE)) {
            slices.push((
                P_VENUS,
                a,
                (b - a).max(0.0),
                "vkQueueSubmit".into(),
                json!({"frame": f.id, "path": path_name(f.path)}),
            ));
            slices.sort_by(|a, b| a.1.total_cmp(&b.1));
        }
        if let (Some(g), Some(end)) = (
            f.gpu_ns,
            f.stamps
                .iter()
                .find(|s| s.0 == stage::V_FENCE_DONE)
                .map(|s| s.1),
        ) {
            slices.push((
                P_GPU,
                end - g as f64,
                g as f64,
                "gpu copy".into(),
                json!({"frame": f.id, "path": path_name(f.path),
                       "placement": "upper bound: ends at the vkr fence done stamp; the duration is measured"}),
            ));
            slices.sort_by(|a, b| a.1.total_cmp(&b.1));
        }
        let last = slices.len().saturating_sub(1);
        for (i, (pid, start, d, name, args)) in slices.into_iter().enumerate() {
            let (ts, tid) = (us(start), f.path);
            ev.push(
                json!({"ph": "X", "name": name, "cat": "stage", "pid": pid, "tid": tid,
                           "ts": ts, "dur": d / 1e3, "args": args}),
            );
            if last > 0 {
                let ph = if i == 0 {
                    "s"
                } else if i == last {
                    "f"
                } else {
                    "t"
                };
                ev.push(
                    json!({"ph": ph, "name": "frame", "cat": "frame", "id": flow, "pid": pid,
                               "tid": tid, "ts": ts, "bp": "e"}),
                );
            }
        }
    }
    if let Some(x) = extra {
        for e in x {
            let mut e = e.clone();
            if let Some(ts) = e.get("ts").and_then(|v| v.as_f64()) {
                e["ts"] = json!(us(ts * 1e3));
            }
            ev.push(e);
        }
    }
    json!({"traceEvents": ev, "displayTimeUnit": "ns"})
}

/// A guest DxgKrnl ETW capture, already converted to CSV with the header
/// `ts_100ns,event,pid,tid[,detail]`, `ts_100ns` the guest's interrupt time
/// (a tracerpt / xperf export has to be converted to that first; raw ETL is
/// not read). Event 41 is paired with the next 42 on the same thread
/// ("BEGINCPUACCESS wait"), 178 with 180 ("Blt packet"); every other event
/// is an instant. Timestamps come out in host µs (absolute; `trace_events`
/// rebases them).
pub fn import_etw(csv: &str, clock: &Clock) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut open: HashMap<(u64, u32), f64> = HashMap::new();
    for (n, line) in csv.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || (n == 0 && line.starts_with("ts")) {
            continue;
        }
        let f: Vec<&str> = line.splitn(5, ',').map(str::trim).collect();
        if f.len() < 4 {
            return Err(oops(
                format!("ETW CSV line {}: expected ts_100ns,event,pid,tid", n + 1),
                "",
            ));
        }
        let parse = |s: &str| {
            s.parse::<u64>()
                .map_err(|_| oops(format!("ETW CSV line {}: {s:?} is not a number", n + 1), ""))
        };
        let (t, event, pid, tid) = (
            parse(f[0])?,
            parse(f[1])? as u32,
            parse(f[2])?,
            parse(f[3])?,
        );
        let detail = f.get(4).copied().unwrap_or("");
        let ts = clock.to_host(t) / 1e3;
        match event {
            41 | 178 => {
                open.insert((tid, event), ts);
            }
            42 | 180 => {
                let start_ev = if event == 42 { 41 } else { 178 };
                if let Some(s) = open.remove(&(tid, start_ev)) {
                    let name = if event == 42 {
                        "BEGINCPUACCESS wait"
                    } else {
                        "Blt packet"
                    };
                    out.push(json!({"ph": "X", "name": name, "cat": "etw", "pid": P_DXGKRNL, "tid": tid,
                                    "ts": s, "dur": ts - s, "args": {"guest_pid": pid, "detail": detail}}));
                }
            }
            _ => out.push(
                json!({"ph": "i", "s": "t", "name": format!("event {event}"), "cat": "etw",
                                 "pid": P_DXGKRNL, "tid": tid, "ts": ts,
                                 "args": {"guest_pid": pid, "detail": detail}}),
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conduit_venus::stage::*;

    /// The guest's record at `t` ns of guest time (rounded to 100 ns).
    fn g(index: u64, t_ns: f64, id: u32, stage: u8, kind: u8) -> guest::GRec {
        guest::GRec {
            index,
            t: (t_ns / 100.0) as u64,
            id,
            stage,
            kind,
            aux: 0,
        }
    }

    fn snapshot(head: u64, slots: usize, recs: &[guest::GRec]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(guest::MAGIC);
        b.extend_from_slice(&head.to_le_bytes());
        b.extend_from_slice(&(slots as u32).to_le_bytes());
        b.extend_from_slice(&24u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        let mut slot = vec![None; slots];
        for r in recs {
            slot[r.index as usize % slots] = Some(*r);
        }
        for s in slot {
            let (seq, t, w) = match s {
                Some(r) => (
                    r.index + 1,
                    r.t,
                    u64::from(r.id)
                        | u64::from(r.stage) << 32
                        | u64::from(r.kind) << 40
                        | u64::from(r.aux) << 48,
                ),
                None => (0, 0, 0),
            };
            b.extend_from_slice(&seq.to_le_bytes());
            b.extend_from_slice(&t.to_le_bytes());
            b.extend_from_slice(&w.to_le_bytes());
        }
        b
    }

    /// A copy frame `id` starting at guest time `tg` (ns), host = guest +
    /// `off(tg)`: kmd present, submit, host decode +20 us, used, guest done.
    fn copy_frame(id: u32, tg: f64, off: f64, idx: &mut u64) -> (Vec<Rec>, Vec<guest::GRec>) {
        let h = |dt: f64| (tg + dt + off) as u64;
        let host = vec![
            Rec::fence(H_KICK, 3, 1, id as u64, h(110_000.0)),
            Rec::fence(H_DECODED, 3, 1, id as u64, h(120_000.0)),
            Rec::fence(V_SUBMIT, 3, 1, id as u64, h(140_000.0)),
            Rec::fence(V_SUBMIT_DONE, 3, 1, id as u64, h(150_000.0)),
            Rec::fence(V_FENCE, 3, 1, id as u64, h(160_000.0)),
            Rec::fence(V_FENCE_DONE, 3, 1, id as u64, h(450_000.0)),
            Rec {
                aux: 200_000,
                ..Rec::fence(V_GPU, 3, 1, id as u64, h(460_000.0))
            },
            Rec::fence(H_USED, 3, 1, id as u64, h(500_000.0)),
            Rec::fence(H_IRQ, 3, 1, id as u64, h(510_000.0)),
        ];
        let mut guest = Vec::new();
        for (dt, s) in [
            (0.0, G_PRESENT),
            (100_000.0, G_SUBMIT),
            (560_000.0, G_ISR),
            (580_000.0, G_DONE),
        ] {
            guest.push(g(*idx, tg + dt, id, s, KIND_FENCE));
            *idx += 1;
        }
        (host, guest)
    }

    #[test]
    fn the_offset_is_recovered_within_its_bound_and_drift_is_followed() {
        let mut idx = 0;
        let (mut host, mut guest) = (Vec::new(), Vec::new());
        let base = 7.0e12;
        let ppm = 20.0;
        for i in 0..2000u32 {
            let tg = 1.0e9 + i as f64 * 4.0e6; // 8 s of frames
            let off = base + tg * ppm * 1e-6;
            let (h, gg) = copy_frame(i, tg, off, &mut idx);
            host.extend(h);
            guest.extend(gg);
        }
        let b = bounds(&host, &guest);
        assert_eq!(b.len(), 4000);
        let k = Clock::from_bounds(&b).unwrap();
        // True offset bounds: decode 20 us after submit, done 80 us after used.
        assert!(k.error_ns() <= 50_000.0 + 100.0, "{}", k.error_ns());
        assert!((k.drift_ppm - ppm).abs() < 1.0, "drift {}", k.drift_ppm);
        for &(t, _, _) in b.iter().step_by(97) {
            let true_off = base + (t - base) * ppm * 1e-6 / (1.0 + ppm * 1e-6);
            let mid = true_off - 30_000.0; // the feasible interval's middle: [-80, +20] us around it
            assert!(
                (k.offset_at(t) - mid).abs() < 2_000.0,
                "at {t}: {} vs {mid}",
                k.offset_at(t)
            );
        }
        assert!(
            k.global.is_none(),
            "8 s at 20 ppm is 160 us, more than the 100 us interval"
        );
    }

    #[test]
    fn one_sided_bounds_give_no_clock() {
        assert!(Clock::from_bounds(&[(0.0, 5.0, true)]).is_none());
        let k = Clock::from_bounds(&[(0.0, 5.0, true), (1.0, 1.0, false)]).unwrap();
        assert_eq!(k.offset_at(0.5), 3.0);
        assert_eq!(k.error_ns(), 2.0);
    }

    #[test]
    fn three_frames_join_and_add_up() {
        let mut idx = 0;
        let (mut host, mut guest) = (Vec::new(), Vec::new());
        let off = 1.0e12;
        for i in 0..3u32 {
            let (h, gg) = copy_frame(100 + i, 1.0e9 + i as f64 * 4.0e6, off, &mut idx);
            host.extend(h);
            guest.extend(gg);
        }
        // A fence on another ring and a context's ring-1 fence the guest never
        // submitted as a copy are not frames.
        host.push(Rec::fence(H_DECODED, 3, 2, 100, 1));
        host.push(Rec::fence(H_DECODED, 9, 1, 555, 1));
        let k = Clock::from_bounds(&bounds(&host, &guest)).unwrap();
        let frames = join(&host, &guest, Some(&k));
        assert_eq!(frames.len(), 3);
        assert!(frames
            .iter()
            .all(|f| f.has_host && f.has_guest && f.gpu_ns == Some(200_000)));
        let order: Vec<u8> = frames[0].stamps.iter().map(|s| s.0).collect();
        assert_eq!(
            order,
            [
                G_PRESENT,
                G_SUBMIT,
                H_KICK,
                H_DECODED,
                V_FENCE,
                V_FENCE_DONE,
                H_USED,
                H_IRQ,
                G_ISR,
                G_DONE
            ]
        );
        let rep = path_report(&frames, KIND_FENCE);
        assert_eq!(rep.frames, 3);
        assert!((rep.interval_us.unwrap() - 4000.0).abs() < 0.5);
        let row = |n: &str| rep.rows.iter().find(|r| r.name == n).unwrap().stats.clone();
        assert!((row("vkr fence -> vkr fence done").mean_us - 290.0).abs() < 0.01);
        assert_eq!(frames[0].side.len(), 2, "vkQueueSubmit kept off the chain");
        assert!(
            (row("vkr submit -> vkr submit done (vkQueueSubmit call)").mean_us - 10.0).abs() < 0.01
        );
        assert!((row("kmd present -> kmd submit").mean_us - 100.0).abs() < 0.01);
        assert!((row("gpu copy (timestamps)").mean_us - 200.0).abs() < 0.01);
        assert!(
            (row("vkr submit done -> fence done, minus gpu (queue/fence wake)").mean_us - 100.0)
                .abs()
                < 0.01
        );
        // The rows that are not breakdowns add up to the total.
        let sum: f64 = rep
            .rows
            .iter()
            .filter(|r| !r.detail)
            .map(|r| r.stats.mean_us)
            .sum();
        let total = rep.total.clone().unwrap();
        assert!(
            (sum - total.mean_us).abs() < 0.01,
            "{sum} vs {}",
            total.mean_us
        );
        assert!((total.mean_us - 580.0).abs() < 50.0, "{}", total.mean_us);
        assert_eq!(total.n, 3);
        let text = render(&rep);
        assert!(
            text.contains("unattributed") && text.contains("% frame"),
            "{text}"
        );
        // Without guest data the host's ring-1 fences are the copies.
        let host_only = join(&host, &[], None);
        assert_eq!(host_only.iter().filter(|f| f.path == KIND_FENCE).count(), 4);
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let mut v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = stats(&mut v).unwrap();
        assert_eq!(
            (s.n, s.p50_us, s.p99_us, s.mean_us),
            (100, 50.0, 99.0, 50.5)
        );
        assert_eq!(stats(&mut [7.0]).unwrap().p99_us, 7.0);
        assert!(stats(&mut []).is_none());
    }

    #[test]
    fn guest_snapshots_are_deduplicated_and_losses_counted() {
        let r = |i: u64| g(i, i as f64 * 1000.0, i as u32, G_PRESENT, KIND_FENCE);
        let base = snapshot(3, 8, &(0..3).map(r).collect::<Vec<_>>());
        let s1 = snapshot(6, 8, &(0..6).map(r).collect::<Vec<_>>());
        let s2 = snapshot(10, 8, &(2..10).map(r).collect::<Vec<_>>()); // overlaps s1
        let s3 = snapshot(30, 8, &(22..30).map(r).collect::<Vec<_>>()); // 10..21 overwritten
        let (recs, lost) = dedupe_guest(&[base, s1, s2, s3]);
        let idx: Vec<u64> = recs.iter().map(|r| r.index).collect();
        assert_eq!(idx, [3, 4, 5, 6, 7, 8, 9, 22, 23, 24, 25, 26, 27, 28, 29]);
        assert_eq!(lost, 12);
    }

    #[test]
    fn trace_events_are_json_with_flows_and_gpu_slices() {
        let mut idx = 0;
        let (host, guest) = copy_frame(1, 1.0e9, 5.0e9, &mut idx);
        let mut host = host;
        host.push(Rec::flip(H_DECODED, 77, 6_000_000_000));
        host.push(Rec::flip(H_USED, 77, 6_000_050_000));
        host.push(Rec::flip(H_IRQ, 77, 6_000_060_000));
        let k = Clock::from_bounds(&bounds(&host, &guest)).unwrap();
        let frames = join(&host, &guest, Some(&k));
        let etw = import_etw(
            "ts_100ns,event,pid,tid\n10000000,41,4,8\n10001000,42,4,8\n10002000,7,4,8\n",
            &k,
        )
        .unwrap();
        assert_eq!(etw.len(), 2);
        assert_eq!(etw[0]["name"], "BEGINCPUACCESS wait");
        assert!((etw[0]["dur"].as_f64().unwrap() - 100.0).abs() < 1e-6);
        let v = trace_events(&frames, Some(&etw));
        let text = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        let ev = back["traceEvents"].as_array().unwrap();
        let count = |ph: &str| ev.iter().filter(|e| e["ph"] == ph).count();
        assert_eq!(count("s"), 2, "one flow start per frame");
        assert_eq!(count("f"), 2);
        assert!(count("t") >= 1);
        assert!(ev
            .iter()
            .any(|e| e["name"] == "gpu copy" && e["pid"] == P_GPU && e["dur"] == 200.0));
        assert!(ev
            .iter()
            .any(|e| e["name"] == "backend irq -> kmd isr" && e["pid"] == P_IRQ));
        assert!(ev.iter().any(|e| e["args"]["name"] == "dxgkrnl (ETW)"));
        assert!(ev
            .iter()
            .filter(|e| e["ph"] == "X")
            .all(|e| e["ts"].as_f64().unwrap() >= -1.0e6));
        assert!(import_etw("1,2\n", &k).is_err());
    }
}
