//! `conduit config`: user settings in ~/.config/conduit/config.json.
//!
//! | key | values | default |
//! |---|---|---|
//! | `view.close_stops_vm` | true, false | true: closing the window of a VM that `conduit view` started shuts it down. A VM started any other way (`conduit up`, virt-manager, virsh) always keeps running. |

use crate::paths;
use crate::ui::oops;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Known keys, their allowed values and what they do.
const KEYS: &[(&str, &[&str], &str)] = &[(
    "view.close_stops_vm",
    &["true", "false"],
    "closing the window of a VM that `conduit view` started shuts it down (default true)",
)];

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
    if !values.contains(&v) {
        return Err(oops(
            format!("\"{v}\" is not a value for {name}"),
            format!("Use one of: {}", values.join(", ")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_and_values_are_refused() {
        assert!(key("view.close_stops_vm").is_ok());
        assert!(key("view.nope").is_err());
    }
}
