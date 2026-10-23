// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// NVIDIA Image Scaling (shaders/nis_scaler_gles.glsl, MIT): the whole source
// texture scaled into the whole output image, one 32x24 block per work
// group.  NISConfig as plain uniforms (nb_enhance.h nb_nis_config).
#define NIS_GLSL 1
#define NIS_HLSL 0
#define NIS_HLSL_6_2 0
#define NIS_SCALER 1
#define NIS_USE_HALF_PRECISION 0
#define NIS_CLAMP_OUTPUT 1
#define NIS_NV12_SUPPORT 0
#define NIS_VIEWPORT_SUPPORT 0
#define NIS_TEXTURE_GATHER 0
#define NIS_BLOCK_WIDTH 32
#define NIS_BLOCK_HEIGHT 24
#define NIS_THREAD_GROUP_SIZE 256
uniform float kDetectRatio;
uniform float kDetectThres;
uniform float kMinContrastRatio;
uniform float kRatioNorm;
uniform float kContrastBoost;
uniform float kEps;
uniform float kSharpStartY;
uniform float kSharpScaleY;
uniform float kSharpStrengthMin;
uniform float kSharpStrengthScale;
uniform float kSharpLimitMin;
uniform float kSharpLimitScale;
uniform float kScaleX;
uniform float kScaleY;
uniform float kDstNormX;
uniform float kDstNormY;
uniform float kSrcNormX;
uniform float kSrcNormY;
uniform uint kInputViewportOriginX;
uniform uint kInputViewportOriginY;
uniform uint kInputViewportWidth;
uniform uint kInputViewportHeight;
uniform uint kOutputViewportOriginX;
uniform uint kOutputViewportOriginY;
uniform uint kOutputViewportWidth;
uniform uint kOutputViewportHeight;
uniform sampler2D in_texture;       // linear filtering, clamped
uniform sampler2D coef_scaler;      // 2 x 64 RGBA32F (shaders/nis_coef.h)
uniform sampler2D coef_usm;
layout(rgba8, binding = 0) uniform writeonly highp image2D out_texture;
// Combined samplers here: the SDK's sampler2D(texture, sampler) is the texture.
#define sampler2D(x, s) (x)
