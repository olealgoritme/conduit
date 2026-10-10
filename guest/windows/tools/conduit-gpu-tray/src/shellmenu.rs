//! Registers the sparse package that puts "Send to Conduit host" in Explorer's
//! Windows 11 context menu (conduit_shell_menu.dll, ConduitShellMenu.msix next
//! to this exe; guest/windows/tools/conduit-shell-menu). A package registers
//! per user, so this runs at every start (the logon task starts the app for
//! each user) and registers once per user and package file.

use std::os::windows::fs::MetadataExt;

const PACKAGE_FILE: &str = "ConduitShellMenu.msix";
/// The package's identity name (package/AppxManifest.xml).
const PACKAGE_NAME: &str = "Conduit.ShellMenu";
/// HKCU setting: the package file (size and time) last registered.
const SETTING: &str = "ShellMenuPackage";
/// The user's package repository: one subkey per registered package.
const REPOSITORY: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";

/// In the background: register the package for this user when this package
/// file has not been registered yet or the package is gone (replacing an
/// older registration).
pub fn ensure_registered() {
    std::thread::spawn(|| {
        let mut exe = [0u16; 1024];
        let n = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(
                std::ptr::null_mut(),
                exe.as_mut_ptr(),
                exe.len() as u32,
            )
        } as usize;
        let exe = std::path::PathBuf::from(String::from_utf16_lossy(&exe[..n]));
        let Some(dir) = exe.parent() else { return };
        let msix = dir.join(PACKAGE_FILE);
        let Ok(meta) = std::fs::metadata(&msix) else {
            return;
        };
        let stamp = format!("{}-{}", meta.file_size(), meta.last_write_time());
        if crate::sys::read_setting_str(SETTING).as_deref() == Some(stamp.as_str())
            && crate::sys::user_subkey_starts_with(REPOSITORY, &format!("{PACKAGE_NAME}_"))
        {
            return;
        }
        let quote = |p: &std::path::Path| p.to_string_lossy().replace('\'', "''");
        let ps = format!(
            "$ErrorActionPreference = 'Stop'; \
             Get-AppxPackage -Name '{PACKAGE_NAME}' | Remove-AppxPackage; \
             Add-AppxPackage -Path '{}' -ExternalLocation '{}'",
            quote(&msix),
            quote(dir)
        );
        if crate::sys::quiet(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", &ps],
        )
        .is_some_and(|o| o.status.success())
        {
            crate::sys::write_setting_str(SETTING, &stamp);
        }
    });
}
