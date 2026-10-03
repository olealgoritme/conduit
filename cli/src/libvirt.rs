//! `conduit attach`: add the Conduit GPU to an existing libvirt VM (QEMU 11.1+).
//!
//! The domain XML gets:
//!   - <emulator>: system QEMU if it is 11.1 or newer, else the bundled one
//!   - <memoryBacking> memfd + shared access (vhost-user maps guest RAM)
//!   - <qemu:commandline> -chardev socket + -device vhost-user-test-device-pci,virtio-id=45
//!     (the generic vhost-user device of QEMU 11.1; see docs/QEMU.md)
//!
//! A libvirt hook starts/stops a backend for that VM.
//!
//! The backend does not speak to QEMU yet (see docs/ROADMAP.md), so the command
//! is gated: `--dry-run` shows the changes; CONDUIT_EXPERIMENTAL_QEMU=1 applies them.

use crate::paths::{self, Tool};
use crate::sys;
use crate::ui::{self, oops};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xmltree::{Element, EmitterConfig, XMLNode};

pub const QEMU_NS: &str = "http://libvirt.org/schemas/domain/qemu/1.0";
pub const CHARDEV_ID: &str = "conduit-gpu";
pub const MIN_QEMU: (u32, u32) = (11, 1);
const HOOK: &str = include_str!("../assets/libvirt-hook.sh");

pub fn gpu_socket(vm: &str) -> PathBuf {
    PathBuf::from(format!("/run/conduit/libvirt/{vm}/gpu.sock"))
}

/// The QEMU arguments Conduit adds.
pub fn qemu_args(sock: &Path) -> Vec<String> {
    vec![
        "-chardev".into(),
        format!("socket,id={CHARDEV_ID},path={}", sock.display()),
        "-device".into(),
        crate::qemu::gpu_device(CHARDEV_ID),
    ]
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

/// Rewrite a libvirt domain XML for Conduit. Idempotent.
pub fn edit_domain(xml: &str, emulator: &Path, sock: &Path) -> Result<String> {
    let mut root =
        Element::parse(xml.as_bytes()).context("libvirt returned XML Conduit cannot read")?;
    if root.name != "domain" {
        anyhow::bail!("not a libvirt domain (root element is <{}>)", root.name);
    }
    // QEMU namespace on the root.
    let mut ns = root
        .namespaces
        .clone()
        .unwrap_or_else(xmltree::Namespace::empty);
    ns.put("qemu", QEMU_NS);
    root.namespaces = Some(ns);

    // <devices><emulator>
    {
        let devices = child_mut(&mut root, "devices");
        let em = child_mut(devices, "emulator");
        em.children = vec![XMLNode::Text(emulator.display().to_string())];
        // libvirt keeps <emulator> first among devices; put it back there.
        let idx = devices
            .children
            .iter()
            .position(|n| matches!(n, XMLNode::Element(e) if e.name == "emulator"))
            .unwrap();
        let node = devices.children.remove(idx);
        devices.children.insert(0, node);
    }

    // <memoryBacking><source type='memfd'/><access mode='shared'/>
    {
        let mb = child_mut(&mut root, "memoryBacking");
        mb.children.retain(
            |n| !matches!(n, XMLNode::Element(e) if e.name == "source" || e.name == "access"),
        );
        let mut src = Element::new("source");
        src.attributes.insert("type".into(), "memfd".into());
        let mut acc = Element::new("access");
        acc.attributes.insert("mode".into(), "shared".into());
        mb.children.push(XMLNode::Element(src));
        mb.children.push(XMLNode::Element(acc));
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
    let args: Vec<Element> = cl
        .children
        .iter()
        .filter_map(|n| match n {
            XMLNode::Element(e) if e.name == "arg" => Some(e.clone()),
            _ => None,
        })
        .collect();
    let mut keep: Vec<Element> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let v = args[i].attributes.get("value").cloned().unwrap_or_default();
        let next = args
            .get(i + 1)
            .and_then(|e| e.attributes.get("value"))
            .cloned()
            .unwrap_or_default();
        let ours = (v == "-chardev" || v == "-device") && next.contains(CHARDEV_ID);
        if ours {
            i += 2;
        } else {
            keep.push(args[i].clone());
            i += 1;
        }
    }
    cl.children
        .retain(|n| !matches!(n, XMLNode::Element(e) if e.name == "arg"));
    for a in keep {
        cl.children.push(XMLNode::Element(a));
    }
    for v in qemu_args(sock) {
        let mut a = Element::new("arg");
        a.prefix = Some("qemu".into());
        a.namespace = Some(QEMU_NS.into());
        a.attributes.insert("value".into(), v);
        cl.children.push(XMLNode::Element(a));
    }
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

fn choose_emulator() -> Result<(PathBuf, String)> {
    if let Some((p, v)) = crate::host::system_qemu() {
        if v >= MIN_QEMU {
            return Ok((p, format!("system QEMU {}.{}", v.0, v.1)));
        }
    }
    let bundled = paths::prefix().join("bin/qemu-system-x86_64");
    if let Some(p) = Tool::BundledQemu.find() {
        return Ok((p, "Conduit's bundled QEMU".into()));
    }
    Ok((bundled, "Conduit's bundled QEMU (not installed yet)".into()))
}

pub fn attach(name: &str, dry_run: bool, uri: Option<&str>) -> Result<()> {
    let experimental = std::env::var("CONDUIT_EXPERIMENTAL_QEMU").as_deref() == Ok("1");
    if !dry_run && !experimental {
        return Err(oops(
            "`conduit attach` is not available yet: the Conduit backend cannot talk to QEMU so far",
            format!(
                "This arrives with QEMU 11.1 support (see the roadmap). Until then use `conduit create` / `conduit import`.\n\
                 To preview what attach would change in the libvirt VM, run: conduit attach {name} --dry-run"
            ),
        ));
    }
    if !sys::have("virsh") {
        return Err(oops(
            "virsh is not installed",
            "Install libvirt's client tools (Ubuntu: sudo apt install libvirt-clients)",
        ));
    }
    let mut base = vec![];
    if let Some(u) = uri {
        base.extend(["-c", u]);
    }
    let dump = |extra: &[&str]| {
        let mut a = base.clone();
        a.extend_from_slice(extra);
        sys::output("virsh", &a)
    };
    let xml = dump(&["dumpxml", "--inactive", "--security-info", name]).map_err(|_| {
        oops(
            format!("libvirt has no VM called \"{name}\""),
            "See your libvirt VMs with `virsh list --all` (add -c qemu:///system if they are system VMs)",
        )
    })?;
    let (emulator, which) = choose_emulator()?;
    let sock = gpu_socket(name);
    let new_xml = edit_domain(&xml, &emulator, &sock)?;

    if dry_run {
        ui::info(format!("emulator: {} ({which})", emulator.display()));
        println!("{new_xml}");
        ui::info("dry run: nothing was changed");
        return Ok(());
    }
    if !emulator.is_file() {
        return Err(oops(
            format!(
                "QEMU {}.{} or newer is needed, and the bundled one is missing",
                MIN_QEMU.0, MIN_QEMU.1
            ),
            "Reinstall the conduit package, or upgrade your system QEMU",
        ));
    }

    // Backup, then define.
    let backup = paths::config_dir()
        .join("libvirt")
        .join(format!("{name}.before-conduit.xml"));
    std::fs::create_dir_all(backup.parent().unwrap())?;
    if !backup.exists() {
        std::fs::write(&backup, &xml)?;
    }
    let tmp = paths::config_dir()
        .join("libvirt")
        .join(format!("{name}.xml"));
    std::fs::write(&tmp, &new_xml)?;
    dump(&["define", tmp.to_str().unwrap()])
        .context("libvirt refused the changed VM definition")?;

    install_hook(name)?;
    println!(
        "Added the Conduit GPU to {name}. The original definition is saved at {}",
        backup.display()
    );
    println!("Inside the VM, install the guest driver: sudo apt install ./conduit-guest_*.deb");
    println!("Then: conduit view {name}");
    Ok(())
}

/// Per-VM settings the hook reads, and the hook itself (libvirt's qemu.d directory).
fn install_hook(name: &str) -> Result<()> {
    let backend = Tool::Backend.require()?;
    let conf = format!(
        "CONDUIT_USER={}\nCONDUIT_BACKEND={}\nCONDUIT_SOCKET={}\n",
        ui::shell_quote(&paths::username()),
        ui::shell_quote(&backend.to_string_lossy()),
        ui::shell_quote(&gpu_socket(name).to_string_lossy()),
    );
    let staged = paths::config_dir().join("libvirt");
    let conf_tmp = staged.join(format!("{name}.conf"));
    let hook_tmp = staged.join("qemu-hook");
    std::fs::write(&conf_tmp, conf)?;
    std::fs::write(&hook_tmp, HOOK)?;
    sys::sudo_ready("Installing a libvirt hook that starts Conduit's GPU backend with the VM.")?;
    sys::sudo(
        "install",
        &[
            "-d",
            "-m755",
            "/etc/conduit/libvirt",
            "/etc/libvirt/hooks/qemu.d",
        ],
    )?;
    sys::sudo(
        "install",
        &[
            "-m644",
            conf_tmp.to_str().unwrap(),
            &format!("/etc/conduit/libvirt/{name}.conf"),
        ],
    )?;
    sys::sudo(
        "install",
        &[
            "-m755",
            hook_tmp.to_str().unwrap(),
            "/etc/libvirt/hooks/qemu.d/conduit",
        ],
    )?;
    ui::info("libvirt hook installed; restart libvirtd once so it notices: sudo systemctl restart libvirtd");
    Ok(())
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
  <devices>
    <disk type='file' device='disk'><source file='/var/lib/libvirt/images/myvm.qcow2'/></disk>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
  </devices>
</domain>"#;

    fn edit(x: &str) -> String {
        edit_domain(
            x,
            Path::new("/opt/conduit/bin/qemu-system-x86_64"),
            &gpu_socket("myvm"),
        )
        .unwrap()
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
        assert_eq!(
            dev.children
                .iter()
                .filter(|n| n.as_element().is_some_and(|e| e.name == "emulator"))
                .count(),
            1
        );

        let mb = root.get_child("memoryBacking").unwrap();
        assert!(
            mb.get_child("hugepages").is_some(),
            "other settings are kept"
        );
        assert_eq!(mb.get_child("source").unwrap().attributes["type"], "memfd");
        assert_eq!(mb.get_child("access").unwrap().attributes["mode"], "shared");

        assert!(out.contains(&format!("xmlns:qemu=\"{QEMU_NS}\"")), "{out}");
        assert!(out.contains("<qemu:commandline>"), "{out}");
        assert!(
            out.contains("value=\"socket,id=conduit-gpu,path=/run/conduit/libvirt/myvm/gpu.sock\"")
        );
        assert!(out.contains(
            "value=\"vhost-user-test-device-pci,chardev=conduit-gpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036\""
        ));
        assert!(out.contains("<name>myvm</name>"));
        assert!(out.contains("myvm.qcow2"));
    }

    #[test]
    fn idempotent_and_keeps_foreign_args() {
        let with_other = DOMAIN.replace(
            "</domain>",
            &format!("<qemu:commandline xmlns:qemu='{QEMU_NS}'><qemu:arg value='-s'/></qemu:commandline></domain>"),
        );
        let once = edit(&with_other);
        let twice = edit(&once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("-chardev").count(), 1);
        assert_eq!(twice.matches("<qemu:commandline").count(), 1);
        assert!(
            twice.contains("value=\"-s\""),
            "user's own args are kept: {twice}"
        );
    }

    #[test]
    fn creates_missing_sections() {
        let out = edit("<domain type='kvm'><name>x</name></domain>");
        let root = Element::parse(out.as_bytes()).unwrap();
        assert!(root
            .get_child("devices")
            .unwrap()
            .get_child("emulator")
            .is_some());
        assert!(root.get_child("memoryBacking").is_some());
    }

    #[test]
    fn rejects_non_domain() {
        assert!(edit_domain("<network/>", Path::new("/q"), Path::new("/s")).is_err());
        assert!(edit_domain("not xml", Path::new("/q"), Path::new("/s")).is_err());
    }
}
