//! Hyprland: finding the running instance, and the opt-in `--tune-hyprland`
//! settings (direct scanout + tearing for the viewer), saved and restored.

use crate::ui;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const VIEWER_CLASS: &str = "nvkvm-display-broker";

/// Run hyprctl against one instance.
pub fn hyprctl(sig: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("hyprctl")
        .args(args)
        .env("HYPRLAND_INSTANCE_SIGNATURE", sig)
        .output()
        .context("could not run hyprctl")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// hyprctl exits 0 even when the instance is gone, so look at what it says.
fn answers(sig: &str) -> bool {
    hyprctl(sig, &["-j", "version"])
        .map(|s| s.contains("\"commit\"") || s.contains("\"tag\""))
        .unwrap_or(false)
}

/// The signature of the running Hyprland. A shell can carry the signature of a
/// Hyprland that has since restarted, so a stale one is replaced by the newest
/// live instance found on disk.
pub fn instance() -> Option<String> {
    if !crate::sys::have("hyprctl") {
        return None;
    }
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if !sig.is_empty() && answers(&sig) {
            return Some(sig);
        }
    }
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for base in [crate::paths::xdg_runtime().join("hypr"), PathBuf::from("/tmp/hypr")] {
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                    dirs.push((t, e.path()));
                }
            }
        }
    }
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    dirs.into_iter()
        .filter_map(|(_, p)| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .find(|s| answers(s))
}

fn get_int(sig: &str, opt: &str) -> Option<i64> {
    let out = hyprctl(sig, &["getoption", opt, "-j"]).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    v.get("int")?.as_i64()
}

/// Saved state: which instance, and the two values we change.
#[derive(Debug, PartialEq)]
pub struct Saved {
    pub sig: String,
    pub no_direct_scanout: i64,
    pub allow_tearing: i64,
}

impl Saved {
    pub fn render(&self) -> String {
        format!("sig={}\nno_direct_scanout={}\nallow_tearing={}\n", self.sig, self.no_direct_scanout, self.allow_tearing)
    }

    pub fn parse(s: &str) -> Option<Saved> {
        let mut sig = None;
        let mut nds = None;
        let mut tear = None;
        for line in s.lines() {
            match line.split_once('=') {
                Some(("sig", v)) => sig = Some(v.to_string()),
                Some(("no_direct_scanout", v)) => nds = v.parse().ok(),
                Some(("allow_tearing", v)) => tear = v.parse().ok(),
                _ => {}
            }
        }
        Some(Saved { sig: sig?, no_direct_scanout: nds?, allow_tearing: tear? })
    }
}

/// `on`: save current values once, then enable direct scanout + tearing + an
/// immediate rule for the viewer window. `off`: put the saved values back.
/// `restore`: `off`, then forget the save.
pub fn hook(action: &str, state: &Path) -> Result<()> {
    match action {
        "on" => {
            let sig = instance().context("no running Hyprland found")?;
            if !state.exists() {
                let nds = get_int(&sig, "misc:no_direct_scanout").context("cannot read misc:no_direct_scanout")?;
                let tear = get_int(&sig, "general:allow_tearing").context("cannot read general:allow_tearing")?;
                if let Some(d) = state.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(state, Saved { sig: sig.clone(), no_direct_scanout: nds, allow_tearing: tear }.render())?;
            }
            let batch = format!(
                "keyword misc:no_direct_scanout 0 ; keyword general:allow_tearing 1 ; keyword windowrulev2 immediate,class:^({VIEWER_CLASS})$"
            );
            hyprctl(&sig, &["--batch", &batch])?;
        }
        "off" | "restore" => {
            let Ok(text) = std::fs::read_to_string(state) else { return Ok(()) };
            if let Some(s) = Saved::parse(&text) {
                let batch = format!(
                    "keyword misc:no_direct_scanout {} ; keyword general:allow_tearing {}",
                    s.no_direct_scanout, s.allow_tearing
                );
                let _ = hyprctl(&s.sig, &["--batch", &batch]);
            }
            if action == "restore" {
                let _ = std::fs::remove_file(state);
                ui::info("Hyprland settings restored");
            }
        }
        other => anyhow::bail!("unknown hook action {other:?} (on, off, restore)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_roundtrip() {
        let s = Saved { sig: "abc_123".into(), no_direct_scanout: 1, allow_tearing: 0 };
        assert_eq!(Saved::parse(&s.render()), Some(s));
        assert_eq!(Saved::parse("sig=x\n"), None);
    }
}
