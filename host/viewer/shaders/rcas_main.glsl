// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
AF4 FsrRcasLoadF(ASU2 p) { return texelFetch(u_src, clamp(p, ivec2(0), u_max), 0); }
void FsrRcasInputF(inout AF1 r, inout AF1 g, inout AF1 b) {}
void main()
{
    AU4 con;
    AF1 r, g, b;

    FsrRcasCon(con, u_stops);
    FsrRcasF(r, g, b, AU2(ivec2(gl_FragCoord.xy) - u_org), con);
    AF3 c = AF3(r, g, b);
    o_col = vec4(mix(c, c.bgr, u_swap), 1.0);
}
