//! The libvirt domain for a guided install (Omarchy and other ISO installs):
//! UEFI without secure boot, a virtio qcow2 disk, user-mode network, VNC on
//! 127.0.0.1, the QEMU guest agent channel and the installer ISO on a CD drive.
//! `conduit attach` later swaps in Conduit's GPU and console.

use crate::virt::{esc, Virsh};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SESSION_URI: &str = "qemu:///session";
/// The CD drive's target name; `change-media` ejects it by this.
pub const CD_TARGET: &str = "sda";

pub struct DomainSpec<'a> {
    pub name: &'a str,
    pub disk: &'a Path,
    pub iso: &'a Path,
    pub ram_mib: u64,
    pub cpus: u32,
}

/// The domain definition. Pure: tested without libvirt.
pub fn domain_xml(s: &DomainSpec) -> String {
    format!(
        "<domain type='kvm'>
  <name>{name}</name>
  <memory unit='MiB'>{ram}</memory>
  <vcpu>{cpus}</vcpu>
  <os firmware='efi'>
    <type arch='x86_64' machine='q35'>hvm</type>
    <firmware>
      <feature enabled='no' name='enrolled-keys'/>
      <feature enabled='no' name='secure-boot'/>
    </firmware>
    <boot dev='cdrom'/>
    <boot dev='hd'/>
  </os>
  <features><acpi/><apic/></features>
  <cpu mode='host-passthrough'/>
  <clock offset='utc'/>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='{iso}'/>
      <target dev='{cd}' bus='sata'/>
      <readonly/>
    </disk>
    <interface type='user'>
      <model type='virtio'/>
    </interface>
    <graphics type='vnc' port='-1' autoport='yes'>
      <listen type='address' address='127.0.0.1'/>
    </graphics>
    <video><model type='virtio'/></video>
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <input type='tablet' bus='usb'/>
    <memballoon model='virtio'/>
  </devices>
</domain>
",
        name = esc(s.name),
        ram = s.ram_mib,
        cpus = s.cpus,
        disk = esc(&s.disk.to_string_lossy()),
        iso = esc(&s.iso.to_string_lossy()),
        cd = CD_TARGET,
    )
}

/// Does this definition still have a CD drive with a medium in it?
pub fn has_cd_media(xml: &str) -> bool {
    xml.split("<disk ").skip(1).any(|d| {
        let body = d.split("</disk>").next().unwrap_or("");
        let tag = body.split('>').next().unwrap_or("");
        tag.contains("device='cdrom'") && body.contains("<source ")
    })
}

pub fn disk_path(name: &str) -> PathBuf {
    crate::paths::data_dir()
        .join("disks")
        .join(format!("{name}.qcow2"))
}

/// `conduit setup define NAME --iso FILE`: make the disk (sparse) and define
/// the domain in the user session. Never starts it.
pub fn define(name: &str, iso: &Path, ram_mib: u64, cpus: u32, disk_size: &str) -> Result<()> {
    crate::vm::check_name(name)?;
    if !iso.is_file() {
        bail!("the installer image {} does not exist", iso.display());
    }
    let iso = iso.canonicalize()?;
    let v = Virsh::new(SESSION_URI);
    v.reachable()?;
    if v.exists(name) {
        bail!("libvirt already has a VM called {name}");
    }
    let disk = disk_path(name);
    if disk.exists() {
        bail!(
            "{} exists already; pick another name or remove it",
            disk.display()
        );
    }
    std::fs::create_dir_all(disk.parent().unwrap())?;
    let qemu_img = crate::sys::which("qemu-img")
        .context("qemu-img is missing (Ubuntu: sudo apt install qemu-utils)")?;
    crate::sys::output(
        &qemu_img.to_string_lossy(),
        &["create", "-f", "qcow2", &disk.to_string_lossy(), disk_size],
    )?;
    let xml = domain_xml(&DomainSpec {
        name,
        disk: &disk,
        iso: &iso,
        ram_mib,
        cpus,
    });
    if let Err(e) = v.define(&xml, name) {
        let _ = std::fs::remove_file(&disk);
        return Err(e);
    }
    println!(
        "defined {name} in libvirt ({SESSION_URI}); disk {}",
        disk.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(disk: &'a Path, iso: &'a Path) -> DomainSpec<'a> {
        DomainSpec {
            name: "omarchy",
            disk,
            iso,
            ram_mib: 8192,
            cpus: 4,
        }
    }

    #[test]
    fn xml_has_the_pieces_the_walkthrough_needs() {
        let x = domain_xml(&spec(Path::new("/d/o.qcow2"), Path::new("/i/omarchy.iso")));
        for want in [
            "<name>omarchy</name>",
            "<os firmware='efi'>",
            "<feature enabled='no' name='secure-boot'/>",
            "<driver name='qemu' type='qcow2'/>",
            "<source file='/d/o.qcow2'/>",
            "<target dev='vda' bus='virtio'/>",
            "device='cdrom'",
            "<source file='/i/omarchy.iso'/>",
            "<interface type='user'>",
            "<graphics type='vnc' port='-1' autoport='yes'>",
            "<listen type='address' address='127.0.0.1'/>",
            "<target type='virtio' name='org.qemu.guest_agent.0'/>",
        ] {
            assert!(x.contains(want), "missing {want}\n{x}");
        }
        // Well-formed.
        xmltree::Element::parse(x.as_bytes()).unwrap();
    }

    #[test]
    fn xml_escapes_paths_and_names() {
        let x = domain_xml(&spec(Path::new("/d/a&b.qcow2"), Path::new("/i/it's.iso")));
        assert!(x.contains("a&amp;b.qcow2") && x.contains("it&apos;s.iso"));
        xmltree::Element::parse(x.as_bytes()).unwrap();
    }

    #[test]
    fn cd_media_is_seen_until_ejected() {
        let x = domain_xml(&spec(Path::new("/d/o.qcow2"), Path::new("/i/o.iso")));
        assert!(has_cd_media(&x));
        let ejected = x.replace("      <source file='/i/o.iso'/>\n", "");
        assert!(!has_cd_media(&ejected));
        // The qcow2 disk's source is not a CD's.
        assert!(!has_cd_media(
            "<disk type='file' device='disk'><source file='/d'/></disk>"
        ));
    }
}
