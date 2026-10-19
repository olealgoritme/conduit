//! The driver's own blt-to-flip swap-effect upgrade (`FlipUpgrade`).
//!
//! # Why the driver does it
//!
//! A windowed blt-model swap chain (`DXGI_SWAP_EFFECT_DISCARD`, Heaven's) is
//! copied into the window's redirection surface on every present, and the app
//! waits for that copy (`docs/dwm-on-nvk.md` 4.5, `vram-redirection.md` 0). A
//! flip-model chain is shared with DWM: DWM composes from the app's own
//! buffers, no copy ("In the bitblt model, contents of the back buffer are
//! copied into the redirection surface on each call to Present1. In the flip
//! model, all back buffers are shared with the Desktop Window Manager (DWM)",
//! win32 `direct3ddxgi/dxgi-flip-model.md`, "Comparing the DXGI flip model and
//! the BitBlt model").
//!
//! Windows has the same upgrade ("Optimizations for windowed games",
//! `SwapEffectUpgradeEnable`), but it is applied only to processes DXGI
//! classifies as games: the `DXGI_ETW_SWAPCHAIN_CREATE` event of Heaven carries
//! `WINDOWEDSWAPEFFECTUPGRADE_REASON_NONGAME` (`independent-flip.md` 11.5), and
//! neither Microsoft's support page nor the DirectX blog names a driver
//! capability that feeds the decision. The driver therefore cannot qualify for
//! it, so it upgrades the chain itself, in the app's process, the same way:
//! the swap chain DXGI creates is a `FLIP_DISCARD` chain.
//!
//! # What changes for the app, and how each difference is covered
//!
//! The flip model's documented extra requirements (win32
//! `direct3ddxgi/for-best-performance--use-dxgi-flip-model.md`, "What do I have
//! to do to use the flip model?"):
//!
//! | requirement | here |
//! |---|---|
//! | "The buffer count must be at least 2." | [`plan`] asks for `FlipUpgradeBuffers` (default 3); `ResizeBuffers` keeps it ([`resize_buffer_count`]) |
//! | "After Present calls, the back buffer needs to explicitly be re-bound to the D3D11 immediate context" | the hooked `Present` saves the immediate context's render targets before the real `Present` and binds the same views again after it: what a blt-model `Present` left bound |
//! | "After calling SetFullscreenState, the application must call ResizeBuffers before Present." | an upgraded chain never enters DXGI's exclusive fullscreen: its `SetFullscreenState` is emulated (below) |
//! | "MSAA swapchains are not directly supported in the flip model" | multisampled chains are not upgraded |
//!
//! And from `dxgi-flip-model.md` ("Use flip model in an HWND that is not also
//! targeted by other APIs ... GDI"): `GDI_COMPATIBLE` chains are not upgraded. An app that
//! draws GDI into its D3D window without that flag would lose the GDI content (that is why
//! Windows limits its upgrade to games): `FlipUpgradeList` narrows the upgrade to named
//! executables, and `FlipUpgrade=0` turns it off.
//!
//! # Exclusive fullscreen
//!
//! A blt-model app may create its chain windowed and call `SetFullscreenState(TRUE)` later
//! without `ResizeBuffers` (Counter-Strike 2: its buffers already have the mode's size, so no
//! `WM_SIZE` arrives and a blt-model chain needs nothing). On a flip-model chain DXGI then
//! refuses every present: "For the flip presentation model, after you transition the display
//! state to full screen, you must call ResizeBuffers to ensure that your call to
//! IDXGISwapChain1::Present1 succeeds" (`IDXGISwapChain::SetFullscreenState`, Remarks). The
//! driver cannot resize for the app: `ResizeBuffers` fails while the app holds its back buffer
//! and views, and a blt-model app holds them for the chain's life. Two rules cover it:
//!
//! 1. **Only desktop windows are upgraded.** A window without a caption (`WS_POPUP`, the shape
//!    of an exclusive-fullscreen or borderless-fullscreen game window) keeps the app's swap
//!    effect and DXGI's own fullscreen path ([`Skip::Borderless`]; `FlipUpgrade=2` upgrades
//!    those windows too). Child windows are upgraded as before.
//! 2. **An upgraded chain's exclusive fullscreen is emulated** the way DXVK's DXGI does it
//!    (`dxvk/src/dxgi/dxgi_swapchain.cpp` `EnterFullscreenMode`, `wsi_window_win32.cpp`): the
//!    chain stays windowed for DXGI, the display mode changes to the chain's buffer size (with
//!    `ALLOW_MODE_SWITCH`, as DXGI does; `ChangeDisplaySettingsExW` with `CDS_FULLSCREEN`), and
//!    the window loses its frame and covers the output, topmost. The window is then promoted
//!    to independent flip like any flip-model window that covers the screen at the mode's
//!    size; interval-0 presents keep `ALLOW_TEARING` (the chain is windowed for DXGI). The app
//!    sees exclusive fullscreen: `GetFullscreenState` TRUE with the output, `GetDesc` /
//!    `GetFullscreenDesc` `Windowed` FALSE, `ResizeTarget` switches the mode.
//!    `SetFullscreenState(FALSE)`, losing the foreground (alt-tab, as DXGI's own exclusive
//!    fullscreen; the present then returns `DXGI_STATUS_OCCLUDED`) and the release of the
//!    chain restore the desktop mode and the window. DXGI never sees a fullscreen transition,
//!    so no `ResizeBuffers` is needed.
//!
//! Only `R8G8B8A8_UNORM` and `B8G8R8A8_UNORM` are upgraded: those are the flip
//! formats NVK mints KMD resource ids for, which DWM needs to import the
//! buffers (`docs/dwm-on-nvk.md` 4.2.2). sRGB buffers are not (a flip buffer
//! cannot be sRGB, and an app's default render-target view would lose the sRGB
//! conversion).
//!
//! A chain created with `SyncInterval` 0 presents is meant to run unthrottled.
//! Composed by DWM it does (DWM takes the newest frame); promoted to
//! independent flip it would wait for vblank unless it may tear, so the chain
//! is created with `ALLOW_TEARING` when DXGI allows it and interval-0 presents
//! of a windowed chain add `DXGI_PRESENT_ALLOW_TEARING` (the conditions are
//! `direct3ddxgi/dxgi-present.md`, `DXGI_PRESENT_ALLOW_TEARING`).
//!
//! The app still sees its own description: `GetDesc` / `GetDesc1` report the
//! swap effect, buffer count, format and flags it asked for.
//!
//! The decisions ([`plan`], [`resize_buffer_count`], [`present_flags`], the
//! fullscreen emulation's [`set_fullscreen_call`], [`fullscreen_mode`],
//! [`fullscreen_style`], [`restore_style`], [`occluded`]) are portable and
//! tested on the Linux host; the COM hooks are Windows-only.

/// `DXGI_SWAP_EFFECT_DISCARD`.
pub const SWAP_EFFECT_DISCARD: u32 = 0;
/// `DXGI_SWAP_EFFECT_FLIP_DISCARD`.
pub const SWAP_EFFECT_FLIP_DISCARD: u32 = 4;
/// `DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH`.
pub const FLAG_ALLOW_MODE_SWITCH: u32 = 0x2;
/// `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING`.
pub const FLAG_ALLOW_TEARING: u32 = 0x800;
/// `DXGI_PRESENT_TEST`.
pub const PRESENT_TEST: u32 = 0x1;
/// `DXGI_PRESENT_ALLOW_TEARING`.
pub const PRESENT_ALLOW_TEARING: u32 = 0x200;
/// `DXGI_FORMAT_R8G8B8A8_UNORM`.
pub const FORMAT_R8G8B8A8_UNORM: u32 = 28;
/// `DXGI_FORMAT_B8G8R8A8_UNORM`.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 87;
/// `WS_CHILD`.
pub const WS_CHILD: u32 = 0x4000_0000;
/// `WS_CAPTION` (`WS_BORDER | WS_DLGFRAME`).
pub const WS_CAPTION: u32 = 0x00C0_0000;
/// `WS_VISIBLE`.
pub const WS_VISIBLE: u32 = 0x1000_0000;
/// `WS_OVERLAPPEDWINDOW`.
pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
/// `WS_EX_TOPMOST`.
pub const WS_EX_TOPMOST: u32 = 0x8;
/// `WS_EX_OVERLAPPEDWINDOW` (`WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE`).
pub const WS_EX_OVERLAPPEDWINDOW: u32 = 0x300;
/// Flip-model chains take 2 to 16 buffers (`dxgi-flip-model.md`).
pub const MIN_BUFFERS: u32 = 2;
/// See [`MIN_BUFFERS`].
pub const MAX_BUFFERS: u32 = 16;

/// What the app asked for, from its `DXGI_SWAP_CHAIN_DESC` or
/// `DXGI_SWAP_CHAIN_DESC1` (+ fullscreen description).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub swap_effect: u32,
    pub format: u32,
    pub sample_count: u32,
    pub buffer_count: u32,
    pub flags: u32,
    pub windowed: bool,
    pub has_window: bool,
    /// `GWL_STYLE` of the output window at creation (0 when unknown).
    pub window_style: u32,
    /// The device is a D3D11 device (the render-target re-bind needs its
    /// immediate context; D3D10 and D3D12 devices are left alone).
    pub d3d11_device: bool,
}

/// Is a window of this style a desktop window the upgrade is meant for: one
/// with a caption, or a child window? A top-level window without a caption is
/// what a fullscreen game draws into (exclusive or borderless): see the module
/// documentation, "Exclusive fullscreen".
pub const fn desktop_window(style: u32) -> bool {
    style & WS_CHILD != 0 || style & WS_CAPTION == WS_CAPTION
}

/// The upgraded chain's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub swap_effect: u32,
    pub buffer_count: u32,
    pub flags: u32,
}

/// Why a chain is left as the app asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Not `DISCARD` (flip model already, or `SEQUENTIAL`, whose back buffer
    /// keeps the presented frame, which a flip chain does not).
    SwapEffect(u32),
    /// Multisampled back buffer.
    Multisample(u32),
    /// A format without a flip / resource-id path here.
    Format(u32),
    /// A flag the upgrade does not cover (`GDI_COMPATIBLE`, `DISPLAY_ONLY`, ...).
    Flags(u32),
    /// Created fullscreen: exclusive fullscreen keeps its scanout path.
    Fullscreen,
    /// No output window.
    NoWindow,
    /// Not a D3D11 device.
    NotD3D11,
    /// A top-level window without a caption (its `GWL_STYLE`): a fullscreen
    /// game's window keeps DXGI's own fullscreen path ([`desktop_window`]).
    Borderless(u32),
}

impl Skip {
    /// Index for per-reason counters.
    pub const fn index(self) -> usize {
        match self {
            Skip::SwapEffect(_) => 0,
            Skip::Multisample(_) => 1,
            Skip::Format(_) => 2,
            Skip::Flags(_) => 3,
            Skip::Fullscreen => 4,
            Skip::NoWindow => 5,
            Skip::NotD3D11 => 6,
            Skip::Borderless(_) => 7,
        }
    }
    /// Number of reasons ([`Skip::index`] is below it).
    pub const COUNT: usize = 8;
}

/// Decide the upgrade of one swap-chain creation. `buffers`: the
/// `FlipUpgradeBuffers` knob (clamped to 2..=16). `tearing`: ask for
/// `ALLOW_TEARING` (the caller retries without it when DXGI refuses).
/// `borderless`: upgrade windows without a caption too (`FlipUpgrade=2`).
pub const fn plan(r: &Request, buffers: u32, tearing: bool, borderless: bool) -> Result<Plan, Skip> {
    if r.swap_effect != SWAP_EFFECT_DISCARD {
        return Err(Skip::SwapEffect(r.swap_effect));
    }
    if r.sample_count > 1 {
        return Err(Skip::Multisample(r.sample_count));
    }
    if r.format != FORMAT_R8G8B8A8_UNORM && r.format != FORMAT_B8G8R8A8_UNORM {
        return Err(Skip::Format(r.format));
    }
    if r.flags & !FLAG_ALLOW_MODE_SWITCH != 0 {
        return Err(Skip::Flags(r.flags));
    }
    if !r.windowed {
        return Err(Skip::Fullscreen);
    }
    if !r.has_window {
        return Err(Skip::NoWindow);
    }
    if !borderless && !desktop_window(r.window_style) {
        return Err(Skip::Borderless(r.window_style));
    }
    if !r.d3d11_device {
        return Err(Skip::NotD3D11);
    }
    let wanted = clamp_buffers(buffers);
    let count = if r.buffer_count > wanted { clamp_buffers(r.buffer_count) } else { wanted };
    Ok(Plan {
        swap_effect: SWAP_EFFECT_FLIP_DISCARD,
        buffer_count: count,
        flags: r.flags | if tearing { FLAG_ALLOW_TEARING } else { 0 },
    })
}

const fn clamp_buffers(n: u32) -> u32 {
    if n < MIN_BUFFERS {
        MIN_BUFFERS
    } else if n > MAX_BUFFERS {
        MAX_BUFFERS
    } else {
        n
    }
}

/// `ResizeBuffers`' buffer count for an upgraded chain: 0 keeps the current
/// count; anything else is at least the planned count (a blt-model app passes
/// its own 1).
pub const fn resize_buffer_count(requested: u32, planned: u32) -> u32 {
    if requested == 0 {
        0
    } else if requested > planned {
        clamp_buffers(requested)
    } else {
        clamp_buffers(planned)
    }
}

/// `ResizeBuffers`' flags for an upgraded chain: `ALLOW_TEARING` cannot be
/// added or removed by a resize, so it is kept as the chain was created.
pub const fn resize_flags(requested: u32, tearing: bool) -> u32 {
    (requested & !FLAG_ALLOW_TEARING) | if tearing { FLAG_ALLOW_TEARING } else { 0 }
}

/// `Present` flags for an upgraded chain: interval 0 on a chain created with
/// `ALLOW_TEARING` that is not in exclusive fullscreen may tear (the
/// documented conditions of `DXGI_PRESENT_ALLOW_TEARING`); a test present is
/// left alone. `fullscreen` is DXGI's own state of the chain: an emulated
/// exclusive fullscreen is windowed for DXGI, so its presents may tear.
pub const fn present_flags(sync_interval: u32, flags: u32, tearing: bool, fullscreen: bool) -> u32 {
    if tearing && sync_interval == 0 && !fullscreen && flags & PRESENT_TEST == 0 {
        flags | PRESENT_ALLOW_TEARING
    } else {
        flags
    }
}

/// What `SetFullscreenState` does on an upgraded chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullscreenCall {
    /// Enter the emulated exclusive fullscreen.
    Enter,
    /// Leave it: desktop mode and window back.
    Leave,
    /// Already in the asked state: fullscreen answers `S_OK`, windowed goes to
    /// DXGI (whose chain is windowed: its own `S_OK`).
    Stay,
    /// `SetFullscreenState(FALSE, target)`: `DXGI_ERROR_INVALID_CALL`, as DXGI.
    Invalid,
}

/// Decide an upgraded chain's `SetFullscreenState`. `emulated`: the chain is
/// in the emulated exclusive fullscreen now.
pub const fn set_fullscreen_call(emulated: bool, fullscreen: bool, has_target: bool) -> FullscreenCall {
    if !fullscreen && has_target {
        FullscreenCall::Invalid
    } else if fullscreen == emulated {
        FullscreenCall::Stay
    } else if fullscreen {
        FullscreenCall::Enter
    } else {
        FullscreenCall::Leave
    }
}

/// A display mode, refresh in whole Hz (0 = any).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// A `DXGI_RATIONAL` refresh rounded to Hz; 0 when unspecified.
pub const fn refresh_hz(numerator: u32, denominator: u32) -> u32 {
    if numerator == 0 || denominator == 0 {
        0
    } else {
        ((numerator as u64 + denominator as u64 / 2) / denominator as u64) as u32
    }
}

/// The display mode an emulated `SetFullscreenState(TRUE)` or `ResizeTarget`
/// switches to: the given size at the given refresh when the app created the
/// chain with `ALLOW_MODE_SWITCH` ("When switching from windowed to
/// full-screen mode, the display mode (or monitor resolution) will be changed
/// to match the dimensions of the application window", `DXGI_SWAP_CHAIN_FLAG`);
/// without it, or without a size, the desktop mode stays (`None`).
pub const fn fullscreen_mode(app_flags: u32, width: u32, height: u32, refresh_num: u32, refresh_den: u32) -> Option<Mode> {
    if app_flags & FLAG_ALLOW_MODE_SWITCH == 0 || width == 0 || height == 0 {
        None
    } else {
        Some(Mode { width, height, refresh_hz: refresh_hz(refresh_num, refresh_den) })
    }
}

/// Does the current mode already serve `wanted` (no mode change needed)?
pub const fn mode_matches(current: Mode, wanted: Mode) -> bool {
    current.width == wanted.width
        && current.height == wanted.height
        && (wanted.refresh_hz == 0 || current.refresh_hz == wanted.refresh_hz)
}

/// The window styles of the emulated exclusive fullscreen: no frame, so the
/// client area is the output (DXVK `enterFullscreenMode`).
pub const fn fullscreen_style(style: u32, exstyle: u32) -> (u32, u32) {
    (style & !WS_OVERLAPPEDWINDOW, exstyle & !WS_EX_OVERLAPPEDWINDOW)
}

/// Put the saved styles back when leaving? Only when the app has not changed
/// the window's styles in between (DXVK `leaveFullscreenMode`: "This is in
/// line with what native DXGI does").
pub const fn restore_style(current_style: u32, current_exstyle: u32, saved_style: u32, saved_exstyle: u32) -> bool {
    let (style, exstyle) = fullscreen_style(saved_style, saved_exstyle);
    current_style & !WS_VISIBLE == style & !WS_VISIBLE
        && current_exstyle & !WS_EX_TOPMOST == exstyle & !WS_EX_TOPMOST
}

/// A window must stay out of the foreground this long before the emulated
/// exclusive fullscreen ends (DXVK `isOccluded`: focus flickers during the
/// mode change itself).
pub const OCCLUSION_GRACE_MS: u64 = 100;

/// Has the emulated exclusive fullscreen lost the foreground? `last_foreground_ms`:
/// when the window was last seen in the foreground, 0 when not yet since it
/// entered (an app that enters behind another window stays until it has been
/// in front once).
pub const fn occluded(foreground: bool, now_ms: u64, last_foreground_ms: u64) -> bool {
    !foreground && last_foreground_ms != 0 && now_ms.saturating_sub(last_foreground_ms) > OCCLUSION_GRACE_MS
}

/// `Windowed` of `GetDesc` / `GetFullscreenDesc` as the app must see it.
pub const fn desc_windowed(dxgi_windowed: bool, emulated: bool) -> bool {
    dxgi_windowed && !emulated
}

/// Does `exe` (a file name) pass the `FlipUpgradeList` filter? An absent or
/// empty list passes every executable; names are `;`-separated, case-blind.
pub fn exe_allowed(list: Option<&str>, exe: &str) -> bool {
    match list.map(str::trim) {
        None | Some("") => true,
        Some(list) => list
            .split(';')
            .map(str::trim)
            .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(exe)),
    }
}

#[cfg(windows)]
pub use hook::{install, summary, Options};

#[cfg(windows)]
mod hook {
    //! The COM hooks: `IDXGIFactory::CreateSwapChain` (slot 10) and
    //! `IDXGIFactory2::CreateSwapChainForHwnd` (slot 15) upgrade the creation;
    //! `IDXGISwapChain` `Release` (2), `Present` (8), `SetFullscreenState` (10),
    //! `GetFullscreenState` (11), `GetDesc` (12), `ResizeBuffers` (13),
    //! `ResizeTarget` (14), `GetDesc1` (18), `GetFullscreenDesc` (19) and
    //! `Present1` (22) serve an upgraded chain and pass every other chain
    //! straight through. Slots are the vtable order of `dxgi.h` /
    //! `dxgi1_2.h`; `IDXGIOutput` `GetDesc` is 7 and `FindClosestMatchingMode`
    //! 9; the immediate context's `OMSetRenderTargets` is slot 33 and
    //! `OMGetRenderTargets` 89 (`d3d11.h`), the device's `GetImmediateContext` 40.
    //!
    //! The vtables are DXGI's (one per class, shared by every object of it),
    //! so the hooks stay for the life of the process: this module pins the
    //! UMD in memory before it patches anything.
    //!
    //! user32 is resolved at run time from the module the process has loaded
    //! already (an app with a window has it): the UMD does not import it, so
    //! it still loads in processes that may not use win32k.

    use super::*;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::cell::Cell;
    use std::sync::{Mutex, OnceLock};

    type Hresult = i32;

    const S_OK: Hresult = 0;
    const DXGI_ERROR_INVALID_CALL: Hresult = 0x887A_0001_u32 as i32;
    const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: Hresult = 0x887A_0022_u32 as i32;
    const DXGI_STATUS_OCCLUDED: Hresult = 0x087A_0001;
    const DXGI_STATUS_MODE_CHANGE_IN_PROGRESS: Hresult = 0x087A_0008;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const IID_IDXGI_FACTORY1: Guid = Guid {
        d1: 0x770a_ae78,
        d2: 0xf26f,
        d3: 0x4dba,
        d4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    };
    const IID_IDXGI_FACTORY2: Guid = Guid {
        d1: 0x50c8_3a1c,
        d2: 0xe072,
        d3: 0x4c48,
        d4: [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0],
    };
    const IID_IDXGI_SWAP_CHAIN1: Guid = Guid {
        d1: 0x790a_45f7,
        d2: 0x0d42,
        d3: 0x4876,
        d4: [0x98, 0x3a, 0x0a, 0x55, 0xcf, 0xe6, 0xf4, 0xaa],
    };
    const IID_ID3D11_DEVICE: Guid = Guid {
        d1: 0xdb6f_6ddb,
        d2: 0xac77,
        d3: 0x4e88,
        d4: [0x82, 0x53, 0x81, 0x9d, 0xf9, 0xbb, 0xf1, 0x40],
    };
    /// Private data on an upgraded swap chain: the app's original request.
    const MARKER_GUID: Guid = Guid {
        d1: 0x6f1c_2b47,
        d2: 0x93a0,
        d3: 0x4d6e,
        d4: [0xb5, 0x1e, 0x48, 0x65, 0x6c, 0x69, 0x6f, 0x73],
    };
    const MARKER_MAGIC: u32 = 0x5055_4648; // 'HFUP'

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    /// The private data of an upgraded chain: what the app asked for, and the
    /// emulated exclusive fullscreen's state (what to restore when it ends).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Marker {
        magic: u32,
        swap_effect: u32,
        buffer_count: u32,
        format: u32,
        flags: u32,
        planned_buffers: u32,
        tearing: u32,
        /// In the emulated exclusive fullscreen.
        fs_active: u32,
        /// The emulation changed the display mode (restore it when it ends).
        fs_mode_changed: u32,
        /// The window's styles and rectangle before it covered the output.
        fs_style: u32,
        fs_exstyle: u32,
        fs_rect: Rect,
        fs_hwnd: u64,
        /// The output's GDI device name (`\\.\DISPLAYn`), NUL-terminated.
        fs_device: [u16; 32],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rational {
        numerator: u32,
        denominator: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ModeDesc {
        width: u32,
        height: u32,
        refresh_rate: Rational,
        format: u32,
        scanline_ordering: u32,
        scaling: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SampleDesc {
        count: u32,
        quality: u32,
    }
    /// `DXGI_SWAP_CHAIN_DESC`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SwapChainDesc {
        buffer_desc: ModeDesc,
        sample_desc: SampleDesc,
        buffer_usage: u32,
        buffer_count: u32,
        output_window: *mut c_void,
        windowed: i32,
        swap_effect: u32,
        flags: u32,
    }
    /// `DXGI_SWAP_CHAIN_DESC1`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SwapChainDesc1 {
        width: u32,
        height: u32,
        format: u32,
        stereo: i32,
        sample_desc: SampleDesc,
        buffer_usage: u32,
        buffer_count: u32,
        scaling: u32,
        swap_effect: u32,
        alpha_mode: u32,
        flags: u32,
    }
    /// `DXGI_SWAP_CHAIN_FULLSCREEN_DESC`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FullscreenDesc {
        refresh_rate: Rational,
        scanline_ordering: u32,
        scaling: u32,
        windowed: i32,
    }
    /// `DXGI_OUTPUT_DESC`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct OutputDesc {
        device_name: [u16; 32],
        desktop_coordinates: Rect,
        attached_to_desktop: i32,
        rotation: u32,
        monitor: *mut c_void,
    }
    /// `DEVMODEW`; only the display fields are named.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DevModeW {
        device_name: [u16; 32],
        spec_version: u16,
        driver_version: u16,
        size: u16,
        driver_extra: u16,
        fields: u32,
        position_union: [u8; 16],
        print_fields: [i16; 5],
        form_name: [u16; 32],
        log_pixels: u16,
        bits_per_pel: u32,
        pels_width: u32,
        pels_height: u32,
        display_flags: u32,
        display_frequency: u32,
        tail: [u32; 8],
    }
    /// `MONITORINFO`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    const _: () = assert!(core::mem::size_of::<ModeDesc>() == 28);
    const _: () = assert!(core::mem::size_of::<SwapChainDesc1>() == 48);
    const _: () = assert!(core::mem::size_of::<FullscreenDesc>() == 20);
    const _: () = assert!(core::mem::size_of::<DevModeW>() == 220);
    const _: () = assert!(core::mem::offset_of!(DevModeW, fields) == 72);
    const _: () = assert!(core::mem::offset_of!(DevModeW, bits_per_pel) == 168);
    const _: () = assert!(core::mem::offset_of!(DevModeW, display_frequency) == 184);
    const _: () = assert!(core::mem::size_of::<MonitorInfo>() == 40);
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(core::mem::size_of::<SwapChainDesc>() == 72);
    #[cfg(target_pointer_width = "32")]
    const _: () = assert!(core::mem::size_of::<SwapChainDesc>() == 60);
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(core::mem::size_of::<OutputDesc>() == 96);
    #[cfg(target_pointer_width = "32")]
    const _: () = assert!(core::mem::size_of::<OutputDesc>() == 92);

    type QueryInterfaceFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult;
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
    type SetPrivateDataFn = unsafe extern "system" fn(*mut c_void, *const Guid, u32, *const c_void) -> Hresult;
    type GetPrivateDataFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut u32, *mut c_void) -> Hresult;
    type CreateFactoryFn = unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> Hresult;
    type CreateSwapChainFn =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut SwapChainDesc, *mut *mut c_void) -> Hresult;
    type CreateSwapChainForHwndFn = unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *const SwapChainDesc1,
        *const FullscreenDesc,
        *mut c_void,
        *mut *mut c_void,
    ) -> Hresult;
    type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> Hresult;
    type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> Hresult;
    type GetDescFn = unsafe extern "system" fn(*mut c_void, *mut SwapChainDesc) -> Hresult;
    type GetDesc1Fn = unsafe extern "system" fn(*mut c_void, *mut SwapChainDesc1) -> Hresult;
    type GetFullscreenDescFn = unsafe extern "system" fn(*mut c_void, *mut FullscreenDesc) -> Hresult;
    type ResizeBuffersFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32) -> Hresult;
    type ResizeTargetFn = unsafe extern "system" fn(*mut c_void, *const ModeDesc) -> Hresult;
    type GetDeviceFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult;
    type SetFullscreenStateFn = unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> Hresult;
    type GetFullscreenStateFn = unsafe extern "system" fn(*mut c_void, *mut i32, *mut *mut c_void) -> Hresult;
    type GetContainingOutputFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult;
    type OutputGetDescFn = unsafe extern "system" fn(*mut c_void, *mut OutputDesc) -> Hresult;
    type FindClosestMatchingModeFn =
        unsafe extern "system" fn(*mut c_void, *const ModeDesc, *mut ModeDesc, *mut c_void) -> Hresult;
    type GetImmediateContextFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void);
    type OmGetRenderTargetsFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut *mut c_void);
    type OmSetRenderTargetsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void, *mut c_void);

    const SLOT_QUERY_INTERFACE: usize = 0;
    const SLOT_RELEASE: usize = 2;
    const SLOT_SET_PRIVATE_DATA: usize = 3;
    const SLOT_GET_PRIVATE_DATA: usize = 5;
    const SLOT_GET_DEVICE: usize = 7;
    const SLOT_FACTORY_CREATE_SWAP_CHAIN: usize = 10;
    const SLOT_FACTORY2_CREATE_SWAP_CHAIN_FOR_HWND: usize = 15;
    const SLOT_SC_PRESENT: usize = 8;
    const SLOT_SC_SET_FULLSCREEN_STATE: usize = 10;
    const SLOT_SC_GET_FULLSCREEN_STATE: usize = 11;
    const SLOT_SC_GET_DESC: usize = 12;
    const SLOT_SC_RESIZE_BUFFERS: usize = 13;
    const SLOT_SC_RESIZE_TARGET: usize = 14;
    const SLOT_SC_GET_CONTAINING_OUTPUT: usize = 15;
    const SLOT_SC1_GET_DESC1: usize = 18;
    const SLOT_SC1_GET_FULLSCREEN_DESC: usize = 19;
    const SLOT_SC1_PRESENT1: usize = 22;
    const SLOT_OUTPUT_GET_DESC: usize = 7;
    const SLOT_OUTPUT_FIND_CLOSEST_MATCHING_MODE: usize = 9;
    const SLOT_DEV_GET_IMMEDIATE_CONTEXT: usize = 40;
    const SLOT_CTX_OM_SET_RENDER_TARGETS: usize = 33;
    const SLOT_CTX_OM_GET_RENDER_TARGETS: usize = 89;
    /// `D3D11_SIMULTANEOUS_RENDER_TARGET_COUNT`.
    const RENDER_TARGETS: usize = 8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
        fn GetTickCount64() -> u64;
    }
    const PAGE_READWRITE: u32 = 0x04;
    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;

    const GWL_STYLE: i32 = -16;
    const GWL_EXSTYLE: i32 = -20;
    const HWND_TOPMOST: isize = -1;
    const HWND_NOTOPMOST: isize = -2;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const SWP_SHOWWINDOW: u32 = 0x0040;
    const CDS_FULLSCREEN: u32 = 0x4;
    const DISP_CHANGE_SUCCESSFUL: i32 = 0;
    const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;
    const DM_BITSPERPEL: u32 = 0x0004_0000;
    const DM_PELSWIDTH: u32 = 0x0008_0000;
    const DM_PELSHEIGHT: u32 = 0x0010_0000;
    const DM_DISPLAYFREQUENCY: u32 = 0x0040_0000;

    /// The user32 entry points the fullscreen emulation and the window check
    /// use, from the user32 the process has loaded.
    struct User32 {
        get_window_long: unsafe extern "system" fn(*mut c_void, i32) -> isize,
        set_window_long: unsafe extern "system" fn(*mut c_void, i32, isize) -> isize,
        set_window_pos: unsafe extern "system" fn(*mut c_void, *mut c_void, i32, i32, i32, i32, u32) -> i32,
        get_window_rect: unsafe extern "system" fn(*mut c_void, *mut Rect) -> i32,
        get_foreground_window: unsafe extern "system" fn() -> *mut c_void,
        is_window: unsafe extern "system" fn(*mut c_void) -> i32,
        get_monitor_info: unsafe extern "system" fn(*mut c_void, *mut MonitorInfo) -> i32,
        change_display_settings_ex:
            unsafe extern "system" fn(*const u16, *mut DevModeW, *mut c_void, u32, *mut c_void) -> i32,
        enum_display_settings: unsafe extern "system" fn(*const u16, u32, *mut DevModeW) -> i32,
    }

    fn user32() -> Option<&'static User32> {
        static CELL: OnceLock<Option<User32>> = OnceLock::new();
        CELL.get_or_init(|| {
            // SAFETY: GetModuleHandleW / GetProcAddress with NUL-terminated
            // names; each pointer is the export of that name, whose signature
            // the field type spells (`GetWindowLongPtrW` is `GetWindowLongW` on
            // 32-bit, where LONG_PTR is LONG).
            unsafe {
                let name: Vec<u16> = "user32.dll\0".encode_utf16().collect();
                let module = GetModuleHandleW(name.as_ptr());
                if module.is_null() {
                    return None;
                }
                let get = |n: &core::ffi::CStr| {
                    let p = GetProcAddress(module, n.as_ptr().cast());
                    (!p.is_null()).then_some(p as usize)
                };
                // A function pointer from an export address (the field's type
                // names the signature).
                fn entry<F: Copy>(address: usize) -> F {
                    const { assert!(core::mem::size_of::<F>() == core::mem::size_of::<usize>()) };
                    // SAFETY: F is a function pointer type of pointer size.
                    unsafe { core::mem::transmute_copy(&address) }
                }
                #[cfg(target_pointer_width = "64")]
                let (get_long, set_long) = (c"GetWindowLongPtrW", c"SetWindowLongPtrW");
                #[cfg(target_pointer_width = "32")]
                let (get_long, set_long) = (c"GetWindowLongW", c"SetWindowLongW");
                Some(User32 {
                    get_window_long: entry(get(get_long)?),
                    set_window_long: entry(get(set_long)?),
                    set_window_pos: entry(get(c"SetWindowPos")?),
                    get_window_rect: entry(get(c"GetWindowRect")?),
                    get_foreground_window: entry(get(c"GetForegroundWindow")?),
                    is_window: entry(get(c"IsWindow")?),
                    get_monitor_info: entry(get(c"GetMonitorInfoW")?),
                    change_display_settings_ex: entry(get(c"ChangeDisplaySettingsExW")?),
                    enum_display_settings: entry(get(c"EnumDisplaySettingsW")?),
                })
            }
        })
        .as_ref()
    }

    /// What the UMD hands [`install`].
    #[derive(Clone, Copy, Debug)]
    pub struct Options {
        /// `FlipUpgradeBuffers`.
        pub buffers: u32,
        /// Ask for `ALLOW_TEARING` (interval-0 presents stay unthrottled when
        /// the window is promoted to independent flip).
        pub tearing: bool,
        /// Upgrade top-level windows without a caption too (`FlipUpgrade=2`).
        pub borderless: bool,
    }

    /// 0 not tried, 1 running, 2 factory hooked, 3 failed.
    static STATE: AtomicU8 = AtomicU8::new(0);
    static BUFFERS: AtomicU32 = AtomicU32::new(3);
    static TEARING: AtomicU8 = AtomicU8::new(1);
    static BORDERLESS: AtomicU8 = AtomicU8::new(0);

    static ORIG_CREATE_SWAP_CHAIN: AtomicUsize = AtomicUsize::new(0);
    static ORIG_CREATE_FOR_HWND: AtomicUsize = AtomicUsize::new(0);
    static ORIG_RELEASE: AtomicUsize = AtomicUsize::new(0);
    static ORIG_PRESENT: AtomicUsize = AtomicUsize::new(0);
    static ORIG_PRESENT1: AtomicUsize = AtomicUsize::new(0);
    static ORIG_SET_FULLSCREEN_STATE: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_FULLSCREEN_STATE: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_DESC: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_DESC1: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_FULLSCREEN_DESC: AtomicUsize = AtomicUsize::new(0);
    static ORIG_RESIZE_BUFFERS: AtomicUsize = AtomicUsize::new(0);
    static ORIG_RESIZE_TARGET: AtomicUsize = AtomicUsize::new(0);
    /// 0 not yet, 1 being patched, 2 done.
    static SWAP_CHAIN_HOOKED: AtomicU8 = AtomicU8::new(0);

    static UPGRADED: AtomicU64 = AtomicU64::new(0);
    static UPGRADED_NO_TEARING: AtomicU64 = AtomicU64::new(0);
    static UPGRADE_FAILED: AtomicU64 = AtomicU64::new(0);
    static SKIPPED: [AtomicU64; Skip::COUNT] = [const { AtomicU64::new(0) }; Skip::COUNT];
    static PRESENTS: AtomicU64 = AtomicU64::new(0);
    static PRESENTS_TEARING: AtomicU64 = AtomicU64::new(0);
    static PRESENTS_FAILED: AtomicU64 = AtomicU64::new(0);
    static REBINDS: AtomicU64 = AtomicU64::new(0);
    static RESIZES: AtomicU64 = AtomicU64::new(0);
    static FS_ENTERED: AtomicU64 = AtomicU64::new(0);
    static FS_LEFT: AtomicU64 = AtomicU64::new(0);
    static FS_FOCUS_LOST: AtomicU64 = AtomicU64::new(0);
    static FS_MODE_CHANGES: AtomicU64 = AtomicU64::new(0);
    static FS_REFUSED: AtomicU64 = AtomicU64::new(0);
    static FS_DXGI_RECOVERED: AtomicU64 = AtomicU64::new(0);
    /// Chains in the emulated exclusive fullscreen now (the `Release` hook
    /// reads no private data while it is 0).
    static FS_ACTIVE_CHAINS: AtomicU64 = AtomicU64::new(0);
    /// The window of the newest emulated exclusive fullscreen and when it was
    /// last seen in the foreground (0: not since it entered).
    static FG_HWND: AtomicUsize = AtomicUsize::new(0);
    static FG_SEEN_MS: AtomicU64 = AtomicU64::new(0);
    /// One fullscreen transition at a time. Only ever `try_lock`ed: a
    /// transition moves the window, which sends messages the app may answer
    /// with a call into these hooks, from this thread or its window thread.
    static FS_LOCK: Mutex<()> = Mutex::new(());

    thread_local! {
        /// Inside our own call of the real creation (DXGI's `CreateSwapChain`
        /// may call `CreateSwapChainForHwnd` through the vtable).
        static INSIDE: Cell<bool> = const { Cell::new(false) };
    }

    unsafe fn slot(obj: *mut c_void, index: usize) -> usize {
        // SAFETY (caller): `obj` is a live COM object whose vtable has `index`.
        unsafe { *(*(obj as *const *const usize)).add(index) }
    }

    unsafe fn release(obj: *mut c_void) {
        if !obj.is_null() {
            // SAFETY (caller): a live COM object we hold a reference on.
            unsafe {
                let f: ReleaseFn = core::mem::transmute(slot(obj, SLOT_RELEASE));
                f(obj);
            }
        }
    }

    unsafe fn query(obj: *mut c_void, iid: &Guid) -> *mut c_void {
        let mut out = core::ptr::null_mut();
        // SAFETY (caller): a live COM object.
        unsafe {
            let f: QueryInterfaceFn = core::mem::transmute(slot(obj, SLOT_QUERY_INTERFACE));
            if f(obj, iid, &mut out) < 0 {
                return core::ptr::null_mut();
            }
        }
        out
    }

    /// Point `vtable[index]` at `hook`, keeping the previous entry in `orig`.
    /// The original is stored before the slot changes, so a hook that runs at
    /// once finds it. Already ours: nothing to do.
    unsafe fn patch(obj: *mut c_void, index: usize, hook: usize, orig: &AtomicUsize) -> bool {
        // SAFETY (caller): `obj` is a live COM object whose vtable has `index`.
        unsafe {
            let entry = (*(obj as *const *mut usize)).add(index);
            let current = core::ptr::read_volatile(entry);
            if current == hook {
                return true;
            }
            let mut old = 0u32;
            if VirtualProtect(entry.cast(), core::mem::size_of::<usize>(), PAGE_READWRITE, &mut old) == 0 {
                return false;
            }
            orig.store(current, Ordering::Release);
            core::ptr::write_volatile(entry, hook);
            let mut ignored = 0u32;
            VirtualProtect(entry.cast(), core::mem::size_of::<usize>(), old, &mut ignored);
            true
        }
    }

    /// Install the creation hooks once per process. Waits up to 500 ms for
    /// the helper thread (DXGI's factory is created there, outside the
    /// runtime's call into the UMD); a later finish still takes effect.
    pub fn install(options: Options) {
        if STATE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        BUFFERS.store(options.buffers, Ordering::Relaxed);
        TEARING.store(options.tearing as u8, Ordering::Relaxed);
        BORDERLESS.store(options.borderless as u8, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), &'static str>>();
        let spawned = std::thread::Builder::new()
            .name("helios-flip-upgrade".into())
            .spawn(move || {
                // SAFETY: the hook installation's own FFI; see `install_now`.
                let r = unsafe { install_now() };
                STATE.store(if r.is_ok() { 2 } else { 3 }, Ordering::Release);
                match r {
                    Ok(()) => crate::log_error!(
                        "FlipUpgrade: windowed blt-model D3D11 swap chains of this process are \
                         created flip-model (FLIP_DISCARD, {} buffers, tearing {}, {}); their \
                         exclusive fullscreen is emulated",
                        BUFFERS.load(Ordering::Relaxed),
                        TEARING.load(Ordering::Relaxed) != 0,
                        if BORDERLESS.load(Ordering::Relaxed) != 0 {
                            "every window"
                        } else {
                            "windows with a caption or child windows"
                        },
                    ),
                    Err(why) => crate::log_error!("FlipUpgrade: not installed: {why}"),
                }
                let _ = tx.send(r);
            });
        match spawned {
            Ok(_) => {
                if rx.recv_timeout(std::time::Duration::from_millis(500)).is_err() {
                    crate::log_error!(
                        "FlipUpgrade: the hook is still being installed after 500 ms; swap chains \
                         created before it finishes keep the app's swap effect"
                    );
                }
            }
            Err(_) => {
                STATE.store(3, Ordering::Release);
                crate::log_error!("FlipUpgrade: not installed: no helper thread");
            }
        }
    }

    unsafe fn install_now() -> Result<(), &'static str> {
        // SAFETY: plain Win32 calls with NUL-terminated names; COM objects are
        // released before return; the vtable writes go through `patch`.
        unsafe {
            let mut me = core::ptr::null_mut();
            if GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                install_now as usize as *const u16,
                &mut me,
            ) == 0
            {
                return Err("the UMD could not be pinned in memory");
            }
            let name: Vec<u16> = "dxgi.dll\0".encode_utf16().collect();
            let dxgi = LoadLibraryW(name.as_ptr());
            if dxgi.is_null() {
                return Err("dxgi.dll not loaded");
            }
            let create = GetProcAddress(dxgi, c"CreateDXGIFactory1".as_ptr().cast());
            if create.is_null() {
                return Err("no CreateDXGIFactory1");
            }
            let create: CreateFactoryFn = core::mem::transmute(create);
            let mut factory = core::ptr::null_mut();
            if create(&IID_IDXGI_FACTORY1, &mut factory) < 0 || factory.is_null() {
                return Err("CreateDXGIFactory1 failed");
            }
            let ok = patch(
                factory,
                SLOT_FACTORY_CREATE_SWAP_CHAIN,
                hook_create_swap_chain as usize,
                &ORIG_CREATE_SWAP_CHAIN,
            );
            let factory2 = query(factory, &IID_IDXGI_FACTORY2);
            let ok2 = !factory2.is_null()
                && patch(
                    factory2,
                    SLOT_FACTORY2_CREATE_SWAP_CHAIN_FOR_HWND,
                    hook_create_for_hwnd as usize,
                    &ORIG_CREATE_FOR_HWND,
                );
            release(factory2);
            release(factory);
            if !ok {
                return Err("IDXGIFactory::CreateSwapChain could not be hooked");
            }
            if !ok2 {
                crate::log_error!("FlipUpgrade: CreateSwapChainForHwnd not hooked (CreateSwapChain is)");
            }
            Ok(())
        }
    }

    /// Patch the swap chain vtable, once, from the first upgraded chain.
    unsafe fn hook_swap_chain_class(sc: *mut c_void) {
        if SWAP_CHAIN_HOOKED.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        // SAFETY (caller): `sc` is a live swap chain.
        unsafe {
            let a = patch(sc, SLOT_SC_PRESENT, hook_present as usize, &ORIG_PRESENT);
            let b = patch(sc, SLOT_SC_GET_DESC, hook_get_desc as usize, &ORIG_GET_DESC);
            let c = patch(sc, SLOT_SC_RESIZE_BUFFERS, hook_resize_buffers as usize, &ORIG_RESIZE_BUFFERS);
            // The fullscreen emulation: the getters first, so the state an
            // emulated transition sets is never read through DXGI's own.
            let f1 = patch(
                sc,
                SLOT_SC_GET_FULLSCREEN_STATE,
                hook_get_fullscreen_state as usize,
                &ORIG_GET_FULLSCREEN_STATE,
            );
            let f2 = patch(sc, SLOT_SC_RESIZE_TARGET, hook_resize_target as usize, &ORIG_RESIZE_TARGET);
            let f3 = patch(sc, SLOT_RELEASE, hook_release as usize, &ORIG_RELEASE);
            let sc1 = query(sc, &IID_IDXGI_SWAP_CHAIN1);
            let (d, e, f4) = if sc1.is_null() {
                (false, false, false)
            } else {
                (
                    patch(sc1, SLOT_SC1_GET_DESC1, hook_get_desc1 as usize, &ORIG_GET_DESC1),
                    patch(sc1, SLOT_SC1_PRESENT1, hook_present1 as usize, &ORIG_PRESENT1),
                    patch(
                        sc1,
                        SLOT_SC1_GET_FULLSCREEN_DESC,
                        hook_get_fullscreen_desc as usize,
                        &ORIG_GET_FULLSCREEN_DESC,
                    ),
                )
            };
            release(sc1);
            // Last: from here on an upgraded chain's SetFullscreenState is
            // emulated; every other hook it relies on is in.
            let f5 = f1
                && f2
                && f3
                && patch(
                    sc,
                    SLOT_SC_SET_FULLSCREEN_STATE,
                    hook_set_fullscreen_state as usize,
                    &ORIG_SET_FULLSCREEN_STATE,
                );
            crate::log_error!(
                "FlipUpgrade: swap chain hooks Present {a} GetDesc {b} ResizeBuffers {c} GetDesc1 {d} \
                 Present1 {e}; fullscreen emulation: GetFullscreenState {f1} ResizeTarget {f2} Release \
                 {f3} GetFullscreenDesc {f4} SetFullscreenState {f5}"
            );
        }
        SWAP_CHAIN_HOOKED.store(2, Ordering::Release);
    }

    unsafe fn is_d3d11(device: *mut c_void) -> bool {
        // SAFETY (caller): `device` is the live object the app passed.
        unsafe {
            let d = query(device, &IID_ID3D11_DEVICE);
            let yes = !d.is_null();
            release(d);
            yes
        }
    }

    /// `GWL_STYLE` of `hwnd`, 0 when it cannot be read (counted as a window
    /// without a caption: not upgraded unless `FlipUpgrade=2`).
    unsafe fn window_style(hwnd: *mut c_void) -> u32 {
        match user32() {
            // SAFETY: `hwnd` is the app's window handle; user32 validates it.
            Some(u) if !hwnd.is_null() => unsafe { (u.get_window_long)(hwnd, GWL_STYLE) as u32 },
            _ => 0,
        }
    }

    unsafe fn marker(sc: *mut c_void) -> Option<Marker> {
        let mut m = Marker::default();
        let mut size = core::mem::size_of::<Marker>() as u32;
        // SAFETY (caller): `sc` is a live swap chain (IDXGIObject).
        unsafe {
            let f: GetPrivateDataFn = core::mem::transmute(slot(sc, SLOT_GET_PRIVATE_DATA));
            if f(sc, &MARKER_GUID, &mut size, (&mut m as *mut Marker).cast()) < 0 {
                return None;
            }
        }
        (size as usize == core::mem::size_of::<Marker>() && m.magic == MARKER_MAGIC).then_some(m)
    }

    unsafe fn set_marker(sc: *mut c_void, m: &Marker) {
        // SAFETY (caller): `sc` is a live swap chain (IDXGIObject).
        unsafe {
            let f: SetPrivateDataFn = core::mem::transmute(slot(sc, SLOT_SET_PRIVATE_DATA));
            let _ = f(sc, &MARKER_GUID, core::mem::size_of::<Marker>() as u32, (m as *const Marker).cast());
        }
    }

    fn note_skip(why: Skip) {
        let n = SKIPPED[why.index()].fetch_add(1, Ordering::Relaxed);
        if n < 4 {
            crate::log_error!("FlipUpgrade: swap chain left as created: {why:?}");
        }
    }

    fn note_upgrade(request: &Request, plan: &Plan, tearing: bool) {
        let n = UPGRADED.fetch_add(1, Ordering::Relaxed) + 1;
        if !tearing && plan.flags & FLAG_ALLOW_TEARING != 0 {
            UPGRADED_NO_TEARING.fetch_add(1, Ordering::Relaxed);
        }
        if n <= 16 {
            crate::log_error!(
                "FlipUpgrade: swap chain #{n} upgraded: swap effect {} -> {} (FLIP_DISCARD), buffers \
                 {} -> {}, format {}, flags {:#x} -> {:#x}, tearing {tearing}, window style {:#010x}",
                request.swap_effect,
                plan.swap_effect,
                request.buffer_count,
                plan.buffer_count,
                request.format,
                request.flags,
                if tearing { plan.flags } else { plan.flags & !FLAG_ALLOW_TEARING },
                request.window_style,
            );
        }
    }

    fn options() -> (u32, bool, bool) {
        (
            BUFFERS.load(Ordering::Relaxed),
            TEARING.load(Ordering::Relaxed) != 0,
            BORDERLESS.load(Ordering::Relaxed) != 0,
        )
    }

    unsafe extern "system" fn hook_create_swap_chain(
        this: *mut c_void,
        device: *mut c_void,
        desc: *mut SwapChainDesc,
        out: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: the original slot was stored before the hook went in; the
        // arguments are the app's, forwarded unchanged or as a copy we own.
        unsafe {
            let orig: CreateSwapChainFn = core::mem::transmute(ORIG_CREATE_SWAP_CHAIN.load(Ordering::Acquire));
            if desc.is_null() || device.is_null() || out.is_null() || INSIDE.with(Cell::get) {
                return orig(this, device, desc, out);
            }
            let d = *desc;
            let request = Request {
                swap_effect: d.swap_effect,
                format: d.buffer_desc.format,
                sample_count: d.sample_desc.count,
                buffer_count: d.buffer_count,
                flags: d.flags,
                windowed: d.windowed != 0,
                has_window: !d.output_window.is_null(),
                window_style: window_style(d.output_window),
                d3d11_device: is_d3d11(device),
            };
            let (buffers, want_tearing, borderless) = options();
            let plan = match plan(&request, buffers, want_tearing, borderless) {
                Ok(p) => p,
                Err(why) => {
                    note_skip(why);
                    return orig(this, device, desc, out);
                }
            };
            let mut up = d;
            up.swap_effect = plan.swap_effect;
            up.buffer_count = plan.buffer_count;
            up.flags = plan.flags;
            INSIDE.with(|c| c.set(true));
            *out = core::ptr::null_mut();
            let mut hr = orig(this, device, &mut up, out);
            let mut tearing = plan.flags & FLAG_ALLOW_TEARING != 0;
            if hr < 0 && tearing {
                up.flags &= !FLAG_ALLOW_TEARING;
                tearing = false;
                *out = core::ptr::null_mut();
                hr = orig(this, device, &mut up, out);
            }
            INSIDE.with(|c| c.set(false));
            if hr < 0 || (*out).is_null() {
                let n = UPGRADE_FAILED.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    crate::log_error!(
                        "FlipUpgrade: the flip-model creation failed hr={:#010x}; the app's own \
                         request goes through",
                        hr as u32
                    );
                }
                *out = core::ptr::null_mut();
                return orig(this, device, desc, out);
            }
            finish_upgrade(*out, &request, &plan, tearing);
            hr
        }
    }

    unsafe extern "system" fn hook_create_for_hwnd(
        this: *mut c_void,
        device: *mut c_void,
        hwnd: *mut c_void,
        desc: *const SwapChainDesc1,
        fullscreen: *const FullscreenDesc,
        restrict: *mut c_void,
        out: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: as `hook_create_swap_chain`.
        unsafe {
            let orig: CreateSwapChainForHwndFn =
                core::mem::transmute(ORIG_CREATE_FOR_HWND.load(Ordering::Acquire));
            if desc.is_null() || device.is_null() || out.is_null() || INSIDE.with(Cell::get) {
                return orig(this, device, hwnd, desc, fullscreen, restrict, out);
            }
            let d = *desc;
            let request = Request {
                swap_effect: d.swap_effect,
                format: d.format,
                sample_count: d.sample_desc.count,
                buffer_count: d.buffer_count,
                flags: d.flags,
                windowed: fullscreen.is_null() || (*fullscreen).windowed != 0,
                has_window: !hwnd.is_null(),
                window_style: window_style(hwnd),
                d3d11_device: is_d3d11(device),
            };
            let (buffers, want_tearing, borderless) = options();
            let plan = match plan(&request, buffers, want_tearing, borderless) {
                Ok(p) => p,
                Err(why) => {
                    note_skip(why);
                    return orig(this, device, hwnd, desc, fullscreen, restrict, out);
                }
            };
            let mut up = d;
            up.swap_effect = plan.swap_effect;
            up.buffer_count = plan.buffer_count;
            up.flags = plan.flags;
            INSIDE.with(|c| c.set(true));
            *out = core::ptr::null_mut();
            let mut hr = orig(this, device, hwnd, &up, fullscreen, restrict, out);
            let mut tearing = plan.flags & FLAG_ALLOW_TEARING != 0;
            if hr < 0 && tearing {
                up.flags &= !FLAG_ALLOW_TEARING;
                tearing = false;
                *out = core::ptr::null_mut();
                hr = orig(this, device, hwnd, &up, fullscreen, restrict, out);
            }
            INSIDE.with(|c| c.set(false));
            if hr < 0 || (*out).is_null() {
                let n = UPGRADE_FAILED.fetch_add(1, Ordering::Relaxed);
                if n < 8 {
                    crate::log_error!(
                        "FlipUpgrade: the flip-model creation (ForHwnd) failed hr={:#010x}; the \
                         app's own request goes through",
                        hr as u32
                    );
                }
                *out = core::ptr::null_mut();
                return orig(this, device, hwnd, desc, fullscreen, restrict, out);
            }
            finish_upgrade(*out, &request, &plan, tearing);
            hr
        }
    }

    unsafe fn finish_upgrade(sc: *mut c_void, request: &Request, plan: &Plan, tearing: bool) {
        // SAFETY (caller): `sc` is the swap chain DXGI just returned.
        unsafe {
            set_marker(
                sc,
                &Marker {
                    magic: MARKER_MAGIC,
                    swap_effect: request.swap_effect,
                    buffer_count: request.buffer_count,
                    format: request.format,
                    flags: request.flags,
                    planned_buffers: plan.buffer_count,
                    tearing: tearing as u32,
                    ..Marker::default()
                },
            );
            hook_swap_chain_class(sc);
        }
        note_upgrade(request, plan, tearing);
    }

    /// DXGI's own fullscreen state of the chain (the real
    /// `GetFullscreenState`, not the emulated one).
    unsafe fn dxgi_fullscreen(sc: *mut c_void) -> bool {
        let mut state = 0i32;
        // SAFETY (caller): a live swap chain; the target out-pointer is optional.
        unsafe {
            let orig = ORIG_GET_FULLSCREEN_STATE.load(Ordering::Acquire);
            let f: GetFullscreenStateFn =
                core::mem::transmute(if orig != 0 { orig } else { slot(sc, SLOT_SC_GET_FULLSCREEN_STATE) });
            f(sc, &mut state, core::ptr::null_mut()) >= 0 && state != 0
        }
    }

    /// DXGI's own description of the chain.
    unsafe fn dxgi_desc(sc: *mut c_void) -> Option<SwapChainDesc> {
        // SAFETY (caller): a live swap chain; GetDesc is hooked, so its
        // original is stored.
        unsafe {
            let orig = ORIG_GET_DESC.load(Ordering::Acquire);
            let f: GetDescFn = core::mem::transmute(if orig != 0 { orig } else { slot(sc, SLOT_SC_GET_DESC) });
            let mut d: SwapChainDesc = core::mem::zeroed();
            (f(sc, &mut d) >= 0).then_some(d)
        }
    }

    /// The output that contains the chain's window (a reference the caller
    /// releases), or null.
    unsafe fn containing_output(sc: *mut c_void) -> *mut c_void {
        let mut out = core::ptr::null_mut();
        // SAFETY (caller): a live swap chain.
        unsafe {
            let f: GetContainingOutputFn = core::mem::transmute(slot(sc, SLOT_SC_GET_CONTAINING_OUTPUT));
            if f(sc, &mut out) < 0 {
                return core::ptr::null_mut();
            }
        }
        out
    }

    unsafe fn output_desc(output: *mut c_void) -> Option<OutputDesc> {
        // SAFETY (caller): a live IDXGIOutput.
        unsafe {
            let f: OutputGetDescFn = core::mem::transmute(slot(output, SLOT_OUTPUT_GET_DESC));
            let mut d: OutputDesc = core::mem::zeroed();
            (f(output, &mut d) >= 0).then_some(d)
        }
    }

    /// The output's supported mode closest to `want` (DXGI picks the mode the
    /// same way for its own exclusive fullscreen); `want` when it finds none.
    unsafe fn closest_mode(output: *mut c_void, want: Mode, format: u32) -> Mode {
        let query = ModeDesc {
            width: want.width,
            height: want.height,
            refresh_rate: Rational { numerator: want.refresh_hz, denominator: (want.refresh_hz != 0) as u32 },
            format,
            scanline_ordering: 0,
            scaling: 0,
        };
        // SAFETY (caller): a live IDXGIOutput; the format is a display format,
        // so no device is needed.
        unsafe {
            let f: FindClosestMatchingModeFn =
                core::mem::transmute(slot(output, SLOT_OUTPUT_FIND_CLOSEST_MATCHING_MODE));
            let mut found: ModeDesc = core::mem::zeroed();
            if f(output, &query, &mut found, core::ptr::null_mut()) >= 0 && found.width != 0 && found.height != 0 {
                Mode {
                    width: found.width,
                    height: found.height,
                    refresh_hz: refresh_hz(found.refresh_rate.numerator, found.refresh_rate.denominator),
                }
            } else {
                want
            }
        }
    }

    /// Switch `device`'s display mode to `want` (temporary, `CDS_FULLSCREEN`,
    /// as DXVK's `setMonitorDisplayMode`). Ok(true): changed; Ok(false): the
    /// current mode already serves it; Err: the `DISP_CHANGE_*` code.
    unsafe fn set_display_mode(u: &User32, device: &[u16; 32], want: Mode) -> Result<bool, i32> {
        // SAFETY: `device` is NUL-terminated (DXGI_OUTPUT_DESC::DeviceName);
        // the DEVMODEW structs are ours, `size` set.
        unsafe {
            let mut current: DevModeW = core::mem::zeroed();
            current.size = core::mem::size_of::<DevModeW>() as u16;
            if (u.enum_display_settings)(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut current) != 0
                && mode_matches(
                    Mode {
                        width: current.pels_width,
                        height: current.pels_height,
                        refresh_hz: current.display_frequency,
                    },
                    want,
                )
            {
                return Ok(false);
            }
            let mut dm: DevModeW = core::mem::zeroed();
            dm.size = core::mem::size_of::<DevModeW>() as u16;
            dm.fields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
            dm.bits_per_pel = 32;
            dm.pels_width = want.width;
            dm.pels_height = want.height;
            if want.refresh_hz != 0 {
                dm.fields |= DM_DISPLAYFREQUENCY;
                dm.display_frequency = want.refresh_hz;
            }
            let null = core::ptr::null_mut();
            let mut r = (u.change_display_settings_ex)(device.as_ptr(), &mut dm, null, CDS_FULLSCREEN, null);
            if r != DISP_CHANGE_SUCCESSFUL && want.refresh_hz != 0 {
                dm.fields &= !DM_DISPLAYFREQUENCY;
                r = (u.change_display_settings_ex)(device.as_ptr(), &mut dm, null, CDS_FULLSCREEN, null);
            }
            if r == DISP_CHANGE_SUCCESSFUL {
                FS_MODE_CHANGES.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            } else {
                Err(r)
            }
        }
    }

    /// The registry (desktop) mode of `device` back.
    unsafe fn restore_display_mode(u: &User32, device: &[u16; 32]) {
        let null = core::ptr::null_mut::<c_void>();
        // SAFETY: NUL-terminated device name; NULL DEVMODE with flags 0 is
        // "the easiest way to return to the default mode after a dynamic mode
        // change" (ChangeDisplaySettingsExW).
        let r = unsafe { (u.change_display_settings_ex)(device.as_ptr(), core::ptr::null_mut(), null, 0, null) };
        if r != DISP_CHANGE_SUCCESSFUL {
            crate::log_error!("FlipUpgrade: the desktop display mode was not restored: DISP_CHANGE {r}");
        }
    }

    /// The output's rectangle now (after a mode change), from its monitor;
    /// `fallback` when the monitor cannot be read.
    unsafe fn monitor_rect(u: &User32, monitor: *mut c_void, fallback: Rect) -> Rect {
        let mut info = MonitorInfo { size: core::mem::size_of::<MonitorInfo>() as u32, ..MonitorInfo::default() };
        // SAFETY: `info` is ours with `size` set; user32 validates the handle.
        if !monitor.is_null() && unsafe { (u.get_monitor_info)(monitor, &mut info) } != 0 {
            info.monitor
        } else {
            fallback
        }
    }

    fn device_name_lossy(name: &[u16; 32]) -> String {
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf16_lossy(&name[..n])
    }

    /// `SetFullscreenState(TRUE, target)` on an upgraded chain: the emulated
    /// exclusive fullscreen (module documentation, "Exclusive fullscreen").
    unsafe fn enter_fullscreen(sc: *mut c_void, target: *mut c_void, m: &mut Marker) -> Hresult {
        let Some(u) = user32() else {
            FS_REFUSED.fetch_add(1, Ordering::Relaxed);
            return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
        };
        let Ok(_guard) = FS_LOCK.try_lock() else {
            return DXGI_STATUS_MODE_CHANGE_IN_PROGRESS;
        };
        // SAFETY: `sc` is a live upgraded chain; `target` is the app's
        // IDXGIOutput or null; every reference taken is released here.
        unsafe {
            // The state as it is now (another thread may have entered since
            // the caller read it).
            if let Some(current) = marker(sc) {
                *m = current;
            }
            if m.fs_active != 0 {
                return S_OK;
            }
            let Some(d) = dxgi_desc(sc) else {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            };
            let hwnd = d.output_window;
            if hwnd.is_null() || (u.is_window)(hwnd) == 0 {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            }
            let owned = target.is_null();
            let output = if owned { containing_output(sc) } else { target };
            if output.is_null() {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                crate::log_error!("FlipUpgrade: fullscreen refused: no output contains the window");
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            }
            let od = output_desc(output);
            let wanted = fullscreen_mode(
                m.flags,
                d.buffer_desc.width,
                d.buffer_desc.height,
                d.buffer_desc.refresh_rate.numerator,
                d.buffer_desc.refresh_rate.denominator,
            )
            .map(|want| closest_mode(output, want, d.buffer_desc.format));
            if owned {
                release(output);
            }
            let Some(od) = od else {
                FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            };
            let mut changed = false;
            if let Some(mode) = wanted {
                match set_display_mode(u, &od.device_name, mode) {
                    Ok(c) => changed = c,
                    Err(code) => {
                        FS_REFUSED.fetch_add(1, Ordering::Relaxed);
                        crate::log_error!(
                            "FlipUpgrade: fullscreen refused: {}x{}@{} on {} failed (DISP_CHANGE {code})",
                            mode.width,
                            mode.height,
                            mode.refresh_hz,
                            device_name_lossy(&od.device_name),
                        );
                        return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
                    }
                }
            }
            let style = (u.get_window_long)(hwnd, GWL_STYLE) as u32;
            let exstyle = (u.get_window_long)(hwnd, GWL_EXSTYLE) as u32;
            let mut rect = Rect::default();
            (u.get_window_rect)(hwnd, &mut rect);
            m.fs_active = 1;
            m.fs_mode_changed = changed as u32;
            m.fs_style = style;
            m.fs_exstyle = exstyle;
            m.fs_rect = rect;
            m.fs_hwnd = hwnd as usize as u64;
            m.fs_device = od.device_name;
            // The state goes in before the window moves: the app may answer
            // the window's messages with GetFullscreenState / GetDesc.
            set_marker(sc, m);
            FS_ACTIVE_CHAINS.fetch_add(1, Ordering::Relaxed);
            FG_HWND.store(hwnd as usize, Ordering::Relaxed);
            FG_SEEN_MS.store(0, Ordering::Relaxed);
            let (fs_style, fs_exstyle) = fullscreen_style(style, exstyle);
            (u.set_window_long)(hwnd, GWL_STYLE, fs_style as i32 as isize);
            (u.set_window_long)(hwnd, GWL_EXSTYLE, fs_exstyle as i32 as isize);
            let fallback = match wanted {
                Some(mode) if changed => Rect {
                    left: od.desktop_coordinates.left,
                    top: od.desktop_coordinates.top,
                    right: od.desktop_coordinates.left + mode.width as i32,
                    bottom: od.desktop_coordinates.top + mode.height as i32,
                },
                _ => od.desktop_coordinates,
            };
            let r = monitor_rect(u, od.monitor, fallback);
            (u.set_window_pos)(
                hwnd,
                HWND_TOPMOST as *mut c_void,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
            let n = FS_ENTERED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 16 {
                crate::log_error!(
                    "FlipUpgrade: exclusive fullscreen #{n} (emulated; the chain stays windowed for \
                     DXGI): {} at {}x{}{}, window {:#x} covers {},{} {}x{}; buffers {}x{}",
                    device_name_lossy(&od.device_name),
                    r.right - r.left,
                    r.bottom - r.top,
                    match wanted {
                        Some(mode) if changed => format!(" (mode switched to {}x{}@{})", mode.width, mode.height, mode.refresh_hz),
                        Some(_) => " (the current mode serves it)".to_string(),
                        None => " (desktop mode: no ALLOW_MODE_SWITCH)".to_string(),
                    },
                    hwnd as usize,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    d.buffer_desc.width,
                    d.buffer_desc.height,
                );
            }
        }
        S_OK
    }

    /// Put the desktop mode and the window back as they were before
    /// [`enter_fullscreen`]. No access to the chain: also runs after its
    /// release.
    unsafe fn restore_desktop(u: &User32, m: &Marker) {
        // SAFETY: the handles come from the marker; user32 validates the window.
        unsafe {
            if m.fs_mode_changed != 0 {
                restore_display_mode(u, &m.fs_device);
            }
            let hwnd = m.fs_hwnd as usize as *mut c_void;
            if hwnd.is_null() || (u.is_window)(hwnd) == 0 {
                return;
            }
            let style = (u.get_window_long)(hwnd, GWL_STYLE) as u32;
            let exstyle = (u.get_window_long)(hwnd, GWL_EXSTYLE) as u32;
            if restore_style(style, exstyle, m.fs_style, m.fs_exstyle) {
                (u.set_window_long)(hwnd, GWL_STYLE, m.fs_style as i32 as isize);
                (u.set_window_long)(hwnd, GWL_EXSTYLE, m.fs_exstyle as i32 as isize);
            }
            let r = m.fs_rect;
            let after = if m.fs_exstyle & WS_EX_TOPMOST != 0 { HWND_TOPMOST } else { HWND_NOTOPMOST };
            (u.set_window_pos)(
                hwnd,
                after as *mut c_void,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOACTIVATE,
            );
        }
    }

    /// Leave the emulated exclusive fullscreen. False: another transition is
    /// running (the chain stays as it is).
    unsafe fn leave_fullscreen(sc: *mut c_void, m: &mut Marker, why: &str) -> bool {
        let Some(u) = user32() else {
            return false;
        };
        let Ok(_guard) = FS_LOCK.try_lock() else {
            return false;
        };
        // SAFETY: `sc` is a live upgraded chain.
        if let Some(current) = unsafe { marker(sc) } {
            *m = current;
        }
        if m.fs_active == 0 {
            // Another thread left it since the caller read the state.
            return true;
        }
        let saved = *m;
        // SAFETY: as above.
        unsafe {
            m.fs_active = 0;
            m.fs_mode_changed = 0;
            set_marker(sc, m);
            FS_ACTIVE_CHAINS.fetch_sub(1, Ordering::Relaxed);
            restore_desktop(u, &saved);
        }
        let n = FS_LEFT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 {
            crate::log_error!(
                "FlipUpgrade: exclusive fullscreen left ({why}){}",
                if saved.fs_mode_changed != 0 { "; desktop mode restored" } else { "" }
            );
        }
        true
    }

    /// Has the emulated exclusive fullscreen of `m` lost the foreground (DXGI
    /// leaves its own exclusive fullscreen then)?
    fn focus_lost(m: &Marker) -> bool {
        let Some(u) = user32() else {
            return false;
        };
        let hwnd = m.fs_hwnd as usize;
        // SAFETY: GetForegroundWindow / GetTickCount64 take no arguments.
        let (foreground, now) = unsafe { ((u.get_foreground_window)() as usize == hwnd, GetTickCount64()) };
        if FG_HWND.load(Ordering::Relaxed) != hwnd {
            // Another chain entered later; this one keeps its state until its
            // own window has been seen again.
            FG_HWND.store(hwnd, Ordering::Relaxed);
            FG_SEEN_MS.store(if foreground { now } else { 0 }, Ordering::Relaxed);
            return false;
        }
        if foreground {
            FG_SEEN_MS.store(now, Ordering::Relaxed);
            return false;
        }
        occluded(false, now, FG_SEEN_MS.load(Ordering::Relaxed))
    }

    /// Run the real present with the immediate context's render targets as
    /// they were before it: a flip-model present unbinds the back buffer, a
    /// blt-model one does not, and the app was written for the latter.
    unsafe fn present_rebinding(sc: *mut c_void, present: impl FnOnce() -> Hresult) -> Hresult {
        // SAFETY (caller): a live swap chain. Every reference taken here is
        // released before return.
        unsafe {
            let mut device = core::ptr::null_mut();
            let get_device: GetDeviceFn = core::mem::transmute(slot(sc, SLOT_GET_DEVICE));
            if get_device(sc, &IID_ID3D11_DEVICE, &mut device) < 0 || device.is_null() {
                return present();
            }
            let mut context = core::ptr::null_mut();
            let get_context: GetImmediateContextFn =
                core::mem::transmute(slot(device, SLOT_DEV_GET_IMMEDIATE_CONTEXT));
            get_context(device, &mut context);
            if context.is_null() {
                release(device);
                return present();
            }
            let mut views = [core::ptr::null_mut::<c_void>(); RENDER_TARGETS];
            let mut depth = core::ptr::null_mut();
            let om_get: OmGetRenderTargetsFn = core::mem::transmute(slot(context, SLOT_CTX_OM_GET_RENDER_TARGETS));
            om_get(context, RENDER_TARGETS as u32, views.as_mut_ptr(), &mut depth);
            let hr = present();
            let count = views.iter().rposition(|v| !v.is_null()).map_or(0, |i| i + 1);
            if hr >= 0 && (count != 0 || !depth.is_null()) {
                let om_set: OmSetRenderTargetsFn =
                    core::mem::transmute(slot(context, SLOT_CTX_OM_SET_RENDER_TARGETS));
                om_set(context, count as u32, views.as_ptr(), depth);
                REBINDS.fetch_add(1, Ordering::Relaxed);
            }
            for v in views {
                release(v);
            }
            release(depth);
            release(context);
            release(device);
            hr
        }
    }

    fn note_present(tearing: bool) {
        let n = PRESENTS.fetch_add(1, Ordering::Relaxed) + 1;
        if tearing {
            PRESENTS_TEARING.fetch_add(1, Ordering::Relaxed);
        }
        if n == 1 || n.is_multiple_of(4096) {
            crate::log_error!("FlipUpgrade: {}", summary());
        }
    }

    fn note_present_result(hr: Hresult) {
        if hr < 0 {
            let n = PRESENTS_FAILED.fetch_add(1, Ordering::Relaxed);
            if n < 4 || n.is_multiple_of(4096) {
                crate::log_error!("FlipUpgrade: present #{} failed hr={:#010x}; {}", n + 1, hr as u32, summary());
            }
        }
    }

    /// What every present of an upgraded chain does before the real one: end
    /// an emulated exclusive fullscreen that lost the foreground (true: the
    /// present reports `DXGI_STATUS_OCCLUDED`, as DXGI's own exclusive
    /// fullscreen does then), and take a chain DXGI put into its own exclusive
    /// fullscreen (only a transition that bypassed the hooked vtable can)
    /// back to the emulated one; and its present flags.
    unsafe fn before_present(sc: *mut c_void, m: &mut Marker, sync_interval: u32, flags: u32) -> (bool, u32) {
        // SAFETY (caller): a live upgraded chain.
        unsafe {
            let mut occluded_now = false;
            if m.fs_active != 0 && focus_lost(m) && leave_fullscreen(sc, m, "the window lost the foreground") {
                FS_FOCUS_LOST.fetch_add(1, Ordering::Relaxed);
                occluded_now = true;
            }
            let mut real_fullscreen = dxgi_fullscreen(sc);
            if real_fullscreen {
                let orig = ORIG_SET_FULLSCREEN_STATE.load(Ordering::Acquire);
                if orig != 0 {
                    let set: SetFullscreenStateFn = core::mem::transmute(orig);
                    let n = FS_DXGI_RECOVERED.fetch_add(1, Ordering::Relaxed);
                    if n < 4 {
                        crate::log_error!(
                            "FlipUpgrade: DXGI put an upgraded chain into its own exclusive \
                             fullscreen (flip-model presents fail there until ResizeBuffers); \
                             back to windowed and the emulated fullscreen"
                        );
                    }
                    if set(sc, 0, core::ptr::null_mut()) >= 0 {
                        real_fullscreen = dxgi_fullscreen(sc);
                        if !real_fullscreen && m.fs_active == 0 {
                            let _ = enter_fullscreen(sc, core::ptr::null_mut(), m);
                        }
                    }
                }
            }
            (occluded_now, present_flags(sync_interval, flags, m.tearing != 0, real_fullscreen))
        }
    }

    unsafe extern "system" fn hook_present(sc: *mut c_void, sync_interval: u32, flags: u32) -> Hresult {
        // SAFETY: the original slot was stored before the hook went in.
        unsafe {
            let orig: PresentFn = core::mem::transmute(ORIG_PRESENT.load(Ordering::Acquire));
            let Some(mut m) = marker(sc) else {
                return orig(sc, sync_interval, flags);
            };
            let (occluded_now, f) = before_present(sc, &mut m, sync_interval, flags);
            note_present(f & PRESENT_ALLOW_TEARING != 0);
            let hr = present_rebinding(sc, || orig(sc, sync_interval, f));
            note_present_result(hr);
            if occluded_now && hr >= 0 { DXGI_STATUS_OCCLUDED } else { hr }
        }
    }

    unsafe extern "system" fn hook_present1(
        sc: *mut c_void,
        sync_interval: u32,
        flags: u32,
        params: *const c_void,
    ) -> Hresult {
        // SAFETY: as `hook_present`.
        unsafe {
            let orig: Present1Fn = core::mem::transmute(ORIG_PRESENT1.load(Ordering::Acquire));
            let Some(mut m) = marker(sc) else {
                return orig(sc, sync_interval, flags, params);
            };
            let (occluded_now, f) = before_present(sc, &mut m, sync_interval, flags);
            note_present(f & PRESENT_ALLOW_TEARING != 0);
            let hr = present_rebinding(sc, || orig(sc, sync_interval, f, params));
            note_present_result(hr);
            if occluded_now && hr >= 0 { DXGI_STATUS_OCCLUDED } else { hr }
        }
    }

    unsafe extern "system" fn hook_set_fullscreen_state(sc: *mut c_void, fullscreen: i32, target: *mut c_void) -> Hresult {
        // SAFETY: the original slot was stored before the hook went in.
        unsafe {
            let orig: SetFullscreenStateFn = core::mem::transmute(ORIG_SET_FULLSCREEN_STATE.load(Ordering::Acquire));
            let Some(mut m) = marker(sc) else {
                return orig(sc, fullscreen, target);
            };
            match set_fullscreen_call(m.fs_active != 0, fullscreen != 0, !target.is_null()) {
                FullscreenCall::Invalid => DXGI_ERROR_INVALID_CALL,
                FullscreenCall::Enter => enter_fullscreen(sc, target, &mut m),
                FullscreenCall::Leave => {
                    if leave_fullscreen(sc, &mut m, "SetFullscreenState(FALSE)") {
                        S_OK
                    } else {
                        DXGI_STATUS_MODE_CHANGE_IN_PROGRESS
                    }
                }
                FullscreenCall::Stay if fullscreen != 0 => S_OK,
                FullscreenCall::Stay => orig(sc, 0, core::ptr::null_mut()),
            }
        }
    }

    unsafe extern "system" fn hook_get_fullscreen_state(
        sc: *mut c_void,
        fullscreen: *mut i32,
        target: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: as `hook_present`; the out-pointers are the app's (optional).
        unsafe {
            let orig: GetFullscreenStateFn = core::mem::transmute(ORIG_GET_FULLSCREEN_STATE.load(Ordering::Acquire));
            let Some(mut m) = marker(sc) else {
                return orig(sc, fullscreen, target);
            };
            if m.fs_active != 0 && focus_lost(&m) && leave_fullscreen(sc, &mut m, "the window lost the foreground") {
                FS_FOCUS_LOST.fetch_add(1, Ordering::Relaxed);
            }
            if m.fs_active == 0 {
                return orig(sc, fullscreen, target);
            }
            if !fullscreen.is_null() {
                *fullscreen = 1;
            }
            if !target.is_null() {
                *target = containing_output(sc);
            }
            S_OK
        }
    }

    unsafe extern "system" fn hook_resize_target(sc: *mut c_void, params: *const ModeDesc) -> Hresult {
        // SAFETY: as `hook_present`; `params` is the app's.
        unsafe {
            let orig: ResizeTargetFn = core::mem::transmute(ORIG_RESIZE_TARGET.load(Ordering::Acquire));
            let Some(mut m) = marker(sc) else {
                return orig(sc, params);
            };
            if m.fs_active == 0 || params.is_null() {
                return orig(sc, params);
            }
            // In exclusive fullscreen ResizeTarget changes the display mode
            // (a windowed chain's resizes the window: DXGI's own call).
            let p = *params;
            let Some(want) = fullscreen_mode(m.flags, p.width, p.height, p.refresh_rate.numerator, p.refresh_rate.denominator)
            else {
                return S_OK;
            };
            let Some(u) = user32() else {
                return S_OK;
            };
            let Ok(_guard) = FS_LOCK.try_lock() else {
                return DXGI_STATUS_MODE_CHANGE_IN_PROGRESS;
            };
            let output = containing_output(sc);
            if output.is_null() {
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            }
            let format = if p.format != 0 { p.format } else { m.format };
            let mode = closest_mode(output, want, format);
            let od = output_desc(output);
            release(output);
            let Some(od) = od else {
                return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
            };
            match set_display_mode(u, &od.device_name, mode) {
                Ok(true) => {
                    m.fs_mode_changed = 1;
                    m.fs_device = od.device_name;
                    set_marker(sc, &m);
                }
                Ok(false) => {}
                Err(code) => {
                    crate::log_error!(
                        "FlipUpgrade: ResizeTarget in fullscreen: {}x{}@{} failed (DISP_CHANGE {code})",
                        mode.width,
                        mode.height,
                        mode.refresh_hz
                    );
                    return DXGI_ERROR_NOT_CURRENTLY_AVAILABLE;
                }
            }
            let hwnd = m.fs_hwnd as usize as *mut c_void;
            let r = monitor_rect(
                u,
                od.monitor,
                Rect {
                    left: od.desktop_coordinates.left,
                    top: od.desktop_coordinates.top,
                    right: od.desktop_coordinates.left + mode.width as i32,
                    bottom: od.desktop_coordinates.top + mode.height as i32,
                },
            );
            (u.set_window_pos)(
                hwnd,
                core::ptr::null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            crate::log_error!(
                "FlipUpgrade: ResizeTarget in fullscreen: {}x{}@{}",
                mode.width,
                mode.height,
                mode.refresh_hz
            );
            S_OK
        }
    }

    unsafe extern "system" fn hook_release(sc: *mut c_void) -> u32 {
        // SAFETY: the original slot was stored before the hook went in; the
        // marker is read while the caller's reference still holds the chain.
        unsafe {
            let orig: ReleaseFn = core::mem::transmute(ORIG_RELEASE.load(Ordering::Acquire));
            if FS_ACTIVE_CHAINS.load(Ordering::Relaxed) == 0 {
                return orig(sc);
            }
            let m = marker(sc).filter(|m| m.fs_active != 0);
            let n = orig(sc);
            if let (0, Some(m)) = (n, m) {
                // Released in (emulated) exclusive fullscreen, which DXGI's
                // own fullscreen does not allow either: give the desktop back.
                FS_ACTIVE_CHAINS.fetch_sub(1, Ordering::Relaxed);
                if let Some(u) = user32() {
                    restore_desktop(u, &m);
                }
                crate::log_error!("FlipUpgrade: a chain was released in exclusive fullscreen; desktop restored");
            }
            n
        }
    }

    unsafe extern "system" fn hook_get_desc(sc: *mut c_void, desc: *mut SwapChainDesc) -> Hresult {
        // SAFETY: as `hook_present`; `desc` is the app's out-struct.
        unsafe {
            let orig: GetDescFn = core::mem::transmute(ORIG_GET_DESC.load(Ordering::Acquire));
            let hr = orig(sc, desc);
            if hr >= 0 && !desc.is_null() {
                if let Some(m) = marker(sc) {
                    (*desc).swap_effect = m.swap_effect;
                    (*desc).buffer_count = m.buffer_count;
                    (*desc).buffer_desc.format = m.format;
                    (*desc).flags = m.flags;
                    (*desc).windowed = desc_windowed((*desc).windowed != 0, m.fs_active != 0) as i32;
                }
            }
            hr
        }
    }

    unsafe extern "system" fn hook_get_desc1(sc: *mut c_void, desc: *mut SwapChainDesc1) -> Hresult {
        // SAFETY: as `hook_get_desc`.
        unsafe {
            let orig: GetDesc1Fn = core::mem::transmute(ORIG_GET_DESC1.load(Ordering::Acquire));
            let hr = orig(sc, desc);
            if hr >= 0 && !desc.is_null() {
                if let Some(m) = marker(sc) {
                    (*desc).swap_effect = m.swap_effect;
                    (*desc).buffer_count = m.buffer_count;
                    (*desc).flags = m.flags;
                }
            }
            hr
        }
    }

    unsafe extern "system" fn hook_get_fullscreen_desc(sc: *mut c_void, desc: *mut FullscreenDesc) -> Hresult {
        // SAFETY: as `hook_get_desc`.
        unsafe {
            let orig: GetFullscreenDescFn = core::mem::transmute(ORIG_GET_FULLSCREEN_DESC.load(Ordering::Acquire));
            let hr = orig(sc, desc);
            if hr >= 0 && !desc.is_null() {
                if let Some(m) = marker(sc) {
                    (*desc).windowed = desc_windowed((*desc).windowed != 0, m.fs_active != 0) as i32;
                }
            }
            hr
        }
    }

    unsafe extern "system" fn hook_resize_buffers(
        sc: *mut c_void,
        count: u32,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> Hresult {
        // SAFETY: as `hook_present`.
        unsafe {
            let orig: ResizeBuffersFn = core::mem::transmute(ORIG_RESIZE_BUFFERS.load(Ordering::Acquire));
            let Some(m) = marker(sc) else {
                return orig(sc, count, width, height, format, flags);
            };
            RESIZES.fetch_add(1, Ordering::Relaxed);
            orig(
                sc,
                resize_buffer_count(count, m.planned_buffers),
                width,
                height,
                format,
                resize_flags(flags, m.tearing != 0),
            )
        }
    }

    /// One line of counters for the log.
    pub fn summary() -> String {
        let skipped: Vec<u64> = SKIPPED.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        format!(
            "{} swap chains upgraded ({} without tearing), {} failed and created as asked, skipped \
             [swap effect {}, MSAA {}, format {}, flags {}, fullscreen {}, no window {}, not D3D11 {}, \
             borderless window {}]; {} presents ({} may tear, {} failed), {} render-target re-binds, \
             {} resizes; emulated exclusive fullscreen: {} entered, {} left ({} on focus loss), {} \
             mode changes, {} refused, {} taken back from DXGI's own",
            UPGRADED.load(Ordering::Relaxed),
            UPGRADED_NO_TEARING.load(Ordering::Relaxed),
            UPGRADE_FAILED.load(Ordering::Relaxed),
            skipped[0],
            skipped[1],
            skipped[2],
            skipped[3],
            skipped[4],
            skipped[5],
            skipped[6],
            skipped[7],
            PRESENTS.load(Ordering::Relaxed),
            PRESENTS_TEARING.load(Ordering::Relaxed),
            PRESENTS_FAILED.load(Ordering::Relaxed),
            REBINDS.load(Ordering::Relaxed),
            RESIZES.load(Ordering::Relaxed),
            FS_ENTERED.load(Ordering::Relaxed),
            FS_LEFT.load(Ordering::Relaxed),
            FS_FOCUS_LOST.load(Ordering::Relaxed),
            FS_MODE_CHANGES.load(Ordering::Relaxed),
            FS_REFUSED.load(Ordering::Relaxed),
            FS_DXGI_RECOVERED.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heaven() -> Request {
        Request {
            swap_effect: SWAP_EFFECT_DISCARD,
            format: FORMAT_R8G8B8A8_UNORM,
            sample_count: 1,
            buffer_count: 1,
            flags: FLAG_ALLOW_MODE_SWITCH,
            windowed: true,
            has_window: true,
            // WS_OVERLAPPEDWINDOW | WS_VISIBLE: Heaven's windowed mode.
            window_style: WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            d3d11_device: true,
        }
    }

    /// Counter-Strike 2 in fullscreen: a blt-model chain created windowed on a
    /// window without a caption (WS_POPUP | WS_VISIBLE), then
    /// SetFullscreenState(TRUE) with no ResizeBuffers.
    fn cs2_fullscreen() -> Request {
        Request { window_style: 0x8000_0000 | WS_VISIBLE, flags: FLAG_ALLOW_MODE_SWITCH, ..heaven() }
    }

    #[test]
    fn a_windowed_discard_chain_becomes_flip_discard() {
        let p = plan(&heaven(), 3, true, false).unwrap();
        assert_eq!(p.swap_effect, SWAP_EFFECT_FLIP_DISCARD);
        assert_eq!(p.buffer_count, 3);
        assert_eq!(p.flags, FLAG_ALLOW_MODE_SWITCH | FLAG_ALLOW_TEARING);
        let p = plan(&heaven(), 3, false, false).unwrap();
        assert_eq!(p.flags, FLAG_ALLOW_MODE_SWITCH);
    }

    #[test]
    fn buffer_count_is_clamped_and_never_lowered() {
        assert_eq!(plan(&heaven(), 0, false, false).unwrap().buffer_count, 2);
        assert_eq!(plan(&heaven(), 40, false, false).unwrap().buffer_count, 16);
        let mut r = heaven();
        r.buffer_count = 5;
        assert_eq!(plan(&r, 3, false, false).unwrap().buffer_count, 5);
    }

    #[test]
    fn what_the_upgrade_does_not_cover_is_left_alone() {
        let mut r = heaven();
        r.swap_effect = 1;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::SwapEffect(1)));
        r.swap_effect = SWAP_EFFECT_FLIP_DISCARD;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::SwapEffect(SWAP_EFFECT_FLIP_DISCARD)));
        let mut r = heaven();
        r.sample_count = 4;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::Multisample(4)));
        for fmt in [29u32, 91, 88, 10, 24] {
            let mut r = heaven();
            r.format = fmt;
            assert_eq!(plan(&r, 3, true, false), Err(Skip::Format(fmt)), "format {fmt}");
        }
        let mut r = heaven();
        r.flags |= 0x4; // GDI_COMPATIBLE
        assert_eq!(plan(&r, 3, true, false), Err(Skip::Flags(0x6)));
        let mut r = heaven();
        r.windowed = false;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::Fullscreen));
        let mut r = heaven();
        r.has_window = false;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::NoWindow));
        let mut r = heaven();
        r.d3d11_device = false;
        assert_eq!(plan(&r, 3, true, false), Err(Skip::NotD3D11));
        let mut r = heaven();
        r.format = FORMAT_B8G8R8A8_UNORM;
        r.flags = 0;
        assert!(plan(&r, 3, true, false).is_ok());
    }

    #[test]
    fn only_desktop_windows_are_upgraded() {
        // Captioned top-level windows and child windows: upgraded.
        assert!(plan(&heaven(), 3, true, false).is_ok());
        let child = Request { window_style: WS_CHILD | WS_VISIBLE, ..heaven() };
        assert!(plan(&child, 3, true, false).is_ok());
        // WS_POPUP | WS_CAPTION (a tool window) has a caption.
        let tool = Request { window_style: 0x8000_0000 | WS_CAPTION, ..heaven() };
        assert!(plan(&tool, 3, true, false).is_ok());
        // A popup without a caption (CS2's fullscreen window) keeps DXGI's
        // own fullscreen path; FlipUpgrade=2 upgrades it anyway.
        let cs2 = cs2_fullscreen();
        assert_eq!(plan(&cs2, 3, true, false), Err(Skip::Borderless(cs2.window_style)));
        assert!(plan(&cs2, 3, true, true).is_ok());
        // WS_BORDER alone (half of WS_CAPTION) is no caption.
        let border = Request { window_style: 0x0080_0000, ..heaven() };
        assert_eq!(plan(&border, 3, true, false), Err(Skip::Borderless(0x0080_0000)));
        // A style that could not be read counts as no caption.
        let unknown = Request { window_style: 0, ..heaven() };
        assert_eq!(plan(&unknown, 3, true, false), Err(Skip::Borderless(0)));
        // The other reasons still come first.
        let fullscreen = Request { windowed: false, ..cs2 };
        assert_eq!(plan(&fullscreen, 3, true, false), Err(Skip::Fullscreen));
        assert_eq!(Skip::Borderless(0).index(), Skip::COUNT - 1);
    }

    #[test]
    fn set_fullscreen_state_is_emulated_on_an_upgraded_chain() {
        use FullscreenCall::*;
        // Windowed -> fullscreen enters the emulation (DXGI never sees the
        // transition, so it never asks for ResizeBuffers).
        assert_eq!(set_fullscreen_call(false, true, false), Enter);
        assert_eq!(set_fullscreen_call(false, true, true), Enter);
        // Again while in it: nothing to do.
        assert_eq!(set_fullscreen_call(true, true, false), Stay);
        assert_eq!(set_fullscreen_call(true, true, true), Stay);
        // Back to windowed.
        assert_eq!(set_fullscreen_call(true, false, false), Leave);
        // Windowed while windowed: DXGI's own (no-op) call.
        assert_eq!(set_fullscreen_call(false, false, false), Stay);
        // DXGI: "pTarget ... If you pass FALSE to Fullscreen, you must set
        // this parameter to NULL".
        assert_eq!(set_fullscreen_call(false, false, true), Invalid);
        assert_eq!(set_fullscreen_call(true, false, true), Invalid);
    }

    #[test]
    fn the_emulated_fullscreen_switches_to_the_buffer_mode() {
        // CS2: 1280x960 buffers, ALLOW_MODE_SWITCH, 240 Hz asked.
        assert_eq!(
            fullscreen_mode(FLAG_ALLOW_MODE_SWITCH, 1280, 960, 240, 1),
            Some(Mode { width: 1280, height: 960, refresh_hz: 240 })
        );
        // NTSC-style rationals round to whole Hz; 0/0 or n/0 is "any".
        assert_eq!(refresh_hz(239_760, 1000), 240);
        assert_eq!(refresh_hz(60_000, 1001), 60);
        assert_eq!(refresh_hz(0, 0), 0);
        assert_eq!(refresh_hz(240, 0), 0);
        assert_eq!(fullscreen_mode(FLAG_ALLOW_MODE_SWITCH, 5120, 1440, 0, 0).unwrap().refresh_hz, 0);
        // Without ALLOW_MODE_SWITCH the desktop mode stays (DXGI_SWAP_CHAIN_FLAG).
        assert_eq!(fullscreen_mode(0, 1280, 960, 240, 1), None);
        // ResizeTarget with no size: nothing to switch to.
        assert_eq!(fullscreen_mode(FLAG_ALLOW_MODE_SWITCH, 0, 960, 240, 1), None);
        // A mode change is skipped when the current mode already serves it.
        let desktop = Mode { width: 5120, height: 1440, refresh_hz: 240 };
        assert!(mode_matches(desktop, Mode { refresh_hz: 0, ..desktop }));
        assert!(mode_matches(desktop, desktop));
        assert!(!mode_matches(desktop, Mode { refresh_hz: 120, ..desktop }));
        assert!(!mode_matches(desktop, Mode { width: 1280, height: 960, refresh_hz: 240 }));
    }

    #[test]
    fn the_fullscreen_window_loses_its_frame_and_gets_it_back() {
        let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
        let exstyle = WS_EX_OVERLAPPEDWINDOW;
        let (fs, fx) = fullscreen_style(style, exstyle);
        assert_eq!(fs, WS_VISIBLE);
        assert_eq!(fx, 0);
        // A popup keeps its styles.
        assert_eq!(fullscreen_style(0x8000_0000 | WS_VISIBLE, 0), (0x8000_0000 | WS_VISIBLE, 0));
        // Unchanged since (topmost / visibility do not count): restore.
        assert!(restore_style(fs, fx, style, exstyle));
        assert!(restore_style(fs & !WS_VISIBLE, fx | WS_EX_TOPMOST, style, exstyle));
        // The app changed the styles itself: leave them.
        assert!(!restore_style(fs | WS_CAPTION, fx, style, exstyle));
        assert!(!restore_style(fs, fx | 0x80, style, exstyle));
    }

    #[test]
    fn alt_tab_ends_the_emulated_fullscreen_after_the_grace() {
        // Never seen in front since it entered: stays.
        assert!(!occluded(false, 10_000, 0));
        // In front: never occluded.
        assert!(!occluded(true, 10_000, 9_000));
        // Out of the foreground for up to the grace (focus flickers during
        // the mode change): stays.
        assert!(!occluded(false, 10_000, 10_000 - OCCLUSION_GRACE_MS));
        // Longer: DXGI's own exclusive fullscreen would leave; so does this.
        assert!(occluded(false, 10_000, 10_000 - OCCLUSION_GRACE_MS - 1));
        // A clock that went backwards does not count as time out of front.
        assert!(!occluded(false, 5_000, 10_000));
    }

    #[test]
    fn the_app_sees_exclusive_fullscreen_and_presents_may_still_tear() {
        // GetDesc / GetFullscreenDesc: DXGI's chain is windowed, the app's is not.
        assert!(!desc_windowed(true, true));
        assert!(desc_windowed(true, false));
        assert!(!desc_windowed(false, false));
        // Present flags follow DXGI's own state: windowed under the emulation,
        // so interval 0 keeps ALLOW_TEARING (no DXGI_ERROR_INVALID_CALL).
        assert_eq!(present_flags(0, 0, true, false), PRESENT_ALLOW_TEARING);
        // DXGI's own exclusive fullscreen (a transition that bypassed the
        // hooks): no tearing flag, which DXGI refuses there.
        assert_eq!(present_flags(0, 0, true, true), 0);
    }

    #[test]
    fn resize_keeps_the_flip_shape() {
        assert_eq!(resize_buffer_count(0, 3), 0);
        assert_eq!(resize_buffer_count(1, 3), 3);
        assert_eq!(resize_buffer_count(4, 3), 4);
        assert_eq!(resize_buffer_count(99, 3), 16);
        assert_eq!(resize_flags(FLAG_ALLOW_MODE_SWITCH, true), FLAG_ALLOW_MODE_SWITCH | FLAG_ALLOW_TEARING);
        assert_eq!(resize_flags(FLAG_ALLOW_TEARING, false), 0);
    }

    #[test]
    fn interval_zero_tears_only_where_dxgi_allows_it() {
        assert_eq!(present_flags(0, 0, true, false), PRESENT_ALLOW_TEARING);
        assert_eq!(present_flags(1, 0, true, false), 0);
        assert_eq!(present_flags(0, 0, false, false), 0);
        assert_eq!(present_flags(0, 0, true, true), 0);
        assert_eq!(present_flags(0, PRESENT_TEST, true, false), PRESENT_TEST);
    }

    #[test]
    fn the_exe_list_filters_by_file_name() {
        assert!(exe_allowed(None, "heaven.exe"));
        assert!(exe_allowed(Some("  "), "heaven.exe"));
        assert!(exe_allowed(Some("foo.exe; Heaven.exe"), "heaven.exe"));
        assert!(!exe_allowed(Some("foo.exe;;"), "heaven.exe"));
    }
}
