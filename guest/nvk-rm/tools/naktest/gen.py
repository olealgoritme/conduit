#!/usr/bin/env python3
"""gen.py OUT_DIR [SEED]: generate the NAK hardware validation shaders (GLSL),
their CPU reference functions (C) and the test table (tests.h).

Every test is a compute shader over N invocations: each reads its input
words from `inp`, runs a chain of operations in which every instruction
depends on the one before, and writes one word to `outp`.  The CPU runs the
same chain.  If the compiler waits too few cycles between a producer and its
consumer (NAK_LAT) or reuses a scoreboard wrongly (NAK_BAR_SHARE), the
consumer reads a stale register and the result differs.

Values stay exactly representable (small integers and halves for floats),
so results do not depend on rounding or on FMA contraction.
"""
import os
import random
import sys

out = sys.argv[1]
seed = int(sys.argv[2]) if len(sys.argv) > 2 else 1
rnd = random.Random(seed)
os.makedirs(out, exist_ok=True)

HDR = """#version 450
#extension GL_ARB_gpu_shader_int64 : enable
layout(local_size_x = 64) in;
layout(std430, binding = 0) readonly buffer In { uint inp[]; };
layout(std430, binding = 1) writeonly buffer Out { uint outp[]; };
layout(push_constant) uniform PC { uint u[8]; } pc;
"""
HDR_TEX = HDR + """#extension GL_EXT_texture_shadow_lod : require
layout(binding = 2) uniform usampler2DArray ut;
layout(binding = 3) uniform sampler2DArrayShadow st;
"""
tests = []   # (name, kind, glsl_body, c_body)


def emit(name, kind, glsl, c):
    tests.append((name, kind, glsl, c))


def fma_chain(n):
    # x stays an integer or a half: *2 and *0.5 alternate
    g, c = [], []
    for i in range(n):
        b = rnd.randint(-3, 3)
        a = 2.0 if i % 2 == 0 else 0.5
        if rnd.random() < 0.25:
            g.append(f"x = x * {a:.1f};"); c.append(f"x = x * {a:.1f}f;")
            g.append(f"x = x + {b}.0;"); c.append(f"x = x + {b}.0f;")
        else:
            g.append(f"x = fma(x, {a:.1f}, {b}.0);"); c.append(f"x = x * {a:.1f}f + {b}.0f;")
    return g, c


def int_chain(n, ops):
    g = []
    for _ in range(n):
        op = rnd.choice(ops)
        k = rnd.randint(1, 0x7fffffff) | 1
        s = rnd.randint(1, 31)
        if op == "imad":
            g.append(f"y = y * {k}u + {rnd.randint(0, 999)}u;")
        elif op == "iadd":
            g.append(f"y = y + {k}u;")
        elif op == "xor":
            g.append(f"y = y ^ {k}u;")
        elif op == "lop3":
            g.append(f"y = (y & {k}u) | (~y & {k ^ 0x5a5a5a5a}u);")
        elif op == "shf":
            g.append(f"y = (y << {s}u) | (y >> {32 - s}u);")
        elif op == "prmt":
            g.append("y = ((y & 0xffu) << 24u) | ((y & 0xff00u) << 8u) | ((y >> 8u) & 0xff00u) | (y >> 24u);")
        elif op == "mix":
            g.append(f"y = y + (z ^ {k}u); z = z * {k}u + y;")
    return g, list(g)


def emit_float(name, n):
    g, c = fma_chain(n)
    glsl = ["float x = float(inp[i] % 1000u) + 1.0;"] + g + ["outp[i] = floatBitsToUint(x);"]
    cc = ["float x = (float)(in[i] % 1000u) + 1.0f;"] + c + ["return f2u(x);"]
    emit(name, "fma", glsl, cc)


def emit_int(name, n, ops):
    g, c = int_chain(n, ops)
    glsl = ["uint y = inp[i]; uint z = inp[i] * 2654435761u;"] + g + ["outp[i] = y ^ z;"]
    cc = ["uint32_t y = in[i]; uint32_t z = in[i] * 2654435761u;"] + c + ["return y ^ z;"]
    emit(name, ops[0], glsl, cc)


def emit_mixed(name, n):
    # float FMA results feed integer ops and back (fma <-> alu pairs)
    g, c = [], []
    for k in range(n):
        b = rnd.randint(-3, 3)
        a = 2.0 if k % 2 == 0 else 0.5
        g.append(f"x = fma(x, {a:.1f}, {b}.0); y = y + uint(int(x)) * {rnd.randint(1, 99)}u;")
        c.append(f"x = x * {a:.1f}f + {b}.0f; y = y + (uint32_t)(int32_t)x * {int(g[-1].split('* ')[-1][:-2])}u;")
    glsl = ["float x = float(inp[i] % 1000u) + 1.0; uint y = inp[i];"] + g + ["outp[i] = y ^ floatBitsToUint(x);"]
    cc = ["float x = (float)(in[i] % 1000u) + 1.0f; uint32_t y = in[i];"] + c + ["return y ^ f2u(x);"]
    emit(name, "fma-alu", glsl, cc)


def emit_guard(name, n):
    # small ifs: NAK predicates them (if-conversion) and guards read the
    # comparison result right away
    g = []
    for _ in range(n):
        m = rnd.randint(1, 7)
        k = rnd.randint(0, m) & m
        c1 = rnd.randint(1, 0xffff)
        c2 = rnd.randint(1, 0xffff)
        g.append(f"if ((y & {m}u) == {k}u) {{ y = y * 3u + {c1}u; }} else {{ y = y ^ {c2}u; }}")
    glsl = ["uint y = inp[i];"] + g + ["outp[i] = y;"]
    cc = ["uint32_t y = in[i];"] + g + ["return y;"]
    emit(name, "guard", glsl, cc)


def emit_carry(name, n):
    g = []
    for _ in range(n):
        k = rnd.randint(1, (1 << 63) - 1)
        g.append(f"w = w + {k}ul; w = w ^ (w >> {rnd.randint(1, 63)}u);")
    glsl = ["uint64_t w = uint64_t(inp[i]) * 0x9e3779b97f4a7c15ul;"] + g + ["outp[i] = uint(w) ^ uint(w >> 32u);"]
    cc = ["uint64_t w = (uint64_t)in[i] * 0x9e3779b97f4a7c15ull;"] + [x.replace("ul;", "ull;") for x in g] + ["return (uint32_t)w ^ (uint32_t)(w >> 32u);"]
    emit(name, "carry", glsl, cc)


def emit_uniform(name, n):
    # a chain on push constants only: NAK computes it in uniform registers
    g = []
    for k in range(n):
        j = rnd.randint(0, 7)
        g.append(f"u = u * {rnd.randint(1, 0xffff) | 1}u + pc.u[{j}]; u = (u << {rnd.randint(1, 31)}u) | (u >> 7u);")
    glsl = ["uint u = pc.u[0];"] + g + ["outp[i] = u ^ inp[i];"]
    cc = ["uint32_t u = pc[0];"] + [x.replace("pc.u[", "pc[") for x in g] + ["return u ^ in[i];"]
    # (u >> 7) after (u << s) | ... is what the GLSL says; keep both identical
    emit(name, "uniform", glsl, cc)


def emit_loads(name, nloads):
    # many independent loads in flight (more than the six scoreboards), each
    # consumed late and in a different order
    idx = [rnd.randint(1, 4093) for _ in range(nloads)]
    order = list(range(nloads))
    rnd.shuffle(order)
    g = [f"uint v{k} = inp[(i * {2 * k + 1}u + {idx[k]}u) & mask];" for k in range(nloads)]
    acc = ["uint s = 0u;"] + [f"s = (s ^ v{k}) * 16777619u + {k}u;" for k in order]
    glsl = ["uint mask = pc.u[1];"] + g + acc + ["outp[i] = s;"]
    cc = ["uint32_t mask = pc[1];"] + [x.replace("uint v", "uint32_t v").replace("inp[", "in[") for x in g] + [x.replace("uint s", "uint32_t s") for x in acc] + ["return s;"]
    emit(name, "loads", glsl, cc)


def emit_wgsync(name):
    # invocations of one workgroup exchange values through the storage
    # buffer around barriers: weak (L1-cached) accesses must still see each
    # other's writes after the barrier (NAK_WEAK_MEM)
    glsl = ["uint a = inp[i] * 2654435761u + 7u;", "outp[i] = a;",
            "memoryBarrierBuffer(); barrier();",
            "uint b = outp[i ^ 1u] ^ outp[i ^ 2u] ^ outp[i ^ 33u];",
            "memoryBarrierBuffer(); barrier();",
            "outp[i] = b * 3u + a;"]
    cc = ["uint32_t a = in[i] * 2654435761u + 7u;",
          "uint32_t b = (in[i ^ 1u] * 2654435761u + 7u) ^ (in[i ^ 2u] * 2654435761u + 7u) ^ (in[i ^ 33u] * 2654435761u + 7u);",
          "return b * 3u + a;"]
    emit(name, "wgsync", glsl, cc)


def emit_tex():
    # Texture instructions with handles from a descriptor set (the same in
    # every lane), in the source layouts NAK_TEX_UHANDLE turns into the
    # scalar form with the handle in a uniform register: 2+1, 2+2 and 5
    # components.  The images (naktest.c): ut, R32_UINT 64x64, 4 layers, 3
    # mips, texel (mip << 24 | layer << 16 | y << 8 | x); st, D32 64x64, 4
    # layers, depth ((x * 7 + y * 13 + layer * 29) & 255) / 256; nearest
    # filtering, clamp to edge, st compares LESS_OR_EQUAL
    pre = ["uint r = inp[i];", "uint x = r & 63u, y = (r >> 6) & 63u, l = (r >> 12) & 3u, m = (r >> 14) % 3u;"]
    cpre = ["uint32_t r = in[i];", "uint32_t x = r & 63u, y = (r >> 6) & 63u, l = (r >> 12) & 3u, m = (r >> 14) % 3u;",
            "(void)x; (void)y; (void)l; (void)m;"]
    ctr = ("vec2 c0 = vec2((float(x) + 0.5) / 64.0, (float(y) + 0.5) / 64.0);",)
    # TLD, array, with a level: layer, x, y + lod
    emit("tld_arr_lod", "tex", pre + ["outp[i] = texelFetch(ut, ivec3(int(x >> m), int(y >> m), int(l)), int(m)).x;"],
         cpre + ["return tex_u(m, l, y >> m, x >> m);"])
    # TEX, array, explicit level
    emit("tex_arr_lod", "tex", pre + ["float s = float(64u >> m);",
                                      "vec3 c = vec3((float(x >> m) + 0.5) / s, (float(y >> m) + 0.5) / s, float(l));",
                                      "outp[i] = textureLod(ut, c, float(m)).x;"],
         cpre + ["return tex_u(m, l, y >> m, x >> m);"])
    # TEX, array, level 0 with an offset: layer, x, y + offset
    emit("tex_arr_off", "tex", pre + list(ctr) + ["outp[i] = textureLodOffset(ut, vec3(c0, float(l)), 0.0, ivec2(1, -1)).x;"],
         cpre + ["return tex_u(0, l, y == 0 ? 0 : y - 1, x == 63 ? 63 : x + 1);"])
    # TLD4, array: layer, x, y; and with an offset
    # the 2x2 footprint (x0, y0)-(x1, y1), clamped: (x0, y1), (x1, y1), (x1, y0), (x0, y0)
    gret = "return tex_u(0, l, y1, x0) ^ tex_u(0, l, y1, x1) * 3u ^ tex_u(0, l, y, x1) * 5u ^ tex_u(0, l, y, x0) * 7u;"
    emit("tld4_arr", "tex", pre + list(ctr) + ["uvec4 g = textureGather(ut, vec3(c0, float(l)), 0);",
                                                "outp[i] = g.x ^ g.y * 3u ^ g.z * 5u ^ g.w * 7u;"],
         cpre + ["uint32_t x0 = x, x1 = x == 63 ? 63 : x + 1, y1 = y == 63 ? 63 : y + 1;", gret])
    emit("tld4_arr_off", "tex", pre + list(ctr) + ["uvec4 g = textureGatherOffset(ut, vec3(c0, float(l)), ivec2(-1, 0), 0);",
                                                    "outp[i] = g.x ^ g.y * 3u ^ g.z * 5u ^ g.w * 7u;"],
         cpre + ["uint32_t x0 = x == 0 ? 0 : x - 1, x1 = x, y1 = y == 63 ? 63 : y + 1;", gret])
    # TEX.DC, array, level 0: layer, x, y + dref (88825d04's shape); and with an offset (5 components)
    dref = "float d = float((r >> 16) & 255u) / 256.0 + 1.0 / 512.0;"
    cd = ["float d = (float)((r >> 16) & 255u) / 256.0f + 1.0f / 512.0f;"]
    emit("tex_dc_arr", "tex", pre + list(ctr) + [dref, "outp[i] = uint(textureLod(st, vec4(c0, float(l), d), 0.0));"],
         cpre + cd + ["return d <= tex_d(l, y, x) ? 1u : 0u;"])
    emit("tex_dc_arr_off", "tex", pre + list(ctr) + [dref, "outp[i] = uint(textureLodOffset(st, vec4(c0, float(l), d), 0.0, ivec2(1, 0)));"],
         cpre + cd + ["return d <= tex_d(l, y, x == 63 ? 63 : x + 1) ? 1u : 0u;"])


for v in range(4):
    emit_float(f"fma{v}", 256)
for v in range(3):
    emit_int(f"imad{v}", 256, ["imad", "imad", "iadd"])
for v in range(3):
    emit_int(f"lop{v}", 256, ["lop3", "xor", "shf", "prmt"])
for v in range(3):
    emit_int(f"alu{v}", 256, ["iadd", "xor", "mix", "imad", "shf"])
for v in range(3):
    emit_mixed(f"fmaalu{v}", 128)
for v in range(4):
    emit_guard(f"guard{v}", 96)
for v in range(3):
    emit_carry(f"carry{v}", 128)
for v in range(3):
    emit_uniform(f"uniform{v}", 128)
for v, n in enumerate([8, 12, 16, 24, 32]):
    emit_loads(f"loads{n}", n)
emit_wgsync("wgsync")
emit_tex()

c = ["/* generated by gen.py: CPU references */", "#include <stdint.h>", "#include <string.h>",
     "static uint32_t f2u(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }",
     "static uint32_t tex_u(uint32_t m, uint32_t l, uint32_t y, uint32_t x) { return m << 24 | l << 16 | y << 8 | x; }",
     "static float tex_d(uint32_t l, uint32_t y, uint32_t x) { return (float)((x * 7u + y * 13u + l * 29u) & 255u) / 256.0f; }"]
for name, kind, glsl, cc in tests:
    hdr = HDR.replace("writeonly buffer", "buffer") if kind == "wgsync" else HDR_TEX if kind == "tex" else HDR
    with open(f"{out}/{name}.comp", "w") as f:
        f.write(hdr + "void main() {\n  uint i = gl_GlobalInvocationID.x;\n  " + "\n  ".join(glsl) + "\n}\n")
    c.append(f"static uint32_t ref_{name}(const uint32_t *in, uint32_t i, const uint32_t *pc) {{")
    c.append("  (void)pc;")
    c += ["  " + l for l in cc]
    c.append("}")
c.append("struct test { const char *name, *kind; const uint32_t *spv; size_t spv_size; uint32_t (*ref)(const uint32_t *, uint32_t, const uint32_t *); };")
for name, *_ in tests:
    c.append(f'#include "{name}.h"')
c.append("static const struct test tests[] = {")
for name, kind, *_ in tests:
    c.append(f'  {{ "{name}", "{kind}", spv_{name}, sizeof(spv_{name}), ref_{name} }},')
c.append("};")
open(f"{out}/tests.h", "w").write("\n".join(c) + "\n")
print(len(tests), "tests")
