//! The host's GPU readings as one JSON line, sent once a second over the VM's
//! `org.conduit.stats.0` virtio-serial channel (`conduit _stats`) and read by
//! the Windows tray app. Every field but `v` may be missing; a reader ignores
//! fields it does not know.

use serde::{Deserialize, Serialize};

/// The virtio-serial port name; the guest opens `\\.\Global\org.conduit.stats.0`.
pub const CHANNEL: &str = "org.conduit.stats.0";

/// The format version this crate writes and reads.
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reading {
    pub v: u32,
    /// Unix time, seconds.
    pub ts: u64,
    /// The host machine's name.
    pub host: String,
    pub gpu: String,
    pub driver: String,
    pub temp_c: Option<u32>,
    pub util_gpu: Option<u32>,
    pub util_mem: Option<u32>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub clk_gfx: Option<u32>,
    pub clk_gfx_max: Option<u32>,
    pub clk_mem: Option<u32>,
    pub clk_mem_max: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub fan_pct: Option<u32>,
    pub pstate: Option<u32>,
    pub pcie_tx_kbs: Option<u32>,
    pub pcie_rx_kbs: Option<u32>,
    pub enc_util: Option<u32>,
}

impl Reading {
    /// One line, newline included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("a Reading always serializes");
        s.push('\n');
        s
    }

    /// Parse one line. Garbage, or a version this crate does not know, is `None`.
    pub fn parse(line: &str) -> Option<Reading> {
        let r: Reading = serde_json::from_str(line.trim()).ok()?;
        (r.v >= 1 && r.v <= VERSION).then_some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Reading {
        Reading {
            v: VERSION,
            ts: 1_700_000_000,
            host: "box".into(),
            gpu: "NVIDIA GeForce RTX 5090".into(),
            driver: "580.95".into(),
            temp_c: Some(51),
            util_gpu: Some(37),
            vram_used: Some(4 << 30),
            vram_total: Some(32 << 30),
            power_mw: Some(120_500),
            ..Reading::default()
        }
    }

    #[test]
    fn round_trip() {
        let r = sample();
        let line = r.to_line();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        assert_eq!(Reading::parse(&line), Some(r));
    }

    #[test]
    fn missing_and_unknown_fields() {
        let r = Reading::parse(r#"{"v":1,"gpu":"X","temp_c":40,"future":[1,2]}"#).unwrap();
        assert_eq!(r.temp_c, Some(40));
        assert_eq!(r.util_gpu, None);
    }

    #[test]
    fn rejects_garbage_and_foreign_versions() {
        assert_eq!(Reading::parse(""), None);
        assert_eq!(Reading::parse("not json"), None);
        assert_eq!(Reading::parse(r#"{"gpu":"X"}"#), None);
        assert_eq!(Reading::parse(r#"{"v":99}"#), None);
    }
}
