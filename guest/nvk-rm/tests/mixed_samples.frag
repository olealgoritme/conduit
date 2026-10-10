#version 450
// Constant white: whatever arrives in the color attachment is the coverage
// modulation factor (or full coverage without modulation).
layout(location = 0) out vec4 color;

void main()
{
   color = vec4(1.0);
}
