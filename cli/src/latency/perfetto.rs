//! The round trips as Chrome trace-event JSON, for the same file as the
//! frame stages (`stages::trace_events` takes them as extra events, in host
//! µs; both clocks are `CLOCK_MONOTONIC`). Each traced thread is a track of
//! its own; a round trip's calls and marks carry its fence as `frame`, the
//! id the stage tracks use for the same copy.

use super::analyze::{Analysis, Capture};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const P_THREADS: u32 = 7;
pub const P_TRIPS: u32 = 8;
/// The track of the NVIDIA interrupts in `P_THREADS`.
const TID_NVIDIA: i64 = 0;

fn us(ns: u64) -> f64 {
    ns as f64 / 1e3
}

/// The events of one capture.
pub fn events(c: &Capture, a: &Analysis) -> Vec<Value> {
    let mut ev = Vec::new();
    let mut tids = BTreeSet::new();
    let mut used_irqs = BTreeSet::new();
    for (pid, name) in [
        (P_THREADS, "host threads (bpftrace)"),
        (P_TRIPS, "host round trips (bpftrace)"),
    ] {
        ev.push(json!({"ph": "M", "name": "process_name", "pid": pid, "tid": 0, "args": {"name": name}}));
        ev.push(json!({"ph": "M", "name": "process_sort_index", "pid": pid, "tid": 0, "args": {"sort_index": pid}}));
    }
    ev.push(json!({"ph": "M", "name": "thread_name", "pid": P_TRIPS, "tid": 1, "args": {"name": "dispatch -> interrupt"}}));
    for t in &a.trips {
        let frame = t.fence as u32;
        let args = json!({"frame": frame, "fence": t.fence});
        let mut slices: Vec<(i64, u64, u64, &str)> = Vec::new();
        slices.push((
            t.tid,
            t.dispatch,
            t.dispatch_ret.unwrap_or(t.dispatch),
            "Venus::dispatch",
        ));
        if let Some((tid, a, b)) = t.venus_submit {
            slices.push((tid, a, b.unwrap_or(a), "Virgl::submit"));
        }
        if let Some((tid, a, b)) = t.copy_submit {
            slices.push((tid, a.unwrap_or(b), b, "vkQueueSubmit (copy)"));
        }
        if let Some((tid, a, b)) = t.sync_submit {
            slices.push((tid, a.unwrap_or(b), b, "sync submit (fence)"));
        }
        slices.push((t.timeline.0, t.timeline.1, t.timeline.1, "timeline written"));
        slices.push((
            t.fence_cb.0,
            t.fence_cb.1,
            t.fence_cb.1,
            "write_context_fence",
        ));
        if let Some((tid, ts, intx)) = t.irq {
            slices.push((tid, ts, ts, if intx { "INTx" } else { "MSI" }));
            used_irqs.insert(ts);
        }
        slices.sort_by_key(|s| s.1);
        let end = t.irq.map(|i| i.1).unwrap_or(t.fence_cb.1);
        let id = format!("rt{}", t.fence);
        ev.push(
            json!({"ph": "b", "cat": "roundtrip", "name": format!("round trip, frame {frame}"),
                       "id": id, "pid": P_TRIPS, "tid": 1, "ts": us(t.dispatch), "args": args}),
        );
        ev.push(
            json!({"ph": "e", "cat": "roundtrip", "name": format!("round trip, frame {frame}"),
                       "id": id, "pid": P_TRIPS, "tid": 1, "ts": us(end)}),
        );
        let last = slices.len() - 1;
        for (i, (tid, a, b, name)) in slices.into_iter().enumerate() {
            tids.insert(tid);
            ev.push(
                json!({"ph": "X", "cat": "roundtrip", "name": name, "pid": P_THREADS, "tid": tid,
                           "ts": us(a), "dur": us(b.saturating_sub(a)), "args": args}),
            );
            let ph = match i {
                0 => "s",
                i if i == last => "f",
                _ => "t",
            };
            ev.push(
                json!({"ph": ph, "cat": "roundtrip", "name": "round trip", "id": id,
                           "pid": P_THREADS, "tid": tid, "ts": us(a), "bp": "e"}),
            );
        }
    }
    // The other interrupts into the guest, and the GPU's.
    for e in c.ev.get("MSI").into_iter().flatten() {
        if !used_irqs.contains(&e.ts) {
            tids.insert(e.tid);
            ev.push(
                json!({"ph": "i", "s": "t", "cat": "irq", "name": "MSI", "pid": P_THREADS,
                           "tid": e.tid, "ts": us(e.ts), "args": {"address": e.f.first()}}),
            );
        }
    }
    let nvidia = c.ev.get("NI").map(Vec::as_slice).unwrap_or(&[]);
    for e in nvidia {
        ev.push(
            json!({"ph": "i", "s": "t", "cat": "irq", "name": "NVIDIA interrupt", "pid": P_THREADS,
                       "tid": TID_NVIDIA, "ts": us(e.ts), "args": {"cpu": e.f.first()}}),
        );
    }
    if !nvidia.is_empty() {
        ev.push(
            json!({"ph": "M", "name": "thread_name", "pid": P_THREADS, "tid": TID_NVIDIA,
                       "args": {"name": "NVIDIA interrupts"}}),
        );
    }
    for tid in tids {
        ev.push(
            json!({"ph": "M", "name": "thread_name", "pid": P_THREADS, "tid": tid,
                       "args": {"name": format!("{} ({tid})", c.name(tid))}}),
        );
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::analyze::{analyze, COPY_RING};

    #[test]
    fn a_round_trip_is_slices_on_its_threads_with_flows() {
        let c = Capture::parse(
            include_str!("fixtures/full.txt"),
            include_str!("fixtures/full.threads"),
        );
        let a = analyze(&c, COPY_RING);
        let ev = events(&c, &a);
        let n = a.trips.len();
        assert!(n > 0);
        let count = |ph: &str| ev.iter().filter(|e| e["ph"] == ph).count();
        assert_eq!(count("b"), n);
        assert_eq!(count("e"), n);
        assert_eq!(count("s"), n);
        assert_eq!(count("f"), n);
        let t = &a.trips[0];
        let frame = t.fence as u32;
        let mine: Vec<&Value> = ev
            .iter()
            .filter(|e| e["ph"] == "X" && e["args"]["frame"] == frame)
            .collect();
        for name in [
            "Venus::dispatch",
            "Virgl::submit",
            "vkQueueSubmit (copy)",
            "timeline written",
            "write_context_fence",
            "MSI",
        ] {
            assert!(mine.iter().any(|e| e["name"] == name), "{name} missing");
        }
        let d = mine
            .iter()
            .find(|e| e["name"] == "Venus::dispatch")
            .unwrap();
        assert_eq!(d["ts"].as_f64().unwrap(), t.dispatch as f64 / 1e3);
        assert!(ev.iter().any(|e| e["ph"] == "M"
            && e["args"]["name"]
                .as_str()
                .is_some_and(|s| s.starts_with("vring_worker ("))));
    }
}
