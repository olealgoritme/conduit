//! The host round-trip table from a capture (docs/TRACING.md "Latency
//! capture"): one row per stage of every fenced `SUBMIT_3D` on the KMD's
//! copy ring, from the queue thread's wakeup to the MSI into the guest, with
//! the threads the fence crosses on its way back and the interrupt rates.
//!
//! The capture is the text bpftrace prints, one event per line:
//! `TAG NSECS TID FIELDS...`, `NSECS` in `CLOCK_MONOTONIC` (the clock of the
//! frame stage stamps). The tags and fields are those of `capture::script`.
//! This is the analysis the former `host/latency/analyze.py` did, kept stage
//! for stage (same names, same pairing rules, the same rows for the same
//! capture), plus the thread hops.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

/// The Venus command type of a 3D submit.
pub const SUBMIT_3D: u64 = 0x0207;
/// The ring of the KMD's windowed-Present copy.
pub const COPY_RING: u64 = 1;

/// Pairing windows (ns), as analyze.py had them.
const WITHIN: u64 = 50_000_000;
const WAKE_WITHIN: u64 = 2_000_000;

/// One probe hit: its time, the thread it hit on, and its numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct Ev {
    pub ts: u64,
    pub tid: i64,
    pub f: Vec<u64>,
}

/// A `sched_wakeup` of a traced thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Wake {
    pub ts: u64,
    pub cpu: i64,
    /// The idle state its CPU was in: 0 awake, else the cpuidle index + 1.
    pub cstate: i64,
    pub waker: i64,
}

/// A parsed capture.
#[derive(Default, Debug)]
pub struct Capture {
    /// By tag, sorted by time.
    pub ev: HashMap<String, Vec<Ev>>,
    /// Wakeups by the thread woken, sorted by time.
    pub wk: HashMap<i64, Vec<Wake>>,
    /// Wakeups by the waker: (time, thread woken), sorted by time. From
    /// `sched_waking` (`WG`, in the waker's context); a capture without it
    /// falls back to `WK`'s waker, which is mostly the idle task (a remote
    /// wakeup completes on the target CPU).
    pub woke: HashMap<i64, Vec<(u64, i64)>>,
    /// When each thread was switched in, sorted.
    pub rn: HashMap<i64, Vec<u64>>,
    /// Thread names (`/proc/PID/task/TID/comm` when the capture ended).
    pub names: HashMap<i64, String>,
    /// The capture has `sched_waking` events (thread hops can be followed).
    pub waking: bool,
    /// Events bpftrace dropped (its "Lost N events" lines).
    pub lost: u64,
}

fn is_int(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}

impl Capture {
    /// Parse the events and the thread list (`TID NAME` per line). Lines
    /// that are not events (bpftrace's own messages) are skipped.
    pub fn parse(events: &str, threads: &str) -> Capture {
        let mut c = Capture::default();
        let mut wk_woke: HashMap<i64, Vec<(u64, i64)>> = HashMap::new();
        for line in events.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.first() == Some(&"Lost") {
                c.lost += p.get(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                continue;
            }
            if p.len() < 3 || !p[1].bytes().all(|b| b.is_ascii_digit()) || !is_int(p[2]) {
                continue;
            }
            let (Ok(ts), Ok(tid)) = (p[1].parse::<u64>(), p[2].parse::<i64>()) else {
                continue;
            };
            match p[0] {
                "WK" => {
                    let n = |i: usize| p.get(i).and_then(|s| s.parse::<i64>().ok());
                    let (Some(cpu), Some(cstate), Some(waker)) = (n(3), n(4), n(5)) else {
                        continue;
                    };
                    c.wk.entry(tid).or_default().push(Wake {
                        ts,
                        cpu,
                        cstate,
                        waker,
                    });
                    if waker != 0 {
                        wk_woke.entry(waker).or_default().push((ts, tid));
                    }
                }
                "WG" => {
                    if let Some(to) = p.get(3).and_then(|s| s.parse::<i64>().ok()) {
                        c.woke.entry(tid).or_default().push((ts, to));
                        c.waking = true;
                    }
                }
                "RN" => c.rn.entry(tid).or_default().push(ts),
                tag => {
                    let f: Option<Vec<u64>> = p[3..].iter().map(|s| s.parse().ok()).collect();
                    if let Some(f) = f {
                        c.ev.entry(tag.to_string())
                            .or_default()
                            .push(Ev { ts, tid, f });
                    }
                }
            }
        }
        // bpftrace prints each CPU's buffer in turn: nearly, not strictly,
        // in time order.
        for v in c.ev.values_mut() {
            v.sort_by_key(|e| e.ts);
        }
        for v in c.wk.values_mut() {
            v.sort_by_key(|w| w.ts);
        }
        if c.woke.is_empty() {
            c.woke = wk_woke;
        }
        for v in c.woke.values_mut() {
            v.sort_by_key(|w| w.0);
        }
        for v in c.rn.values_mut() {
            v.sort_unstable();
        }
        for line in threads.lines() {
            let mut it = line.trim().splitn(2, char::is_whitespace);
            if let (Some(tid), Some(name)) = (it.next(), it.next()) {
                if let Ok(tid) = tid.parse() {
                    c.names.insert(tid, name.trim().to_string());
                }
            }
        }
        c
    }

    fn list(&self, tag: &str) -> &[Ev] {
        self.ev.get(tag).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The first `tag` event at or after `t` (within `within`) that `pred` takes.
    pub fn next(&self, tag: &str, t: u64, within: u64, pred: impl Fn(&Ev) -> bool) -> Option<&Ev> {
        let l = self.list(tag);
        let i = l.partition_point(|e| e.ts < t);
        l[i..]
            .iter()
            .take_while(|e| e.ts - t < within)
            .find(|e| pred(e))
    }

    /// The last `tag` event at or before `t` (within `within`) that `pred` takes.
    pub fn prev(&self, tag: &str, t: u64, within: u64, pred: impl Fn(&Ev) -> bool) -> Option<&Ev> {
        let l = self.list(tag);
        let i = l.partition_point(|e| e.ts <= t);
        l[..i]
            .iter()
            .rev()
            .take_while(|e| t - e.ts < within)
            .find(|e| pred(e))
    }

    /// A thread's name, or its number.
    pub fn name(&self, tid: i64) -> String {
        self.names
            .get(&tid)
            .cloned()
            .unwrap_or_else(|| tid.to_string())
    }

    fn tids_named(&self, n: &str) -> Vec<i64> {
        self.names
            .iter()
            .filter(|(_, v)| v.as_str() == n)
            .map(|(k, _)| *k)
            .collect()
    }

    /// First and last event time (the capture's own START/STOP marks when
    /// it has them).
    pub fn span_ns(&self) -> u64 {
        let mark = |t: &str| self.list(t).first().map(|e| e.ts);
        if let (Some(a), Some(b)) = (mark("START"), mark("STOP")) {
            if b > a {
                return b - a;
            }
        }
        let all = self.ev.values().flatten().map(|e| e.ts);
        let (lo, hi) = all.fold((u64::MAX, 0), |(lo, hi), t| (lo.min(t), hi.max(t)));
        hi.saturating_sub(lo)
    }

    /// Was this a full capture (the probes the light one leaves out)?
    pub fn full(&self) -> bool {
        self.ev.contains_key("D1") || self.ev.contains_key("VS0")
    }
}

/// The last time in `series` at or before `t`, within `within`.
fn before(series: &[u64], t: u64, within: u64) -> Option<u64> {
    let i = series.partition_point(|&x| x <= t);
    (i > 0 && t - series[i - 1] < within).then(|| series[i - 1])
}

/// One host round trip, as found in the capture (for the trace-event file).
#[derive(Clone, Debug, Default)]
pub struct RoundTrip {
    pub fence: u64,
    /// The queue thread and `Venus::dispatch`'s entry and return.
    pub tid: i64,
    pub dispatch: u64,
    pub dispatch_ret: Option<u64>,
    /// `Virgl::submit` in conduit-venus: (thread, entry, return).
    pub venus_submit: Option<(i64, u64, Option<u64>)>,
    /// The copy's `vkQueueSubmit` and the fence's sync submit in
    /// virglrenderer: (thread, entry, return).
    pub copy_submit: Option<(i64, Option<u64>, u64)>,
    pub sync_submit: Option<(i64, Option<u64>, u64)>,
    /// The timeline write (vkr-queue saw the fence) and the fence callback.
    pub timeline: (i64, u64),
    pub fence_cb: (i64, u64),
    /// The interrupt into the guest: (thread, time, INTx rather than MSI).
    pub irq: Option<(i64, u64, bool)>,
    /// The threads from the timeline write to the interrupt, through the
    /// wakeups; empty when the chain could not be followed.
    pub hops: Vec<i64>,
}

/// Everything the table shows.
#[derive(Debug, Default)]
pub struct Analysis {
    pub ring: u64,
    pub full: bool,
    pub span_ns: u64,
    pub dispatches: usize,
    /// Stage name to its samples (µs); names sort into the path's order.
    pub stages: BTreeMap<String, Vec<f64>>,
    pub held: u64,
    pub held_then_irq: u64,
    /// (deliverer's name, idle state at its wakeup) -> count.
    pub cstate: BTreeMap<(String, i64), u64>,
    /// MSIs per thread name over the capture.
    pub msi: BTreeMap<String, u64>,
    pub intx: u64,
    pub nvidia_irqs: u64,
    pub trips: Vec<RoundTrip>,
}

/// The round trips of a capture on `ring`.
pub fn analyze(c: &Capture, ring: u64) -> Analysis {
    let mut a = Analysis {
        ring,
        full: c.full(),
        span_ns: c.span_ns(),
        dispatches: c.list("D0").len(),
        ..Default::default()
    };
    let mut deliverers = c.tids_named("nvgpu-fences");
    deliverers.extend(c.tids_named("venus-ipc"));
    let delivers = |e: &Ev| deliverers.is_empty() || deliverers.contains(&e.tid);
    let wk_ts: HashMap<i64, Vec<u64>> =
        c.wk.iter()
            .map(|(t, v)| (*t, v.iter().map(|w| w.ts).collect()))
            .collect();
    let no_ts: Vec<u64> = Vec::new();
    let wk_of = |tid: i64| wk_ts.get(&tid).unwrap_or(&no_ts);
    let rn_of = |tid: i64| c.rn.get(&tid).map(Vec::as_slice).unwrap_or(&[]);
    let mut stages: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut add = |name: &str, a: Option<u64>, b: Option<u64>| {
        if let (Some(a), Some(b)) = (a, b) {
            if b >= a {
                stages
                    .entry(name.to_string())
                    .or_default()
                    .push((b - a) as f64 / 1000.0);
            }
        }
    };
    let ts = |e: Option<&Ev>| e.map(|e| e.ts);

    for d0 in c.list("D0") {
        let (t0, tid) = (d0.ts, d0.tid);
        let [ty, flags, fence, _ctx, ring_idx, ..] = d0.f[..] else {
            continue;
        };
        let on_ring = if flags & 2 != 0 { ring_idx } else { 0 };
        if ty != SUBMIT_3D || flags & 1 == 0 || on_ring != ring {
            continue;
        }
        let Some(wf) = c.next("WF", t0, WITHIN, |e| e.f.get(2) == Some(&fence)) else {
            continue;
        };
        let Some(t) = c.prev("T", wf.ts, WITHIN, |e| e.f.first() == Some(&ring)) else {
            continue;
        };
        if t.ts < t0 {
            continue;
        }
        // The chain's interrupt: the first MSI after the fence callback from
        // a thread that delivers completions.
        let mut irq = c.next("MSI", wf.ts, WITHIN, delivers).map(|e| (e, false));
        // INTx instead of MSI-X: the backend's signal reaches QEMU's main
        // loop, which raises the line.
        if irq.is_none() {
            let u0 = c.next("U0", wf.ts, WITHIN, delivers);
            if let Some(l) = c.next("IRQ", u0.map_or(wf.ts, |u| u.ts), WITHIN, |_| true) {
                if let Some(u) = u0 {
                    add(
                        "16b signal_used_queue -> QEMU raises INTx",
                        Some(u.ts),
                        Some(l.ts),
                    );
                }
                irq = Some((l, true));
            }
        }
        let kick = before(wk_of(tid), t0, WAKE_WITHIN);
        let run = before(rn_of(tid), t0, WAKE_WITHIN);
        add("01 queue thread woken -> Venus::dispatch", kick, Some(t0));
        add("02 queue thread running -> Venus::dispatch", run, Some(t0));
        let same = |e: &Ev| e.tid == tid;
        let d1 = c.next("D1", t0, WITHIN, same);
        let s0 = c.next("S0", t0, WITHIN, same);
        let f1 = c.next("F1", t0, WITHIN, same);
        let vs0 = c.next("VS0", t0, WITHIN, |_| true);
        let vs1 = vs0.and_then(|v| c.next("VS1", v.ts, WITHIN, |e| e.tid == v.tid));
        let vf0 = c.next("VF0", t0, WITHIN, |e| e.f.get(2) == Some(&fence));
        let q1 = vs0.and_then(|v| c.next("Q1", v.ts, WITHIN, |_| true));
        let y1 = vf0.and_then(|v| c.next("Y1", v.ts, WITHIN, |_| true));
        add("03 dispatch -> renderer call sent", Some(t0), ts(s0));
        add(
            "04 call sent -> conduit-venus Virgl::submit",
            ts(s0),
            ts(vs0),
        );
        add("05 Virgl::submit", ts(vs0), ts(vs1));
        add("06 Virgl::submit -> Virgl::create_fence", ts(vs1), ts(vf0));
        add(
            "07 dispatch -> backend has the fence reply",
            Some(t0),
            ts(f1),
        );
        add(
            "08 dispatch -> chain held (queue thread free)",
            Some(t0),
            ts(d1),
        );
        add(
            "09 dispatch -> copy's vkQueueSubmit returned",
            Some(t0),
            ts(q1),
        );
        add(
            "10 dispatch -> fence's sync submit returned",
            Some(t0),
            ts(y1),
        );
        if let Some(q1) = q1 {
            let qrun = before(rn_of(t.tid), t.ts, 20_000_000);
            let ni = c.prev("NI", qrun.unwrap_or(t.ts), 5_000_000, |_| true);
            add(
                "11 copy submitted -> vkr-queue running (GPU + wake)",
                Some(q1.ts),
                qrun,
            );
            if let Some(ni) = ni.filter(|n| n.ts >= q1.ts) {
                add(
                    "12 copy submitted -> NVIDIA interrupt (GPU)",
                    Some(q1.ts),
                    Some(ni.ts),
                );
                add(
                    "13 NVIDIA interrupt -> vkr-queue running",
                    Some(ni.ts),
                    qrun,
                );
            }
        }
        add("14 dispatch -> timeline written", Some(t0), Some(t.ts));
        add(
            "15 timeline -> write_context_fence",
            Some(t.ts),
            Some(wf.ts),
        );
        let mut hops = Vec::new();
        if let Some((m, _)) = irq {
            add("16 write_context_fence -> MSI", Some(wf.ts), Some(m.ts));
            add(
                "17 timeline -> MSI (fence return path)",
                Some(t.ts),
                Some(m.ts),
            );
            add("18 dispatch -> MSI (host round trip)", Some(t0), Some(m.ts));
            let w = wk_of(m.tid);
            if let Some(wt) = before(w, m.ts, WAKE_WITHIN).filter(|&x| x >= wf.ts) {
                let i = w.partition_point(|&x| x < wt);
                *a.cstate
                    .entry((c.name(m.tid), c.wk[&m.tid][i].cstate))
                    .or_default() += 1;
            }
            hops = return_hops(c, t, wf, m);
        }
        // An interrupt from the queue thread right after holding the chain:
        // the empty one `quiet-held` removes.
        if let Some(d1) = d1 {
            let m = c.next("MSI", d1.ts, 30_000, same);
            let d_next = c.next("D0", d1.ts + 1, 30_000, same);
            a.held += 1;
            if let Some(m) = m {
                if d_next.is_none_or(|d| m.ts < d.ts) {
                    a.held_then_irq += 1;
                }
            }
        }
        let call = |e0: Option<&Ev>, tag0: &str, e1: Option<&Ev>| {
            e1.map(|e1| {
                let start = c
                    .prev(tag0, e1.ts, WITHIN, |e| e.tid == e1.tid)
                    .map(|e| e.ts)
                    .filter(|&s| e0.is_none_or(|e0| s >= e0.ts));
                (e1.tid, start, e1.ts)
            })
        };
        a.trips.push(RoundTrip {
            fence,
            tid,
            dispatch: t0,
            dispatch_ret: ts(d1),
            venus_submit: vs0.map(|v| (v.tid, v.ts, ts(vs1))),
            copy_submit: call(vs0, "Q0", q1),
            sync_submit: call(vf0, "Y0", y1),
            timeline: (t.tid, t.ts),
            fence_cb: (wf.tid, wf.ts),
            irq: irq.map(|(m, intx)| (m.tid, m.ts, intx)),
            hops,
        });
    }
    a.stages = stages;
    for e in c.list("MSI") {
        *a.msi.entry(c.name(e.tid)).or_default() += 1;
    }
    a.intx = c.list("IRQ").len() as u64;
    a.nvidia_irqs = c.list("NI").len() as u64;
    a
}

/// The threads the fence crosses from the timeline write to the interrupt:
/// the timeline writer, the fence callback's thread, then each thread the
/// last one woke first (`sched_waking`), until the interrupting thread.
/// Empty when the wakeups do not lead there within six hops.
fn return_hops(c: &Capture, t: &Ev, wf: &Ev, irq: &Ev) -> Vec<i64> {
    let mut chain = vec![t.tid];
    if wf.tid != t.tid {
        chain.push(wf.tid);
    }
    let (mut cur, mut now) = (wf.tid, wf.ts);
    for _ in 0..6 {
        if cur == irq.tid {
            return chain;
        }
        let Some(w) = c.woke.get(&cur) else { break };
        let i = w.partition_point(|x| x.0 < now);
        let Some(&(ts, to)) = w[i..].iter().find(|x| x.1 != cur) else {
            break;
        };
        if ts > irq.ts {
            break;
        }
        chain.push(to);
        (cur, now) = (to, ts);
    }
    if cur == irq.tid {
        chain
    } else {
        Vec::new()
    }
}

/// A nearest-rank percentile, as analyze.py had it: `v[min(n-1, p*n)]`.
pub fn pct(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    sorted[((p * n as f64) as usize).min(n - 1)]
}

const IDLE: [&str; 5] = ["awake", "POLL", "C1", "C2", "C3"];

fn idle_name(i: i64) -> String {
    usize::try_from(i)
        .ok()
        .and_then(|i| IDLE.get(i))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("state {}", i - 1))
}

/// The report text.
pub fn render(a: &Analysis, c: &Capture) -> String {
    let mut s = String::new();
    let secs = a.span_ns as f64 / 1e9;
    let _ = writeln!(
        s,
        "{} host round trips (fenced SUBMIT_3D on ring {}) of {} dispatches in {}, {} capture",
        a.trips.len(),
        a.ring,
        a.dispatches,
        if secs >= 1.0 {
            format!("{secs:.1} s")
        } else {
            format!("{:.1} ms", secs * 1e3)
        },
        if a.full { "full" } else { "light" }
    );
    if c.lost > 0 {
        let _ = writeln!(
            s,
            "bpftrace dropped {} events: rows may miss samples (capture shorter, or light)",
            c.lost
        );
    }
    if a.trips.is_empty() {
        let _ = writeln!(
            s,
            "no round trip found: is the guest presenting windowed frames (the KMD's ring-{} copy)?",
            a.ring
        );
    } else {
        let _ = writeln!(
            s,
            "{:55} {:>5} {:>7} {:>7} {:>7} {:>7}",
            "stage (us)", "n", "mean", "p50", "p90", "p99"
        );
        for (k, v) in &a.stages {
            let mut v = v.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let _ = writeln!(
                s,
                "{k:55} {:5} {mean:7.1} {:7.1} {:7.1} {:7.1}",
                v.len(),
                pct(&v, 0.5),
                pct(&v, 0.9),
                pct(&v, 0.99)
            );
        }
    }
    if a.held > 0 {
        let _ = writeln!(
            s,
            "held chains: {}, followed by an interrupt from the queue thread: {}",
            a.held, a.held_then_irq
        );
    }
    if !a.cstate.is_empty() {
        let mut parts = Vec::new();
        for ((who, st), n) in &a.cstate {
            parts.push(format!("{who} {} {n}", idle_name(*st)));
        }
        let _ = writeln!(
            s,
            "idle state of the deliverer's CPU at its wakeup: {}",
            parts.join(", ")
        );
    }
    let mut chains: BTreeMap<Vec<String>, u64> = BTreeMap::new();
    let mut unresolved = 0;
    for t in a.trips.iter().filter(|t| t.irq.is_some()) {
        if t.hops.is_empty() {
            unresolved += 1;
        } else {
            *chains
                .entry(t.hops.iter().map(|&h| c.name(h)).collect())
                .or_default() += 1;
        }
    }
    let total = chains.values().sum::<u64>() + unresolved;
    if total > 0 && !c.waking && chains.is_empty() {
        let _ = writeln!(
            s,
            "thread hops: not in this capture (it has no sched_waking events; `conduit trace NAME latency` records them)"
        );
    } else if total > 0 {
        let _ = writeln!(s, "thread hops, timeline written -> interrupt:");
        let mut v: Vec<_> = chains.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (chain, n) in v.iter().take(4) {
            let _ = writeln!(
                s,
                "  {:3.0}% {n:6}  {} hop(s): {}",
                100.0 * *n as f64 / total as f64,
                chain.len() - 1,
                chain.join(" -> ")
            );
        }
        if v.len() > 4 {
            let rest: u64 = v[4..].iter().map(|x| x.1).sum();
            let _ = writeln!(
                s,
                "  {:3.0}% {rest:6}  other chains",
                100.0 * rest as f64 / total as f64
            );
        }
        if unresolved > 0 {
            let _ = writeln!(
                s,
                "  {:3.0}% {unresolved:6}  not followed through the wakeups",
                100.0 * unresolved as f64 / total as f64
            );
        }
    }
    if secs > 0.0 {
        let rate = |n: u64| (n as f64 / secs).round() as u64;
        let mut by: Vec<_> = a.msi.iter().collect();
        by.sort_by(|x, y| y.1.cmp(x.1).then(x.0.cmp(y.0)));
        let per: Vec<String> = by
            .iter()
            .map(|(k, n)| format!("{k} {}", rate(**n)))
            .collect();
        let msi_total: u64 = a.msi.values().sum();
        let mut line = format!("interrupts per second: MSI {}", rate(msi_total));
        if !per.is_empty() {
            let _ = write!(line, " ({})", per.join(", "));
        }
        if a.intx > 0 {
            let _ = write!(line, ", INTx {}", rate(a.intx));
        }
        let _ = write!(line, ", NVIDIA {}", rate(a.nvidia_irqs));
        let _ = writeln!(s, "{line}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows of two real captures (Heaven on a Windows 11 guest, MSI-X,
    /// before the latency options): a light one with three dispatches, of
    /// which two complete in the window, and a full one with two. The
    /// expected numbers are what host/latency/analyze.py printed for them.
    const LIGHT: &str = include_str!("fixtures/light.txt");
    const LIGHT_THREADS: &str = include_str!("fixtures/light.threads");
    const FULL: &str = include_str!("fixtures/full.txt");
    const FULL_THREADS: &str = include_str!("fixtures/full.threads");

    fn stage<'a>(a: &'a Analysis, prefix: &str) -> &'a [f64] {
        a.stages
            .iter()
            .find(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.as_slice())
            .unwrap_or_else(|| panic!("no stage {prefix}: {:?}", a.stages.keys()))
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn parsing_skips_what_is_not_an_event_and_sorts() {
        let c = Capture::parse(
            "Attaching 12 probes...\nT 300 7 1 5\nT 100 7 1 4\nWK 90 7 3 0 11 CPU 2/KVM\n@cst[3]: 1\nMSI x 1 2\nRN 95 7 3\nLost 12 events\n",
            "7 vkr-queue\n11 CPU 2/KVM\n",
        );
        let t = &c.ev["T"];
        assert_eq!(t.iter().map(|e| e.ts).collect::<Vec<_>>(), [100, 300]);
        assert_eq!(t[0].f, [1, 4]);
        assert!(!c.ev.contains_key("MSI"));
        assert_eq!(c.wk[&7][0].waker, 11);
        // No sched_waking: the wakeup's own waker stands in.
        assert!(!c.waking);
        assert_eq!(c.woke[&11], [(90, 7)]);
        assert_eq!(c.rn[&7], [95]);
        assert_eq!(c.name(11), "CPU 2/KVM");
        assert_eq!(c.name(99), "99");
        assert_eq!(c.span_ns(), 200);
        assert_eq!(c.lost, 12);
    }

    #[test]
    fn a_light_capture_gives_what_analyze_py_gave() {
        let c = Capture::parse(LIGHT, LIGHT_THREADS);
        let a = analyze(&c, COPY_RING);
        assert!(!a.full);
        assert_eq!(a.dispatches, 3);
        assert_eq!(
            a.trips.iter().map(|t| t.fence).collect::<Vec<_>>(),
            [767, 768]
        );
        assert!(close(stage(&a, "14 "), &[409.729, 404.68]));
        assert!(close(stage(&a, "15 "), &[15.248, 14.196]));
        assert!(!a.stages.keys().any(|k| k.starts_with("05 ")));
        assert_eq!(a.cstate[&("nvgpu-fences".to_string(), 0)], 2);
        assert_eq!(a.msi["nvgpu-events"], 150);
        assert_eq!(a.msi["vring_worker"], 28);
        assert_eq!(a.msi["nvgpu-fences"], 2);
        assert_eq!(a.nvidia_irqs, 31);
        let text = render(&a, &c);
        for line in [
            "2 host round trips (fenced SUBMIT_3D on ring 1) of 3 dispatches in 13.9 ms, light capture",
            "stage (us)                                                  n    mean     p50     p90     p99",
            "01 queue thread woken -> Venus::dispatch                    2     7.3     8.3     8.3     8.3",
            "16 write_context_fence -> MSI                               2    43.4    47.2    47.2    47.2",
            "18 dispatch -> MSI (host round trip)                        2   465.3   472.2   472.2   472.2",
            "idle state of the deliverer's CPU at its wakeup: nvgpu-fences awake 2",
            "thread hops: not in this capture",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
    }

    #[test]
    fn a_full_capture_adds_the_call_rows_and_the_held_chains() {
        let c = Capture::parse(FULL, FULL_THREADS);
        let a = analyze(&c, COPY_RING);
        assert!(a.full);
        assert_eq!(
            a.trips.iter().map(|t| t.fence).collect::<Vec<_>>(),
            [3019, 3020]
        );
        let rows: Vec<&str> = a.stages.keys().map(|k| &k[..2]).collect();
        assert_eq!(
            rows,
            [
                "01", "02", "03", "04", "05", "06", "07", "08", "09", "10", "11", "12", "13", "14",
                "15", "16", "17", "18"
            ]
        );
        assert_eq!((a.held, a.held_then_irq), (2, 2));
        let t = &a.trips[0];
        assert!(t.venus_submit.is_some() && t.copy_submit.is_some() && t.sync_submit.is_some());
        assert_eq!(t.timeline, (2397385, 271984765510045));
        assert_eq!(t.fence_cb, (2397374, 271984765520324));
        let text = render(&a, &c);
        for line in [
            "04 call sent -> conduit-venus Virgl::submit                 2    34.0    36.3    36.3    36.3",
            "11 copy submitted -> vkr-queue running (GPU + wake)         2   341.9   343.7   343.7   343.7",
            "12 copy submitted -> NVIDIA interrupt (GPU)                 2   326.6   327.6   327.6   327.6",
            "18 dispatch -> MSI (host round trip)                        2   509.6   512.7   512.7   512.7",
            "held chains: 2, followed by an interrupt from the queue thread: 2",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
    }

    #[test]
    fn thread_hops_follow_sched_waking() {
        let ev = "\
D0 1000000 100 519 3 9 0 1
T 1050000 300 1 9
WF 1060000 301 0 1 9
WG 1061000 301 302
WG 1063000 302 302
WG 1065000 302 303
WG 1070000 303 400
MSI 1075000 400 1
D0 2000000 100 519 3 10 0 1
T 2050000 300 1 10
WF 2060000 301 0 1 10
WG 2061000 301 555
MSI 2075000 400 1
";
        let th =
            "300 vkr-queue\n301 vkr-sync\n302 conduit-venus\n303 venus-ipc\n400 nvgpu-fences\n";
        let c = Capture::parse(ev, th);
        assert!(c.waking);
        let a = analyze(&c, COPY_RING);
        assert_eq!(a.trips[0].hops, [300, 301, 302, 303, 400]);
        assert!(a.trips[1].hops.is_empty());
        let text = render(&a, &c);
        assert!(text.contains(" 50%      1  4 hop(s): vkr-queue -> vkr-sync -> conduit-venus -> venus-ipc -> nvgpu-fences"), "{text}");
        assert!(
            text.contains(" 50%      1  not followed through the wakeups"),
            "{text}"
        );
        assert!(text.contains("interrupts per second: MSI "), "{text}");
    }

    #[test]
    fn intx_is_the_interrupt_when_there_is_no_msi() {
        let ev = "D0 1000000 100 519 3 9 0 1\nT 1050000 300 1 9\nWF 1060000 301 0 1 9\nU0 1062000 400\nIRQ 1070000 500 24\n";
        let c = Capture::parse(ev, "400 nvgpu-fences\n");
        let a = analyze(&c, COPY_RING);
        assert_eq!(a.trips.len(), 1);
        assert_eq!(stage(&a, "16b"), [8.0]);
        assert_eq!(stage(&a, "18 "), [70.0]);
        assert_eq!(a.trips[0].irq, Some((500, 1_070_000, true)));
    }

    #[test]
    fn percentiles_are_nearest_rank_as_before() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(pct(&v, 0.5), 6.0);
        assert_eq!(pct(&v, 0.9), 10.0);
        assert_eq!(pct(&v, 0.99), 10.0);
        assert_eq!(pct(&[3.0], 0.99), 3.0);
    }
}
