//! The D3D11 caps surface: everything `GetCaps` answers for this adapter.
//!
//! Moved verbatim out of `lib.rs` by T8/R1106.

use crate::ddi;
use crate::hr::{Hresult, S_OK};
use crate::log_error;
use crate::{device_funcs, feature_level_mode};

/// How the FL11 multisample caps are answered.
///
/// A named field rather than a bool because the two answers are policies, not
/// a capability: `Full` reports real quality levels, `SingleSampleOnly` reports
/// 1 for sample_count==1 and 0 for everything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MsaaPolicy {
    Full,
    SingleSampleOnly,
}

/// How `check_format_support` treats the multisample bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FormatPolicy {
    Unmasked,
    StripMultisampleBits,
}

/// The feature-level profile, as ONE value.
///
/// ⚠ THIS EXISTS BECAUSE THE CONTRACT IS DISTRIBUTED AND ASYMMETRIC ON PURPOSE.
/// Before it, `feature_level_mode()` gated three caps here with `>= 1` and five
/// sites in `forward.rs`'s `check_format_support` / `helios_multisample_quality_
/// levels` with `== 1`, and the difference between the two operators WAS the
/// knob=2 diagnostic ("pipeline claims 11_0 but keeps the FL10 MSAA/format
/// caps"). Nothing said so: it was discoverable only by grepping two files and
/// noticing that the same knob is compared two different ways deliberately.
///
/// The runtime validates the three pipeline/shader/options caps and the
/// MSAA/format answers as ONE coherent feature-level contract in
/// `CDevice::LLOCompleteLayerConstruction`; a partial edit is rejected with
/// `DXGI_ERROR_UNSUPPORTED`. Selecting one of three named constants makes
/// "these caps move together" a data fact, and adding a field forces every
/// consumer to be updated.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FeatureProfile {
    /// `D3D11DDICAPS_3DPIPELINESUPPORT`: a BITMASK of supported levels, not the
    /// bare `D3D11DDI_3DPIPELINELEVEL` enum.
    pub pipeline_mask: u32,
    /// `D3D11DDICAPS_SHADER`.
    pub shader_caps: u32,
    /// `D3DWDDM1_3DDICAPS_D3D11_OPTIONS1` tiled-resource tier flags.
    pub options1: u32,
    /// `D3DWDDM2_0DDICAPS_D3D11_OPTIONS3` (WDDM 2.0+ DDI only).
    pub vp_rt_index_any_stage: bool,
    pub msaa: MsaaPolicy,
    pub format: FormatPolicy,
}

const LVL_10_0: u32 = 1 << 0;
const LVL_10_1: u32 = 1 << 1;
const LVL_11_0: u32 = 1 << 2;
const LVL_11_1: u32 = 1 << 3;
const LVL_12_0: u32 = 1 << 7;
pub(crate) const SHADER_DOUBLES: u32 = 0x1;
const SHADER_COMPUTE: u32 = 0x2;
pub(crate) const SHADER_SPECIFIED_STENCIL_REF: u32 = 0x10;
pub(crate) const SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS: u32 = 0x20;
pub(crate) const SHADER_ROVS: u32 = 0x40;
const TILED_RESOURCES_TIER_2_SUPPORTED: u32 = 0x2;
const FL11_PIPELINE_MASK: u32 = LVL_10_0 | LVL_10_1 | LVL_11_0 | LVL_11_1 | LVL_12_0;
/// The optional shader caps. `d3d11.dll` checks every shader's `SFI0` flags
/// against these at `Create*Shader` and fails the call with `E_INVALIDARG`
/// before the driver sees it, so a cap the engine backs but this word leaves
/// out is a shader that silently never exists (CS2's bloom and OIT upsample
/// compute shaders need TYPED_UAV_LOAD_ADDITIONAL_FORMATS).
///
/// Advertised: what DXVK backs on every engine this UMD drives (NVK on RM and
/// Venus on the host's NVIDIA driver): doubles (`shaderFloat64` +
/// `shaderInt64`) and typed UAV loads of the additional formats
/// (`shaderStorageImageReadWithoutFormat`). Withheld: SPECIFIED_STENCIL_REF
/// (NVK has no `VK_EXT_shader_stencil_export`) and ROVS (NVK has no
/// `VK_EXT_fragment_shader_interlock`). [`audit_engine_caps`] compares this
/// word with the engine of each created device and logs any difference.
///
/// ⚠ Bits 0x10/0x20/0x40 are read by the runtime only from a WDDM 2.0+ DDI
/// interface; under WDDM 1.3 it treats them as absent (`ddi_level.rs`).
const FL11_SHADER_CAPS: u32 = SHADER_DOUBLES | SHADER_COMPUTE | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS;

/// `FeatureLevel11 = 0`: the proven FL10_0 baseline opt-out.
const FL10_0: FeatureProfile = FeatureProfile {
    pipeline_mask: LVL_10_0,
    shader_caps: 0,
    options1: 0,
    vp_rt_index_any_stage: false,
    msaa: MsaaPolicy::SingleSampleOnly,
    format: FormatPolicy::StripMultisampleBits,
};

/// `FeatureLevel11` absent or `= 1`: the production profile.
const FL11_0: FeatureProfile = FeatureProfile {
    pipeline_mask: FL11_PIPELINE_MASK,
    shader_caps: FL11_SHADER_CAPS,
    options1: TILED_RESOURCES_TIER_2_SUPPORTED,
    vp_rt_index_any_stage: true,
    msaa: MsaaPolicy::Full,
    format: FormatPolicy::Unmasked,
};

/// `FeatureLevel11 = 2`: the DIAGNOSTIC profile. FL11 pipeline / shader /
/// options1 with the FL10 MSAA and format policy, which isolates pipeline-level
/// validation from the later FL11 caps gates.
///
/// ⚠ THIS THIRD PROFILE IS THE ONE A "TIDY-UP" DELETES. Two profiles would fold
/// mode 2 into either FL10_0 or FL11_0 and silently retire the knob. The
/// assertion below is what stops that: it fails to compile if this profile
/// stops being distinct from both others in exactly the documented way.
const FL11_PIPELINE_ONLY: FeatureProfile = FeatureProfile {
    pipeline_mask: FL11_PIPELINE_MASK,
    shader_caps: FL11_SHADER_CAPS,
    options1: TILED_RESOURCES_TIER_2_SUPPORTED,
    vp_rt_index_any_stage: true,
    msaa: MsaaPolicy::SingleSampleOnly,
    format: FormatPolicy::StripMultisampleBits,
};

// Mode 2 is FL11's three caps with FL10's two policies. If a future edit makes
// it equal to either neighbour, the knob has been deleted -- say so at compile
// time rather than in a bug report six months later.
const _: () = {
    assert!(FL11_PIPELINE_ONLY.pipeline_mask == FL11_0.pipeline_mask);
    assert!(FL11_PIPELINE_ONLY.shader_caps == FL11_0.shader_caps);
    assert!(FL11_PIPELINE_ONLY.options1 == FL11_0.options1);
    assert!(FL11_PIPELINE_ONLY.pipeline_mask != FL10_0.pipeline_mask);
    // ... and FL10's MSAA/format policy, which is what makes it a diagnostic.
    assert!(matches!(
        FL11_PIPELINE_ONLY.msaa,
        MsaaPolicy::SingleSampleOnly
    ));
    assert!(matches!(
        FL11_PIPELINE_ONLY.format,
        FormatPolicy::StripMultisampleBits
    ));
    // The old bug this replaces: writing the bare enum value 2 as the pipeline
    // mask meant bit1 only, i.e. "10_1 without 10_0" -- an invalid mask.
    assert!(FL11_0.pipeline_mask & LVL_10_0 != 0);
};

/// The profile this process advertises, selected ONCE from the knob.
///
/// All four knob settings are enumerated. Deliberately NOT a range: `>= 2`
/// would read as "mode 2 and up" and invite collapsing it into FL11_0, which
/// deletes the diagnostic. Any other nonzero value lands in the same arm the
/// old `>= 1` / `!= 1` PAIR gave it -- FL11's three caps with FL10's two
/// policies -- so this is behaviour-preserving for every input.
pub(crate) fn feature_profile() -> &'static FeatureProfile {
    match feature_level_mode() {
        // `feature_level_mode()` returns 1 when the value is ABSENT, so the
        // absent case and an explicit 1 are the same arm by construction.
        0 => &FL10_0,
        1 => &FL11_0,
        2 => &FL11_PIPELINE_ONLY,
        _ => &FL11_PIPELINE_ONLY,
    }
}

pub(crate) unsafe extern "system" fn get_caps(
    _h_adapter: ddi::D3D10DDI_HADAPTER,
    args: *const ddi::D3D10_2DDIARG_GETCAPS,
) -> Hresult {
    // Aliases onto the generated caps-type enumerators rather than eight hand-
    // written literals. The numerics are identical (128, 129, 130, 131, 132,
    // 134, 136, 137) -- what changes is that they now come from the WDK header
    // and carry `D3D10_2DDICAPS_TYPE`, which is what `args.Type` actually is.
    // The short local names are kept so the match arms below read unchanged.
    use ddi::{
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_3DPIPELINESUPPORT as D3D11DDICAPS_3DPIPELINESUPPORT,
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_SHADER as D3D11DDICAPS_SHADER,
        D3D10_2DDICAPS_TYPE_D3D11DDICAPS_THREADING as D3D11DDICAPS_THREADING,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_ARCHITECTURE_INFO as D3D11_1DDICAPS_ARCHITECTURE_INFO,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_D3D11_OPTIONS as D3D11_1DDICAPS_D3D11_OPTIONS,
        D3D10_2DDICAPS_TYPE_D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT as D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT,
        D3D10_2DDICAPS_TYPE_D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 as D3DWDDM1_3DDICAPS_D3D11_OPTIONS1,
        D3D10_2DDICAPS_TYPE_D3DWDDM1_3DDICAPS_MARKER as D3DWDDM1_3DDICAPS_MARKER,
        D3D10_2DDICAPS_TYPE_D3DWDDM2_0DDICAPS_D3D11_OPTIONS3 as D3DWDDM2_0DDICAPS_D3D11_OPTIONS3,
        D3D10_2DDICAPS_TYPE_D3DWDDM2_0DDICAPS_GPUVA_CAPS as D3DWDDM2_0DDICAPS_GPUVA_CAPS,
    };
    // The old literals, pinned so the alias swap is provably value-preserving.
    const _: () = assert!(D3D11DDICAPS_THREADING == 128);
    const _: () = assert!(D3D11DDICAPS_SHADER == 129);
    const _: () = assert!(D3D11DDICAPS_3DPIPELINESUPPORT == 130);
    const _: () = assert!(D3D11_1DDICAPS_D3D11_OPTIONS == 131);
    const _: () = assert!(D3D11_1DDICAPS_ARCHITECTURE_INFO == 132);
    const _: () = assert!(D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT == 134);
    const _: () = assert!(D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 == 136);
    const _: () = assert!(D3DWDDM1_3DDICAPS_MARKER == 137);
    const _: () = assert!(D3DWDDM2_0DDICAPS_D3D11_OPTIONS3 == 152);
    const _: () = assert!(D3DWDDM2_0DDICAPS_GPUVA_CAPS == 153);

    if !args.is_null() {
        let args = unsafe { &*args };
        log_error!(
            "GetCaps type=0x{:08x} dataSize={} pInfo={:p}",
            args.Type,
            args.DataSize,
            args.pInfo,
        );
        if !args.pData.is_null() && args.DataSize != 0 {
            // Default: zero the output.
            unsafe { core::ptr::write_bytes(args.pData as *mut u8, 0, args.DataSize as usize) };
            match args.Type {
                // D3D11DDI_THREADING_CAPS::Caps. Zero means no free-threaded
                // mode and no command-list build support; the runtime must
                // serialize/emulate. Knob-dependent since Phase B: the value
                // and the state model it licenses live on one function whose
                // POSSIBLE set is compile-time pinned to exclude the
                // COMMANDLISTS bits while the R812 slots are stubs. R811.
                D3D11DDICAPS_THREADING if args.DataSize >= 4 => {
                    let caps = device_funcs::threading_caps();
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: THREADING caps = {caps}");
                }
                // D3D11DDI_SHADER_CAPS::Caps. FL11 mandates compute shaders;
                // the runtime rejects the adapter with "Driver doesn't support
                // compute on FL11" (0x887a0020) if this doesn't advertise
                // compute. Bit 0x2 =
                // D3D11DDICAPS_SHADER_COMPUTE_PLUS_RAW_AND_STRUCTURED_BUFFERS_IN_SHADER_4_X
                // is the driver's compute-capability signal; dxvk/venus back
                // full CS 5.0. FL12_0 additionally requires the D3D11.3 typed
                // UAV-load additional-formats bit. The optional bits are the
                // ones FL11_SHADER_CAPS documents. FL10 profile stays 0 (no
                // optional shader caps).
                D3D11DDICAPS_SHADER if args.DataSize >= 4 => {
                    let caps = feature_profile().shader_caps;
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: SHADER caps = 0x{caps:x}");
                }
                // D3D11DDI_3DPIPELINESUPPORT_CAPS::Caps is a BITMASK, NOT the
                // bare D3D11DDI_3DPIPELINELEVEL enum: each supported level sets
                // one bit, D3D11DDI_ENCODE_3DPIPELINESUPPORT_CAP(Level)=(1<<Level),
                // OR'd contiguously from 10_0 up (WDK 10.0.26100 d3d10umddi.h).
                // Enum: 10_0=0, 10_1=1, 11_0=2, 11_1=3, 12_0=7, 12_1=8.
                // FL12_0 requires tiled-resource tier 2+; GetCaps(OPTIONS1)
                // below advertises tier 2 and the WDDM1.3 function table
                // forwards the tile DDIs to DXVK's sparse-resource path. Do
                // not advertise FL12_1 until ROV support is plumbed.
                // Writing the bare enum value was THE FL11 bug: value 2 =
                // bit1 only = "10_1 without 10_0" = an invalid mask, which
                // d3d11.dll rejects with "Driver returned invalid pipeline
                // caps" (0x887a0020) → "Failed to find DDI to drive requested
                // feature levels" (0x887a0004) for EVERY level. (The old FL10
                // path wrote 1 == (1<<0) == the 10_0 bit, so it worked by
                // coincidence and produced an FL10_0 device.)
                D3D11DDICAPS_3DPIPELINESUPPORT if args.DataSize >= 4 => {
                    let caps = feature_profile().pipeline_mask;
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: 3DPIPELINESUPPORT bitmask=0x{caps:x}");
                }
                // D3D11.1 caps. FL11_1 requires output-merger logic ops; the
                // 11.1 blend-state forwarder maps LogicOpEnable/LogicOp to
                // ID3D11Device1::CreateBlendState1. Keep debug binary support
                // and shader min-precision support disabled.
                D3D11_1DDICAPS_D3D11_OPTIONS if args.DataSize >= 8 => {
                    unsafe { *(args.pData as *mut u32) = 1 };
                    log_error!("  GetCaps: D3D11_OPTIONS OutputMergerLogicOp=TRUE");
                }
                D3D11_1DDICAPS_ARCHITECTURE_INFO if args.DataSize >= 4 => {
                    log_error!("  GetCaps: ARCHITECTURE_INFO = zero");
                }
                D3D11_1DDICAPS_SHADER_MIN_PRECISION_SUPPORT if args.DataSize >= 8 => {
                    log_error!("  GetCaps: SHADER_MIN_PRECISION_SUPPORT = zero");
                }
                D3DWDDM1_3DDICAPS_D3D11_OPTIONS1 if args.DataSize >= 4 => {
                    let caps = feature_profile().options1;
                    unsafe { *(args.pData as *mut u32) = caps };
                    log_error!("  GetCaps: D3D11_OPTIONS1 TiledResourcesSupportFlags=0x{caps:x}");
                }
                // Queried only by a WDDM 2.x D3D11 device (`ddi_level.rs`).
                // Non-zero, and 40 bits for the same reason as umd12's
                // GPUVA_CAPS: kmd_render declares a 40-bit GPU VA and vkd3d
                // hardcodes 40. OPTIONS3 is answered above; the other WDDM 2.x caps
                // (OPTIONS2 = conservative tier, MEMORY_ARCHITECTURE, SHADERCACHE,
                // TEXTURE_LAYOUT, SWIZZLE_PATTERN) keep the zeroed answer: not
                // supported/discrete.
                D3DWDDM2_0DDICAPS_GPUVA_CAPS if args.DataSize >= 4 => {
                    const HELIOS_GPU_VA_BITS: u32 = 40;
                    unsafe { *(args.pData as *mut u32) = HELIOS_GPU_VA_BITS };
                    log_error!("  GetCaps: GPUVA MaxGPUVirtualAddressBitsPerResource={HELIOS_GPU_VA_BITS}");
                }
                // D3DWDDM2_0DDI_D3D11_OPTIONS3_DATA: viewport / render-target
                // array index written by any stage feeding the rasterizer (VS,
                // DS), not only the GS. Gates SFI0 flag 0x2000 at shader
                // creation. DXVK backs it with Vulkan 1.2 shaderOutputLayer +
                // shaderOutputViewportIndex (NVK Maxwell B+, NVIDIA).
                D3DWDDM2_0DDICAPS_D3D11_OPTIONS3 if args.DataSize >= 4 => {
                    let v = feature_profile().vp_rt_index_any_stage as u32;
                    unsafe { *(args.pData as *mut u32) = v };
                    log_error!("  GetCaps: D3D11_OPTIONS3 VPAndRTArrayIndexFromAnyShaderFeedingRasterizer={v}");
                }
                D3DWDDM1_3DDICAPS_MARKER if args.DataSize >= 4 => {
                    const D3DWDDM1_3DDI_MARKER_TYPE_NONE: u32 = 0;
                    unsafe { *(args.pData as *mut u32) = D3DWDDM1_3DDI_MARKER_TYPE_NONE };
                    log_error!("  GetCaps: MARKER type = NONE");
                }
                other => {
                    log_error!(
                        "  GetCaps: unsupported cap type {} (zeroed {} bytes)",
                        other,
                        args.DataSize
                    );
                }
            }
        }
    } else {
        log_error!("GetCaps: null args");
    }
    S_OK
}

/// What the engine behind one created device backs, in the caps' terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EngineShaderCaps {
    pub shader_caps: u32,
    pub vp_rt_index_any_stage: bool,
}

/// One line per difference between what GetCaps advertised and what the
/// engine backs. Pure, for tests.
///
/// * engine backs, caps hide it: the runtime refuses every shader that needs
///   it before the driver sees it (silent: no DDI call to log).
/// * caps claim, engine lacks it: the runtime admits shaders the engine then
///   fails to compile or mis-renders.
pub(crate) fn engine_caps_mismatches(advertised: EngineShaderCaps, engine: EngineShaderCaps, wddm2: bool) -> Vec<String> {
    const BITS: &[(u32, &str, bool)] = &[
        (SHADER_DOUBLES, "DOUBLES", false),
        (SHADER_SPECIFIED_STENCIL_REF, "SPECIFIED_STENCIL_REF", true),
        (SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS, "TYPED_UAV_LOAD_ADDITIONAL_FORMATS", true),
        (SHADER_ROVS, "ROVS", true),
    ];
    let mut out = Vec::new();
    for &(bit, name, needs_wddm2) in BITS {
        let adv = advertised.shader_caps & bit != 0 && (wddm2 || !needs_wddm2);
        let eng = engine.shader_caps & bit != 0;
        if eng && !adv {
            let why = if advertised.shader_caps & bit != 0 { " (WDDM 1.3 DDI: the runtime ignores the bit)" } else { "" };
            out.push(format!("engine backs {name}, runtime sees it unsupported{why}: shaders requiring it are refused before the driver"));
        } else if adv && !eng {
            out.push(format!("caps claim {name}, engine lacks it: shaders requiring it reach an engine that cannot run them"));
        }
    }
    let adv = advertised.vp_rt_index_any_stage && wddm2;
    if engine.vp_rt_index_any_stage && !adv {
        out.push("engine backs VPAndRTArrayIndexFromAnyShaderFeedingRasterizer, runtime sees it unsupported".into());
    } else if adv && !engine.vp_rt_index_any_stage {
        out.push("caps claim VPAndRTArrayIndexFromAnyShaderFeedingRasterizer, engine lacks it".into());
    }
    out
}

/// Compare the adapter caps with the engine of a freshly created device and
/// log every difference (`engine_caps_mismatches`), each also as a
/// `d3d11-caps` line in the shader-failure ledger: a hidden cap predicts
/// runtime refusals no DDI call will ever show. Once per device; not on a hot
/// path.
pub(crate) fn audit_engine_caps(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device, wddm2: bool) -> usize {
    use windows::Win32::Graphics::Direct3D11::*;
    fn get<T: Default>(device: &ID3D11Device, f: D3D11_FEATURE) -> Option<T> {
        let mut data = T::default();
        unsafe {
            device
                .CheckFeatureSupport(f, &mut data as *mut T as *mut core::ffi::c_void, core::mem::size_of::<T>() as u32)
                .ok()?;
        }
        Some(data)
    }
    let mut engine = EngineShaderCaps::default();
    if let Some(d) = get::<D3D11_FEATURE_DATA_DOUBLES>(device, D3D11_FEATURE_DOUBLES) {
        if d.DoublePrecisionFloatShaderOps.as_bool() {
            engine.shader_caps |= SHADER_DOUBLES;
        }
    }
    if let Some(o) = get::<D3D11_FEATURE_DATA_D3D11_OPTIONS2>(device, D3D11_FEATURE_D3D11_OPTIONS2) {
        if o.PSSpecifiedStencilRefSupported.as_bool() {
            engine.shader_caps |= SHADER_SPECIFIED_STENCIL_REF;
        }
        if o.TypedUAVLoadAdditionalFormats.as_bool() {
            engine.shader_caps |= SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS;
        }
        if o.ROVsSupported.as_bool() {
            engine.shader_caps |= SHADER_ROVS;
        }
    }
    if let Some(o) = get::<D3D11_FEATURE_DATA_D3D11_OPTIONS3>(device, D3D11_FEATURE_D3D11_OPTIONS3) {
        engine.vp_rt_index_any_stage = o.VPAndRTArrayIndexFromAnyShaderFeedingRasterizer.as_bool();
    }
    let p = feature_profile();
    let advertised = EngineShaderCaps { shader_caps: p.shader_caps, vp_rt_index_any_stage: p.vp_rt_index_any_stage };
    let lines = engine_caps_mismatches(advertised, engine, wddm2);
    log_error!(
        "  CreateDevice: shader caps advertised=0x{:x} engine=0x{:x} vp_rt_any advertised={} engine={} wddm2={wddm2} mismatches={}",
        advertised.shader_caps,
        engine.shader_caps,
        advertised.vp_rt_index_any_stage,
        engine.vp_rt_index_any_stage,
        lines.len()
    );
    for l in &lines {
        log_error!("  CreateDevice: caps mismatch: {l}");
        helios_umd_common::shader_diag::ledger_note(&format!("d3d11-caps {l}"));
    }
    lines.len()
}

#[cfg(test)]
mod engine_caps_tests {
    use super::*;

    fn caps(shader_caps: u32, vp: bool) -> EngineShaderCaps {
        EngineShaderCaps { shader_caps, vp_rt_index_any_stage: vp }
    }

    #[test]
    fn production_profile_matches_an_nvk_engine_on_wddm2() {
        // NVK: doubles + typed loads + VP/RT index; no stencil export, no ROV.
        let nvk = caps(SHADER_DOUBLES | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS, true);
        let adv = caps(FL11_0.shader_caps & !SHADER_COMPUTE, FL11_0.vp_rt_index_any_stage);
        assert!(engine_caps_mismatches(adv, nvk, true).is_empty());
    }

    #[test]
    fn wddm13_hides_typed_uav_loads() {
        let nvk = caps(SHADER_DOUBLES | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS, true);
        let adv = caps(FL11_0.shader_caps, true);
        let m = engine_caps_mismatches(adv, nvk, false);
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m[0].contains("TYPED_UAV_LOAD_ADDITIONAL_FORMATS") && m[0].contains("WDDM 1.3"));
        assert!(m[1].contains("VPAndRTArrayIndex"));
    }

    #[test]
    fn venus_extra_features_are_named() {
        let venus = caps(SHADER_DOUBLES | SHADER_TYPED_UAV_LOAD_ADDITIONAL_FORMATS | SHADER_ROVS | SHADER_SPECIFIED_STENCIL_REF, true);
        let adv = caps(FL11_0.shader_caps, true);
        let m = engine_caps_mismatches(adv, venus, true);
        assert_eq!(m.len(), 2, "{m:?}");
        assert!(m.iter().any(|l| l.contains("ROVS")) && m.iter().any(|l| l.contains("STENCIL_REF")));
    }

    #[test]
    fn overclaim_is_named() {
        let weak = caps(0, false);
        let m = engine_caps_mismatches(caps(SHADER_DOUBLES, true), weak, true);
        assert_eq!(m.len(), 2);
        assert!(m[0].starts_with("caps claim DOUBLES"));
    }
}
