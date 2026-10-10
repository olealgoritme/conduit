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
/// A virtiofs tag is at most 36 bytes (the Windows service limit too); the
/// `conduit-` prefix takes 8.
const MAX_NAME: usize = 28;
const TAG_PREFIX: &str = "conduit-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
    /// Added with `--force` although it covers a folder Conduit protects
    /// (see [`refused`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
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
        force: false,
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

/// A folder a guest must not get, and whether what is inside it is off
/// limits too.
#[derive(Clone, Debug)]
pub struct Protected {
    pub path: PathBuf,
    pub inside: bool,
}

/// What sharing must not expose: the home folder and the configuration
/// folder (and everything above them), and Conduit's own configuration and
/// data (where `shares.json` lives) and the systemd user units, inside and
/// above. A guest that could write any of these could run code on the host
/// or widen its own shares.
pub fn protected() -> Vec<Protected> {
    let config_home = paths::config_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths::home().join(".config"));
    let p = |path: PathBuf, inside| Protected { path, inside };
    vec![
        p(PathBuf::from("/"), false),
        p(paths::home(), false),
        p(config_home.clone(), false),
        p(paths::config_dir(), true),
        p(paths::data_dir(), true),
        p(config_home.join("systemd"), true),
    ]
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Why sharing `dir` would expose a protected folder, if it would.
pub fn refused(dir: &Path, protect: &[Protected]) -> Option<String> {
    let d = canon(dir);
    for pr in protect {
        let pp = canon(&pr.path);
        if pp.starts_with(&d) {
            return Some(if pp == d {
                format!("{} is a folder Conduit protects", d.display())
            } else {
                format!("{} contains {}", d.display(), pp.display())
            });
        }
        if pr.inside && d.starts_with(&pp) {
            return Some(format!("{} is inside {}", d.display(), pp.display()));
        }
    }
    None
}

/// Add a folder to the list (not saved). `dir` must be an existing directory
/// that exposes nothing [`protected`], unless `force`.
pub fn add_to(
    list: &mut Vec<Share>,
    dir: &Path,
    name: Option<&str>,
    ro: bool,
    force: bool,
    protect: &[Protected],
) -> Result<Share> {
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
    let forced = match refused(&abs, protect) {
        Some(why) if !force => {
            return Err(oops(
                format!("will not share {}: {why}", abs.display()),
                "A guest that can write there can change the host's configuration or its own shares. Share a folder inside your home instead, or add --force if you mean it",
            ))
        }
        Some(_) => true,
        None => false,
    };
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
        force: forced,
    };
    list.push(s.clone());
    Ok(s)
}

/// What `conduit _share` serves for one folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serve {
    pub dir: PathBuf,
    pub read_only: bool,
    /// Why the empty placeholder is served instead of the folder.
    pub placeholder: Option<String>,
}

/// Decide what `conduit _share VM:NAME` serves. It never refuses: QEMU
/// cannot start the VM without every share socket answering, so anything
/// wrong (no such folder in `list`, a list that does not parse, a missing
/// directory, a protected path that was not added with `--force`, read-only
/// without `--readonly` support) serves the empty read-only `placeholder`
/// instead and says why. A missing `default` folder is made again.
pub struct ServeCtx<'a> {
    pub can_ro: bool,
    pub protect: &'a [Protected],
    pub placeholder: &'a Path,
    pub default: &'a Share,
}

pub fn plan_serve(vm: &str, name: &str, list: Result<Vec<Share>>, cx: &ServeCtx) -> Serve {
    let ServeCtx {
        can_ro,
        protect,
        placeholder,
        default: d,
    } = *cx;
    let empty = |why: String| Serve {
        dir: placeholder.to_path_buf(),
        read_only: can_ro,
        placeholder: Some(why),
    };
    let list = match list {
        Ok(l) => l,
        Err(e) => return empty(format!("{}: {e:#}", file(vm).display())),
    };
    let Some(s) = list.into_iter().find(|s| s.name == name) else {
        return empty(format!("{vm} has no shared folder \"{name}\""));
    };
    if !s.force {
        if let Some(why) = refused(Path::new(&s.path), protect) {
            return empty(format!("will not share {}: {why}", s.path));
        }
    }
    let dir = PathBuf::from(&s.path);
    if !dir.is_dir() {
        let remade = s.name == d.name && s.path == d.path && std::fs::create_dir_all(&dir).is_ok();
        if !remade {
            return empty(format!("the shared folder {} does not exist", s.path));
        }
    }
    if s.read_only && !can_ro {
        return empty(format!(
            "\"{name}\" is read-only but this virtiofsd has no --readonly (1.11 or newer has)"
        ));
    }
    Serve {
        dir,
        read_only: s.read_only,
        placeholder: None,
    }
}

/// The empty folder served in place of one that cannot be.
pub fn placeholder_dir(vm: &str, name: &str) -> PathBuf {
    paths::run_dir(vm).join(format!("share-{name}.empty"))
}

/// Make `p` an empty folder (anything a guest left there goes), read-only
/// by mode as well.
pub fn make_placeholder(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if p.symlink_metadata().is_ok() {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
    }
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o555))?;
    Ok(())
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

pub fn add(vm: &str, dir: &Path, name: Option<&str>, ro: bool, force: bool) -> Result<()> {
    let link = link_for(vm)?;
    readonly_ok(ro)?;
    let mut v = load_or_init(vm)?;
    let s = add_to(&mut v, dir, name, ro, force, &protected())?;
    if s.force {
        ui::warn(format!(
            "sharing {} although it exposes a protected folder (--force)",
            s.path
        ));
    }
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
        for ok in ["Conduit", "my_games-2", "a", &"x".repeat(28)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "my games", "a/b", "ö", "a:b", &"x".repeat(29)] {
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
        let s = add_to(&mut v, &dir, Some("Docs"), true, false, &[]).unwrap();
        assert!(s.read_only && s.name == "Docs");
        assert!(add_to(&mut v, &dir, Some("docs"), false, false, &[]).is_err());
        assert!(add_to(&mut v, &dir, Some("bad name"), false, false, &[]).is_err());
        assert!(add_to(&mut v, &dir.join("missing"), None, false, false, &[]).is_err());
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("conduit-shares-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A home with ~/.config/conduit, data and units, as `protected()` lists
    /// them, under `root`.
    fn fake_protected(root: &Path) -> Vec<Protected> {
        let home = root.join("home/me");
        for d in [
            ".config/conduit",
            ".config/systemd/user",
            ".local/share/conduit/vms/w",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join("Games")).unwrap();
        let p = |path: PathBuf, inside| Protected { path, inside };
        vec![
            p(PathBuf::from("/"), false),
            p(home.clone(), false),
            p(home.join(".config"), false),
            p(home.join(".config/conduit"), true),
            p(home.join(".local/share/conduit"), true),
            p(home.join(".config/systemd"), true),
        ]
    }

    #[test]
    fn protected_folders_are_refused_unless_forced() {
        let root = scratch("prot");
        let pr = fake_protected(&root);
        let home = root.join("home/me");
        for bad in [
            PathBuf::from("/"),
            root.join("home"),
            home.clone(),
            home.join(".config"),
            home.join(".config/conduit"),
            home.join(".config/systemd/user"),
            home.join(".local/share"),
            home.join(".local/share/conduit/vms/w"),
        ] {
            assert!(refused(&bad, &pr).is_some(), "{}", bad.display());
            let mut v = Vec::new();
            let e = add_to(&mut v, &bad, Some("x"), false, false, &pr).unwrap_err();
            assert!(format!("{e:#}").contains("will not share"), "{e:#}");
            let s = add_to(&mut v, &bad, Some("x"), false, true, &pr).unwrap();
            assert!(s.force);
        }
        assert_eq!(refused(&home.join("Games"), &pr), None);
        // Above Conduit's data folder is refused too.
        assert!(refused(&home.join(".local"), &pr).is_some());
        // A symlink to a protected folder is the folder.
        let link = root.join("sneaky");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(refused(&link, &pr).is_some());
        let mut v = Vec::new();
        let s = add_to(&mut v, &home.join("Games"), None, false, false, &pr).unwrap();
        assert!(!s.force);
        // `force` is kept in shares.json only when set.
        assert!(!render(&v).contains("force"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn share(name: &str, path: &Path) -> Share {
        Share {
            name: name.into(),
            path: path.display().to_string(),
            read_only: false,
            force: false,
        }
    }

    #[test]
    fn the_share_helper_serves_a_placeholder_instead_of_failing() {
        let root = scratch("serve");
        let pr = fake_protected(&root);
        let home = root.join("home/me");
        let ph = root.join("ph");
        let games = home.join("Games");
        let good = |l: Vec<Share>| Ok(l);
        let dflt = share(DEFAULT_NAME, &home.join("Conduit/w"));
        let cx = |can_ro| ServeCtx {
            can_ro,
            protect: &pr,
            placeholder: &ph,
            default: &dflt,
        };

        // The folder itself.
        let s = plan_serve("w", "G", good(vec![share("G", &games)]), &cx(true));
        assert_eq!(
            (s.dir.clone(), s.placeholder.is_none()),
            (games.clone(), true)
        );

        // Everything else: the placeholder, read-only, with a reason.
        let cases: Vec<(Result<Vec<Share>>, bool, &str)> = vec![
            (Err(anyhow::anyhow!("broken json")), true, "broken json"),
            (good(vec![share("G", &games)]), true, ""),
            (good(vec![share("Other", &games)]), true, "no shared folder"),
            (
                good(vec![share("G", &root.join("gone"))]),
                true,
                "does not exist",
            ),
            // shares.json may be guest-writable: a protected path written
            // into it is refused at start too.
            (good(vec![share("G", &home)]), true, "will not share"),
            (
                good(vec![share("G", Path::new("/"))]),
                true,
                "will not share",
            ),
            (
                good(vec![Share {
                    read_only: true,
                    ..share("G", &games)
                }]),
                false,
                "--readonly",
            ),
        ];
        for (list, can_ro, why) in cases {
            if why.is_empty() {
                continue;
            }
            let s = plan_serve("w", "G", list, &cx(can_ro));
            assert_eq!(s.dir, ph);
            assert_eq!(s.read_only, can_ro);
            let w = s.placeholder.unwrap();
            assert!(w.contains(why), "{w}");
        }
        // A share added with --force is served.
        let forced = Share {
            force: true,
            ..share("G", &home)
        };
        let s = plan_serve("w", "G", good(vec![forced]), &cx(true));
        assert_eq!(s.dir, home);
        // A missing default folder is made again; another missing one is not.
        let gone = home.join("Conduit/w");
        assert!(!gone.exists());
        let s = plan_serve("w", DEFAULT_NAME, good(vec![dflt.clone()]), &cx(true));
        assert_eq!((s.dir, s.placeholder), (gone.clone(), None));
        assert!(gone.is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_placeholder_is_emptied_and_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("ph");
        let ph = root.join("share-G.empty");
        make_placeholder(&ph).unwrap();
        std::fs::set_permissions(&ph, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(ph.join("left-by-guest"), b"x").unwrap();
        make_placeholder(&ph).unwrap();
        assert_eq!(std::fs::read_dir(&ph).unwrap().count(), 0);
        assert_eq!(
            std::fs::metadata(&ph).unwrap().permissions().mode() & 0o777,
            0o555
        );
        std::fs::set_permissions(&ph, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
