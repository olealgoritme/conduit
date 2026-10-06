#!/usr/bin/env python3
# Writes the GLSL sources and compiles them to SPIR-V C headers.
import os, subprocess, sys

here = os.path.dirname(os.path.abspath(__file__))
S = {}

S["fsq.vert"] = """#version 450
void main() {
   vec2 p = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
   gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"""
S["fsqz.vert"] = """#version 450
layout(push_constant) uniform PC { vec4 p; } pc;
void main() {
   vec2 p = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
   gl_Position = vec4(p * 2.0 - 1.0, pc.p.z, 1.0);
}
"""
S["alu.frag"] = """#version 450
layout(push_constant) uniform PC { vec4 p; } pc;
layout(location = 0) out vec4 o;
void main() {
   vec4 a = vec4(gl_FragCoord.xy * 0.001, pc.p.x, 1.0);
   int n = int(pc.p.y);
   for (int i = 0; i < n; i++) {
      a = fract(a * a.yzwx * 1.0001 + vec4(0.1, 0.2, 0.3, 0.4));
      a = sin(a) * 0.5 + a;
   }
   o = a;
}
"""
S["tex.frag"] = """#version 450
layout(set = 0, binding = 1) uniform sampler2D t;
layout(push_constant) uniform PC { vec4 p; } pc;
layout(location = 0) out vec4 o;
void main() {
   vec2 uv = gl_FragCoord.xy * pc.p.xy;
   vec4 s = vec4(0.0);
   for (int i = 0; i < 8; i++)
      s += texture(t, uv + vec2(float(i) * 0.123, float(i) * 0.071));
   o = s * 0.125;
}
"""
S["blend.frag"] = """#version 450
layout(location = 0) out vec4 o;
void main() { o = vec4(0.3, 0.5, 0.7, 0.5); }
"""
S["ubo.vert"] = """#version 450
layout(set = 0, binding = 0) uniform U { mat4 m; vec4 c; } u;
layout(location = 0) out vec4 col;
void main() {
   vec2 p = vec2(float(gl_VertexIndex & 1), float(gl_VertexIndex >> 1)) * 0.02;
   gl_Position = u.m * vec4(p, 0.0, 1.0);
   col = u.c;
}
"""
S["color.frag"] = """#version 450
layout(location = 0) in vec4 col;
layout(location = 0) out vec4 o;
void main() { o = col; }
"""
S["colortex.frag"] = """#version 450
layout(set = 0, binding = 1) uniform sampler2D t;
layout(location = 0) in vec4 col;
layout(location = 0) out vec4 o;
void main() { o = col * texture(t, gl_FragCoord.xy * 0.001); }
"""
S["mesh.vert"] = """#version 450
layout(location = 0) in vec4 pos;
layout(location = 0) out vec4 col;
void main() {
   gl_Position = vec4(pos.xy, pos.z, 1.0);
   col = vec4(pos.zw, 0.5, 1.0);
}
"""
S["meshubo.vert"] = """#version 450
layout(set = 0, binding = 0) uniform U { mat4 m; vec4 c; } u;
layout(location = 0) in vec4 pos;
layout(location = 0) out vec4 col;
void main() {
   gl_Position = u.m * vec4(pos.xy * 0.05, pos.z, 1.0);
   col = u.c * pos.z;
}
"""
S["tess.vert"] = """#version 450
layout(push_constant) uniform PC { vec4 p; } pc;
void main() {
   int patch_id = gl_VertexIndex >> 2;
   int c = gl_VertexIndex & 3;
   int n = int(pc.p.y);
   vec2 base = vec2(patch_id % n, patch_id / n);
   vec2 corner = vec2(c & 1, c >> 1);
   gl_Position = vec4((base + corner) / float(n) * 2.0 - 1.0, 0.5, 1.0);
}
"""
S["tess.tesc"] = """#version 450
layout(vertices = 4) out;
layout(push_constant) uniform PC { vec4 p; } pc;
void main() {
   gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;
   if (gl_InvocationID == 0) {
      float l = pc.p.x;
      gl_TessLevelOuter[0] = l; gl_TessLevelOuter[1] = l;
      gl_TessLevelOuter[2] = l; gl_TessLevelOuter[3] = l;
      gl_TessLevelInner[0] = l; gl_TessLevelInner[1] = l;
   }
}
"""
S["tess.tese"] = """#version 450
layout(quads, fractional_odd_spacing, ccw) in;
layout(location = 0) out vec4 col;
void main() {
   vec4 a = mix(gl_in[0].gl_Position, gl_in[1].gl_Position, gl_TessCoord.x);
   vec4 b = mix(gl_in[2].gl_Position, gl_in[3].gl_Position, gl_TessCoord.x);
   vec4 p = mix(a, b, gl_TessCoord.y);
   p.z = 0.5 + 0.1 * sin(p.x * 40.0) * cos(p.y * 40.0);
   gl_Position = p;
   col = vec4(gl_TessCoord.xy, p.z, 1.0);
}
"""
S["tri.vert"] = """#version 450
layout(push_constant) uniform PC { vec4 p; } pc;
layout(location = 0) out vec2 uv;
void main() {
   int n = int(pc.p.y);
   int tri = gl_VertexIndex / 3;
   int c = gl_VertexIndex % 3;
   int cell = tri >> 1;
   vec2 base = vec2(cell % n, cell / n);
   vec2 o[6] = vec2[](vec2(0,0), vec2(1,0), vec2(0,1), vec2(1,0), vec2(1,1), vec2(0,1));
   vec2 q = (base + o[(tri & 1) * 3 + c]) / float(n);
   uv = q;
   gl_Position = vec4(q * 2.0 - 1.0, 0.5, 1.0);
}
"""
S["tri.tesc"] = """#version 450
layout(vertices = 3) out;
layout(push_constant) uniform PC { vec4 p; } pc;
layout(set = 0, binding = 0) uniform U { mat4 m; vec4 c; } u;
layout(location = 0) in vec2 uv[];
layout(location = 0) out vec2 uv_out[];
float lvl(vec4 a, vec4 b) {
   float d = length((u.m * (a + b) * 0.5).xyz - vec3(0.0, 0.0, -2.0));
   return clamp(pc.p.x * 2.0 / d, 1.0, 64.0);
}
void main() {
   gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;
   uv_out[gl_InvocationID] = uv[gl_InvocationID];
   if (gl_InvocationID == 0) {
      gl_TessLevelOuter[0] = lvl(gl_in[1].gl_Position, gl_in[2].gl_Position);
      gl_TessLevelOuter[1] = lvl(gl_in[2].gl_Position, gl_in[0].gl_Position);
      gl_TessLevelOuter[2] = lvl(gl_in[0].gl_Position, gl_in[1].gl_Position);
      gl_TessLevelInner[0] = (gl_TessLevelOuter[0] + gl_TessLevelOuter[1] + gl_TessLevelOuter[2]) / 3.0;
   }
}
"""
S["tri.tese"] = """#version 450
layout(triangles, fractional_odd_spacing, cw) in;
layout(set = 0, binding = 1) uniform sampler2D t;
layout(set = 0, binding = 0) uniform U { mat4 m; vec4 c; } u;
layout(location = 0) in vec2 uv[];
layout(location = 0) out vec4 col;
void main() {
   vec3 b = gl_TessCoord;
   vec4 p = gl_in[0].gl_Position * b.x + gl_in[1].gl_Position * b.y + gl_in[2].gl_Position * b.z;
   vec2 q = uv[0] * b.x + uv[1] * b.y + uv[2] * b.z;
   float h = textureLod(t, q * 4.0, 0.0).r;
   float hx = textureLod(t, q * 4.0 + vec2(0.001, 0.0), 0.0).r;
   float hy = textureLod(t, q * 4.0 + vec2(0.0, 0.001), 0.0).r;
   vec3 nrm = normalize(vec3(h - hx, h - hy, 0.05));
   p.z = 0.5 + 0.2 * h;
   gl_Position = u.m * p;
   col = vec4(nrm * 0.5 + 0.5, 1.0) * u.c;
}
"""
S["copy.comp"] = """#version 450
layout(local_size_x = 256) in;
layout(std430, set = 0, binding = 0) readonly buffer A { vec4 a[]; };
layout(std430, set = 0, binding = 1) writeonly buffer B { vec4 b[]; };
void main() { uint i = gl_GlobalInvocationID.x; b[i] = a[i] * 1.0001; }
"""

out = os.path.join(here, "shaders")
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
