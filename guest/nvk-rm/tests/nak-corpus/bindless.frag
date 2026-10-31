#version 460
// Descriptor-indexed textures with uniform and non-uniform indices, as
// vkd3d-proton and DXVK's descriptor heaps use them.
#extension GL_EXT_nonuniform_qualifier : require
layout(location = 0) in vec2 uv;
layout(location = 1) flat in uint material;
layout(location = 0) out vec4 frag;
layout(set = 0, binding = 0) uniform texture2D textures[];
layout(set = 1, binding = 0) uniform sampler samplers[16];
layout(push_constant) uniform PC { uint base; uint sampler_index; };
void main() {
  vec4 a = texture(sampler2D(textures[base], samplers[sampler_index]), uv);
  vec4 b = texture(sampler2D(textures[nonuniformEXT(base + material)], samplers[0]), uv * 2.0);
  vec4 c = textureLod(sampler2D(textures[base + 1u], samplers[1]), uv, 2.0);
  frag = a * b + c;
}
