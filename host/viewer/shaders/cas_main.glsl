// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
vec3 CasLoad(ivec2 p) { return texelFetch(u_src, clamp(p, ivec2(0), u_max), 0).rgb; }
void CasInput(inout float r, inout float g, inout float b) {}
void main()
{
    AU4 c0, c1;
    AF1 r, g, b;

    CasSetup(c0, c1, u_sharp, u_in.x, u_in.y, u_in.x, u_in.y);
    CasFilter(r, g, b, AU2(gl_FragCoord.xy), c0, c1, true);
    AF3 c = AF3(r, g, b);
    o_col = vec4(mix(c, c.bgr, u_swap), 1.0);
}
