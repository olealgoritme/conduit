//! `conduit config`: user settings in ~/.config/conduit/config.json.
//!
//! | key | values | default |
//! |---|---|---|
//! | `view.close_stops_vm` | true, false | true: closing the window of a VM that `conduit view` started shuts it down. A VM started any other way (`conduit up`, virt-manager, virsh) always keeps running. |
//! | `venus.guest_blobs` | true, false | false: the backend serves guest-memory blobs (docs/VENUS.md "Guest-memory blobs"), Venus copy destinations over the guest's own pages, for the Windows KMD's windowed Present. Opt-in while new. Applies when a VM's backend next starts. |
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
    if !values.contains(&v) && !number {
        return Err(oops(
            format!("\"{v}\" is not a value for {name}"),
            format!(
                "Use one of: {}{}",
                values.join(", "),
                if *name == "gpu.vram_limit_mib" {
                    ", or a number of MiB"
                } else {
                    ""
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

/// `gpu.vram_limit_mib`: how the backend's video-memory cap is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VramSetting {
    /// Unset: the default cap when safe mode is on and the card drives a
    /// monitor, nothing otherwise (protect.rs).
    Default,
    /// The default cap whenever the card drives a monitor.
    Auto,
    /// No cap.
    Off,
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
