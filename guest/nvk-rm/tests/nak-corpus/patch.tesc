#version 460
// Distance-based tessellation factors.
layout(vertices = 3) out;
layout(set = 0, binding = 0) uniform Frame { vec4 eye; float scale; };
void main() {
  gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;
  if (gl_InvocationID == 0) {
    float d = distance(eye.xyz, gl_in[0].gl_Position.xyz);
    float f = clamp(scale / max(d, 0.001), 1.0, 64.0);
    gl_TessLevelOuter[0] = f;
    gl_TessLevelOuter[1] = f;
    gl_TessLevelOuter[2] = f;
    gl_TessLevelInner[0] = f;
  }
}
