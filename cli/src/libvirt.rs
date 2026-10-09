//! `conduit attach` / `conduit detach`: give an existing libvirt (virt-manager)
//! VM Conduit's GPU, and take it away again.
//!
//! attach, all validated before anything changes:
//!   1. backs the domain's definition up to vms/NAME/libvirt-backup-TIME.xml
//!      (only the first time: that is the original detach restores),
//!   2. installs the socket-activated GPU backend and virtiofsd units
//!      (units.rs), and lets libvirtd run Conduit's QEMU (AppArmor),
//!   3. defines the edited domain in one step (`virsh define --validate`):
//!      Conduit's QEMU as <emulator>, memfd shared memory, host-passthrough
//!      CPU with the host's physical address width, the NVIDIA share
//!      (virtiofs, tag "nvidia"), Conduit's metadata, the GPU as
//!      <qemu:commandline> (libvirt has no element for a generic vhost-user
//!      device), and the boot console: one VNC <graphics> on a Unix socket
//!      the backend shows, in place of SPICE and its devices (Conduit's QEMU
//!      has no SPICE). If libvirt refuses it, the units are removed again.
//!   4. installs the guest side through the QEMU guest agent when the VM runs
//!      one, or prints the one command to run inside it.
//!
//! Windows guests (libosinfo metadata, Hyper-V features, or the guest agent's
//! guest-get-osinfo; decided before step 1) also get the Hyper-V
//! enlightenments in step 3, and step 4 is replaced by a note: the guest
//! driver there is the Helios package (docs/WINDOWS.md).
//!
//! Running it again re-applies the same edit (nothing changes); detach
//! defines the backup again and removes the units.

use crate::guest;
use crate::paths;
use crate::run;
use crate::sys;
use crate::ui::{self, oops};
use crate::units::{self, Scope};
use crate::virt::{self, Kind, Link, Virsh, CHARDEV_ID, META_NS, QEMU_NS, SESSION, SYSTEM};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xmltree::{Element, EmitterConfig, XMLNode};

/// What the edited domain points at.
pub struct Wiring<'a> {
    pub emulator: &'a Path,
    pub gpu_sock: &'a Path,
    pub vfs_sock: &'a Path,
    /// QEMU's VNC server (the boot console) listens here.
    pub console_sock: &'a Path,
    pub vm: &'a str,
    /// A Windows guest ([`is_windows`]): add the Hyper-V enlightenments.
    pub windows: bool,
}

/// libosinfo's namespace in a domain's <metadata>, where virt-manager and
/// virt-install record the OS a VM was installed as.
const OSINFO_NS: &str = "http://libosinfo.org/xmlns/libvirt/domain/1.0";

/// Does the definition say the guest is Windows? Either the libosinfo OS
/// virt-manager recorded (`http://microsoft.com/win/...`) or Hyper-V
/// enlightenments, which only Windows guests are given.
pub fn is_windows(xml: &str) -> bool {
    let Ok(root) = Element::parse(xml.as_bytes()) else {
        return false;
    };
    let osinfo = root.get_child("metadata").is_some_and(|md| {
        elements(md)
            .filter(|e| {
                e.name == "libosinfo"
                    && (e.namespace.as_deref() == Some(OSINFO_NS)
                        || e.prefix.as_deref() == Some("libosinfo"))
            })
            .flat_map(elements)
            .any(|os| {
                os.name == "os"
                    && os
                        .attributes
                        .get("id")
                        .is_some_and(|id| id.starts_with("http://microsoft.com/win"))
            })
    });
    let hyperv = root
        .get_child("features")
        .and_then(|f| f.get_child("hyperv"))
        .is_some();
    osinfo || hyperv
}

/// Hyper-V enlightenments for Windows guests (docs/WINDOWS.md), with the one
/// each needs: an enlightenment whose prerequisite the domain turns off is
/// not added (QEMU would refuse to start).
const HYPERV: &[(&str, Option<&str>)] = &[
    ("relaxed", None),
    ("vapic", None),
    ("spinlocks", None), // retries='8191'
    ("vpindex", None),
    ("runtime", None),
    ("synic", Some("vpindex")),
    ("stimer", Some("synic")), // <direct state='on'/>
    ("reset", None),
    ("frequencies", None),
    ("tlbflush", Some("vpindex")),
    ("ipi", Some("vpindex")),
];

fn is_off(e: &Element) -> bool {
    e.attributes.get("state").map(String::as_str) == Some("off")
}

/// Add the Hyper-V enlightenments and the Hyper-V clock a Windows domain
/// lacks; whatever it already sets (on or off) stays. `<hyperv
/// mode='passthrough'>` already gives the guest everything and is left alone.
fn add_hyperv(root: &mut Element) {
    let hv = child_mut(child_mut(root, "features"), "hyperv");
    if hv.attributes.get("mode").map(String::as_str) != Some("passthrough") {
        for (name, needs) in HYPERV {
            if needs.is_some_and(|n| hv.get_child(n).is_some_and(is_off)) {
                continue;
            }
            match hv.get_mut_child(*name) {
                // stimer runs best in direct mode (no SynIC message per tick).
                Some(e) if *name == "stimer" && !is_off(e) && e.get_child("direct").is_none() => {
                    e.children
                        .push(XMLNode::Element(el("direct", &[("state", "on")])));
                }
                Some(_) => {}
                None => {
                    let mut e = el(name, &[("state", "on")]);
                    if *name == "spinlocks" {
                        e.attributes.insert("retries".into(), "8191".into());
                    }
                    if *name == "stimer" {
                        e.children
                            .push(XMLNode::Element(el("direct", &[("state", "on")])));
                    }
                    hv.children.push(XMLNode::Element(e));
                }
            }
        }
    }
    // <clock>: Windows keeps local time in the RTC (what virt-manager writes
    // for it), plus the Hyper-V reference clock.
    let new = root.get_child("clock").is_none();
    let clock = child_mut(root, "clock");
    if new {
        clock.attributes.insert("offset".into(), "localtime".into());
    }
    let has = elements(clock).any(|t| {
        t.name == "timer" && t.attributes.get("name").map(String::as_str) == Some("hypervclock")
    });
    if !has {
        clock.children.push(XMLNode::Element(el(
            "timer",
            &[("name", "hypervclock"), ("present", "yes")],
        )));
    }
}

fn child_mut<'a>(e: &'a mut Element, name: &str) -> &'a mut Element {
    if e.get_child(name).is_none() {
        e.children.push(XMLNode::Element(Element::new(name)));
    }
    e.get_mut_child(name).unwrap()
}

fn is_qemu(e: &Element) -> bool {
    e.prefix.as_deref() == Some("qemu") || e.namespace.as_deref() == Some(QEMU_NS)
}

fn el(name: &str, attrs: &[(&str, &str)]) -> Element {
    let mut e = Element::new(name);
    for (k, v) in attrs {
        e.attributes.insert((*k).into(), (*v).into());
    }
    e
}

fn elements(e: &Element) -> impl Iterator<Item = &Element> {
    e.children.iter().filter_map(|n| n.as_element())
}

/// PCI slots already used on bus 0 (explicit <address> elements).
fn used_slots(root: &Element) -> Vec<u8> {
    fn walk(e: &Element, out: &mut Vec<u8>) {
        if e.name == "address" && e.attributes.get("type").map(String::as_str) == Some("pci") {
            let bus = e
                .attributes
                .get("bus")
                .map(String::as_str)
                .unwrap_or("0x00");
            if u8::from_str_radix(bus.trim_start_matches("0x"), 16) == Ok(0) {
                if let Some(s) = e.attributes.get("slot") {
                    if let Ok(v) = u8::from_str_radix(s.trim_start_matches("0x"), 16) {
                        out.push(v);
                    }
                }
            }
        }
        for c in elements(e) {
            walk(c, out);
        }
    }
    let mut v = Vec::new();
    walk(root, &mut v);
    v
}

/// Our GPU's slot: kept if already chosen, else the highest free one below
/// 0x1f (libvirt fills bus 0 from the bottom; q35's ICH9 sits at 0x1f).
fn pick_slot(root: &Element, previous: Option<u8>) -> Result<u8> {
    let used = used_slots(root);
    if let Some(p) = previous.filter(|p| !used.contains(p)) {
        return Ok(p);
    }
    (0x10..=0x1e)
        .rev()
        .find(|s| !used.contains(s))
        .ok_or_else(|| {
            oops(
                "no free PCI slot on the VM's main bus for the GPU",
                "Remove a device from the VM in virt-manager, then try again",
            )
        })
}

/// The slot an earlier attach gave our GPU (from its -device argument).
fn previous_slot(args: &[String]) -> Option<u8> {
    args.iter()
        .find(|a| a.contains(&format!("chardev={CHARDEV_ID}")))
        .and_then(|a| {
            a.split(',')
                .find_map(|kv| kv.strip_prefix("addr="))
                .and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        })
}

/// SPICE-only character devices (agent, USB redirection, smartcard, ports).
fn is_spice_dev(e: &Element) -> bool {
    matches!(
        e.attributes.get("type").map(String::as_str),
        Some("spicevmc" | "spiceport")
    )
}

/// Make the devices runnable on Conduit's QEMU, which has VNC but no SPICE
/// and no OpenGL: one VNC server on `console` (the boot console the backend
/// shows), no SPICE devices, no SPICE audio, no QXL or 3D-accelerated video.
/// The emulated video device stays: firmware and boot screens draw on it.
fn edit_display(devices: &mut Element, console: &Path) {
    let first = devices
        .children
        .iter()
        .position(|n| matches!(n, XMLNode::Element(e) if e.name == "graphics"));
    devices.children.retain(|n| match n {
        XMLNode::Element(e) => e.name != "graphics" && e.name != "redirfilter" && !is_spice_dev(e),
        _ => true,
    });
    let mut g = el("graphics", &[("type", "vnc")]);
    g.attributes
        .insert("socket".into(), console.display().to_string());
    let at = first
        .unwrap_or(devices.children.len())
        .min(devices.children.len());
    devices.children.insert(at, XMLNode::Element(g));
    for n in devices.children.iter_mut() {
        let XMLNode::Element(e) = n else { continue };
        match e.name.as_str() {
            "audio" if e.attributes.get("type").map(String::as_str) == Some("spice") => {
                e.attributes.insert("type".into(), "none".into());
            }
            "video" => {
                if let Some(m) = e.get_mut_child("model") {
                    // virtio-gpu without virgl (no OpenGL in Conduit's QEMU).
                    m.children
                        .retain(|c| !matches!(c, XMLNode::Element(a) if a.name == "acceleration"));
                    // QXL exists only with SPICE: virtio-vga, its heads kept.
                    if m.attributes.get("type").map(String::as_str) == Some("qxl") {
                        m.attributes.retain(|k, _| k == "heads" || k == "primary");
                        m.attributes.insert("type".into(), "virtio".into());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Rewrite a libvirt domain XML for Conduit. Idempotent: applying it to its
/// own output changes nothing.
pub fn edit_domain(xml: &str, w: &Wiring) -> Result<String> {
    let mut root =
        Element::parse(xml.as_bytes()).context("libvirt returned XML Conduit cannot read")?;
    if root.name != "domain" {
        anyhow::bail!("not a libvirt domain (root element is <{}>)", root.name);
    }
    let mut ns = root
        .namespaces
        .clone()
        .unwrap_or_else(xmltree::Namespace::empty);
    ns.put("qemu", QEMU_NS);
    root.namespaces = Some(ns);

    let machine = root
        .get_child("os")
        .and_then(|o| o.get_child("type"))
        .and_then(|t| t.attributes.get("machine").cloned())
        .unwrap_or_default();
    let q35 = machine.contains("q35") || machine.is_empty();
    let bus = if q35 { "pcie.0" } else { "pci.0" };
    // A versioned machine type belongs to the old emulator (e.g. Ubuntu's
    // "pc-q35-noble"); Conduit's QEMU takes the plain alias of the same family.
    if let Some(t) = root
        .get_mut_child("os")
        .and_then(|o| o.get_mut_child("type"))
    {
        t.attributes
            .insert("machine".into(), if q35 { "q35" } else { "pc" }.into());
    }

    // <metadata><conduit:vm .../>
    {
        let md = child_mut(&mut root, "metadata");
        md.children.retain(|n| {
            !matches!(n, XMLNode::Element(e) if e.namespace.as_deref() == Some(META_NS) || e.prefix.as_deref() == Some("conduit"))
        });
        let mut v = el("vm", &[("name", w.vm), ("kind", "attached")]);
        v.prefix = Some("conduit".into());
        v.namespace = Some(META_NS.into());
        let mut vns = xmltree::Namespace::empty();
        vns.put("conduit", META_NS);
        v.namespaces = Some(vns);
        md.children.push(XMLNode::Element(v));
    }

    // <memoryBacking><source type='memfd'/><access mode='shared'/>
    {
        let mb = child_mut(&mut root, "memoryBacking");
        mb.children.retain(
            |n| !matches!(n, XMLNode::Element(e) if e.name == "source" || e.name == "access"),
        );
        mb.children
            .push(XMLNode::Element(el("source", &[("type", "memfd")])));
        mb.children
            .push(XMLNode::Element(el("access", &[("mode", "shared")])));
    }

    // <cpu mode='host-passthrough'><maxphysaddr mode='passthrough'/>: the
    // GPU's 64-bit shared-memory BAR needs the host's address width. A
    // <topology> the VM had is kept.
    {
        let topo = root
            .get_child("cpu")
            .and_then(|c| c.get_child("topology"))
            .cloned();
        root.children
            .retain(|n| !matches!(n, XMLNode::Element(e) if e.name == "cpu"));
        let mut cpu = el(
            "cpu",
            &[
                ("mode", "host-passthrough"),
                ("check", "none"),
                ("migratable", "off"),
            ],
        );
        if let Some(t) = topo {
            cpu.children.push(XMLNode::Element(t));
        }
        cpu.children.push(XMLNode::Element(el(
            "maxphysaddr",
            &[("mode", "passthrough")],
        )));
        // libvirt wants <cpu> after <features>; it reorders on define anyway.
        root.children.push(XMLNode::Element(cpu));
    }

    // <devices>: emulator first; the NVIDIA share (virtiofs tag "nvidia").
    {
        let devices = child_mut(&mut root, "devices");
        let em = child_mut(devices, "emulator");
        em.children = vec![XMLNode::Text(w.emulator.display().to_string())];
        let idx = devices
            .children
            .iter()
            .position(|n| matches!(n, XMLNode::Element(e) if e.name == "emulator"))
            .unwrap();
        let node = devices.children.remove(idx);
        devices.children.insert(0, node);
        // Before the share is re-appended, so a new <graphics> keeps its place.
        edit_display(devices, w.console_sock);
        devices.children.retain(|n| {
            !matches!(n, XMLNode::Element(e) if e.name == "filesystem"
                && e.get_child("target").and_then(|t| t.attributes.get("dir")).map(String::as_str) == Some("nvidia"))
        });
        let mut fs = el("filesystem", &[("type", "mount")]);
        fs.children.push(XMLNode::Element(el(
            "driver",
            &[("type", "virtiofs"), ("queue", "1024")],
        )));
        let sock = w.vfs_sock.display().to_string();
        fs.children
            .push(XMLNode::Element(el("source", &[("socket", &sock)])));
        fs.children
            .push(XMLNode::Element(el("target", &[("dir", "nvidia")])));
        devices.children.push(XMLNode::Element(fs));
    }

    if w.windows {
        add_hyperv(&mut root);
    }

    // <qemu:commandline>: drop our old args (if any), append the current ones.
    let pos = root
        .children
        .iter()
        .position(|n| matches!(n, XMLNode::Element(e) if e.name == "commandline" && is_qemu(e)));
    let mut cl = match pos {
        Some(i) => match root.children.remove(i) {
            XMLNode::Element(e) => e,
            _ => unreachable!(),
        },
        None => {
            let mut e = Element::new("commandline");
            e.prefix = Some("qemu".into());
            e.namespace = Some(QEMU_NS.into());
            e
        }
    };
    let args: Vec<Element> = elements(&cl).filter(|e| e.name == "arg").cloned().collect();
    let values: Vec<String> = args
        .iter()
        .map(|a| a.attributes.get("value").cloned().unwrap_or_default())
        .collect();
    let prev = previous_slot(&values);
    let mut keep: Vec<Element> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let next = values.get(i + 1).cloned().unwrap_or_default();
        let ours = (values[i] == "-chardev" || values[i] == "-device") && next.contains(CHARDEV_ID);
        if ours {
            i += 2;
        } else {
            keep.push(args[i].clone());
            i += 1;
        }
    }
    let slot = pick_slot(&root, prev)?;
    let others: Vec<XMLNode> = cl
        .children
        .iter()
        .filter(|n| !matches!(n, XMLNode::Element(e) if e.name == "arg"))
        .cloned()
        .collect();
    cl.children.clear();
    for a in keep {
        cl.children.push(XMLNode::Element(a));
    }
    for v in [
        "-chardev".to_string(),
        virt::gpu_chardev_arg(w.gpu_sock),
        "-device".into(),
        virt::gpu_device_arg(bus, slot),
    ] {
        let mut a = el("arg", &[("value", &v)]);
        a.prefix = Some("qemu".into());
        a.namespace = Some(QEMU_NS.into());
        cl.children.push(XMLNode::Element(a));
    }
    cl.children.extend(others); // <qemu:env> after the args, as libvirt writes them
    root.children.push(XMLNode::Element(cl));

    let mut out = Vec::new();
    root.write_with_config(
        &mut out,
        EmitterConfig::new()
            .perform_indent(true)
            .write_document_declaration(false),
    )?;
    Ok(String::from_utf8(out)?)
}

/// Which libvirt has this domain: -c if given, else the session, else the system.
fn find_domain(name: &str, uri: Option<&str>) -> Result<String> {
    if let Some(u) = uri {
        let v = Virsh::new(u);
        v.reachable()?;
        if !v.exists(name) {
            return Err(oops(
                format!("libvirt ({u}) has no VM called \"{name}\""),
                format!("See its VMs with `virsh -c {u} list --all`"),
            ));
        }
        return Ok(u.into());
    }
    let s = Virsh::new(SESSION);
    s.reachable()?;
    if s.exists(name) {
        return Ok(SESSION.into());
    }
    let y = Virsh::new(SYSTEM);
    if y.reachable().is_ok() && y.exists(name) {
        return Ok(SYSTEM.into());
    }
    Err(oops(
        format!("libvirt has no VM called \"{name}\" (looked in {SESSION} and {SYSTEM})"),
        "See your VMs with `virsh -c qemu:///session list --all` and `virsh -c qemu:///system list --all`",
    ))
}

fn timestamp() -> String {
    sys::output("date", &["+%Y%m%d-%H%M%S"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "backup".into())
}

const AA_ABSTRACTION: &str = "/etc/apparmor.d/abstractions/conduit";
const AA_QEMU_LOCAL: &str = "/etc/apparmor.d/local/abstractions/libvirt-qemu";
const AA_HELPER_LOCAL: &str = "/etc/apparmor.d/local/usr.lib.libvirt.virt-aa-helper";
const AA_BODY: &str = include_str!("../../packaging/common/apparmor/conduit");

/// System domains: libvirt confines each QEMU with a generated AppArmor
/// profile; the Conduit abstraction lets it run the bundled QEMU and reach
/// /run/conduit. (The packages do this on install; a source build may not have.)
fn system_apparmor() -> Result<()> {
    if !Path::new("/sys/kernel/security/apparmor").is_dir()
        || !Path::new("/etc/apparmor.d").is_dir()
    {
        return Ok(());
    }
    let has = |f: &str, s: &str| {
        std::fs::read_to_string(f)
            .map(|t| t.contains(s))
            .unwrap_or(false)
    };
    if Path::new(AA_ABSTRACTION).is_file()
        && has(AA_QEMU_LOCAL, "abstractions/conduit")
        && has(AA_HELPER_LOCAL, "/opt/conduit/")
    {
        return Ok(());
    }
    sys::sudo_ready(
        "Letting the system libvirt's QEMU profile use Conduit's QEMU and sockets (AppArmor).",
    )?;
    let stage = paths::config_dir().join("apparmor-conduit");
    std::fs::create_dir_all(paths::config_dir())?;
    std::fs::write(&stage, AA_BODY)?;
    sys::sudo(
        "install",
        &["-D", "-m644", stage.to_str().unwrap(), AA_ABSTRACTION],
    )?;
    let script = format!(
        "grep -q 'abstractions/conduit' {q} 2>/dev/null || echo 'include if exists <abstractions/conduit> # added by conduit' >> {q}; \
         grep -q '/opt/conduit/' {h} 2>/dev/null || echo '/opt/conduit/** r, # added by conduit' >> {h}; \
         apparmor_parser -r /etc/apparmor.d/usr.lib.libvirt.virt-aa-helper 2>/dev/null || true",
        q = AA_QEMU_LOCAL,
        h = AA_HELPER_LOCAL
    );
    sys::sudo("sh", &["-c", &script])?;
    Ok(())
}

/// What a Windows guest needs after attach, in place of the Linux guest setup.
fn windows_guest_note(name: &str, running: bool) -> String {
    let mut s = format!(
        "{name} is a Windows guest: Conduit's Linux guest setup does not apply, and Hyper-V enlightenments were added.\n\
         Inside the VM, install the Helios driver package (HeliosSetup.exe; see docs/WINDOWS.md, \"Guest driver\")."
    );
    if running {
        s += &format!("\nRestart {name} so the GPU and the enlightenments take effect (shut it down and start it again).");
    }
    s += &format!("\nThen: `conduit view {name} --venus` starts it with the Venus renderer and shows its screen.");
    s
}

pub fn attach(name: &str, dry_run: bool, uri: Option<&str>, guest_later: bool) -> Result<()> {
    crate::vm::check_name(name)?;
    // ---- validate everything first
    if let Some(l) = Link::load(name) {
        if l.kind == Kind::Managed {
            return Err(oops(
                format!("{name} is a VM Conduit made; it has the GPU already"),
                format!("`conduit view {name}` opens it"),
            ));
        }
    } else if paths::vm_dir(name).join("vm.json").is_file() {
        return Err(oops(
            format!("{name} is a VM Conduit made (not a libvirt one)"),
            format!("Make it a libvirt VM with `conduit libvirt enable {name}`"),
        ));
    }
    let uri = find_domain(name, uri)?;
    let v = Virsh::new(&uri);
    let scope = virt::scope_for(&uri)?;
    let xml = v.inactive_xml(name)?;
    let already = virt::is_ours(&xml);
    let running = v.state(name).is_some_and(|s| virt::state_is_up(&s));
    // Decided before anything changes: a Windows guest gets the Hyper-V
    // enlightenments and none of the Linux guest setup.
    let windows = is_windows(&xml) || (running && guest::agent_says_windows(&v, name));
    let emu = virt::emulator()?;
    if matches!(scope, Scope::System { .. })
        && !emu.starts_with("/opt/conduit")
        && !emu.starts_with("/usr")
    {
        return Err(oops(
            format!("the system libvirt cannot run Conduit's QEMU from {}", emu.display()),
            "Install the conduit package (it puts QEMU in /opt/conduit), or keep the VM in the session daemon (qemu:///session)",
        ));
    }
    crate::paths::Tool::Backend.require()?;
    run::need_virtiofsd()?;
    if !dry_run {
        units::check(&scope)?;
    }
    let gpu = units::socket_path(&scope, name, "backend");
    let vfs = units::socket_path(&scope, name, "virtiofsd");
    let console = units::console_path(&scope, name);
    let new_xml = edit_domain(
        &xml,
        &Wiring {
            emulator: &emu,
            gpu_sock: &gpu,
            vfs_sock: &vfs,
            console_sock: &console,
            vm: name,
            windows,
        },
    )?;
    if dry_run {
        ui::info(format!("libvirt: {uri}; emulator: {}", emu.display()));
        if windows {
            ui::info("a Windows guest: Hyper-V enlightenments added, no Linux guest setup");
        }
        println!("{new_xml}");
        ui::info("dry run: nothing was changed");
        return Ok(());
    }
    // The Linux guest bundle (the conduit-guest packages) before anything
    // changes, so a missing package cannot leave the domain half done.
    if !windows {
        guest::bundle(name)?;
    }
    let me = virt::conduit_exe()?;

    // ---- apply
    let dir = paths::vm_dir(name);
    std::fs::create_dir_all(dir.join("logs"))?;
    let old_link = Link::load(name);
    let backup = match old_link.as_ref().and_then(|l| l.backup.clone()).filter(|b| b.is_file()) {
        Some(b) => b,
        None if already => {
            return Err(oops(
                format!("{name} already carries Conduit's GPU, but its original definition is not on record"),
                format!("Remove Conduit's parts from it in virt-manager (the GPU lines under <qemu:commandline>), then run `conduit attach {name}` again"),
            ))
        }
        None => {
            let b = dir.join(format!("libvirt-backup-{}.xml", timestamp()));
            std::fs::write(&b, &xml).with_context(|| format!("writing {}", b.display()))?;
            b
        }
    };
    let undo = |e: anyhow::Error| -> anyhow::Error {
        if old_link.is_none() {
            units::remove(&scope, name);
            let _ = std::fs::remove_file(&backup);
        }
        e
    };
    virt::allow_libvirtd_exec(&emu)?;
    if matches!(scope, Scope::System { .. }) {
        system_apparmor()?;
    }
    units::install(&scope, name, &me).map_err(undo)?;
    if let Err(e) = v.define(&new_xml, name) {
        return Err(undo(oops(
            format!("libvirt refused the changed definition of {name}: {e:#}"),
            format!(
                "Nothing was changed in libvirt (the original is saved at {})",
                backup.display()
            ),
        )));
    }
    Link {
        uri: uri.clone(),
        domain: name.into(),
        kind: Kind::Attached,
        backup: Some(backup.clone()),
        emulator: emu,
        console: Some(console),
    }
    .save(name)?;
    virt::install_desktop_entry(name, &me);
    println!(
        "{} {name} ({uri}). Its original definition is saved at {}",
        if already {
            "Conduit's GPU is up to date in"
        } else {
            "Added Conduit's GPU to"
        },
        backup.display()
    );

    // ---- the guest side
    if windows {
        println!("{}", windows_guest_note(name, running));
        return Ok(());
    }
    let link = Link::load(name).unwrap();
    if guest_later {
        println!(
            "Install the guest side later, inside the VM:\n  {}",
            guest::manual_command(name)
        );
    } else if running && guest::agent_ready(&link) {
        guest::install_via_agent(name, &link)?;
        println!("Installed the guest driver inside {name}.");
    } else {
        println!(
            "Could not install the guest driver automatically ({}).",
            if running {
                "the VM runs no QEMU guest agent"
            } else {
                "the VM is not running"
            }
        );
        println!(
            "Either run this from the host once the VM is up and reachable over ssh:\n  {}",
            guest::manual_command(name)
        );
        println!("or start the VM (with qemu-guest-agent installed in it) and run `conduit attach {name}` again.");
    }
    if running {
        println!("Restart {name} so the GPU appears (shut it down and start it again; a reboot from inside is not enough).");
    }
    println!("Then: start it in virt-manager, and `conduit view {name}` shows its screen.");
    Ok(())
}

pub fn detach(name: &str) -> Result<()> {
    let link = Link::load(name).ok_or_else(|| {
        oops(
            format!("{name} has no Conduit GPU attached (nothing on record)"),
            "See `conduit list` for your VMs",
        )
    })?;
    if link.kind == Kind::Managed {
        return Err(oops(
            format!("{name} is a VM Conduit made"),
            format!("`conduit libvirt disable {name}` removes its libvirt definition"),
        ));
    }
    let backup = link.backup.clone().filter(|b| b.is_file()).ok_or_else(|| {
        oops(
            format!("the original definition of {name} is missing"),
            format!("Expected it under {}", paths::vm_dir(name).display()),
        )
    })?;
    let v = link.virsh();
    v.reachable()?;
    if v.state(&link.domain).is_some_and(|s| virt::state_is_up(&s)) {
        return Err(oops(
            format!("{name} is running"),
            format!("Shut it down first (`conduit down {name}` or in virt-manager), then run this again"),
        ));
    }
    let xml = std::fs::read_to_string(&backup)?;
    v.define(&xml, name).map_err(|e| {
        oops(
            format!("libvirt refused the original definition: {e:#}"),
            "Nothing was changed",
        )
    })?;
    let scope = link.scope()?;
    units::remove(&scope, name);
    virt::remove_desktop_entry(name);
    let _ = std::fs::remove_file(Link::file(name));
    println!(
        "{name} has its original definition back (from {}). Conduit's guest driver stays installed inside it; it does nothing without the GPU.",
        backup.display()
    );
    Ok(())
}

/// Backup files of a VM, oldest first.
#[allow(dead_code)]
pub fn backups(name: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(paths::vm_dir(name))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("libvirt-backup-"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOMAIN: &str = r#"<domain type='kvm'>
  <name>myvm</name>
  <memory unit='KiB'>8388608</memory>
  <memoryBacking>
    <hugepages/>
    <source type='file'/>
  </memoryBacking>
  <os><type arch='x86_64' machine='pc-q35-8.2'>hvm</type></os>
  <cpu mode='host-model' check='partial'><topology sockets='1' cores='4' threads='1'/></cpu>
  <devices>
    <disk type='file' device='disk'><source file='/var/lib/libvirt/images/myvm.qcow2'/>
      <address type='pci' domain='0x0000' bus='0x04' slot='0x00' function='0x0'/></disk>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <controller type='sata'><address type='pci' domain='0x0000' bus='0x00' slot='0x1f' function='0x2'/></controller>
    <video><address type='pci' domain='0x0000' bus='0x00' slot='0x1e' function='0x0'/></video>
  </devices>
</domain>"#;

    const CONSOLE: &str = "/run/user/1000/conduit/myvm/console.sock";

    /// What virt-install / virt-manager write by default (SPICE desktop).
    const VIRT_INSTALL: &str = r#"<domain type='kvm'>
  <name>myvm</name>
  <memory unit='KiB'>4194304</memory>
  <os firmware='efi'><type arch='x86_64' machine='pc-q35-8.2'>hvm</type><boot dev='hd'/></os>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <controller type='usb' index='0' model='qemu-xhci' ports='15'/>
    <controller type='virtio-serial' index='0'/>
    <channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>
    <channel type='spicevmc'><target type='virtio' name='com.redhat.spice.0'/></channel>
    <input type='tablet' bus='usb'/>
    <graphics type='spice' autoport='yes'><listen type='address'/><image compression='off'/><gl enable='no'/></graphics>
    <sound model='ich9'/>
    <audio id='1' type='spice'/>
    <video><model type='qxl' ram='65536' vram='65536' vgamem='16384' heads='1' primary='yes'/></video>
    <redirdev bus='usb' type='spicevmc'/>
    <redirdev bus='usb' type='spicevmc'/>
    <redirfilter><usbdev allow='yes'/></redirfilter>
    <smartcard mode='passthrough' type='spicevmc'/>
    <tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>
  </devices>
</domain>"#;

    fn edit_as(x: &str, windows: bool) -> String {
        edit_domain(
            x,
            &Wiring {
                emulator: Path::new("/opt/conduit/bin/qemu-system-x86_64"),
                gpu_sock: Path::new("/run/user/1000/conduit/myvm/gpu-libvirt.sock"),
                vfs_sock: Path::new("/run/user/1000/conduit/myvm/vfs-libvirt.sock"),
                console_sock: Path::new(CONSOLE),
                vm: "myvm",
                windows,
            },
        )
        .unwrap()
    }

    fn edit(x: &str) -> String {
        edit_as(x, false)
    }

    /// What virt-manager writes for a Windows 11 VM (trimmed).
    const WIN11: &str = r#"<domain type='kvm'>
  <name>win11</name>
  <metadata>
    <libosinfo:libosinfo xmlns:libosinfo="http://libosinfo.org/xmlns/libvirt/domain/1.0">
      <libosinfo:os id="http://microsoft.com/win/11"/>
    </libosinfo:libosinfo>
  </metadata>
  <memory unit='KiB'>16777216</memory>
  <os firmware='efi'><type arch='x86_64' machine='pc-q35-8.2'>hvm</type></os>
  <features>
    <acpi/>
    <apic/>
    <hyperv mode='custom'>
      <relaxed state='on'/>
      <vapic state='on'/>
      <spinlocks state='on' retries='4096'/>
    </hyperv>
    <vmport state='off'/>
  </features>
  <clock offset='localtime'>
    <timer name='rtc' tickpolicy='catchup'/>
    <timer name='pit' tickpolicy='delay'/>
    <timer name='hpet' present='no'/>
  </clock>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <channel type='spicevmc'><target type='virtio' name='com.redhat.spice.0'/></channel>
    <graphics type='spice' autoport='yes'/>
    <video><model type='qxl' heads='1' primary='yes'/></video>
    <tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>
  </devices>
</domain>"#;

    fn hyperv(out: &str) -> Element {
        Element::parse(out.as_bytes())
            .unwrap()
            .get_child("features")
            .unwrap()
            .get_child("hyperv")
            .unwrap()
            .clone()
    }

    #[test]
    fn detects_windows() {
        assert!(is_windows(WIN11));
        // Only the libosinfo id, or only Hyper-V features, is enough.
        let no_hv = WIN11
            .replace("<hyperv mode='custom'>", "<!--")
            .replace("</hyperv>", "-->");
        assert!(is_windows(&no_hv), "{no_hv}");
        let no_osinfo = WIN11.replace("microsoft.com/win/11", "ubuntu.com/ubuntu/24.04");
        assert!(is_windows(&no_osinfo));
        let linux = no_osinfo
            .replace("<hyperv mode='custom'>", "<!--")
            .replace("</hyperv>", "-->");
        assert!(!is_windows(&linux), "{linux}");
        assert!(!is_windows(DOMAIN));
        assert!(!is_windows(VIRT_INSTALL));
        assert!(!is_windows("not xml"));
        // Linux guests attach without enlightenments, and stay Linux.
        assert!(!is_windows(&edit(VIRT_INSTALL)));
    }

    #[test]
    fn windows_gets_the_hyperv_enlightenments() {
        let out = edit_as(WIN11, true);
        let hv = hyperv(&out);
        assert_eq!(hv.attributes["mode"], "custom");
        for n in [
            "relaxed",
            "vapic",
            "spinlocks",
            "vpindex",
            "runtime",
            "synic",
            "stimer",
            "reset",
            "frequencies",
            "tlbflush",
            "ipi",
        ] {
            let e = all(&hv, n);
            assert_eq!(e.len(), 1, "{n}: {out}");
            assert_eq!(e[0].attributes["state"], "on", "{n}");
        }
        assert_eq!(
            all(&hv, "spinlocks")[0].attributes["retries"],
            "4096",
            "an existing setting is kept"
        );
        let stimer = all(&hv, "stimer")[0];
        assert_eq!(
            stimer.get_child("direct").unwrap().attributes["state"],
            "on"
        );
        let root = Element::parse(out.as_bytes()).unwrap();
        let clock = root.get_child("clock").unwrap();
        assert_eq!(clock.attributes["offset"], "localtime");
        let timers: Vec<_> = all(clock, "timer");
        assert_eq!(timers.len(), 4, "rtc, pit and hpet are kept: {out}");
        let hvc: Vec<_> = timers
            .iter()
            .filter(|t| t.attributes["name"] == "hypervclock")
            .collect();
        assert_eq!(hvc.len(), 1);
        assert_eq!(hvc[0].attributes["present"], "yes");
        let features = root.get_child("features").unwrap();
        assert!(features.get_child("acpi").is_some() && features.get_child("vmport").is_some());
        // The rest of attach's edit applies as for Linux guests.
        assert!(virt::is_ours(&out));
        assert!(!out.contains("spice"), "{out}");
        assert_eq!(edit_as(&out, true), out, "idempotent");
    }

    #[test]
    fn hyperv_respects_what_the_domain_sets() {
        // No <features>/<clock> at all: both are created.
        let bare = edit_as(DOMAIN, true);
        assert_eq!(all(&hyperv(&bare), "stimer").len(), 1, "{bare}");
        assert_eq!(
            all(&hyperv(&bare), "spinlocks")[0].attributes["retries"],
            "8191"
        );
        let root = Element::parse(bare.as_bytes()).unwrap();
        assert_eq!(
            root.get_child("clock").unwrap().attributes["offset"],
            "localtime"
        );
        // A setting turned off stays off, and what depends on it is not added.
        let off = WIN11.replace(
            "<relaxed state='on'/>",
            "<relaxed state='off'/><synic state='off'/><stimer state='on'/>",
        );
        let out = edit_as(&off, true);
        let hv = hyperv(&out);
        assert_eq!(all(&hv, "relaxed")[0].attributes["state"], "off");
        assert_eq!(all(&hv, "synic")[0].attributes["state"], "off");
        assert!(
            all(&hv, "stimer")[0].get_child("direct").is_none(),
            "stimer is the user's (QEMU refuses it without synic anyway): {out}"
        );
        assert_eq!(all(&hv, "vpindex").len(), 1);
        // An existing hypervclock timer is kept as it is.
        let clk = WIN11.replace(
            "<timer name='hpet' present='no'/>",
            "<timer name='hypervclock' present='no'/>",
        );
        let out = edit_as(&clk, true);
        assert!(out.contains("name=\"hypervclock\" present=\"no\""), "{out}");
        assert_eq!(out.matches("hypervclock").count(), 1);
        // Passthrough mode already exposes everything.
        let pt = WIN11.replace("<hyperv mode='custom'>", "<hyperv mode='passthrough'>");
        assert_eq!(elements(&hyperv(&edit_as(&pt, true))).count(), 3);
    }

    #[test]
    fn windows_note_points_at_the_helios_package() {
        let n = windows_guest_note("win11", true);
        assert!(
            n.contains("HeliosSetup.exe") && n.contains("docs/WINDOWS.md"),
            "{n}"
        );
        assert!(n.contains("conduit view win11 --venus"), "{n}");
        assert!(n.contains("Restart win11"));
        assert!(!windows_guest_note("win11", false).contains("Restart"));
    }

    #[test]
    fn adds_everything() {
        let out = edit(DOMAIN);
        let root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_child("devices").unwrap();
        let first = dev.children.iter().find_map(|n| n.as_element()).unwrap();
        assert_eq!(first.name, "emulator", "emulator stays first");
        assert_eq!(
            first.get_text().unwrap(),
            "/opt/conduit/bin/qemu-system-x86_64"
        );
        let mb = root.get_child("memoryBacking").unwrap();
        assert!(
            mb.get_child("hugepages").is_some(),
            "other settings are kept"
        );
        assert_eq!(mb.get_child("source").unwrap().attributes["type"], "memfd");
        assert_eq!(mb.get_child("access").unwrap().attributes["mode"], "shared");
        let cpu = root.get_child("cpu").unwrap();
        assert_eq!(cpu.attributes["mode"], "host-passthrough");
        assert!(cpu.get_child("topology").is_some(), "topology kept");
        assert_eq!(
            cpu.get_child("maxphysaddr").unwrap().attributes["mode"],
            "passthrough"
        );
        let fs = dev.get_child("filesystem").unwrap();
        assert_eq!(fs.get_child("target").unwrap().attributes["dir"], "nvidia");
        assert_eq!(
            fs.get_child("source").unwrap().attributes["socket"],
            "/run/user/1000/conduit/myvm/vfs-libvirt.sock"
        );
        assert!(virt::is_ours(&out), "{out}");
        assert!(out.contains(
            "value=\"socket,id=conduit-gpu,path=/run/user/1000/conduit/myvm/gpu-libvirt.sock\""
        ));
        // 0x1e and 0x1f are taken: the next free slot down is 0x1d.
        assert!(
            out.contains("vhost-user-test-device-pci,chardev=conduit-gpu,virtio-id=45,class=0x0380,num_vqs=3,vq_size=256,config_size=4036,bus=pcie.0,addr=0x1d"),
            "{out}"
        );
        assert!(out.contains("machine=\"q35\""), "{out}");
        assert!(out.contains("myvm.qcow2"));
    }

    #[test]
    fn idempotent_and_keeps_foreign_args() {
        let with_other = DOMAIN.replace(
            "</domain>",
            &format!("<qemu:commandline xmlns:qemu='{QEMU_NS}'><qemu:arg value='-s'/><qemu:env name='A' value='b'/></qemu:commandline></domain>"),
        );
        let once = edit(&with_other);
        let twice = edit(&once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("-chardev").count(), 1);
        assert_eq!(twice.matches("<qemu:commandline").count(), 1);
        assert_eq!(twice.matches("conduit:vm").count(), 1);
        assert_eq!(twice.matches("<filesystem").count(), 1);
        assert!(
            twice.contains("value=\"-s\""),
            "user's own args are kept: {twice}"
        );
        assert!(twice.contains("name=\"A\""), "qemu:env kept: {twice}");
    }

    #[test]
    fn i440fx_uses_pci0_and_missing_sections_are_created() {
        let out = edit("<domain type='kvm'><name>x</name><os><type machine='pc-i440fx-8.2'>hvm</type></os></domain>");
        assert!(out.contains("bus=pci.0,addr=0x1e"), "{out}");
        let root = Element::parse(out.as_bytes()).unwrap();
        assert!(root
            .get_child("devices")
            .unwrap()
            .get_child("emulator")
            .is_some());
        assert!(root.get_child("memoryBacking").is_some());
    }

    fn devices(out: &str) -> Element {
        Element::parse(out.as_bytes())
            .unwrap()
            .get_child("devices")
            .unwrap()
            .clone()
    }

    fn all<'a>(dev: &'a Element, name: &'a str) -> Vec<&'a Element> {
        elements(dev).filter(|e| e.name == name).collect()
    }

    #[test]
    fn spice_becomes_the_boot_console() {
        let out = edit(VIRT_INSTALL);
        assert!(!out.contains("spice"), "{out}");
        let dev = devices(&out);
        let g = all(&dev, "graphics");
        assert_eq!(g.len(), 1, "{out}");
        assert_eq!(g[0].attributes["type"], "vnc");
        assert_eq!(g[0].attributes["socket"], CONSOLE);
        assert!(g[0].children.is_empty(), "no listen/gl children: {out}");
        assert!(all(&dev, "redirdev").is_empty());
        assert!(all(&dev, "redirfilter").is_empty());
        assert!(all(&dev, "smartcard").is_empty());
        let ch = all(&dev, "channel");
        assert_eq!(ch.len(), 1, "the guest agent channel stays: {out}");
        assert_eq!(ch[0].attributes["type"], "unix");
        assert_eq!(all(&dev, "audio")[0].attributes["type"], "none");
        assert_eq!(all(&dev, "sound").len(), 1, "the sound card stays");
        let m = all(&dev, "video")[0].get_child("model").unwrap();
        assert_eq!(m.attributes["type"], "virtio", "no QXL without SPICE");
        assert_eq!(m.attributes["heads"], "1");
        assert!(m.attributes.get("vram").is_none(), "{out}");
        assert_eq!(all(&dev, "tpm").len(), 1, "the TPM stays");
        assert_eq!(all(&dev, "input").len(), 1, "the tablet stays");
        assert_eq!(edit(&out), out, "idempotent");
    }

    #[test]
    fn existing_vnc_moves_to_the_console_socket() {
        let x = DOMAIN.replace(
            "<video>",
            "<graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1'><listen type='address' address='127.0.0.1'/></graphics>\
             <audio id='1' type='none'/>\
             <video><model type='virtio' heads='1' primary='yes'><acceleration accel3d='yes'/></model></video><video>",
        );
        let out = edit(&x);
        let dev = devices(&out);
        let g = all(&dev, "graphics");
        assert_eq!(g.len(), 1, "{out}");
        assert_eq!(g[0].attributes.len(), 2, "{out}");
        assert_eq!(g[0].attributes["socket"], CONSOLE);
        assert!(!out.contains("127.0.0.1"), "{out}");
        assert!(!out.contains("accel3d"), "no OpenGL: {out}");
        assert_eq!(all(&dev, "audio")[0].attributes["type"], "none");
        assert_eq!(all(&dev, "video").len(), 2);
        assert_eq!(edit(&out), out);
    }

    #[test]
    fn rejects_non_domain() {
        let w = Wiring {
            emulator: Path::new("/q"),
            gpu_sock: Path::new("/s"),
            vfs_sock: Path::new("/v"),
            console_sock: Path::new("/c"),
            vm: "x",
            windows: false,
        };
        assert!(edit_domain("<network/>", &w).is_err());
        assert!(edit_domain("not xml", &w).is_err());
    }
}
