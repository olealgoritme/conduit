//! `conduit config`: user settings in ~/.config/conduit/config.json.
//!
//! | key | values | default |
//! |---|---|---|
//! | `view.close_stops_vm` | true, false | true: closing the window of a VM that `conduit view` started shuts it down. A VM started any other way (`conduit up`, virt-manager, virsh) always keeps running. |
//! | `venus.guest_blobs` | true, false | false: the backend serves guest-memory blobs (docs/VENUS.md "Guest-memory blobs"), Venus copy destinations over the guest's own pages, for the Windows KMD's windowed Present. Opt-in while new. Applies when a VM's backend next starts. |
//! | `backend.latency` | off, all, or a comma-separated list of quiet-held, fused-submit, direct-fences, event-batch, fence-spin | unset: the backend's and conduit-venus's defaults (`all`: the first four); a list is exactly those options (`all,fence-spin` adds the polling fence wait). Round-trip latency options, docs/research/host-roundtrip-latency.md. Applies when a VM's backend next starts. |
//! | `backend.cpus` | a CPU list such as 0-7,16-23 | unset: the backend and conduit-venus run on any CPU; set: every thread of both stays on these (docs/HOST-TUNING.md). Applies when a VM's backend next starts. |
//! | `gpu.window_mib` | auto, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144 | auto: the host GPU's BAR1 (as Resizable BAR on bare metal), clamped to what the guest's 64-bit MMIO window holds, 4096 without a GPU. The shared window every guest CPU mapping of GPU memory goes through, in MiB. Address space, not memory. Applies when a VM's backend next starts. |

use crate::paths;
use crate::ui::oops;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Known keys, their allowed values and what they do.
const KEYS: &[(&str, &[&str], &str)] = &[
    (
        "view.close_stops_vm",
        &["true", "false"],
        "closing the window of a VM that `conduit view` started shuts it down (default true)",
    ),
    (
        "venus.guest_blobs",
        &["true", "false"],
        "serve guest-memory blobs to Venus guests (the Windows KMD's windowed Present writes guest pages directly), from the next backend start (default false)",
    ),
    (
        "backend.latency",
        &["off", "all"],
        "round-trip latency options for the backend and conduit-venus, from the next backend start (default all: quiet-held, fused-submit, direct-fences, event-batch; a comma-separated list sets exactly those, fence-spin included only when named)",
    ),
    (
        "backend.cpus",
        &[],
        "keep the backend and conduit-venus on these host CPUs, from the next backend start (unset: any CPU; a list such as 0-7,16-23)",
    ),
    (
        "gpu.vram_limit_mib",
        &["auto", "off"],
        "MiB of video memory a guest may hold, or off, from the next backend start (unset: a cap when safe mode is on and the NVIDIA card drives a monitor; auto: that cap whatever the driver; off: none; also any number of MiB)",
    ),
    (
        "gpu.safe_mode",
        &["auto", "true", "false"],
        "safe mode (CONDUIT_SAFE_MODE=1): a 2 GiB video-memory cap and 1 s blocking timeouts, from the next backend start (default auto: on for a driver Conduit is untested with, off for open modules 580 or newer)",
    ),
    (
        "gpu.window_mib",
        &[
            "auto", "1024", "2048", "4096", "8192", "16384", "32768", "65536", "131072", "262144",
        ],
        "MiB of shared window for guest CPU mappings of GPU memory, from the next backend start (default auto: the host GPU's BAR1)",
    ),
];

fn file() -> PathBuf {
    paths::config_dir().join("config.json")
}

fn load() -> Map<String, Value> {
    std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save(m: &Map<String, Value>) -> Result<()> {
    let f = file();
    std::fs::create_dir_all(f.parent().unwrap())?;
    let tmp = f.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(m)? + "\n")?;
    std::fs::rename(&tmp, &f).with_context(|| format!("saving {}", f.display()))
}

fn key(k: &str) -> Result<&'static (&'static str, &'static [&'static str], &'static str)> {
    KEYS.iter().find(|(n, _, _)| *n == k).ok_or_else(|| {
        oops(
            format!("there is no setting called \"{k}\""),
            format!(
                "Settings: {}",
                KEYS.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")
            ),
        )
    })
}

pub fn set(k: &str, v: &str) -> Result<()> {
    let (name, values, _) = key(k)?;
    let number = *name == "gpu.vram_limit_mib" && v.parse::<u64>().is_ok_and(|n| n >= 1);
    let list = match *name {
        "backend.latency" => latency_list(v).is_some(),
        "backend.cpus" => cpu_list(v),
        _ => false,
    };
    if !values.contains(&v) && !number && !list {
        return Err(oops(
            format!("\"{v}\" is not a value for {name}"),
            format!(
                "Use one of: {}{}",
                values.join(", "),
                match *name {
                    "gpu.vram_limit_mib" => ", or a number of MiB",
                    "backend.latency" => ", or a comma-separated list of quiet-held, fused-submit, direct-fences, event-batch, fence-spin",
                    "backend.cpus" => "a CPU list such as 0-7,16-23",
                    _ => "",
                }
            ),
        ));
    }
    let mut m = load();
    let val = match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        s => Value::String(s.into()),
    };
    m.insert(name.to_string(), val);
    save(&m)?;
    println!("{name} = {v}");
    Ok(())
}

pub fn get(k: Option<&str>) -> Result<()> {
    let m = load();
    let show = |name: &str, desc: &str| {
        let v = m
            .get(name)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(default)".into());
        println!("{name} = {v}    # {desc}");
    };
    match k {
        Some(k) => {
            let (name, _, desc) = key(k)?;
            show(name, desc);
        }
        None => KEYS.iter().for_each(|(n, _, d)| show(n, d)),
    }
    Ok(())
}

pub fn unset(k: &str) -> Result<()> {
    let (name, _, _) = key(k)?;
    let mut m = load();
    m.remove(*name);
    save(&m)?;
    println!("{name} is back to its default");
    Ok(())
}

/// `view.close_stops_vm` (default true).
pub fn close_stops_vm() -> bool {
    load()
        .get("view.close_stops_vm")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// `venus.guest_blobs` (default false): the backend gets `--venus-guest-blobs`.
pub fn venus_guest_blobs() -> bool {
    load()
        .get("venus.guest_blobs")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The backend's `--latency` options a `backend.latency` value names, in a
/// fixed order; `None` for a value that is not one.
fn latency_list(v: &str) -> Option<Vec<&'static str>> {
    const ALL: [&str; 4] = ["quiet-held", "fused-submit", "direct-fences", "event-batch"];
    // Named only: it spends CPU time polling.
    const EXTRA: [&str; 1] = ["fence-spin"];
    match v {
        "off" => return Some(Vec::new()),
        "all" => return Some(ALL.to_vec()),
        _ => {}
    }
    let names: Vec<&str> = v
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty()
        || names
            .iter()
            .any(|n| *n != "all" && !ALL.contains(n) && !EXTRA.contains(n))
    {
        return None;
    }
    let every = names.contains(&"all");
    Some(
        ALL.into_iter()
            .filter(|a| every || names.contains(a))
            .chain(EXTRA.into_iter().filter(|a| names.contains(a)))
            .collect(),
    )
}

/// A CPU list in the kernel's format (`0-7,16-23`).
fn cpu_list(v: &str) -> bool {
    !v.is_empty()
        && v.split(',').all(|part| {
            let mut ends = part.splitn(2, '-').map(|n| n.trim().parse::<u32>().ok());
            match (ends.next().flatten(), ends.next()) {
                (Some(_), None) => true,
                (Some(a), Some(Some(b))) => a <= b && b < 1024,
                _ => false,
            }
        })
}

/// `backend.latency`: `None` when unset (or not a value), and the backend
/// and conduit-venus keep their defaults; otherwise exactly the options
/// named (empty for off).
pub fn backend_latency() -> Option<Vec<&'static str>> {
    latency_of(&load())
}

fn latency_of(m: &Map<String, Value>) -> Option<Vec<&'static str>> {
    m.get("backend.latency")
        .and_then(Value::as_str)
        .and_then(latency_list)
}

/// `backend.cpus`: the CPU list the backend and conduit-venus get as `--cpus`.
pub fn backend_cpus() -> Option<String> {
    load()
        .get("backend.cpus")
        .and_then(Value::as_str)
        .filter(|v| cpu_list(v))
        .map(str::to_owned)
}

/// `gpu.vram_limit_mib`: how the backend's video-memory cap is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VramSetting {
    /// Unset: the display default while safe mode is on and the card drives
    /// a monitor, nothing otherwise. Safe mode's own 2 GiB applies on top
    /// (`protect::final_limit_mib` is the one rule).
    Default,
    /// The default cap whenever the card drives a monitor.
    Auto,
    /// No cap of the owner's; safe mode's 2 GiB, if safe mode is on, stays.
    Off,
    /// A number of MiB; safe mode's 2 GiB, if on, still bounds it.
    Mib(u64),
}

pub fn vram_limit_mib() -> VramSetting {
    vram_setting_of(&load())
}

fn vram_setting_of(m: &Map<String, Value>) -> VramSetting {
    match m.get("gpu.vram_limit_mib").and_then(Value::as_str) {
        Some("off") => VramSetting::Off,
        Some("auto") => VramSetting::Auto,
        Some(s) => s
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= 1)
            .map_or(VramSetting::Default, VramSetting::Mib),
        None => VramSetting::Default,
    }
}

/// `gpu.safe_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeSetting {
    /// On for a driver Conduit is untested with (protect.rs).
    Auto,
    On,
    Off,
}

pub fn safe_setting() -> SafeSetting {
    safe_setting_of(&load())
}

fn safe_setting_of(m: &Map<String, Value>) -> SafeSetting {
    match m.get("gpu.safe_mode") {
        Some(Value::Bool(true)) => SafeSetting::On,
        Some(Value::Bool(false)) => SafeSetting::Off,
        _ => SafeSetting::Auto,
    }
}

/// `gpu.window_mib` as a number; `None` for `auto` (unset, or set to auto).
/// The backend gets it as `--window-mib` and conduit-vmm as
/// `gpu-forward.window-mib`, which must agree, so `auto` is resolved once per
/// start, by the backend itself (`run::window_mib`).
pub fn window_mib() -> Option<u64> {
    window_mib_of(&load())
}

fn window_mib_of(m: &Map<String, Value>) -> Option<u64> {
    let v = m.get("gpu.window_mib")?.as_str()?;
    let (_, values, _) = key("gpu.window_mib").ok()?;
    values.contains(&v).then(|| v.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_and_values_are_refused() {
        assert!(key("view.close_stops_vm").is_ok());
        assert!(key("view.nope").is_err());
        assert!(key("gpu.window_mib").is_ok());
    }

    #[test]
    fn latency_and_cpu_settings() {
        let mut m = Map::new();
        assert_eq!(latency_of(&m), None);
        for (v, want) in [
            ("off", vec![]),
            (
                "all",
                vec!["quiet-held", "fused-submit", "direct-fences", "event-batch"],
            ),
            ("event-batch,quiet-held", vec!["quiet-held", "event-batch"]),
            (
                "all,fence-spin",
                vec![
                    "quiet-held",
                    "fused-submit",
                    "direct-fences",
                    "event-batch",
                    "fence-spin",
                ],
            ),
            (
                "fence-spin,direct-fences",
                vec!["direct-fences", "fence-spin"],
            ),
        ] {
            m.insert("backend.latency".into(), Value::String(v.into()));
            assert_eq!(latency_of(&m), Some(want), "{v}");
        }
        // Not a value: the defaults, as unset.
        for v in ["fast", "quiet-held,fast"] {
            m.insert("backend.latency".into(), Value::String(v.into()));
            assert_eq!(latency_of(&m), None, "{v}");
        }
        assert!(latency_list("quiet-held,fast").is_none());
        for ok in ["0-7,16-23", "3", "0-0"] {
            assert!(cpu_list(ok), "{ok}");
        }
        for bad in ["", "7-0", "a", "1,", "1-2-3", "0-5000"] {
            assert!(!cpu_list(bad), "{bad}");
        }
    }

    #[test]
    fn vram_limit_setting_reads_auto_off_and_numbers() {
        let mut m = Map::new();
        assert_eq!(vram_setting_of(&m), VramSetting::Default);
        for (v, want) in [
            ("auto", VramSetting::Auto),
            ("off", VramSetting::Off),
            ("4096", VramSetting::Mib(4096)),
            ("0", VramSetting::Default),
            ("lots", VramSetting::Default),
        ] {
            m.insert("gpu.vram_limit_mib".into(), Value::String(v.into()));
            assert_eq!(vram_setting_of(&m), want, "{v}");
        }
    }

    #[test]
    fn safe_mode_setting_reads_auto_true_false() {
        let mut m = Map::new();
        assert_eq!(safe_setting_of(&m), SafeSetting::Auto);
        for (v, want) in [
            (Value::Bool(true), SafeSetting::On),
            (Value::Bool(false), SafeSetting::Off),
            (Value::String("auto".into()), SafeSetting::Auto),
            (Value::String("maybe".into()), SafeSetting::Auto),
        ] {
            m.insert("gpu.safe_mode".into(), v.clone());
            assert_eq!(safe_setting_of(&m), want, "{v}");
        }
    }

    #[test]
    fn window_mib_is_read_only_when_valid() {
        let mut m = Map::new();
        assert_eq!(window_mib_of(&m), None);
        m.insert("gpu.window_mib".into(), Value::String("8192".into()));
        assert_eq!(window_mib_of(&m), Some(8192));
        m.insert("gpu.window_mib".into(), Value::String("131072".into()));
        assert_eq!(window_mib_of(&m), Some(131072));
        m.insert("gpu.window_mib".into(), Value::String("auto".into()));
        assert_eq!(window_mib_of(&m), None, "auto: the backend decides");
        // Hand-edited to something the backend would refuse: not passed.
        m.insert("gpu.window_mib".into(), Value::String("3000".into()));
        assert_eq!(window_mib_of(&m), None);
        m.insert("gpu.window_mib".into(), Value::from(4096));
        assert_eq!(window_mib_of(&m), None);
    }
}
