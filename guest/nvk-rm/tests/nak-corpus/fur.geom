#version 460
// A geometry shader that emits a variable number of primitives.
layout(triangles) in;
layout(triangle_strip, max_vertices = 12) out;
layout(location = 0) in vec3 in_normal[];
layout(location = 0) out vec4 shade;
layout(set = 0, binding = 0) uniform Frame { mat4 view_proj; float len; uint shells; };
void main() {
  for (uint s = 0u; s < min(shells, 4u); s++) {
    for (int k = 0; k < 3; k++) {
      vec4 p = gl_in[k].gl_Position + vec4(in_normal[k] * len * float(s), 0.0);
      shade = vec4(float(s) / 4.0);
      gl_Position = view_proj * p;
      EmitVertex();
    }
    EndPrimitive();
  }
}
