// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// FSR 1 RCAS (third_party/fsr1, MIT): sharpens a texture 1:1 into the
// viewport at u_org.  Followed by ffx_a.h, ffx_fsr1.h, rcas_main.glsl.
uniform sampler2D u_src;            // texelFetch
uniform ivec2 u_org;                // the viewport's origin in the target
uniform ivec2 u_max;                // the texture's last pixel
uniform float u_stops;              // FsrRcasCon sharpness: 0 = sharpest
#define A_GPU 1
#define A_GLSL 1
#define FSR_RCAS_F 1
