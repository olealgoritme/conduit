// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// AMD FidelityFX CAS (third_party/cas, MIT).  After cas_noscale.glsl:
// sharpen a texture 1:1; after cas_scale.glsl: sharpen and scale in one
// pass (CAS supports up to 4x the area).  Into the viewport at u_org.
// Followed by ffx_a.h, ffx_cas.h, cas_main.glsl.
uniform sampler2D u_src;            // texelFetch
uniform ivec2 u_max;                // the texture's last pixel
uniform vec2  u_in;                 // its size
uniform vec2  u_out;                // the output's size
uniform ivec2 u_org;                // the viewport's origin in the target
uniform float u_sharp;              // CasSetup sharpness 0..1
vec3 CasLoad(ivec2 p);
void CasInput(inout float r, inout float g, inout float b);
#define A_GPU 1
#define A_GLSL 1
