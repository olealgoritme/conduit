//! `conduit display VM [--add WxH] [--rm WxH]`: the modes a VM's virtual
//! monitor offers (docs/SCANOUT.md "Mode list").
//!
//! Native (the mode the VM runs with), the standard modes up to it, and the
//! VM's custom modes, kept one `WxH` per line in its `display-modes` file.
//! The backend re-reads the file when it changes and gives the guest and the
//! viewer the new list at once; the viewer's menu edits the same file
//! through the backend. One list per VM, whoever edits it.

use crate::lvrun;
use crate::paths;
use crate::run::Rt;
use crate::ui;
use anyhow::{bail, Context, Result};
use protocol::modes;
use std::path::{Path, PathBuf};

/// The VM's custom-modes file (the backend's `--display-modes`).
pub fn modes_file(name: &str) -> PathBuf {
    paths::vm_dir(name).join("display-modes")
}

fn read(path: &Path) -> Vec<(u32, u32)> {
    std::fs::read_to_string(path)
        .map(|t| modes::parse_custom(&t))
        .unwrap_or_default()
}

fn write(path: &Path, list: &[(u32, u32)]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, modes::format_custom(list))
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("writing {}", path.display()))
}

/// Apply `--add` and `--rm` (in that order) to `list`.
fn edit(mut list: Vec<(u32, u32)>, add: &[String], rm: &[String]) -> Result<Vec<(u32, u32)>> {
    for a in add {
        let m = modes::parse_wxh(a).map_err(|e| anyhow::anyhow!("--add {a}: {e}"))?;
        list = modes::add_custom(&list, m).map_err(|e| anyhow::anyhow!("--add {a}: {e}"))?;
    }
    for r in rm {
        let m = modes::parse_wxh(r).map_err(|e| anyhow::anyhow!("--rm {r}: {e}"))?;
        list = modes::remove_custom(&list, m)
            .ok_or_else(|| anyhow::anyhow!("--rm {r}: not one of the VM's custom modes"))?;
    }
    Ok(list)
}

pub fn run(name: &str, add: &[String], rm: &[String]) -> Result<()> {
    if !paths::vm_dir(name).is_dir() {
        bail!("no VM named {name}");
    }
    let path = modes_file(name);
    let before = read(&path);
    let after = edit(before.clone(), add, rm)?;
    if after != before {
        write(&path, &after)?;
    }
    let native = Rt::new(name)
        .ok()
        .and_then(|rt| lvrun::running_mode(&rt))
        .flatten();
    match native {
        Some(m) => {
            println!("{name}: native {}x{}@{}", m.width, m.height, m.hz);
            let list = modes::mode_list((m.width, m.height), &after);
            let mut line = String::new();
            for e in &list {
                line.push_str(&format!(
                    "{}x{}{} ",
                    e.width,
                    e.height,
                    if e.custom { "*" } else { "" }
                ));
            }
            println!("  modes: {}", line.trim_end());
            if !after.is_empty() {
                println!("  (* custom)");
            }
        }
        None => {
            println!("{name}: native is the mode it starts with (not running now)");
            println!("  standard modes up to native, plus the custom modes:");
        }
    }
    if after.is_empty() {
        println!("  custom: none (add one with --add WxH)");
    } else {
        let s: Vec<String> = after.iter().map(|(w, h)| format!("{w}x{h}")).collect();
        println!("  custom: {}", s.join(" "));
    }
    if after != before {
        ui::info("saved; a running VM's guest and viewer get the new list within a second");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn add_then_remove() {
        let l = edit(vec![], &s(&["1280x960", "1600x1200"]), &[]).unwrap();
        assert_eq!(l, vec![(1280, 960), (1600, 1200)]);
        let l = edit(l, &s(&["1280x960"]), &s(&["1600x1200"])).unwrap();
        assert_eq!(l, vec![(1280, 960)]);
        assert!(edit(l.clone(), &[], &s(&["800x600"])).is_err());
        assert!(edit(l.clone(), &s(&["junk"]), &[]).is_err());
        assert!(edit(l, &s(&["10x10"]), &[]).is_err());
    }

    #[test]
    fn the_file_is_what_the_backend_parses() {
        let dir = std::env::temp_dir().join(format!("conduit-display-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("display-modes");
        write(&p, &[(1280, 960), (2560, 1080)]).unwrap();
        assert_eq!(read(&p), vec![(1280, 960), (2560, 1080)]);
        assert_eq!(
            modes::parse_custom(&std::fs::read_to_string(&p).unwrap()),
            vec![(1280, 960), (2560, 1080)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
