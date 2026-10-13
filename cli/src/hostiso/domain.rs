//! A libvirt domain's CPU facts (vCPU count, `<vcpupin>`, `<emulatorpin>`,
//! iothreads) and the `<cputune>` edit `conduit attach` makes for host
//! isolation.

use super::cpus::CpuSet;
use super::policy::Vm;
use xmltree::{Element, XMLNode};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DomainCpus {
    pub vcpus: u32,
    /// Per vCPU; `None` = no `<vcpupin>` for it.
    pub pins: Vec<Option<CpuSet>>,
    pub emulatorpin: Option<CpuSet>,
    /// iothread ids (`<iothreads>N</iothreads>` = 1..=N, or `<iothreadids>`).
    pub iothreads: Vec<u32>,
}

impl DomainCpus {
    pub fn vm(&self) -> Vm {
        Vm {
            vcpus: self.vcpus,
            pins: self.pins.clone(),
        }
    }

    /// The pinning as vCPU i -> one CPU, when every vCPU has exactly one.
    #[cfg(test)]
    pub fn one_cpu_each(&self) -> Option<Vec<u32>> {
        self.pins
            .iter()
            .map(|p| p.as_ref().filter(|c| c.len() == 1).and_then(CpuSet::first))
            .collect()
    }
}

/// libvirt's cpuset syntax: a kernel list with `^N` exclusions (`0-7,^3`).
pub fn parse_libvirt_cpuset(s: &str) -> Option<CpuSet> {
    let mut plus = Vec::new();
    let mut minus = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.strip_prefix('^') {
            Some(x) => minus.push(x),
            None => plus.push(part),
        }
    }
    let p = CpuSet::parse(&plus.join(","))?;
    let m = CpuSet::parse(&minus.join(","))?;
    Some(p.minus(&m))
}

fn text(e: &Element) -> String {
    e.children
        .iter()
        .filter_map(|n| match n {
            XMLNode::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn children<'a>(e: &'a Element, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
    e.children.iter().filter_map(move |n| match n {
        XMLNode::Element(c) if c.name == name => Some(c),
        _ => None,
    })
}

pub fn read(xml: &str) -> Option<DomainCpus> {
    let root = Element::parse(xml.as_bytes()).ok()?;
    read_root(&root)
}

pub fn read_root(root: &Element) -> Option<DomainCpus> {
    let vcpus: u32 = text(root.get_child("vcpu")?).parse().ok()?;
    let mut pins = vec![None; vcpus as usize];
    let mut emulatorpin = None;
    if let Some(ct) = root.get_child("cputune") {
        for p in children(ct, "vcpupin") {
            let (Some(i), Some(c)) = (
                p.attributes
                    .get("vcpu")
                    .and_then(|v| v.parse::<usize>().ok()),
                p.attributes
                    .get("cpuset")
                    .and_then(|c| parse_libvirt_cpuset(c)),
            ) else {
                continue;
            };
            if i < pins.len() {
                pins[i] = Some(c);
            }
        }
        emulatorpin = ct
            .get_child("emulatorpin")
            .and_then(|e| e.attributes.get("cpuset"))
            .and_then(|c| parse_libvirt_cpuset(c));
    }
    let mut iothreads: Vec<u32> = root
        .get_child("iothreadids")
        .map(|ids| {
            children(ids, "iothread")
                .filter_map(|t| t.attributes.get("id")?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    if iothreads.is_empty() {
        if let Some(n) = root
            .get_child("iothreads")
            .and_then(|e| text(e).parse::<u32>().ok())
        {
            iothreads = (1..=n).collect();
        }
    }
    Some(DomainCpus {
        vcpus,
        pins,
        emulatorpin,
        iothreads,
    })
}

fn el(name: &str, attrs: &[(&str, &str)]) -> Element {
    let mut e = Element::new(name);
    for (k, v) in attrs {
        e.attributes.insert((*k).into(), (*v).into());
    }
    e
}

/// Make `<cputune>` pin vCPU i to `pinning[i]` and QEMU's own threads
/// (emulator, iothreads) to `host`. Other `<cputune>` settings stay; an
/// existing pinning is replaced. Idempotent.
pub fn set_pinning(root: &mut Element, pinning: &[u32], host: &CpuSet) {
    let iothreads = read_root(root).map(|d| d.iothreads).unwrap_or_default();
    if root.get_child("cputune").is_none() {
        // After <vcpu>, where libvirt writes it.
        let at = root
            .children
            .iter()
            .position(|n| matches!(n, XMLNode::Element(e) if e.name == "vcpu"))
            .map_or(root.children.len(), |i| i + 1);
        root.children
            .insert(at, XMLNode::Element(Element::new("cputune")));
    }
    let ct = root.get_mut_child("cputune").unwrap();
    ct.children.retain(|n| {
        !matches!(n, XMLNode::Element(e) if matches!(e.name.as_str(), "vcpupin" | "emulatorpin" | "iothreadpin"))
            && !matches!(n, XMLNode::Text(t) if t.trim().is_empty())
    });
    let mut add: Vec<XMLNode> = Vec::new();
    for (i, c) in pinning.iter().enumerate() {
        add.push(XMLNode::Element(el(
            "vcpupin",
            &[("vcpu", &i.to_string()), ("cpuset", &c.to_string())],
        )));
    }
    let h = host.to_string();
    add.push(XMLNode::Element(el("emulatorpin", &[("cpuset", &h)])));
    for t in iothreads {
        add.push(XMLNode::Element(el(
            "iothreadpin",
            &[("iothread", &t.to_string()), ("cpuset", &h)],
        )));
    }
    // vcpupin, emulatorpin and iothreadpin come first in libvirt's schema order.
    let rest = std::mem::take(&mut ct.children);
    ct.children = add;
    ct.children.extend(rest);
}

/// Put back `<cputune>`'s pinning from `original` (another definition of the
/// same domain): its vcpupin/emulatorpin/iothreadpin, or none.
pub fn restore_pinning(root: &mut Element, original: &Element) {
    let pin = |n: &XMLNode| matches!(n, XMLNode::Element(e) if matches!(e.name.as_str(), "vcpupin" | "emulatorpin" | "iothreadpin"));
    let orig: Vec<XMLNode> = original
        .get_child("cputune")
        .map(|c| c.children.iter().filter(|n| pin(n)).cloned().collect())
        .unwrap_or_default();
    if let Some(ct) = root.get_mut_child("cputune") {
        ct.children.retain(|n| !pin(n));
        let rest = std::mem::take(&mut ct.children);
        ct.children = orig;
        ct.children.extend(rest);
        let empty = ct
            .children
            .iter()
            .all(|n| matches!(n, XMLNode::Text(t) if t.trim().is_empty()));
        if empty {
            root.children
                .retain(|n| !matches!(n, XMLNode::Element(e) if e.name == "cputune"));
        }
    } else if !orig.is_empty() {
        let mut ct = Element::new("cputune");
        ct.children = orig;
        root.children.push(XMLNode::Element(ct));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = "<domain type='kvm'><name>win11</name><vcpu placement='static'>4</vcpu>\
        <iothreads>1</iothreads>\
        <cputune><vcpupin vcpu='0' cpuset='8'/><vcpupin vcpu='1' cpuset='24'/>\
        <vcpupin vcpu='2' cpuset='9'/><vcpupin vcpu='3' cpuset='25'/>\
        <emulatorpin cpuset='0-7,16-23'/><shares>2048</shares></cputune></domain>";

    fn set(s: &str) -> CpuSet {
        CpuSet::parse(s).unwrap()
    }

    #[test]
    fn reads_pinning_and_iothreads() {
        let d = read(PINNED).unwrap();
        assert_eq!(d.vcpus, 4);
        assert_eq!(d.one_cpu_each(), Some(vec![8, 24, 9, 25]));
        assert_eq!(d.emulatorpin, Some(set("0-7,16-23")));
        assert_eq!(d.iothreads, vec![1]);
        let plain = read("<domain><vcpu>2</vcpu></domain>").unwrap();
        assert_eq!(plain.pins, vec![None, None]);
        assert_eq!(plain.one_cpu_each(), None);
        assert!(read("<domain/>").is_none());
    }

    #[test]
    fn libvirt_cpuset_exclusions() {
        assert_eq!(parse_libvirt_cpuset("0-7,^3"), Some(set("0-2,4-7")));
        assert_eq!(parse_libvirt_cpuset("8"), Some(set("8")));
        assert_eq!(parse_libvirt_cpuset("x"), None);
    }

    #[test]
    fn pinning_edit_is_idempotent_and_keeps_other_tuning() {
        let mut root = Element::parse(PINNED.as_bytes()).unwrap();
        set_pinning(&mut root, &[10, 26, 11, 27], &set("0-7,16-23"));
        let d = read_root(&root).unwrap();
        assert_eq!(d.one_cpu_each(), Some(vec![10, 26, 11, 27]));
        let ct = root.get_child("cputune").unwrap();
        assert!(ct.get_child("shares").is_some());
        assert_eq!(
            ct.get_child("iothreadpin")
                .unwrap()
                .attributes
                .get("cpuset")
                .unwrap(),
            "0-7,16-23"
        );
        let once = format!("{:?}", root);
        set_pinning(&mut root, &[10, 26, 11, 27], &set("0-7,16-23"));
        assert_eq!(format!("{:?}", root), once);

        // A domain without <cputune> gets one right after <vcpu>.
        let mut bare =
            Element::parse(b"<domain><name>x</name><vcpu>2</vcpu><os/></domain>".as_slice())
                .unwrap();
        set_pinning(&mut bare, &[8, 24], &set("0-7"));
        let names: Vec<_> = bare
            .children
            .iter()
            .filter_map(|n| n.as_element().map(|e| e.name.clone()))
            .collect();
        assert_eq!(names, ["name", "vcpu", "cputune", "os"]);
    }

    #[test]
    fn restore_brings_back_the_original_pinning() {
        let original = Element::parse(PINNED.as_bytes()).unwrap();
        let mut root = original.clone();
        set_pinning(&mut root, &[1, 2, 3, 4], &set("0"));
        restore_pinning(&mut root, &original);
        assert_eq!(read_root(&root).unwrap(), read_root(&original).unwrap());
        // Unpinned originally: attach's <cputune> goes away entirely.
        let bare = Element::parse(b"<domain><vcpu>2</vcpu></domain>".as_slice()).unwrap();
        let mut root = bare.clone();
        set_pinning(&mut root, &[8, 24], &set("0-7"));
        restore_pinning(&mut root, &bare);
        assert!(root.get_child("cputune").is_none());
    }
}
