// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// AMD FidelityFX CAS (third_party/cas, MIT), sharpen only: a texture 1:1
// into a texture of its size.  Followed by ffx_a.h, ffx_cas.h, cas_main.glsl.
uniform sampler2D u_src;            // texelFetch
uniform ivec2 u_max;                // the texture's last pixel
uniform vec2  u_in;                 // its size
uniform float u_sharp;              // CasSetup sharpness 0..1
vec3 CasLoad(ivec2 p);
void CasInput(inout float r, inout float g, inout float b);
#define A_GPU 1
#define A_GLSL 1
