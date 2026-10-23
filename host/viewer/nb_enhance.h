/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * nb_enhance.h -- the arithmetic around the upscalers in the viewer's GPU pass
 * (nb_gpuscale.c, docs/VIEWER-ENHANCE.md): which pass runs for a given
 * scale, how the menu's sharpness maps onto each one, and NVIDIA Image
 * Scaling's constants.
 *
 * The FSR 1 and CAS constants are computed in the shaders themselves, by the
 * vendored headers' own setup functions (FsrEasuCon, FsrRcasCon, CasSetup
 * "work on CPU or GPU"), so there is no second copy of them here.  NIS's
 * setup is C++ (third_party/nis/NIS_Config.h NVScalerUpdateConfig): ported
 * below for SDR input, unchanged otherwise.
 *
 * Pure arithmetic, no GL: test/test_enhance.c drives it.
 */
#ifndef NB_ENHANCE_H
#define NB_ENHANCE_H

#include <stdbool.h>
#include <stdint.h>

#include "nb_view.h"

/*
 * The pass that runs for an `up` choice when a iw x ih picture is drawn at
 * ow x oh.  The spatial upscalers only make a picture larger: drawn smaller
 * (a window smaller than the guest's mode) FSR and NIS give way to the plain
 * pass; CAS sharpens at the guest's resolution and still runs.  NIS is
 * specified for 1x..2x per axis: beyond that FSR (1x..4x area by design,
 * usable above) takes over.  *why says so when the answer is not `up`.
 */
static inline int nb_enh_pick(int up, uint32_t iw, uint32_t ih, uint32_t ow,
                              uint32_t oh, const char **why)
{
    if (why) {
        *why = NULL;
    }
    if (up <= NB_UPSCALE_OFF || up >= NB_UPSCALE_COUNT || !iw || !ih ||
        !ow || !oh) {
        return NB_UPSCALE_OFF;
    }
    if (up == NB_UPSCALE_CAS) {
        return up;
    }
    if (ow < iw || oh < ih) {
        if (why) {
            *why = "the picture is drawn smaller than the guest renders it";
        }
        return NB_UPSCALE_OFF;
    }
    if (up == NB_UPSCALE_NIS && (ow > 2 * iw || oh > 2 * ih)) {
        if (why) {
            *why = "NIS scales up to 2x per axis; FSR above that";
        }
        return NB_UPSCALE_FSR;
    }
    return up;
}

/*
 * Menu sharpness 0..100 for each pass.
 *
 * RCAS takes "stops" of reduction from its maximum: 0 is the sharpest, each
 * stop halves it.  100 -> 0 stops, 50 -> 1, 0 -> no RCAS pass at all (EASU
 * alone, which is already edge-adaptive).  CAS and NIS take 0..1.
 */
static inline bool nb_rcas_on(int sharp)
{
    return sharp > 0;
}

static inline float nb_rcas_stops(int sharp)
{
    if (sharp < 0) {
        sharp = 0;
    }
    if (sharp > 100) {
        sharp = 100;
    }
    return 2.0f * (1.0f - (float)sharp / 100.0f);
}

static inline float nb_sharp_unit(int sharp)
{
    return sharp <= 0 ? 0.0f : sharp >= 100 ? 1.0f : (float)sharp / 100.0f;
}

/* NIS's shader constants (NISConfig, NIS_Config.h), as plain uniforms. */
struct nb_nis_cfg {
    float    kDetectRatio, kDetectThres, kMinContrastRatio, kRatioNorm;
    float    kContrastBoost, kEps, kSharpStartY, kSharpScaleY;
    float    kSharpStrengthMin, kSharpStrengthScale, kSharpLimitMin,
             kSharpLimitScale;
    float    kScaleX, kScaleY, kDstNormX, kDstNormY, kSrcNormX, kSrcNormY;
    uint32_t kInputViewportOriginX, kInputViewportOriginY;
    uint32_t kInputViewportWidth, kInputViewportHeight;
    uint32_t kOutputViewportOriginX, kOutputViewportOriginY;
    uint32_t kOutputViewportWidth, kOutputViewportHeight;
};

static inline float nb_nis_max(float a, float b) { return a > b ? a : b; }

/*
 * NVScalerUpdateConfig for SDR input, a whole iw x ih texture scaled to a
 * whole ow x oh image.  false when the scale is outside NIS's 1x..2x.
 */
static inline bool nb_nis_config(struct nb_nis_cfg *c, float sharpness,
                                 uint32_t iw, uint32_t ih, uint32_t ow,
                                 uint32_t oh)
{
    float s, MaxScale, MinScale, LimitScale;
    float kSharpStartY = 0.45f, kSharpEndY = 0.9f;
    float kMinContrastRatio = 2.0f, kMaxContrastRatio = 10.0f;
    float kSharpStrengthMin, kSharpStrengthMax, kSharpLimitMin, kSharpLimitMax;

    if (!iw || !ih || !ow || !oh) {
        return false;
    }
    sharpness = sharpness < 0.0f ? 0.0f : sharpness > 1.0f ? 1.0f : sharpness;
    s = sharpness - 0.5f;
    MaxScale = s >= 0.0f ? 1.25f : 1.75f;
    MinScale = s >= 0.0f ? 1.25f : 1.0f;
    LimitScale = s >= 0.0f ? 1.25f : 1.0f;
    kSharpStrengthMin = nb_nis_max(0.0f, 0.4f + s * MinScale * 1.2f);
    kSharpStrengthMax = 1.6f + s * MaxScale * 1.8f;
    kSharpLimitMin = nb_nis_max(0.1f, 0.14f + s * LimitScale * 0.32f);
    kSharpLimitMax = 0.5f + s * LimitScale * 0.6f;

    c->kDetectRatio = 2 * 1127.f / 1024.f;
    c->kDetectThres = 64.0f / 1024.0f;
    c->kMinContrastRatio = kMinContrastRatio;
    c->kRatioNorm = 1.0f / (kMaxContrastRatio - kMinContrastRatio);
    c->kContrastBoost = 1.0f;
    c->kEps = 1.0f / 255.0f;
    c->kSharpStartY = kSharpStartY;
    c->kSharpScaleY = 1.0f / (kSharpEndY - kSharpStartY);
    c->kSharpStrengthMin = kSharpStrengthMin;
    c->kSharpStrengthScale = kSharpStrengthMax - kSharpStrengthMin;
    c->kSharpLimitMin = kSharpLimitMin;
    c->kSharpLimitScale = kSharpLimitMax - kSharpLimitMin;
    c->kInputViewportOriginX = c->kInputViewportOriginY = 0;
    c->kOutputViewportOriginX = c->kOutputViewportOriginY = 0;
    c->kInputViewportWidth = iw;
    c->kInputViewportHeight = ih;
    c->kOutputViewportWidth = ow;
    c->kOutputViewportHeight = oh;
    c->kSrcNormX = 1.f / (float)iw;
    c->kSrcNormY = 1.f / (float)ih;
    c->kDstNormX = 1.f / (float)ow;
    c->kDstNormY = 1.f / (float)oh;
    c->kScaleX = (float)iw / (float)ow;
    c->kScaleY = (float)ih / (float)oh;
    return !(c->kScaleX < 0.5f || c->kScaleX > 1.f || c->kScaleY < 0.5f ||
             c->kScaleY > 1.f);
}

/* NIS's dispatch: one 32x24 output block per work group (NIS_Scaler.h). */
#define NB_NIS_BLOCK_W 32
#define NB_NIS_BLOCK_H 24

#endif /* NB_ENHANCE_H */
