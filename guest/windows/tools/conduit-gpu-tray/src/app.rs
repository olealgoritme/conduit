//! The Win32 shell: tray icon, popup window, the channel reader thread.

use crate::gfx::{self, argb, wide, Gfx};
use crate::sys;
use crate::view::{self, Snapshot, Wait};
use gpu_tray::{icon_face, tooltip, IconMetric, LineReader, Model, CHANNEL};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "ConduitGpuTrayWindow";
const MUTEX: &str = "Local\\ConduitGpuTray";
const WM_DATA: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const WM_SHOW: u32 = WM_APP + 3;
const TIMER: usize = 1;
/// Repaints the open popup for the waiting animation.
const ANIM: usize = 2;
const TRAY_ID: u32 = 1;

const ID_SHOW: usize = 1;
const ID_TEMP: usize = 2;
const ID_LOAD: usize = 3;
const ID_AUTOSTART: usize = 4;
const ID_EXIT: usize = 5;

// Channel state, written by the reader thread.
const CH_WAITING: u8 = 0;
const CH_DENIED: u8 = 1;
const CH_OPEN: u8 = 2;

static MODEL: OnceLock<Mutex<Model>> = OnceLock::new();
static CHANNEL_STATE: AtomicU8 = AtomicU8::new(CH_WAITING);
static METRIC_LOAD: AtomicU8 = AtomicU8::new(0);
static ICON: AtomicIsize = AtomicIsize::new(0);
/// The app's window, for threads that need to end it.
static MAIN: AtomicIsize = AtomicIsize::new(0);
static LAST_FACE: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_HIDE: AtomicU64 = AtomicU64::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn model() -> &'static Mutex<Model> {
    MODEL.get_or_init(|| Mutex::new(Model::default()))
}

fn metric() -> IconMetric {
    if METRIC_LOAD.load(Relaxed) == 1 {
        IconMetric::Load
    } else {
        IconMetric::Temp
    }
}

fn tick() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

fn exe_path() -> String {
    let mut buf = [0u16; 520];
    let n = unsafe { GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn snapshot() -> Snapshot {
    let m = model().lock().unwrap_or_else(|e| e.into_inner());
    let live = m.live(Instant::now()).cloned();
    let wait = match (CHANNEL_STATE.load(Relaxed), &m.latest) {
        (CH_DENIED, _) => Wait::Denied,
        (_, Some(_)) => Wait::Quiet,
        _ => Wait::NoChannel,
    };
    Snapshot {
        reading: live,
        wait,
        load: m.load.values().collect(),
        power_w: m.power_w.values().collect(),
        temp_c: m.temp_c.values().collect(),
    }
}

// ------------------------------------------------------------ channel reader

/// Reads the virtio-serial port for good: opens it (it exists once the VM has
/// the channel and the guest has the VirtIO serial driver), reads lines, and
/// starts over when it goes away.
fn reader(hwnd: usize) {
    let path = wide(&format!(r"\\.\Global\{CHANNEL}"));
    loop {
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let denied = unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
            if denied && !sys::elevated() && sys::relaunch_elevated(&exe_path()) {
                // The elevated copy takes over (the port is admin-only).
                let h = MAIN.load(Relaxed);
                if h != 0 {
                    unsafe { DestroyWindow(h as HWND) };
                    unsafe { PostMessageW(h as HWND, WM_QUIT, 0, 0) };
                }
                std::thread::sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            CHANNEL_STATE.store(if denied { CH_DENIED } else { CH_WAITING }, Relaxed);
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        CHANNEL_STATE.store(CH_OPEN, Relaxed);
        let mut lines = LineReader::default();
        let mut buf = [0u8; 4096];
        loop {
            let mut n = 0u32;
            let ok = unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, null_mut()) };
            if ok == 0 || n == 0 {
                break;
            }
            for r in lines.push(&buf[..n as usize]) {
                model()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ingest(r, Instant::now());
                unsafe { PostMessageW(hwnd as HWND, WM_DATA, 0, 0) };
            }
        }
        unsafe { CloseHandle(h) };
        CHANNEL_STATE.store(CH_WAITING, Relaxed);
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ----------------------------------------------------------------- tray icon

fn small_icon_size() -> i32 {
    unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16)
}

/// The icon for the current reading: a rounded square in the heat colour with
/// the number on it; a dim green placeholder with no reading.
fn build_icon(face: Option<(String, (u8, u8, u8))>) -> Option<isize> {
    let size = small_icon_size();
    let s = size as f32;
    gfx::make_icon(size, |g| match face {
        Some((text, c)) => {
            g.fill_rrect(argb(255, c), 0.0, 0.0, s, s, s * 0.24);
            let lum = 0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32;
            let ink = if lum > 125.0 {
                (12, 14, 16)
            } else {
                (255, 255, 255)
            };
            let px = if text.len() >= 3 { s * 0.58 } else { s * 0.80 };
            g.text(
                &text,
                argb(255, ink),
                px,
                true,
                gfx::Align::Center,
                true,
                (0.0, 0.0, s, s),
            );
        }
        None => {
            g.fill_rrect(argb(255, (46, 52, 58)), 0.0, 0.0, s, s, s * 0.24);
            g.fill_rrect(
                argb(255, (118, 185, 0)),
                s * 0.22,
                s * 0.62,
                s * 0.56,
                s * 0.12,
                s * 0.06,
            );
        }
    })
}

fn tray_data(hwnd: HWND, icon: isize, tip: &str) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon as HICON;
    for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *d = s;
    }
    nid
}

/// Redraw the tray icon and tooltip when what they show changed.
fn refresh_tray(hwnd: HWND, force: bool) {
    let snap = model().lock().unwrap_or_else(|e| e.into_inner());
    let live = snap.live(Instant::now()).cloned();
    drop(snap);
    let face = live.as_ref().and_then(|r| icon_face(r, metric()));
    let key = match &face {
        Some((t, c)) => {
            let mut k = 1u64 << 40 | (c.0 as u64) << 24 | (c.1 as u64) << 16 | (c.2 as u64) << 8;
            for b in t.bytes() {
                k = k.wrapping_mul(131).wrapping_add(b as u64);
            }
            k ^ ((metric() as u64) << 50)
        }
        None => 0,
    };
    let tip = tooltip(live.as_ref());
    if !force && LAST_FACE.swap(key, Relaxed) == key {
        // Same face; keep the tooltip fresh anyway (it changes with power and VRAM).
        let nid = tray_data(hwnd, ICON.load(Relaxed), &tip);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        return;
    }
    LAST_FACE.store(key, Relaxed);
    let Some(new) = build_icon(face) else { return };
    let nid = tray_data(hwnd, new, &tip);
    let old = ICON.swap(new, Relaxed);
    unsafe {
        if old == 0 || force {
            Shell_NotifyIconW(NIM_ADD, &nid);
            // Explorer writes the icon's settings entry shortly after the
            // first add; promote it to the visible tray once it is there.
            std::thread::spawn(|| {
                let mut b = [0u16; 1024];
                let n = GetModuleFileNameW(null_mut(), b.as_mut_ptr(), b.len() as u32);
                let exe = String::from_utf16_lossy(&b[..n as usize]);
                for _ in 0..30 {
                    if sys::promote_tray_icon(&exe) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
        } else {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        if old != 0 {
            DestroyIcon(old as HICON);
        }
    }
}

// --------------------------------------------------------------------- popup

fn monitor_scale(pt: POINT) -> (f32, RECT) {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        (dx as f32 / 96.0, mi.rcWork)
    }
}

/// Where the tray icon is (or the cursor, when Windows will not say).
fn anchor(hwnd: HWND) -> POINT {
    unsafe {
        let id = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: hwnd,
            uID: TRAY_ID,
            guidItem: std::mem::zeroed(),
        };
        let mut r: RECT = std::mem::zeroed();
        if Shell_NotifyIconGetRect(&id, &mut r) == 0 && r.right > r.left {
            return POINT {
                x: (r.left + r.right) / 2,
                y: (r.top + r.bottom) / 2,
            };
        }
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        p
    }
}

fn place(hwnd: HWND, snap: &Snapshot, keep_bottom: Option<i32>) {
    let at = anchor(hwnd);
    let (scale, work) = monitor_scale(at);
    let (w, h) = (
        (view::WIDTH * scale).round() as i32,
        (view::height(snap) * scale).round() as i32,
    );
    let margin = (10.0 * scale) as i32;
    let x = (at.x - w / 2).clamp(
        work.left + margin,
        (work.right - w - margin).max(work.left + margin),
    );
    let below = at.y < (work.top + work.bottom) / 2;
    let mut y = match keep_bottom {
        Some(b) => b - h,
        None if below => work.top + margin,
        None => work.bottom - h - margin,
    };
    y = y.clamp(
        work.top + margin,
        (work.bottom - h - margin).max(work.top + margin),
    );
    unsafe { SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE) };
    round_corners(hwnd, scale);
}

fn round_corners(hwnd: HWND, scale: f32) {
    unsafe {
        let pref: i32 = 2; // DWMWCP_ROUND
        let hr = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &pref as *const i32 as *const _,
            4,
        );
        let dark: i32 = 1;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const i32 as *const _,
            4,
        );
        let border: u32 = 0x0030_2a26; // COLORREF 0x00BBGGRR
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border as *const u32 as *const _,
            4,
        );
        if hr != 0 {
            // Before Windows 11: cut the corners ourselves.
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut r);
            let d = (16.0 * scale) as i32;
            SetWindowRgn(
                hwnd,
                CreateRoundRectRgn(0, 0, r.right + 1, r.bottom + 1, d, d),
                1,
            );
        }
    }
}

fn show_popup(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            return;
        }
        let snap = snapshot();
        place(hwnd, &snap, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        InvalidateRect(hwnd, null(), 0);
    }
}

fn hide_popup(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
            LAST_HIDE.store(tick(), Relaxed);
        }
    }
}

fn toggle_popup(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            hide_popup(hwnd);
        } else if tick().saturating_sub(LAST_HIDE.load(Relaxed)) > 250 {
            // Clicking the icon deactivates (hides) the popup first; do not reopen it.
            show_popup(hwnd);
        }
    }
}

/// Fit the open popup to what it now shows (full panel or waiting message).
fn refit(hwnd: HWND) {
    unsafe {
        let snap = snapshot();
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut r);
        let mut c: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut c);
        let scale = c.right as f32 / view::WIDTH;
        let want = (view::height(&snap) * scale).round() as i32;
        if want != c.bottom {
            place(hwnd, &snap, Some(r.bottom));
        }
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut c: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut c);
        let (w, h) = (c.right.max(1), c.bottom.max(1));
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp);
        if let Some(g) = Gfx::from_hdc(mem) {
            g.scale(w as f32 / view::WIDTH);
            view::draw(&g, &snapshot());
        }
        BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

// ----------------------------------------------------------------- menu etc.

fn context_menu(hwnd: HWND) {
    unsafe {
        let m = CreatePopupMenu();
        let add = |id: usize, text: &str, checked: bool| {
            let t = wide(text);
            AppendMenuW(
                m,
                MF_STRING | if checked { MF_CHECKED } else { 0 },
                id,
                t.as_ptr(),
            );
        };
        add(ID_SHOW, "Show", false);
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(
            ID_TEMP,
            "Icon shows temperature",
            metric() == IconMetric::Temp,
        );
        add(ID_LOAD, "Icon shows load", metric() == IconMetric::Load);
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(ID_AUTOSTART, "Start with Windows", sys::autostart_enabled());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(ID_EXIT, "Exit", false);
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            p.x,
            p.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(m);
        match cmd as usize {
            ID_SHOW => {
                hide_popup(hwnd);
                show_popup(hwnd);
            }
            ID_TEMP | ID_LOAD => {
                METRIC_LOAD.store((cmd as usize == ID_LOAD) as u8, Relaxed);
                sys::write_setting("IconMetric", (cmd as usize == ID_LOAD) as u32);
                refresh_tray(hwnd, false);
            }
            ID_AUTOSTART => sys::set_autostart(!sys::autostart_enabled(), &exe_path()),
            ID_EXIT => {
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp as u32) & 0xFFFF {
                WM_LBUTTONUP => toggle_popup(hwnd),
                WM_RBUTTONUP => context_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_DATA => {
            refresh_tray(hwnd, false);
            if IsWindowVisible(hwnd) != 0 {
                refit(hwnd);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_TIMER if wp == ANIM => {
            if IsWindowVisible(hwnd) != 0 && CHANNEL_STATE.load(Relaxed) != CH_OPEN {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_TIMER => {
            // Catches a feed that went quiet, and keeps the open popup honest.
            refresh_tray(hwnd, false);
            if IsWindowVisible(hwnd) != 0 {
                refit(hwnd);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_SHOW => {
            show_popup(hwnd);
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_ACTIVATE => {
            if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                hide_popup(hwnd);
            }
            0
        }
        WM_KEYDOWN if wp as u32 == 0x1B => {
            hide_popup(hwnd);
            0
        }
        // The popup is a tool window the user never closes; Alt+F4 hides it.
        WM_CLOSE => {
            hide_popup(hwnd);
            0
        }
        WM_DESTROY => {
            let nid = tray_data(hwnd, 0, "");
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        m if m == TASKBAR_CREATED.load(Relaxed) && m != 0 => {
            // Explorer restarted: the icon is gone; add it again.
            refresh_tray(hwnd, true);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub fn run() {
    unsafe {
        // One instance per session: a second launch asks the first to show itself.
        let name = wide(MUTEX);
        let mut mutex = CreateMutexW(null(), 0, name.as_ptr());
        // An elevated copy started by a hand-off waits for the other to exit.
        let mut tries = 0;
        while !mutex.is_null()
            && GetLastError() == ERROR_ALREADY_EXISTS
            && sys::elevated()
            && tries < 25
        {
            CloseHandle(mutex);
            std::thread::sleep(Duration::from_millis(200));
            mutex = CreateMutexW(null(), 0, name.as_ptr());
            tries += 1;
        }
        if !mutex.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let cls = wide(CLASS);
            let other = FindWindowW(cls.as_ptr(), null());
            if !other.is_null() {
                PostMessageW(other, WM_SHOW, 0, 0);
            }
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        gfx::startup();
        METRIC_LOAD.store(
            sys::read_setting("IconMetric").unwrap_or(0).min(1) as u8,
            Relaxed,
        );
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Relaxed,
        );

        let hinst = GetModuleHandleW(null());
        let cls = wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hIcon = logo() as _;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            cls.as_ptr(),
            wide("Conduit GPU").as_ptr(),
            WS_POPUP,
            0,
            0,
            400,
            300,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        refresh_tray(hwnd, true);
        MAIN.store(hwnd as isize, Relaxed);
        SetTimer(hwnd, TIMER, 1000, None);
        SetTimer(hwnd, ANIM, 70, None);
        let h = hwnd as usize;
        std::thread::spawn(move || reader(h));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The embedded Conduit icon (resource 1), loaded once at 64 px.
pub fn logo() -> isize {
    use std::sync::OnceLock;
    static LOGO: OnceLock<isize> = OnceLock::new();
    *LOGO.get_or_init(|| unsafe {
        LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, 64, 64, 0) as isize
    })
}
