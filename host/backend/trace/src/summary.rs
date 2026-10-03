//! `--summary` and `conduit trace analyze`: counts, latency percentiles, the
//! slowest RM controls and what failed.

use crate::{Call, DriverVersion, Kind, Record, Refusal, names};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

/// A latency, short: `850ns`, `12.3µs`, `4.56ms`, `1.20s`.
pub fn fmt_ns(ns: u64) -> String {
    match ns {
        0..1_000 => format!("{ns}ns"),
        1_000..1_000_000 => format!("{:.1}µs", ns as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.2}ms", ns as f64 / 1e6),
        _ => format!("{:.2}s", ns as f64 / 1e9),
    }
}

/// The value at a percentile of sorted samples (nearest rank).
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Latency distribution of one group of requests.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub count: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub sum: u64,
    /// Median time inside the host driver, for requests that reached it.
    pub host_p50: Option<u64>,
    pub errors: u64,
}

#[derive(Default)]
struct Group {
    total: Vec<u64>,
    host: Vec<u64>,
    errors: u64,
}

impl Group {
    fn add(&mut self, r: &Record) {
        self.total.push(r.total_ns());
        if let Some(h) = r.host_time_ns() {
            self.host.push(h);
        }
        if r.failed() {
            self.errors += 1;
        }
    }

    fn stats(&self) -> Stats {
        let mut t = self.total.clone();
        t.sort_unstable();
        let mut h = self.host.clone();
        h.sort_unstable();
        Stats {
            count: t.len() as u64,
            p50: percentile(&t, 50.0),
            p95: percentile(&t, 95.0),
            p99: percentile(&t, 99.0),
            max: t.last().copied().unwrap_or(0),
            sum: t.iter().sum(),
            host_p50: (!h.is_empty()).then(|| percentile(&h, 50.0)),
            errors: self.errors,
        }
    }
}

/// Everything a summary is built from. Feed it records with [`add`].
///
/// [`add`]: Summary::add
#[derive(Default)]
pub struct Summary {
    driver: Option<DriverVersion>,
    records: u64,
    dropped: u64,
    first_ts: Option<u64>,
    last_ts: u64,
    calls: BTreeMap<Call, Group>,
    controls: HashMap<u32, Group>,
    errors: HashMap<(Call, String, String), u64>,
}

/// What failed, as one line of the error table: what was asked, and how it
/// came back.
fn error_key(r: &Record, driver: Option<DriverVersion>) -> (Call, String, String) {
    let n = names(r, driver);
    let what = match (n.sub, r.sub, n.op) {
        (Some(name), Some(s), _) => format!("{name} ({s:#x})"),
        (None, Some(s), Some(op)) => format!("{op} {s:#x}"),
        (None, Some(s), None) => format!("{s:#x}"),
        (_, None, Some(op)) => op,
        (_, None, None) => format!("{:#x}", r.nr),
    };
    let how = if !matches!(r.refusal, Refusal::None | Refusal::Local) {
        format!("refused: {}", r.refusal)
    } else if r.errno != 0 {
        format!("errno {}", r.errno)
    } else {
        format!("nv_status {:#x}", r.nv_status.unwrap_or(0))
    };
    (r.call, what, how)
}

impl Summary {
    pub fn new(driver: Option<DriverVersion>) -> Self {
        Summary {
            driver,
            ..Default::default()
        }
    }

    pub fn set_driver(&mut self, driver: Option<DriverVersion>) {
        self.driver = driver;
    }

    pub fn add(&mut self, r: &Record) {
        if r.kind == Kind::Dropped {
            self.dropped += r.sub.unwrap_or(0) as u64;
            return;
        }
        self.records += 1;
        self.first_ts.get_or_insert(r.ts_ns);
        self.last_ts = self.last_ts.max(r.ts_ns);
        self.calls.entry(r.call).or_default().add(r);
        if r.call == Call::Control
            && let Some(cmd) = r.sub
        {
            self.controls.entry(cmd).or_default().add(r);
        }
        if r.failed() {
            *self.errors.entry(error_key(r, self.driver)).or_insert(0) += 1;
        }
    }

    pub fn records(&self) -> u64 {
        self.records
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Per call kind, in a stable order.
    pub fn by_call(&self) -> Vec<(Call, Stats)> {
        self.calls.iter().map(|(c, g)| (*c, g.stats())).collect()
    }

    /// The `n` RM controls with the highest worst-case latency.
    pub fn slowest_controls(&self, n: usize) -> Vec<(u32, Stats)> {
        let mut v: Vec<(u32, Stats)> = self.controls.iter().map(|(c, g)| (*c, g.stats())).collect();
        v.sort_by(|a, b| b.1.max.cmp(&a.1.max).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Failures grouped by what was asked and how it failed, most frequent
    /// first.
    pub fn errors(&self) -> Vec<((Call, String, String), u64)> {
        let mut v: Vec<_> = self.errors.iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// The report `--summary` and `analyze` print.
    pub fn render(&self) -> String {
        let mut o = String::new();
        let span = self
            .first_ts
            .map(|f| self.last_ts.saturating_sub(f))
            .unwrap_or(0);
        let _ = write!(o, "{} requests", self.records);
        if span > 0 {
            let _ = write!(
                o,
                " over {} ({:.0}/s)",
                fmt_ns(span),
                self.records as f64 / (span as f64 / 1e9)
            );
        }
        let _ = write!(o, ", {} dropped", self.dropped);
        if let Some(v) = self.driver {
            let _ = write!(o, "; host driver {v}");
        }
        o.push('\n');
        if self.records == 0 {
            return o;
        }

        let _ = writeln!(
            o,
            "\n{:<10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>7}",
            "call", "count", "p50", "p95", "p99", "max", "host p50", "errors"
        );
        for (call, s) in self.by_call() {
            let _ = writeln!(
                o,
                "{:<10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>7}",
                call.as_str(),
                s.count,
                fmt_ns(s.p50),
                fmt_ns(s.p95),
                fmt_ns(s.p99),
                fmt_ns(s.max),
                s.host_p50.map(fmt_ns).unwrap_or_else(|| "-".into()),
                s.errors
            );
        }

        let slow = self.slowest_controls(20);
        if !slow.is_empty() {
            let _ = writeln!(o, "\nslowest RM controls (by max latency)");
            let _ = writeln!(
                o,
                "{:<10} {:<48} {:>7} {:>9} {:>9} {:>9} {:>9}",
                "cmd", "name", "count", "p50", "p99", "max", "total"
            );
            for (cmd, s) in slow {
                let name = abi::names::control(cmd).unwrap_or("?");
                let _ = writeln!(
                    o,
                    "{:<10} {:<48} {:>7} {:>9} {:>9} {:>9} {:>9}",
                    format!("{cmd:#010x}"),
                    name,
                    s.count,
                    fmt_ns(s.p50),
                    fmt_ns(s.p99),
                    fmt_ns(s.max),
                    fmt_ns(s.sum)
                );
            }
        }

        let errors = self.errors();
        if !errors.is_empty() {
            let total: u64 = errors.iter().map(|e| e.1).sum();
            let _ = writeln!(o, "\nerrors ({total})");
            for ((call, what, how), n) in errors.iter().take(30) {
                let _ = writeln!(o, "{n:>8}  {:<8} {what:<60} {how}", call.as_str());
            }
            if errors.len() > 30 {
                let _ = writeln!(o, "     ...  {} more kinds", errors.len() - 30);
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;

    #[test]
    fn percentiles_use_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 95.0), 95);
        assert_eq!(percentile(&v, 99.0), 99);
        assert_eq!(percentile(&v, 100.0), 100);
        assert_eq!(percentile(&[7], 99.0), 7);
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn durations_print_short() {
        assert_eq!(fmt_ns(850), "850ns");
        assert_eq!(fmt_ns(12_345), "12.3µs");
        assert_eq!(fmt_ns(4_560_000), "4.56ms");
        assert_eq!(fmt_ns(1_200_000_000), "1.20s");
    }

    fn workload() -> Summary {
        let mut s = Summary::new(None);
        // 100 controls of one command at 1..=100 µs, one slow outlier of
        // another, two allocs (one refused), and a dropped marker.
        for i in 1..=100u64 {
            s.add(&Record {
                ts_ns: i * 1_000_000,
                reply_ns: i * 1_000,
                host_ns: Some((100, i * 1_000 - 100)),
                ..sample()
            });
        }
        s.add(&Record {
            ts_ns: 200_000_000,
            sub: Some(0x0000_0102),
            reply_ns: 5_000_000,
            nv_status: Some(0x56),
            ..sample()
        });
        s.add(&Record {
            call: Call::Alloc,
            sub: Some(0xc86f),
            reply_ns: 2_000,
            ..sample()
        });
        s.add(&Record {
            call: Call::Alloc,
            sub: Some(0xc7b7),
            reply_ns: 900,
            host_ns: None,
            refusal: Refusal::Caps,
            ..sample()
        });
        s.add(&Record::dropped(0, 3));
        s
    }

    #[test]
    fn per_call_counts_and_percentiles() {
        let s = workload();
        assert_eq!(s.records(), 103);
        assert_eq!(s.dropped(), 3);
        let calls = s.by_call();
        let control = &calls.iter().find(|c| c.0 == Call::Control).unwrap().1;
        assert_eq!(control.count, 101);
        assert_eq!(control.p50, 51_000);
        assert_eq!(control.max, 5_000_000);
        assert_eq!(control.errors, 1);
        let alloc = &calls.iter().find(|c| c.0 == Call::Alloc).unwrap().1;
        assert_eq!((alloc.count, alloc.errors), (2, 1));
    }

    #[test]
    fn the_slowest_control_comes_first() {
        let s = workload();
        let slow = s.slowest_controls(20);
        assert_eq!(slow.len(), 2);
        assert_eq!(slow[0].0, 0x0000_0102);
        assert_eq!(slow[1].0, 0x2080_0102);
        assert_eq!(slow[1].1.count, 100);
        assert_eq!(s.slowest_controls(1).len(), 1);
    }

    #[test]
    fn errors_say_what_and_how() {
        let s = workload();
        let e = s.errors();
        assert_eq!(e.len(), 2);
        let text: Vec<String> = e.iter().map(|((_, w, h), _)| format!("{w} {h}")).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("0x102") && t.contains("nv_status 0x56")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("0xc7b7") && t.contains("refused: caps")),
            "{text:?}"
        );
    }

    #[test]
    fn the_report_has_every_section() {
        let out = workload().render();
        assert!(out.starts_with("103 requests"), "{out}");
        assert!(out.contains("3 dropped"), "{out}");
        assert!(out.contains("slowest RM controls"), "{out}");
        assert!(out.contains("NV2080_CTRL_CMD_GPU_GET_INFO_V2"), "{out}");
        assert!(out.contains("errors (2)"), "{out}");
        assert_eq!(Summary::new(None).render(), "0 requests, 0 dropped\n");
    }
}
