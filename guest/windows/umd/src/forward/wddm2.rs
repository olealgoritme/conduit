//! The WDDM 2.x D3D11 DDI entry points (`ddi_level.rs`, interface D3DWDDM2_3).
//!
//! WDDM 2.0 retyped six create families and Flush; the new argument structures
//! extend the old ones without moving a field:
//!
//! * `D3DWDDM2_0DDIARG_CREATE{SHADERRESOURCE,RENDERTARGET,UNORDEREDACCESS}VIEW`
//!   keep `hDrvResource, Format, ResourceDimension` and the union; only the
//!   Tex2D member appends `PlaneSlice`. DXVK picks the plane of a planar
//!   resource from the view format (`D3D11Device::GetViewPlaneIndex`), which
//!   the runtime already validated against `PlaneSlice`, so the 10.x/11.0
//!   readers are forwarded the same pointer.
//! * `D3DWDDM2_0DDI_RASTERIZER_DESC` appends `ConservativeRasterizationMode` to
//!   the 11.1 desc; `ForcedSampleCount` is forwarded like the 11.1 reader does.
//!   Conservative rasterization is not advertised, so the mode is always OFF.
//! * `D3DWDDM2_0DDIARG_CREATEQUERY` appends `ContextType`. One context.
//! * `PFND3DWDDM2_0DDI_FLUSH` adds a context-type argument. One context.
//!
//! The appended WDDM 2.0/2.1/2.2 entries are real no-ops for features this
//! driver does not advertise (hardware content protection, standard-swizzle
//! resource layouts, shader comments, shader-cache sessions); keyed-mutex sync
//! tokens (2.1) flush on release so a consumer in another process sees the
//! producer's work.

use super::*;

static WDDM2_LOG_COUNT: LogThrottle = LogThrottle::new();

pub(crate) unsafe extern "system" fn calc_size_srv_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATESHADERRESOURCEVIEW,
) -> ddi::SIZE_T {
    calc_size_srv(h, a.cast())
}

pub(crate) unsafe extern "system" fn create_srv_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATESHADERRESOURCEVIEW,
    h_srv: ddi::D3D10DDI_HSHADERRESOURCEVIEW,
    hrt: ddi::D3D10DDI_HRTSHADERRESOURCEVIEW,
) {
    create_srv(h, a.cast(), h_srv, hrt)
}

pub(crate) unsafe extern "system" fn calc_size_rtv_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATERENDERTARGETVIEW,
) -> ddi::SIZE_T {
    calc_size_rtv(h, a.cast())
}

pub(crate) unsafe extern "system" fn create_rtv_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATERENDERTARGETVIEW,
    h_rtv: ddi::D3D10DDI_HRENDERTARGETVIEW,
    hrt: ddi::D3D10DDI_HRTRENDERTARGETVIEW,
) {
    create_rtv(h, a.cast(), h_rtv, hrt)
}

pub(crate) unsafe extern "system" fn calc_size_uav_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATEUNORDEREDACCESSVIEW,
) -> ddi::SIZE_T {
    calc_size_uav(h, a.cast())
}

pub(crate) unsafe extern "system" fn create_uav_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATEUNORDEREDACCESSVIEW,
    h_uav: ddi::D3D11DDI_HUNORDEREDACCESSVIEW,
    hrt: ddi::D3D11DDI_HRTUNORDEREDACCESSVIEW,
) {
    create_uav(h, a.cast(), h_uav, hrt)
}

pub(crate) unsafe extern "system" fn calc_size_raster_wddm2_0(
    h: Hdevice,
    d: *const ddi::D3DWDDM2_0DDI_RASTERIZER_DESC,
) -> ddi::SIZE_T {
    calc_size_raster(h, d.cast())
}

pub(crate) unsafe extern "system" fn create_rasterizer_state_wddm2_0(
    h: Hdevice,
    d: *const ddi::D3DWDDM2_0DDI_RASTERIZER_DESC,
    h_rs: ddi::D3D10DDI_HRASTERIZERSTATE,
    _hrt: ddi::D3D10DDI_HRTRASTERIZERSTATE,
) {
    if !d.is_null() && (*d).ConservativeRasterizationMode != 0 {
        if WDDM2_LOG_COUNT.first_n(16).is_some() {
            log_error!(
                "DDI CreateRasterizerState(WDDM2.0): conservative mode {} requested but not advertised; ignored",
                (*d).ConservativeRasterizationMode
            );
        }
    }
    let forced = if d.is_null() {
        0
    } else {
        (*d).ForcedSampleCount
    };
    create_rasterizer_state_forced(h, d.cast(), forced, h_rs)
}

pub(crate) unsafe extern "system" fn calc_size_query_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATEQUERY,
) -> ddi::SIZE_T {
    calc_size_query(h, a.cast())
}

pub(crate) unsafe extern "system" fn create_query_wddm2_0(
    h: Hdevice,
    a: *const ddi::D3DWDDM2_0DDIARG_CREATEQUERY,
    h_query: ddi::D3D10DDI_HQUERY,
    hrt: ddi::D3D10DDI_HRTQUERY,
) {
    create_query(h, a.cast(), h_query, hrt)
}

pub(crate) unsafe extern "system" fn flush_wddm2_0(
    h: Hdevice,
    _context_type: u32,
    flush_flags: u32,
) -> ddi::BOOL {
    flush_11_1(h, flush_flags)
}

pub(crate) unsafe extern "system" fn set_hardware_protection(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
    protected: ddi::BOOL,
) {
    if protected != 0 && WDDM2_LOG_COUNT.first_n(16).is_some() {
        log_error!("DDI SetHardwareProtection: no hardware content protection; ignored");
    }
}

pub(crate) unsafe extern "system" fn set_hardware_protection_state(_h: Hdevice, enable: ddi::BOOL) {
    if enable != 0 && WDDM2_LOG_COUNT.first_n(16).is_some() {
        log_error!("DDI SetHardwareProtectionState: no hardware content protection; ignored");
    }
}

/// Only called for resources with a standard-swizzle / 64KB-undefined layout,
/// which this driver never advertises. Answer "no layout" deterministically
/// rather than leaving the runtime's outputs uninitialised.
pub(crate) unsafe extern "system" fn get_resource_layout(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
    subresource_count: u32,
    km_handle: *mut ddi::D3DKMT_HANDLE,
    layout: *mut ddi::D3DWDDM2_0DDI_TEXTURE_LAYOUT,
    mip_swizzle_transition: *mut u32,
    subresources: *mut ddi::D3DWDDM2_0DDI_SUBRESOURCE_LAYOUT,
) {
    if WDDM2_LOG_COUNT.first_n(16).is_some() {
        log_error!("DDI GetResourceLayout({subresource_count}): no special layouts advertised");
    }
    if !km_handle.is_null() {
        *km_handle = 0;
    }
    if !layout.is_null() {
        layout.write_bytes(0, 1);
    }
    if !mip_swizzle_transition.is_null() {
        *mip_swizzle_transition = 0;
    }
    if !subresources.is_null() {
        subresources.write_bytes(0, subresource_count as usize);
    }
}

pub(crate) unsafe extern "system" fn retrieve_shader_comment(
    _h: Hdevice,
    _shader: ddi::D3D10DDI_HSHADER,
    buffer: *mut u16,
    count: *mut ddi::SIZE_T,
) -> ddi::HRESULT {
    // No comments: an empty string.
    if !count.is_null() {
        if !buffer.is_null() && *count > 0 {
            *buffer = 0;
        }
        *count = 1;
    }
    0
}

pub(crate) unsafe extern "system" fn acquire_resource_wddm2_1(
    _h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
    _sync_token: ddi::HANDLE,
) {
}

/// Releasing a keyed-mutex sync token hands the resource to another process:
/// get this device's work submitted first, as the WDDM 1.3 path does on the
/// `pfnFlush` the runtime issues before `D3DKMTReleaseKeyedMutex2`.
pub(crate) unsafe extern "system" fn release_resource_wddm2_1(
    h: Hdevice,
    _resource: ddi::D3D10DDI_HRESOURCE,
    _sync_token: ddi::HANDLE,
) {
    flush(h);
}

pub(crate) unsafe extern "system" fn calc_size_shader_cache_session(_h: Hdevice) -> ddi::SIZE_T {
    8
}

pub(crate) unsafe extern "system" fn create_shader_cache_session(
    _h: Hdevice,
    _session: ddi::D3DWDDM2_2DDI_HCACHESESSION,
    _rt: ddi::D3DWDDM2_2DDI_HRTCACHESESSION,
) {
}

pub(crate) unsafe extern "system" fn destroy_shader_cache_session(
    _h: Hdevice,
    _session: ddi::D3DWDDM2_2DDI_HCACHESESSION,
) {
}

pub(crate) unsafe extern "system" fn set_shader_cache_session(
    _h: Hdevice,
    _session: ddi::D3DWDDM2_2DDI_HCACHESESSION,
) {
}

// --- DXGI 1.4 .. 1.6.1 -------------------------------------------------------

pub(crate) unsafe extern "system" fn dxgi_present1_6_1(arg: *mut ddi::DXGI1_6_1_DDI_ARG_PRESENT) -> i32 {
    // Identical layout up to `RotationHint`, which replaces DXGI 1.3's
    // `Reserved` and is not read; `BackBufferMultiplicity` is LDA-only.
    dxgi_present1(arg.cast())
}

pub(crate) unsafe extern "system" fn dxgi_offer_resources1(
    arg: *mut ddi::DXGI_DDI_ARG_OFFERRESOURCES1,
) -> i32 {
    dxgi_offer_resources(arg.cast())
}

/// `D3DDDI_RECLAIM_RESULT` 0 = OK (contents kept), the same element size and
/// value the 1.3 handler writes into its BOOL `pDiscarded` array.
pub(crate) unsafe extern "system" fn dxgi_reclaim_resources1(
    arg: *mut ddi::DXGI_DDI_ARG_RECLAIMRESOURCES1,
) -> i32 {
    dxgi_reclaim_resources(arg.cast())
}

pub(crate) unsafe extern "system" fn dxgi_trim_residency_set(
    _arg: *mut ddi::DXGI_DDI_ARG_TRIMRESIDENCYSET,
) -> i32 {
    // Nothing is evicted by this driver (VidMm owns residency, the KMD keeps
    // every allocation resident).
    0
}

pub(crate) unsafe extern "system" fn dxgi_check_mpo_color_space_support(
    arg: *mut ddi::DXGI_DDI_ARG_CHECKMULTIPLANEOVERLAYCOLORSPACESUPPORT,
) -> i32 {
    if !arg.is_null() {
        (*arg).Supported = 0;
    }
    0
}

pub(crate) unsafe extern "system" fn dxgi_present_mpo1(
    _arg: *mut ddi::DXGI1_6_1_DDI_ARG_PRESENTMULTIPLANEOVERLAY,
) -> i32 {
    // Only reachable after an MPO color-space query said yes, which it never
    // does; refuse like the reserved slots.
    dxgi_reserved_unsupported(core::ptr::null_mut())
}

const _: () = {
    // The prefix casts above rely on these layouts.
    assert!(
        core::mem::offset_of!(ddi::D3DWDDM2_0DDIARG_CREATESHADERRESOURCEVIEW, __bindgen_anon_1)
            == core::mem::offset_of!(ddi::D3D11DDIARG_CREATESHADERRESOURCEVIEW, __bindgen_anon_1)
    );
    assert!(
        core::mem::offset_of!(ddi::D3DWDDM2_0DDIARG_CREATERENDERTARGETVIEW, __bindgen_anon_1)
            == core::mem::offset_of!(ddi::D3D10DDIARG_CREATERENDERTARGETVIEW, __bindgen_anon_1)
    );
    assert!(
        core::mem::offset_of!(ddi::D3DWDDM2_0DDIARG_CREATEUNORDEREDACCESSVIEW, __bindgen_anon_1)
            == core::mem::offset_of!(ddi::D3D11DDIARG_CREATEUNORDEREDACCESSVIEW, __bindgen_anon_1)
    );
    assert!(
        core::mem::offset_of!(ddi::D3DWDDM2_0DDI_RASTERIZER_DESC, ForcedSampleCount)
            == core::mem::size_of::<ddi::D3D10_DDI_RASTERIZER_DESC>()
    );
    assert!(
        core::mem::offset_of!(ddi::D3DWDDM2_0DDIARG_CREATEQUERY, ContextType)
            == core::mem::size_of::<ddi::D3D10DDIARG_CREATEQUERY>()
    );
    assert!(
        core::mem::offset_of!(ddi::DXGI1_6_1_DDI_ARG_PRESENT, DirtyRects)
            == core::mem::offset_of!(ddi::DXGI_DDI_ARG_PRESENT1, DirtyRects)
    );
    assert!(
        core::mem::offset_of!(ddi::DXGI_DDI_ARG_RECLAIMRESOURCES1, Resources)
            == core::mem::offset_of!(ddi::DXGI_DDI_ARG_RECLAIMRESOURCES, Resources)
    );
};
