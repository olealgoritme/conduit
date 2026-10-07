//! Keeping a guest from taking the host's desktop down with it.
//!
//! A guest's video memory comes out of the same card as the host's own. When
//! that card also drives a monitor, a guest that fills it leaves the desktop's
//! next allocation to fail, and the compositor with it. So the backend gets a
//! video-memory cap by default whenever the NVIDIA card has a connected display,
//! unless the owner said otherwise (`gpu.vram_limit_mib`).
//!
//! Everything here reads sysfs and procfs only. Opening `/dev/nvidia*` to ask
//! the driver for its memory size would itself be a GPU call, on the very card
//! being protected, so the size comes from the PCI device id.

use std::path::{Path, PathBuf};

const NVIDIA: u64 = 0x10de;
/// The cap when the card's memory size is not known: fits any card with the
/// 8 GiB a desktop-plus-guest setup is realistic on, and leaves a 6 GiB one 2.
pub const UNKNOWN_CARD_MIB: u64 = 4096;
/// Kept free for the host whatever the fraction says.
const HOST_RESERVE_MIB: u64 = 3072;

/// GeForce device ids (PCI, lowercase hex) with exactly one memory size.
/// Ambiguous ids (RTX 4060 Ti 8/16 GiB, laptop parts) are left out on purpose:
/// a wrong size would set a cap that protects nothing.
const VRAM_MIB: &[(u16, u64)] = &[
    // RTX 20 (Turing)
    (0x1e03, 12288), // RTX 2080 Ti 12 GB
    (0x1e04, 11264), // RTX 2080 Ti
    (0x1e07, 11264), // RTX 2080 Ti Rev. A
    (0x1e81, 8192),  // RTX 2080 SUPER
    (0x1e82, 8192),  // RTX 2080
    (0x1e87, 8192),  // RTX 2080 Rev. A
    (0x1e84, 8192),  // RTX 2070 SUPER
    (0x1ec2, 8192),  // RTX 2070 SUPER
    (0x1ec7, 8192),  // RTX 2070 SUPER
    (0x1f02, 8192),  // RTX 2070
    (0x1f07, 8192),  // RTX 2070 Rev. A
    (0x1f06, 8192),  // RTX 2060 SUPER
    (0x1f42, 8192),  // RTX 2060 SUPER
    (0x1f47, 8192),  // RTX 2060 SUPER
    (0x1f03, 12288), // RTX 2060 12 GB
    (0x1e89, 6144),  // RTX 2060
    (0x1f08, 6144),  // RTX 2060 Rev. A
    // RTX 30 (Ampere)
    (0x2203, 24576), // RTX 3090 Ti
    (0x2204, 24576), // RTX 3090
    (0x2205, 20480), // RTX 3080 Ti 20 GB
    (0x2206, 10240), // RTX 3080 10 GB
    (0x2208, 12288), // RTX 3080 Ti
    (0x220a, 12288), // RTX 3080 12 GB
    (0x2207, 8192),  // RTX 3070 Ti
    (0x2482, 8192),  // RTX 3070 Ti
    (0x248c, 8192),  // RTX 3070 Ti
    (0x2484, 8192),  // RTX 3070
    (0x248d, 8192),  // RTX 3070
    (0x2488, 8192),  // RTX 3070 Lite Hash Rate
    (0x24c8, 8192),  // RTX 3070 GDDR6X
    (0x2414, 8192),  // RTX 3060 Ti
    (0x2486, 8192),  // RTX 3060 Ti
    (0x248e, 8192),  // RTX 3060 Ti
    (0x2489, 8192),  // RTX 3060 Ti Lite Hash Rate
    (0x24c9, 8192),  // RTX 3060 Ti GDDR6X
    (0x24c7, 8192),  // RTX 3060 8 GB
    (0x2503, 12288), // RTX 3060
    (0x2504, 12288), // RTX 3060 Lite Hash Rate
    (0x2509, 12288), // RTX 3060 12 GB Rev. 2
    (0x2582, 8192),  // RTX 3050 8 GB
    (0x2583, 4096),  // RTX 3050 4 GB
    (0x2584, 6144),  // RTX 3050 6 GB
    // RTX 40 (Ada)
    (0x2684, 24576), // RTX 4090
    (0x2685, 24576), // RTX 4090 D
    (0x2704, 16384), // RTX 4080
    (0x2702, 16384), // RTX 4080 SUPER
    (0x2703, 16384), // RTX 4080 SUPER
    (0x2705, 16384), // RTX 4070 Ti SUPER
    (0x2782, 12288), // RTX 4070 Ti
    (0x2783, 12288), // RTX 4070 SUPER
    (0x2786, 12288), // RTX 4070
    (0x2709, 12288), // RTX 4070 (AD103)
    (0x2805, 16384), // RTX 4060 Ti 16 GB
    (0x2808, 8192),  // RTX 4060
    (0x2882, 8192),  // RTX 4060
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    pub name: String,
    pub connected: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The PCI device directory (`/sys/class/drm/cardN/device`).
    pub pci: PathBuf,
    pub device_id: u16,
    pub connectors: Vec<Connector>,
}

impl Card {
    /// Whether a monitor is attached to this card. `connected` and not
    /// `enabled`: a connected monitor that is asleep or not yet lit by the
    /// compositor is still the desktop's to lose.
    pub fn drives_display(&self) -> bool {
        self.connectors.iter().any(|c| c.connected)
    }

    /// A connected monitor the card is lighting right now.
    pub fn display_active(&self) -> bool {
        self.connectors.iter().any(|c| c.connected && c.enabled)
    }

    pub fn total_vram_mib(&self) -> Option<u64> {
        VRAM_MIB
            .iter()
            .find(|(id, _)| *id == self.device_id)
            .map(|(_, m)| *m)
    }

    pub fn bar1_mib(&self) -> Option<u64> {
        bar1_mib(&self.pci)
    }
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// The NVIDIA cards under `drm_root` (`/sys/class/drm`), the one with a
/// connected display first.
pub fn nvidia_card(drm_root: &Path) -> Option<Card> {
    let mut names: Vec<String> = std::fs::read_dir(drm_root)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.strip_prefix("card")
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    names.sort();
    let mut cards = Vec::new();
    for n in names {
        let dev = drm_root.join(&n).join("device");
        if read_trim(&dev.join("vendor")).and_then(|v| hex(&v)) != Some(NVIDIA) {
            continue;
        }
        let device_id = read_trim(&dev.join("device"))
            .and_then(|v| hex(&v))
            .unwrap_or(0) as u16;
        let mut connectors = Vec::new();
        if let Ok(rd) = std::fs::read_dir(drm_root) {
            for e in rd.flatten() {
                let c = e.file_name().to_string_lossy().into_owned();
                if c.strip_prefix(&format!("{n}-")).is_none() {
                    continue;
                }
                connectors.push(Connector {
                    connected: read_trim(&e.path().join("status")).as_deref() == Some("connected"),
                    enabled: read_trim(&e.path().join("enabled")).as_deref() == Some("enabled"),
                    name: c,
                });
            }
        }
        connectors.sort_by(|a, b| a.name.cmp(&b.name));
        cards.push(Card {
            pci: dev,
            device_id,
            connectors,
        });
    }
    let i = cards.iter().position(Card::drives_display).unwrap_or(0);
    (!cards.is_empty()).then(|| cards.swap_remove(i))
}

/// BAR1 (PCI region 1) size in MiB, from the `resource` file.
pub fn bar1_mib(pci: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(pci.join("resource")).ok()?;
    let line = text.lines().nth(1)?;
    let mut f = line.split_whitespace();
    let (start, end) = (hex(f.next()?)?, hex(f.next()?)?);
    (end >= start && end != 0).then(|| (end - start + 1) >> 20)
}

/// What a guest may hold on a card that also drives a display: half of it, and
/// never leaving the host less than 3 GiB; 4 GiB where the size is unknown.
pub fn default_limit_mib(total: Option<u64>) -> u64 {
    match total {
        Some(t) => (t / 2).min(t.saturating_sub(HOST_RESERVE_MIB)).max(1024),
        None => UNKNOWN_CARD_MIB,
    }
}

/// `--vram-limit-mib` for the backend: the setting, or under `auto` the
/// default when the card drives a display (nothing when it does not, or when
/// there is no NVIDIA card to ask).
pub fn vram_limit_mib(drm_root: &Path, setting: crate::config::VramSetting) -> Option<u64> {
    use crate::config::VramSetting::*;
    match setting {
        Off => None,
        Mib(n) => Some(n),
        Auto => {
            let card = nvidia_card(drm_root)?;
            card.drives_display()
                .then(|| default_limit_mib(card.total_vram_mib()))
        }
    }
}

/// The flags and environment every backend start gets.
pub fn apply(cmd: &mut std::process::Command) {
    if let Some(mib) = vram_limit_mib(Path::new("/sys/class/drm"), crate::config::vram_limit_mib())
    {
        cmd.arg("--vram-limit-mib").arg(mib.to_string());
    }
    if crate::config::safe_mode() {
        cmd.env("CONDUIT_SAFE_MODE", "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VramSetting;

    /// A fake /sys/class/drm: card0 is not NVIDIA, card1 is `device_id`, with
    /// the given connectors `(name, status, enabled)`.
    fn drm(device_id: &str, connectors: &[(&str, &str, &str)]) -> tempdir::T {
        let t = tempdir::T::new();
        for (card, vendor, dev) in [
            ("card0", "0x1002", "0x164e"),
            ("card1", "0x10de", device_id),
        ] {
            let d = t.0.join(card).join("device");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(d.join("device"), format!("{dev}\n")).unwrap();
        }
        std::fs::write(
            t.0.join("card1/device/resource"),
            "0x00000000f5000000 0x00000000f5ffffff 0x0000000000040200\n\
             0x00000000a0000000 0x00000000afffffff 0x000000000014220c\n",
        )
        .unwrap();
        for (n, status, enabled) in connectors {
            let c = t.0.join(format!("card1-{n}"));
            std::fs::create_dir_all(&c).unwrap();
            std::fs::write(c.join("status"), format!("{status}\n")).unwrap();
            std::fs::write(c.join("enabled"), format!("{enabled}\n")).unwrap();
        }
        // An unrelated connector on the other card must not count.
        let c = t.0.join("card0-HDMI-A-1");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("status"), "connected\n").unwrap();
        t
    }

    mod tempdir {
        use std::path::PathBuf;
        pub struct T(pub PathBuf);
        impl T {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "conduit-protect-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                T(p)
            }
        }
        impl Drop for T {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn every_device_id_is_listed_once() {
        let mut ids: Vec<u16> = VRAM_MIB.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "a device id is in VRAM_MIB twice");
    }

    #[test]
    fn the_default_leaves_the_desktop_room() {
        assert_eq!(default_limit_mib(Some(12288)), 6144);
        assert_eq!(default_limit_mib(Some(24576)), 12288);
        // Small cards: the 3 GiB reserve wins over half.
        assert_eq!(default_limit_mib(Some(6144)), 3072);
        assert_eq!(default_limit_mib(Some(4096)), 1024);
        assert_eq!(default_limit_mib(None), UNKNOWN_CARD_MIB);
    }

    #[test]
    fn a_card_with_a_connected_monitor_gets_a_cap_even_if_it_is_not_lit() {
        let t = drm(
            "0x2783",
            &[
                ("DP-1", "disconnected", "disabled"),
                ("DP-2", "connected", "disabled"),
            ],
        );
        let card = nvidia_card(&t.0).unwrap();
        assert!(card.drives_display());
        assert!(!card.display_active());
        assert_eq!(card.total_vram_mib(), Some(12288));
        assert_eq!(card.bar1_mib(), Some(256));
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto), Some(6144));
    }

    #[test]
    fn a_headless_card_is_not_capped_by_default() {
        let t = drm("0x2783", &[("DP-1", "disconnected", "disabled")]);
        assert!(!nvidia_card(&t.0).unwrap().drives_display());
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto), None);
    }

    #[test]
    fn the_setting_overrides_the_default_either_way() {
        let t = drm("0x2783", &[("DP-2", "connected", "enabled")]);
        assert!(nvidia_card(&t.0).unwrap().display_active());
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Off), None);
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Mib(2000)), Some(2000));
        let bare = drm("0x2783", &[]);
        assert_eq!(vram_limit_mib(&bare.0, VramSetting::Mib(2000)), Some(2000));
    }

    #[test]
    fn an_unknown_card_on_a_display_gets_the_fixed_cap() {
        let t = drm("0x9999", &[("DP-1", "connected", "enabled")]);
        assert_eq!(
            vram_limit_mib(&t.0, VramSetting::Auto),
            Some(UNKNOWN_CARD_MIB)
        );
    }

    #[test]
    fn no_nvidia_card_means_nothing_to_cap() {
        let t = tempdir::T::new();
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto), None);
    }
}
