#!/usr/bin/env python3
# GLSL for vk_gpu_bound, compiled to SPIR-V C headers in shaders_gb/.
# The HLSL twins in guest/windows/tools/d3d12_gpu_bound.cpp do the same work.
import os, subprocess

here = os.path.dirname(os.path.abspath(__file__))
S = {}

PC = """layout(push_constant) uniform PC { vec4 p; uvec4 u; } pc;
"""
HASH = """uint pcg(uint v) {
   uint s = v * 747796405u + 2891336453u;
   uint w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
   return (w >> 22u) ^ w;
}
"""

S["fsq.vert"] = """#version 450
void main() {
   vec2 p = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
   gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"""
# Transcendental + FMA mix (the shape of vk_perf_bench's alu)
S["alu.frag"] = "#version 450\n" + PC + """layout(location = 0) out vec4 o;
void main() {
   vec4 a = vec4(gl_FragCoord.xy * 0.001, pc.p.x, 1.0);
   for (uint i = 0u; i < pc.u.x; i++) {
      a = fract(a * a.yzwx * 1.0001 + vec4(0.1, 0.2, 0.3, 0.4));
      a = sin(a) * 0.5 + a;
   }
   o = a;
}
"""
# Four independent vec4 FMA chains: 16 FMAs (32 flops) per iteration
S["fma.frag"] = "#version 450\n" + PC + """layout(location = 0) out vec4 o;
void main() {
   vec4 a = vec4(gl_FragCoord.xyxy * 0.001);
   vec4 b = a + 0.1, c = a + 0.2, d = a + 0.3;
   vec4 k = pc.p * 0.5 + 0.25, m = pc.p * 0.125;
   for (uint i = 0u; i < pc.u.x; i++) {
      a = fma(a, k, m); b = fma(b, k, m);
      c = fma(c, k, m); d = fma(d, k, m);
   }
   o = a + b + c + d;
}
"""
S["color.frag"] = "#version 450\n" + PC + """layout(location = 0) out vec4 o;
void main() { o = pc.p; }
"""
# A different hashed colour per pixel and pass: incompressible
S["noise.frag"] = "#version 450\n" + PC + HASH + """layout(location = 0) out vec4 o;
void main() {
   uvec2 c = uvec2(gl_FragCoord.xy);
   uint h = pcg(c.x ^ (c.y << 16) ^ (pc.u.y * 0x9e3779b9u));
   o = unpackUnorm4x8(h);
}
"""
S["tex.frag"] = "#version 450\n" + PC + """layout(set = 0, binding = 0) uniform sampler2D t;
layout(location = 0) out vec4 o;
void main() {
   vec2 uv = gl_FragCoord.xy * pc.p.xy;
   vec4 s = vec4(0.0);
   for (int i = 0; i < 8; i++)
      s += textureLod(t, uv + vec2(i) * pc.p.zw, 0.0);
   o = s;
}
"""
# 8x8 pixel blocks each read a random place of a large texture: little reuse
# across blocks, so every block walks new pages (TLB and DRAM, not L2)
S["texrand.frag"] = "#version 450\n" + PC + HASH + """layout(set = 0, binding = 0) uniform sampler2D t;
layout(location = 0) out vec4 o;
void main() {
   uvec2 c = uvec2(gl_FragCoord.xy);
   uint h = pcg((c.x >> 3u) ^ ((c.y >> 3u) << 12u) ^ (pc.u.y * 0x9e3779b9u));
   vec2 base = vec2(h & 0xffffu, h >> 16u) / 65536.0;
   vec2 texel = pc.p.xy;
   vec4 s = vec4(0.0);
   for (int i = 0; i < 4; i++)
      s += textureLod(t, base + (vec2(c & 7u) + vec2(i * 8, 0)) * texel, 0.0);
   o = s;
}
"""
# Reads the previous pass's target (render pass split + barrier chain)
S["blit.frag"] = "#version 450\n" + PC + """layout(set = 0, binding = 0) uniform sampler2D t;
layout(location = 0) out vec4 o;
void main() {
   o = textureLod(t, gl_FragCoord.xy * pc.p.xy, 0.0) * 0.999 + 0.001;
}
"""
S["copy.comp"] = "#version 450\n" + PC + """layout(local_size_x = 256) in;
layout(set = 0, binding = 1) readonly buffer Src { uvec4 s[]; };
layout(set = 0, binding = 2) writeonly buffer Dst { uvec4 d[]; };
void main() {
   uint i = gl_GlobalInvocationID.x;
   if (i < pc.u.x)
      d[i] = s[i];
}
"""
# ALU loop per thread, a fixed amount of GPU work per dispatch
S["spin.comp"] = "#version 450\n" + PC + """layout(local_size_x = 64) in;
layout(set = 0, binding = 2) writeonly buffer Dst { vec4 d[]; };
void main() {
   uint i = gl_GlobalInvocationID.x;
   vec4 a = vec4(float(i) * 1e-6, 0.1, 0.2, 0.3);
   for (uint k = 0u; k < pc.u.x; k++)
      a = fma(a, vec4(1.0001), vec4(1e-4));
   d[i & 0xffffu] = a;
}
"""

out = os.path.join(here, "shaders_gb")
os.makedirs(out, exist_ok=True)
for name, src in S.items():
    p = os.path.join(out, name)
    with open(p, "w") as f:
        f.write(src)
    var = "spv_" + name.replace(".", "_")
    subprocess.check_call(["glslangValidator", "-V", "--target-env", "vulkan1.3",
                           "--vn", var, "-o", os.path.join(out, name.replace(".", "_") + ".h"), p],
                          stdout=subprocess.DEVNULL)
print("ok")
