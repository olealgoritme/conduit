//! Registry: the autostart switch and the tray's one setting.

use crate::gfx::wide;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const SETTINGS: &str = r"Software\Conduit\GpuTray";
pub const VALUE: &str = "ConduitGpuTray";

fn open(root: HKEY, sub: &str, write: bool) -> Option<HKEY> {
    let mut k: HKEY = null_mut();
    let s = wide(sub);
    let r = unsafe {
        if write {
            RegCreateKeyExW(
                root,
                s.as_ptr(),
                0,
                null(),
                0,
                KEY_READ | KEY_WRITE,
                null(),
                &mut k,
                null_mut(),
            )
        } else {
            RegOpenKeyExW(root, s.as_ptr(), 0, KEY_READ, &mut k)
        }
    };
    (r == ERROR_SUCCESS).then_some(k)
}

fn get(root: HKEY, sub: &str, name: &str) -> Option<Vec<u8>> {
    let k = open(root, sub, false)?;
    let n = wide(name);
    let mut len = 0u32;
    let mut out = None;
    unsafe {
        if RegQueryValueExW(k, n.as_ptr(), null(), null_mut(), null_mut(), &mut len)
            == ERROR_SUCCESS
        {
            let mut buf = vec![0u8; len as usize];
            if RegQueryValueExW(
                k,
                n.as_ptr(),
                null(),
                null_mut(),
                buf.as_mut_ptr(),
                &mut len,
            ) == ERROR_SUCCESS
            {
                buf.truncate(len as usize);
                out = Some(buf);
            }
        }
        RegCloseKey(k);
    }
    out
}

fn set(root: HKEY, sub: &str, name: &str, kind: u32, data: &[u8]) -> bool {
    let Some(k) = open(root, sub, true) else {
        return false;
    };
    let n = wide(name);
    let ok = unsafe {
        RegSetValueExW(k, n.as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) == ERROR_SUCCESS
    };
    unsafe { RegCloseKey(k) };
    ok
}

fn bytes_of(s: &str) -> Vec<u8> {
    wide(s).iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// Starts with Windows: a Run entry (this user's or the machine's, which the
/// installer writes) that Explorer's StartupApproved switch has not turned off.
pub fn autostart_enabled() -> bool {
    let run = get(HKEY_CURRENT_USER, RUN, VALUE).is_some()
        || get(HKEY_LOCAL_MACHINE, RUN, VALUE).is_some();
    let off = get(HKEY_CURRENT_USER, APPROVED, VALUE)
        .is_some_and(|d| d.first().is_some_and(|b| b & 1 == 1));
    run && !off
}

pub fn set_autostart(on: bool, exe: &str) {
    if on {
        let has_run = get(HKEY_LOCAL_MACHINE, RUN, VALUE).is_some();
        if !has_run {
            set(
                HKEY_CURRENT_USER,
                RUN,
                VALUE,
                REG_SZ,
                &bytes_of(&format!("\"{exe}\"")),
            );
        }
    }
    // 02 = enabled, 03 = disabled, then 8 bytes of the disable time (unused).
    let mut flag = [0u8; 12];
    flag[0] = if on { 2 } else { 3 };
    set(HKEY_CURRENT_USER, APPROVED, VALUE, REG_BINARY, &flag);
}

pub fn read_setting(name: &str) -> Option<u32> {
    let d = get(HKEY_CURRENT_USER, SETTINGS, name)?;
    Some(u32::from_le_bytes(d.get(..4)?.try_into().ok()?))
}

pub fn write_setting(name: &str, v: u32) {
    set(
        HKEY_CURRENT_USER,
        SETTINGS,
        name,
        REG_DWORD,
        &v.to_le_bytes(),
    );
}

/// Windows 11 keeps each tray icon's placement under
/// HKCU\Control Panel\NotifyIconSettings\<id>; IsPromoted=1 shows it next to
/// the clock instead of in the overflow. Set once, on the first run that
/// finds this exe's entry without the value, so a later "hide" by the user
/// sticks. Returns true once the entry was found.
pub fn promote_tray_icon(exe: &str) -> bool {
    const ROOT: &str = "Control Panel\\NotifyIconSettings";
    let Some(k) = open(HKEY_CURRENT_USER, ROOT, false) else {
        return false;
    };
    let mut names = Vec::new();
    let mut buf = [0u16; 256];
    for i in 0.. {
        let mut len = buf.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                k,
                i,
                buf.as_mut_ptr(),
                &mut len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if r != ERROR_SUCCESS {
            break;
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    unsafe { RegCloseKey(k) };
    let want = exe.to_ascii_lowercase();
    for n in names {
        let sub = format!("{ROOT}\\{n}");
        let Some(p) = get(HKEY_CURRENT_USER, &sub, "ExecutablePath") else {
            continue;
        };
        let w: Vec<u16> = p
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let path = String::from_utf16_lossy(&w)
            .trim_end_matches('\0')
            .to_ascii_lowercase();
        if path != want {
            continue;
        }
        if get(HKEY_CURRENT_USER, &sub, "IsPromoted").is_none() {
            set(
                HKEY_CURRENT_USER,
                &sub,
                "IsPromoted",
                REG_DWORD,
                &1u32.to_le_bytes(),
            );
        }
        return true;
    }
    false
}
