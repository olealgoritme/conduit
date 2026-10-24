//! The upscaler override (`upscaler` on the control channel, the viewer
//! menu's Upscaler row), independent of Windows.
//!
//! The mode lives in `HKLM\SOFTWARE\Helios` `Upscaler` (REG_SZ, one of
//! [`conduit_ctl::UPSCALER_MODES`]). What follows it:
//!
//! - `dlss` also sets `Dlss` = 1 (NVK's VK_NVX_binary_import, which NGX
//!   needs) and asks NGX for the newest model (DRS override, read by the
//!   guest's nvapi; see guest/nvk-rm/docs/dlss.md).
//! - Games with OptiScaler (a per-game install the user makes; GPL-3.0, not
//!   shipped by Conduit) are listed under `HKLM\SOFTWARE\Helios\OptiScaler`,
//!   one REG_SZ per game holding the path of its `OptiScaler.ini`; each of
//!   those files gets the mode's outputs ([`optiscaler_ini`]).

use conduit_ctl::UPSCALER_MODES;

/// The mode word, lower-cased and checked; `None` when it is not one.
pub fn mode(word: &str) -> Option<&'static str> {
    let w = word.trim().to_ascii_lowercase();
    UPSCALER_MODES.iter().copied().find(|m| *m == w)
}

/// OptiScaler.ini values per mode: (section, key, value).
///
/// - FSR: `fsr31` on every API (native DX11, DX12 and Vulkan FSR 3.1; on
///   RDNA4 OptiScaler upgrades it to FSR4). Not `fsr31_12`: that runs a DX12
///   device beside a DX11 game (D3D11on12), which needs resources shared
///   between Conduit's D3D11 and D3D12 drivers.
/// - XeSS: `xess` on DX12 and Vulkan; DX11 only has XeSS natively on Intel
///   Arc, so `xess_12` there (D3D11on12, same caveat).
/// - DLSS: `dlss` everywhere and render preset K (11) for every quality
///   mode: the transformer model, which needs DLSS 310.1 or newer.
/// - default: `auto` for all of them (OptiScaler's own defaults).
pub fn optiscaler_values(mode: &str) -> Vec<(&'static str, &'static str, &'static str)> {
    let (dx11, dx12, vk) = match mode {
        "fsr" => ("fsr31", "fsr31", "fsr31"),
        "xess" => ("xess_12", "xess", "xess"),
        "dlss" => ("dlss", "dlss", "dlss"),
        _ => ("auto", "auto", "auto"),
    };
    let (preset_on, preset) = if mode == "dlss" {
        ("true", "11")
    } else {
        ("auto", "auto")
    };
    vec![
        ("Upscalers", "Dx11Upscaler", dx11),
        ("Upscalers", "Dx12Upscaler", dx12),
        ("Upscalers", "VulkanUpscaler", vk),
        ("DLSS", "RenderPresetOverride", preset_on),
        ("DLSS", "RenderPresetForAll", preset),
    ]
}

/// `ini` (an OptiScaler.ini) with the mode's values: an existing `Key=`
/// line in its section is rewritten, a missing key is added at the start of
/// its section, a missing section at the end. Everything else (comments,
/// other keys, the file's line endings) stays as it was.
pub fn optiscaler_ini(ini: &str, mode: &str) -> String {
    let crlf = ini.contains("\r\n");
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = ini.lines().map(str::to_string).collect();
    for (section, key, value) in optiscaler_values(mode) {
        set_value(&mut lines, section, key, value);
    }
    let mut out = lines.join(nl);
    if ini.ends_with('\n') || ini.is_empty() {
        out.push_str(nl);
    }
    out
}

fn section_of(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with(';') || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim())
}

fn set_value(lines: &mut Vec<String>, section: &str, key: &str, value: &str) {
    let mut in_section = false;
    let mut header = None;
    for (i, line) in lines.iter_mut().enumerate() {
        if let Some(s) = section_of(line) {
            in_section = s.eq_ignore_ascii_case(section);
            if in_section && header.is_none() {
                header = Some(i);
            }
            continue;
        }
        if in_section && key_of(line).is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            *line = format!("{key}={value}");
            return;
        }
    }
    match header {
        Some(h) => lines.insert(h + 1, format!("{key}={value}")),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.push(format!("{key}={value}"));
        }
    }
}

/// The OptiScaler.ini path a registry value names, when it is one: an
/// absolute Windows path ending in `OptiScaler.ini` (any case), no `..`.
pub fn ini_path_ok(p: &str) -> bool {
    let b = p.as_bytes();
    let absolute = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    absolute
        && p.to_ascii_lowercase().ends_with("\\optiscaler.ini")
        && !p.split('\\').any(|c| c == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[Upscalers]\r\n; Select Upscaler for Dx11 games\r\nDx11Upscaler=auto\r\n\r\nDx12Upscaler=auto\r\nVulkanUpscaler=auto\r\n\r\n[DLSS]\r\nEnabled=auto\r\nRenderPresetOverride=auto\r\nRenderPresetForAll=auto\r\n";

    #[test]
    fn modes() {
        assert_eq!(mode("DLSS"), Some("dlss"));
        assert_eq!(mode(" fsr "), Some("fsr"));
        assert_eq!(mode("fsr4"), None);
        assert_eq!(mode(""), None);
    }

    #[test]
    fn fsr_rewrites_the_outputs_and_keeps_the_rest() {
        let out = optiscaler_ini(SAMPLE, "fsr");
        assert!(out.contains("Dx11Upscaler=fsr31\r\n"));
        assert!(out.contains("Dx12Upscaler=fsr31\r\n"));
        assert!(out.contains("VulkanUpscaler=fsr31\r\n"));
        assert!(out.contains("; Select Upscaler for Dx11 games\r\n"));
        assert!(out.contains("Enabled=auto\r\n"));
        assert!(out.contains("RenderPresetOverride=auto\r\n"));
        assert_eq!(out.lines().count(), SAMPLE.lines().count());
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn dlss_sets_preset_k_and_default_undoes_it() {
        let d = optiscaler_ini(SAMPLE, "dlss");
        assert!(d.contains("Dx12Upscaler=dlss\r\n"));
        assert!(d.contains("RenderPresetOverride=true\r\n"));
        assert!(d.contains("RenderPresetForAll=11\r\n"));
        let back = optiscaler_ini(&d, "default");
        assert_eq!(back, SAMPLE);
    }

    #[test]
    fn xess_on_dx11_goes_through_dx12() {
        let out = optiscaler_ini(SAMPLE, "xess");
        assert!(out.contains("Dx11Upscaler=xess_12\r\n"));
        assert!(out.contains("Dx12Upscaler=xess\r\n"));
    }

    #[test]
    fn missing_keys_and_sections_are_added() {
        let out = optiscaler_ini(
            "[Upscalers]\nDx12Upscaler=xess\n\n[Log]\nLogLevel=2\n",
            "fsr",
        );
        assert!(out.starts_with(
            "[Upscalers]\nVulkanUpscaler=fsr31\nDx11Upscaler=fsr31\nDx12Upscaler=fsr31\n"
        ));
        assert!(out.contains("[Log]\nLogLevel=2\n"));
        assert!(
            out.contains("\n[DLSS]\nRenderPresetForAll=auto\nRenderPresetOverride=auto\n")
                || out.contains("\n[DLSS]\nRenderPresetOverride=auto\nRenderPresetForAll=auto\n")
        );
        // a key of the same name in another section is not touched
        let other = optiscaler_ini("[XeSS]\nDx12Upscaler=keep\n", "fsr");
        assert!(other.starts_with("[XeSS]\nDx12Upscaler=keep\n"));
        // comments that look like keys stay comments
        let c = optiscaler_ini("[Upscalers]\n;Dx11Upscaler=auto\n", "fsr");
        assert!(c.contains(";Dx11Upscaler=auto\n"));
        assert!(optiscaler_ini("", "default").contains("[Upscalers]\n"));
    }

    #[test]
    fn ini_paths() {
        assert!(ini_path_ok(r"C:\Games\Foo\OptiScaler.ini"));
        assert!(ini_path_ok(
            r"G:\SteamLibrary\steamapps\common\X\bin\optiscaler.INI"
        ));
        assert!(!ini_path_ok(r"OptiScaler.ini"));
        assert!(!ini_path_ok(r"C:\Windows\System32\drivers\etc\hosts"));
        assert!(!ini_path_ok(r"C:\Games\..\Windows\OptiScaler.ini"));
        assert!(!ini_path_ok(r"\\server\share\OptiScaler.ini"));
    }
}
