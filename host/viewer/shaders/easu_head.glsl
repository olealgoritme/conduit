// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// FSR 1 EASU (third_party/fsr1, MIT): the whole source texture upscaled to
// the whole viewport.  Followed by ffx_a.h, ffx_fsr1.h, easu_main.glsl.
uniform sampler2D u_src;            // linear filtering, clamped
uniform vec4 u_io;                  // input w, h; output w, h (pixels)
#define A_GPU 1
#define A_GLSL 1
#define FSR_EASU_F 1
