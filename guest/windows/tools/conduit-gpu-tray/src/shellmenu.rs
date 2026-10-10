//! Registers the sparse package that puts "Send to Conduit host" in Explorer's
//! Windows 11 context menu (conduit_shell_menu.dll, ConduitShellMenu.msix next
//! to this exe; guest/windows/tools/conduit-shell-menu). A package registers
//! per user, so this runs at every start (the logon task starts the app for
//! each user) and registers once per user and package file.
//!
//! The registration goes straight to the package manager
//! (Windows.Management.Deployment.PackageManager): no PowerShell, whose
//! module path the user controls, runs with the tray's elevated token.

use std::os::windows::fs::MetadataExt;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Foundation::{AsyncStatus, IAsyncOperationWithProgress, Uri};
use windows::Management::Deployment::{
    AddPackageOptions, DeploymentProgress, DeploymentResult, PackageManager,
};

const PACKAGE_FILE: &str = "ConduitShellMenu.msix";
/// The package's identity name (package/AppxManifest.xml).
const PACKAGE_NAME: &str = "Conduit.ShellMenu";
/// HKCU setting: the package file (size and time) last registered.
const SETTING: &str = "ShellMenuPackage";
/// The user's package repository: one subkey per registered package.
const REPOSITORY: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
/// A deployment step that takes longer is cancelled.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

type Op = IAsyncOperationWithProgress<DeploymentResult, DeploymentProgress>;

/// Waits for `op` up to `STEP_TIMEOUT` (cancelling it after that).
fn wait(op: &Op, what: &str) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        match op.Status().map_err(|e| format!("{what}: {e}"))? {
            AsyncStatus::Started => {}
            AsyncStatus::Completed => {
                let r = op.GetResults().map_err(|e| format!("{what}: {e}"))?;
                let code = r.ExtendedErrorCode().map(|c| c.0).unwrap_or(0);
                if code < 0 {
                    let text = r.ErrorText().map(|t| t.to_string()).unwrap_or_default();
                    return Err(format!("{what}: 0x{code:08x} {text}"));
                }
                return Ok(());
            }
            s => {
                let r = op.GetResults().ok();
                let text = r
                    .and_then(|r| r.ErrorText().ok())
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                return Err(format!("{what}: status {} {text}", s.0));
            }
        }
        if t0.elapsed() > STEP_TIMEOUT {
            let _ = op.Cancel();
            return Err(format!("{what}: timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Removes this user's registrations of the package, then registers
/// `msix` with `dir` as its external location.
fn register(msix: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    let pm = PackageManager::new().map_err(|e| format!("package manager: {e}"))?;
    // "" is the current user.
    let mine = pm
        .FindPackagesByUserSecurityId(&HSTRING::new())
        .map_err(|e| format!("listing packages: {e}"))?;
    for p in mine {
        let Ok(id) = p.Id() else { continue };
        if id.Name().is_ok_and(|n| n == PACKAGE_NAME) {
            let full = id.FullName().map_err(|e| e.to_string())?;
            wait(
                &pm.RemovePackageAsync(&full).map_err(|e| e.to_string())?,
                "removing the old package",
            )?;
        }
    }
    let uri = |p: &std::path::Path| {
        Uri::CreateUri(&HSTRING::from(gpu_tray::policy::file_uri(
            &p.to_string_lossy(),
        )))
        .map_err(|e| format!("{}: {e}", p.display()))
    };
    let opts = AddPackageOptions::new().map_err(|e| e.to_string())?;
    opts.SetExternalLocationUri(&uri(dir)?)
        .map_err(|e| e.to_string())?;
    wait(
        &pm.AddPackageByUriAsync(&uri(msix)?, &opts)
            .map_err(|e| format!("adding the package: {e}"))?,
        "adding the package",
    )
}

/// In the background: register the package for this user when this package
/// file has not been registered yet or the package is gone (replacing an
/// older registration).
pub fn ensure_registered() {
    std::thread::spawn(|| {
        let mut exe = vec![0u16; 32768];
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
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
        }
        match register(&msix, dir) {
            Ok(()) => crate::sys::write_setting_str(SETTING, &stamp),
            Err(e) => crate::ctl::log(&format!("context menu package: {e}")),
        }
    });
}
