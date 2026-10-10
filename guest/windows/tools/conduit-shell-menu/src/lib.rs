//! "Send to Conduit host" in Explorer's Windows 11 context menu: an
//! IExplorerCommand in-process COM server. The sparse package
//! (package/AppxManifest.xml) registers it for files and folders. Invoking it
//! hands the selected paths to `conduit-gpu-tray.exe --send-list <file>`,
//! next to this DLL, which copies them into the default shared folder with
//! Explorer's copy dialog (the same place the classic Send To shortcut goes).
//!
//! The command shows only while a Conduit shared folder is mounted (the drive
//! map under HKLM\SOFTWARE\Conduit\ShareDrives that the mount task keeps).
//!
//! Every entry point Windows calls (the COM methods and the DLL exports) runs
//! inside [`guard`]: a panic becomes `E_UNEXPECTED` instead of unwinding into
//! the host process (the release profile keeps `panic = "unwind"` for this).

#![allow(non_snake_case)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

pub const TITLE: &str = "Send to Conduit host";
pub const TRAY_EXE: &str = "conduit-gpu-tray.exe";
/// The list file's name: `%TEMP%\conduit-send-*.txt` (the tray deletes
/// only files named like this).
pub const LIST_PREFIX: &str = "conduit-send-";

/// Runs `f`; `None` when it panicked.
pub fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

/// Live COM objects (commands and class factories) and server locks:
/// `DllCanUnloadNow` says yes only when both are zero. Releases never go
/// below zero (an unbalanced `LockServer(FALSE)` is ignored).
pub struct Live {
    objects: AtomicUsize,
    locks: AtomicUsize,
}

fn dec(n: &AtomicUsize) -> bool {
    n.fetch_update(SeqCst, SeqCst, |v| v.checked_sub(1)).is_ok()
}

impl Live {
    pub const fn new() -> Live {
        Live {
            objects: AtomicUsize::new(0),
            locks: AtomicUsize::new(0),
        }
    }
    pub fn object_created(&self) {
        self.objects.fetch_add(1, SeqCst);
    }
    pub fn object_dropped(&self) {
        dec(&self.objects);
    }
    pub fn lock(&self) {
        self.locks.fetch_add(1, SeqCst);
    }
    /// False when there was no lock to release.
    pub fn unlock(&self) -> bool {
        dec(&self.locks)
    }
    pub fn can_unload(&self) -> bool {
        self.objects.load(SeqCst) == 0 && self.locks.load(SeqCst) == 0
    }
}

impl Default for Live {
    fn default() -> Self {
        Live::new()
    }
}

/// Appends `arg` to a command line the way `CommandLineToArgvW` reads it
/// back: quoted, backslashes doubled before a quote and at the end.
pub fn push_arg(line: &mut Vec<u16>, arg: &[u16]) {
    const Q: u16 = b'"' as u16;
    const BS: u16 = b'\\' as u16;
    if !line.is_empty() {
        line.push(b' ' as u16);
    }
    line.push(Q);
    let mut slashes = 0usize;
    for &c in arg {
        if c == BS {
            slashes += 1;
            continue;
        }
        let n = if c == Q { slashes * 2 + 1 } else { slashes };
        line.extend(std::iter::repeat_n(BS, n));
        line.push(c);
        slashes = 0;
    }
    line.extend(std::iter::repeat_n(BS, slashes * 2));
    line.push(Q);
}

/// `"<exe>" --send-list "<list>"`, NUL-terminated (CreateProcessW's
/// writable command line).
pub fn command_line(exe: &[u16], list: &[u16]) -> Vec<u16> {
    let mut v = Vec::new();
    push_arg(&mut v, exe);
    v.extend(" --send-list".encode_utf16());
    push_arg(&mut v, list);
    v.push(0);
    v
}

/// The list file's contents: UTF-16LE, one path per line (`\n`). UTF-16
/// keeps every name as the shell gave it, unpaired surrogates included.
pub fn list_bytes(paths: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.extend((b'\n' as u16).to_le_bytes());
        }
        for u in p {
            out.extend(u.to_le_bytes());
        }
    }
    out
}

#[cfg(windows)]
mod com {
    use super::{command_line, guard, list_bytes, Live, LIST_PREFIX, TITLE, TRAY_EXE};
    use std::ffi::c_void;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
    use windows::core::{implement, Interface, GUID, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOTIMPL,
        E_POINTER, E_UNEXPECTED, HMODULE, S_FALSE, S_OK,
    };
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };
    use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
    use windows::Win32::System::Threading::{
        CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::Win32::UI::Shell::{
        IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW,
        ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
    };

    /// The command's class; package/AppxManifest.xml names the same id.
    pub const CLSID_SEND_TO_CONDUIT: GUID = GUID::from_u128(0xc85db2cf_ceed_424c_9ba0_6b52c576c0f3);

    static MODULE: AtomicIsize = AtomicIsize::new(0);
    static LIVE: Live = Live::new();

    /// A COM method body: a panic is `E_UNEXPECTED`.
    fn com<T>(f: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
        guard(f).unwrap_or_else(|| Err(E_UNEXPECTED.into()))
    }

    fn dup(s: &str) -> windows::core::Result<PWSTR> {
        let w: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
        unsafe { SHStrDupW(PCWSTR(w.as_ptr())) }
    }

    /// The directory holding this DLL (the install directory), with a
    /// trailing backslash.
    fn module_dir() -> Option<Vec<u16>> {
        let mut buf = vec![0u16; 32768];
        let n =
            unsafe { GetModuleFileNameW(HMODULE(MODULE.load(Relaxed) as _), &mut buf) } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        let cut = buf[..n].iter().rposition(|&c| c == b'\\' as u16)?;
        buf.truncate(cut + 1);
        Some(buf)
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
                PCWSTR(sub.as_ptr()),
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

    /// The selection's file system paths, as the shell gives them (items
    /// without one, such as virtual folders, are left out).
    fn selected_paths(items: &IShellItemArray) -> Vec<Vec<u16>> {
        let mut out = Vec::new();
        let n = unsafe { items.GetCount() }.unwrap_or(0);
        for i in 0..n {
            let Ok(item) = (unsafe { items.GetItemAt(i) }) else {
                continue;
            };
            if let Ok(p) = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
                let w = unsafe { p.as_wide() }.to_vec();
                if !w.is_empty() {
                    out.push(w);
                }
                unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
            }
        }
        out
    }

    /// Writes the paths to a new list file in %TEMP% and starts the tray exe
    /// on it; the exe deletes the file once read. The child inherits no
    /// handles from the process hosting this DLL.
    fn hand_off(paths: &[Vec<u16>]) -> windows::core::Result<()> {
        let fail = |_| windows::core::Error::from(E_FAIL);
        let mut exe = module_dir().ok_or(windows::core::Error::from(E_POINTER))?;
        exe.extend(TRAY_EXE.encode_utf16());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let list =
            std::env::temp_dir().join(format!("{LIST_PREFIX}{}-{stamp}.txt", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&list)
            .map_err(fail)?;
        if let Err(e) = f.write_all(&list_bytes(paths)) {
            drop(f);
            let _ = std::fs::remove_file(&list);
            return Err(fail(e));
        }
        drop(f);
        let list_w: Vec<u16> = list.as_os_str().encode_wide().collect();
        let mut line = command_line(&exe, &list_w);
        exe.push(0);
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let r = unsafe {
            CreateProcessW(
                PCWSTR(exe.as_ptr()),
                PWSTR(line.as_mut_ptr()),
                None,
                None,
                BOOL(0),
                PROCESS_CREATION_FLAGS(0),
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        };
        if let Err(e) = r {
            let _ = std::fs::remove_file(&list);
            return Err(e);
        }
        unsafe {
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
        Ok(())
    }

    #[implement(IExplorerCommand)]
    struct SendCommand;

    impl SendCommand {
        fn new() -> Self {
            LIVE.object_created();
            SendCommand
        }
    }

    impl Drop for SendCommand {
        fn drop(&mut self) {
            LIVE.object_dropped();
        }
    }

    impl IExplorerCommand_Impl for SendCommand_Impl {
        fn GetTitle(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
            com(|| dup(TITLE))
        }

        fn GetIcon(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
            com(|| {
                // The Conduit icon: resource 1 of the tray exe.
                let dir = module_dir().ok_or(windows::core::Error::from(E_POINTER))?;
                dup(&format!("{}{TRAY_EXE},-1", String::from_utf16_lossy(&dir)))
            })
        }

        fn GetToolTip(&self, _: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
            Err(E_NOTIMPL.into())
        }

        fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
            Ok(CLSID_SEND_TO_CONDUIT)
        }

        fn GetState(&self, _: Option<&IShellItemArray>, _slow: BOOL) -> windows::core::Result<u32> {
            com(|| {
                Ok(if share_mounted() {
                    ECS_ENABLED.0 as u32
                } else {
                    ECS_HIDDEN.0 as u32
                })
            })
        }

        fn Invoke(
            &self,
            items: Option<&IShellItemArray>,
            _: Option<&IBindCtx>,
        ) -> windows::core::Result<()> {
            com(|| {
                let paths = items.map(selected_paths).unwrap_or_default();
                if paths.is_empty() {
                    return Ok(());
                }
                hand_off(&paths)
            })
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

    impl Factory {
        fn new() -> Self {
            LIVE.object_created();
            Factory
        }
    }

    impl Drop for Factory {
        fn drop(&mut self) {
            LIVE.object_dropped();
        }
    }

    impl IClassFactory_Impl for Factory_Impl {
        fn CreateInstance(
            &self,
            outer: Option<&windows::core::IUnknown>,
            riid: *const GUID,
            out: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            com(|| {
                if out.is_null() || riid.is_null() {
                    return Err(E_POINTER.into());
                }
                unsafe { *out = std::ptr::null_mut() };
                if outer.is_some() {
                    return Err(CLASS_E_NOAGGREGATION.into());
                }
                let cmd: IExplorerCommand = SendCommand::new().into();
                unsafe { cmd.query(riid, out).ok() }
            })
        }

        fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
            com(|| {
                if lock.as_bool() {
                    LIVE.lock();
                } else {
                    LIVE.unlock();
                }
                Ok(())
            })
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
        guard(|| {
            if out.is_null() || clsid.is_null() || riid.is_null() {
                return E_POINTER;
            }
            *out = std::ptr::null_mut();
            if *clsid != CLSID_SEND_TO_CONDUIT {
                return CLASS_E_CLASSNOTAVAILABLE;
            }
            let factory: IClassFactory = Factory::new().into();
            factory.query(riid, out)
        })
        .unwrap_or(E_UNEXPECTED)
    }

    #[no_mangle]
    extern "system" fn DllCanUnloadNow() -> HRESULT {
        guard(|| if LIVE.can_unload() { S_OK } else { S_FALSE }).unwrap_or(S_FALSE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// CommandLineToArgvW's rules (arguments after the program name).
    fn argv(line: &[u16]) -> Vec<String> {
        let s = String::from_utf16(line.strip_suffix(&[0]).unwrap_or(line)).unwrap();
        let mut out = Vec::new();
        let mut cur = String::new();
        let (mut quoted, mut any, mut slashes) = (false, false, 0usize);
        for c in s.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    cur.extend(std::iter::repeat_n('\\', slashes / 2));
                    if slashes % 2 == 1 {
                        cur.push('"');
                    } else {
                        quoted = !quoted;
                        any = true;
                    }
                    slashes = 0;
                }
                ' ' | '\t' if !quoted => {
                    cur.extend(std::iter::repeat_n('\\', slashes));
                    slashes = 0;
                    if any || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    any = false;
                }
                c => {
                    cur.extend(std::iter::repeat_n('\\', slashes));
                    slashes = 0;
                    cur.push(c);
                }
            }
        }
        cur.extend(std::iter::repeat_n('\\', slashes));
        if any || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn command_line_round_trips() {
        for (exe, list) in [
            (
                r"C:\Program Files\Conduit\conduit-gpu-tray.exe",
                r"C:\Users\A B\AppData\Local\Temp\conduit-send-1-2.txt",
            ),
            (r"C:\odd dir\", r"C:\x\y z\"),
            (r#"C:\q"uote\a.exe"#, r#"C:\t\\"x.txt"#),
            (r"C:\tab	dir\a.exe", ""),
        ] {
            let line = command_line(&w(exe), &w(list));
            assert_eq!(line.last(), Some(&0));
            assert_eq!(argv(&line), vec![exe, "--send-list", list], "{exe} {list}");
        }
    }

    #[test]
    fn list_is_utf16_lines_and_keeps_lone_surrogates() {
        let odd = vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let b = list_bytes(&[w(r"C:\a b"), odd.clone()]);
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let lines: Vec<&[u16]> = units.split(|&u| u == b'\n' as u16).collect();
        assert_eq!(lines, vec![&w(r"C:\a b")[..], &odd[..]]);
        assert!(list_bytes(&[]).is_empty());
    }

    #[test]
    fn live_counts_objects_and_locks() {
        let l = Live::new();
        assert!(l.can_unload());
        l.object_created(); // a class factory
        assert!(!l.can_unload());
        l.object_created(); // a command
        l.object_dropped();
        assert!(!l.can_unload());
        l.object_dropped();
        assert!(l.can_unload());
        l.lock();
        assert!(!l.can_unload());
        assert!(l.unlock());
        assert!(l.can_unload());
    }

    #[test]
    fn unbalanced_unlock_does_not_underflow() {
        let l = Live::new();
        assert!(!l.unlock());
        l.object_dropped();
        assert!(l.can_unload());
        l.lock();
        assert!(
            !l.can_unload(),
            "the stray unlock must not cancel a real lock"
        );
    }

    #[test]
    fn guard_turns_a_panic_into_none() {
        assert_eq!(guard(|| 5), Some(5));
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let r: Option<u32> = guard(|| panic!("boom"));
        std::panic::set_hook(prev);
        assert_eq!(r, None);
    }
}
