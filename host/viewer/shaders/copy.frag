// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
// The plain pass on an internal texture: the source rectangle, filtered by
// the texture's own filter (linear or nearest), opaque.
uniform sampler2D u_src;
void main()
{
    vec4 t = texture(u_src, v_tc);
    o_col = vec4(mix(t.rgb, t.bgr, u_swap), 1.0);
}
