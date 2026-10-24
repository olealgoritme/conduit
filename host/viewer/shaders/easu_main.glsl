// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
AF4 FsrEasuRF(AF2 p) { return textureGather(u_src, p, 0); }
AF4 FsrEasuGF(AF2 p) { return textureGather(u_src, p, 1); }
AF4 FsrEasuBF(AF2 p) { return textureGather(u_src, p, 2); }
void main()
{
    AU4 c0, c1, c2, c3;
    AF3 c;

    // The constant setup is upstream's own (it "works on CPU or GPU").
    FsrEasuCon(c0, c1, c2, c3, u_io.x, u_io.y, u_io.x, u_io.y, u_io.z,
               u_io.w);
    FsrEasuF(c, AU2(ivec2(gl_FragCoord.xy) - u_org), c0, c1, c2, c3);
    o_col = vec4(mix(c, c.bgr, u_swap), 1.0);
}
