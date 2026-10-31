#version 460
// Displaced triangle patches.
layout(triangles, fractional_odd_spacing, ccw) in;
layout(set = 0, binding = 0) uniform Frame { mat4 view_proj; };
layout(set = 0, binding = 1) uniform sampler2D height;
void main() {
  vec4 p = gl_TessCoord.x * gl_in[0].gl_Position + gl_TessCoord.y * gl_in[1].gl_Position +
           gl_TessCoord.z * gl_in[2].gl_Position;
  p.y += textureLod(height, p.xz, 0.0).r;
  gl_Position = view_proj * p;
}
