#version 310 es
// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// The one quad every enhancement fragment pass draws: a_pos 0..1 over the
// viewport, v_tc the source rectangle u_rect (x, y, w, h) in texture space.
layout(location = 0) in vec2 a_pos;
uniform vec4 u_rect;
out vec2 v_tc;
void main()
{
    v_tc = u_rect.xy + a_pos * u_rect.zw;
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
}
