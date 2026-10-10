//! Follow the host viewer's mode requests: poll the KMD's `HostModeSeq` /
//! `HostModeReq` (see `gpu_tray::display_logic`) and switch the Conduit
//! display with `ChangeDisplaySettingsEx`. A driver cannot change the desktop
//! mode itself; this runs in the user's session, where it can.

use gpu_tray::display_logic::{Follower, Step};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_TEST,
    DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISP_CHANGE_SUCCESSFUL, DEVMODEW,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};

const KEY: &str = "SYSTEM\\CurrentControlSet\\Services\\helios_kmd_render";
const POLL: Duration = Duration::from_millis(250);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn read(name: &str) -> Option<u32> {
    let (k, n) = (wide(KEY), wide(name));
    let mut v: u32 = 0;
    let mut len = 4u32;
    // SAFETY: valid strings and out-pointers of the stated size.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut v as *mut u32).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(v)
}

/// The desktop's Conduit display (`\\.\DISPLAYn`), NUL-terminated.
fn conduit_display() -> Option<[u16; 32]> {
    for i in 0..16 {
        // SAFETY: zeroed plain-data struct with its size set.
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        // SAFETY: valid out-pointer; NULL device enumerates adapters.
        if unsafe { EnumDisplayDevicesW(null(), i, &mut dd, 0) } == 0 {
            break;
        }
        let s = String::from_utf16_lossy(&dd.DeviceString);
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0
            && (s.contains("Helios") || s.contains("Conduit"))
        {
            return Some(dd.DeviceName);
        }
    }
    None
}

fn apply(w: u32, h: u32) {
    let Some(dev) = conduit_display() else {
        crate::ctl::log(&format!("display: {w}x{h} asked, no Conduit display on the desktop"));
        return;
    };
    // SAFETY: zeroed plain-data struct with its size set.
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    // SAFETY: valid device name and out-pointer.
    if unsafe { EnumDisplaySettingsW(dev.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm) } == 0 {
        return;
    }
    if dm.dmPelsWidth == w && dm.dmPelsHeight == h {
        return;
    }
    dm.dmPelsWidth = w;
    dm.dmPelsHeight = h;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    // Tested first; then applied for this session only (not saved: native
    // again after a restart, until the viewer asks again).
    // SAFETY: valid device name and mode for both calls.
    let t = unsafe { ChangeDisplaySettingsExW(dev.as_ptr(), &dm, null_mut(), CDS_TEST, null()) };
    let r = if t == DISP_CHANGE_SUCCESSFUL {
        unsafe { ChangeDisplaySettingsExW(dev.as_ptr(), &dm, null_mut(), 0, null()) }
    } else {
        t
    };
    crate::ctl::log(&format!(
        "display: host asked for {w}x{h}: {}",
        if r == DISP_CHANGE_SUCCESSFUL { "switched".to_string() } else { format!("refused ({r})") }
    ));
}

/// The follower thread: forever, a cheap poll of two registry values.
pub(crate) fn follow() {
    let mut f = Follower::default();
    loop {
        if let Step::Apply(w, h) = f.step(read("HostModeSeq"), read("HostModeReq")) {
            apply(w, h);
        }
        std::thread::sleep(POLL);
    }
}
