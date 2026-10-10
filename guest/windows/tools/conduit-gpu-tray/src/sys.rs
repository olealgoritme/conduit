//! Registry: the autostart switch and the tray's one setting.

use crate::gfx::wide;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;

const SETTINGS: &str = r"Software\Conduit\GpuTray";

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

/// Is the volume at `root` ("Z:\\") the virtiofs share `tag`? A letter in
/// the drive map is trusted only when the volume there says so (label and
/// file system), not just because some drive has that letter.
fn volume_is_share(root: &str, tag: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let r = wide(root);
    let ok = unsafe {
        GetVolumeInformationW(
            r.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return false;
    }
    let s = |b: &[u16]| {
        String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
    };
    gpu_tray::policy::share_volume_matches(tag, &s(&label), &s(&fs))
}

/// The host shared folders mounted here. The mount task records each tag's
/// drive letter under HKLM\SOFTWARE\Conduit\ShareDrives; only letters that
/// are drives right now and whose volume is that share count.
///
/// The mount task runs as SYSTEM, so a mounted share is a drive every user
/// of this Windows sees, with the same access to the host folder.
pub fn mounted_shares() -> Vec<gpu_tray::Share> {
    use windows_sys::Win32::Foundation::ERROR_NO_MORE_ITEMS;
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let Some(k) = open(HKEY_LOCAL_MACHINE, r"SOFTWARE\Conduit\ShareDrives", false) else {
        return Vec::new();
    };
    let present = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = [0u16; 128];
        let mut nlen = name.len() as u32;
        let mut data = [0u8; 64];
        let mut dlen = data.len() as u32;
        let r = unsafe {
            RegEnumValueW(
                k,
                i,
                name.as_mut_ptr(),
                &mut nlen,
                null(),
                null_mut(),
                data.as_mut_ptr(),
                &mut dlen,
            )
        };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        if r != ERROR_SUCCESS {
            continue;
        }
        let tag = String::from_utf16_lossy(&name[..nlen as usize]);
        let units: Vec<u16> = data[..(dlen as usize).min(64) / 2 * 2]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|&u| u != 0)
            .collect();
        let drive = String::from_utf16_lossy(&units);
        if let Some(s) = gpu_tray::parse_share(&tag, &drive) {
            let bit = s.drive.as_bytes()[0] - b'A';
            if present & (1 << bit) != 0 && volume_is_share(&s.root(), &tag) {
                out.push(s);
            }
        }
    }
    unsafe { RegCloseKey(k) };
    out.sort_by_key(|s| (s.name != gpu_tray::DEFAULT_SHARE, s.name.to_lowercase()));
    out
}

/// The logon task the installer registers: it starts the app with
/// administrator rights (the VirtIO serial port is admin-only) and no UAC
/// prompt, for every user who logs on.
pub const TASK: &str = "ConduitGpuTray";

/// `name` in the System32 folder: system tools are always started by full
/// path, never looked up on PATH (the tray runs elevated).
pub fn system32(name: &str) -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = if n > 0 && n < buf.len() {
        String::from_utf16_lossy(&buf[..n])
    } else {
        r"C:\Windows\System32".to_string()
    };
    PathBuf::from(dir).join(name)
}

/// Runs a System32 tool without a window and waits for it.
pub fn quiet(tool: &str, args: &[&OsStr]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(system32(tool))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
}

fn schtasks(args: &[&str]) -> Option<std::process::Output> {
    let a: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    quiet("schtasks.exe", &a)
}

fn task_status() -> Option<String> {
    let o = schtasks(&["/query", "/tn", TASK, "/fo", "csv", "/nh"])?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Starts with Windows: the logon task exists and is not disabled.
pub fn autostart_enabled() -> bool {
    task_status().is_some_and(|s| !s.contains("Disabled"))
}

/// `%ProgramFiles%\Conduit`, from the known folder (not the environment).
fn install_dir() -> Option<String> {
    let _com = crate::apps::ComGuard::new();
    let pf = crate::apps::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles)?;
    Some(gpu_tray::policy::install_dir(&pf.to_string_lossy()))
}

/// Registers the elevated logon task for `exe` (needs administrator
/// rights). Only an exe in the install directory, which only
/// administrators can change, gets one.
pub fn register_task(exe: &str) -> bool {
    let Some(dir) = install_dir() else {
        return false;
    };
    if !gpu_tray::policy::is_under(exe, &dir) {
        return false;
    }
    // The task definition goes through a file in the install directory
    // (administrators only), as UTF-16 with a BOM like the XML says.
    let xml = PathBuf::from(&dir).join(format!("{TASK}-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(
        gpu_tray::policy::task_xml(exe)
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&xml)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
    if written.is_err() {
        let _ = std::fs::remove_file(&xml);
        return false;
    }
    let ok = quiet(
        "schtasks.exe",
        &[
            OsStr::new("/create"),
            OsStr::new("/tn"),
            OsStr::new(TASK),
            OsStr::new("/xml"),
            xml.as_os_str(),
            OsStr::new("/f"),
        ],
    )
    .is_some_and(|o| o.status.success());
    let _ = std::fs::remove_file(&xml);
    ok
}

pub fn set_autostart(on: bool, exe: &str) {
    if task_status().is_none() {
        if on {
            register_task(exe);
        }
        return;
    }
    let flag = if on { "/enable" } else { "/disable" };
    schtasks(&["/change", "/tn", TASK, flag]);
}

/// Is this process elevated (an administrator token)?
pub fn elevated() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// This process's token elevation type.
pub fn elevation() -> gpu_tray::policy::Elevation {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevationType, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut t: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) == 0 {
            return gpu_tray::policy::Elevation::Unknown;
        }
        let mut v: i32 = 0;
        let mut n = 0u32;
        let ok = GetTokenInformation(
            t,
            TokenElevationType,
            &mut v as *mut i32 as *mut _,
            4,
            &mut n,
        );
        CloseHandle(t);
        if ok == 0 {
            return gpu_tray::policy::Elevation::Unknown;
        }
        gpu_tray::policy::Elevation::from_raw(v)
    }
}

/// Starts an elevated copy: through the logon task when it exists (no
/// prompt), else with one UAC prompt. True when a copy was started (the
/// caller still waits for its window before leaving).
pub fn relaunch_elevated(exe: &str) -> bool {
    if task_status().is_some()
        && schtasks(&["/run", "/tn", TASK]).is_some_and(|o| o.status.success())
    {
        return true;
    }
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide("runas");
    let file = wide(exe);
    let r = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
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

/// Does HKCU\`sub` have a subkey whose name starts with `prefix`?
pub fn user_subkey_starts_with(sub: &str, prefix: &str) -> bool {
    let Some(k) = open(HKEY_CURRENT_USER, sub, false) else {
        return false;
    };
    let mut found = false;
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                k,
                i,
                name.as_mut_ptr(),
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
        if String::from_utf16_lossy(&name[..len as usize]).starts_with(prefix) {
            found = true;
            break;
        }
    }
    unsafe { RegCloseKey(k) };
    found
}

pub fn read_setting_str(name: &str) -> Option<String> {
    let d = get(HKEY_CURRENT_USER, SETTINGS, name)?;
    let units: Vec<u16> = d
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

pub fn write_setting_str(name: &str, v: &str) {
    let bytes: Vec<u8> = wide(v).iter().flat_map(|u| u.to_le_bytes()).collect();
    set(HKEY_CURRENT_USER, SETTINGS, name, REG_SZ, &bytes);
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
        // Explorer writes known folders as their GUID ("{6D80…}\\Conduit\\x.exe"
        // for Program Files): compare what follows it.
        let same = match path.strip_prefix('{').and_then(|r| r.split_once('}')) {
            Some((_, rest)) => want.ends_with(rest),
            None => path == want,
        };
        if !same {
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
