#version 460
// Matrix palette skinning with per-instance data from a storage buffer.
layout(location = 0) in vec3 pos;
layout(location = 1) in vec3 normal;
layout(location = 2) in uvec4 bones;
layout(location = 3) in vec4 weights;
layout(location = 0) out vec3 out_normal;
layout(location = 1) out vec4 out_world;
layout(set = 0, binding = 0) uniform Frame { mat4 view_proj; vec4 light; };
layout(set = 0, binding = 1, std430) readonly buffer Bones { mat4 palette[]; };
layout(set = 0, binding = 2, std430) readonly buffer Instances { mat4 world[]; };
void main() {
  mat4 skin = palette[bones.x] * weights.x + palette[bones.y] * weights.y +
              palette[bones.z] * weights.z + palette[bones.w] * weights.w;
  vec4 w = world[gl_InstanceIndex] * (skin * vec4(pos, 1.0));
  out_world = w;
  out_normal = normalize(mat3(world[gl_InstanceIndex]) * (mat3(skin) * normal));
  gl_Position = view_proj * w;
}
