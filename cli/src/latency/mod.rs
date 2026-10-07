//! `conduit trace NAME latency` and `conduit trace latency DIR`: where a
//! Windows guest's GPU copy spends its time on the host, from the queue
//! kick to the MSI back into the guest (docs/TRACING.md "Latency capture",
//! docs/research/host-roundtrip-latency.md).
//!
//! A read-only bpftrace capture on the VM's backend, conduit-venus and QEMU
//! (`capture`), analysed here (`analyze`) into the per-stage table, the
//! thread hops and the interrupt rates; with `--perfetto` it is joined with
//! the frame stages of the same window (`stages`) in one trace-event file.

pub mod analyze;
pub mod capture;
pub mod perfetto;

use crate::stages::{self, Collected};
use crate::ui::{info, oops, warn};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A capture writes about 80,000 lines a second under load (full).
pub const MAX_SECS: u64 = 30;
pub const DEFAULT_SECS: u64 = 5;

const EVENTS: &str = "latency.txt";
const THREADS: &str = "latency.threads";
const SCRIPT: &str = "latency.bt";

/// What `conduit trace NAME latency` was asked for.
pub struct Opts {
    pub duration: u64,
    pub full: bool,
    pub save: Option<PathBuf>,
    pub perfetto: Option<PathBuf>,
    /// For the frame stages collected alongside (`--perfetto`).
    pub guest_cmd: Option<String>,
}

/// Removes the scratch folder of a capture that is not kept.
struct Scratch(Option<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(d) = &self.0 {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// `conduit trace NAME latency`.
pub fn live(name: &str, o: Opts) -> Result<()> {
    if o.duration == 0 || o.duration > MAX_SECS {
        return Err(oops(
            format!("--duration {} is out of range", o.duration),
            format!("Keep a latency capture between 1 and {MAX_SECS} s: a full one writes about 80,000 lines a second."),
        ));
    }
    let bt = capture::bpftrace().ok_or_else(|| {
        oops(
            "bpftrace is not installed",
            "Install it (Ubuntu/Debian: sudo apt install bpftrace; Fedora: sudo dnf install bpftrace; Arch: sudo pacman -S bpftrace).",
        )
    })?;
    if !Path::new("/sys/kernel/btf/vmlinux").exists() {
        return Err(oops(
            "this kernel has no BTF (/sys/kernel/btf/vmlinux)",
            "The scheduler probes read kernel structures through BTF; use a kernel built with CONFIG_DEBUG_INFO_BTF (every major distribution's is).",
        ));
    }
    let p = capture::privilege()
        .ok_or_else(|| oops("cannot run bpftrace as root", capture::PRIV_HINT))?;
    let pids = capture::find_pids(name)?;
    if pids.qemu.is_none() {
        info("QEMU's pid is not known (built-in runner?): vCPU wakeups and INTx are left out");
    }
    let (script, missing) = capture::prepare(pids.clone(), o.duration, o.full, p)?;
    if !missing.is_empty() {
        warn(format!(
            "left out, not in the binaries: {} (stripped or older builds; their rows stay empty)",
            missing.join(", ")
        ));
    }

    let (dir, _scratch) = match &o.save {
        Some(d) => (d.clone(), Scratch(None)),
        None => {
            let d =
                std::env::temp_dir().join(format!("conduit-latency-{name}-{}", std::process::id()));
            (d.clone(), Scratch(Some(d)))
        }
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let script_file = dir.join(SCRIPT);
    std::fs::write(&script_file, &script)?;

    // The frame stages of the same window, for one trace-event file.
    let done = Arc::new(AtomicBool::new(false));
    let stage_thread = o.perfetto.as_ref().map(|_| {
        let (name, cmd, done) = (name.to_string(), o.guest_cmd.clone(), done.clone());
        std::thread::spawn(move || {
            stages::collect(&name, cmd.as_deref(), &|| done.load(Ordering::Relaxed))
        })
    });

    info(format!(
        "capturing {name}'s host round trips for {} s ({} capture; read-only probes, attaching takes a few seconds)",
        o.duration,
        if o.full { "full" } else { "light" }
    ));
    let r = capture::run(&script_file, &dir.join(EVENTS), p, &bt);
    done.store(true, Ordering::Relaxed);
    let stage_c = match stage_thread.map(|h| h.join()) {
        Some(Ok(Ok(c))) => Some(c),
        Some(Ok(Err(e))) => {
            warn(format!("frame stages not collected: {e}"));
            None
        }
        Some(Err(_)) => None,
        None => None,
    };
    r?;
    let threads = capture::threads(&[pids.backend, pids.venus]);
    std::fs::write(dir.join(THREADS), &threads)?;
    if let (Some(d), Some(c)) = (&o.save, &stage_c) {
        stages::save(d, c)?;
    }
    if let Some(d) = &o.save {
        info(format!(
            "saved to {} (reanalyse with `conduit trace latency {}`)",
            d.display(),
            d.display()
        ));
    }
    let events = std::fs::read_to_string(dir.join(EVENTS))?;
    report(&events, &threads, stage_c.as_ref(), o.perfetto.as_deref())
}

/// `conduit trace latency DIR`: a capture saved with `--save` (or the OUT
/// file of the former host/latency/capture.sh, with OUT.threads next to it).
pub fn offline(path: &Path, perfetto: Option<&Path>) -> Result<()> {
    let (events, threads, stage_c) = if path.is_dir() {
        let c = stages::load(path)?;
        let c = (!c.host.is_empty() || !c.guest.is_empty()).then_some(c);
        (path.join(EVENTS), path.join(THREADS), c)
    } else {
        let mut t = path.as_os_str().to_owned();
        t.push(".threads");
        (path.to_path_buf(), PathBuf::from(t), None)
    };
    let ev = std::fs::read_to_string(&events).map_err(|e| {
        oops(
            format!("cannot read {} ({e})", events.display()),
            "Give the folder of `conduit trace NAME latency --save DIR`.",
        )
    })?;
    let th = std::fs::read_to_string(&threads).unwrap_or_default();
    report(&ev, &th, stage_c.as_ref(), perfetto)
}

fn report(
    events: &str,
    threads: &str,
    stage_c: Option<&Collected>,
    perfetto_out: Option<&Path>,
) -> Result<()> {
    let c = analyze::Capture::parse(events, threads);
    let a = analyze::analyze(&c, analyze::COPY_RING);
    print!("{}", analyze::render(&a, &c));
    let Some(out) = perfetto_out else {
        return Ok(());
    };
    let ev = perfetto::events(&c, &a);
    match stage_c {
        Some(sc) => {
            let ids: HashSet<u32> = sc
                .host
                .iter()
                .filter(|r| r.kind == conduit_venus::stage::KIND_FENCE)
                .map(|r| r.id as u32)
                .collect();
            let joined = a
                .trips
                .iter()
                .filter(|t| ids.contains(&(t.fence as u32)))
                .count();
            println!(
                "\nframe stages, same window ({joined} of {} round trips have stage stamps under the same fence):",
                a.trips.len()
            );
            stages::report(sc, Some(out), None, ev)
        }
        None => {
            let v = stages::trace_events(&[], Some(&ev));
            std::fs::write(out, serde_json::to_vec(&v)?)
                .with_context(|| format!("writing {}", out.display()))?;
            info(format!(
                "trace events written to {} (open in ui.perfetto.dev)",
                out.display()
            ));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_reads_a_saved_folder_and_a_capture_sh_file() {
        let dir = std::env::temp_dir().join(format!("conduit-latency-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(EVENTS), include_str!("fixtures/light.txt")).unwrap();
        std::fs::write(dir.join(THREADS), include_str!("fixtures/light.threads")).unwrap();
        let out = dir.join("t.json");
        offline(&dir, Some(&out)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let ev = v["traceEvents"].as_array().unwrap();
        assert!(ev.iter().any(|e| e["ph"] == "b"));
        // Without frames there are no stage tracks.
        assert!(!ev.iter().any(|e| e["args"]["name"] == "guest KMD"));
        offline(&dir.join(EVENTS), None).unwrap();
        assert!(offline(&dir.join("nope"), None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_saved_stages_one_file_holds_both() {
        use conduit_venus::stage::{self, Rec};
        let dir = std::env::temp_dir().join(format!("conduit-latency-join-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(EVENTS), include_str!("fixtures/light.txt")).unwrap();
        std::fs::write(dir.join(THREADS), include_str!("fixtures/light.threads")).unwrap();
        // Host stages of the first copy (fence 767), on the capture's clock.
        let t0 = 271_973_420_186_550u64;
        let c = Collected {
            host: vec![
                Rec::fence(stage::H_KICK, 1, 1, 767, t0 - 6_000),
                Rec::fence(stage::H_DECODED, 1, 1, 767, t0 + 1_000),
                Rec::fence(stage::H_USED, 1, 1, 767, t0 + 470_000),
                Rec::fence(stage::H_IRQ, 1, 1, 767, t0 + 472_000),
            ],
            ..Default::default()
        };
        stages::save(&dir, &c).unwrap();
        let out = dir.join("both.json");
        offline(&dir, Some(&out)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let ev = v["traceEvents"].as_array().unwrap();
        assert!(ev.iter().any(|e| e["args"]["name"] == "host backend"));
        assert!(ev
            .iter()
            .any(|e| e["pid"] == perfetto::P_THREADS && e["args"]["frame"] == 767));
        assert!(ev
            .iter()
            .any(|e| e["cat"] == "stage" && e["args"]["frame"] == 767));
        // Time 0 is the earliest event of either kind: the backend kick.
        let min = ev
            .iter()
            .filter_map(|e| e["ts"].as_f64())
            .fold(f64::INFINITY, f64::min);
        assert!(min.abs() < 1e-6, "{min}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
