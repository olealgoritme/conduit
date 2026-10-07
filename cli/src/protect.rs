//! Keeping a guest from taking the host's desktop down with it.
//!
//! A guest's video memory comes out of the same card as the host's own. When
//! that card also drives a monitor, a guest that fills it leaves the desktop's
//! next allocation to fail, and the compositor with it. So the backend gets a
//! video-memory cap when safe mode is on or the owner asks for one
//! (`gpu.vram_limit_mib`). [`final_limit_mib`] is the one rule that computes
//! the number; the backend enforces it as given.
//!
//! One rule decides what is on by default: [`protection`], from the loaded
//! driver. The setup Conduit was built and tested on (open kernel modules,
//! release 580 or newer) is `Proven` and runs exactly as it always did; any
//! other is `Untested` and starts in safe mode. `gpu.safe_mode` and
//! `gpu.vram_limit_mib` override it either way.
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
    // RTX 50 (Blackwell). Left out: 5060 Ti (8 or 16 GB), the GB203 5070 and
    // the GB205 5060 (not sure of their size), and the 5090 D V2.
    (0x2b85, 32768), // RTX 5090
    (0x2b87, 32768), // RTX 5090 D
    (0x2c02, 16384), // RTX 5080
    (0x2c05, 16384), // RTX 5070 Ti
    (0x2f04, 12288), // RTX 5070
    (0x2d05, 8192),  // RTX 5060
    (0x2d83, 8192),  // RTX 5050
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

/// How much Conduit has been proven on the loaded driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// Open kernel modules, release 580 or newer: nothing changes.
    Proven,
    /// Anything else, or no driver to ask: safe mode is on unless switched off.
    Untested,
}

pub fn protection(d: Option<&crate::host::Driver>) -> Protection {
    match d {
        Some(d) if crate::host::untested_because(d).is_none() => Protection::Proven,
        _ => Protection::Untested,
    }
}

/// Whether safe mode is on, and why. `env` is `CONDUIT_SAFE_MODE` (`1` on, `0`
/// off); then the setting; then `auto`, which follows the driver.
pub fn safe_mode(
    env: Option<&str>,
    setting: crate::config::SafeSetting,
    protection: Protection,
) -> bool {
    use crate::config::SafeSetting::*;
    match (env, setting) {
        (Some("1"), _) => true,
        (Some("0"), _) => false,
        (_, On) => true,
        (_, Off) => false,
        (_, Auto) => protection == Protection::Untested,
    }
}

/// What safe mode holds a guest's video memory to, whatever else is set.
pub const SAFE_MODE_VRAM_MIB: u64 = 2048;

/// The one rule for the backend's video-memory limit, and the only place it
/// is computed: the backend enforces the number it is given and has no rule
/// of its own.
///
/// The limit is the smallest of
///   - `gpu.vram_limit_mib`, when it is a number;
///   - the display default (`display_default`), when it applies: setting
///     `auto`, or unset with safe mode on, and the card drives a monitor;
///   - [`SAFE_MODE_VRAM_MIB`], when safe mode is on.
///
/// `off` drops the first two, so with safe mode off it means no limit. Safe
/// mode's 2 GiB is a ceiling nothing here raises, a larger number and `off`
/// included: to lift it, switch safe mode off (`gpu.safe_mode false`).
pub fn final_limit_mib(
    setting: crate::config::VramSetting,
    safe_mode: bool,
    display_default: Option<u64>,
) -> Option<u64> {
    use crate::config::VramSetting::*;
    let owner = match setting {
        Off => None,
        Mib(n) => Some(n),
        Auto => display_default,
        Default => safe_mode.then_some(display_default).flatten(),
    };
    [owner, safe_mode.then_some(SAFE_MODE_VRAM_MIB)]
        .into_iter()
        .flatten()
        .min()
}

/// `--vram-limit-mib` for the backend: [`final_limit_mib`] with the display
/// default read from sysfs (only when the setting could use it).
pub fn vram_limit_mib(
    drm_root: &Path,
    setting: crate::config::VramSetting,
    safe_mode: bool,
) -> Option<u64> {
    let display_default = nvidia_card(drm_root)
        .filter(Card::drives_display)
        .map(|card| default_limit_mib(card.total_vram_mib()));
    final_limit_mib(setting, safe_mode, display_default)
}

/// What a backend start gets: the flag and the environment.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub vram_limit_mib: Option<u64>,
    pub safe_mode: bool,
}

impl Plan {
    /// Read from this computer: the loaded driver, the settings, sysfs.
    pub fn current() -> Plan {
        let safe_mode = safe_mode(
            std::env::var("CONDUIT_SAFE_MODE").ok().as_deref(),
            crate::config::safe_setting(),
            protection(crate::host::driver().as_ref()),
        );
        Plan {
            vram_limit_mib: vram_limit_mib(
                Path::new("/sys/class/drm"),
                crate::config::vram_limit_mib(),
                safe_mode,
            ),
            safe_mode,
        }
    }

    pub fn apply(&self, cmd: &mut std::process::Command) {
        if let Some(mib) = self.vram_limit_mib {
            cmd.arg("--vram-limit-mib").arg(mib.to_string());
        }
        if self.safe_mode {
            cmd.env("CONDUIT_SAFE_MODE", "1");
        } else {
            // The backend reads this variable; an inherited `1` must not
            // switch on what the settings turned off.
            cmd.env_remove("CONDUIT_SAFE_MODE");
        }
    }
}

/// The flags and environment every backend start gets.
pub fn apply(cmd: &mut std::process::Command) {
    Plan::current().apply(cmd);
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
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto, false), Some(6144));
    }

    #[test]
    fn a_headless_card_is_not_capped_by_default() {
        let t = drm("0x2783", &[("DP-1", "disconnected", "disabled")]);
        assert!(!nvidia_card(&t.0).unwrap().drives_display());
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto, false), None);
    }

    #[test]
    fn the_setting_overrides_the_default_either_way() {
        let t = drm("0x2783", &[("DP-2", "connected", "enabled")]);
        assert!(nvidia_card(&t.0).unwrap().display_active());
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Off, false), None);
        assert_eq!(
            vram_limit_mib(&t.0, VramSetting::Mib(2000), false),
            Some(2000)
        );
        let bare = drm("0x2783", &[]);
        assert_eq!(
            vram_limit_mib(&bare.0, VramSetting::Mib(2000), false),
            Some(2000)
        );
    }

    #[test]
    fn an_unknown_card_on_a_display_gets_the_fixed_cap() {
        let t = drm("0x9999", &[("DP-1", "connected", "enabled")]);
        assert_eq!(
            vram_limit_mib(&t.0, VramSetting::Auto, false),
            Some(UNKNOWN_CARD_MIB)
        );
    }

    #[test]
    fn no_nvidia_card_means_nothing_to_cap() {
        let t = tempdir::T::new();
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Auto, false), None);
    }

    // The single rule: what a start passes to the backend, per driver.

    fn drv(version: &str, open: bool) -> crate::host::Driver {
        crate::host::Driver {
            version: version.into(),
            open,
        }
    }

    /// The Plan a start gets: the owner's settings, the loaded driver and a
    /// display card (RTX 4070 SUPER) with a connected monitor.
    fn plan_for(
        d: &crate::host::Driver,
        vram: VramSetting,
        safe: crate::config::SafeSetting,
        env: Option<&str>,
    ) -> Plan {
        let t = drm("0x2783", &[("DP-2", "connected", "enabled")]);
        let safe_mode = safe_mode(env, safe, protection(Some(d)));
        Plan {
            vram_limit_mib: vram_limit_mib(&t.0, vram, safe_mode),
            safe_mode,
        }
    }

    fn command_line(p: &Plan) -> (Vec<String>, Option<String>, bool) {
        let mut cmd = std::process::Command::new("conduit-backend");
        cmd.env("CONDUIT_SAFE_MODE", "inherited");
        p.apply(&mut cmd);
        let args = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let env = cmd.get_envs().find(|(k, _)| *k == "CONDUIT_SAFE_MODE");
        let set = env
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        (args, set, env.is_some_and(|(_, v)| v.is_none()))
    }

    use crate::config::SafeSetting;

    #[test]
    fn the_proven_setup_with_a_monitor_and_no_settings_gets_nothing() {
        for d in [drv("610.57.04", true), drv("580.65.06", true)] {
            assert_eq!(protection(Some(&d)), Protection::Proven);
            let p = plan_for(&d, VramSetting::Default, SafeSetting::Auto, None);
            assert_eq!(
                p,
                Plan {
                    vram_limit_mib: None,
                    safe_mode: false
                }
            );
            let (args, set, removed) = command_line(&p);
            assert!(args.is_empty(), "{args:?}");
            assert_eq!(set, None);
            assert!(
                removed,
                "no inherited CONDUIT_SAFE_MODE reaches the backend"
            );
        }
    }

    #[test]
    fn closed_565_77_with_a_monitor_gets_safe_mode_and_2_gib_not_half_the_card() {
        for d in [
            drv("565.77", false),
            drv("595.104.02", false),
            drv("535.1", true),
        ] {
            assert_eq!(protection(Some(&d)), Protection::Untested);
            let p = plan_for(&d, VramSetting::Default, SafeSetting::Auto, None);
            assert_eq!(
                p,
                Plan {
                    vram_limit_mib: Some(SAFE_MODE_VRAM_MIB),
                    safe_mode: true
                }
            );
            let (args, set, _) = command_line(&p);
            assert_eq!(args, ["--vram-limit-mib", "2048"]);
            assert_eq!(set.as_deref(), Some("1"));
        }
        assert_eq!(protection(None), Protection::Untested);
    }

    #[test]
    fn explicit_settings_win_over_auto_both_ways() {
        let proven = drv("610.57.04", true);
        let closed = drv("565.77", false);
        // Forced on for the proven driver.
        let p = plan_for(&proven, VramSetting::Default, SafeSetting::On, None);
        assert_eq!((p.safe_mode, p.vram_limit_mib), (true, Some(2048)));
        // Forced off for the untested one: nothing reaches the backend.
        let p = plan_for(&closed, VramSetting::Default, SafeSetting::Off, None);
        assert_eq!((p.safe_mode, p.vram_limit_mib), (false, None));
        // The environment is an explicit setting too.
        assert!(plan_for(&proven, VramSetting::Default, SafeSetting::Auto, Some("1")).safe_mode);
        assert!(!plan_for(&closed, VramSetting::Default, SafeSetting::Auto, Some("0")).safe_mode);
        // A number or `auto` caps even a proven driver; `off` cannot lift
        // safe mode's 2 GiB, only switching safe mode off does.
        let p = plan_for(&proven, VramSetting::Mib(3000), SafeSetting::Auto, None);
        assert_eq!((p.safe_mode, p.vram_limit_mib), (false, Some(3000)));
        let p = plan_for(&proven, VramSetting::Auto, SafeSetting::Auto, None);
        assert_eq!((p.safe_mode, p.vram_limit_mib), (false, Some(6144)));
        let p = plan_for(&closed, VramSetting::Off, SafeSetting::Auto, None);
        assert_eq!((p.safe_mode, p.vram_limit_mib), (true, Some(2048)));
        let (_, set, _) = command_line(&p);
        assert_eq!(set.as_deref(), Some("1"));
    }

    #[test]
    fn a_headless_untested_card_gets_the_safe_mode_cap() {
        let t = drm("0x2783", &[("DP-1", "disconnected", "disabled")]);
        assert_eq!(vram_limit_mib(&t.0, VramSetting::Default, true), Some(2048));
    }

    /// The one rule, as a table: (setting, safe mode, display default) -> the
    /// smallest of the owner's number, the display default and 2 GiB.
    #[test]
    fn the_final_limit_is_the_smallest_of_number_display_default_and_safe_mode() {
        use VramSetting::*;
        let half = Some(16384); // a monitor on a 32 GiB card
        for (setting, safe, display, want) in [
            // Safe mode with a monitor: 2 GiB, not half the card (the defect).
            (Default, true, half, Some(2048)),
            (Default, true, None, Some(2048)),
            (Auto, true, half, Some(2048)),
            // A smaller number wins; a larger one does not raise the cap.
            (Mib(1024), true, half, Some(1024)),
            (Mib(8192), true, half, Some(2048)),
            (Off, true, half, Some(2048)),
            // A small card's display default is below 2 GiB.
            (Default, true, Some(1024), Some(1024)),
            // Safe mode off: the owner's number, the default for `auto`, or none.
            (Default, false, half, None),
            (Auto, false, half, half),
            (Auto, false, None, None),
            (Mib(8192), false, half, Some(8192)),
            (Off, false, half, None),
        ] {
            assert_eq!(
                final_limit_mib(setting, safe, display),
                want,
                "{setting:?} safe={safe} display={display:?}"
            );
        }
    }
}
