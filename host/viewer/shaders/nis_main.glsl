// SPDX-License-Identifier: GPL-2.0 OR Apache-2.0
layout(local_size_x = NIS_THREAD_GROUP_SIZE) in;
void main()
{
    NVScaler(gl_WorkGroupID.xy, gl_LocalInvocationID.x);
}
