# Async compute on GB202 at the RM level

How NVIDIA's own Linux driver runs a Vulkan compute-only queue concurrently with the graphics queue on an
RTX 5090 (GB202), observed at the RM API, and what NVK on RM needs to do the same. Method: an `LD_PRELOAD` trace
of the RM escapes of our own test process (`guest/nvk-rm/tests/perf/async-compute/`), a Vulkan overlap test, and
creation-only RM allocations with `async_compute_probe` (`guest/rmclient/tests/async_compute_probe/`). No NVIDIA
binary was disassembled. Measured with driver 610.57.04 (open kernel module); source references are to
open-gpu-kernel-modules 615.78.08.

## Summary

- A compute object (`BLACKWELL_COMPUTE_B`, 0xcec0) on a channel of a non-zero VEID needs that channel on
  **runqueue 1**: `NV_CHANNEL_ALLOC_PARAMS.flags` bit 4 (`NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE_ONE`). NVIDIA
  allocates every compute-only channel with `flags = 0x10000010` (`GROUP_CHANNEL_RUNQUEUE_ONE |
  GROUP_CHANNEL_THREAD_ONE`). With `flags = 0` GSP-RM refuses the compute object with `NV_ERR_INVALID_OBJECT`.
  The VEID number does not matter (1, 8, 9 and 63 all work with the flags).
- The compute queue's channel sits in the **graphics queue's TSG**, on its own `FERMI_CONTEXT_SHARE_A`; NVIDIA uses
  `SPECIFIED` subctxIds 1, 2, 3, ... Only the compute class is allocated on it; a copy object is refused on a
  runqueue-1 channel.
- On this GPU the compute queue overlaps graphics queue 0 fully (100 %), two compute queues overlap fully, and
  queues in different TSGs (graphics queue 1 against the compute queue, or two graphics queues) are time-sliced
  (0 %).

## NVIDIA's RM objects for one graphics and one compute queue

All allocations use NVOS64 with `paramsSize = 0` (RM takes the per-class size from its resource table,
`src/nvidia/src/kernel/rmapi/alloc_free.c:138`), and the device's single `FERMI_VASPACE_A`.

```
TSG0  KEPLER_CHANNEL_GROUP_A {hVASpace, engineType GR}        driver-internal, exists with no queues
  SYNC ctxshare (VEID 0), one GR channel (2D, copy, I2M, 3D, compute); plus TSG-less COPY4/COPY5 channels
TSG1  KEPLER_CHANNEL_GROUP_A {hVASpace, engineType GR}        graphics queue 0
  ctxshare SYNC -> VEID 0
    channel {engineType GR, hContextShare, flags 0x0}            2D 0x902d, copy 0xcab5, I2M 0xcd40, 3D 0xce97, compute 0xcec0
  ctxshare SPECIFIED subctxId 1 -> VEID 1
    channel {engineType GR, hContextShare, flags 0x10000010}     compute 0xcec0 only
  ctxshare SPECIFIED subctxId 2 -> VEID 2                       compute queue 0
    channel {engineType GR, hContextShare, flags 0x10000010}     compute 0xcec0 only
TSG2  KEPLER_CHANNEL_GROUP_A                                   compute queue 0, standalone
  ctxshare SYNC -> VEID 0, one GR channel (2D, copy, I2M, 3D, compute)
```

Per channel, in order: alloc (`BLACKWELL_CHANNEL_GPFIFO_B` 0xca6f, 32768 GPFIFO entries) ->
`NVA06F_CTRL_CMD_BIND {GR}` -> `NVA06F_CTRL_CMD_GPFIFO_SCHEDULE {enable}` (per channel; the TSG-level NVA06C
schedule is not used) -> `NV_SEMAPHORE_SURFACE_CTRL_CMD_BIND_CHANNEL` ->
`NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX` -> `NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN` ->
objects (NULL params for 3D, compute, 2D, I2M; copy gets `NVB0B5_ALLOCATION_PARAMETERS {version 1, engineType}`)
-> `NV906F_CTRL_GET_CLASS_ENGINEID` per object -> `NV0090_CTRL_CMD_SET_LG_SECTOR_PROMOTION {1}`. VEID 0 channels
also get `NV2080_CTRL_CMD_GR_CTXSW_ZCULL_BIND`. There are no ctxshare (0x9067) or TSG (0xa06c) controls: no TPC
partition table, CWD watermark, timeslice or interleave changes.

The compute-only channel's parameters (`NV_CHANNEL_ALLOC_PARAMS`, 376 bytes): `hObjectError`, `gpFifoOffset`,
`gpFifoEntries 32768`, `flags 0x10000010`, `hContextShare`, `hVASpace 0`, `hUserdMemory[0]` + `userdOffset[0]`,
`engineType 1` (GR), `subDeviceId 0`, `internalFlags 0`, `tpcConfigID 0`. The graphics channel differs only in
`flags 0`.

With more queues:

| queues | allocation |
|---|---|
| 1 graphics, 0 compute | TSG1 still gets the VEID 1 compute-only channel |
| 1 graphics, n compute | per compute queue: a standalone TSG with a VEID 0 channel, and a compute-only channel in TSG1 on VEID 2 + j (VEIDs 2..9 for 8 queues) |
| n graphics | each graphics queue: its own TSG with VEID 0 and VEID 1 channels; all compute-queue channels go into the first graphics queue's TSG |

NVIDIA's queue families: 0 = 16 x graphics/compute/transfer, 1 = 2 x transfer, 2 = 8 x compute/transfer,
3-5 = video and optical flow.

## Overlap

`vkqtest` on NVIDIA's driver: a few SMs of busy-wait per queue (compute: 4 workgroups of 32 threads spinning on
the shader realtime clock; graphics: an 8x8-pixel draw whose fragment shader spins), `vkCmdWriteTimestamp` at
start and end on each queue (one device timestamp domain), both submissions released together by one timeline
semaphore. overlap = 1 - (makespan - longer solo) / shorter solo; CONCURRENT >= 75 %, SERIAL <= 25 %.

Two graphics and two compute queues, 8 ms of work per queue, medians of 7 runs:

| case | solo A / B (us) | makespan (us) | overlap | verdict |
|---|---|---|---|---|
| graphics q0 + compute q0 | 7992 / 8216 | 8219 | 100 % | CONCURRENT |
| compute q0 + compute q1 | 8216 / 8217 | 8231 | 100 % | CONCURRENT |
| graphics q0 + compute q1 | 7992 / 8216 | 8225 | 100 % | CONCURRENT |
| graphics q0 + graphics q1 | 7992 / 7992 | 16047 | 0 % | SERIAL |
| graphics q1 + compute q0 | 7992 / 8216 | 16347 | 0 % | SERIAL |

The same verdicts hold with 15 ms of work and with one queue of each kind. The last row shows that the compute
queue executes on its channel inside graphics queue 0's TSG (it is time-sliced against graphics queue 1), not on
its standalone TSG.

## What the compute object needs

`async_compute_probe --scenario matrix` against the host RM (native Linux build, allocations only, no GPU work).
Each case: one TSG, a VEID 0 channel A with 3D, compute and copy, then channel B with a compute object:

| case | B's ctxshare -> VEID | B's flags | result |
|---|---|---|---|
| `async-old` | ASYNC -> 63 | 0 | compute refused, `NV_ERR_INVALID_OBJECT` |
| `m-spec1-nochf` | SPECIFIED -> 1 | 0 | compute refused, `NV_ERR_INVALID_OBJECT` |
| `m-spec1-thr1` | SPECIFIED -> 1 | THREAD_ONE | channel refused, `NV_ERR_INVALID_ARGUMENT` |
| `m-spec1-rq1` | SPECIFIED -> 1 | RUNQUEUE_ONE | ok |
| `m-async-chf` | ASYNC -> 63 | 0x10000010 | ok |
| `m-spec8-chf` | SPECIFIED -> 8 | 0x10000010 | ok |
| `m-spec63-chf` | SPECIFIED -> 63 | 0x10000010 | ok |
| `m-lower-chf` | ASYNC_PREFER_LOWER -> 1 | 0x10000010 | ok |
| `async3d` | SPECIFIED -> 1 | 0x10000010 | 3D refused, `NV_ERR_INVALID_OBJECT` |
| `m-chf-copy` | SPECIFIED -> 1 | 0x10000010 | compute ok; copy refused, `NV_ERR_INVALID_STATE` |
| `m-bonly` (no channel A) | SPECIFIED -> 1 | 0x10000010 | ok |

## Open RM source references

| location (615.78.08) | relevance |
|---|---|
| `src/common/sdk/nvidia/inc/alloc/alloc_channel.h:122-133` | `NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE` 4:4: the runqueue a TSG channel executes on; channels on different runqueues may feed the engine simultaneously |
| `alloc_channel.h:264-280` | `NVOS04_FLAGS_GROUP_CHANNEL_THREAD` 29:28: thread id inside the TSG; not all classes are supported at thread > 0 |
| `src/nvidia/src/kernel/gpu/fifo/arch/maxwell/kernel_channel_gm107.c:193-197` | channel allocation validates (thread, runqueue) and returns `NV_ERR_INVALID_ARGUMENT` |
| `src/nvidia/src/kernel/gpu/fifo/arch/blackwell/kernel_fifo_gb202.c:242-258` | GB202: thread 1 on runqueue 0 is invalid |
| `src/nvidia/src/kernel/gpu/fifo/kernel_channel.c:217` | the channel's runqueue is taken from the flags |
| `kernel_channel.c:700-710` | without `hContextShare` the thread field selects the TSG's legacy SYNC/ASYNC ctxshare |
| `src/nvidia/src/kernel/gpu/fifo/kernel_ctxshare.c:401-438` | ctxshare flags: SYNC = VEID 0; ASYNC = highest free VEID first (63); ASYNC_PREFER_LOWER = lowest free from 1; SPECIFIED = the given subctxId |
| `kernel_ctxshare.c:547-552` | ASYNC_PREFER_LOWER reports in bit 31 whether the VEID is below the "max lower subcontext" |
| `src/nvidia/generated/g_kernel_fifo_nvoc.c:587-596`, `g_kernel_fifo_nvoc.h:2579-2581` | max lower subcontext is 8 on GB20x; it does not limit which VEIDs accept compute |
| `src/nvidia/src/kernel/gpu/fifo/arch/volta/kernel_fifo_gv100.c:181-205, 257-260` | subcontexts per TSG come from GR info (64 on GB202); SPECIFIED and PREFER_LOWER count as ASYNC |
| `src/common/sdk/nvidia/inc/nvos.h:3190-3247` | `NV_CTXSHARE_ALLOCATION_PARAMETERS` and its flags |
| `src/nvidia/src/kernel/gpu/fifo/channel_descendant.c:133-144` | a copy object on a GR channel needs exactly one GRCE partner for the channel's runqueue, else `NV_ERR_INVALID_STATE` (the refused copy object on a runqueue-1 channel) |
| `src/nvidia/src/kernel/gpu/gr/kernel_graphics_object.c:173-228, 286-361` | kernel side of a GR object allocation; it has no VEID or runqueue check |
| `src/nvidia/src/kernel/rmapi/resource_list.h:2272-2280`, `resource_desc_flags.h:45`, `alloc_free.c:880-901` | compute objects are forwarded to GSP-RM (`RS_FLAGS_ALLOC_RPC_TO_ALL`); the `NV_ERR_INVALID_OBJECT` for compute on a runqueue-0 channel of a non-zero VEID comes from GSP-RM, whose source is not public |
| `src/common/sdk/nvidia/inc/ctrl/ctrl2080/ctrl2080fifo.h:131-150` | `NV2080_CTRL_FIFO_INFO_INDEX_MAX_LOWER_SUBCONTEXT` (8 on the RTX 5090; 64 subcontexts per TSG) |

## Recipe for NVK on RM

1. Keep one TSG per graphics queue with the VEID 0 channel (3D, compute, copy).
2. For each compute queue, in the TSG of the device's first graphics queue: allocate
   `FERMI_CONTEXT_SHARE_A {hVASpace, flags SPECIFIED, subctxId 2 + j}` (ASYNC or ASYNC_PREFER_LOWER also work),
   then the GPFIFO channel `{engineType GR, hContextShare, flags NVOS04_FLAGS_GROUP_CHANNEL_RUNQUEUE_ONE |
   NVOS04_FLAGS_GROUP_CHANNEL_THREAD_ONE (0x10000010)}`, BIND GR, schedule, work-submit token, and the compute
   class with NULL params.
3. No copy object on that channel: copies on a compute queue need compute shaders or a separate copy-engine
   channel.
4. No 3D on a non-zero VEID. A queue in its own TSG is only time-sliced against the others.

## Reproducing

- RM trace and overlap test: `guest/nvk-rm/tests/perf/async-compute/README.md`.
- RM allocation matrix and the channel-level concurrency test: `guest/rmclient/tests/async_compute_probe/README.md`.
