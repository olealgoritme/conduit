#version 450
layout(location = 0) in vec4 a0;
layout(location = 1) in vec4 a1;
layout(location = 2) in vec4 a2;
layout(location = 3) in vec4 a3;
layout(location = 4) in vec4 a4;
layout(location = 5) in vec4 a5;
layout(location = 6) in vec4 a6;
layout(location = 7) in vec4 a7;
layout(location = 8) in vec4 a8;
layout(location = 9) in vec4 a9;
layout(location = 10) in vec4 a10;
layout(location = 11) in vec4 a11;
layout(location = 12) in uvec4 u0;
layout(location = 13) in ivec4 i0;
layout(location = 14) in uvec4 u1;
layout(location = 15) in ivec4 i1;
layout(std430, set = 0, binding = 0) buffer Out { uvec4 v[]; } o;
void main() {
   uint b = uint(gl_VertexIndex) * 16u;
   o.v[b + 0u] = floatBitsToUint(a0);
   o.v[b + 1u] = floatBitsToUint(a1);
   o.v[b + 2u] = floatBitsToUint(a2);
   o.v[b + 3u] = floatBitsToUint(a3);
   o.v[b + 4u] = floatBitsToUint(a4);
   o.v[b + 5u] = floatBitsToUint(a5);
   o.v[b + 6u] = floatBitsToUint(a6);
   o.v[b + 7u] = floatBitsToUint(a7);
   o.v[b + 8u] = floatBitsToUint(a8);
   o.v[b + 9u] = floatBitsToUint(a9);
   o.v[b + 10u] = floatBitsToUint(a10);
   o.v[b + 11u] = floatBitsToUint(a11);
   o.v[b + 12u] = u0;
   o.v[b + 13u] = uvec4(i0);
   o.v[b + 14u] = u1;
   o.v[b + 15u] = uvec4(i1);
   gl_PointSize = 1.0;
   gl_Position = vec4(0.0);
}
