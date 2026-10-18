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
//! | "After calling SetFullscreenState, the application must call ResizeBuffers before Present." | only windowed chains are upgraded; an app that goes fullscreen later must resize (blt-model apps normally do on `WM_SIZE`) |
//! | "MSAA swapchains are not directly supported in the flip model" | multisampled chains are not upgraded |
//!
//! And from `dxgi-flip-model.md` ("Use flip model in an HWND that is not also
//! targeted by other APIs ... GDI"): `GDI_COMPATIBLE` chains are not upgraded,
//! and the knob is off by default because an app drawing GDI into the same
//! window would lose that content (that is why Windows limits its upgrade to
//! games). `FlipUpgradeList` narrows it to named executables.
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
//! The decision ([`plan`], [`resize_buffer_count`], [`present_flags`]) is
//! portable and tested on the Linux host; the COM hooks are Windows-only.

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
    /// The device is a D3D11 device (the render-target re-bind needs its
    /// immediate context; D3D10 and D3D12 devices are left alone).
    pub d3d11_device: bool,
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
        }
    }
    /// Number of reasons ([`Skip::index`] is below it).
    pub const COUNT: usize = 7;
}

/// Decide the upgrade of one swap-chain creation. `buffers`: the
/// `FlipUpgradeBuffers` knob (clamped to 2..=16). `tearing`: ask for
/// `ALLOW_TEARING` (the caller retries without it when DXGI refuses).
pub const fn plan(r: &Request, buffers: u32, tearing: bool) -> Result<Plan, Skip> {
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
/// left alone.
pub const fn present_flags(sync_interval: u32, flags: u32, tearing: bool, fullscreen: bool) -> u32 {
    if tearing && sync_interval == 0 && !fullscreen && flags & PRESENT_TEST == 0 {
        flags | PRESENT_ALLOW_TEARING
    } else {
        flags
    }
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
    //! `IDXGISwapChain` `Present` (8), `GetDesc` (12), `ResizeBuffers` (13),
    //! `GetDesc1` (18) and `Present1` (22) serve an upgraded chain and pass
    //! every other chain straight through. Slots are the vtable order of
    //! `dxgi.h` / `dxgi1_2.h`; the immediate context's `OMSetRenderTargets`
    //! is slot 33 and `OMGetRenderTargets` 89 (`d3d11.h`), the device's
    //! `GetImmediateContext` 40.
    //!
    //! The vtables are DXGI's (one per class, shared by every object of it),
    //! so the hooks stay for the life of the process: this module pins the
    //! UMD in memory before it patches anything.

    use super::*;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::cell::Cell;

    type Hresult = i32;

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
    struct Marker {
        magic: u32,
        swap_effect: u32,
        buffer_count: u32,
        format: u32,
        flags: u32,
        planned_buffers: u32,
        tearing: u32,
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

    const _: () = assert!(core::mem::size_of::<ModeDesc>() == 28);
    const _: () = assert!(core::mem::size_of::<SwapChainDesc1>() == 48);
    const _: () = assert!(core::mem::size_of::<FullscreenDesc>() == 20);
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(core::mem::size_of::<SwapChainDesc>() == 72);
    #[cfg(target_pointer_width = "32")]
    const _: () = assert!(core::mem::size_of::<SwapChainDesc>() == 60);

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
    type ResizeBuffersFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32) -> Hresult;
    type GetDeviceFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult;
    type GetFullscreenStateFn = unsafe extern "system" fn(*mut c_void, *mut i32, *mut *mut c_void) -> Hresult;
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
    const SLOT_SC_GET_FULLSCREEN_STATE: usize = 11;
    const SLOT_SC_GET_DESC: usize = 12;
    const SLOT_SC_RESIZE_BUFFERS: usize = 13;
    const SLOT_SC1_GET_DESC1: usize = 18;
    const SLOT_SC1_PRESENT1: usize = 22;
    const SLOT_DEV_GET_IMMEDIATE_CONTEXT: usize = 40;
    const SLOT_CTX_OM_SET_RENDER_TARGETS: usize = 33;
    const SLOT_CTX_OM_GET_RENDER_TARGETS: usize = 89;
    /// `D3D11_SIMULTANEOUS_RENDER_TARGET_COUNT`.
    const RENDER_TARGETS: usize = 8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    }
    const PAGE_READWRITE: u32 = 0x04;
    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;

    /// What the UMD hands [`install`].
    #[derive(Clone, Copy, Debug)]
    pub struct Options {
        /// `FlipUpgradeBuffers`.
        pub buffers: u32,
        /// Ask for `ALLOW_TEARING` (interval-0 presents stay unthrottled when
        /// the window is promoted to independent flip).
        pub tearing: bool,
    }

    /// 0 not tried, 1 running, 2 factory hooked, 3 failed.
    static STATE: AtomicU8 = AtomicU8::new(0);
    static BUFFERS: AtomicU32 = AtomicU32::new(3);
    static TEARING: AtomicU8 = AtomicU8::new(1);

    static ORIG_CREATE_SWAP_CHAIN: AtomicUsize = AtomicUsize::new(0);
    static ORIG_CREATE_FOR_HWND: AtomicUsize = AtomicUsize::new(0);
    static ORIG_PRESENT: AtomicUsize = AtomicUsize::new(0);
    static ORIG_PRESENT1: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_DESC: AtomicUsize = AtomicUsize::new(0);
    static ORIG_GET_DESC1: AtomicUsize = AtomicUsize::new(0);
    static ORIG_RESIZE_BUFFERS: AtomicUsize = AtomicUsize::new(0);
    /// 0 not yet, 1 being patched, 2 done.
    static SWAP_CHAIN_HOOKED: AtomicU8 = AtomicU8::new(0);

    static UPGRADED: AtomicU64 = AtomicU64::new(0);
    static UPGRADED_NO_TEARING: AtomicU64 = AtomicU64::new(0);
    static UPGRADE_FAILED: AtomicU64 = AtomicU64::new(0);
    static SKIPPED: [AtomicU64; Skip::COUNT] = [const { AtomicU64::new(0) }; Skip::COUNT];
    static PRESENTS: AtomicU64 = AtomicU64::new(0);
    static PRESENTS_TEARING: AtomicU64 = AtomicU64::new(0);
    static REBINDS: AtomicU64 = AtomicU64::new(0);
    static RESIZES: AtomicU64 = AtomicU64::new(0);

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
                         created flip-model (FLIP_DISCARD, {} buffers, tearing {})",
                        BUFFERS.load(Ordering::Relaxed),
                        TEARING.load(Ordering::Relaxed) != 0
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
            let sc1 = query(sc, &IID_IDXGI_SWAP_CHAIN1);
            let (d, e) = if sc1.is_null() {
                (false, false)
            } else {
                (
                    patch(sc1, SLOT_SC1_GET_DESC1, hook_get_desc1 as usize, &ORIG_GET_DESC1),
                    patch(sc1, SLOT_SC1_PRESENT1, hook_present1 as usize, &ORIG_PRESENT1),
                )
            };
            release(sc1);
            crate::log_error!(
                "FlipUpgrade: swap chain hooks Present {a} GetDesc {b} ResizeBuffers {c} GetDesc1 {d} Present1 {e}"
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
                 {} -> {}, format {}, flags {:#x} -> {:#x}, tearing {tearing}",
                request.swap_effect,
                plan.swap_effect,
                request.buffer_count,
                plan.buffer_count,
                request.format,
                request.flags,
                if tearing { plan.flags } else { plan.flags & !FLAG_ALLOW_TEARING },
            );
        }
    }

    fn options() -> (u32, bool) {
        (BUFFERS.load(Ordering::Relaxed), TEARING.load(Ordering::Relaxed) != 0)
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
                d3d11_device: is_d3d11(device),
            };
            let (buffers, want_tearing) = options();
            let plan = match plan(&request, buffers, want_tearing) {
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
                d3d11_device: is_d3d11(device),
            };
            let (buffers, want_tearing) = options();
            let plan = match plan(&request, buffers, want_tearing) {
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
                },
            );
            hook_swap_chain_class(sc);
        }
        note_upgrade(request, plan, tearing);
    }

    unsafe fn fullscreen(sc: *mut c_void) -> bool {
        let mut state = 0i32;
        // SAFETY (caller): a live swap chain; the target out-pointer is optional.
        unsafe {
            let f: GetFullscreenStateFn = core::mem::transmute(slot(sc, SLOT_SC_GET_FULLSCREEN_STATE));
            f(sc, &mut state, core::ptr::null_mut()) >= 0 && state != 0
        }
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
        if n == 1 || n % 4096 == 0 {
            crate::log_error!("FlipUpgrade: {}", summary());
        }
    }

    unsafe extern "system" fn hook_present(sc: *mut c_void, sync_interval: u32, flags: u32) -> Hresult {
        // SAFETY: the original slot was stored before the hook went in.
        unsafe {
            let orig: PresentFn = core::mem::transmute(ORIG_PRESENT.load(Ordering::Acquire));
            let Some(m) = marker(sc) else {
                return orig(sc, sync_interval, flags);
            };
            let f = present_flags(sync_interval, flags, m.tearing != 0, m.tearing != 0 && fullscreen(sc));
            note_present(f & PRESENT_ALLOW_TEARING != 0);
            present_rebinding(sc, || orig(sc, sync_interval, f))
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
            let Some(m) = marker(sc) else {
                return orig(sc, sync_interval, flags, params);
            };
            let f = present_flags(sync_interval, flags, m.tearing != 0, m.tearing != 0 && fullscreen(sc));
            note_present(f & PRESENT_ALLOW_TEARING != 0);
            present_rebinding(sc, || orig(sc, sync_interval, f, params))
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
             [swap effect {}, MSAA {}, format {}, flags {}, fullscreen {}, no window {}, not D3D11 {}]; \
             {} presents ({} may tear), {} render-target re-binds, {} resizes",
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
            PRESENTS.load(Ordering::Relaxed),
            PRESENTS_TEARING.load(Ordering::Relaxed),
            REBINDS.load(Ordering::Relaxed),
            RESIZES.load(Ordering::Relaxed),
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
            d3d11_device: true,
        }
    }

    #[test]
    fn a_windowed_discard_chain_becomes_flip_discard() {
        let p = plan(&heaven(), 3, true).unwrap();
        assert_eq!(p.swap_effect, SWAP_EFFECT_FLIP_DISCARD);
        assert_eq!(p.buffer_count, 3);
        assert_eq!(p.flags, FLAG_ALLOW_MODE_SWITCH | FLAG_ALLOW_TEARING);
        let p = plan(&heaven(), 3, false).unwrap();
        assert_eq!(p.flags, FLAG_ALLOW_MODE_SWITCH);
    }

    #[test]
    fn buffer_count_is_clamped_and_never_lowered() {
        assert_eq!(plan(&heaven(), 0, false).unwrap().buffer_count, 2);
        assert_eq!(plan(&heaven(), 40, false).unwrap().buffer_count, 16);
        let mut r = heaven();
        r.buffer_count = 5;
        assert_eq!(plan(&r, 3, false).unwrap().buffer_count, 5);
    }

    #[test]
    fn what_the_upgrade_does_not_cover_is_left_alone() {
        let mut r = heaven();
        r.swap_effect = 1;
        assert_eq!(plan(&r, 3, true), Err(Skip::SwapEffect(1)));
        r.swap_effect = SWAP_EFFECT_FLIP_DISCARD;
        assert_eq!(plan(&r, 3, true), Err(Skip::SwapEffect(SWAP_EFFECT_FLIP_DISCARD)));
        let mut r = heaven();
        r.sample_count = 4;
        assert_eq!(plan(&r, 3, true), Err(Skip::Multisample(4)));
        for fmt in [29u32, 91, 88, 10, 24] {
            let mut r = heaven();
            r.format = fmt;
            assert_eq!(plan(&r, 3, true), Err(Skip::Format(fmt)), "format {fmt}");
        }
        let mut r = heaven();
        r.flags |= 0x4; // GDI_COMPATIBLE
        assert_eq!(plan(&r, 3, true), Err(Skip::Flags(0x6)));
        let mut r = heaven();
        r.windowed = false;
        assert_eq!(plan(&r, 3, true), Err(Skip::Fullscreen));
        let mut r = heaven();
        r.has_window = false;
        assert_eq!(plan(&r, 3, true), Err(Skip::NoWindow));
        let mut r = heaven();
        r.d3d11_device = false;
        assert_eq!(plan(&r, 3, true), Err(Skip::NotD3D11));
        let mut r = heaven();
        r.format = FORMAT_B8G8R8A8_UNORM;
        r.flags = 0;
        assert!(plan(&r, 3, true).is_ok());
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
