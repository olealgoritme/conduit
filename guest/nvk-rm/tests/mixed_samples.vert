#version 450
// One triangle over most of the lower half of the target; its slanted edge
// cuts pixels at every fraction, so every partial coverage count shows up.
void main()
{
   const vec2 p[3] = vec2[3](vec2(-1.0, 1.0), vec2(1.0, 1.0), vec2(-0.37, -1.0));
   gl_Position = vec4(p[gl_VertexIndex], 0.5, 1.0);
}
