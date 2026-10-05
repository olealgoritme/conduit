# NVK on RM

A first cut of an NVK backend that drives the GPU through NVIDIA's Resource
Manager (RM, the open kernel modules' `/dev/nvidiactl` interface, forwarded
by Conduit in a guest) instead of nouveau. It is a patch series against
upstream Mesa that adds a second implementation of NVK's kernel abstraction,
`src/nouveau/vulkan/nvkmd/rm/`, next to `nvkmd/nouveau/`. It talks to RM
through [librmclient](../rmclient), which it loads at runtime.

Status: **compiles; not yet run on a GPU.** Everything below "What is
implemented" is written against the RM API and NVIDIA's own channel code,
but untested. The test plan at the end is the next step.

Design background: `docs/research/nvk-rm.md` (on the `feat/nvk-rm` branch).

## Base and patches

Mesa `main` at **`70c4c018cbe5b78a1db7e9413bc7e511b366fd95`**
("lavapipe: drop LVP_SNORM_BLEND workaround", 2026-10-05).

| # | patch | what |
|---|---|---|
| 1 | `nvk: prepare for an nvkmd backend that is not DRM` | split physical-device creation from DRM probing; instance `enumerate` hook (`nvkmd_enumerate_non_drm_pdevs`, falls back to DRM); `copy_sync_payloads` only with a DRM fd; fd/dma-buf external extensions and non-software WSI only with `has_dma_buf`; new `nvkmd_info::has_host_visible_vram`; export `nouveau_ws_device_info_init_limits()`. No change for nouveau. |
| 2 | `nvk/rm: build glue, RM API subset and librmclient loader` | `-Dnvk-rm` option, `nvrm/nvrm_api.h` (RM classes, params, controls and host methods copied from open-gpu-kernel-modules 610.57.04, MIT, with size asserts), verbatim copy of `rmclient.h` as a fallback, `dlopen` loader |
| 3 | `nvk/rm: enumerate RM GPUs and fill in the physical device` | device/subdevice, `nv_device_info` from RM controls |
| 4 | `nvk/rm: logical device and memory` | per-device client, `FERMI_VASPACE_A`, usermode doorbell, non-stall event, VRAM / system memory |
| 5 | `nvk/rm: GPU VA allocation and binding` | NVK-chosen VAs via fixed `NV50_MEMORY_VIRTUAL`, `crm_map_dma2` with PTE kind |
| 6 | `nvk/rm: execution and bind contexts` | TSG + subcontext + GPFIFO channel + engine objects, doorbell submission |
| 7 | `nvk/rm: timeline syncs on RM semaphores` | `vk_sync` type on 64-bit semaphores in memory |

Each patch builds on its own.

## Build

```sh
guest/nvk-rm/build.sh                 # ~/code/mesa-nvk-rm, build dir build-rm
guest/nvk-rm/build.sh /path/to/mesa build-dir
```

The script clones Mesa if needed, checks out the base commit on a local
branch `nvk-rm`, applies the series with `git am` (skipped if already
applied), points meson at `guest/rmclient/include/rmclient.h` through a
throwaway `rmclient.pc`, and builds only NVK:

```sh
meson setup build-rm -Dvulkan-drivers=nouveau -Dgallium-drivers= \
    -Dnvk-rm=enabled -Dbuildtype=debugoptimized
ninja -C build-rm src/nouveau/vulkan/libvulkan_nouveau.so \
    src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json
```

By hand: `git am guest/nvk-rm/patches/*.patch` on the base commit, then the
two commands above. librmclient is not needed to build (only its header, and
a copy is in patch 2).

Build dependencies (Ubuntu 24.04; this is what the host needed on top of its
existing packages): meson >= 1.4 (`pip install meson`), `bindgen-cli` and
`cbindgen` (`cargo install`), rustc >= 1.85, `llvm-20-dev libclang-20-dev
libclang-cpp20-dev libpolly-20-dev libllvmspirvlib-20-dev llvm-spirv-20
libclc-20-dev` (for `mesa_clc`), `libxshmfence-dev`, plus the usual Mesa
deps (libdrm, libelf, wayland, xcb, glslang, python3-mako/yaml).

Verified on the host: the series applies to the base commit and builds, both
with `-Dnvk-rm=enabled` and without it (plain nouveau NVK).

## Running (in a guest)

```sh
export NVK_RM=1                                  # opt in (see below)
export VK_ICD_FILENAMES=$PWD/build-rm/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json
export NVK_RMCLIENT_LIB=/path/to/librmclient.so  # default: librmclient.so.0, librmclient.so
vulkaninfo --summary
```

- `NVK_RM=1` is required: with NVIDIA's own driver stack installed, the RM
  backend would otherwise show the same GPU twice. Without it (or if
  librmclient or `/dev/nvidiactl` is missing) NVK falls back to its normal
  DRM/nouveau enumeration.
- librmclient's `crm_open` checks the kernel's RM version strictly against
  610.57.04; `CRM_RM_VERSION=any` relaxes that.
- The VM needs `--caps graphics`: NVK allocates the 3D class on every queue,
  compute included.
- `NVK_DEBUG=vm` prints what RM reported for the GPU (classes, VRAM, GPCs)
  and every VA operation; `NVK_DEBUG=push_sync,push_dump` syncs and dumps
  every submit.

## NVK on Blackwell (Mesa main)

GB20x is supported on main. `BLACKWELL_B` (0xCE97) is on NVK's conformant
list (`nvk_is_conformant()` in `nvk_physical_device.c`, together with Kepler
to Ada); Mesa has the class headers `clcd97/clce97` (3D), `clcdc0/clcec0`
(compute, QMD v5), `clc96f/clca6f` (GPFIFO), `clc9b5/clcab5` (copy);
`nouveau_device.c` maps chipset 0x1b0+ to SM 120 with its warp, block and
shared-memory limits; NAK has SM120 latencies and NIL has the Blackwell GOB
and tiling rules. An RTX 5090 (GB202, chipset 0x1b2) gets 3D 0xCE97,
compute 0xCEC0, copy 0xCAB5, GPFIFO 0xCA6F, usermode 0xC761 from this
backend's class selection.

## What is implemented (and how)

**Enumeration / physical device** (`nvkmd_rm_pdev.c`). One RM client per
GPU from `crm_gpu_count/crm_gpu_info`; `NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2`
for the device instance; `NV01_DEVICE_0` + `NV20_SUBDEVICE_0`; chipset from
`NV2080_CTRL_CMD_MC_GET_ARCH_INFO` (`architecture | implementation`); classes
from `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2` (highest one Mesa has headers
for); PCI ids/revision from `NV2080_CTRL_CMD_BUS_GET_PCI_INFO`, bus location
from `NV0000_CTRL_CMD_GPU_GET_PCI_INFO`; name from `GPU_GET_NAME_STRING`;
VRAM size and usage from `NV2080_CTRL_CMD_FB_GET_INFO_V2` (`HEAP_SIZE`,
`HEAP_FREE`; the values Conduit clamps to `--vram-limit-mib`); GPC/TPC from
`NV2080_CTRL_CMD_GR_GET_INFO_V2`; SM limits as nouveau computes them. Volta
or newer is required (doorbell submission). `kmd_info`: only
`has_get_vram_used`.

**Device** (`nvkmd_rm_dev.c`). Its own RM client per `VkDevice`;
`FERMI_VASPACE_A` with 64 KiB big pages and `VA_INTERNAL_LIMIT`, which keeps
RM's internal mappings (GR context buffers) in [4 GiB, 4.5 GiB);
`*_USERMODE_A` (BAR1, write-only) mapped through the subdevice for the
doorbell; an `NV01_EVENT_OS_EVENT` on the FIFO non-stall interrupt via
`crm_event_open` (created without event data) plus
`NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION` (repeat); timestamps via
`NV2080_CTRL_CMD_TIMER_GET_TIME`.

**Memory** (`nvkmd_rm_mem.c`). CPU-mappable and GART memory: our own
anonymous pages wrapped in `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, so the CPU map
is the allocation itself (zero-copy, coherent, no host window space);
fallback `NV01_MEMORY_SYSTEM`. Device-local memory: `NV01_MEMORY_LOCAL_USER`,
64 KiB pages, no compression. No host-visible VRAM type is exposed
(`bar_size_B = 0`, `has_host_visible_vram = false`); an explicit
`VRAM | CAN_MAP` request would map through BAR1 with `crm_map_memory`. All
memory is coherent.

**VA** (`nvkmd_rm_va.c`). NVK keeps picking addresses from its
`util_vma_heap` ([2 MiB, 256 GiB) minus RM's internal range; replay heap
[256 GiB, 512 GiB); all below 2^40 as GPFIFO entries require). Each
`nvkmd_va` is an `NV50_MEMORY_VIRTUAL` at exactly that address
(`FIXED_ADDRESS_ALLOCATE`, `SPARSE` for sparse ranges; RM collisions are
retried elsewhere). Binds are `crm_map_dma2` into it with
`DMA_OFFSET_FIXED`, 64 KiB PTEs for VRAM, snooped 4 KiB PTEs for system
memory and `PAGE_KIND_OVERRIDE` with the VA's PTE kind. RM unmaps whole
mappings only, so each VA tracks its mappings and partial unbinds unmap and
re-map the remainders.

**Exec contexts** (`nvkmd_rm_ctx.c`), after `nvidia-push-init.c`:
`KEPLER_CHANNEL_GROUP_A` (GR, our VA space) → `FERMI_CONTEXT_SHARE_A`
(async) → GPFIFO channel (1024 entries; ring and push slots in our system
pages; error notifier + USERD in RM system memory) → 3D / compute / copy
objects without parameters → `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`,
`GET_WORK_SUBMIT_TOKEN` → `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`. Submission has
no RM call: GPFIFO entries (`NO_PREFETCH` → `SYNC_WAIT`), GP_PUT in USERD,
token to `usermode + 0x90`. A full ring waits for GP_GET (10 s, then
`DEVICE_LOST`); a non-zero error notifier is `VK_ERROR_DEVICE_LOST`.

**Bind contexts**: synchronous; waits and signals on the CPU.

**Syncs** (`nvkmd_rm_sync.c`). Timeline `vk_sync` = 64-bit value in a
per-device pool of system memory; `vk_sync_binary` on top. GPU signal:
`SEM_EXECUTE` release (64-bit, WFI) + `NON_STALL_INTERRUPT`; GPU wait:
`SEM_EXECUTE ACQ_STRICT_GEQ` with TSG switch. CPU wait: read the value, spin
briefly, then `poll()` the non-stall event fd (bounded at 10 ms per round, so
a lost wakeup costs at most that) or sleep with backoff without an event.
Event payloads are never needed (`crm_event_drain` is best effort; Conduit
refuses `NV_ESC_RM_GET_EVENT_DATA` in guests). `WAIT_PENDING` uses a
per-sync "highest submitted value". `WAIT_BEFORE_SIGNAL` is not advertised,
so Vulkan runs in assisted timeline mode (a submit thread holds back waits on
unsubmitted values instead of leaving GPU acquires spinning).

## What is stubbed or missing

| item | state | needs |
|---|---|---|
| dma-buf import/export, external memory/semaphore/fence fds | not supported (extensions not advertised) | nvidia-drm GEM import of RM memory or `NV0000_CTRL_CMD_OS_UNIX_EXPORT/IMPORT_OBJECT_*`; `NV_SEMAPHORE_SURFACE` + nvidia-drm sync_file bridge |
| presentation | software WSI (CPU copy per frame), untested | the above, for zero-copy |
| host-visible VRAM, BAR heap | off | Conduit's 1 GiB mapping window question |
| tiled BOs / DRM modifiers, compression | off | comptags (`NVOS32_ATTR_COMPR_REQUIRED`) |
| transfer queue (async CE channel), video decode | off | a second TSG with `NV2080_ENGINE_TYPE_COPY(n)` |
| zcull info | not queried | `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` |
| fixed CPU maps, overmap (`VK_EXT_map_memory_placed`) | off | |
| sparse | code path present (`NVOS32_ALLOC_FLAGS_SPARSE`), unverified for an unprivileged client | test; NVK always advertises sparse binding |
| proper CPU waits | spin + `poll()` on the non-stall event; payload not read | fine as is; per-sync events via `NV_SEMAPHORE_SURFACE` waiters would avoid waking every waiter on every interrupt |
| device-lost detection while idle | only when a call touches the context | `NV2080_NOTIFIERS_RC_ERROR` event |

## librmclient

Used: the base contract plus `crm_map_dma2` (PTE kind), `crm_free_quiet`,
`crm_event_open/close/drain`. All additions are looked up with `dlsym` and
optional: without `crm_map_dma2` images get the physical (generic) kind,
without events CPU waits sleep-poll.

Still missing / wanted:

- **`crm_unmap_dma` has no size** (NVOS47 `size`). RM supports partial
  unmaps; the backend works around it by unmapping whole mappings and mapping
  the remainders back, which costs extra round trips on sparse unbinds. A
  `crm_unmap_dma2(..., gpu_va, size)` would remove that.
- **Event payloads**: not needed by this design.
- Nice to have: a versioned soname (`librmclient.so.0`) and an installed
  `rmclient.pc`, so distributions can ship it next to Mesa (the backend
  already tries `librmclient.so.0` first).

## Host changes needed

None in the allowlist: every class and control used is in the 610.57.04
table with the parameter sizes the backend sends (`NV01_ROOT_CLIENT`,
`NV01_DEVICE_0`, `NV20_SUBDEVICE_0`, `NV01_MEMORY_LOCAL_USER`,
`NV01_MEMORY_SYSTEM`, `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`,
`NV50_MEMORY_VIRTUAL`, `FERMI_VASPACE_A`, `KEPLER_CHANNEL_GROUP_A`,
`FERMI_CONTEXT_SHARE_A`, `BLACKWELL_CHANNEL_GPFIFO_B`, `BLACKWELL_B`,
`BLACKWELL_COMPUTE_B`, `BLACKWELL_DMA_COPY_B`, `BLACKWELL_USERMODE_A`,
`NV01_EVENT_OS_EVENT`; controls 0x205, 0x21b, 0x800292, 0x20801701,
0x20801801, 0x20800110, 0x20801303, 0x20801228, 0x20800403, 0x20800301,
0xc36f010a, 0xc36f0108, 0xa06c0101). Operationally:

- the VM needs `--caps graphics`;
- worth checking in the first run: that the backend's OS-descriptor
  translation accepts the anonymous `MAP_PRIVATE | MAP_POPULATE` pages NVK
  passes (with `MADV_DONTFORK`), and that RM-allocated system memory used
  for USERD and the error notifier is mappable within the window budget
  (8 KiB per queue);
- the non-stall event wakeup: whether `poll()` on a dataless event fd
  re-arms without `GET_EVENT_DATA` under the guest module (if it stays
  readable, waits degrade to spinning on `poll()`, still correct).

## Test plan (Linux `lab` guest, RTX 5090)

Build on the host (`build.sh`), copy `build-rm/` and `librmclient.so` into
the guest, then:

1. **Enumeration**: `NVK_RM=1 NVK_DEBUG=vm vulkaninfo --summary` lists the
   GB202 through NVK with the classes above and the VRAM limit. Also check
   that without `NVK_RM` nothing changes (NVK declines, NVIDIA's ICD works).
2. **Device creation**: `vulkaninfo` (full) or any app reaching
   `vkCreateDevice`. This creates the VA space, the usermode doorbell, the
   TSG/channel/engine objects and runs `nvk_queue_init_context_state`, i.e.
   the first GPFIFO submission and the first semaphore wait. Watch
   `conduit logs lab` for allowlist refusals. Check with `NVK_DEBUG=vm` that
   no VA collides with RM's [4 GiB, 4.5 GiB).
3. **Submission**: `NVK_DEBUG=push_sync,push_dump` on a trivial compute
   dispatch; GP_GET must advance and the context semaphore must complete.
4. **Compute**: `dEQP-VK.compute.basic.*`, `dEQP-VK.api.smoke.*`,
   `dEQP-VK.synchronization2.basic.*`, then vkpeak / a SPIR-V compute sample.
5. **Errors**: a deliberately bad pushbuffer (invalid method) must give
   `VK_ERROR_DEVICE_LOST`, not a hang.
6. **Graphics**: `dEQP-VK.renderpass.*` subsets, then `vkcube` (software
   WSI, CPU copy).
7. **Stress**: GPFIFO wrap (more than 1024 submits without waiting), many
   fences (semaphore pool growth), sparse binding tests.
