#version 450
/* vk_scanout_present: a triangle from gl_VertexIndex, turned by angle and
 * corrected for the aspect ratio of the swapchain image */
layout(push_constant) uniform Push {
    float angle;
    float aspect; /* height / width */
} pc;
layout(location = 0) out vec3 color;
void main()
{
    const vec2 pos[3] = vec2[](vec2(0.0, -0.8), vec2(-0.69, 0.4), vec2(0.69, 0.4));
    const vec3 col[3] = vec3[](vec3(1, 0, 0), vec3(0, 1, 0), vec3(0, 0, 1));
    float c = cos(pc.angle), s = sin(pc.angle);
    vec2 p = mat2(c, s, -s, c) * pos[gl_VertexIndex];
    gl_Position = vec4(p.x * pc.aspect, p.y, 0.0, 1.0);
    color = col[gl_VertexIndex];
}
