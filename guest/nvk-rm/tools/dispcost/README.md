# dispcost: GPU time per dependent compute dispatch

One command buffer holds N dependent compute dispatches (each invocation
reads and writes its own word of one buffer). It is timestamped only before
the first dispatch and after the last, so per-dispatch timing does not change
the stream (`NVK_DISPATCH_PROFILE` idles the GPU around every dispatch). The
value printed is `(T(N) - T(empty)) / N` in microseconds, the median over R
submits. The same binary runs on NVIDIA's driver on the host and on NVK on RM
in the guest.

`tests/perf/dispatch-bench` measures independent dispatches and per-dispatch
state changes. dispcost covers the barrier variants, dependent dispatches,
shader size and indirect dispatch.

## Build

```sh
guest/nvk-rm/tools/dispcost/build.sh [OUT_DIR]     # dispcost and dispcost.exe
```

`dispcost_spv.h` (the three builds of `dispcost.comp` as SPIR-V words) is in
the tree. When `glslangValidator` is installed, build.sh regenerates it. The
.exe loads `vulkan-1.dll` at run time.

## Run

```
dispcost [-g GPU_NAME_SUBSTRING] [-n DISPATCHES] [-r SUBMITS] [-q gfx|compute] [-f ROW_FILTER]
```

The defaults are `-n 256` and `-r 15`, on the graphics queue family. On
Windows, `VK_DIRECT_DRIVER=path\to\vulkan_nouveau.dll` passes that ICD to the
loader as the only driver (see `tests/vk_direct_driver.h`).

Each column is the command recorded between consecutive dispatches:

| Column | Between dispatches |
|---|---|
| `none` | nothing (the dispatches may overlap) |
| `mem1` | `vkCmdPipelineBarrier` COMPUTE->COMPUTE, `VkMemoryBarrier` SHADER_WRITE -> SHADER_READ |
| `buf1` | the same as a `VkBufferMemoryBarrier` on the data buffer |
| `mem2` | `vkCmdPipelineBarrier2` COMPUTE->COMPUTE, SHADER_STORAGE_WRITE -> SHADER_STORAGE_READ |
| `vkd3d` | the barrier vkd3d-proton records for a D3D12 UAV barrier on a graphics queue: stages PRE_RASTERIZATION_SHADERS, FRAGMENT_SHADER, COMPUTE_SHADER and CLEAR; access SHADER_READ, SHADER_WRITE and TRANSFER_WRITE on both sides |
| `full` | ALL_COMMANDS MEMORY_WRITE -> ALL_COMMANDS MEMORY_READ, MEMORY_WRITE |
| `vkd3d+pc` | `vkd3d` plus `vkCmdPushConstants` before every dispatch |
| `ts` | no barrier; a BOTTOM_OF_PIPE timestamp after every dispatch (what a per-dispatch profiler adds) |
| `cpu` | CPU microseconds per dispatch to record the `vkd3d+pc` stream |

Each row is a combination of:

- shader: `tiny` (about 10 instructions), `mid` (about 100: integer chain
  plus 6 loads) or `big` (about 450: chain plus 27 loads)
- grid: 1, 5, 64 or 1024 workgroups of 64 invocations
- `direct` (`vkCmdDispatch`) or `indirect` (`vkCmdDispatchIndirect`)

When the driver has VK_KHR_pipeline_executable_properties, its shader
statistics are printed first.
