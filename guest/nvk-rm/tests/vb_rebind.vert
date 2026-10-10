#version 460
layout(location = 0) in uvec3 a_pos;   /* b0: per vertex, stride 12 */
layout(location = 1) in uint a_inst;   /* b1: per instance, stride 4, divisor 1 */
layout(location = 2) in uvec4 a_b2;    /* b2: null or real, rate/stride per pipeline */
layout(location = 3) in vec4 a_b2f;    /* b2 again as RGBA32F (CS2's character draws) */
layout(std430, set = 0, binding = 0) buffer Out { uint count; uint pad[3]; uvec4 rec[]; } o;
layout(push_constant) uniform PC { uint tag; uint pad0; uint pad1; uint pad2; } pc;
void main() {
   uint slot = atomicAdd(o.count, 1u);
   if (slot < 16384u) {
      o.rec[slot * 4u + 0u] = uvec4(pc.tag, uint(gl_VertexIndex), uint(gl_InstanceIndex),
                                    uint(gl_BaseInstance));
      o.rec[slot * 4u + 1u] = uvec4(a_pos.x, a_inst, uint(gl_BaseVertex), 0u);
      o.rec[slot * 4u + 2u] = a_b2;
      o.rec[slot * 4u + 3u] = floatBitsToUint(a_b2f);
   }
   gl_PointSize = 1.0;
   gl_Position = vec4(0.0);
}
