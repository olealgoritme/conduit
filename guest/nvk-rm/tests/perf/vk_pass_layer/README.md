# VK_LAYER_CONDUIT_pass_profile

A Vulkan layer that measures GPU time per render pass, per run of
operations outside render passes and per command buffer, on any Vulkan
driver. It writes the same signatures and log lines as NVK's
`NVK_PASS_PROFILE` (see "Diagnostics" in `guest/nvk-rm/README.md`), so a
workload run on NVIDIA's driver lines up pass for pass with the same
workload on NVK. On a Linux host it also works under Proton (D3D11 through
DXVK, D3D12 through vkd3d-proton).

## Build

```
make -C guest/nvk-rm/tests/perf/vk_pass_layer
```

Builds `libVkLayer_conduit_pass_profile.so` and its loader manifest
`VkLayer_conduit_pass_profile.json` (with the absolute library path) in the
same directory. Needs a C compiler and the Vulkan headers
(`VULKAN_CORE`, default `/usr/include/vulkan/vulkan_core.h`, for the format
names).

## Run

```
VK_LAYER_PATH=<this directory> VK_INSTANCE_LAYERS=VK_LAYER_CONDUIT_pass_profile \
  CONDUIT_PASS_PROFILE_FILE=<dir>/nv <app>
```

For a Steam game, put the same variables before `%command%` in its launch
options.

| Variable | Meaning |
|---|---|
| `CONDUIT_PASS_PROFILE_FILE` | log file prefix; the log is `<path>.<pid>`; `-` is stderr; default `conduit-pass.<pid>` in the working directory |
| `CONDUIT_PASS_PROFILE_WINDOW_S` | seconds per log window (default 2) |
| `CONDUIT_PASS_PROFILE_LABELS` | 1 also times `VK_EXT_debug_utils` label regions outside render passes (D3D12 PIX events through vkd3d-proton with `VKD3D_CONFIG=debug_utils`) as `label:` lines |

What is timed:

- a render pass (`vkCmdBeginRendering` to `vkCmdEndRendering`, a resumed
  pass continues the suspended one), keyed by size, samples, layers, colour
  attachment count and the format and load/store ops of the first colour
  and the depth attachment;
- a run of operations of one kind outside render passes (dispatches, copies,
  fills, updates, clears, blits, resolves, query resets and copies), up to
  the next render pass, operation of another kind or the end of the command
  buffer, barriers included;
- every primary command buffer.

Each region is two bottom-of-pipe timestamps, so it waits for the work
before it: good for finding a pass that takes ten times as long, not for
measuring 1 %. The layer enables `hostQueryReset` on the device and a thread
collects the timestamps every 200 ms.

## Compare with NVK

Run the same scene with the layer on NVIDIA's driver and with
`NVK_PASS_PROFILE=1` on NVK, then:

```
./compare_pass_profiles.py --nvidia <dir>/nv.<pid> --nvk nvk-pass-<pid>.txt \
    --fps-nvidia 145 --fps-nvk 65
```

It sums the active windows of each log (`--skip`, `--windows A-B`, `--all`,
`--active` choose them), turns the totals into GPU ms per frame and prints
NVIDIA against NVK per pass signature, per operation signature and per
operation kind, then the command buffer / passes / operations / between
them summary. `--csv` also writes the table. Without `--fps-nvidia` the
frame count comes from the presents the layer counted.

## Smoke test

`layer_smoke_test.c` records render passes, fills, copies, an image clear
and a blit; it needs no GPU with lavapipe:

```
cc -o layer_smoke_test layer_smoke_test.c -lvulkan
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json \
  VK_LAYER_PATH=$PWD VK_INSTANCE_LAYERS=VK_LAYER_CONDUIT_pass_profile \
  CONDUIT_PASS_PROFILE_FILE=- ./layer_smoke_test
```
