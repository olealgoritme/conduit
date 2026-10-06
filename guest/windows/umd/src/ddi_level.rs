//! Which D3D11 DDI interface this process's adapter advertises.
//!
//! # Why a WDDM 2.3 level exists at all
//!
//! The D3D11 runtime decides most per-format capabilities from its own
//! requirement tables (`d3d11!CD3D11FormatHelper::GetRequirementsTable`, indexed
//! by feature level and by an "extended format features" level derived from the
//! negotiated DDI interface). Only capabilities a table marks *optional* are
//! asked of the driver. Typed UAV on `B8G8R8A8_UNORM` is one of them, and it is
//! optional only in the tables for D3D11 DDI interface >= 0x000b0024
//! (D3DWDDM2_3); below that the runtime reports it unsupported whatever the
//! driver says. Final Fantasy XIV (Dawntrail benchmark) creates a BGRA8
//! SRV|RTV|UAV render target unconditionally, the runtime refuses it with
//! E_INVALIDARG, and the game stops with "A fatal DirectX error has occurred
//! (30000000)" -- on Venus and NVK alike.
//!
//! # Selection, once per process
//!
//!  1. `HELIOS_UMD_DDI` (environment): `2.3` / `23` / `0x24` / `36` selects
//!     WDDM 2.3, `1.3` / `13` / `0x10` / `16` WDDM 1.3. Tests and per-launch.
//!  2. dwm.exe: `HKLM\SOFTWARE\Helios!UmdDdiLevelDwm` (REG_DWORD, absent 0x10).
//!     DWM keeps WDDM 1.3 until the new level is proven elsewhere.
//!  3. `UmdDdiLevel` (REG_DWORD, absent 0x10): 0x24 = WDDM 2.3 everywhere else.
//!  4. `UmdDdiAllowList` (REG_SZ, executable names separated by `;`): these
//!     processes get WDDM 2.3 while `UmdDdiLevel` is still 0x10.
//!
//! WDDM 2.3 is advertised IN ADDITION to the WDDM 1.3 / 11.1 / 11.0 versions
//! (highest first); the runtime picks the highest it supports.

use std::sync::OnceLock;

use crate::log_error;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum DdiLevel {
    Wddm1_3,
    Wddm2_3,
}

const LEVEL_WDDM1_3: u32 = 0x10;
const LEVEL_WDDM2_3: u32 = 0x24;

fn parse(text: &str) -> Option<DdiLevel> {
    match text.trim().to_ascii_lowercase().as_str() {
        "2.3" | "23" | "0x24" | "36" => Some(DdiLevel::Wddm2_3),
        "1.3" | "13" | "0x10" | "16" => Some(DdiLevel::Wddm1_3),
        _ => None,
    }
}

fn from_dword(value: u32) -> DdiLevel {
    if value == LEVEL_WDDM2_3 {
        DdiLevel::Wddm2_3
    } else {
        DdiLevel::Wddm1_3
    }
}

fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

fn list_has(list: &str, exe: &str) -> bool {
    list.split(';')
        .map(|s| s.trim())
        .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(exe))
}

fn decide() -> (DdiLevel, &'static str) {
    if let Some(level) = std::env::var("HELIOS_UMD_DDI").ok().as_deref().and_then(parse) {
        return (level, "HELIOS_UMD_DDI");
    }
    let exe = exe_name();
    if exe == "dwm.exe" {
        return (
            from_dword(crate::knobs::UMD_DDI_LEVEL_DWM.get()),
            "HKLM\\SOFTWARE\\Helios!UmdDdiLevelDwm (dwm.exe)",
        );
    }
    let level = crate::knobs::UMD_DDI_LEVEL.get();
    if level == LEVEL_WDDM2_3 {
        return (DdiLevel::Wddm2_3, "HKLM\\SOFTWARE\\Helios!UmdDdiLevel=0x24");
    }
    if level != LEVEL_WDDM1_3 {
        log_error!("UmdDdiLevel=0x{level:x} is not 0x10 or 0x24: WDDM1.3");
    }
    if let Some(list) = helios_umd_common::knobs::reg_sz(c"UmdDdiAllowList") {
        if list_has(&list, &exe) {
            return (DdiLevel::Wddm2_3, "HKLM\\SOFTWARE\\Helios!UmdDdiAllowList");
        }
    }
    (DdiLevel::Wddm1_3, "default")
}

/// The process's level. Computed and logged on first use.
pub(crate) fn ddi_level() -> DdiLevel {
    static LEVEL: OnceLock<DdiLevel> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let (level, why) = decide();
        log_error!("D3D11 DDI level: {level:?} ({why})");
        level
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_spellings() {
        assert_eq!(parse("2.3"), Some(DdiLevel::Wddm2_3));
        assert_eq!(parse(" 0x24 "), Some(DdiLevel::Wddm2_3));
        assert_eq!(parse("16"), Some(DdiLevel::Wddm1_3));
        assert_eq!(parse("2.4"), None);
    }

    #[test]
    fn allow_list_is_case_insensitive() {
        assert!(list_has("foo.exe; FFXIV_DX11.exe ;bar.exe", "ffxiv_dx11.exe"));
        assert!(!list_has("foo.exe;;", "ffxiv_dx11.exe"));
    }
}
