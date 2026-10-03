//! One line per request, for `conduit trace --follow`.
//!
//! ```text
//!     0.004213 h=3   control  NV2080_CTRL_CMD_GPU_GET_INFO_V2 (0x20800102)   32/32     ok        17.3µs  [q 1.2µs host 14.7µs +1.4µs]
//! ```
//! The time column is seconds since the first record shown. With colour on,
//! the latency is green under 100µs, yellow under 1ms and red above, and a
//! failure is red.

use crate::summary::fmt_ns;
use crate::{DriverVersion, Kind, Record, Refusal, names};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

/// The colour a latency is shown in.
pub fn latency_color(ns: u64) -> &'static str {
    match ns {
        0..100_000 => GREEN,
        100_000..1_000_000 => YELLOW,
        _ => RED,
    }
}

fn outcome(r: &Record) -> String {
    if !matches!(r.refusal, Refusal::None) {
        return format!("refused:{}", r.refusal);
    }
    if r.errno != 0 {
        return format!("errno {}", r.errno);
    }
    match r.nv_status {
        Some(0) | None => "ok".to_string(),
        Some(s) => format!("nv {s:#x}"),
    }
}

/// Render one record. `t0` is the timestamp the time column counts from.
pub fn line(r: &Record, t0: u64, driver: Option<DriverVersion>, color: bool) -> String {
    let (c, dim, reset) = if color {
        (latency_color(r.total_ns()), DIM, RESET)
    } else {
        ("", "", "")
    };
    let t = r.ts_ns.saturating_sub(t0) as f64 / 1e9;
    if r.kind == Kind::Dropped {
        let red = if color { RED } else { "" };
        return format!(
            "{t:>12.6} {red}-- {} records dropped: the trace reader fell behind --{reset}",
            r.sub.unwrap_or(0)
        );
    }
    let n = names(r, driver);
    let what = match (n.sub, r.sub, &n.op) {
        (Some(name), Some(s), _) => format!("{name} ({s:#x})"),
        (None, Some(s), Some(op)) => format!("{op} {s:#x}"),
        (None, Some(s), None) => format!("{:#x} {s:#x}", r.nr),
        (_, None, Some(op)) => op.clone(),
        (_, None, None) => format!("{:#x}", r.nr),
    };
    let out = outcome(r);
    let out = if color && r.failed() {
        format!("{RED}{out:<18}{RESET}")
    } else {
        format!("{out:<18}")
    };
    let split = match (r.queue_ns(), r.host_time_ns(), r.after_host_ns()) {
        (Some(q), Some(h), Some(a)) => format!(
            "  {dim}[q {} host {} +{}{}]{reset}",
            fmt_ns(q),
            fmt_ns(h),
            fmt_ns(a),
            if r.host_calls > 1 {
                format!(" x{}", r.host_calls)
            } else {
                String::new()
            }
        ),
        _ => String::new(),
    };
    format!(
        "{t:>12.6} h={:<4} {:<9} {what:<56} {:>6}/{:<6} {out} {c}{:>9}{reset}{split}",
        r.handle,
        r.call.as_str(),
        r.size_in,
        r.size_out,
        fmt_ns(r.total_ns()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;

    #[test]
    fn a_line_says_what_how_and_how_long() {
        let l = line(&sample(), sample().ts_ns, None, false);
        assert!(l.contains("control"), "{l}");
        assert!(
            l.contains("NV2080_CTRL_CMD_GPU_GET_INFO_V2 (0x20800102)"),
            "{l}"
        );
        assert!(l.contains(" ok "), "{l}");
        assert!(l.contains("17.3µs"), "{l}");
        assert!(l.contains("host 14.7µs"), "{l}");
        assert!(!l.contains('\x1b'), "no colour asked for: {l}");
    }

    #[test]
    fn colour_follows_latency() {
        assert_eq!(latency_color(50_000), GREEN);
        assert_eq!(latency_color(500_000), YELLOW);
        assert_eq!(latency_color(5_000_000), RED);
        let slow = Record {
            reply_ns: 2_000_000,
            ..sample()
        };
        assert!(line(&slow, 0, None, true).contains(RED));
    }

    #[test]
    fn refusals_and_drops_are_visible() {
        let r = Record {
            refusal: Refusal::Allowlist,
            nv_status: Some(0x56),
            ..sample()
        };
        assert!(line(&r, 0, None, false).contains("refused:allowlist"));
        assert!(line(&Record::dropped(0, 12), 0, None, false).contains("12 records dropped"));
    }
}
