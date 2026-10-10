//! `conduit attach` / `conduit detach`: give an existing libvirt (virt-manager)
//! VM Conduit's GPU, and take it away again.
//!
//! attach, all validated before anything changes:
//!   1. backs the domain's definition up to vms/NAME/libvirt-backup-TIME.xml
//!      (only the first time, mode 0600: graphics passwords),
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
//! Running it again re-applies the same edit (nothing changes). detach saves
//! the current definition (libvirt-pre-detach-TIME.xml), takes Conduit's
//! parts out of it ([`undo_domain`]) with what attach overwrote restored from
//! the backup, and removes the units.

use crate::bios;
use crate::guest;
use crate::paths;
use crate::run;
use crate::shares::{self, Wire};
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
    /// QEMU's end of the stats channel (`org.conduit.stats.0`) binds here.
    pub stats_sock: &'a Path,
    /// QEMU's end of the control channel (`org.conduit.ctl.0`) binds here.
    pub ctl_sock: &'a Path,
    pub vm: &'a str,
    /// The VM's shared folders (virtiofs tags `conduit-*`).
    pub shares: &'a [Wire],
    /// A Windows guest ([`is_windows`]): add the Hyper-V enlightenments.
    pub windows: bool,
    /// The installed Conduit BIOS (bios.rs), and whether its image may stand
    /// in for a given stock loader (`Err` says why not).
    pub bios: Option<&'a Path>,
    pub bios_fits: &'a dyn Fn(&str, &Path, bool) -> std::result::Result<(), String>,
    /// The display mode the boot console starts in (the video device's
    /// preferred mode, which the firmware and the Windows boot screens use).
    pub display: Option<(u32, u32)>,
    /// The machine types Conduit's QEMU has (`-machine help`), when known.
    pub machines: Option<&'a [String]>,
}

/// The metadata attribute that remembers the stock loader while the Conduit
/// BIOS stands in for it.
const STOCK_LOADER_ATTR: &str = "stock-loader";

/// What happens to the domain's <loader> (bios.rs decides).
pub fn loader_plan(xml: &str, w: &Wiring) -> bios::Plan {
    let Ok(root) = Element::parse(xml.as_bytes()) else {
        return bios::Plan {
            loader: None,
            stock: None,
            note: None,
        };
    };
    let os = root.get_child("os");
    let loader = os.and_then(|o| o.get_child("loader"));
    let path = loader
        .and_then(|l| l.get_text())
        .map(|t| t.trim().to_string());
    let pflash = loader
        .and_then(|l| l.attributes.get("type"))
        .map(String::as_str)
        == Some("pflash");
    let autoselect = os
        .and_then(|o| o.attributes.get("firmware"))
        .map(String::as_str)
        == Some("efi");
    let recorded = root
        .get_child("metadata")
        .and_then(|m| {
            m.children.iter().find_map(|n| match n {
                XMLNode::Element(e)
                    if e.name == "vm"
                        && (e.namespace.as_deref() == Some(META_NS)
                            || e.prefix.as_deref() == Some("conduit")) =>
                {
                    Some(e)
                }
                _ => None,
            })
        })
        .and_then(|v| v.attributes.get(STOCK_LOADER_ATTR).cloned());
    let fits = |stock: &str, img: &Path| (w.bios_fits)(stock, img, autoselect);
    bios::plan(path.as_deref(), pflash, recorded.as_deref(), w.bios, &fits)
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
/// not added (QEMU would refuse to start). stimer also needs hv-time, the
/// Hyper-V reference clock (`<timer name='hypervclock'>`, [`add_hyperv`]).
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
    // hv-stimer needs hv-time: with the Hyper-V clock turned off QEMU refuses
    // to start ("Hyper-V synthetic timers (hv-stimer) requires Hyper-V clock
    // source (hv-time)").
    let no_time = root.get_child("clock").is_some_and(|c| {
        elements(c).any(|t| {
            t.name == "timer"
                && t.attributes.get("name").map(String::as_str) == Some("hypervclock")
                && t.attributes.get("present").map(String::as_str) == Some("no")
        })
    });
    let hv = child_mut(child_mut(root, "features"), "hyperv");
    if hv.attributes.get("mode").map(String::as_str) != Some("passthrough") {
        for (name, needs) in HYPERV {
            if needs.is_some_and(|n| hv.get_child(n).is_some_and(is_off)) {
                continue;
            }
            if *name == "stimer" && no_time {
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
/// chardev id of the QMP monitor the backend sends relative pointer motion
/// through (`input-send-event`).
const QMP_ID: &str = "conduit-qmp";
/// id of the USB tablet once it is Conduit's (see [`take_usb_tablet`]).
const TABLET_ID: &str = "conduit-tablet";

/// A QMP monitor next to the console socket. Under a viewer grab the backend
/// moves QEMU's relative (PS/2) mouse through it: a first-person game reads
/// raw relative motion, and VNC carries only absolute positions to the
/// tablet. QEMU removes a stale socket file itself.
fn qmp_args(console: &Path) -> [String; 4] {
    let sock = console.with_file_name("qmp.sock");
    [
        "-chardev".into(),
        format!(
            "socket,id={QMP_ID},path={},server=on,wait=off",
            sock.display()
        ),
        "-mon".into(),
        format!("chardev={QMP_ID},mode=control"),
    ]
}

/// The USB tablet, bound to the VGA's console (`display=video0`): the VNC
/// server (which uses that console) reaches it first, and QMP input with no
/// device reaches only the unbound ones -- the PS/2 mouse gets the backend's
/// relative motion *and* its buttons, so a click under a grab never jumps the
/// pointer back to the tablet's last position. On the command line, after
/// libvirt's devices, because the binding needs the VGA to exist already
/// (QEMU skips it silently otherwise) -- and in JSON form: QEMU creates every
/// `key=value` -device before any JSON one, and libvirt writes its own in
/// JSON, so a `key=value` tablet came before the xhci controller and failed
/// with "Bus 'usb.0' not found" (QEMU 11.1, 2026-10-08).
fn tablet_arg() -> String {
    format!(
        r#"{{"driver":"usb-tablet","id":"{TABLET_ID}","bus":"usb.0","port":"1","display":"video0"}}"#
    )
}

/// Take libvirt's `<input type='tablet' bus='usb'>` out (its place is the
/// command line, [`tablet_arg`]); `true` if there was one.
fn take_usb_tablet(devices: &mut Element) -> bool {
    let before = devices.children.len();
    devices.children.retain(|n| {
        !matches!(n, XMLNode::Element(e) if e.name == "input"
            && e.attributes.get("type").map(String::as_str) == Some("tablet")
            && e.attributes.get("bus").map(String::as_str) == Some("usb"))
    });
    devices.children.len() != before
}

/// The video device's preferred mode: QEMU's xres/yres, which its EDID (stdvga,
/// bochs) or display info (virtio) report, so the firmware, the boot logo and
/// Windows' boot screens start in the VM's native mode. stdvga/bochs also get
/// enough video memory for one frame of it.
///
/// A mode 4096 pixels or more wide or high (5120x1440) does not fit EDID's
/// detailed timing descriptor; QEMU puts it in a DisplayID extension. The
/// Conduit BIOS reads that (packaging/bios/patches/0001), and so do the guest
/// kernels' and Windows' display drivers; stock OVMF does not and starts in
/// 1280x800, as it would without the element. A libvirt without <resolution>
/// support refuses the definition, and attach retries without it
/// ([`without_resolution`]).
fn set_resolution(model: &mut Element, (x, y): (u32, u32)) {
    let ty = model.attributes.get("type").cloned().unwrap_or_default();
    if !matches!(ty.as_str(), "vga" | "bochs" | "virtio") {
        return;
    }
    model
        .children
        .retain(|c| !matches!(c, XMLNode::Element(r) if r.name == "resolution"));
    model.children.push(XMLNode::Element(el(
        "resolution",
        &[("x", &x.to_string()), ("y", &y.to_string())],
    )));
    if ty != "virtio" {
        let mib = (u64::from(x) * u64::from(y) * 4)
            .div_ceil(1 << 20)
            .next_power_of_two()
            .max(16);
        let have: u64 = model
            .attributes
            .get("vram")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if have < mib * 1024 {
            model
                .attributes
                .insert("vram".into(), (mib * 1024).to_string());
        }
    }
}

fn edit_display(devices: &mut Element, console: &Path, display: Option<(u32, u32)>) {
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
                    if let Some(d) = display {
                        set_resolution(m, d);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The `<address>` libvirt assigned a device (its virtio-serial port, PCI
/// slot), kept when Conduit writes the device again: a moved port or slot is
/// new hardware to the guest.
fn address_of(e: &Element) -> Option<XMLNode> {
    e.get_child("address").cloned().map(XMLNode::Element)
}

/// A Conduit channel: a virtio-serial port named `name` whose host end is a
/// unix socket QEMU binds (the stats feed [`conduit_stats::CHANNEL`], the
/// control channel [`conduit_ctl::CHANNEL`]), plus a virtio-serial controller
/// when the domain has none.
fn add_channel(devices: &mut Element, name: &str, sock: &Path) {
    let ours = |n: &XMLNode| {
        matches!(n, XMLNode::Element(e) if e.name == "channel"
            && e.get_child("target").and_then(|t| t.attributes.get("name")).map(String::as_str)
                == Some(name))
    };
    // Where our channel was, so a second edit changes nothing.
    let at = devices.children.iter().position(ours);
    let addr = at.and_then(|i| devices.children[i].as_element().and_then(address_of));
    devices.children.retain(|n| !ours(n));
    let has_ctl = elements(devices).any(|e| {
        e.name == "controller"
            && e.attributes.get("type").map(String::as_str) == Some("virtio-serial")
    });
    let mut ch = el("channel", &[("type", "unix")]);
    let path = sock.display().to_string();
    ch.children.push(XMLNode::Element(el(
        "source",
        &[("mode", "bind"), ("path", &path)],
    )));
    ch.children.push(XMLNode::Element(el(
        "target",
        &[("type", "virtio"), ("name", name)],
    )));
    ch.children.extend(addr);
    let at = at.unwrap_or(devices.children.len());
    devices.children.insert(at, XMLNode::Element(ch));
    if !has_ctl {
        devices.children.insert(
            at,
            XMLNode::Element(el(
                "controller",
                &[("type", "virtio-serial"), ("index", "0")],
            )),
        );
    }
}

fn domain_machine(root: &Element) -> String {
    root.get_child("os")
        .and_then(|o| o.get_child("type"))
        .and_then(|t| t.attributes.get("machine").cloned())
        .unwrap_or_default()
}

fn is_q35(machine: &str) -> bool {
    machine.contains("q35") || machine.is_empty()
}

/// The machine type on Conduit's QEMU: the domain's own when that QEMU has it
/// (a pinned versioned type such as pc-q35-8.2 keeps the guest's hardware as
/// it was), else the plain alias of the same family (Ubuntu's "pc-q35-noble"
/// exists only in Ubuntu's QEMU). `machines` is `-machine help` of Conduit's
/// QEMU; unknown, only the aliases are sure to exist.
fn pick_machine(cur: &str, machines: Option<&[String]>) -> String {
    let alias = if is_q35(cur) { "q35" } else { "pc" };
    if cur == alias || machines.is_some_and(|m| !cur.is_empty() && m.iter().any(|x| x == cur)) {
        cur.to_string()
    } else {
        alias.to_string()
    }
}

/// What attach tells the user about its edit of `xml`, besides the GPU.
pub fn notes(xml: &str, w: &Wiring) -> Vec<String> {
    let Ok(root) = Element::parse(xml.as_bytes()) else {
        return Vec::new();
    };
    let mut v = Vec::new();
    let old = domain_machine(&root);
    let new = pick_machine(&old, w.machines);
    if old != new && !old.is_empty() {
        v.push(format!(
            "Machine type {old} becomes {new}: Conduit's QEMU has no {old}."
        ));
    }
    let private = root
        .get_child("cpu")
        .and_then(|c| c.get_child("numa"))
        .is_some_and(|n| {
            elements(n).any(|c| {
                c.name == "cell"
                    && c.attributes.get("memAccess").map(String::as_str) == Some("private")
            })
        });
    if private {
        v.push(
            "NUMA cells with memAccess='private' become shared: the GPU backend maps the guest's memory."
                .into(),
        );
    }
    let plan = loader_plan(xml, w);
    if let Some(n) = plan.note {
        v.push(n);
    }
    if plan.loader.is_some() && has_tpm(&root) {
        v.push(bios::TPM_WARNING.into());
    }
    v
}

fn has_tpm(root: &Element) -> bool {
    root.get_child("devices")
        .is_some_and(|d| elements(d).any(|e| e.name == "tpm"))
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

    let machine = domain_machine(&root);
    let bus = if is_q35(&machine) { "pcie.0" } else { "pci.0" };
    let new_machine = pick_machine(&machine, w.machines);
    if let Some(t) = root
        .get_mut_child("os")
        .and_then(|o| o.get_mut_child("type"))
    {
        t.attributes.insert("machine".into(), new_machine);
    }

    // <os><loader>: the Conduit BIOS in place of the matching stock firmware.
    let bios_plan = loader_plan(xml, w);
    if let Some(l) = &bios_plan.loader {
        if let Some(e) = root
            .get_mut_child("os")
            .and_then(|o| o.get_mut_child("loader"))
        {
            e.children = vec![XMLNode::Text(l.clone())];
        }
    }

    // <metadata><conduit:vm .../>
    {
        let md = child_mut(&mut root, "metadata");
        md.children.retain(|n| {
            !matches!(n, XMLNode::Element(e) if e.namespace.as_deref() == Some(META_NS) || e.prefix.as_deref() == Some("conduit"))
        });
        let mut v = el("vm", &[("name", w.vm), ("kind", "attached")]);
        if let Some(stock) = &bios_plan.stock {
            v.attributes.insert(STOCK_LOADER_ATTR.into(), stock.clone());
        }
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
    // GPU's 64-bit shared-memory BAR needs the host's address width.
    // Everything else the VM's <cpu> had (topology, features such as
    // topoext, cache, numa) is kept.
    {
        let old = root.get_child("cpu").cloned();
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
        if let Some(o) = old {
            // A host-model or custom CPU's <model>/<vendor> mean nothing in
            // host-passthrough; drop them and any old <maxphysaddr>.
            cpu.children.extend(o.children.into_iter().filter(|n| {
                !matches!(n, XMLNode::Element(e)
                    if matches!(e.name.as_str(), "maxphysaddr" | "model" | "vendor"))
            }));
        }
        cpu.children.push(XMLNode::Element(el(
            "maxphysaddr",
            &[("mode", "passthrough")],
        )));
        // The GPU maps guest RAM from the backend: every NUMA cell must be
        // shared memory too (a private cell overrides <access mode='shared'>).
        for numa in cpu.children.iter_mut().filter_map(|n| match n {
            XMLNode::Element(e) if e.name == "numa" => Some(e),
            _ => None,
        }) {
            for cell in numa.children.iter_mut().filter_map(|n| match n {
                XMLNode::Element(e) if e.name == "cell" => Some(e),
                _ => None,
            }) {
                if cell.attributes.get("memAccess").map(String::as_str) == Some("private") {
                    cell.attributes.insert("memAccess".into(), "shared".into());
                }
            }
        }
        // libvirt wants <cpu> after <features>; it reorders on define anyway.
        root.children.push(XMLNode::Element(cpu));
    }

    // <devices>: emulator first; the NVIDIA share (virtiofs tag "nvidia").
    let moved_tablet;
    let has_video;
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
        edit_display(devices, w.console_sock, w.display);
        moved_tablet = take_usb_tablet(devices);
        has_video = elements(devices).any(|e| e.name == "video");
        let is_nvidia = |e: &Element| {
            e.name == "filesystem"
                && e.get_child("target")
                    .and_then(|t| t.attributes.get("dir"))
                    .map(String::as_str)
                    == Some("nvidia")
        };
        let nvidia_addr = elements(devices)
            .find(|e| is_nvidia(e))
            .and_then(address_of);
        devices
            .children
            .retain(|n| !matches!(n, XMLNode::Element(e) if is_nvidia(e)));
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
        fs.children.extend(nvidia_addr);
        // Before the share is re-appended: each edit leaves the order as it was.
        add_channel(devices, conduit_stats::CHANNEL, w.stats_sock);
        add_channel(devices, conduit_ctl::CHANNEL, w.ctl_sock);
        devices.children.push(XMLNode::Element(fs));
        set_shares(devices, w.shares);
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
    // The tablet moves to the command line once and stays there.
    let tablet = has_video && (moved_tablet || values.iter().any(|v| v.contains(TABLET_ID)));
    let keep = foreign_args(&args, &values);
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
    let mut ours = vec![
        "-chardev".to_string(),
        virt::gpu_chardev_arg(w.gpu_sock),
        "-device".into(),
        virt::gpu_device_arg(bus, slot),
    ];
    ours.extend(qmp_args(w.console_sock));
    if tablet {
        ours.extend(["-device".to_string(), tablet_arg()]);
    }
    for v in ours {
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

/// The `<qemu:arg>`s that are not Conduit's (the GPU, QMP and tablet pairs).
fn foreign_args(args: &[Element], values: &[String]) -> Vec<Element> {
    let mut keep: Vec<Element> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let next = values.get(i + 1).cloned().unwrap_or_default();
        let ours = match values[i].as_str() {
            "-chardev" => next.contains(CHARDEV_ID) || next.contains(&format!("id={QMP_ID},")),
            "-device" => {
                next.contains(CHARDEV_ID)
                    || next.contains(&format!("id={TABLET_ID},"))
                    || next.contains(&format!(r#""id":"{TABLET_ID}""#))
            }
            "-mon" => next.contains(&format!("chardev={QMP_ID},")),
            _ => false,
        };
        if ours {
            i += 2;
        } else {
            keep.push(args[i].clone());
            i += 1;
        }
    }
    keep
}

/// The `<filesystem>` element of one shared folder.
fn share_fs(w: &Wire) -> Element {
    let mut fs = el("filesystem", &[("type", "mount")]);
    fs.children.push(XMLNode::Element(el(
        "driver",
        &[("type", "virtiofs"), ("queue", "1024")],
    )));
    let sock = w.sock.display().to_string();
    fs.children
        .push(XMLNode::Element(el("source", &[("socket", &sock)])));
    fs.children
        .push(XMLNode::Element(el("target", &[("dir", &w.tag)])));
    fs
}

/// Make `<devices>` carry exactly these shared folders: Conduit's
/// (`conduit-*` targets) are replaced, other filesystems stay as they are.
fn set_shares(devices: &mut Element, shares: &[Wire]) {
    let tag = |e: &Element| {
        (e.name == "filesystem")
            .then(|| e.get_child("target")?.attributes.get("dir").cloned())
            .flatten()
            .filter(|d| shares::is_share_tag(d))
    };
    // A share that stays keeps its address.
    let addrs: Vec<(String, XMLNode)> = elements(devices)
        .filter_map(|e| Some((tag(e)?, address_of(e)?)))
        .collect();
    devices
        .children
        .retain(|n| !matches!(n, XMLNode::Element(e) if tag(e).is_some()));
    for w in shares {
        let mut fs = share_fs(w);
        fs.children.extend(
            addrs
                .iter()
                .find(|(t, _)| *t == w.tag)
                .map(|(_, a)| a.clone()),
        );
        devices.children.push(XMLNode::Element(fs));
    }
}

/// A domain definition with its shared folders set to `shares`, nothing else changed.
pub fn apply_shares(xml: &str, shares: &[Wire]) -> Result<String> {
    let mut root =
        Element::parse(xml.as_bytes()).context("libvirt returned XML Conduit cannot read")?;
    if root.name != "domain" {
        anyhow::bail!("not a libvirt domain (root element is <{}>)", root.name);
    }
    set_shares(child_mut(&mut root, "devices"), shares);
    let mut out = Vec::new();
    root.write_with_config(
        &mut out,
        EmitterConfig::new()
            .perform_indent(true)
            .write_document_declaration(false),
    )?;
    Ok(String::from_utf8(out)?)
}

fn parse_domain(xml: &str) -> Result<Element> {
    let root =
        Element::parse(xml.as_bytes()).context("libvirt returned XML Conduit cannot read")?;
    if root.name != "domain" {
        anyhow::bail!("not a libvirt domain (root element is <{}>)", root.name);
    }
    Ok(root)
}

fn emit(root: &Element) -> Result<String> {
    let mut out = Vec::new();
    root.write_with_config(
        &mut out,
        EmitterConfig::new()
            .perform_indent(true)
            .write_document_declaration(false),
    )?;
    Ok(String::from_utf8(out)?)
}

fn is_conduit_meta(n: &XMLNode) -> bool {
    matches!(n, XMLNode::Element(e) if e.namespace.as_deref() == Some(META_NS) || e.prefix.as_deref() == Some("conduit"))
}

fn text(e: &Element) -> String {
    e.get_text()
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

fn attr<'a>(e: &'a Element, k: &str) -> Option<&'a str> {
    e.attributes.get(k).map(String::as_str)
}

fn remove_empty(root: &mut Element, name: &str) {
    root.children.retain(|n| {
        !matches!(n, XMLNode::Element(e) if e.name == name && e.attributes.is_empty()
            && !e.children.iter().any(|c| c.as_element().is_some()))
    });
}

/// The stock loader recorded in Conduit's metadata, if any.
fn recorded_stock(root: &Element) -> Option<String> {
    root.get_child("metadata")?
        .children
        .iter()
        .find(|n| is_conduit_meta(n))?
        .as_element()?
        .attributes
        .get(STOCK_LOADER_ATTR)
        .cloned()
}

/// Machine types in `qemu-system-x86_64 -machine help` output.
fn parse_machines(help: &str) -> Vec<String> {
    help.lines()
        .filter(|l| !l.starts_with("Supported machines"))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// The machine types Conduit's QEMU has, cached per emulator build
/// (~/.cache/conduit/qemu-machines, keyed by path, size and mtime).
pub fn qemu_machines(emu: &Path) -> Option<Vec<String>> {
    let meta = std::fs::metadata(emu).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("{} {} {mtime}", emu.display(), meta.len());
    let cache = paths::cache_dir().join("qemu-machines");
    if let Ok(s) = std::fs::read_to_string(&cache) {
        if let Some(rest) = s.strip_prefix(&format!("{key}\n")) {
            return Some(rest.lines().map(str::to_string).collect());
        }
    }
    let help = sys::output(emu.to_str()?, &["-machine", "help"]).ok()?;
    let list = parse_machines(&help);
    if list.is_empty() {
        return None;
    }
    let _ = std::fs::create_dir_all(paths::cache_dir());
    let _ = std::fs::write(&cache, format!("{key}\n{}\n", list.join("\n")));
    Some(list)
}

/// The definition without any video <resolution>, for a libvirt that does
/// not know the element ([`set_resolution`]).
pub fn without_resolution(xml: &str) -> Result<String> {
    let mut root = parse_domain(xml)?;
    if let Some(d) = root.get_mut_child("devices") {
        for v in d.children.iter_mut().filter_map(|n| match n {
            XMLNode::Element(e) if e.name == "video" => Some(e),
            _ => None,
        }) {
            if let Some(m) = v.get_mut_child("model") {
                m.children
                    .retain(|c| !matches!(c, XMLNode::Element(r) if r.name == "resolution"));
            }
        }
    }
    emit(&root)
}

/// Did libvirt refuse the definition over the video <resolution> (an older
/// libvirt's schema: "Element model has extra content: resolution")?
pub fn refused_resolution(err: &str) -> bool {
    err.contains("resolution")
}

/// A domain on a Conduit BIOS image that is gone (the package removed): the
/// definition back on the stock loader, and that loader. `exists` checks a
/// file; the stock loader is the recorded one ([`bios::stock_for`]).
pub fn restore_missing_bios(xml: &str, exists: &dyn Fn(&str) -> bool) -> Option<(String, String)> {
    let mut root = parse_domain(xml).ok()?;
    let cur = text(root.get_child("os")?.get_child("loader")?);
    if !bios::is_ours(&cur) || exists(&cur) {
        return None;
    }
    let stock = bios::stock_for(&cur, recorded_stock(&root).as_deref());
    let loader = root.get_mut_child("os")?.get_mut_child("loader")?;
    loader.children = vec![XMLNode::Text(stock.clone())];
    if let Some(md) = root.get_mut_child("metadata") {
        for n in md.children.iter_mut() {
            if is_conduit_meta(n) {
                if let XMLNode::Element(e) = n {
                    e.attributes.shift_remove(STOCK_LOADER_ATTR);
                }
            }
        }
    }
    Some((emit(&root).ok()?, stock))
}

/// Before `conduit up`/`view` start an attached VM: if its Conduit BIOS
/// image is gone, put it back on the stock firmware so it boots.
pub fn ensure_firmware(name: &str, link: &Link) -> Result<()> {
    if link.kind != Kind::Attached {
        return Ok(());
    }
    let v = link.virsh();
    let Ok(xml) = v.inactive_xml(&link.domain) else {
        return Ok(());
    };
    let Some((new, stock)) = restore_missing_bios(&xml, &|p| Path::new(p).is_file()) else {
        return Ok(());
    };
    v.define(&new, name).map_err(|e| {
        oops(
            format!("{name} boots the Conduit BIOS, which is no longer installed, and libvirt refused the stock firmware: {e:#}"),
            format!("Run `conduit attach {name}` (or reinstall conduit-bios)"),
        )
    })?;
    ui::warn(format!(
        "The Conduit BIOS is no longer installed: {name} boots its stock firmware again ({stock})."
    ));
    if Element::parse(xml.as_bytes()).is_ok_and(|r| has_tpm(&r)) {
        ui::warn(bios::TPM_WARNING);
    }
    Ok(())
}

/// The model attach makes of a video model, without the mode it sets.
fn conduit_model(m: &Element) -> Element {
    let mut m = m.clone();
    m.children.retain(|c| match c {
        XMLNode::Element(a) => a.name != "acceleration" && a.name != "resolution",
        XMLNode::Text(s) => !s.trim().is_empty(),
        _ => true,
    });
    if attr(&m, "type") == Some("qxl") {
        m.attributes.retain(|k, _| k == "heads" || k == "primary");
        m.attributes.insert("type".into(), "virtio".into());
    }
    m.attributes.shift_remove("vram");
    // In-scope namespace declarations differ between the two documents.
    fn no_ns(e: &mut Element) {
        e.namespaces = None;
        for c in e.children.iter_mut() {
            if let XMLNode::Element(x) = c {
                no_ns(x);
            }
        }
    }
    no_ns(&mut m);
    m
}

/// detach: the domain as it is now without Conduit's parts -- the inverse of
/// [`edit_domain`]. What attach overwrote (machine type, loader, emulator,
/// CPU mode, memory backing, NUMA memory access, the SPICE console and its
/// devices, the video model, the Hyper-V additions) comes back from
/// `original`, the definition from before the first attach; everything else
/// the user changed since (disks, memory, CPUs, other devices) stays.
/// `console` is the boot console socket attach set.
pub fn undo_domain(current: &str, original: &str, console: Option<&Path>) -> Result<String> {
    let mut root = parse_domain(current)?;
    let orig =
        parse_domain(original).context("the saved original definition is not a libvirt domain")?;
    let recorded = recorded_stock(&root);

    // <metadata>
    if let Some(md) = root.get_mut_child("metadata") {
        md.children.retain(|n| !is_conduit_meta(n));
    }
    if orig.get_child("metadata").is_none() {
        remove_empty(&mut root, "metadata");
    }

    // <os>: machine type and loader.
    let orig_os = orig.get_child("os");
    if let Some(os) = root.get_mut_child("os") {
        let m = orig_os
            .and_then(|o| o.get_child("type"))
            .and_then(|t| t.attributes.get("machine"));
        if let (Some(t), Some(m)) = (os.get_mut_child("type"), m) {
            t.attributes.insert("machine".into(), m.clone());
        }
        if let Some(l) = os.get_mut_child("loader") {
            let cur = text(l);
            if bios::is_ours(&cur) {
                let stock = orig_os
                    .and_then(|o| o.get_child("loader"))
                    .map(text)
                    .filter(|s| !s.is_empty() && !bios::is_ours(s))
                    .unwrap_or_else(|| bios::stock_for(&cur, recorded.as_deref()));
                l.children = vec![XMLNode::Text(stock)];
            }
        }
    }

    // <memoryBacking>: attach's <source>/<access> out, the original's back.
    let orig_mb = orig.get_child("memoryBacking");
    if let Some(mb) = root.get_mut_child("memoryBacking") {
        let ours = |n: &XMLNode| matches!(n, XMLNode::Element(e) if e.name == "source" || e.name == "access");
        mb.children.retain(|n| !ours(n));
        if let Some(o) = orig_mb {
            mb.children
                .extend(o.children.iter().filter(|n| ours(n)).cloned());
        }
    }
    if orig_mb.is_none() {
        remove_empty(&mut root, "memoryBacking");
    }

    // <cpu>: the original mode attributes, model/vendor/maxphysaddr, and the
    // NUMA cells' memory access.
    let orig_cpu = orig.get_child("cpu");
    if let Some(cpu) = root.get_mut_child("cpu") {
        for k in ["mode", "check", "migratable"] {
            match orig_cpu.and_then(|c| c.attributes.get(k)) {
                Some(v) => {
                    cpu.attributes.insert(k.into(), v.clone());
                }
                None => {
                    cpu.attributes.shift_remove(k);
                }
            }
        }
        let replaced = |n: &XMLNode| matches!(n, XMLNode::Element(e) if matches!(e.name.as_str(), "maxphysaddr" | "model" | "vendor"));
        cpu.children.retain(|n| !replaced(n));
        if let Some(o) = orig_cpu {
            cpu.children
                .extend(o.children.iter().filter(|n| replaced(n)).cloned());
        }
        let private: Vec<String> = orig_cpu
            .and_then(|c| c.get_child("numa"))
            .map(|n| {
                elements(n)
                    .filter(|c| c.name == "cell" && attr(c, "memAccess") == Some("private"))
                    .filter_map(|c| c.attributes.get("id").cloned())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(numa) = cpu.get_mut_child("numa") {
            for cell in numa.children.iter_mut().filter_map(|n| match n {
                XMLNode::Element(e) if e.name == "cell" => Some(e),
                _ => None,
            }) {
                if cell
                    .attributes
                    .get("id")
                    .is_some_and(|id| private.contains(id))
                {
                    cell.attributes.insert("memAccess".into(), "private".into());
                }
            }
        }
    }
    if orig_cpu.is_none() {
        remove_empty(&mut root, "cpu");
    }

    // Hyper-V: the enlightenments (and stimer's direct mode) attach added.
    let orig_hv = orig
        .get_child("features")
        .and_then(|f| f.get_child("hyperv"));
    if let Some(hv) = root
        .get_mut_child("features")
        .and_then(|f| f.get_mut_child("hyperv"))
    {
        hv.children.retain(|n| match n {
            XMLNode::Element(e) => {
                !HYPERV.iter().any(|(h, _)| *h == e.name)
                    || orig_hv.is_some_and(|o| o.get_child(e.name.as_str()).is_some())
            }
            _ => true,
        });
        let direct = orig_hv
            .and_then(|o| o.get_child("stimer"))
            .is_some_and(|s| s.get_child("direct").is_some());
        if let Some(s) = hv.get_mut_child("stimer") {
            if !direct {
                s.children
                    .retain(|c| !matches!(c, XMLNode::Element(d) if d.name == "direct"));
            }
        }
    }
    if orig_hv.is_none() {
        if let Some(f) = root.get_mut_child("features") {
            f.children.retain(|n| {
                !matches!(n, XMLNode::Element(e) if e.name == "hyperv"
                    && !e.children.iter().any(|c| c.as_element().is_some()))
            });
        }
        if orig.get_child("features").is_none() {
            remove_empty(&mut root, "features");
        }
    }
    let orig_clock = orig.get_child("clock");
    let hvclock = |c: &Element| {
        elements(c).any(|t| t.name == "timer" && attr(t, "name") == Some("hypervclock"))
    };
    if !orig_clock.is_some_and(hvclock) {
        if let Some(c) = root.get_mut_child("clock") {
            c.children.retain(|n| {
                !matches!(n, XMLNode::Element(t) if t.name == "timer"
                    && attr(t, "name") == Some("hypervclock"))
            });
        }
    }
    if orig_clock.is_none() {
        root.children.retain(|n| {
            !matches!(n, XMLNode::Element(c) if c.name == "clock"
                && !c.children.iter().any(|x| x.as_element().is_some())
                && c.attributes.len() == 1 && attr(c, "offset") == Some("localtime"))
        });
    }

    // <devices>
    let orig_dev = orig.get_child("devices");
    if let Some(dev) = root.get_mut_child("devices") {
        // Emulator.
        match orig_dev.and_then(|d| d.get_child("emulator")) {
            Some(e) => {
                child_mut(dev, "emulator").children = e.children.clone();
            }
            None => dev
                .children
                .retain(|n| !matches!(n, XMLNode::Element(e) if e.name == "emulator")),
        }
        // The boot console, Conduit's channels and shares.
        let console = console.map(|c| c.display().to_string());
        dev.children.retain(|n| {
            let XMLNode::Element(e) = n else { return true };
            let ours = match e.name.as_str() {
                "graphics" => attr(e, "socket").is_some_and(|s| {
                    Some(s) == console.as_deref()
                        || s.ends_with("/console.sock")
                        || s.ends_with("/console/vnc.sock")
                }),
                "channel" => e
                    .get_child("target")
                    .and_then(|t| attr(t, "name"))
                    .is_some_and(|t| t == conduit_stats::CHANNEL || t == conduit_ctl::CHANNEL),
                "filesystem" => e
                    .get_child("target")
                    .and_then(|t| attr(t, "dir"))
                    .is_some_and(|d| d == "nvidia" || shares::is_share_tag(d)),
                _ => false,
            };
            !ours
        });
        // The virtio-serial controller attach added, when nothing else uses it.
        let had_ctl = orig_dev.is_some_and(|d| {
            elements(d).any(|e| e.name == "controller" && attr(e, "type") == Some("virtio-serial"))
        });
        let virtio_users = elements(dev).any(|e| {
            matches!(e.name.as_str(), "channel" | "console")
                && e.get_child("target").and_then(|t| attr(t, "type")) == Some("virtio")
        });
        if !had_ctl && !virtio_users {
            dev.children.retain(|n| {
                !matches!(n, XMLNode::Element(e) if e.name == "controller"
                    && attr(e, "type") == Some("virtio-serial"))
            });
        }
        if let Some(od) = orig_dev {
            // What attach took out: the SPICE console and its devices, and
            // libvirt's USB tablet (it moved to the command line).
            let has_tablet =
                elements(dev).any(|e| e.name == "input" && attr(e, "type") == Some("tablet"));
            for e in elements(od) {
                let back = matches!(e.name.as_str(), "graphics" | "redirfilter")
                    || is_spice_dev(e)
                    || (!has_tablet
                        && e.name == "input"
                        && attr(e, "type") == Some("tablet")
                        && attr(e, "bus") == Some("usb"));
                if back {
                    dev.children.push(XMLNode::Element(e.clone()));
                }
            }
            // SPICE audio that became type='none'.
            for a in dev.children.iter_mut().filter_map(|n| match n {
                XMLNode::Element(e) if e.name == "audio" && attr(e, "type") == Some("none") => {
                    Some(e)
                }
                _ => None,
            }) {
                let was_spice = elements(od).any(|o| {
                    o.name == "audio"
                        && attr(o, "type") == Some("spice")
                        && attr(o, "id") == attr(a, "id")
                });
                if was_spice {
                    a.attributes.insert("type".into(), "spice".into());
                }
            }
            // Video models: the original where attach's change is still all
            // there is; else (changed since) only attach's <resolution> goes.
            let orig_models: Vec<Option<&Element>> = elements(od)
                .filter(|e| e.name == "video")
                .map(|v| v.get_child("model"))
                .collect();
            let videos = dev.children.iter_mut().filter_map(|n| match n {
                XMLNode::Element(e) if e.name == "video" => Some(e),
                _ => None,
            });
            for (v, om) in videos.zip(orig_models) {
                let Some(m) = v.get_mut_child("model") else {
                    continue;
                };
                match om {
                    Some(o) if conduit_model(o) == conduit_model(m) => *m = o.clone(),
                    Some(o) if o.get_child("resolution").is_none() => m
                        .children
                        .retain(|c| !matches!(c, XMLNode::Element(r) if r.name == "resolution")),
                    _ => {}
                }
            }
        }
    }

    // <qemu:commandline>: Conduit's arguments out.
    let had_cl = elements(&orig).any(|e| e.name == "commandline" && is_qemu(e));
    if let Some(cl) = root.children.iter_mut().find_map(|n| match n {
        XMLNode::Element(e) if e.name == "commandline" && is_qemu(e) => Some(e),
        _ => None,
    }) {
        let args: Vec<Element> = elements(cl).filter(|e| e.name == "arg").cloned().collect();
        let values: Vec<String> = args
            .iter()
            .map(|a| a.attributes.get("value").cloned().unwrap_or_default())
            .collect();
        let others: Vec<XMLNode> = cl
            .children
            .iter()
            .filter(|n| !matches!(n, XMLNode::Element(e) if e.name == "arg"))
            .cloned()
            .collect();
        cl.children = foreign_args(&args, &values)
            .into_iter()
            .map(XMLNode::Element)
            .collect();
        cl.children.extend(others);
    }
    if !had_cl {
        root.children.retain(|n| {
            !matches!(n, XMLNode::Element(e) if e.name == "commandline" && is_qemu(e)
                && !e.children.iter().any(|c| c.as_element().is_some()))
        });
    }
    emit(&root)
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
    let stats = units::stats_path(&scope, name);
    let ctl = units::ctl_path(&scope, name);
    let share_list = if dry_run {
        let l = shares::load(name)?;
        if l.is_empty() && !shares::file(name).is_file() {
            vec![shares::default_share(name)]
        } else {
            l
        }
    } else {
        shares::load_or_init(name)?
    };
    let share_wires = shares::wires(&scope, name, &share_list);
    let bios_dir = bios::installed_dir();
    let machines = qemu_machines(&emu);
    let (mode, _) = crate::mode::detect();
    let wiring = Wiring {
        emulator: &emu,
        gpu_sock: &gpu,
        vfs_sock: &vfs,
        console_sock: &console,
        stats_sock: &stats,
        ctl_sock: &ctl,
        vm: name,
        shares: &share_wires,
        windows,
        bios: bios_dir.as_deref(),
        bios_fits: &bios::check,
        display: Some((mode.width, mode.height)),
        machines: machines.as_deref(),
    };
    let new_xml = edit_domain(&xml, &wiring)?;
    let edit_notes = notes(&xml, &wiring);
    if dry_run {
        ui::info(format!("libvirt: {uri}; emulator: {}", emu.display()));
        for n in &edit_notes {
            ui::info(n);
        }
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
        Some(b) => {
            // Backups from before they were written private.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o600));
            b
        }
        None if already => {
            return Err(oops(
                format!("{name} already carries Conduit's GPU, but its original definition is not on record"),
                format!("Remove Conduit's parts from it in virt-manager (the GPU lines under <qemu:commandline>), then run `conduit attach {name}` again"),
            ))
        }
        None => {
            let b = dir.join(format!("libvirt-backup-{}.xml", timestamp()));
            sys::write_private(&b, &xml)?;
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
    let defined = v.define(&new_xml, name).or_else(|e| {
        if !refused_resolution(&format!("{e:#}")) || !new_xml.contains("<resolution") {
            return Err(e);
        }
        v.define(&without_resolution(&new_xml)?, name)?;
        println!("This libvirt has no <resolution> for video devices: the boot console starts in the firmware's default mode.");
        Ok(())
    });
    if let Err(e) = defined {
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
    for n in &edit_notes {
        println!("{n}");
    }

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
    // The definition as it is now (the user may have changed disks, memory,
    // devices since attach) minus Conduit's parts; the values attach
    // overwrote come from the backup. Saved first, so nothing is lost.
    let current = v.inactive_xml(&link.domain)?;
    let original = std::fs::read_to_string(&backup)?;
    let restored = undo_domain(&current, &original, link.console.as_deref())?;
    let pre = paths::vm_dir(name).join(format!("libvirt-pre-detach-{}.xml", timestamp()));
    sys::write_private(&pre, &current)?;
    v.define(&restored, name).map_err(|e| {
        oops(
            format!("libvirt refused the definition without Conduit's parts: {e:#}"),
            format!(
                "Nothing was changed. The definition from before attach is at {}, the current one at {}",
                backup.display(),
                pre.display()
            ),
        )
    })?;
    let scope = link.scope()?;
    units::remove(&scope, name);
    virt::remove_desktop_entry(name);
    let _ = std::fs::remove_file(Link::file(name));
    println!(
        "Removed Conduit's GPU from {name}; your changes since attach are kept, and what attach had changed is back as it was (from {}). \
         The definition from just before this is saved at {}. Conduit's guest driver stays installed inside it; it does nothing without the GPU.",
        backup.display(),
        pre.display()
    );
    if firmware_changed(&current, &restored) {
        ui::warn(bios::TPM_WARNING);
    }
    Ok(())
}

/// Did the loader change between two definitions of a domain with a TPM?
fn firmware_changed(before: &str, after: &str) -> bool {
    let loader = |x: &str| {
        Element::parse(x.as_bytes()).ok().and_then(|r| {
            let l = r.get_child("os")?.get_child("loader").map(text);
            Some((l, has_tpm(&r)))
        })
    };
    match (loader(before), loader(after)) {
        (Some((a, tpm)), Some((b, _))) => tpm && a != b,
        _ => false,
    }
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
  <cpu mode='host-model' check='partial'><model>EPYC</model><topology sockets='1' cores='4' threads='1'/><feature policy='require' name='topoext'/></cpu>
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

    const STATS: &str = "/run/user/1000/conduit/myvm/stats.sock";
    const CTL: &str = "/run/user/1000/conduit/myvm/ctl.sock";

    fn edit_as(x: &str, windows: bool) -> String {
        edit_domain(
            x,
            &Wiring {
                emulator: Path::new("/opt/conduit/bin/qemu-system-x86_64"),
                gpu_sock: Path::new("/run/user/1000/conduit/myvm/gpu-libvirt.sock"),
                vfs_sock: Path::new("/run/user/1000/conduit/myvm/vfs-libvirt.sock"),
                console_sock: Path::new(CONSOLE),
                stats_sock: Path::new(STATS),
                ctl_sock: Path::new(CTL),
                vm: "myvm",
                shares: &[Wire {
                    tag: "conduit-Conduit".into(),
                    sock: PathBuf::from("/run/user/1000/conduit/myvm/share-Conduit.sock"),
                }],
                windows,
                bios: None,
                bios_fits: &fits,
                display: None,
                machines: Some(&machines()),
            },
        )
        .unwrap()
    }

    fn fits(_: &str, _: &Path, _: bool) -> std::result::Result<(), String> {
        Ok(())
    }

    fn edit_bios(x: &str, bios: Option<&Path>, display: Option<(u32, u32)>) -> String {
        edit_domain(
            x,
            &Wiring {
                emulator: Path::new("/opt/conduit/bin/qemu-system-x86_64"),
                gpu_sock: Path::new("/run/user/1000/conduit/myvm/gpu-libvirt.sock"),
                vfs_sock: Path::new("/run/user/1000/conduit/myvm/vfs-libvirt.sock"),
                console_sock: Path::new(CONSOLE),
                stats_sock: Path::new(STATS),
                ctl_sock: Path::new(CTL),
                vm: "myvm",
                shares: &[],
                windows: false,
                bios,
                bios_fits: &fits,
                display,
                machines: Some(&machines()),
            },
        )
        .unwrap()
    }

    /// A UEFI VM on Ubuntu's stock OVMF (what virt-manager writes), stdvga.
    const UEFI: &str = r#"<domain type='kvm'>
  <name>myvm</name>
  <os firmware='efi'>
    <type arch='x86_64' machine='pc-q35-8.2'>hvm</type>
    <loader readonly='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd'>/home/u/.config/libvirt/qemu/nvram/myvm_VARS.fd</nvram>
  </os>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <video><model type='vga' vram='16384' heads='1' primary='yes'/></video>
  </devices>
</domain>"#;

    fn os_child(out: &str, name: &str) -> Element {
        Element::parse(out.as_bytes())
            .unwrap()
            .get_child("os")
            .unwrap()
            .get_child(name)
            .unwrap()
            .clone()
    }

    fn meta_vm(out: &str) -> Element {
        let root = Element::parse(out.as_bytes()).unwrap();
        root.get_child("metadata")
            .unwrap()
            .children
            .iter()
            .find_map(|n| n.as_element().filter(|e| e.name == "vm").cloned())
            .unwrap()
    }

    #[test]
    fn the_conduit_bios_replaces_the_matching_stock_loader_and_keeps_the_vars() {
        let dir = Path::new("/usr/share/conduit/bios");
        let out = edit_bios(UEFI, Some(dir), None);
        let loader = os_child(&out, "loader");
        assert_eq!(
            loader.get_text().unwrap(),
            "/usr/share/conduit/bios/conduit-bios.fd"
        );
        assert_eq!(loader.attributes["type"], "pflash");
        let nvram = os_child(&out, "nvram");
        assert_eq!(
            nvram.get_text().unwrap(),
            "/home/u/.config/libvirt/qemu/nvram/myvm_VARS.fd"
        );
        assert_eq!(
            nvram.attributes["template"],
            "/usr/share/OVMF/OVMF_VARS_4M.fd"
        );
        assert_eq!(
            meta_vm(&out).attributes["stock-loader"],
            "/usr/share/OVMF/OVMF_CODE_4M.fd"
        );
        // Attaching again changes nothing.
        assert_eq!(edit_bios(&out, Some(dir), None), out);
        // Without the package the stock loader comes back.
        let back = edit_bios(&out, None, None);
        assert_eq!(
            os_child(&back, "loader").get_text().unwrap(),
            "/usr/share/OVMF/OVMF_CODE_4M.fd"
        );
        assert!(!meta_vm(&back).attributes.contains_key("stock-loader"));
    }

    #[test]
    fn other_firmware_and_missing_package_leave_the_loader_alone() {
        let out = edit_bios(UEFI, None, None);
        assert_eq!(
            os_child(&out, "loader").get_text().unwrap(),
            "/usr/share/OVMF/OVMF_CODE_4M.fd"
        );
        let arch = UEFI.replace(
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        );
        let out = edit_bios(&arch, Some(Path::new("/usr/share/conduit/bios")), None);
        assert_eq!(
            os_child(&out, "loader").get_text().unwrap(),
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd"
        );
        // SeaBIOS: no <loader> at all, none added.
        let out = edit_bios(DOMAIN, Some(Path::new("/usr/share/conduit/bios")), None);
        assert!(Element::parse(out.as_bytes())
            .unwrap()
            .get_child("os")
            .unwrap()
            .get_child("loader")
            .is_none());
    }

    #[test]
    fn the_video_device_starts_in_the_native_mode() {
        let out = edit_bios(UEFI, None, Some((5120, 1440)));
        let dev = devices(&out);
        let m = dev.get_child("video").unwrap().get_child("model").unwrap();
        let r = m.get_child("resolution").unwrap();
        assert_eq!(
            (r.attributes["x"].as_str(), r.attributes["y"].as_str()),
            ("5120", "1440")
        );
        // 5120x1440x4 = 28.1 MiB: 32 MiB of video memory.
        assert_eq!(m.attributes["vram"], "32768");
        assert_eq!(edit_bios(&out, None, Some((5120, 1440))), out);
        // QXL becomes virtio-vga: a resolution, no video memory to size.
        let out = edit_bios(VIRT_INSTALL, None, Some((1920, 1080)));
        let m = devices(&out)
            .get_child("video")
            .unwrap()
            .get_child("model")
            .unwrap()
            .clone();
        assert_eq!(m.attributes["type"], "virtio");
        assert_eq!(m.get_child("resolution").unwrap().attributes["x"], "1920");
        assert!(!m.attributes.contains_key("vram"));
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
        assert!(
            cpu.children.iter().any(|n| matches!(n, XMLNode::Element(e)
                if e.name == "feature" && e.attributes.get("name").map(String::as_str) == Some("topoext"))),
            "cpu features kept"
        );
        assert!(
            cpu.get_child("model").is_none(),
            "no model in host-passthrough"
        );
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
        assert!(
            out.contains("machine=\"pc-q35-8.2\""),
            "Conduit's QEMU has the pinned type: {out}"
        );
        assert!(out.contains("myvm.qcow2"));
    }

    /// The `<qemu:arg>` values, unescaped.
    fn qemu_args(out: &str) -> Vec<String> {
        let root = Element::parse(out.as_bytes()).unwrap();
        root.children
            .iter()
            .filter_map(|n| n.as_element())
            .filter(|e| e.name == "commandline")
            .flat_map(|e| e.children.iter().filter_map(|n| n.as_element()))
            .filter_map(|a| a.attributes.get("value").cloned())
            .collect()
    }

    #[test]
    fn the_tablet_moves_to_the_command_line_bound_to_the_display_and_qmp_is_added() {
        let out = edit(VIRT_INSTALL);
        let root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_child("devices").unwrap();
        assert!(
            !all(dev, "input")
                .iter()
                .any(|e| e.attributes.get("type").map(String::as_str) == Some("tablet")),
            "libvirt's tablet is gone: {out}"
        );
        assert!(qemu_args(&out).contains(&super::tablet_arg()), "{out}");
        assert_eq!(
            super::tablet_arg(),
            r#"{"driver":"usb-tablet","id":"conduit-tablet","bus":"usb.0","port":"1","display":"video0"}"#
        );
        assert!(
            out.contains(&format!(
                "value=\"socket,id=conduit-qmp,path={},server=on,wait=off\"",
                Path::new(CONSOLE).with_file_name("qmp.sock").display()
            )),
            "{out}"
        );
        assert!(
            out.contains("value=\"chardev=conduit-qmp,mode=control\""),
            "{out}"
        );
        // The tablet comes after the GPU, and so after every libvirt device.
        let gpu = out.find("vhost-user-test-device-pci").unwrap();
        assert!(out.find("usb-tablet").unwrap() > gpu);
        // A domain without a tablet does not get one.
        assert_eq!(edit(&out), out, "idempotent");
        let none = edit(DOMAIN);
        assert!(!none.contains("usb-tablet"), "{none}");
        assert!(none.contains("conduit-qmp"), "{none}");
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
        assert_eq!(
            twice.matches("-chardev").count(),
            2,
            "the GPU and QMP: {twice}"
        );
        assert_eq!(
            twice.matches("conduit-tablet").count(),
            0,
            "no tablet, none added: {twice}"
        );
        assert_eq!(twice.matches("<qemu:commandline").count(), 1);
        assert_eq!(twice.matches("conduit:vm").count(), 1);
        assert_eq!(twice.matches("<filesystem").count(), 2);
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
        assert_eq!(ch.len(), 3, "the guest agent channel stays: {out}");
        assert!(out.contains("org.qemu.guest_agent.0"), "{out}");
        assert_eq!(ch[0].attributes["type"], "unix");
        assert_eq!(all(&dev, "audio")[0].attributes["type"], "none");
        assert_eq!(all(&dev, "sound").len(), 1, "the sound card stays");
        let m = all(&dev, "video")[0].get_child("model").unwrap();
        assert_eq!(m.attributes["type"], "virtio", "no QXL without SPICE");
        assert_eq!(m.attributes["heads"], "1");
        assert!(m.attributes.get("vram").is_none(), "{out}");
        assert_eq!(all(&dev, "tpm").len(), 1, "the TPM stays");
        assert!(all(&dev, "input").is_empty(), "the tablet moved: {out}");
        assert!(
            qemu_args(&out).contains(&super::tablet_arg()),
            "to the command line: {out}"
        );
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
    fn stats_channel_added_once_with_a_controller() {
        let out = edit(WIN11);
        let root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_child("devices").unwrap();
        let chans: Vec<_> = elements(dev)
            .filter(|e| e.name == "channel")
            .filter(|e| {
                e.get_child("target")
                    .unwrap()
                    .attributes
                    .get("name")
                    .map(String::as_str)
                    == Some("org.conduit.stats.0")
            })
            .collect();
        assert_eq!(chans.len(), 1, "{out}");
        assert_eq!(chans[0].attributes["type"], "unix");
        let src = chans[0].get_child("source").unwrap();
        assert_eq!(src.attributes["mode"], "bind");
        assert_eq!(src.attributes["path"], STATS);
        assert_eq!(
            chans[0].get_child("target").unwrap().attributes["type"],
            "virtio"
        );
        let ctl = |d: &Element| {
            elements(d)
                .filter(|e| {
                    e.name == "controller"
                        && e.attributes.get("type").map(String::as_str) == Some("virtio-serial")
                })
                .count()
        };
        assert_eq!(ctl(dev), 1, "{out}");
        assert_eq!(edit(&out), out, "idempotent");
    }

    #[test]
    fn stats_channel_keeps_an_existing_controller_and_other_channels() {
        let x = "<domain type='kvm'><name>a</name><devices>\
            <controller type='virtio-serial' index='0'/>\
            <channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>\
            </devices></domain>";
        let out = edit(x);
        let root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_child("devices").unwrap();
        assert_eq!(
            elements(dev).filter(|e| e.name == "controller").count(),
            1,
            "{out}"
        );
        // The guest agent's, the stats feed's and the control channel's.
        assert_eq!(
            elements(dev).filter(|e| e.name == "channel").count(),
            3,
            "{out}"
        );
        assert!(out.contains("org.qemu.guest_agent.0"));
    }

    #[test]
    fn ctl_channel_added_once_next_to_the_stats_channel() {
        let out = edit(WIN11);
        let root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_child("devices").unwrap();
        let named = |n: &str| -> Vec<&Element> {
            elements(dev)
                .filter(|e| e.name == "channel")
                .filter(|e| {
                    e.get_child("target")
                        .and_then(|t| t.attributes.get("name"))
                        .map(String::as_str)
                        == Some(n)
                })
                .collect()
        };
        let ctl = named("org.conduit.ctl.0");
        assert_eq!(ctl.len(), 1, "{out}");
        assert_eq!(ctl[0].attributes["type"], "unix");
        let src = ctl[0].get_child("source").unwrap();
        assert_eq!(src.attributes["mode"], "bind");
        assert_eq!(src.attributes["path"], CTL);
        assert_eq!(
            ctl[0].get_child("target").unwrap().attributes["type"],
            "virtio"
        );
        assert_eq!(named("org.conduit.stats.0").len(), 1, "{out}");
        // One controller serves both, and a second edit changes nothing.
        let controllers = elements(dev)
            .filter(|e| {
                e.name == "controller"
                    && e.attributes.get("type").map(String::as_str) == Some("virtio-serial")
            })
            .count();
        assert_eq!(controllers, 1, "{out}");
        assert_eq!(edit(&out), out, "idempotent");
        // A domain that already has the stats channel (attached before the
        // control channel existed) gains only the new one.
        let at = out.find("org.conduit.ctl.0").unwrap();
        let from = out[..at].rfind("<channel").unwrap();
        let to = at + out[at..].find("</channel>").unwrap() + "</channel>".len();
        let old = format!("{}{}", &out[..from], &out[to..]);
        assert!(!old.contains("org.conduit.ctl.0"), "{old}");
        let again = edit(&old);
        assert!(again.contains("org.conduit.ctl.0"), "{again}");
    }

    fn share_targets(xml: &str) -> Vec<String> {
        let root = Element::parse(xml.as_bytes()).unwrap();
        let devices = root.get_child("devices").unwrap();
        elements(devices)
            .filter(|e| e.name == "filesystem")
            .filter_map(|e| e.get_child("target")?.attributes.get("dir").cloned())
            .collect()
    }

    #[test]
    fn attach_adds_the_default_share_once() {
        let out = edit(WIN11);
        assert_eq!(share_targets(&out), ["nvidia", "conduit-Conduit"], "{out}");
        assert!(out.contains("share-Conduit.sock"));
        assert_eq!(share_targets(&edit(&out)), ["nvidia", "conduit-Conduit"]);
    }

    #[test]
    fn apply_shares_replaces_only_ours() {
        let with_user_fs = edit(WIN11).replace(
            "</devices>",
            "<filesystem type='mount'><driver type='virtiofs'/><source dir='/tmp'/><target dir='hostshare'/></filesystem></devices>",
        );
        let ws = [
            Wire {
                tag: "conduit-A".into(),
                sock: PathBuf::from("/s/a.sock"),
            },
            Wire {
                tag: "conduit-B".into(),
                sock: PathBuf::from("/s/b.sock"),
            },
        ];
        let out = apply_shares(&with_user_fs, &ws).unwrap();
        assert_eq!(
            share_targets(&out),
            ["nvidia", "hostshare", "conduit-A", "conduit-B"],
            "{out}"
        );
        let none = apply_shares(&out, &[]).unwrap();
        assert_eq!(share_targets(&none), ["nvidia", "hostshare"]);
        assert!(apply_shares("<network/>", &ws).is_err());
    }

    #[test]
    fn rejects_non_domain() {
        let w = Wiring {
            emulator: Path::new("/q"),
            gpu_sock: Path::new("/s"),
            vfs_sock: Path::new("/v"),
            console_sock: Path::new("/c"),
            stats_sock: Path::new("/t"),
            ctl_sock: Path::new("/u"),
            vm: "x",
            shares: &[],
            windows: false,
            bios: None,
            bios_fits: &fits,
            display: None,
            machines: None,
        };
        assert!(edit_domain("<network/>", &w).is_err());
        assert!(edit_domain("not xml", &w).is_err());
    }

    /// What `qemu-system-x86_64 -machine help` lists (trimmed): QEMU 11.1
    /// keeps its versioned types back to 8.x, Ubuntu's "noble" ones are not
    /// upstream.
    const MACHINE_HELP: &str = "Supported machines are:\n\
        pc                   Standard PC (i440FX + PIIX, 1996) (alias of pc-i440fx-11.1)\n\
        pc-i440fx-11.1       Standard PC (i440FX + PIIX, 1996) (default)\n\
        q35                  Standard PC (Q35 + ICH9, 2009) (alias of pc-q35-11.1)\n\
        pc-q35-11.1          Standard PC (Q35 + ICH9, 2009)\n\
        pc-q35-8.2           Standard PC (Q35 + ICH9, 2009)\n\
        none                 empty machine\n";

    fn machines() -> Vec<String> {
        parse_machines(MACHINE_HELP)
    }

    fn wiring_with<'a>(
        bios: Option<&'a Path>,
        windows: bool,
        machines: Option<&'a [String]>,
        shares: &'a [Wire],
    ) -> Wiring<'a> {
        Wiring {
            emulator: Path::new("/opt/conduit/bin/qemu-system-x86_64"),
            gpu_sock: Path::new("/run/user/1000/conduit/myvm/gpu-libvirt.sock"),
            vfs_sock: Path::new("/run/user/1000/conduit/myvm/vfs-libvirt.sock"),
            console_sock: Path::new(CONSOLE),
            stats_sock: Path::new(STATS),
            ctl_sock: Path::new(CTL),
            vm: "myvm",
            shares,
            windows,
            bios,
            bios_fits: &fits,
            display: Some((5120, 1440)),
            machines,
        }
    }

    /// A tree to compare definitions by: no whitespace, no namespace
    /// declarations, children in a fixed order (libvirt orders devices itself).
    fn canon(xml: &str) -> String {
        fn norm(e: &mut Element) {
            e.namespaces = None;
            e.children.retain(|n| match n {
                XMLNode::Text(t) => !t.trim().is_empty(),
                XMLNode::Comment(_) => false,
                _ => true,
            });
            for c in e.children.iter_mut() {
                match c {
                    XMLNode::Element(x) => norm(x),
                    XMLNode::Text(t) => *t = t.trim().to_string(),
                    _ => {}
                }
            }
            let key = |n: &XMLNode| match n {
                XMLNode::Element(x) => emit_el(x),
                XMLNode::Text(t) => t.clone(),
                _ => String::new(),
            };
            e.children.sort_by_key(key);
        }
        fn emit_el(e: &Element) -> String {
            let mut out = Vec::new();
            e.write_with_config(
                &mut out,
                EmitterConfig::new()
                    .perform_indent(false)
                    .write_document_declaration(false),
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        }
        let mut root = Element::parse(xml.as_bytes()).unwrap();
        norm(&mut root);
        emit_el(&root)
    }

    fn attach_then_detach(orig: &str, windows: bool, bios: Option<&Path>) -> (String, String) {
        let ws = [Wire {
            tag: "conduit-Conduit".into(),
            sock: PathBuf::from("/run/user/1000/conduit/myvm/share-Conduit.sock"),
        }];
        let m = machines();
        let attached = edit_domain(orig, &wiring_with(bios, windows, Some(&m), &ws)).unwrap();
        let back = undo_domain(&attached, orig, Some(Path::new(CONSOLE))).unwrap();
        (attached, back)
    }

    #[test]
    fn detach_takes_out_exactly_what_attach_put_in() {
        let noble = DOMAIN.replace("pc-q35-8.2", "pc-q35-noble");
        let numa = DOMAIN.replace(
            "<feature policy='require' name='topoext'/>",
            "<feature policy='require' name='topoext'/><numa><cell id='0' cpus='0-3' memory='8388608' unit='KiB' memAccess='private'/></numa>",
        );
        let bios = Path::new("/usr/share/conduit/bios");
        for (name, x, win, b) in [
            ("DOMAIN", DOMAIN.to_string(), false, None),
            ("noble", noble, false, None),
            ("numa", numa, false, None),
            ("VIRT_INSTALL", VIRT_INSTALL.to_string(), false, None),
            ("WIN11", WIN11.to_string(), true, None),
            ("UEFI", UEFI.to_string(), false, Some(bios)),
        ] {
            let (attached, back) = attach_then_detach(&x, win, b);
            assert_ne!(
                canon(&attached),
                canon(&x),
                "{name}: attach changed nothing"
            );
            assert_eq!(canon(&back), canon(&x), "{name}:\n{back}");
            assert!(!virt::is_ours(&back), "{name}");
        }
    }

    #[test]
    fn detach_keeps_changes_made_after_attach() {
        // virt-manager after attach: more RAM, a second disk, hugepages, a
        // CPU topology change and a user's own qemu argument.
        let user = |x: &str| {
            x.replace(
                "<memory unit='KiB'>4194304</memory>",
                "<memory unit='KiB'>16777216</memory>",
            )
            .replace(
                "<memory unit=\"KiB\">4194304</memory>",
                "<memory unit=\"KiB\">16777216</memory>",
            )
            .replace(
                "</devices>",
                "<disk type='file' device='disk'><source file='/var/lib/libvirt/images/data.qcow2'/><target dev='vdb' bus='virtio'/></disk></devices>",
            )
        };
        let (attached, _) = attach_then_detach(VIRT_INSTALL, false, None);
        let changed = user(&attached);
        assert!(changed.contains("16777216") && changed.contains("data.qcow2"));
        let back = undo_domain(&changed, VIRT_INSTALL, Some(Path::new(CONSOLE))).unwrap();
        assert_eq!(canon(&back), canon(&user(VIRT_INSTALL)), "{back}");

        // The same on a domain with <cpu>/<memoryBacking>: the user's
        // hugepages and topology stay, attach's memfd/host-passthrough go.
        let (attached, _) = attach_then_detach(DOMAIN, false, None);
        let changed = attached
            .replace("cores=\"4\"", "cores=\"8\"")
            .replace("<hugepages />", "<hugepages /><nosharepages />");
        assert!(changed.contains("nosharepages"), "{attached}");
        let back = undo_domain(&changed, DOMAIN, None).unwrap();
        let want = DOMAIN
            .replace("cores='4'", "cores='8'")
            .replace("<hugepages/>", "<hugepages/><nosharepages/>");
        assert_eq!(canon(&back), canon(&want), "{back}");
        let root = Element::parse(back.as_bytes()).unwrap();
        assert_eq!(
            root.get_child("cpu").unwrap().attributes["mode"],
            "host-model"
        );
        assert_eq!(
            root.get_child("os")
                .unwrap()
                .get_child("type")
                .unwrap()
                .attributes["machine"],
            "pc-q35-8.2"
        );
    }

    #[test]
    fn detach_puts_the_stock_loader_back() {
        let bios = Path::new("/usr/share/conduit/bios");
        let (attached, back) = attach_then_detach(UEFI, false, Some(bios));
        assert!(attached.contains("conduit-bios.fd"));
        assert_eq!(
            os_child(&back, "loader").get_text().unwrap(),
            "/usr/share/OVMF/OVMF_CODE_4M.fd"
        );
        // The nvram the user moved after attach stays where it is now.
        let moved = attached.replace("myvm_VARS.fd", "myvm_VARS-new.fd");
        let back = undo_domain(&moved, UEFI, None).unwrap();
        assert!(back.contains("myvm_VARS-new.fd"), "{back}");
        assert!(firmware_changed(
            &attached.replace("</devices>", "<tpm model='tpm-crb'/></devices>"),
            &back.replace("</devices>", "<tpm model='tpm-crb'/></devices>")
        ));
        assert!(
            !firmware_changed(&attached, &back),
            "no TPM, nothing to say"
        );
    }

    #[test]
    fn stimer_is_not_added_without_the_hyperv_clock() {
        let off = WIN11.replace(
            "<timer name='hpet' present='no'/>",
            "<timer name='hpet' present='no'/><timer name='hypervclock' present='no'/>",
        );
        let out = edit_as(&off, true);
        let hv = hyperv(&out);
        assert!(
            all(&hv, "stimer").is_empty(),
            "QEMU refuses hv-stimer without hv-time: {out}"
        );
        assert_eq!(all(&hv, "synic").len(), 1, "the rest is added: {out}");
        assert!(out.contains("name=\"hypervclock\" present=\"no\""), "{out}");
        assert_eq!(edit_as(&out, true), out, "idempotent");
        // With the clock on (or added by attach), stimer is there.
        assert_eq!(all(&hyperv(&edit_as(WIN11, true)), "stimer").len(), 1);
    }

    #[test]
    fn a_pinned_machine_type_is_kept_when_conduits_qemu_has_it() {
        assert_eq!(
            machines(),
            [
                "pc",
                "pc-i440fx-11.1",
                "q35",
                "pc-q35-11.1",
                "pc-q35-8.2",
                "none"
            ]
        );
        let m = machines();
        let w = wiring_with(None, false, Some(&m), &[]);
        let machine =
            |x: &str| os_child(&edit_domain(x, &w).unwrap(), "type").attributes["machine"].clone();
        assert_eq!(machine(DOMAIN), "pc-q35-8.2");
        assert!(notes(DOMAIN, &w)
            .iter()
            .all(|n| !n.contains("Machine type")));
        // Ubuntu's own type is not in Conduit's QEMU: the alias, and a note.
        let noble = DOMAIN.replace("pc-q35-8.2", "pc-q35-noble");
        assert_eq!(machine(&noble), "q35");
        let n = notes(&noble, &w);
        assert!(
            n.iter().any(|n| n.contains("pc-q35-noble becomes q35")),
            "{n:?}"
        );
        let old = DOMAIN.replace("pc-q35-8.2", "pc-i440fx-6.2");
        assert_eq!(machine(&old), "pc");
        // Unknown machine list: only the aliases are sure to exist.
        let w = wiring_with(None, false, None, &[]);
        assert_eq!(
            os_child(&edit_domain(DOMAIN, &w).unwrap(), "type").attributes["machine"],
            "q35"
        );
    }

    #[test]
    fn private_numa_cells_become_shared_memory() {
        let numa = DOMAIN.replace(
            "<feature policy='require' name='topoext'/>",
            "<numa><cell id='0' cpus='0-1' memory='4' unit='GiB' memAccess='private'/><cell id='1' cpus='2-3' memory='4' unit='GiB'/></numa>",
        );
        let out = edit(&numa);
        let root = Element::parse(out.as_bytes()).unwrap();
        let cells = all(
            root.get_child("cpu").unwrap().get_child("numa").unwrap(),
            "cell",
        )
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
        assert_eq!(cells[0].attributes["memAccess"], "shared", "{out}");
        assert!(!cells[1].attributes.contains_key("memAccess"), "{out}");
        assert_eq!(edit(&out), out, "idempotent");
        let m = machines();
        let n = notes(&numa, &wiring_with(None, false, Some(&m), &[]));
        assert!(n.iter().any(|n| n.contains("memAccess='private'")), "{n:?}");
        // detach gives the cell its private memory back.
        let back = undo_domain(&out, &numa, None).unwrap();
        assert!(back.contains("memAccess=\"private\""), "{back}");
    }

    #[test]
    fn reattach_keeps_the_addresses_libvirt_assigned() {
        // libvirt gives each channel a virtio-serial port and each
        // filesystem a PCI slot on define; a second attach keeps them.
        let ws = [Wire {
            tag: "conduit-Conduit".into(),
            sock: PathBuf::from("/s/c.sock"),
        }];
        let m = machines();
        let w = wiring_with(None, false, Some(&m), &ws);
        let out = edit_domain(VIRT_INSTALL, &w).unwrap();
        let mut root = Element::parse(out.as_bytes()).unwrap();
        let dev = root.get_mut_child("devices").unwrap();
        let mut port = 3;
        for n in dev.children.iter_mut() {
            let XMLNode::Element(e) = n else { continue };
            let addr = match e.name.as_str() {
                "channel" => {
                    port += 1;
                    el(
                        "address",
                        &[
                            ("type", "virtio-serial"),
                            ("controller", "0"),
                            ("bus", "0"),
                            ("port", &port.to_string()),
                        ],
                    )
                }
                "filesystem" => {
                    port += 1;
                    el(
                        "address",
                        &[
                            ("type", "pci"),
                            ("domain", "0x0000"),
                            ("bus", "0x0a"),
                            ("slot", "0x00"),
                            ("function", &port.to_string()),
                        ],
                    )
                }
                _ => continue,
            };
            e.children.push(XMLNode::Element(addr));
        }
        let defined = emit(&root).unwrap();
        let again = edit_domain(&defined, &w).unwrap();
        assert_eq!(canon(&again), canon(&defined), "{again}");
        for p in ["port=\"4\"", "port=\"5\"", "port=\"6\""] {
            assert!(again.contains(p), "{p}: {again}");
        }
        assert_eq!(
            again
                .matches("type=\"pci\" domain=\"0x0000\" bus=\"0x0a\"")
                .count(),
            2
        );
        // A share that goes away takes its address with it.
        let none = apply_shares(&again, &[]).unwrap();
        assert_eq!(none.matches("bus=\"0x0a\"").count(), 1, "{none}");
    }

    #[test]
    fn a_missing_conduit_bios_goes_back_to_the_stock_loader() {
        let bios = Path::new("/usr/share/conduit/bios");
        let attached = edit_bios(UEFI, Some(bios), None);
        let gone = |_: &str| false;
        let there = |_: &str| true;
        assert!(restore_missing_bios(&attached, &there).is_none());
        let (fixed, stock) = restore_missing_bios(&attached, &gone).unwrap();
        assert_eq!(stock, "/usr/share/OVMF/OVMF_CODE_4M.fd");
        assert_eq!(os_child(&fixed, "loader").get_text().unwrap(), stock);
        assert!(!meta_vm(&fixed).attributes.contains_key("stock-loader"));
        assert!(
            virt::is_ours(&fixed),
            "still attached, only the firmware changed"
        );
        // The recorded stock loader wins (an ms.fd VM on the secboot image).
        let ms = UEFI.replace("OVMF_CODE_4M.fd", "OVMF_CODE_4M.ms.fd");
        let (_, stock) = restore_missing_bios(&edit_bios(&ms, Some(bios), None), &gone).unwrap();
        assert_eq!(stock, "/usr/share/OVMF/OVMF_CODE_4M.ms.fd");
        // Stock firmware, or no loader: nothing to do.
        assert!(restore_missing_bios(UEFI, &gone).is_none());
        assert!(restore_missing_bios(DOMAIN, &gone).is_none());
    }

    #[test]
    fn a_firmware_swap_on_a_tpm_vm_warns_about_sealed_keys() {
        let bios = Path::new("/usr/share/conduit/bios");
        let m = machines();
        let w = wiring_with(Some(bios), false, Some(&m), &[]);
        let tpm = UEFI.replace(
            "</devices>",
            "<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm></devices>",
        );
        let n = notes(&tpm, &w);
        assert!(n.iter().any(|n| n == bios::TPM_WARNING), "{n:?}");
        assert!(
            !notes(UEFI, &w).iter().any(|n| n.contains("PCR0")),
            "no TPM"
        );
        // Re-attach with the BIOS in place: the firmware does not change.
        let again = edit_domain(&tpm, &w).unwrap();
        assert!(!notes(&again, &w).iter().any(|n| n.contains("PCR0")));
        // No Conduit BIOS installed: nothing changes, nothing to warn about.
        let w = wiring_with(None, false, Some(&m), &[]);
        assert!(!notes(&tpm, &w).iter().any(|n| n.contains("PCR0")));
    }

    #[test]
    fn backups_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("conduit-backup-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("libvirt-backup-x.xml");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        sys::write_private(&f, "<domain><graphics passwd='secret'/></domain>").unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(std::fs::read_to_string(&f).unwrap().contains("secret"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_libvirt_without_video_resolution_gets_the_definition_without_it() {
        let out = edit_bios(UEFI, None, Some((5120, 1440)));
        assert!(out.contains("<resolution"));
        let plain = without_resolution(&out).unwrap();
        assert!(!plain.contains("resolution"), "{plain}");
        assert_eq!(canon(&without_resolution(&plain).unwrap()), canon(&plain));
        assert!(refused_resolution(
            "error: XML document failed to validate against schema: Element model has extra content: resolution"
        ));
        assert!(!refused_resolution(
            "error: unsupported configuration: maxphysaddr"
        ));
    }
}
