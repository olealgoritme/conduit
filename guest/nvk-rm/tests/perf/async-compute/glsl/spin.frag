#version 450
#extension GL_EXT_shader_realtime_clock : require
layout(push_constant) uniform PC { uint ticks; } pc;
layout(location = 0) out vec4 color;
void main()
{
    uint s = clockRealtime2x32EXT().x;
    uint n = 0u;
    while (clockRealtime2x32EXT().x - s < pc.ticks)
        n++;
    color = vec4(float(n & 255u) / 255.0, 0.5, 0.25, 1.0);
}
