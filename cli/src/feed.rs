//! `conduit _stats NAME` (conduit-stats@NAME.service): once a second, send the
//! host GPU's readings as one JSON line ([`conduit_stats::Reading`]) to the
//! VM's `org.conduit.stats.0` virtio-serial channel. QEMU listens on the
//! channel's unix socket while the VM runs; this connects, and connects again
//! whenever the socket goes away (VM stopped, restarted). It touches nothing
//! else of the VM, so a failure here cannot disturb the VM's other helpers.

use crate::tui::nvml::{Nvml, Sample};
use crate::units;
use crate::virt;
use anyhow::Result;
use conduit_stats::{Reading, VERSION};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The wire form of one NVML sample.
pub fn reading(s: &Sample, driver: &str, host: &str, ts: u64) -> Reading {
    Reading {
        v: VERSION,
        ts,
        host: host.into(),
        gpu: s.name.clone(),
        driver: driver.into(),
        temp_c: s.temp_c,
        util_gpu: s.util_gpu,
        util_mem: s.util_mem,
        vram_used: s.vram_used,
        vram_total: s.vram_total,
        clk_gfx: s.clk_gfx,
        clk_gfx_max: s.clk_gfx_max,
        clk_mem: s.clk_mem,
        clk_mem_max: s.clk_mem_max,
        power_mw: s.power_mw,
        power_limit_mw: s.power_limit_mw,
        fan_pct: s.fan_pct,
        pstate: s.pstate,
        pcie_tx_kbs: s.pcie_tx_kbs,
        pcie_rx_kbs: s.pcie_rx_kbs,
        enc_util: s.enc_util,
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn connect(path: &Path) -> Option<UnixStream> {
    let s = UnixStream::connect(path).ok()?;
    // QEMU stops reading while the guest has the port closed: never block on it.
    s.set_write_timeout(Some(Duration::from_millis(500))).ok()?;
    Some(s)
}

/// Run until stopped.
pub fn run(name: &str) -> Result<()> {
    let uri = crate::lvrun::link_uri(name)?;
    let scope = virt::scope_for(&uri)?;
    let path = units::stats_path(&scope, name);
    let host = hostname();
    let mut nvml: Option<Nvml> = None;
    let mut conn: Option<UnixStream> = None;
    eprintln!("conduit: feeding GPU readings to {}", path.display());
    loop {
        if conn.is_none() {
            conn = connect(&path);
            if conn.is_none() {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        }
        if nvml.is_none() {
            nvml = Nvml::open();
        }
        if let (Some(n), Some(c)) = (nvml.as_ref(), conn.as_mut()) {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let line = reading(&n.sample(), &n.driver, &host, ts).to_line();
            if c.write_all(line.as_bytes()).is_err() {
                conn = None;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    fn reading_carries_the_sample() {
        let s = Sample {
            name: "GPU".into(),
            temp_c: Some(60),
            util_gpu: Some(5),
            ..Sample::default()
        };
        let r = reading(&s, "1.2", "h", 7);
        assert_eq!((r.gpu.as_str(), r.driver.as_str(), r.ts), ("GPU", "1.2", 7));
        assert_eq!(
            (r.temp_c, r.util_gpu, r.vram_used),
            (Some(60), Some(5), None)
        );
    }

    /// With a GPU: `cargo test -p conduit live_line -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_line() {
        let n = Nvml::open().expect("NVML");
        print!(
            "{}",
            reading(&n.sample(), &n.driver, &hostname(), 0).to_line()
        );
    }

    #[test]
    fn connects_and_sends_a_parsable_line() {
        let dir = std::env::temp_dir().join(format!("conduit-feed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let mut c = connect(&sock).unwrap();
        let line = reading(&Sample::default(), "d", "h", 1).to_line();
        c.write_all(line.as_bytes()).unwrap();
        let (srv, _) = l.accept().unwrap();
        let mut got = String::new();
        BufReader::new(srv).read_line(&mut got).unwrap();
        assert!(Reading::parse(&got).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
