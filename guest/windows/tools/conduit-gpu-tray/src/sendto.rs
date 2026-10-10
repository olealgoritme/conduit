//! Explorer's "Send to > Conduit host": a shortcut to the default share's drive
//! root in the user's SendTo folder, kept in step with the mounted shares.
//! Explorer does the copying itself.
//!
//! Also the copy behind the Windows 11 context menu's "Send to Conduit host"
//! (conduit_shell_menu.dll, which starts this exe with `--send-list <file>`)
//! and behind drops on the popup.

use crate::gfx::wide;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};

const LINK_NAME: &str = "Conduit host.lnk";

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const IID_ISHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const IID_IPERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

type Fn1 = unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT;

/// IUnknown plus IShellLinkW, up to the slots used (the order is the ABI).
#[repr(C)]
struct ShellLinkVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_path: usize,
    get_id_list: usize,
    set_id_list: usize,
    get_description: usize,
    set_description: Fn1,
    get_working_directory: usize,
    set_working_directory: usize,
    get_arguments: usize,
    set_arguments: usize,
    get_hotkey: usize,
    set_hotkey: usize,
    get_show_cmd: usize,
    set_show_cmd: usize,
    get_icon_location: usize,
    set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    set_relative_path: usize,
    resolve: usize,
    set_path: Fn1,
}

/// IUnknown plus IPersistFile, up to Save.
#[repr(C)]
struct PersistFileVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_class_id: usize,
    is_dirty: usize,
    load: usize,
    save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
}

#[repr(C)]
struct Com<V> {
    vtbl: *const V,
}

/// Write the shortcut `lnk` to `target`, with `icon_exe`'s first icon.
fn write_link(lnk: &Path, target: &str, icon_exe: &str) -> bool {
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let mut p: *mut c_void = null_mut();
        if CoCreateInstance(
            &CLSID_SHELL_LINK,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_ISHELL_LINK_W,
            &mut p,
        ) < 0
            || p.is_null()
        {
            return false;
        }
        let link = p as *mut Com<ShellLinkVtbl>;
        let v = &*(*link).vtbl;
        let mut ok = (v.set_path)(p, wide(target).as_ptr()) >= 0;
        (v.set_description)(p, wide("The Conduit host's shared folder").as_ptr());
        (v.set_icon_location)(p, wide(icon_exe).as_ptr(), 0);
        let mut f: *mut c_void = null_mut();
        if ok && (v.query_interface)(p, &IID_IPERSIST_FILE, &mut f) >= 0 && !f.is_null() {
            let file = f as *mut Com<PersistFileVtbl>;
            let fv = &*(*file).vtbl;
            ok = (fv.save)(f, wide(&lnk.to_string_lossy()).as_ptr(), 1) >= 0;
            (fv.release)(f);
        } else {
            ok = false;
        }
        (v.release)(p);
        ok
    }
}

fn link_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join(r"Microsoft\Windows\SendTo")
            .join(LINK_NAME),
    )
}

/// What was last applied: outer `None` before the first call, then the target
/// (or `None` when the shortcut was removed).
static APPLIED: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Make the SendTo shortcut match `target` (a drive root, "Z:\\"), or remove
/// it. Cheap when nothing changed: one file-exists check.
pub fn sync(target: Option<&str>, icon_exe: &str) {
    let Some(lnk) = link_path() else { return };
    let mut applied = APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    let want = target.map(str::to_string);
    let exists = lnk.exists();
    if applied.as_ref() == Some(&want) && exists == want.is_some() {
        return;
    }
    let done = match &want {
        Some(t) => {
            if let Some(dir) = lnk.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            write_link(&lnk, t, icon_exe)
        }
        None => !exists || std::fs::remove_file(&lnk).is_ok(),
    };
    if done {
        *applied = Some(want);
    }
}

/// Copy `files` into the folder `dest` ("Z:\\") with Explorer's copy dialog
/// (progress, name clash questions). False when it failed or was cancelled.
pub fn copy_into(files: &[String], dest: &str) -> bool {
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FO_COPY, SHFILEOPSTRUCTW,
    };
    let from = gpu_tray::path_list(files);
    let to = gpu_tray::path_list(&[dest.to_string()]);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_COPY;
    op.pFrom = from.as_ptr();
    op.pTo = to.as_ptr();
    op.fFlags = (FOF_NOCONFIRMMKDIR | FOF_ALLOWUNDO) as u16;
    let r = unsafe { SHFileOperationW(&mut op) };
    r == 0 && op.fAnyOperationsAborted == 0
}

/// `--send-list <file>`: copy the paths listed in `file` (one per line) into
/// the default shared folder, then delete the list.
pub fn send_list(file: &Path) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let _ = std::fs::remove_file(file);
    let files: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if files.is_empty() {
        return;
    }
    let shares = crate::sys::mounted_shares();
    match gpu_tray::default_share(&shares) {
        Some(dest) => {
            copy_into(&files, &dest.root());
        }
        None => unsafe {
            MessageBoxW(
                null_mut(),
                wide("No Conduit shared folder is mounted to copy to.").as_ptr(),
                wide("Conduit").as_ptr(),
                MB_OK | MB_ICONWARNING,
            );
        },
    }
}
