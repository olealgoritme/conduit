//! "Send to Conduit host" in Explorer's Windows 11 context menu: an
//! IExplorerCommand in-process COM server. The sparse package
//! (package/AppxManifest.xml) registers it for files and folders. Invoking it
//! hands the selected paths to `conduit-gpu-tray.exe --send-list <file>`,
//! next to this DLL, which copies them into the default shared folder with
//! Explorer's copy dialog (the same place the classic Send To shortcut goes).
//!
//! The command shows only while a Conduit shared folder is mounted (the drive
//! map under HKLM\SOFTWARE\Conduit\ShareDrives that the mount task keeps).

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::Relaxed};
use windows::core::{implement, Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_POINTER, HMODULE, S_FALSE,
    S_OK,
};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW,
    ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};

/// The command's class; package/AppxManifest.xml names the same id.
pub const CLSID_SEND_TO_CONDUIT: GUID = GUID::from_u128(0xc85db2cf_ceed_424c_9ba0_6b52c576c0f3);

const TITLE: &str = "Send to Conduit host";
const TRAY_EXE: &str = "conduit-gpu-tray.exe";

static MODULE: AtomicIsize = AtomicIsize::new(0);
/// Live objects plus server locks: DllCanUnloadNow says yes at zero.
static LIVE: AtomicUsize = AtomicUsize::new(0);

fn dup(s: &str) -> windows::core::Result<PWSTR> {
    let w: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    unsafe { SHStrDupW(windows::core::PCWSTR(w.as_ptr())) }
}

/// The directory holding this DLL (the install directory), with a trailing
/// backslash.
fn module_dir() -> Option<String> {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(HMODULE(MODULE.load(Relaxed) as _), &mut buf) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    let cut = path.rfind('\\')?;
    Some(path[..=cut].to_string())
}

/// Is any Conduit shared folder mounted as a drive right now?
fn share_mounted() -> bool {
    let sub: Vec<u16> = r"SOFTWARE\Conduit\ShareDrives"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut k = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            windows::core::PCWSTR(sub.as_ptr()),
            0,
            KEY_READ,
            &mut k,
        )
    }
    .is_err()
    {
        return false;
    }
    let present = unsafe { GetLogicalDrives() };
    let mut found = false;
    for i in 0..64 {
        let mut name = [0u16; 128];
        let mut nlen = name.len() as u32;
        let mut data = [0u8; 64];
        let mut dlen = data.len() as u32;
        let r = unsafe {
            RegEnumValueW(
                k,
                i,
                PWSTR(name.as_mut_ptr()),
                &mut nlen,
                None,
                None,
                Some(data.as_mut_ptr()),
                Some(&mut dlen),
            )
        };
        if r.is_err() {
            if r.0 == 259 {
                break; // ERROR_NO_MORE_ITEMS
            }
            continue;
        }
        let tag = String::from_utf16_lossy(&name[..nlen as usize]);
        let letter = data[0].to_ascii_uppercase();
        if tag.starts_with("conduit-")
            && letter.is_ascii_uppercase()
            && present & (1 << (letter - b'A')) != 0
        {
            found = true;
            break;
        }
    }
    unsafe {
        let _ = RegCloseKey(k);
    }
    found
}

/// The selection's file system paths (items without one are skipped).
fn selected_paths(items: &IShellItemArray) -> Vec<String> {
    let mut out = Vec::new();
    let n = unsafe { items.GetCount() }.unwrap_or(0);
    for i in 0..n {
        let Ok(item) = (unsafe { items.GetItemAt(i) }) else {
            continue;
        };
        if let Ok(p) = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
            if let Ok(s) = unsafe { p.to_string() } {
                out.push(s);
            }
            unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
        }
    }
    out
}

/// Writes the paths to a list file in %TEMP% and starts the tray exe on it;
/// the exe deletes the file once read.
fn hand_off(paths: &[String]) -> windows::core::Result<()> {
    let fail = |_| windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
    let exe = module_dir().ok_or(E_POINTER)?.to_string() + TRAY_EXE;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let list =
        std::env::temp_dir().join(format!("conduit-send-{}-{stamp}.txt", std::process::id()));
    std::fs::write(&list, paths.join("\n")).map_err(fail)?;
    if let Err(e) = std::process::Command::new(&exe)
        .arg("--send-list")
        .arg(&list)
        .spawn()
    {
        let _ = std::fs::remove_file(&list);
        return Err(fail(e));
    }
    Ok(())
}

#[implement(IExplorerCommand)]
struct SendCommand;

impl SendCommand {
    fn new() -> Self {
        LIVE.fetch_add(1, Relaxed);
        SendCommand
    }
}

impl Drop for SendCommand {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Relaxed);
    }
}

impl IExplorerCommand_Impl for SendCommand_Impl {
    fn GetTitle(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        dup(TITLE)
    }

    fn GetIcon(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        // The Conduit icon: resource 1 of the tray exe.
        dup(&format!("{}{TRAY_EXE},-1", module_dir().ok_or(E_POINTER)?))
    }

    fn GetToolTip(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_SEND_TO_CONDUIT)
    }

    fn GetState(&self, _: Option<&IShellItemArray>, _slow: BOOL) -> windows::core::Result<u32> {
        Ok(if share_mounted() {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(
        &self,
        items: Option<&IShellItemArray>,
        _: Option<&IBindCtx>,
    ) -> windows::core::Result<()> {
        let paths = items.map(selected_paths).unwrap_or_default();
        if paths.is_empty() {
            return Ok(());
        }
        hand_off(&paths)
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&windows::core::IUnknown>,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if out.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *out = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let cmd: IExplorerCommand = SendCommand::new().into();
        unsafe { cmd.query(riid, out).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
        if lock.as_bool() {
            LIVE.fetch_add(1, Relaxed);
        } else {
            LIVE.fetch_sub(1, Relaxed);
        }
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0 as isize, Relaxed);
    }
    BOOL(1)
}

#[no_mangle]
unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || clsid.is_null() || riid.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if *clsid != CLSID_SEND_TO_CONDUIT {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    factory.query(riid, out)
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Relaxed) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
