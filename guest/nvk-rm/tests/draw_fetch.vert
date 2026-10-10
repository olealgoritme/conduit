#version 460
layout(location = 0) in uvec3 a_pos;   /* b0: per vertex, stride 12 */
layout(location = 1) in uint a_inst;   /* b1: per instance, stride 4, divisor 1 */
layout(location = 2) in uvec4 a_null;  /* b2: null, stride 0, per instance, divisor 0 */
layout(std430, set = 0, binding = 0) buffer Out { uint count; uint pad[3]; uvec4 rec[]; } o;
layout(push_constant) uniform PC { uint tag; uint voff; uint ioff; uint pad; } pc;
layout(constant_id = 0) const uint READ_BASES = 1u;
void main() {
   uint slot = atomicAdd(o.count, 1u);
   if (slot < 16384u) {
      uint bi = 0u, bv = 0u;
      if (READ_BASES != 0u) { bi = uint(gl_BaseInstance); bv = uint(gl_BaseVertex); }
      o.rec[slot * 3u + 0u] = uvec4(pc.tag, uint(gl_VertexIndex), uint(gl_InstanceIndex), bi);
      o.rec[slot * 3u + 1u] = uvec4(a_pos.x, a_pos.y, a_inst, bv);
      o.rec[slot * 3u + 2u] = a_null;
   }
   gl_PointSize = 1.0;
   gl_Position = vec4(0.0);
}
