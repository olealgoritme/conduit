//! Installed apps and their icons for the control channel: Start Menu
//! shortcuts and Steam games (the parsing is in `gpu_tray::startmenu` and
//! `gpu_tray::steam`), icons rendered to 64 px PNGs through the shell and GDI+.

use crate::gfx::wide;
use gpu_tray::{png, startmenu, steam};
use std::ffi::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::result::Result::Ok;
use windows_sys::core::GUID;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

const SIZE: i32 = 64;
const FOLDERID_PROGRAMS: GUID = GUID::from_u128(0xA77F5D77_2E2B_44C3_A6A2_ABA601054A51);
const FOLDERID_COMMON_PROGRAMS: GUID = GUID::from_u128(0x0139D44E_6AFE_49F2_8690_3DAFCAE6FFB8);
const IID_IIMAGELIST: GUID = GUID::from_u128(0x46EB5926_582E_4017_9FDF_E8998DAA0950);

pub fn wide_os(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// A known folder such as Downloads.
pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut out: *mut u16 = null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut out) };
    if hr < 0 || out.is_null() {
        return None;
    }
    let mut n = 0;
    // Bounded: a known folder path is at most a few hundred units.
    while n < 32_768 && unsafe { *out.add(n) } != 0 {
        n += 1;
    }
    let s = unsafe { std::slice::from_raw_parts(out, n) };
    let p = PathBuf::from(std::ffi::OsString::from_wide(s));
    unsafe { CoTaskMemFree(out as *const c_void) };
    Some(p)
}

/// COM for the calling thread while alive.
pub struct ComGuard(bool);

impl ComGuard {
    pub fn new() -> ComGuard {
        // Apartment threaded; S_OK or S_FALSE both need the matching uninit.
        ComGuard(unsafe { CoInitializeEx(null(), 2) } >= 0)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn steam_root() -> Option<PathBuf> {
    let key = wide(r"Software\Valve\Steam");
    let name = wide("SteamPath");
    let mut buf = [0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut bytes,
        )
    };
    let from_reg = (r == 0).then(|| {
        let n = buf[..(bytes as usize / 2).min(buf.len())]
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(buf.len());
        PathBuf::from(String::from_utf16_lossy(&buf[..n]).replace('/', "\\"))
    });
    let fallback = std::env::var_os("ProgramFiles(x86)")
        .map(|p| PathBuf::from(p).join("Steam"))
        .or_else(|| std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("Steam")));
    from_reg
        .into_iter()
        .chain(fallback)
        .find(|p| p.join("steamapps").is_dir())
}

/// Start Menu shortcuts (all users, then this user) and Steam games.
pub fn list() -> Result<Vec<conduit_ctl::App>, String> {
    let _com = ComGuard::new();
    let roots: Vec<PathBuf> = [FOLDERID_COMMON_PROGRAMS, FOLDERID_PROGRAMS]
        .iter()
        .filter_map(known_folder)
        .collect();
    let mut apps = startmenu::scan(&roots);
    if let Some(root) = steam_root() {
        apps.extend(steam::scan(&root));
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

/// The icon for an app's `icon` key as a 64 px PNG.
pub fn icon(key: &str) -> Result<Vec<u8>, String> {
    let _com = ComGuard::new();
    if key.starts_with("steam:") {
        let id = steam::icon_key_id(key).ok_or_else(|| format!("bad icon key: {key}"))?;
        let root = steam_root().ok_or("Steam is not installed")?;
        for cand in steam::icon_candidates(&root, id) {
            if cand.is_file() {
                if let Some(png) = from_image_file(&cand) {
                    return Ok(png);
                }
            }
        }
        return shell_icon(&root.join("steam.exe")).ok_or_else(|| format!("no icon for {key}"));
    }
    let p = PathBuf::from(key.replace('/', "\\"));
    if !p.is_absolute() || !p.exists() {
        return Err(format!("not found: {key}"));
    }
    shell_icon(&p).ok_or_else(|| format!("no icon for {key}"))
}

fn from_image_file(p: &Path) -> Option<Vec<u8>> {
    let w = wide_os(p);
    let mut bmp: *mut GpBitmap = null_mut();
    if unsafe { GdipCreateBitmapFromFile(w.as_ptr(), &mut bmp) } != 0 || bmp.is_null() {
        return None;
    }
    let out = render(bmp);
    unsafe { GdipDisposeImage(bmp as *mut GpImage) };
    out
}

/// The shell's icon for a file (the shortcut's own icon for a `.lnk`),
/// the largest the system image list has.
fn shell_icon(p: &Path) -> Option<Vec<u8>> {
    let path = wide_os(p);
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<SHFILEINFOW>() as u32;
    if unsafe { SHGetFileInfoW(path.as_ptr(), 0, &mut sfi, sz, SHGFI_SYSICONINDEX) } == 0 {
        return None;
    }
    // SHIL_JUMBO (256 px), then SHIL_EXTRALARGE (48 px).
    for list in [4, 2] {
        if let Some(h) = image_list_icon(list, sfi.iIcon) {
            let out = from_hicon(h);
            unsafe { DestroyIcon(h) };
            if out.is_some() {
                return out;
            }
        }
    }
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    if unsafe { SHGetFileInfoW(path.as_ptr(), 0, &mut sfi, sz, SHGFI_ICON | SHGFI_LARGEICON) } == 0
        || sfi.hIcon.is_null()
    {
        return None;
    }
    let out = from_hicon(sfi.hIcon);
    unsafe { DestroyIcon(sfi.hIcon) };
    out
}

/// `IImageList::GetIcon` through its vtable (QueryInterface, AddRef,
/// Release, Add, ReplaceIcon, SetOverlayImage, Replace, AddMasked, Draw,
/// Remove, GetIcon: slot 10).
fn image_list_icon(list: i32, index: i32) -> Option<HICON> {
    type GetIcon = unsafe extern "system" fn(*mut c_void, i32, u32, *mut HICON) -> i32;
    type Release = unsafe extern "system" fn(*mut c_void) -> u32;
    let mut il: *mut c_void = null_mut();
    if unsafe { SHGetImageList(list, &IID_IIMAGELIST, &mut il) } < 0 || il.is_null() {
        return None;
    }
    let mut icon: HICON = null_mut();
    unsafe {
        let vtbl = *(il as *const *const usize);
        let get_icon: GetIcon = std::mem::transmute(*vtbl.add(10));
        let release: Release = std::mem::transmute(*vtbl.add(2));
        let hr = get_icon(il, index, 1 /* ILD_TRANSPARENT */, &mut icon);
        release(il);
        (hr >= 0 && !icon.is_null()).then_some(icon)
    }
}

fn from_hicon(h: HICON) -> Option<Vec<u8>> {
    let mut bmp: *mut GpBitmap = null_mut();
    if unsafe { GdipCreateBitmapFromHICON(h, &mut bmp) } != 0 || bmp.is_null() {
        return None;
    }
    let out = render(bmp);
    unsafe { GdipDisposeImage(bmp as *mut GpImage) };
    out
}

const FORMAT_ARGB: i32 = 0x0026_200A; // PixelFormat32bppARGB

/// The bitmap's pixels as RGBA.
fn read_rgba(bmp: *mut GpBitmap, w: u32, h: u32) -> Option<Vec<u8>> {
    let rect = Rect {
        X: 0,
        Y: 0,
        Width: w as i32,
        Height: h as i32,
    };
    let mut data: BitmapData = unsafe { std::mem::zeroed() };
    // ImageLockModeRead
    if unsafe { GdipBitmapLockBits(bmp, &rect, 1, FORMAT_ARGB, &mut data) } != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    let ok = !data.Scan0.is_null() && data.Stride != 0;
    if ok {
        for y in 0..h as isize {
            let row = unsafe {
                std::slice::from_raw_parts(
                    (data.Scan0 as *const u8).offset(y * data.Stride as isize),
                    w as usize * 4,
                )
            };
            for px in row.chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]); // BGRA to RGBA
            }
        }
    }
    unsafe { GdipBitmapUnlockBits(bmp, &mut data) };
    ok.then_some(out)
}

/// Fits the visible part of `src` into a 64 px square (transparent
/// padding, aspect kept) and encodes it as PNG.
fn render(src: *mut GpBitmap) -> Option<Vec<u8>> {
    let (mut w, mut h) = (0u32, 0u32);
    unsafe {
        GdipGetImageWidth(src as *mut GpImage, &mut w);
        GdipGetImageHeight(src as *mut GpImage, &mut h);
    }
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let pixels = read_rgba(src, w, h)?;
    let (sx, sy, sw, sh) = png::alpha_bounds(&pixels, w, h)?;
    drop(pixels);
    let scale = (SIZE as f32 / sw as f32).min(SIZE as f32 / sh as f32);
    let dw = ((sw as f32 * scale).round() as i32).clamp(1, SIZE);
    let dh = ((sh as f32 * scale).round() as i32).clamp(1, SIZE);
    let (dx, dy) = ((SIZE - dw) / 2, (SIZE - dh) / 2);

    let mut dst: *mut GpBitmap = null_mut();
    if unsafe { GdipCreateBitmapFromScan0(SIZE, SIZE, 0, FORMAT_ARGB, null(), &mut dst) } != 0
        || dst.is_null()
    {
        return None;
    }
    let mut g: *mut GpGraphics = null_mut();
    let mut attr: *mut GpImageAttributes = null_mut();
    let mut out = None;
    unsafe {
        if GdipGetImageGraphicsContext(dst as *mut GpImage, &mut g) == 0 && !g.is_null() {
            GdipCreateImageAttributes(&mut attr);
            if !attr.is_null() {
                GdipSetImageAttributesWrapMode(attr, 3 /* TileFlipXY */, 0, 0);
            }
            GdipSetInterpolationMode(g, 7); // high quality bicubic
            GdipSetPixelOffsetMode(g, 2); // high quality
            GdipGraphicsClear(g, 0);
            let st = GdipDrawImageRectRectI(
                g,
                src as *mut GpImage,
                dx,
                dy,
                dw,
                dh,
                sx as i32,
                sy as i32,
                sw as i32,
                sh as i32,
                2, // UnitPixel
                attr,
                0,
                null_mut(),
            );
            if st == 0 {
                out = read_rgba(dst, SIZE as u32, SIZE as u32)
                    .and_then(|px| png::encode_rgba(SIZE as u32, SIZE as u32, &px));
            }
            if !attr.is_null() {
                GdipDisposeImageAttributes(attr);
            }
            GdipDeleteGraphics(g);
        }
        GdipDisposeImage(dst as *mut GpImage);
    }
    out
}
