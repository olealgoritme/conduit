#version 450
layout(location = 0) in uvec4 a_vtx;   /* binding 0: null, stride 16, per vertex */
layout(location = 1) in uvec4 a_inst;  /* binding 1: null, stride 0, per instance, divisor 0 */
layout(location = 2) in uvec4 a_off;   /* binding 1, offset 2032 */
layout(std430, set = 0, binding = 0) buffer Out { uvec4 v[]; } o;
layout(push_constant) uniform PC { uint phase; uint mode; } pc;
void main() {
   uint base = pc.phase * 260u;
   o.v[base + uint(gl_VertexIndex)] = a_vtx;
   if (gl_VertexIndex == 0) {
      o.v[base + 256u] = a_inst;
      o.v[base + 257u] = a_off;
   }
   gl_PointSize = 1.0;
   gl_Position = vec4(0.0);
}
