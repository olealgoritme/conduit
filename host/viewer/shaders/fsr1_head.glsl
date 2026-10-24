// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// FSR 1 in ONE pass (third_party/fsr1, MIT): RCAS whose five taps are EASU
// evaluated where RCAS reads them, so the upscaled picture never goes
// through memory; into the viewport at u_org.  Five EASU evaluations per
// pixel instead of one, and no output-size intermediate written and read
// back.  Followed by ffx_a.h, ffx_fsr1.h, fsr1_main.glsl.
uniform sampler2D u_src;            // linear filtering, clamped
uniform vec4  u_io;                 // input w, h; output w, h (pixels)
uniform ivec2 u_org;                // the viewport's origin in the target
uniform ivec2 u_max;                // the output's last pixel
uniform float u_stops;              // FsrRcasCon sharpness: 0 = sharpest
#define A_GPU 1
#define A_GLSL 1
#define FSR_EASU_F 1
#define FSR_RCAS_F 1
