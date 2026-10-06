//! The D3D11.1 video DDI (`D3D11_1DDI_VIDEODEVICEFUNCS`): video decoder and
//! video processor, forwarded to DXVK's `ID3D11VideoDevice` /
//! `ID3D11VideoContext` like every other DDI is forwarded to its D3D11 COM
//! counterpart.
//!
//! The runtime fetches this table through `PFND3D10DDI_RETRIEVESUBOBJECT`
//! (`D3D11_1DDI_VIDEO_FUNCTIONS`), which `CreateDevice` hands out for devices
//! the `VideoDdi` knob admits (`video_ddi_enabled`). Without it the runtime
//! has no `ID3D11VideoDevice` for the app at all, which is how Helios behaved
//! before: no D3D11VA, no DXVA-HD.
//!
//! On NVK on RM, DXVK decodes H.264 on Vulkan Video (third_party/patches/dxvk
//! 0002); on Venus it reports no decoder profiles, only the video processor.
//!
//! Decoder buffers. The D3D11 runtime implements `GetDecoderBuffer` by
//! creating one resource per buffer type (sized by
//! `pfnGetVideoDecoderBufferInfo`, `DecoderBufferType` set in
//! `D3D11DDIARG_CREATERESOURCE`) and mapping it for the app;
//! `pfnVideoDecoderSubmitBuffers` then names those resources. `create_resource`
//! makes them CPU-readable staging buffers (see `decoder_buffer_desc`); here
//! their contents are copied into DXVK's own decoder buffers, which the
//! decoder reads (the bitstream buffer is DXVK's mapped Vulkan buffer), and
//! submitted through the public D3D11 API. One memcpy per buffer per frame.
//!
//! Content protection (crypto sessions, authenticated channels) is not
//! implemented: those entries refuse, as DXVK's API-level ones do.

use super::*;

use windows::core::GUID as ApiGuid;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, S_OK, SIZE};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;

/// Whether `RetrieveSubObject` hands this device the video function table
/// (`VideoDdi` knob, `crate::knobs::VIDEO_DDI`).
pub(crate) fn video_ddi_enabled(dev: &HeliosDevice) -> bool {
    match crate::knobs::VIDEO_DDI.get() {
        0 => false,
        1 => dev.dxvk.is_nvk(),
        _ => true,
    }
}

// The DDI's sub-object id for the D3D11.1 video function table, and the
// shape of its input (`D3D11_1DDI_VIDEO_INPUT`) from d3d10umddi.h.
const D3D11_1DDI_VIDEO_FUNCTIONS: u32 = 2;

// D3D11_1DDI_VIDEO_DECODER_BUFFER_TYPE is the API's D3D11_VIDEO_DECODER_BUFFER_TYPE
// plus one (0 is "unknown"/not a decoder buffer).
const DDI_BUFFER_PICTURE_PARAMETERS: i32 = 1;
const DDI_BUFFER_INVERSE_QUANTIZATION_MATRIX: i32 = 5;
const DDI_BUFFER_SLICE_CONTROL: i32 = 6;
const DDI_BUFFER_BITSTREAM: i32 = 7;

// D3D10_DDI_RESOURCE_USAGE / D3D11_USAGE staging.
const USAGE_STAGING: u32 = 3;

/// `PFND3D10DDI_RETRIEVESUBOBJECT`, installed by `CreateDevice` for the
/// >=11.1 interfaces.
pub(crate) unsafe extern "system" fn retrieve_sub_object(
    h: Hdevice,
    sub_device_id: u32,
    param_size: ddi::SIZE_T,
    params: *mut c_void,
    _output_param_size: ddi::SIZE_T,
    _output: *mut c_void,
) -> ddi::HRESULT {
    let Some(dev) = helios_device(h) else {
        return E_FAIL.0;
    };
    if sub_device_id != D3D11_1DDI_VIDEO_FUNCTIONS {
        // D3DWDDM2_0DDI_VIDEO_FUNCTIONS (3) and anything newer: this UMD
        // negotiates at most WDDM 1.3, so the runtime does not ask for them.
        log_error!("DDI RetrieveSubObject: unsupported sub-object {sub_device_id}");
        return E_NOTIMPL.0;
    }
    if !video_ddi_enabled(dev) {
        trace_line!("DDI RetrieveSubObject: video DDI off for this device (VideoDdi knob)");
        return E_NOTIMPL.0;
    }
    if params.is_null() || (param_size as usize) < core::mem::size_of::<ddi::D3D11_1DDI_VIDEO_INPUT>() {
        return E_INVALIDARG.0;
    }
    let input = &*(params as *const ddi::D3D11_1DDI_VIDEO_INPUT);
    if input.p11VideoDeviceFuncs.is_null() {
        return E_INVALIDARG.0;
    }
    *input.p11VideoDeviceFuncs = video_device_funcs();
    log_error!("DDI RetrieveSubObject: D3D11.1 video function table installed");
    S_OK.0
}

fn video_device_funcs() -> ddi::D3D11_1DDI_VIDEODEVICEFUNCS {
    // Every field named: a header change that adds one fails compilation.
    ddi::D3D11_1DDI_VIDEODEVICEFUNCS {
        pfnGetVideoDecoderProfileCount: Some(get_video_decoder_profile_count),
        pfnGetVideoDecoderProfile: Some(get_video_decoder_profile),
        pfnCheckVideoDecoderFormat: Some(check_video_decoder_format),
        pfnGetVideoDecoderConfigCount: Some(get_video_decoder_config_count),
        pfnGetVideoDecoderConfig: Some(get_video_decoder_config),
        pfnGetVideoDecoderBufferTypeCount: Some(get_video_decoder_buffer_type_count),
        pfnGetVideoDecoderBufferInfo: Some(get_video_decoder_buffer_info),
        pfnCalcPrivateVideoDecoderSize: Some(calc_size_video_decoder),
        pfnCreateVideoDecoder: Some(create_video_decoder),
        pfnDestroyVideoDecoder: Some(destroy_video_decoder),
        pfnVideoDecoderExtension: Some(video_decoder_extension),
        pfnVideoDecoderBeginFrame: Some(video_decoder_begin_frame),
        pfnVideoDecoderEndFrame: Some(video_decoder_end_frame),
        pfnVideoDecoderSubmitBuffers: Some(video_decoder_submit_buffers),
        pfnCalcPrivateVideoProcessorEnumSize: Some(calc_size_video_processor_enum),
        pfnCreateVideoProcessorEnum: Some(create_video_processor_enum),
        pfnDestroyVideoProcessorEnum: Some(destroy_video_processor_enum),
        pfnCheckVideoProcessorFormat: Some(check_video_processor_format),
        pfnGetVideoProcessorCaps: Some(get_video_processor_caps),
        pfnGetVideoProcessorRateConversionCaps: Some(get_video_processor_rate_conversion_caps),
        pfnGetVideoProcessorCustomRate: Some(get_video_processor_custom_rate),
        pfnGetVideoProcessorFilterRange: Some(get_video_processor_filter_range),
        pfnCalcPrivateVideoProcessorSize: Some(calc_size_video_processor),
        pfnCreateVideoProcessor: Some(create_video_processor),
        pfnDestroyVideoProcessor: Some(destroy_video_processor),
        pfnVideoProcessorSetOutputTargetRect: Some(vp_set_output_target_rect),
        pfnVideoProcessorSetOutputBackgroundColor: Some(vp_set_output_background_color),
        pfnVideoProcessorSetOutputColorSpace: Some(vp_set_output_color_space),
        pfnVideoProcessorSetOutputAlphaFillMode: Some(vp_set_output_alpha_fill_mode),
        pfnVideoProcessorSetOutputConstriction: Some(vp_set_output_constriction),
        pfnVideoProcessorSetOutputStereoMode: Some(vp_set_output_stereo_mode),
        pfnVideoProcessorSetOutputExtension: Some(vp_output_extension),
        pfnVideoProcessorGetOutputExtension: Some(vp_output_extension),
        pfnVideoProcessorSetStreamFrameFormat: Some(vp_set_stream_frame_format),
        pfnVideoProcessorSetStreamColorSpace: Some(vp_set_stream_color_space),
        pfnVideoProcessorSetStreamOutputRate: Some(vp_set_stream_output_rate),
        pfnVideoProcessorSetStreamSourceRect: Some(vp_set_stream_source_rect),
        pfnVideoProcessorSetStreamDestRect: Some(vp_set_stream_dest_rect),
        pfnVideoProcessorSetStreamAlpha: Some(vp_set_stream_alpha),
        pfnVideoProcessorSetStreamPalette: Some(vp_set_stream_palette),
        pfnVideoProcessorSetStreamPixelAspectRatio: Some(vp_set_stream_pixel_aspect_ratio),
        pfnVideoProcessorSetStreamLumaKey: Some(vp_set_stream_luma_key),
        pfnVideoProcessorSetStreamStereoFormat: Some(vp_set_stream_stereo_format),
        pfnVideoProcessorSetStreamAutoProcessingMode: Some(vp_set_stream_auto_processing_mode),
        pfnVideoProcessorSetStreamFilter: Some(vp_set_stream_filter),
        pfnVideoProcessorSetStreamExtension: Some(vp_stream_extension),
        pfnVideoProcessorGetStreamExtension: Some(vp_stream_extension),
        pfnVideoProcessorBlt: Some(video_processor_blt),
        pfnCalcPrivateVideoDecoderOutputViewSize: Some(calc_size_vdov),
        pfnCreateVideoDecoderOutputView: Some(create_vdov),
        pfnDestroyVideoDecoderOutputView: Some(destroy_vdov),
        pfnCalcPrivateVideoProcessorInputViewSize: Some(calc_size_vpiv),
        pfnCreateVideoProcessorInputView: Some(create_vpiv),
        pfnDestroyVideoProcessorInputView: Some(destroy_vpiv),
        pfnCalcPrivateVideoProcessorOutputViewSize: Some(calc_size_vpov),
        pfnCreateVideoProcessorOutputView: Some(create_vpov),
        pfnDestroyVideoProcessorOutputView: Some(destroy_vpov),
        pfnVideoProcessorInputViewReadAfterWriteHazard: Some(vpiv_read_after_write_hazard),
        pfnGetContentProtectionCaps: Some(get_content_protection_caps),
        pfnGetCryptoKeyExchangeType: Some(get_crypto_key_exchange_type),
        pfnCalcPrivateCryptoSessionSize: Some(calc_size_crypto_session),
        pfnCreateCryptoSession: Some(create_crypto_session),
        pfnDestroyCryptoSession: Some(destroy_crypto_session),
        pfnGetCertificateSize: Some(get_certificate_size),
        pfnGetCertificate: Some(get_certificate),
        pfnNegotiateCryptoSessionKeyExchange: Some(negotiate_crypto_session_key_exchange),
        pfnEncryptionBlt: Some(encryption_blt),
        pfnDecryptionBlt: Some(decryption_blt),
        pfnStartSessionKeyRefresh: Some(start_session_key_refresh),
        pfnFinishSessionKeyRefresh: Some(finish_session_key_refresh),
        pfnGetEncryptionBltKey: Some(get_encryption_blt_key),
        pfnCalcPrivateAuthenticatedChannelSize: Some(calc_size_auth_channel),
        pfnCreateAuthenticatedChannel: Some(create_auth_channel),
        pfnDestroyAuthenticatedChannel: Some(destroy_auth_channel),
        pfnNegotiateAuthenticatedChannelKeyExchange: Some(negotiate_auth_channel_key_exchange),
        pfnQueryAuthenticatedChannel: Some(query_auth_channel),
        pfnConfigureAuthenticatedChannel: Some(configure_auth_channel),
        pfnVideoDecoderGetHandle: Some(video_decoder_get_handle),
        pfnCryptoSessionGetHandle: Some(crypto_session_get_handle),
        pfnVideoProcessorSetStreamRotation: Some(vp_set_stream_rotation),
        pfnGetCaptureHandle: Some(get_capture_handle),
    }
}

// --- DDI <-> API conversions ------------------------------------------------
//
// The DDI video structs are the API structs field for field (the DDI was
// written as a lowering of the API). Copy through a size-checked
// reinterpretation rather than by hand, so a transcription slip cannot hide.

unsafe fn reinterpret<T: Copy, U: Copy>(src: &T) -> U {
    const { assert!(core::mem::size_of::<T>() == core::mem::size_of::<U>()) };
    core::ptr::read_unaligned(src as *const T as *const U)
}

unsafe fn api_guid(g: *const ddi::GUID) -> ApiGuid {
    reinterpret::<ddi::GUID, ApiGuid>(&*g)
}

unsafe fn api_decoder_desc(d: *const ddi::D3D11_1DDI_VIDEO_DECODER_DESC) -> D3D11_VIDEO_DECODER_DESC {
    let d = &*d;
    D3D11_VIDEO_DECODER_DESC {
        Guid: api_guid(&d.Guid),
        SampleWidth: d.SampleWidth,
        SampleHeight: d.SampleHeight,
        OutputFormat: DXGI_FORMAT(d.OutputFormat as i32),
    }
}

unsafe fn api_rect(r: *const ddi::RECT) -> Option<*const RECT> {
    (!r.is_null()).then(|| r as *const RECT)
}

unsafe fn api_rational(r: *const ddi::DXGI_RATIONAL) -> Option<*const DXGI_RATIONAL> {
    (!r.is_null()).then(|| r as *const DXGI_RATIONAL)
}

unsafe fn api_color_space(c: *const ddi::D3D11_1DDI_VIDEO_PROCESSOR_COLOR_SPACE) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    reinterpret(&*c)
}

fn bool32(b: ddi::BOOL) -> BOOL {
    BOOL((b != 0) as i32)
}

// --- COM lookups --------------------------------------------------------------

unsafe fn video_device(h: Hdevice) -> Option<ID3D11VideoDevice> {
    d3d11_device(h)?.cast::<ID3D11VideoDevice>().ok()
}

unsafe fn video_context(h: Hdevice) -> Option<ID3D11VideoContext> {
    d3d11_context(h)?.cast::<ID3D11VideoContext>().ok()
}

fn hr_of(r: windows::core::Result<()>) -> ddi::HRESULT {
    match r {
        Ok(()) => S_OK.0,
        Err(e) => e.code().0,
    }
}

// --- Decoder --------------------------------------------------------------------

unsafe extern "system" fn get_video_decoder_profile_count(h: Hdevice, count: *mut ddi::UINT) {
    if count.is_null() {
        return;
    }
    *count = video_device(h).map_or(0, |v| v.GetVideoDecoderProfileCount());
}

unsafe extern "system" fn get_video_decoder_profile(h: Hdevice, index: ddi::UINT, guid: *mut ddi::GUID) {
    let Some(v) = video_device(h) else { return };
    if guid.is_null() {
        return;
    }
    if let Ok(g) = v.GetVideoDecoderProfile(index) {
        *guid = reinterpret::<ApiGuid, ddi::GUID>(&g);
    }
}

unsafe extern "system" fn check_video_decoder_format(
    h: Hdevice,
    profile: *const ddi::GUID,
    format: ddi::DXGI_FORMAT,
    supported: *mut ddi::BOOL,
) {
    if supported.is_null() {
        return;
    }
    *supported = 0;
    let Some(v) = video_device(h) else { return };
    if profile.is_null() {
        return;
    }
    let g = api_guid(profile);
    if let Ok(b) = v.CheckVideoDecoderFormat(&g, DXGI_FORMAT(format as i32)) {
        *supported = b.0;
    }
}

unsafe extern "system" fn get_video_decoder_config_count(
    h: Hdevice,
    desc: *const ddi::D3D11_1DDI_VIDEO_DECODER_DESC,
    count: *mut ddi::UINT,
) {
    if count.is_null() {
        return;
    }
    *count = 0;
    let Some(v) = video_device(h) else { return };
    if desc.is_null() {
        return;
    }
    let d = api_decoder_desc(desc);
    *count = v.GetVideoDecoderConfigCount(&d).unwrap_or(0);
}

unsafe extern "system" fn get_video_decoder_config(
    h: Hdevice,
    desc: *const ddi::D3D11_1DDI_VIDEO_DECODER_DESC,
    index: ddi::UINT,
    config: *mut ddi::D3D11_1DDI_VIDEO_DECODER_CONFIG,
) {
    let Some(v) = video_device(h) else { return };
    if desc.is_null() || config.is_null() {
        return;
    }
    let d = api_decoder_desc(desc);
    let mut c = D3D11_VIDEO_DECODER_CONFIG::default();
    if v.GetVideoDecoderConfig(&d, index, &mut c).is_ok() {
        *config = reinterpret(&c);
    }
}

/// The buffers an H.264 VLD decoder takes, with the sizes DXVK's decoder
/// accepts (`d3d11_video_decoder.cpp`): picture parameters, inverse
/// quantization matrix, slice control (up to 255 long-format entries) and
/// the bitstream (one raw frame's worth, at least 2 MiB).
fn decoder_buffers(desc: &ddi::D3D11_1DDI_VIDEO_DECODER_DESC) -> [(i32, u32); 4] {
    let w = (desc.SampleWidth + 15) & !15;
    let h = (desc.SampleHeight + 15) & !15;
    let bitstream = (w.saturating_mul(h)).max(2 << 20);
    [
        (DDI_BUFFER_PICTURE_PARAMETERS, 4096),
        (DDI_BUFFER_INVERSE_QUANTIZATION_MATRIX, 4096),
        (DDI_BUFFER_SLICE_CONTROL, 256 << 10),
        (DDI_BUFFER_BITSTREAM, bitstream),
    ]
}

unsafe extern "system" fn get_video_decoder_buffer_type_count(
    _h: Hdevice,
    desc: *const ddi::D3D11_1DDI_VIDEO_DECODER_DESC,
    count: *mut ddi::UINT,
) {
    if count.is_null() {
        return;
    }
    *count = if desc.is_null() { 0 } else { decoder_buffers(&*desc).len() as u32 };
}

unsafe extern "system" fn get_video_decoder_buffer_info(
    _h: Hdevice,
    desc: *const ddi::D3D11_1DDI_VIDEO_DECODER_DESC,
    index: ddi::UINT,
    info: *mut ddi::D3D11_1DDI_VIDEO_DECODER_BUFFER_INFO,
) {
    if desc.is_null() || info.is_null() {
        return;
    }
    let Some(&(ty, size)) = decoder_buffers(&*desc).get(index as usize) else {
        return;
    };
    (*info).Type = ty as _;
    (*info).Size = size;
    (*info).Usage = USAGE_STAGING;
}

/// Buffer description for a `DecoderBufferType` resource: a CPU-readable
/// staging buffer whatever the runtime asked for, since the UMD reads it
/// back at `pfnVideoDecoderSubmitBuffers`.
pub(crate) fn decoder_buffer_desc(byte_width: u32) -> D3D11_BUFFER_DESC {
    D3D11_BUFFER_DESC {
        ByteWidth: byte_width,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    }
}

/// Map type for a decoder buffer: staging buffers take no DISCARD or
/// NO_OVERWRITE, which the runtime may use for its write map.
pub(crate) fn decoder_buffer_map_type(map_type: u32) -> u32 {
    match map_type {
        // D3D10_DDI_MAP_WRITE_DISCARD, D3D10_DDI_MAP_WRITE_NOOVERWRITE
        4 | 5 => 2,
        t => t,
    }
}

unsafe extern "system" fn calc_size_video_decoder(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEODECODER,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_video_decoder(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEODECODER,
    h_decode: ddi::D3D11_1DDI_HDECODE,
    _hrt: ddi::D3D11_1DDI_HRTDECODE,
) -> ddi::HRESULT {
    clear_handle(h_decode);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let desc = api_decoder_desc(&a.Desc);
    let config: D3D11_VIDEO_DECODER_CONFIG = reinterpret(&a.Config);
    match v.CreateVideoDecoder(&desc, &config) {
        Ok(decoder) => {
            log_error!(
                "DDI create_video_decoder: {}x{} fmt={} raw={}",
                desc.SampleWidth,
                desc.SampleHeight,
                desc.OutputFormat.0,
                config.ConfigBitstreamRaw
            );
            store_com(h_decode, decoder);
            S_OK.0
        }
        Err(e) => {
            log_error!("DDI create_video_decoder failed: {e:?}");
            e.code().0
        }
    }
}

unsafe extern "system" fn destroy_video_decoder(_h: Hdevice, h_decode: ddi::D3D11_1DDI_HDECODE) {
    release_com(h_decode);
}

unsafe extern "system" fn video_decoder_extension(
    _h: Hdevice,
    _h_decode: ddi::D3D11_1DDI_HDECODE,
    _a: *const ddi::D3D11_1DDIARG_VIDEODECODEREXTENSION,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn video_decoder_begin_frame(
    h: Hdevice,
    h_decode: ddi::D3D11_1DDI_HDECODE,
    arg: *const ddi::D3D11_1DDIARG_VIDEODECODERBEGINFRAME,
) -> ddi::HRESULT {
    let Some(ctx) = video_context(h) else { return E_FAIL.0 };
    let Some(decoder) = load_com::<ID3D11VideoDecoder>(h_decode) else { return E_INVALIDARG.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let Some(view) = load_com::<ID3D11VideoDecoderOutputView>(a.hOutputView) else {
        return E_INVALIDARG.0;
    };
    let key = (!a.pContentKey.is_null()).then_some(a.pContentKey);
    hr_of(ctx.DecoderBeginFrame(&*decoder, &*view, a.ContentKeySize, key))
}

unsafe extern "system" fn video_decoder_end_frame(h: Hdevice, h_decode: ddi::D3D11_1DDI_HDECODE) {
    let Some(ctx) = video_context(h) else { return };
    let Some(decoder) = load_com::<ID3D11VideoDecoder>(h_decode) else { return };
    if let Err(e) = ctx.DecoderEndFrame(&*decoder) {
        log_error!("DDI video_decoder_end_frame failed: {e:?}");
        set_runtime_error(h, e.code().0);
    }
}

unsafe extern "system" fn video_decoder_submit_buffers(
    h: Hdevice,
    h_decode: ddi::D3D11_1DDI_HDECODE,
    count: ddi::UINT,
    descs: *const ddi::D3D11_1DDI_VIDEO_DECODER_BUFFER_DESC,
) -> ddi::HRESULT {
    let Some(ctx) = video_context(h) else { return E_FAIL.0 };
    let Some(context) = d3d11_context(h) else { return E_FAIL.0 };
    let Some(decoder) = load_com::<ID3D11VideoDecoder>(h_decode) else { return E_INVALIDARG.0 };
    if count != 0 && descs.is_null() {
        return E_INVALIDARG.0;
    }
    let descs = core::slice::from_raw_parts(descs, count as usize);
    let mut api: Vec<D3D11_VIDEO_DECODER_BUFFER_DESC> = Vec::with_capacity(descs.len());

    for d in descs {
        let ty = d.BufferType as i32;
        if ty <= 0 {
            continue;
        }
        let api_type = D3D11_VIDEO_DECODER_BUFFER_TYPE(ty - 1);
        let Some(res) = load_resource(d.hResource) else {
            log_error!("DDI video_decoder_submit_buffers: buffer type {ty} has no resource");
            return E_INVALIDARG.0;
        };

        // Copy the app's data into DXVK's decoder buffer of the same type.
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if let Err(e) = context.Map(&*res, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) {
            log_error!("DDI video_decoder_submit_buffers: map of type {ty} failed: {e:?}");
            return e.code().0;
        }
        let mut dst: *mut c_void = core::ptr::null_mut();
        let mut dst_size: u32 = 0;
        let got = ctx.GetDecoderBuffer(&*decoder, api_type, &mut dst_size, &mut dst);
        let size = d.DataSize.min(dst_size);
        if got.is_ok() && !dst.is_null() && !mapped.pData.is_null() {
            core::ptr::copy_nonoverlapping(
                (mapped.pData as *const u8).add(d.DataOffset as usize),
                dst as *mut u8,
                size as usize,
            );
            let _ = ctx.ReleaseDecoderBuffer(&*decoder, api_type);
        }
        context.Unmap(&*res, 0);
        if let Err(e) = got {
            log_error!("DDI video_decoder_submit_buffers: no decoder buffer of type {ty}: {e:?}");
            return e.code().0;
        }
        if size < d.DataSize {
            log_error!(
                "DDI video_decoder_submit_buffers: type {ty} truncated {} -> {size}",
                d.DataSize
            );
        }

        api.push(D3D11_VIDEO_DECODER_BUFFER_DESC {
            BufferType: api_type,
            BufferIndex: d.BufferIndex,
            DataOffset: 0,
            DataSize: size,
            FirstMBaddress: d.FirstMBaddress,
            NumMBsInBuffer: d.NumMBsInBuffer,
            Width: d.Width,
            Height: d.Height,
            Stride: d.Stride,
            ReservedBits: d.ReservedBits,
            pIV: d.pIV,
            IVSize: d.IVSize,
            PartialEncryption: bool32(d.PartialEncryption),
            EncryptedBlockInfo: reinterpret(&d.EncryptedBlockInfo),
        });
    }

    hr_of(ctx.SubmitDecoderBuffers(&*decoder, &api))
}

unsafe extern "system" fn video_decoder_get_handle(
    _h: Hdevice,
    h_decode: ddi::D3D11_1DDI_HDECODE,
    handle: *mut ddi::HANDLE,
) -> ddi::HRESULT {
    let Some(decoder) = load_com::<ID3D11VideoDecoder>(h_decode) else { return E_INVALIDARG.0 };
    if handle.is_null() {
        return E_INVALIDARG.0;
    }
    match decoder.GetDriverHandle() {
        Ok(hd) => {
            *handle = hd.0 as ddi::HANDLE;
            S_OK.0
        }
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn calc_size_vdov(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEODECODEROUTPUTVIEW,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_vdov(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEODECODEROUTPUTVIEW,
    h_view: ddi::D3D11_1DDI_HVIDEODECODEROUTPUTVIEW,
    _hrt: ddi::D3D11_1DDI_HRTVIDEODECODEROUTPUTVIEW,
) -> ddi::HRESULT {
    clear_handle(h_view);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else { return E_INVALIDARG.0 };
    let desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
        DecodeProfile: api_guid(&a.DecodeProfile),
        ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VDOV { ArraySlice: a.FirstArraySlice },
        },
    };
    let mut view: Option<ID3D11VideoDecoderOutputView> = None;
    match v.CreateVideoDecoderOutputView(&*res, &desc, Some(&mut view)) {
        Ok(()) => match view {
            Some(view) => {
                store_com(h_view, view);
                S_OK.0
            }
            None => E_FAIL.0,
        },
        Err(e) => {
            log_error!("DDI create_vdov failed: slice={} {e:?}", a.FirstArraySlice);
            e.code().0
        }
    }
}

unsafe extern "system" fn destroy_vdov(_h: Hdevice, h_view: ddi::D3D11_1DDI_HVIDEODECODEROUTPUTVIEW) {
    release_com(h_view);
}

// --- Video processor --------------------------------------------------------------

unsafe extern "system" fn calc_size_video_processor_enum(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSORENUM,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_video_processor_enum(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSORENUM,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    _hrt: ddi::D3D11_1DDI_HRTVIDEOPROCESSORENUM,
) -> ddi::HRESULT {
    clear_handle(h_enum);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let desc: D3D11_VIDEO_PROCESSOR_CONTENT_DESC = reinterpret(&(*arg).Desc);
    match v.CreateVideoProcessorEnumerator(&desc) {
        Ok(e) => {
            store_com(h_enum, e);
            S_OK.0
        }
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn destroy_video_processor_enum(_h: Hdevice, h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM) {
    release_com(h_enum);
}

unsafe extern "system" fn check_video_processor_format(
    _h: Hdevice,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    format: ddi::DXGI_FORMAT,
    flags: *mut ddi::UINT,
) {
    if flags.is_null() {
        return;
    }
    *flags = 0;
    if let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(h_enum) {
        *flags = e.CheckVideoProcessorFormat(DXGI_FORMAT(format as i32)).unwrap_or(0);
    }
}

unsafe extern "system" fn get_video_processor_caps(
    _h: Hdevice,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    caps: *mut ddi::D3D11_1DDI_VIDEO_PROCESSOR_CAPS,
) {
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(h_enum) else { return };
    if caps.is_null() {
        return;
    }
    let mut c = D3D11_VIDEO_PROCESSOR_CAPS::default();
    if e.GetVideoProcessorCaps(&mut c).is_ok() {
        *caps = reinterpret(&c);
    }
}

unsafe extern "system" fn get_video_processor_rate_conversion_caps(
    _h: Hdevice,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    index: ddi::UINT,
    caps: *mut ddi::D3D11_1DDI_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS,
) {
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(h_enum) else { return };
    if caps.is_null() {
        return;
    }
    let mut c = D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default();
    if e.GetVideoProcessorRateConversionCaps(index, &mut c).is_ok() {
        *caps = reinterpret(&c);
    }
}

unsafe extern "system" fn get_video_processor_custom_rate(
    _h: Hdevice,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    index: ddi::UINT,
    rate_index: ddi::UINT,
    rate: *mut ddi::D3D11_1DDI_VIDEO_PROCESSOR_CUSTOM_RATE,
) {
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(h_enum) else { return };
    if rate.is_null() {
        return;
    }
    let mut r = D3D11_VIDEO_PROCESSOR_CUSTOM_RATE::default();
    if e.GetVideoProcessorCustomRate(index, rate_index, &mut r).is_ok() {
        *rate = reinterpret(&r);
    }
}

unsafe extern "system" fn get_video_processor_filter_range(
    _h: Hdevice,
    h_enum: ddi::D3D11_1DDI_HVIDEOPROCESSORENUM,
    filter: ddi::D3D11_1DDI_VIDEO_PROCESSOR_FILTER,
    range: *mut ddi::D3D11_1DDI_VIDEO_PROCESSOR_FILTER_RANGE,
) {
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(h_enum) else { return };
    if range.is_null() {
        return;
    }
    if let Ok(r) = e.GetVideoProcessorFilterRange(D3D11_VIDEO_PROCESSOR_FILTER(filter as i32)) {
        *range = reinterpret(&r);
    }
}

unsafe extern "system" fn calc_size_video_processor(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSOR,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_video_processor(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSOR,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    _hrt: ddi::D3D11_1DDI_HRTVIDEOPROCESSOR,
) -> ddi::HRESULT {
    clear_handle(h_vp);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(a.hVideoProcessorEnum) else {
        return E_INVALIDARG.0;
    };
    match v.CreateVideoProcessor(&*e, a.RateConversionCapsIndex) {
        Ok(vp) => {
            store_com(h_vp, vp);
            S_OK.0
        }
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn destroy_video_processor(_h: Hdevice, h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR) {
    release_com(h_vp);
}

/// The video context and the processor behind a DDI processor handle.
unsafe fn vp(h: Hdevice, h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR) -> Option<(ID3D11VideoContext, ManuallyDrop<ID3D11VideoProcessor>)> {
    Some((video_context(h)?, load_com::<ID3D11VideoProcessor>(h_vp)?))
}

unsafe extern "system" fn vp_set_output_target_rect(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    enable: ddi::BOOL,
    rect: *const ddi::RECT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetOutputTargetRect(&*p, bool32(enable), api_rect(rect));
}

unsafe extern "system" fn vp_set_output_background_color(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    ycbcr: ddi::BOOL,
    color: *const ddi::D3D11_1DDI_VIDEO_COLOR,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    if color.is_null() {
        return;
    }
    let c: D3D11_VIDEO_COLOR = reinterpret(&*color);
    ctx.VideoProcessorSetOutputBackgroundColor(&*p, bool32(ycbcr), &c);
}

unsafe extern "system" fn vp_set_output_color_space(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    cs: *const ddi::D3D11_1DDI_VIDEO_PROCESSOR_COLOR_SPACE,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    if cs.is_null() {
        return;
    }
    let c = api_color_space(cs);
    ctx.VideoProcessorSetOutputColorSpace(&*p, &c);
}

unsafe extern "system" fn vp_set_output_alpha_fill_mode(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    mode: ddi::D3D11_1DDI_VIDEO_PROCESSOR_ALPHA_FILL_MODE,
    stream: ddi::UINT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetOutputAlphaFillMode(&*p, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE(mode as i32), stream);
}

unsafe extern "system" fn vp_set_output_constriction(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    enable: ddi::BOOL,
    size: ddi::SIZE,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetOutputConstriction(&*p, bool32(enable), SIZE { cx: size.cx, cy: size.cy });
}

unsafe extern "system" fn vp_set_output_stereo_mode(h: Hdevice, h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR, enable: ddi::BOOL) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetOutputStereoMode(&*p, bool32(enable));
}

unsafe extern "system" fn vp_output_extension(
    _h: Hdevice,
    _h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    _guid: *const ddi::GUID,
    _size: ddi::UINT,
    _data: *mut c_void,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn vp_set_stream_frame_format(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    format: ddi::D3D11_1DDI_VIDEO_FRAME_FORMAT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamFrameFormat(&*p, stream, D3D11_VIDEO_FRAME_FORMAT(format as i32));
}

unsafe extern "system" fn vp_set_stream_color_space(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    cs: *const ddi::D3D11_1DDI_VIDEO_PROCESSOR_COLOR_SPACE,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    if cs.is_null() {
        return;
    }
    let c = api_color_space(cs);
    ctx.VideoProcessorSetStreamColorSpace(&*p, stream, &c);
}

unsafe extern "system" fn vp_set_stream_output_rate(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    rate: ddi::D3D11_1DDI_VIDEO_PROCESSOR_OUTPUT_RATE,
    repeat: ddi::BOOL,
    custom: *const ddi::DXGI_RATIONAL,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamOutputRate(
        &*p,
        stream,
        D3D11_VIDEO_PROCESSOR_OUTPUT_RATE(rate as i32),
        bool32(repeat),
        api_rational(custom),
    );
}

unsafe extern "system" fn vp_set_stream_source_rect(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    rect: *const ddi::RECT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamSourceRect(&*p, stream, bool32(enable), api_rect(rect));
}

unsafe extern "system" fn vp_set_stream_dest_rect(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    rect: *const ddi::RECT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamDestRect(&*p, stream, bool32(enable), api_rect(rect));
}

unsafe extern "system" fn vp_set_stream_alpha(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    alpha: ddi::FLOAT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamAlpha(&*p, stream, bool32(enable), alpha);
}

unsafe extern "system" fn vp_set_stream_palette(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    count: ddi::UINT,
    entries: *const ddi::UINT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    let entries = (!entries.is_null() && count != 0).then(|| core::slice::from_raw_parts(entries, count as usize));
    ctx.VideoProcessorSetStreamPalette(&*p, stream, entries);
}

unsafe extern "system" fn vp_set_stream_pixel_aspect_ratio(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    src: *const ddi::DXGI_RATIONAL,
    dst: *const ddi::DXGI_RATIONAL,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamPixelAspectRatio(&*p, stream, bool32(enable), api_rational(src), api_rational(dst));
}

unsafe extern "system" fn vp_set_stream_luma_key(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    lower: ddi::FLOAT,
    upper: ddi::FLOAT,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamLumaKey(&*p, stream, bool32(enable), lower, upper);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn vp_set_stream_stereo_format(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    format: ddi::D3D11_1DDI_VIDEO_PROCESSOR_STEREO_FORMAT,
    left_view_frame0: ddi::BOOL,
    base_view_frame0: ddi::BOOL,
    flip_mode: ddi::D3D11_1DDI_VIDEO_PROCESSOR_STEREO_FLIP_MODE,
    mono_offset: core::ffi::c_int,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamStereoFormat(
        &*p,
        stream,
        bool32(enable),
        D3D11_VIDEO_PROCESSOR_STEREO_FORMAT(format as i32),
        bool32(left_view_frame0),
        bool32(base_view_frame0),
        D3D11_VIDEO_PROCESSOR_STEREO_FLIP_MODE(flip_mode as i32),
        mono_offset,
    );
}

unsafe extern "system" fn vp_set_stream_auto_processing_mode(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamAutoProcessingMode(&*p, stream, bool32(enable));
}

unsafe extern "system" fn vp_set_stream_filter(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    filter: ddi::D3D11_1DDI_VIDEO_PROCESSOR_FILTER,
    enable: ddi::BOOL,
    level: core::ffi::c_int,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamFilter(&*p, stream, D3D11_VIDEO_PROCESSOR_FILTER(filter as i32), bool32(enable), level);
}

unsafe extern "system" fn vp_stream_extension(
    _h: Hdevice,
    _h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    _stream: ddi::UINT,
    _guid: *const ddi::GUID,
    _size: ddi::UINT,
    _data: *mut c_void,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn vp_set_stream_rotation(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    stream: ddi::UINT,
    enable: ddi::BOOL,
    rotation: ddi::D3D11_1DDI_VIDEO_PROCESSOR_ROTATION,
) {
    let Some((ctx, p)) = vp(h, h_vp) else { return };
    ctx.VideoProcessorSetStreamRotation(&*p, stream, bool32(enable), D3D11_VIDEO_PROCESSOR_ROTATION(rotation as i32));
}

/// Input views of a DDI handle array, borrowed into `out` (the API takes
/// `Option<ID3D11VideoProcessorInputView>` arrays; ManuallyDrop keeps the
/// handles' own references untouched).
unsafe fn borrow_views(
    handles: *const ddi::D3D11_1DDI_HVIDEOPROCESSORINPUTVIEW,
    count: u32,
    out: &mut Vec<ManuallyDrop<Option<ID3D11VideoProcessorInputView>>>,
) -> *mut Option<ID3D11VideoProcessorInputView> {
    if handles.is_null() || count == 0 {
        return core::ptr::null_mut();
    }
    let start = out.len();
    for i in 0..count as usize {
        let view = load_com::<ID3D11VideoProcessorInputView>(*handles.add(i));
        out.push(ManuallyDrop::new(view.map(ManuallyDrop::into_inner)));
    }
    out.as_mut_ptr().add(start) as *mut Option<ID3D11VideoProcessorInputView>
}

unsafe extern "system" fn video_processor_blt(
    h: Hdevice,
    h_vp: ddi::D3D11_1DDI_HVIDEOPROCESSOR,
    h_output: ddi::D3D11_1DDI_HVIDEOPROCESSOROUTPUTVIEW,
    output_frame: ddi::UINT,
    stream_count: ddi::UINT,
    streams: *const ddi::D3D11_1DDI_VIDEO_PROCESSOR_STREAM,
) -> ddi::HRESULT {
    let Some((ctx, p)) = vp(h, h_vp) else { return E_FAIL.0 };
    let Some(output) = load_com::<ID3D11VideoProcessorOutputView>(h_output) else { return E_INVALIDARG.0 };
    if stream_count != 0 && streams.is_null() {
        return E_INVALIDARG.0;
    }
    let streams = core::slice::from_raw_parts(streams, stream_count as usize);

    // Reserve up front: borrow_views hands out pointers into this vector.
    let total: usize = streams
        .iter()
        .map(|s| 2 * (s.PastFrames + s.FutureFrames) as usize)
        .sum();
    let mut views: Vec<ManuallyDrop<Option<ID3D11VideoProcessorInputView>>> = Vec::with_capacity(total);
    let mut api: Vec<D3D11_VIDEO_PROCESSOR_STREAM> = Vec::with_capacity(streams.len());

    for s in streams {
        let input = load_com::<ID3D11VideoProcessorInputView>(s.hInputSurface).map(ManuallyDrop::into_inner);
        let input_right = load_com::<ID3D11VideoProcessorInputView>(s.hInputSurfaceRight).map(ManuallyDrop::into_inner);
        api.push(D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: bool32(s.Enable),
            OutputIndex: s.OutputIndex,
            InputFrameOrField: s.InputFrameOrField,
            PastFrames: s.PastFrames,
            FutureFrames: s.FutureFrames,
            ppPastSurfaces: borrow_views(s.pPastSurfaces, s.PastFrames, &mut views),
            pInputSurface: ManuallyDrop::new(input),
            ppFutureSurfaces: borrow_views(s.pFutureSurfaces, s.FutureFrames, &mut views),
            ppPastSurfacesRight: borrow_views(s.pPastSurfacesRight, s.PastFrames, &mut views),
            pInputSurfaceRight: ManuallyDrop::new(input_right),
            ppFutureSurfacesRight: borrow_views(s.pFutureSurfacesRight, s.FutureFrames, &mut views),
        });
    }

    // `api` and `views` hold borrowed references only (ManuallyDrop), so
    // dropping them releases nothing.
    hr_of(ctx.VideoProcessorBlt(&*p, &*output, output_frame, &api))
}

unsafe extern "system" fn calc_size_vpiv(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSORINPUTVIEW,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_vpiv(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSORINPUTVIEW,
    h_view: ddi::D3D11_1DDI_HVIDEOPROCESSORINPUTVIEW,
    _hrt: ddi::D3D11_1DDI_HRTVIDEOPROCESSORINPUTVIEW,
) -> ddi::HRESULT {
    clear_handle(h_view);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else { return E_INVALIDARG.0 };
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(a.hDrvVideoProcessorEnum) else {
        return E_INVALIDARG.0;
    };
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: a.FourCC,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV { MipSlice: a.MipSlice, ArraySlice: a.FirstArraySlice },
        },
    };
    let mut view: Option<ID3D11VideoProcessorInputView> = None;
    match v.CreateVideoProcessorInputView(&*res, &*e, &desc, Some(&mut view)) {
        Ok(()) => match view {
            Some(view) => {
                store_com(h_view, view);
                S_OK.0
            }
            None => E_FAIL.0,
        },
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn destroy_vpiv(_h: Hdevice, h_view: ddi::D3D11_1DDI_HVIDEOPROCESSORINPUTVIEW) {
    release_com(h_view);
}

unsafe extern "system" fn calc_size_vpov(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSOROUTPUTVIEW,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_vpov(
    h: Hdevice,
    arg: *const ddi::D3D11_1DDIARG_CREATEVIDEOPROCESSOROUTPUTVIEW,
    h_view: ddi::D3D11_1DDI_HVIDEOPROCESSOROUTPUTVIEW,
    _hrt: ddi::D3D11_1DDI_HRTVIDEOPROCESSOROUTPUTVIEW,
) -> ddi::HRESULT {
    clear_handle(h_view);
    let Some(v) = video_device(h) else { return E_FAIL.0 };
    if arg.is_null() {
        return E_INVALIDARG.0;
    }
    let a = &*arg;
    let Some(res) = load_resource(a.hDrvResource) else { return E_INVALIDARG.0 };
    let Some(e) = load_com::<ID3D11VideoProcessorEnumerator>(a.hDrvVideoProcessorEnum) else {
        return E_INVALIDARG.0;
    };
    // A one-slice view of slice N > 0 is an array view too (needs_array_form).
    let desc = if needs_array_form(a.ArraySize, a.FirstArraySlice) {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                    MipSlice: a.MipSlice,
                    FirstArraySlice: a.FirstArraySlice,
                    ArraySize: a.ArraySize,
                },
            },
        }
    } else {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: a.MipSlice },
            },
        }
    };
    let mut view: Option<ID3D11VideoProcessorOutputView> = None;
    match v.CreateVideoProcessorOutputView(&*res, &*e, &desc, Some(&mut view)) {
        Ok(()) => match view {
            Some(view) => {
                store_com(h_view, view);
                S_OK.0
            }
            None => E_FAIL.0,
        },
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn destroy_vpov(_h: Hdevice, h_view: ddi::D3D11_1DDI_HVIDEOPROCESSOROUTPUTVIEW) {
    release_com(h_view);
}

// DXVK orders reads after the decoder's writes itself.
unsafe extern "system" fn vpiv_read_after_write_hazard(
    _h: Hdevice,
    _view: ddi::D3D11_1DDI_HVIDEOPROCESSORINPUTVIEW,
    _res: ddi::D3D10DDI_HRESOURCE,
) {
}

// --- Content protection: not implemented ------------------------------------------

unsafe extern "system" fn get_content_protection_caps(
    _h: Hdevice,
    _crypto: *const ddi::GUID,
    _profile: *const ddi::GUID,
    _caps: *mut ddi::D3D11_1DDI_VIDEO_CONTENT_PROTECTION_CAPS,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn get_crypto_key_exchange_type(
    _h: Hdevice,
    _crypto: *const ddi::GUID,
    _profile: *const ddi::GUID,
    _index: ddi::UINT,
    _kx: *mut ddi::GUID,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn calc_size_crypto_session(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATECRYPTOSESSION,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_crypto_session(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATECRYPTOSESSION,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _hrt: ddi::D3D11_1DDI_HRTCRYPTOSESSION,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn destroy_crypto_session(_h: Hdevice, _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION) {}

unsafe extern "system" fn get_certificate_size(
    _h: Hdevice,
    _info: *const ddi::D3D11_1DDI_CERTIFICATE_INFO,
    size: *mut ddi::UINT,
) {
    if !size.is_null() {
        *size = 0;
    }
}

unsafe extern "system" fn get_certificate(
    _h: Hdevice,
    _info: *const ddi::D3D11_1DDI_CERTIFICATE_INFO,
    _size: ddi::UINT,
    _cert: *mut ddi::BYTE,
) {
}

unsafe extern "system" fn negotiate_crypto_session_key_exchange(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _size: ddi::UINT,
    _data: *mut ddi::BYTE,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn encryption_blt(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _src: ddi::D3D10DDI_HRESOURCE,
    _dst: ddi::D3D10DDI_HRESOURCE,
    _iv_size: ddi::UINT,
    _iv: *const c_void,
) {
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn decryption_blt(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _src: ddi::D3D10DDI_HRESOURCE,
    _dst: ddi::D3D10DDI_HRESOURCE,
    _info: *const ddi::D3D11_1DDI_ENCRYPTED_BLOCK_INFO,
    _key_size: ddi::UINT,
    _key: *const c_void,
    _iv_size: ddi::UINT,
    _iv: *const c_void,
) {
}

unsafe extern "system" fn start_session_key_refresh(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _size: ddi::UINT,
    _random: *mut c_void,
) {
}

unsafe extern "system" fn finish_session_key_refresh(_h: Hdevice, _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION) {}

unsafe extern "system" fn get_encryption_blt_key(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _size: ddi::UINT,
    _key: *mut c_void,
) {
}

unsafe extern "system" fn calc_size_auth_channel(
    _h: Hdevice,
    _a: *const ddi::D3D11_1DDIARG_CREATEAUTHENTICATEDCHANNEL,
) -> ddi::SIZE_T {
    8
}

unsafe extern "system" fn create_auth_channel(
    _h: Hdevice,
    _a: *mut ddi::D3D11_1DDIARG_CREATEAUTHENTICATEDCHANNEL,
    _hac: ddi::D3D11_1DDI_HAUTHCHANNEL,
    _hrt: ddi::D3D11_1DDI_HRTAUTHCHANNEL,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn destroy_auth_channel(_h: Hdevice, _hac: ddi::D3D11_1DDI_HAUTHCHANNEL) {}

unsafe extern "system" fn negotiate_auth_channel_key_exchange(
    _h: Hdevice,
    _hac: ddi::D3D11_1DDI_HAUTHCHANNEL,
    _size: ddi::UINT,
    _data: *mut c_void,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn query_auth_channel(
    _h: Hdevice,
    _hac: ddi::D3D11_1DDI_HAUTHCHANNEL,
    _in_size: ddi::UINT,
    _input: *const c_void,
    _out_size: ddi::UINT,
    _output: *mut c_void,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn configure_auth_channel(
    _h: Hdevice,
    _hac: ddi::D3D11_1DDI_HAUTHCHANNEL,
    _in_size: ddi::UINT,
    _input: *const c_void,
    _output: *mut ddi::D3D11_1DDI_AUTHENTICATED_CONFIGURE_OUTPUT,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn crypto_session_get_handle(
    _h: Hdevice,
    _hcs: ddi::D3D11_1DDI_HCRYPTOSESSION,
    _handle: *mut ddi::HANDLE,
) -> ddi::HRESULT {
    E_NOTIMPL.0
}

unsafe extern "system" fn get_capture_handle(_h: Hdevice, data: *mut ddi::D3D11_1DDI_GETCAPTUREHANDLEDATA) {
    if !data.is_null() {
        (*data).hAllocation = 0;
        (*data).DataOffset = 0;
        (*data).DataSize = 0;
    }
}
