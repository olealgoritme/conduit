# dispatch-bench

Per-dispatch cost of tiny compute dispatches, in GPU time. One command buffer
holds N dispatches (default 1000) between two timestamps; the GPU time
divided by N is the cost of one dispatch plus whatever the pattern puts
between dispatches. It runs on any Vulkan driver, so the same GPU can be
measured under NVIDIA's driver on the host and under NVK on RM in a guest.

## Build

```
guest/nvk-rm/tests/perf/dispatch-bench/build.sh [OUT_DIR]
```

Builds `dispatch-bench` (Linux) and `dispatch-bench.exe` (Windows, when
`x86_64-w64-mingw32-gcc` is installed) into `OUT_DIR` (default
`./dispatch-bench-out`), with `run-guest.cmd` next to the exe. Needs
`glslangValidator`, `xxd` and the Vulkan headers (`VULKAN_INCLUDE`, default
`/usr/include`). Both binaries load the Vulkan loader at run time.

## Run

```
dispatch-bench [-n N] [-r reps] [-q u|c] [--dev STR] [--grids 1,4,64,1024] [--csv file]
```

| Option | Meaning |
|---|---|
| `-n` | dispatches per command buffer (1000) |
| `-r` | measured repetitions; the median is reported (15, after 3 warm-up runs) |
| `-q u` / `-q c` | graphics+compute queue family / compute-only queue family |
| `--dev` | substring of the device name; default the first NVIDIA, NVK or Helios device |
| `--grids` | workgroup counts per dispatch (local size 64x1x1) |
| `--csv` | also write the table as tab-separated values |

In a Windows guest, copy the `OUT_DIR` contents and run `run-guest.cmd` from
`cmd.exe`: both queue families with the driver defaults, then with
`NVK_RM_UPLOAD_VRAM=0`, into `guest-*.txt` and `guest-*.tsv`.

Patterns between dispatches:

| Pattern | Between dispatches |
|---|---|
| `none` | nothing (bind once, back-to-back dispatches) |
| `barrier` | compute-to-compute memory barrier |
| `push` | `vkCmdPushConstants` (vkd3d-proton root constants) |
| `bind` | `vkCmdBindDescriptorSets` alternating two sets, plus push constants |
| `pipe` | the other of two pipelines (same shader, other specialization constant) |
| `bpush` | barrier plus push constants |
| `vbar` | the barrier vkd3d-proton records for a D3D12 UAV barrier (all shader stages and transfer), plus push constants |
| `ts` | a bottom-of-pipe timestamp after every dispatch (what a per-dispatch profiler adds) |

Shaders: `simple` (one storage buffer) and `rich` (push constants, two
storage buffers and a uniform buffer, like a vkd3d-proton compute root
signature).

## Output

```
shader  pattern   grid |  gpu_us/d   gpu_min |  subm2fence    record
-       empty        - |     0.320     0.320 |       7.000     0.700   (whole command buffer, us)
rich    none      1024 |     0.486     0.479 |       0.551     0.225
```

- `gpu_us/d`: GPU timestamp delta divided by N, median of the repetitions;
  `gpu_min`: the fastest repetition.
- `subm2fence`: CPU time from `vkQueueSubmit` to the fence, divided by N.
- `record`: CPU time to record the command buffer, divided by N.
- `empty`: a command buffer with the two timestamps only, in microseconds for
  the whole command buffer.

## Results (RTX 5090)

GPU microseconds per dispatch, `rich` shader, graphics+compute queue:

| Pattern, grid | NVIDIA driver (host) | NVK on RM, guest (defaults) | NVK on RM, `NVK_RM_UPLOAD_VRAM=0` |
|---|---:|---:|---:|
| `none`, 1 | 0.046 | 0.045 | 0.056 |
| `none`, 64 | 0.046 | 0.046 | 3.09 |
| `none`, 1024 | 0.485 | 0.480 | 3.59 |
| `push`, 1024 | 0.485 | 0.479 | 3.67 |
| `bind`, 1024 | 0.486 | 0.485 | 3.66 |
| `barrier`, 1024 | 1.41 | 2.98 | 12.0 |
| `vbar`, 1024 | | 2.98 | 12.0 |

With the compute root descriptor tables in CPU-mapped VRAM (the Windows
default), back-to-back dispatches cost what they cost under NVIDIA's driver.
From system memory every dispatch waits for its root table to come across
PCIe and the guest's IOMMU. Barriers between dispatches cost about twice
NVIDIA's time: NVK drains the compute pipe with a WAIT_FOR_IDLE per barrier.
