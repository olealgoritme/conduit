// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
AF4 FsrEasuRF(AF2 p) { return textureGather(u_src, p, 0); }
AF4 FsrEasuGF(AF2 p) { return textureGather(u_src, p, 1); }
AF4 FsrEasuBF(AF2 p) { return textureGather(u_src, p, 2); }
AU4 nb_c0, nb_c1, nb_c2, nb_c3;     // EASU's constants, set once in main
// RCAS's load: the EASU output at that pixel, computed on the spot.
AF4 FsrRcasLoadF(ASU2 p)
{
    AF3 c;

    FsrEasuF(c, AU2(clamp(p, ivec2(0), u_max)), nb_c0, nb_c1, nb_c2, nb_c3);
    return AF4(c, 1.0);
}
void FsrRcasInputF(inout AF1 r, inout AF1 g, inout AF1 b) {}
void main()
{
    AU4 con;
    AF1 r, g, b;

    FsrEasuCon(nb_c0, nb_c1, nb_c2, nb_c3, u_io.x, u_io.y, u_io.x, u_io.y,
               u_io.z, u_io.w);
    FsrRcasCon(con, u_stops);
    FsrRcasF(r, g, b, AU2(ivec2(gl_FragCoord.xy) - u_org), con);
    AF3 c = AF3(r, g, b);
    o_col = vec4(mix(c, c.bgr, u_swap), 1.0);
}
