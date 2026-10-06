#version 450
/* vk_offscreen_test: one triangle from gl_VertexIndex, no vertex buffers */
layout(location = 0) out vec3 color;
void main()
{
    const vec2 pos[3] = vec2[](vec2(0.0, -0.75), vec2(-0.75, 0.75), vec2(0.75, 0.75));
    const vec3 col[3] = vec3[](vec3(1, 0, 0), vec3(0, 1, 0), vec3(0, 0, 1));
    gl_Position = vec4(pos[gl_VertexIndex], 0.0, 1.0);
    color = col[gl_VertexIndex];
}
