//! Shared folders: host directories a libvirt VM sees as virtiofs devices.
//!
//! A VM's folders are listed in `vms/NAME/shares.json` (name, path,
//! read_only). Each one is a virtiofs device with the tag `conduit-NAME`,
//! served by its own socket-activated virtiofsd (`conduit-share@VM:NAME`,
//! see units.rs). The first `conduit attach` / `libvirt enable` creates the
//! default folder `Conduit` at `~/Conduit/VM`. Windows guests mount the tags
//! as drive letters (docs/WINDOWS.md), Linux guests at `/mnt/conduit/NAME`.

use crate::paths;
use crate::qemu;
use crate::ui::{self, oops};
use crate::units::{self, Scope};
use crate::virt::{self, esc, Link};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_NAME: &str = "Conduit";
const MAX_NAME: usize = 32;
const TAG_PREFIX: &str = "conduit-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

/// What the domain gets for one folder: its virtiofs tag and socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wire {
    pub tag: String,
    pub sock: PathBuf,
}

pub fn tag(name: &str) -> String {
    format!("{TAG_PREFIX}{name}")
}

/// Is this a tag of ours (a `conduit-*` virtiofs target)?
pub fn is_share_tag(t: &str) -> bool {
    t.starts_with(TAG_PREFIX) && t.len() > TAG_PREFIX.len()
}

/// Tag-safe: letters, digits, `_` and `-`, 1 to 32 characters.
pub fn validate_name(n: &str) -> Result<()> {
    if n.is_empty()
        || n.len() > MAX_NAME
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(oops(
            format!("\"{n}\" is not a usable shared folder name"),
            format!("Use 1 to {MAX_NAME} letters, digits, _ or -"),
        ));
    }
    Ok(())
}

/// A folder name from a directory's last component ("My Games" -> "My-Games").
pub fn name_from_dir(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(MAX_NAME)
        .collect();
    let n = n.trim_matches('-').to_string();
    if n.is_empty() {
        "share".into()
    } else {
        n
    }
}

/// The list in `shares.json` form. Names must be valid and unique (ignoring case).
pub fn parse(text: &str) -> Result<Vec<Share>> {
    let v: Vec<Share> = serde_json::from_str(text).context("shares.json is not a share list")?;
    let mut seen: Vec<String> = Vec::new();
    for s in &v {
        validate_name(&s.name)?;
        let k = s.name.to_ascii_lowercase();
        if seen.contains(&k) {
            anyhow::bail!("shared folder \"{}\" is listed twice", s.name);
        }
        seen.push(k);
    }
    Ok(v)
}

pub fn render(v: &[Share]) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "[]".into()) + "\n"
}

pub fn file(vm: &str) -> PathBuf {
    paths::vm_dir(vm).join("shares.json")
}

/// The VM's folders; none when it has no list yet.
pub fn load(vm: &str) -> Result<Vec<Share>> {
    match std::fs::read_to_string(file(vm)) {
        Ok(t) => parse(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", file(vm).display())),
    }
}

pub fn save(vm: &str, v: &[Share]) -> Result<()> {
    let f = file(vm);
    std::fs::create_dir_all(f.parent().unwrap())?;
    std::fs::write(&f, render(v)).with_context(|| format!("writing {}", f.display()))
}

pub fn default_share(vm: &str) -> Share {
    Share {
        name: DEFAULT_NAME.into(),
        path: paths::home().join("Conduit").join(vm).display().to_string(),
        read_only: false,
    }
}

/// The VM's folders; a VM that has no list yet gets the default one (and its
/// directory). A list that exists is never topped up, so removing the
/// default folder sticks.
pub fn load_or_init(vm: &str) -> Result<Vec<Share>> {
    if file(vm).is_file() {
        return load(vm);
    }
    let d = default_share(vm);
    std::fs::create_dir_all(&d.path).with_context(|| format!("creating {}", d.path))?;
    let v = vec![d];
    save(vm, &v)?;
    Ok(v)
}

pub fn wires(scope: &Scope, vm: &str, shares: &[Share]) -> Vec<Wire> {
    shares
        .iter()
        .map(|s| Wire {
            tag: tag(&s.name),
            sock: units::share_socket_path(scope, vm, &s.name),
        })
        .collect()
}

/// The `<filesystem>` of one folder, as libvirt takes it (also for hot-plug).
pub fn device_xml(w: &Wire) -> String {
    format!(
        "<filesystem type='mount'>\n  <driver type='virtiofs' queue='1024'/>\n  <source socket='{}'/>\n  <target dir='{}'/>\n</filesystem>\n",
        esc(&w.sock.display().to_string()),
        esc(&w.tag)
    )
}

/// Add a folder to the list (not saved). `dir` must be an existing directory.
pub fn add_to(list: &mut Vec<Share>, dir: &Path, name: Option<&str>, ro: bool) -> Result<Share> {
    let abs = std::fs::canonicalize(dir).map_err(|_| {
        oops(
            format!("{} does not exist", dir.display()),
            "Give an existing directory",
        )
    })?;
    if !abs.is_dir() {
        return Err(oops(
            format!("{} is not a directory", abs.display()),
            "Give an existing directory",
        ));
    }
    let name = name
        .map(String::from)
        .unwrap_or_else(|| name_from_dir(&abs));
    validate_name(&name)?;
    if list.iter().any(|s| s.name.eq_ignore_ascii_case(&name)) {
        return Err(oops(
            format!("there is already a shared folder called \"{name}\""),
            "Pick another name with --name",
        ));
    }
    let s = Share {
        name,
        path: abs.display().to_string(),
        read_only: ro,
    };
    list.push(s.clone());
    Ok(s)
}

fn readonly_ok(ro: bool) -> Result<()> {
    if ro {
        let vfsd = qemu::virtiofsd()
            .ok_or_else(|| oops("virtiofsd is not installed", "Install it, then try again"))?;
        if !qemu::supports_readonly(&vfsd) {
            return Err(oops(
                "this virtiofsd cannot serve read-only (--readonly needs virtiofsd 1.11 or newer)",
                "Update virtiofsd, or share the folder writable",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- commands

fn link_for(vm: &str) -> Result<Link> {
    Link::load(vm).ok_or_else(|| {
        oops(
            format!("{vm} is not a libvirt VM Conduit knows"),
            format!("`conduit attach {vm}` gives it Conduit's GPU and shared folders"),
        )
    })
}

pub fn list(vm: &str) -> Result<()> {
    link_for(vm)?;
    let v = load(vm)?;
    if v.is_empty() {
        println!("{vm} has no shared folders (`conduit share add {vm} DIR`)");
        return Ok(());
    }
    let w = v.iter().map(|s| s.name.len()).max().unwrap_or(4).max(4);
    println!("{:<w$}  {:<4}  PATH", "NAME", "MODE");
    for s in v {
        println!(
            "{:<w$}  {:<4}  {}",
            s.name,
            if s.read_only { "ro" } else { "rw" },
            s.path
        );
    }
    Ok(())
}

pub fn add(vm: &str, dir: &Path, name: Option<&str>, ro: bool) -> Result<()> {
    let link = link_for(vm)?;
    readonly_ok(ro)?;
    let mut v = load_or_init(vm)?;
    let s = add_to(&mut v, dir, name, ro)?;
    save(vm, &v)?;
    let how = apply(&link, vm, &v, Some((&s.name, true)))?;
    println!("Shared {} as \"{}\" ({}). {how}", s.path, s.name, mode(&s));
    Ok(())
}

pub fn rm(vm: &str, name: &str) -> Result<()> {
    let link = link_for(vm)?;
    let mut v = load_or_init(vm)?;
    let i = v
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            oops(
                format!("{vm} has no shared folder \"{name}\""),
                format!("`conduit share list {vm}` shows them"),
            )
        })?;
    let s = v.remove(i);
    save(vm, &v)?;
    let how = apply(&link, vm, &v, Some((&s.name, false)))?;
    println!("Stopped sharing \"{}\" ({}). {how}", s.name, s.path);
    Ok(())
}

pub fn set_read_only(vm: &str, name: &str, ro: bool) -> Result<()> {
    let link = link_for(vm)?;
    readonly_ok(ro)?;
    let mut v = load_or_init(vm)?;
    let s = v
        .iter_mut()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            oops(
                format!("{vm} has no shared folder \"{name}\""),
                format!("`conduit share list {vm}` shows them"),
            )
        })?;
    s.read_only = ro;
    let label = format!("\"{}\" is now {}", s.name, mode(s));
    save(vm, &v)?;
    apply(&link, vm, &v, None)?;
    let running = link
        .virsh()
        .state(&link.domain)
        .is_some_and(|s| virt::state_is_up(&s));
    println!(
        "{label}.{}",
        if running {
            " Applies at the next start of the VM."
        } else {
            ""
        }
    );
    Ok(())
}

fn mode(s: &Share) -> &'static str {
    if s.read_only {
        "read-only"
    } else {
        "read-write"
    }
}

/// Bring the host side in line with `shares`: units, then the persistent
/// definition, then (a running VM) a live hot-plug of the one folder that
/// changed (`change`: name and whether it was added). Returns what to tell
/// the user.
fn apply(link: &Link, vm: &str, shares: &[Share], change: Option<(&str, bool)>) -> Result<String> {
    let scope = link.scope()?;
    let v = link.virsh();
    v.reachable()?;
    let running = v.state(&link.domain).is_some_and(|s| virt::state_is_up(&s));
    let live_has = |t: &str| {
        v.run(&["dumpxml", &link.domain])
            .map(|x| x.contains(&format!("<target dir='{t}'/>")))
            .unwrap_or(false)
    };
    let names: Vec<String> = shares.iter().map(|s| s.name.clone()).collect();
    let exe = virt::conduit_exe()?;
    // A removed folder's units stay while a running QEMU still has the device.
    let busy = |n: &str| running && live_has(&tag(n));
    units::install_shares(&scope, vm, &exe, &names, &busy)?;

    let ws = wires(&scope, vm, shares);
    let xml = crate::libvirt::apply_shares(&v.inactive_xml(&link.domain)?, &ws)?;
    v.define(&xml, vm).map_err(|e| {
        oops(
            format!("libvirt refused the shared folders: {e:#}"),
            "Nothing was changed in the VM",
        )
    })?;
    if !running {
        return Ok("It appears the next time the VM starts.".into());
    }
    let Some((name, added)) = change else {
        return Ok(String::new());
    };
    let t = tag(name);
    let dev = paths::vm_dir(vm).join(".share-device.xml");
    let w = Wire {
        sock: units::share_socket_path(&scope, vm, name),
        tag: t.clone(),
    };
    std::fs::write(&dev, device_xml(&w))?;
    let live = if added {
        if live_has(&t) {
            Ok(String::new())
        } else {
            v.run(&[
                "attach-device",
                &link.domain,
                dev.to_str().unwrap(),
                "--live",
            ])
        }
    } else if live_has(&t) {
        v.run(&[
            "detach-device",
            &link.domain,
            dev.to_str().unwrap(),
            "--live",
        ])
    } else {
        Ok(String::new())
    };
    let _ = std::fs::remove_file(&dev);
    Ok(match (live, added) {
        (Ok(_), true) => "Attached to the running VM.".into(),
        (Ok(_), false) => "Detached from the running VM.".into(),
        (Err(e), _) => {
            ui::warn(format!("could not change the running VM: {e:#}"));
            "Applies at the next start of the VM.".into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_tag_safe() {
        for ok in ["Conduit", "my_games-2", "a", &"x".repeat(32)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "my games", "a/b", "ö", "a:b", &"x".repeat(33)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn name_from_a_directory() {
        assert_eq!(name_from_dir(Path::new("/data/My Games")), "My-Games");
        assert_eq!(name_from_dir(Path::new("/")), "share");
        assert_eq!(name_from_dir(Path::new("/x/.hidden")), "hidden");
    }

    #[test]
    fn parses_and_rejects_duplicates() {
        let v = parse(r#"[{"name":"A","path":"/a"},{"name":"B","path":"/b","read_only":true}]"#)
            .unwrap();
        assert_eq!(v.len(), 2);
        assert!(!v[0].read_only && v[1].read_only);
        assert_eq!(parse(&render(&v)).unwrap(), v);
        assert!(parse(r#"[{"name":"A","path":"/a"},{"name":"a","path":"/b"}]"#).is_err());
        assert!(parse(r#"[{"name":"a b","path":"/a"}]"#).is_err());
        assert!(parse("{}").is_err());
    }

    #[test]
    fn adds_existing_directories_only() {
        let dir = std::env::temp_dir().join(format!("conduit-share-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut v = vec![default_share("t")];
        let s = add_to(&mut v, &dir, Some("Docs"), true).unwrap();
        assert!(s.read_only && s.name == "Docs");
        assert!(add_to(&mut v, &dir, Some("docs"), false).is_err());
        assert!(add_to(&mut v, &dir, Some("bad name"), false).is_err());
        assert!(add_to(&mut v, &dir.join("missing"), None, false).is_err());
        assert_eq!(v.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_and_tag() {
        let w = Wire {
            tag: tag("Docs"),
            sock: PathBuf::from("/run/user/1000/conduit/vm/share-Docs.sock"),
        };
        let x = device_xml(&w);
        assert!(x.contains("<target dir='conduit-Docs'/>"));
        assert!(x.contains("share-Docs.sock"));
        assert!(
            is_share_tag("conduit-Docs") && !is_share_tag("nvidia") && !is_share_tag("conduit-")
        );
    }
}
