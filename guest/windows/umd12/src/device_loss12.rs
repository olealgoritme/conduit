//! Device removal when the KMD goes away under a live process (D3D12; the
//! D3D11 UMD's `umd/src/device_loss.rs` is the same mechanism).
//!
//! A live driver update or a device restart stops the KMD under every running
//! D3D process. Windows' recovery depends on each process SEEING the device as
//! removed: DWM tears its device down and creates a new one on the restarted
//! adapter only once an API call returns `DXGI_ERROR_DEVICE_REMOVED`. The
//! KMD-view loss guard (`umd_common/bridge/helios_kmdmap.h`, shared with the
//! Venus ICD and librmclient) keeps the process alive when the KMD's mappings
//! vanish, but that alone turned the crash into a silent stall: DWM kept
//! presenting into a lost renderer, nothing reached the screen, and the
//! desktop stayed black until dwm.exe was killed (326.1 device restart).
//!
//! So a D3D12 device records the process's loss epoch when it is created, and
//! ExecuteCommandLists, the queue fence signal/wait and Present check it (and
//! vkd3d's own device status, a non-S_OK `GetDeviceRemovedReason` after
//! `VK_ERROR_DEVICE_LOST`). A removed device reports `D3DDDIERR_DEVICEREMOVED`
//! through the corelayer `pfnSetErrorCb`, which the runtime surfaces as
//! `DXGI_ERROR_DEVICE_REMOVED` / `GetDeviceRemovedReason`.

use core::sync::atomic::{AtomicBool, Ordering};

// `umd_common/bridge/bridge_kmdmap.cpp`, compiled into this DLL by build.rs.
unsafe extern "C" {
    fn helios_kmdmap_c_attach() -> i32;
    fn helios_kmdmap_c_detach();
    fn helios_kmdmap_c_lost(epoch: i32) -> bool;
}

/// Per-device loss watch: the epoch at creation, and whether the removal has
/// been logged. Attaches to the shared table on creation, detaches on drop
/// (so both CreateDevice rollback and DestroyDevice pair it).
pub struct LossWatch {
    epoch: i32,
    logged: AtomicBool,
}

impl LossWatch {
    pub fn new() -> Self {
        // SAFETY: plain call into bridge_kmdmap.cpp; paired with Drop.
        let epoch = unsafe { helios_kmdmap_c_attach() };
        Self {
            epoch,
            logged: AtomicBool::new(false),
        }
    }

    /// The KMD went away since this device was created.
    pub fn kmd_lost(&self) -> bool {
        // SAFETY: plain call into bridge_kmdmap.cpp. A "lost" answer also
        // backs views the KMD unmapped since (rate-limited).
        unsafe { helios_kmdmap_c_lost(self.epoch) }
    }

    /// First observation of the removal (for a one-time log line).
    pub fn first_report(&self) -> bool {
        !self.logged.swap(true, Ordering::Relaxed)
    }
}

impl Drop for LossWatch {
    fn drop(&mut self) {
        // SAFETY: plain call into bridge_kmdmap.cpp, paired with new().
        unsafe { helios_kmdmap_c_detach() }
    }
}
