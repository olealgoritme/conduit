#version 460
// Small if/else blocks (candidates for predication), comparisons combined
// with predicates, and discard.
layout(location = 0) in vec4 color;
layout(location = 1) in vec2 uv;
layout(location = 2) flat in uint flags;
layout(location = 0) out vec4 frag;
layout(set = 0, binding = 0) uniform sampler2D tex;
void main() {
  vec4 t = texture(tex, uv);
  vec4 c = color * t;
  if ((flags & 1u) != 0u && c.a < 0.5)
    c.rgb *= 0.5;
  else
    c.rgb += vec3(0.1);
  if (c.r > c.g || (flags & 2u) != 0u)
    c = c.bgra;
  if ((flags & 4u) != 0u && t.a < 0.01)
    discard;
  frag = clamp(c, 0.0, 1.0);
}
