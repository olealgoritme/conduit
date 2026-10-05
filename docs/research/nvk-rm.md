# NVK on RM: what an RM backend for NVK needs

Research for Route C of `docs/research/windows-thin-path.md` (NVK talking RM
instead of nouveau), first in a **Linux** Conduit guest. Written 2026-10-05
against Mesa main `70c4c01` (26.3.0-devel, cloned to `~/code/mesa-nvk-rm`),
open-gpu-kernel-modules 610.57.04 (`~/code/ogkm-610.57.04`, the host driver),
Linux 7.2.9 nouveau (`~/code/nvgpu-lab/linux-7.2.9`) and Conduit `dc3b963`.
Paths below are relative to those trees: `mesa:`, `ogkm:`, `linux:`, and
repo-relative for Conduit.

## Summary

- **The forwarder already allows everything the core path needs.** Every class
  and control in the minimal sequence (root client to doorbell, Blackwell
  GB202 classes included) is in the generated 610.57.04 allowlist, and the
  backend runs as a non-root user, so RM's own runtime privilege checks still
  apply. No allowlist change is needed for vkcube or a compute shader. The
  only gate is `--caps graphics` (on by default): NVK allocates the 3D class
  even for compute-only queues.
- **NVK supports GB202 on upstream main** (`BLACKWELL_B` is on the conformant
  list) with QMD v5 and SM 120 in NAK. Nothing Blackwell-specific is missing
  on the Mesa side.
- **The rmclient API contract needs three additions** (§7): `MAP_MEMORY_DMA` /
  `UNMAP_MEMORY_DMA` (the GPU VA bind, the core of the backend), OS events (a
  pollable fd per wait source, plus `GET_EVENT_DATA`), and one fresh
  `/dev/nvidiaN` fd per CPU mapping (RM and Conduit both refuse a second
  mapping on the same fd).
- **VA model fits.** NVK picks every GPU VA itself (util_vma_heap). RM allows
  that: `NV50_MEMORY_VIRTUAL` with `FIXED_ADDRESS_ALLOCATE` reserves a range,
  and `NvRmMapMemoryDma` with `DMA_OFFSET_FIXED` and `PAGE_KIND_OVERRIDE` binds
  memory at an offset with NVK's PTE kind. Allocating the VA space with
  `VA_INTERNAL_LIMIT` keeps RM's own buffers in [4 GiB, 4.5 GiB), so NVK's heap
  only has to avoid that hole.
- **Submission is userspace-only:** NVK writes its own GPFIFO ring and USERD
  `GP_PUT`, then writes the work-submit token to the usermode doorbell
  (`BLACKWELL_USERMODE_A` + 0x90). No ioctl per submit.
- **Sync is the main piece of new design.** nouveau uses DRM syncobjs. The RM
  backend needs its own `vk_sync_type`: a 64-bit memory timeline released by
  the GPU (host `SEM_*` methods), waited on by the CPU through an RM OS event
  (non-stall interrupt) and by the GPU through semaphore acquire.
- **The Conduit-specific risk is the 1 GiB shared window.** Every CPU mapping
  of host-allocated memory (VRAM BAR1 or `NV01_MEMORY_SYSTEM`) is placed there.
  NVK keeps many persistent maps. Put mappable GART memory in guest pages
  registered as `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` (zero-copy, no window cost)
  and report no host-visible VRAM at first.

## 1. What NVK needs from its kernel layer (nvkmd)

NVK reaches the kernel only through `nvkmd` (`mesa:src/nouveau/vulkan/nvkmd/nvkmd.h`,
628 lines). The one implementation is `nvkmd/nouveau/` (about 1,500 lines
including `src/nouveau/winsys`). An RM backend is a sibling directory
`nvkmd/rm/` plus a dispatch line in `nvkmd.c:86-93`
(`nvkmd_try_create_pdev_for_drm`, which today calls nouveau only).

### Objects and operations

| op | nvkmd.h | what it must do | nouveau impl |
|---|---|---|---|
| `nvkmd_try_create_pdev_for_drm` | 382-386 | Probe a `drmDevice`. Fill `nv_device_info` (chipset, SM, classes, GPC/TPC, VRAM, BAR, zcull), `kmd_info` (feature bits), `bind_align_B`, `drm.render_dev`/`primary_dev`, `sync_types[]`. Return `VK_ERROR_INCOMPATIBLE_DRIVER` if not ours. | `nvkmd_nouveau_pdev.c:35-140`; device info `winsys/nouveau_device.c:475-614` |
| `pdev->destroy` | 128 | Free. | pdev.c:142-152 |
| `pdev->get_vram_used` | 130 | Bytes of VRAM in use (budget ext). Only if `kmd_info.has_get_vram_used`. | GETPARAM VRAM_USED |
| `pdev->get_drm_primary_fd` | 132 | Optional (WSI display). May be NULL. | pdev.c:162-186 |
| `pdev->create_dev` | 134 | Make a per-VkDevice `nvkmd_dev`: set `va_start`/`va_end`, init `mems` list. | dev.c:13-63 |
| `dev->destroy` | 163 | | dev.c:65-75 |
| `dev->get_gpu_timestamp` | 165 | GPU ns timestamp (`vkGetCalibratedTimestamps`, queries). | GETPARAM PTIMER_TIME |
| `dev->get_drm_fd` | 167 | Optional; `nvk_device.c:260` passes it to `vk_device_set_drm_fd`. May be NULL. | dev.c:85-91 |
| `dev->alloc_mem` | 169-173 | Allocate memory with a placement (`LOCAL`/`GART`/`VRAM`, exactly one), `CAN_MAP`, `SHARED`, `COHERENT`. **Also allocate a VA and bind the whole BO there**, so `mem->va` is valid: nouveau does this in `create_mem_or_close_bo`, mem.c:25-81 (VA alloc 56-62, bind 64-66). | mem.c:13-23 → 105-177 |
| `dev->alloc_tiled_mem` | 175-180 | As above with `pte_kind`, `tile_mode`. Only if `kmd_info.has_alloc_tiled` (enables DRM format modifiers). | mem.c:105-177 |
| `dev->import_dma_buf` | 182-184 | Only if `has_dma_buf`. | mem.c:179-212 |
| `dev->alloc_va` | 186-190 | Reserve `[addr, addr+size)` from NVK's own heap (`util_vma_heap`, dev.c:50-58: main heap `[4 KiB, 2^38)`, replay heap `[2^38, 2^39)`). Honour `ALLOC_FIXED` (capture/replay), `REPLAY`, `SPARSE` (back the range with sparse PTEs: reads 0, writes dropped), `GART`. Store `pte_kind` on the VA: **the kind belongs to the VA binding, not to the BO** (va.c:229, ctx.c:392). | va.c:114-172 |
| `dev->create_ctx` | 192-195 | `engines == BIND`: a VM-bind queue. Otherwise an exec context (a channel) with the given engine classes. | ctx.c:459-471 |
| `mem->free` | 212 | Unbind and free its VA, free BO. | mem.c:214-222 |
| `mem->map` | 214-218 | CPU map (`RD`/`WR`, `CLIENT` vs internal, `FIXED` at `fixed_addr`). Refcounting is in core (`nvkmd.c`). | mmap of the GEM handle, mem.c:224-252 |
| `mem->unmap` | 220-222 | munmap. | mem.c:254-262 |
| `mem->overmap` | 224-227 | Replace a client map with PROT_NONE anon (`VK_EXT_map_memory_placed`). Only if `has_overmap`. | mem.c:264-282 |
| `mem->sync_to_gpu/from_gpu` | 229-233 | Cache maintenance for non-coherent maps. NULL on dGPU (all maps coherent). | not set |
| `mem->export_dma_buf` | 235-237 | Only if `has_dma_buf`. | mem.c:284-297 |
| `mem->log_handle` | 240 | Any integer for `NVK_DEBUG=vm` logs. | bo handle |
| `va->free` | 273 | Unmap everything in range (and sparse), release heap range. | va.c:174-207 |
| `va->bind_mem` | 275-280 | Map `mem[mem_offset, +range)` at `va->addr + va_offset` with `va->pte_kind`. Synchronous. Core asserts alignment to `mem->bind_align_B` (nvkmd.c:232-247). | va.c:209-232 |
| `va->unbind` | 282-285 | Unmap range (back to sparse if the VA is sparse). | va.c:234-249 |
| `ctx->destroy` | 323 | | |
| `ctx->wait` | 325-328 | Queue waits on `vk_sync`s (with timeline values) before the next work. | ctx.c:115-134 |
| `ctx->exec` | 330-333 | Queue pushbuf ranges `{addr, size_B, incomplete, no_prefetch}` (nvkmd.h:298-307). An `incomplete` push must go in the same submission as the next one. Max size < 2^23 B, 4-byte aligned (ctx.c:198-200). | ctx.c:165-216 |
| `ctx->bind` | 335-338 | Bind ctx only: a batch of `{BIND/UNBIND, va, va_offset, mem, mem_offset, range}` executed in order after the waits (sparse binding). | ctx.c:375-428 |
| `ctx->signal` | 341-344 | Queue signals, then flush. | ctx.c:218-237 |
| `ctx->flush` | 346-347 | Submit what is queued. | ctx.c:136-163 |
| `ctx->sync` | 350-351 | Flush and wait until the context is idle. Report `VK_ERROR_DEVICE_LOST` on a channel error. | ctx.c:239-280 |

### How NVK uses them (so the backend knows what matters)

- **Engines per queue** (`mesa:nvk_queue.h:63-82`): a graphics queue needs 3D +
  COMPUTE; a compute queue needs COMPUTE + **3D** ("We currently rely on 3D
  engine MMEs for indirect dispatch"); a transfer queue needs COPY
  (`has_transfer_queue`, a separate CE channel). The push streams bind classes
  themselves with `SET_OBJECT` on fixed subchannels (`nv_push.h`: 3D=0,
  compute=1, M2MF=2, 2D=3, copy=4; `nvk_cmd_draw.c:150`,
  `nvk_cmd_dispatch.c:37`). The kernel layer only has to allocate those engine
  objects on the channel so RM creates their context state. 2D and M2MF are not
  used on Kepler+ (`nvk_queue.c:338-349`).
- **Submit path** (`nvk_queue.c:172-282`): `wait` → `exec` per command buffer
  → `signal`. Sparse binds go to the bind ctx (`nvk_queue.c:172-206`).
- **Syncs:** `pdev->sync_types[0]` must support `VK_SYNC_FEATURE_TIMELINE`
  (`nvk_mem_stream.c:61-62`) and is NVK's only sync type
  (`nvk_physical_device.c:1727`). Upload and push streams CPU-wait on it
  (`nvk_upload_queue.c:117`, `nvk_mem_stream.c:279`).
- **Device info** comes from the kernel layer: `chipset` (SM via
  `sm_for_chipset`, `nouveau_device.c:53-70`: `>= 0x1b0` is SM 120), class
  numbers picked as "highest of each type" (`nouveau_context.c:67-80`),
  `gpc_count`/`tpc_count`, `vram_size_B`, `bar_size_B`, zcull info. Struct:
  `mesa:src/nouveau/headers/nv_device_info.h`.
- **Enumeration** is by DRM device (`nvk_instance.c:164`,
  `nvk_physical_device.c:1519-1535`). In a Conduit guest `renderD128` is a PCI
  device `10de:2b85` (GB202) served by the Conduit module, so NVK's normal
  enumeration reaches `nvkmd_try_create_pdev_for_drm`. The RM backend accepts it
  when the DRM driver is not nouveau and `/dev/nvidiactl` opens (check
  `drmGetVersion()->name`). Under Windows there is no DRM: that port will use
  `instance->vk.physical_devices.enumerate` instead.
- **WSI** (`nvk_wsi.c:20-40`) uses dma-buf for X11/Wayland present. Without
  `has_dma_buf`, set `.sw_device = true` there (one line) so Mesa WSI copies
  into host-visible memory and presents with `xcb_put_image` / `wl_shm`. This
  is slow but enough for vkcube.

### nouveau kernel contract NVK assumes (to replicate or drop)

| nouveau feature | where | RM backend |
|---|---|---|
| VM_INIT, kernel VA reservation `[2^39, 2^40)` | `nouveau_device.h:18`, `nouveau_device.c:509-514` | `FERMI_VASPACE_A` with `VA_INTERNAL_LIMIT` (§2.3) |
| VM_BIND sync and async (with syncobj waits and signals) | va.c:97-112, ctx.c:291-457 | `MapMemoryDma`/`UnmapMemoryDma` from the CPU after the waits are met (§2.3, §2.6) |
| EXEC: pushes, waits, signals in one ioctl | ctx.c:136-163 | userspace GPFIFO + doorbell + semaphore methods (§2.5) |
| DRM syncobj (binary and timeline) | pdev.c:132-135 | a custom `vk_sync_type` (§2.6) |
| channel kill detection by an empty EXEC | ctx.c:268-277 | the error notifier in `hObjectError` (§2.7) |

## 2. RM equivalents

Headers: `ogkm:src/common/sdk/nvidia/inc/` (`nvos.h`, `class/`, `ctrl/`,
`alloc/alloc_channel.h`). "Allowed" = in
`host/backend/gen/src/rmallow/v610_57_04.rs` (parameter size in bytes).

### 2.1 Opening, device information

| step | RM | allowed |
|---|---|---|
| open | `/dev/nvidiactl`: `NV_ESC_CHECK_VERSION_STR`, `NV_ESC_CARD_INFO`; `/dev/nvidia0`: `NV_ESC_REGISTER_FD`, `NV_ESC_ATTACH_GPUS_TO_FD` | escapes, ABI-checked |
| root client | `NV01_ROOT_CLIENT` (0x41), params `NvHandle` | yes (4) |
| device | `NV01_DEVICE_0` (0x80), `NV0080_ALLOC_PARAMETERS` (`deviceId`, `hClientShare`, `vaMode`, ...) | yes (56) |
| subdevice | `NV20_SUBDEVICE_0` (0x2080) | yes (4) |
| GPU list | `NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS` 0x201, `..._GET_ID_INFO_V2` 0x205, `..._GET_PROBED_IDS` 0x214 | yes |
| classes | `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2` 0x00800292 → pick highest `*97`, `*c0`, `*b5`, `*6f`, `*61` | yes (804) |
| chipset | `NV2080_CTRL_CMD_MC_GET_ARCH_INFO` 0x20801701 (`architecture` \| `implementation` → 0x1b2 for GB202) | yes (16) |
| PCI ids, BAR | `NV2080_CTRL_CMD_BUS_GET_PCI_INFO` 0x20801801, `..._GET_PCI_BAR_INFO` 0x20801803 | yes |
| name | `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` 0x20800110 | yes |
| VRAM size, free | `NV2080_CTRL_CMD_FB_GET_INFO_V2` 0x20801303 (`RAM_SIZE`, `HEAP_FREE`; the backend rewrites these to `--vram-limit-mib`, `device/src/nvidia/vidmem.rs`) | yes (1028) |
| GPC/TPC | `NV2080_CTRL_CMD_GR_GET_GPC_MASK` 0x2080122a, `..._GET_TPC_MASK` 0x2080122b, `..._GR_GET_INFO_V2` 0x20801228 | yes |
| zcull | `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` 0x20801206 (nouveau's `drm_nouveau_get_zcull_info` mirrors it) | yes (40) |
| engines | `NV2080_CTRL_CMD_GPU_GET_ENGINES_V2` 0x20800170, `NV2080_CTRL_CMD_CE_GET_ALL_CAPS` 0x20802a0a (find a GRCE for the GR channel and an async CE for the transfer queue) | yes |
| timestamp | `NV2080_CTRL_CMD_TIMER_GET_TIME` 0x20800403 (or read `NVC361_TIME_0/1` at usermode+0x80, which needs a BAR0 usermode map; a BAR1 map is write-only, `nvidia-push-init.c:964-970`) | yes (8) |
| compbits (later) | `NV0080_CTRL_CMD_FB_GET_COMPBIT_STORE_INFO` 0x00801306 | yes |

### 2.2 Memory

| nvkmd | RM | notes |
|---|---|---|
| VRAM | `NV01_MEMORY_LOCAL_USER` (0x40), `NV_MEMORY_ALLOCATION_PARAMS` (`nvos.h:1600-1634`): `type=NVOS32_TYPE_IMAGE`, `attr = LOCATION_VIDMEM \| PAGE_SIZE_{4KB,BIG,HUGE} \| COMPR_NONE`, `attr2` `PAGE_SIZE_HUGE_2MB`, `flags = ALIGNMENT_FORCE`, `alignment`. Allowed (128). NVIDIA's own Vulkan driver uses the equivalent `NV_ESC_RM_VID_HEAP_CONTROL` (`vidmem.rs:18-20`). | Page size: 64 KiB big pages are the default on Blackwell; 2 MiB huge for large allocations. Set `bind_align_B` = 64 KiB for VRAM and 4 KiB for sysmem (per BO, `nvkmd_mem::bind_align_B`). |
| GART, host-allocated | `NV01_MEMORY_SYSTEM` (0x3e), same params with `LOCATION_PCI`, `COHERENCY_CACHED`. Allowed (128). | CPU map goes into the 1 GiB window (§3.3). |
| GART, guest pages | `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` (0x71), `NV_OS_DESC_MEMORY_ALLOCATION_PARAMS` with the guest VA of an anonymous mmap. Allowed (40). The backend translates the address to pinned guest page runs (`device/src/nvidia/osdesc.rs:1-29`). | **Preferred for mappable memory**: zero-copy, coherent, no window cost. The CPU map is the anon mapping itself. |
| PTE kind, compression | Per mapping: `NVOS46_FLAGS_PAGE_KIND_OVERRIDE_YES` + `kindOverride` on MapMemoryDma (`nvos.h:2114-2116`, `NVOS46_PARAMETERS.kindOverride` 2178). RM checks only `FB_IS_KIND_SUPPORTED` (`ogkm:src/nvidia/src/kernel/mem_mgr/virtual_mem.c:1319-1328`). Compressible kinds need comptags at allocation (`NVOS32_ATTR_COMPR_REQUIRED`). | Phase 1: `has_compression = false`. |
| CPU map | `NV_ESC_RM_MAP_MEMORY` (NVOS33 with fd, `nv_ioctl_nvos33_parameters_with_fd`) on a **fresh** `/dev/nvidia0` fd, then `mmap(fd, pLinearAddress cookie)`; `NV_ESC_RM_UNMAP_MEMORY` + munmap + close. | One mapping per fd: RM returns `NV_ERR_STATE_IN_USE` for a second (`device/src/nvidia/rm_fd.rs:199-207`). |
| export, import | `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` 0x3d05 / `IMPORT_OBJECT_FROM_FD` 0x3d06, or `NV_ESC_RM_DUP_OBJECT` | allowed; later |

### 2.3 GPU VA

| nvkmd | RM |
|---|---|
| VA space (per `nvkmd_dev`) | `FERMI_VASPACE_A` (0x90f1) under the device, `NV_VASPACE_ALLOCATION_PARAMETERS` (`nvos.h:3146-3156`). Allowed (56). Flags: **`VA_INTERNAL_LIMIT` (BIT 7)**, which confines RM-internal mappings (context buffers etc.) to `[0x1_0000_0000, 0x1_2000_0000)` (`vaspace_api.c:370-375`, `g_gpu_vaspace_nvoc.h:90-91`). Optional `REQUIRE_FIXED_OFFSET` (BIT 12) makes any client map without an explicit address fail: a good debug guard. `bigPageSize = 64 KiB`. |
| `alloc_va` | Keep NVK's `util_vma_heap` (copy `nvkmd_nouveau_va.c:16-95`), but carve `[4 GiB, 4.5 GiB)` out of the main heap with `util_vma_heap_alloc_addr` at dev creation. Then **either** (A, simplest) allocate one `NV50_MEMORY_VIRTUAL` (0x50a0, `NV_MEMORY_ALLOCATION_PARAMS` with `flags = VIRTUAL \| FIXED_ADDRESS_ALLOCATE \| MEMORY_HANDLE_PROVIDED`, `offset`, `size`, `hVASpace`; allowed, 128) covering each heap at dev creation and map into it, **or** (B) one `NV50_MEMORY_VIRTUAL` per `nvkmd_va` at the address the heap chose (as `nvidia-push-init.c:722-760` does). B is needed for sparse. |
| `SPARSE` | `NVOS32_ALLOC_FLAGS_SPARSE` (0x04000000, `nvos.h:1414-1465`) on that VA's `NV50_MEMORY_VIRTUAL` (option B). Unverified for an unprivileged client: test. |
| `bind_mem` | `NV_ESC_RM_MAP_MEMORY_DMA` (0x57, `NVOS46_PARAMETERS`, `nvos.h:2168-2184`): `hDevice`, `hDma` = the `NV50_MEMORY_VIRTUAL`, `hMemory` = the BO, `offset` = mem offset, `length`, `dmaOffset` = VA (absolute for a virtual-memory `hDma`, see the field comment), `flags = ACCESS_READ_WRITE \| DMA_OFFSET_FIXED_TRUE \| PAGE_SIZE_{4KB,BIG,HUGE} \| CACHE_SNOOP_ENABLE (sysmem) \| PAGE_KIND_OVERRIDE_YES`, `kindOverride = va->pte_kind`. |
| `unbind` | `NV_ESC_RM_UNMAP_MEMORY_DMA` (0x58, NVOS47: `hDma`, `hMemory`, `dmaOffset`, `size`). An unmap is per mapping, so the backend tracks `(va range → hMemory, dmaOffset)` and splits partial unbinds. `DEFER_TLB_INVALIDATION` exists for batching. |
| bind ctx (`NVKMD_ENGINE_BIND`) | No queue in RM: wait for the `ctx->wait` syncs on the CPU, do the maps and unmaps, then signal. |

Both escapes have fixed-size, pointer-free parameters; the backend passes them
through after the ABI size check (`device/src/nvidia/ioctl.rs:790-805`, default
arm). They act only on the caller's own client's objects.

### 2.4 Channel (exec context)

Order, following `ogkm:src/common/unix/nvidia-push/src/nvidia-push-init.c`
(NVIDIA's own userspace-style channel library used by nvkms headSurface;
`AllocUserMode` 931-1005, `nvDmaAllocUserD` 487-548, `AllocChannelObject`
365-485, `BindAndScheduleChannel` 329-363, `RequestChidToken` 288-327) and
`ogkm:src/nvidia/src/kernel/rmapi/nv_gpu_ops.c` (UVM's channels: TSG 6456-6520,
`channelAllocate` 5832, work-submit token 5708-5721):

| # | object / control | class / cmd | params | allowed |
|---|---|---|---|---|
| 1 | usermode (once per subdevice) | `BLACKWELL_USERMODE_A` 0xc761 (`HOPPER_USERMODE_A` 0xc661 on Hopper) under subdevice | `NV_HOPPER_USERMODE_A_PARAMS {bBar1Mapping=1, bPriv=0}` (`nvos.h:3319-3327`); `bPriv` needs kernel privilege in RM (`usermode_api.c:63-72`) | yes (2, optional) |
| 2 | map usermode | `NV_ESC_RM_MAP_MEMORY` on it, `NVC361_NV_USERMODE__SIZE` = 64 KiB | doorbell = `NVC361_NOTIFY_CHANNEL_PENDING` 0x90 | escape |
| 3 | channel group (TSG) | `KEPLER_CHANNEL_GROUP_A` 0xa06c under device | `NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS {hObjectError, hObjectEccError, hVASpace, engineType=NV2080_ENGINE_TYPE_GRAPHICS, ...}` | yes (20) |
| 4 | context share | `FERMI_CONTEXT_SHARE_A` 0x9067 under the TSG | `NV_CTXSHARE_ALLOCATION_PARAMETERS {hVASpace, flags=SUBCONTEXT_ASYNC, subctxId}` | yes (12) |
| 5 | error notifier, GPFIFO ring, USERD | one sysmem allocation (OS descriptor or `NV01_MEMORY_SYSTEM`) mapped at a fixed VA | `hObjectError` may be a plain memory object: RM looks it up with `memGetByHandle` first, ctxdma second (`kernel_channel.c:1925-2014`) | yes |
| 6 | GPFIFO channel | `BLACKWELL_CHANNEL_GPFIFO_B` 0xca6f under the TSG | `NV_CHANNEL_ALLOC_PARAMS` (`alloc/alloc_channel.h:297-347`): `hObjectError`, `gpFifoOffset` (VA), `gpFifoEntries`, `hContextShare`, `hVASpace`, `hUserdMemory[0]`, `userdOffset[0]`, `engineType`. Leave the physical-memory fields (`instanceMem`, `userdMem`, `ramfcMem`, `mthdbufMem`, `internalFlags`) zero: they are kernel-client only (nouveau's use, `linux:.../gsp/rm/r535/fifo.c:76-148`) | yes (376) |
| 7 | engine objects on the channel | `BLACKWELL_B` 0xce97 (3D), `BLACKWELL_COMPUTE_B` 0xcec0, `BLACKWELL_DMA_COPY_B` 0xcab5 (`NVB0B5_ALLOCATION_PARAMETERS.engineType` = a GRCE), optional `BLACKWELL_INLINE_TO_MEMORY_A` 0xcd40 | RM allocates the GR context buffers here. nouveau's `NV2080_CTRL_CMD_GPU_PROMOTE_CTX` is not needed by a user client (it is refused anyway) | yes (16/16/8/16); 3D needs `--caps graphics` |
| 8 | bind | `NVA06F_CTRL_CMD_BIND` 0xa06f0104 `{engineType}` | | yes (4) |
| 9 | token setup | `NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX` 0xc36f010a, then `NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN` 0xc36f0108 → token | | yes (4, 4) |
| 10 | schedule | `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE` 0xa06c0101 on the TSG (or `NVA06F_..._GPFIFO_SCHEDULE` 0xa06f0103 on a bare channel) `{bEnable=1}` | | yes (3) |

Transfer queue: a second TSG and channel with `engineType = NV2080_ENGINE_TYPE_COPY(n)`
for an async CE, copy class only.

### 2.5 Submission

1. NVK's pushbuf ranges become GPFIFO entries in the ring:
   `GP_ENTRY0 = addr[31:2]`, `GP_ENTRY1 = addr[63:32] | (len/4) << 10`, plus the
   `NO_PREFETCH` bit (format `NVC86F_GP_ENTRY*`, as in `nvidia-push.c:204-209`).
   An `incomplete` push is just the next entry; no special handling is needed
   once the backend writes all entries before moving `GP_PUT`.
2. `wmb()`, then write `GP_PUT` (entry index) to USERD offset 0x8c
   (`clca6f.h:27-31`, `nvidia-push.c:428`).
3. Write the work-submit token to `usermode + 0x90` (`nvidia-push.c:477`).
4. Ring full: wait on the context's own timeline (§2.6) for `GP_GET` (USERD 0x88)
   to move.

No ioctl per submit. Mesa already has the host-class headers
(`mesa:src/nouveau/headers/nvidia/classes/clc96f.h`, `clca6f.h`).

### 2.6 Fences and syncs

| need | RM mechanism | notes |
|---|---|---|
| GPU signals timeline value V | Host methods on the channel: `SEM_ADDR_LO/HI`, `SEM_PAYLOAD_LO/HI`, `SEM_EXECUTE {OPERATION=RELEASE, RELEASE_WFI=EN, PAYLOAD_SIZE=64BIT}`, then `NON_STALL_INTERRUPT` (`clca6f.h`). Written by the backend at the end of `signal`/`flush`. | The semaphore lives in a coherent sysmem page that the CPU reads directly. |
| GPU waits | `SEM_EXECUTE {OPERATION=ACQ_STRICT_GEQ, ACQUIRE_SWITCH_TSG=EN, PAYLOAD_SIZE=64BIT}` on another context's semaphore. | Same-device cross-queue waits never touch the CPU. |
| CPU waits | Read the value; if not reached, `poll()` an fd that RM signals on the non-stall interrupt: `NV01_EVENT_OS_EVENT` (0x79, `NV0005_ALLOC_PARAMETERS{hParentClient, hSrcResource, hClass=0x79, notifyIndex = NV2080_NOTIFIERS_FIFO_EVENT_MTHD \| NV01_EVENT_NONSTALL_INTR, data = fd}`) under the subdevice, then `NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION` 0x20800301 `{event, action=REPEAT}`. Drain with `NV_ESC_RM_GET_EVENT_DATA` (0x52). The same pattern in kernel form: `ogkm:src/nvidia/src/kernel/gpu/mem_mgr/mem_utils.c:1907-1937`, `nvkms-rm.c:4189-4197`. | Allowed (24, 20). The guest module translates the fd in `data` (`guest/linux/conduit_gpu.c:255-260`) and the backend wakes the guest's `poll()` with `EventReady` (`docs/ARCHITECTURE.md:72-77`). This is the path NVIDIA's userspace already uses. Spinning briefly first avoids the wake latency of the virtio round trip. |
| `vk_sync_type` | One type: timeline in memory, features `TIMELINE \| GPU_WAIT \| CPU_WAIT \| CPU_SIGNAL \| WAIT_ANY \| WAIT_PENDING`; binary syncs via Mesa's `vk_sync_binary` on top (`mesa:src/vulkan/runtime/vk_sync_binary.h`). CPU signal = store + `wmb`. | Replaces `vk_drm_syncobj` (`nvkmd_nouveau_pdev.c:132-135`). |
| interop (later) | `NV_SEMAPHORE_SURFACE` (0xda, allowed 16) + nvidia-drm `SEMSURF_FENCE_CTX_CREATE/CREATE/WAIT`, already forwarded (`docs/SYNC.md`), turn a semaphore value into a sync_file → DRM syncobj. That gives `VK_KHR_external_semaphore_fd` and explicit-sync present. | `NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER` (0xda0003) carries an OS event handle in `notificationHandle`; whether the backend must translate it like the NV0005 fd is unverified. Use nvidia-drm instead. |

### 2.7 Errors (RC)

- `hObjectError` (channel and TSG) points at a `NvNotification` array. RM
  writes a non-zero `status` and `info32` (the RC error type) on a channel
  error. `ctx->sync` and `ctx->flush` check it and return `VK_ERROR_DEVICE_LOST`.
- Optional event: `NV2080_NOTIFIERS_RC_ERROR` on the subdevice, the same
  OS-event mechanism. `NV906F_CTRL_CMD_GET_MMU_FAULT_INFO` 0x906f0106 (allowed)
  gives the faulting VA for `VK_EXT_device_fault`. `NV906F_CTRL_CMD_RESET_CHANNEL`
  and `NVA06F_CTRL_CMD_STOP_CHANNEL` are allowed for teardown.
- A hung wait is bounded by the PBDMA acquire timeout plus the RC watchdog. Keep
  CPU waits timeout-able.

### 2.8 Which of these the proprietary userspace already exercises through Conduit

No RM traces are checked into the repo. From code and docs: NVIDIA's Vulkan
ICD (graphics, default caps) and CUDA (`--caps compute`) run in guests. They
need root client, device, subdevice, VA space, video memory (`VID_HEAP_CONTROL`
for Vulkan/GL, `NV01_MEMORY_LOCAL_USER` for CUDA, `vidmem.rs:18-20`), CPU
mapping and the window (`window.rs`), `MAP_MEMORY_DMA` (its ABI size is tested,
`tests.rs:96-117`), the channel-group/GPFIFO/usermode/token sequence, the 3D and
compute classes (`caps.rs:18-20`: "NVIDIA's Vulkan driver allocates the compute
class for its compute queues"), OS events with fd translation (the guest
`.poll` comment, `conduit_gpu.c:846-861`), OS-descriptor registration
(`osdesc.rs`) and semaphore-surface fences via nvidia-drm (`docs/SYNC.md`). The
one thing NVK does differently is `NV50_MEMORY_VIRTUAL` with fixed addresses
plus `PAGE_KIND_OVERRIDE`. NVIDIA's GL/Vulkan driver uses fixed-address VA
management too, but that is not verified here.

**Step 0 of the plan settles this:** record NVIDIA's own vkcube and a compute
sample in the lab VM with `conduit trace lab -o nvidia-vkcube.jsonl` (host side)
and keep the class/control sequence as the reference.

## 3. Host side: allowlist, caps, and what changes

### 3.1 How a guest call is judged (610.57.04)

- `NV_ESC_RM_ALLOC`: cap check (`caps.rs:76-88`, `Caps::for_class`: 3D classes
  → `graphics`, video → `video`, everything else ungated), then RM's
  non-privileged class table with exact parameter size
  (`device/src/nvidia/ioctl.rs:735-776`, `rm_class_refusal` 138).
- `NV_ESC_RM_CONTROL`: deny list for host-snooping controls
  (`gen/src/rmallow/mod.rs:176`), then the deprecated table, the GSS rule, and
  the exported-method table with exact size (`ctrl_rule`, mod.rs:353;
  `ioctl.rs:706-733`).
- Other escapes (`MAP_MEMORY_DMA`, `UNMAP_MEMORY_DMA`, `GET_EVENT_DATA`, `FREE`,
  `DUP_OBJECT`): ABI size check, then passthrough (`ioctl.rs:790-805`).
  `MAP_MEMORY` / `UNMAP_MEMORY` / `ALLOC_OS_EVENT` / OS-descriptor allocations
  get fd, window or page-run translation (`rm_fd.rs`, `window.rs`, `osdesc.rs`).
- RM's own runtime checks (`privLevel`, e.g. usermode `bPriv`, privileged
  channels, kernel-only VA-space controls) still apply, because the backend
  refuses to run as root or with `CAP_SYS_ADMIN` (`docs/SECURITY.md`).

### 3.2 Per object and control

| item | today | change needed |
|---|---|---|
| ROOT_CLIENT, DEVICE_0, SUBDEVICE_0 | allowed (v610_57_04.rs:790+) | none |
| FERMI_VASPACE_A (+ `VA_INTERNAL_LIMIT`) | allowed (849) | none |
| NV50_MEMORY_VIRTUAL (fixed address, sparse) | allowed (833) | none |
| LOCAL_USER, MEMORY_SYSTEM, OS_DESCRIPTOR | allowed (800, 798, 804); VRAM charged against `--vram-limit-mib` | none |
| MAP/UNMAP_MEMORY_DMA (incl. `PAGE_KIND_OVERRIDE`) | passthrough after size check | none |
| MAP_MEMORY + mmap | window placement, one mapping per fd | none (see 3.3) |
| KEPLER_CHANNEL_GROUP_A, FERMI_CONTEXT_SHARE_A | allowed (850, 838) | none |
| BLACKWELL_CHANNEL_GPFIFO_B | allowed (914) | none |
| BLACKWELL_USERMODE_A (+ map) | allowed (895); `bPriv` refused by RM itself | none |
| BLACKWELL_B 0xce97 | allowed (928), **needs `--caps graphics`** | none; a compute-only VM cannot run NVK (NVK needs 3D for compute queues) |
| BLACKWELL_COMPUTE_B, DMA_COPY_B, INLINE_TO_MEMORY_A | allowed (931, 915) | none |
| NVA06F BIND, NVA06C/NVA06F SCHEDULE, NVC36F tokens | allowed (sizes 4, 3, 3, 4, 4) | none |
| NV01_EVENT_OS_EVENT + EVENT_SET_NOTIFICATION + GET_EVENT_DATA | allowed (807; 20; escape) | none |
| device-info controls in 2.1 | all allowed | none |
| NV_SEMAPHORE_SURFACE + 0xda0001..6 | allowed | verify `REGISTER_WAITER`'s `notificationHandle` before using it directly (phase 2) |
| `NV2080_CTRL_CMD_GPU_PROMOTE_CTX` 0x2080012b | refused (RM: kernel-only) | none: not needed by a user client |
| `NV90F1_CTRL_CMD_VASPACE_*` (GMMU format, reserve entries) | refused (RM flags 0x18000) | none: NVK does not need page-table control |
| `NV2080_CTRL_CMD_RC_READ_VIRTUAL_MEM`, `NV0041_..._GET_SURFACE_PHYS_ATTR` | refused | none |

Result: **no allowlist or backend change is needed** for the plan in §6. Nothing
here weakens isolation. The guest client gets exactly what an unprivileged
local process gets from RM, inside its own RM client and VA space.

### 3.3 Practical limits that matter for NVK

| limit | where | effect | mitigation |
|---|---|---|---|
| shared window 1 GiB per VM, fixed at start | `docs/ARCHITECTURE.md:131`, `docs/QEMU.md:95,172` | every CPU map of host-allocated memory (VRAM via BAR1, `NV01_MEMORY_SYSTEM`) takes window space, across all guest processes | backend: mappable memory = OS-descriptor guest pages; `bar_size_B` = 0 so NVK exposes no host-visible VRAM heap at first; map on demand, not persistently. Growing the window later is a Conduit change (`shm_regions.rs`, VMM), not an isolation change |
| one mapping per host fd | `rm_fd.rs:199-207` | rmclient must open, register and close a `/dev/nvidia0` fd per mapping | in the rmclient API (§7) |
| KVM memslots | `aperture.rs:30-34` (pools); the window uses one region | only relevant if mappings move to the aperture | none now |
| `--vram-limit-mib` undercounts RM-internal context buffers | `vidmem.rs:26-28` | NVK's channels each carry GR context buffers | known, documented |
| fixed-address CPU maps (`has_map_fixed`) | guest module mmap of the window | untested | report `has_map_fixed = has_overmap = false` in phase 1 |

## 4. Prior art

| project | what it is | relevance |
|---|---|---|
| **nvidia-push** (`ogkm:src/common/unix/nvidia-push/`, with `nvidia-3d/`, MIT/GPL dual) | NVIDIA's reusable channel library: usermode alloc+map, client-allocated USERD, `NV50_MEMORY_VIRTUAL` + MapMemoryDma for pushbuffers, GPFIFO channel, bind/schedule, work-submit token, doorbell kick, semaphore progress tracking; `nvidia-3d` drives the 3D class on it (`nvidia-3d-hopper.c`) | **The closest reference: the exact RM call sequence for a 3D-capable channel, written against RM's API.** Port `nvidia-push-init.c` logic into `nvkmd/rm/` |
| `nv_gpu_ops.c` (`ogkm:src/nvidia/src/kernel/rmapi/`) | UVM's channel and TSG creation through RM's API | second reference for TSG, ctx share, work-submit token, GPFIFO in vidmem or sysmem |
| nouveau GSP-RM (`linux:drivers/gpu/drm/nouveau/nvkm/subdev/gsp/rm/`, `r535/`, `r570/`) | nouveau as an RM client: `gb20x.c` lists the GB20x classes (usermode `BLACKWELL_USERMODE_A`, `BLACKWELL_CHANNEL_GPFIFO_B`, `BLACKWELL_DMA_COPY_B`, `BLACKWELL_B`, `BLACKWELL_COMPUTE_B`, `BLACKWELL_INLINE_TO_MEMORY_A`); `r535/fifo.c` channel alloc, BIND, SCHEDULE, PROMOTE_CTX | same objects; but it is a **kernel** client (physical instance memory, `PROMOTE_CTX`, privileged channels): copy the class choice, not the parameters |
| **NVBringup** (github.com/kvarun-p/NVBringup, MIT) | macOS kext booting GSP-RM on Turing, with **NVK on top via a `nvkmd/macos` backend** (~70 Mesa patches, fork `nvbringup-mesa`). User-managed VA binding, compute queues, timeline semaphores, doorbells, MSI non-stall wakeups. Vulkan 1.4 compute works; no WSI | proof that a non-nouveau nvkmd backend over GSP-RM objects carries NVK compute. Its userspace ABI is the kext's own (`src/nv_uapi.h`), not RM escapes. Read its `nvkmd/macos` for the sync and VA design |
| tinygrad `ops_nv.py` / libtinynv (macuda) | userspace RM client in Python: root/device/subdevice, `FERMI_VASPACE_A`, channel group, ctx share, GPFIFO, compute and copy classes, usermode doorbell, semaphores; RTX 5090 (QMD v5) supported. Uses **UVM** for GPU VA (external ranges) | confirms the channel/doorbell sequence on Blackwell from userspace. Its UVM-based VA would need `--caps compute`: **use MapMemoryDma instead** |
| Mesa nvkmd abstraction (Faith Ekstrand, Collabora, Mesa 24.2; Phoronix "NVK Driver Lands New Platform Abstraction") | designed so that nothing DRM/nouveau leaks into NVK; Nova and NVIDIA's out-of-tree driver named as possible targets | no public RM backend upstream or in a known fork (searched 2026-10-05) |

## 5. Blackwell GB202 (RTX 5090) specifics

| item | value | source |
|---|---|---|
| PCI id in the lab guest | `10de:2b85` | `/sys/class/drm/renderD128/device` in `lab` |
| chipset / SM | 0x1b2 → SM 120 | `mesa:winsys/nouveau_device.c:56-57` |
| 3D | `BLACKWELL_B` 0xCE97 | `linux:.../gsp/rm/gb20x.c`, `mesa:headers/nvidia/classes/clce97.h` |
| compute | `BLACKWELL_COMPUTE_B` 0xCEC0, QMD v5.0 | `clcec0.h`, `clcec0qmd.h`; NAK `compiler/nak/qmd.rs:796-840`, size select 886-888 |
| copy | `BLACKWELL_DMA_COPY_B` 0xCAB5 (new GOB kinds) | `nvk_cmd_copy.c:179-183, 264` |
| GPFIFO | `BLACKWELL_CHANNEL_GPFIFO_B` 0xCA6F | `clca6f.h` |
| usermode | `BLACKWELL_USERMODE_A` 0xC761 (Hopper params struct) | `ogkm:class/clc761.h` |
| I2M | `BLACKWELL_INLINE_TO_MEMORY_A` 0xCD40 | gb20x.c |
| NVK status | `BLACKWELL_B` is on the conformant list (`nvk_physical_device.c:87-102`, Vulkan 1.4 conformance version at 1030); separate depth/stencil, Blackwell GOBs and tiling in NIL (`nil/tiling.rs:79-166`, `nil/image.rs:745`) | upstream main |
| shaders | NAK SM 120 latencies (`compiler/nak/sm120_instr_latencies.rs`), SM70 encoder family | upstream main |
| page tables | MMU v3 (RM's business, invisible here) | - |

## 6. Plan

### Phase 0: references (half a day)

1. Record NVIDIA's own vkcube and a compute sample in `lab` with
   `conduit trace lab -o ...` (host). Keep the class/control sequence and any
   refusals.
2. Build Mesa (`~/code/mesa-nvk-rm` + patches) on the host:
   `meson setup build -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms=x11,wayland -Dbuildtype=debugoptimized`
   (needs rustc, bindgen, libclc/clang for `src/nouveau/vulkan/cl`, glslang,
   python3-mako). Copy `build/` to the guest
   (`scp -i ~/.config/conduit/ssh/id_ed25519 ... 172.30.0.2:`), run with
   `VK_ICD_FILENAMES=.../nouveau_devenv_icd.x86_64.json VK_LOADER_DEBUG=driver`.
   The lab VM already has `/dev/nvidiactl`, `/dev/nvidia0`, `renderD128`
   (10de:2b85), the 610.57.04 userspace, vkcube and vulkaninfo, and Mesa
   26.0.8's own nouveau ICD (which declines this device).

### Phase 1: device and memory (vulkaninfo)

| step | nvkmd op | RM | done when |
|---|---|---|---|
| 1 | `try_create_pdev` | open, root, device, subdevice, info controls (§2.1); `kmd_info` all false except `has_get_vram_used`; `sync_types` = the RM timeline type | `vulkaninfo --summary` lists the 5090 through NVK |
| 2 | `create_dev` | `FERMI_VASPACE_A` with `VA_INTERNAL_LIMIT`; heaps with the 4 GiB hole | |
| 3 | `alloc_va` / `va_free` | `NV50_MEMORY_VIRTUAL` fixed (option A or B) | |
| 4 | `alloc_mem` | VRAM: `LOCAL_USER`; GART/`CAN_MAP`: OS descriptor | |
| 5 | `va_bind_mem` / `unbind` | MapMemoryDma / UnmapMemoryDma with kind override | `NVK_DEBUG=vm` log clean |
| 6 | `mem_map` / `unmap` | OS descriptor: return the anon map; VRAM: `MAP_MEMORY` + mmap on a fresh fd | |

### Phase 2: a channel and a compute shader

| step | nvkmd op | RM | done when |
|---|---|---|---|
| 7 | `create_ctx` (exec) | usermode, TSG, ctx share, ring+USERD+notifier, GPFIFO, engine objects 3D+compute+copy, bind, token, schedule (§2.4) | `vkCreateDevice` succeeds (it runs `nvk_queue_init_context_state`, which pushes `SET_OBJECT` and MME uploads) |
| 8 | `exec` / `flush` | GPFIFO entries, `GP_PUT`, doorbell (§2.5) | GP_GET advances |
| 9 | `signal` / `wait` / `sync` + `vk_sync_type` | semaphore release + non-stall, OS event, acquire (§2.6) | `vkQueueWaitIdle` returns without spinning |
| 10 | error notifier | §2.7 | a deliberately bad push gives `VK_ERROR_DEVICE_LOST` |
| 11 | test | `NVK_DEBUG=push_sync,push_dump`, then a SPIR-V compute test (deqp-vk `dEQP-VK.compute.basic.*`, or vkpeak) | results match |

### Phase 3: vkcube

12. `nvk_wsi.c`: `sw_device = !kmd_info.has_dma_buf`. Run vkcube on the
    guest desktop (X11 or Wayland via `wl_shm`). Expect a CPU copy per frame.
13. Bind ctx (`NVKMD_ENGINE_BIND`): CPU-side wait, map, signal. Sparse via
    option B VAs.
14. Transfer queue: a CE channel; set `has_transfer_queue`.

### Phase 4: interop and performance (not needed for vkcube)

- dma-buf export/import via nvidia-drm GEM (`GEM_IMPORT_NVKMS_MEMORY` and
  related ioctls, already forwarded) or the RM `EXPORT_OBJECT_TO_FD` controls;
  then `has_dma_buf` and zero-copy present. **Done** (both, combined:
  `EXPORT_OBJECT_TO_FD` then `GEM_IMPORT_NVKMS_MEMORY`, as NVIDIA's
  userspace does; see guest/nvk-rm/README.md, "Zero-copy presentation").
- Semaphore surface + nvidia-drm sync_file → DRM syncobj interop (explicit-sync
  WSI, external semaphores).
- Compression (comptags, `has_compression`), host-visible VRAM once the window
  question is settled, `has_map_fixed`/`has_overmap`.

### Risks and unknowns

| # | risk | how to settle |
|---|---|---|
| 1 | Copy class on the GR channel (GRCE) is assumed allowed by RM as on nouveau; RM may want the CE on its own runlist | step 7; fallback: copy only on the transfer channel, and NVK's in-queue copies through compute (NVK already has compute-based copies for some cases) |
| 2 | Sparse `NV50_MEMORY_VIRTUAL` for an unprivileged client | step 13; fallback: no sparse features in phase 1 (NVK always advertises sparse binding: needs a `kmd_info.has_sparse` bit, a small NVK patch) |
| 3 | `VA_INTERNAL_LIMIT` behaviour on a user-allocated VA space (written for unlinked SLI) | step 2: alloc, then check that RM's ctx buffers land in [4 G, 4.5 G) (`NVK_DEBUG=vm` collisions) |
| 4 | MapMemoryDma cost: one ioctl round trip through virtio per bind (NVK binds once per BO, plus sparse) | measure; batch with `DEFER_TLB_INVALIDATION` |
| 5 | 1 GiB window exhaustion | OS-descriptor sysmem for mapped memory; window growth is a separate Conduit task |
| 6 | Non-stall event latency through the virtio event queue (`conduit_gpu.c:513-544` describes it) | spin briefly before `poll`; `poll_spin_us` exists in the guest module |
| 7 | GR context state NVK expects from nouveau's golden context (zcull ctxsw buffer `ctxsw_size/align`, SLM) | `NV2080_CTRL_CMD_GR_GET_CTX_BUFFER_SIZE` / `GR_CTXSW_ZCULL_BIND` are allowed; check `nvk_queue.c` SLM path |
| 8 | Windows port: no `/dev/nvidiactl`, WDDM owns VA and paging (`windows-thin-path.md:145-148`) | keep everything above behind the rmclient transport vtable; VA management may need to differ |

## 7. Changes to the rmclient API contract

The planned C API (`crm_open/close`, `crm_alloc`, `crm_control`, `crm_free`,
`crm_map_memory`, `crm_unmap_memory`, `crm_root`, transport vtable) covers
§2.1, §2.2 allocation and §2.4. Missing:

| add | escape | why |
|---|---|---|
| `crm_map_memory_dma(client, device, hDma, hMemory, offset, length, flags, kind_override, &dma_offset)` / `crm_unmap_memory_dma(client, device, hDma, hMemory, flags, dma_offset, size)` | `NV_ESC_RM_MAP_MEMORY_DMA` 0x57 (NVOS46, 64 B) / `NV_ESC_RM_UNMAP_MEMORY_DMA` 0x58 (NVOS47, 48 B) | every `va_bind_mem` / `unbind`; channel ring/USERD placement |
| `crm_event_open(client, parent, &handle, notify_index, &pollable_fd)` and `crm_event_drain(fd)` | new `/dev/nvidiactl` fd, `NV_ESC_REGISTER_FD`/`NV_ESC_ALLOC_OS_EVENT` as libnvidia does, `NV01_EVENT_OS_EVENT` alloc with `data = fd`, `NV_ESC_RM_GET_EVENT_DATA` 0x52 | CPU waits (§2.6), RC events |
| `crm_map_memory` opens, registers and keeps **one `/dev/nvidiaN` fd per mapping** and closes it in `crm_unmap_memory`; the transport vtable needs `open_dev`, `mmap`, `munmap`, `close` | `NV_ESC_RM_MAP_MEMORY` with `nv_ioctl_nvos33_parameters_with_fd` | RM and the Conduit backend refuse two mappings on one fd |
| `hDevice` of `crm_map_memory` may be a subdevice handle (usermode lives under the subdevice) | NVOS33 `hDevice` | doorbell mapping |
| optional: `crm_dup_object`, `crm_export_fd`/`crm_import_fd` | `NV_ESC_RM_DUP_OBJECT` 0x34; `NV0000_CTRL_CMD_OS_UNIX_EXPORT/IMPORT_OBJECT_*` | sharing (phase 4) |
| `crm_alloc` returns RM's status *and* writes back the (in/out) params | NVOS64/NVOS21 | `NV_MEMORY_ALLOCATION_PARAMS.offset/size` come back from RM (VA, rounded size) |

Status reporting: RM answers with a status inside the parameter block and the
ioctl succeeds. The backend also refuses that way (`osdesc.rs:24-29`). The
client must check `status`, not just the ioctl return.
