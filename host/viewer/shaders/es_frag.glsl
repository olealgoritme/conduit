#version 310 es
// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// Head of every enhancement fragment program (nb_gpuscale.c).  Rows are
// top-down in every texture and target here, so gl_FragCoord is the pixel
// position in the image as it is (docs/VIEWER-ENHANCE.md).
precision highp float;
precision highp int;
precision highp sampler2D;
in vec2 v_tc;
layout(location = 0) out vec4 o_col;
uniform float u_swap;               // 1: R and B swapped (read-back targets)
