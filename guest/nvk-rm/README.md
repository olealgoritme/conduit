# NVK on RM

An NVK backend that drives the GPU through NVIDIA's Resource Manager (RM,
the open kernel modules' `/dev/nvidiactl` interface, forwarded by Conduit
from a guest) instead of nouveau. It is a patch series against upstream Mesa
that adds a second implementation of NVK's kernel abstraction,
`src/nouveau/vulkan/nvkmd/rm/`, next to `nvkmd/nouveau/`, and talks to RM
through [librmclient](../rmclient), loaded at runtime.

Status: runs on an RTX 5090 (GB202, GSP firmware).

- **Windows guests:** the rendering driver of the Windows guest stack
  (driver 22.22.405.24). The Helios D3D11 (DXVK) and D3D12 (vkd3d-proton)
  UMDs, Vulkan applications and OpenGL (Zink) run on it; RM calls go through
  the Helios KMD and the virtio transport. The desktop (DWM) and games run
  on NVK at 240 Hz; Counter-Strike 2 runs at ~330–380 fps at 1080p and
  ~350 fps at 5120x1440 with bots. Zero-copy presentation through the KMD's
  scanout.
- **Linux guests:** opt-in (`NVK_RM=1`), nothing changes for a guest that
  does not set it. `vulkaninfo`, compute and `vkcube` with zero-copy X11 and
  Wayland presentation work; dEQP has not been run.

Design background: [docs/research/nvk-rm.md](../../docs/research/nvk-rm.md).

## Series layout

| Directory | What | Used by |
|---|---|---|
| `patches/` | the RM backend (0001-0013), and the Linux versions of host-visible VRAM, cached system memory, compression and ZCULL (0014-0017) | Linux: 0001-0017; Windows: 0001-0013 |
| `patches-windows/` | the Windows build, Win32 WSI, Helios scanout, shared surfaces and composed present, RM fences, Zink, NVDEC | Windows |
| `patches-windows-dxvk/` | what DXVK needs on Windows, CPU waits, composed present by default | Windows |
| `patches-common/` | generic NVK work (per-draw cost, compression, deferred frees, profiling, waits), applied last | both |

Order of application:

- Linux: base + `patches/*` + `patches-common/*` (`build.sh`).
- Windows: base + `patches/0001-0013` + `patches-windows/*` +
  `patches-windows-dxvk/*` + `patches-common/*` (`build-windows.sh`).

Both apply with `git am` on the base commit, and the Windows stack also
builds for Linux (the tests run natively on it). `windows/stage-helios-package.sh`
stages the Windows build for the driver package.

## Base

Mesa `main` at **`70c4c018cbe5b78a1db7e9413bc7e511b366fd95`**
("lavapipe: drop LVP_SNORM_BLEND workaround").

## Knobs

Environment variables of the process; the defaults are the ones in force.
Patch numbers are file numbers in the directory named in the patch tables.

| Variable | Default | What it does |
|---|---|---|
| `NVK_RM` | 0 on Linux, 1 on Windows | use the RM backend; on Windows the device is hidden from processes the Helios ICD policy sends to Venus |
| `NVK_RM_DMABUF` | 1 | dma-buf export/import (Linux); 0 = software WSI, no external memory extensions |
| `NVK_RMCLIENT_LIB` | `librmclient.so.0`, `librmclient.so` | path of librmclient (Windows: `librmclient.dll` next to the ICD first) |
| **3D state and draws** | | |
| `NVK_CPU_STATE_TRACKING` | 1 | CPU-side draw, root table, cbuf and VB/IB tracking (common 1-4, 6); 0 = upstream MME macros, for A/B |
| `NVK_FS_STATE_TRACKING` | 1 | fragment shader state emitted only when it changes (common 34) |
| `NVK_NULL_VB_ZERO_PAGE` | 1 | a null vertex buffer is bound to the 4 KiB zero page instead of address 0 / size 0 (common 41) |
| `NVK_ZERO_PAGE_VRAM` | 1 | the zero page (null descriptors) lives in VRAM instead of uncached system memory (common 41) |
| `NVK_UBO_DESC_CBUF` | 0 on Windows, 1 elsewhere | promote UBO descriptors to bound cbufs (windows 0051) |
| `NVK_INDIRECT_PUSH` | 0 | indirect draw records as inline macro data from the pushbuffer (common 28); testing only |
| `NVK_MDI_BATCH` | 0 | read indirect multi-draw records in batches of n (max 204; 64 suggested) (common 24) |
| `NVK_DRAW_SYSVAL_SKIP` | 0 | write draw parameters to the root table only for shaders that read them (common 35) |
| `NVK_MME_PACK_INSTR`, `NVK_SKIP_DRAWID`, `NVK_BLACKWELL_MME_MEMBAR` | 0, 0, 1 | MME/indirect-draw A/B levers (common 26, 29) |
| **Memory** | | |
| `NVK_RM_BAR_MB` | 0 | the DEVICE_LOCAL \| HOST_VISIBLE heap through BAR1: 0 off, -1 all of BAR1, N MiB caps it (common 30) |
| `NVK_RM_DESC_TABLE_VRAM`, `NVK_RM_UPLOAD_VRAM` | 0 | descriptor tables/pools and command upload memory in the BAR heap (needs `NVK_RM_BAR_MB`) (common 26, 27) |
| `NVK_RM_SYSMEM_CACHED` | 1 | host-visible system memory GPU-cacheable in L2 (`=0` off, `=desc`/`=app` one kind only) |
| `NVK_RM_COMPRESSION` | 1 | compressible VRAM for images in dedicated allocations on GB20x |
| `NVK_RM_COMPRESS_ALL`, `_ZS`, `_TYPE` | 0 | compression outside dedicated allocations, for depth/stencil, and as a memory type (see "Compression") |
| `NVK_RM_COMPRESS_CLEAR`, `_UPGRADE`, `_TYPE_SCOPE` | unset | compression diagnostics (common 15) |
| `NVK_RM_ZCULL` | 1 | ZCULL from `GR_GET_ZCULL_INFO` |
| `NVK_RM_HUGE_PAGES` | 0 | 2 MiB GPU pages for CPU-unmapped VRAM of 2 MiB or more (common 9) |
| `NVK_RM_DEFER_FREE` | 1 on Windows, 0 on Linux | memory and VA frees wait for the GPU work submitted before them (common 19); `NVK_RM_DEFER_FREE_MB` (2048) caps the pending bytes |
| `NVK_RM_VIDEO` | 1 | NVDEC channel for H.264 decode (also needs `-Dvideo-codecs=h264dec` and `NVK_EXPERIMENTAL=video`) |
| **CPU waits and the GPFIFO ring** | | |
| `NVK_RM_EVENT_GEN` | 1 | CPU waits on librmclient's wake generations (common 42) |
| `NVK_RM_WAIT_POLL_MS` | 1 on Windows, 10 on Linux | longest single block on the non-stall event |
| `NVK_RM_WAIT_SPIN`, `NVK_RM_WAIT_SPIN_US` | 64 yields, 0 us | spin before blocking on the event |
| `NVK_RM_EVENT_DRAIN` | 0 on Windows | read `NV_ESC_RM_GET_EVENT_DATA` after every wake (windows-dxvk 6) |
| `NVK_RM_CPU_KICK` | 1 | CPU signals wake waiters through `crm_win_event_kick` (windows-dxvk 6) |
| `NVK_RM_GPFIFO_ENTRIES` | 4096 | GPFIFO ring size, a power of two from 64 to 4096 |
| `NVK_RM_RING_YIELD_MS` | 4 | how long a full-ring wait yields before it sleeps |
| `NVK_RM_HIRES_SLEEP` | 1 | sleeps on a high-resolution waitable timer instead of `os_time_sleep` |
| `NVK_RM_TIMESLICE_US`, `NVK_RM_INTERLEAVE` | unset (RM defaults) | GR runlist timeslice and interleave level of each context's TSG |
| **Windows presentation** | | |
| `NVK_HELIOS_WSI` | 1 | Helios zero-copy swapchains; 0 = GDI copy |
| `NVK_HELIOS_WSI_LINEAR` | 0 | linear instead of block-linear scanout images |
| `NVK_HELIOS_WSI_COMPOSE` | 1 under a DWM on NVK, else 0 | composed present through a D3D11 flip swap chain; 0 = GDI |
| `NVK_HELIOS_WSI_SCANOUT` | 0 | force scanout swapchains under a DWM on NVK |
| `NVK_HELIOS_DRI` | 0 | host render node index for GEM imports |
| `NVK_RM_FENCE`, `NVK_RM_FENCE_KMD` | 1 | presents retire on RM fences; the KMD (not a flip thread) holds them |
| `NVK_SCANOUT_RELEASE` | 1 | scanout images come back on the host's release event |
| `MESA_WSI_SCANOUT_HZ` | 0 (unpaced) | cap scanout presents per second |
| `MESA_WSI_SCANOUT_MIN_IMAGES` | 3 | `minImageCount` of scanout surfaces (1-8) |
| `MESA_WSI_WIN32_FIFO_ONLY` | 0 | offer FIFO only (otherwise IMMEDIATE and MAILBOX too) |
| `MESA_WSI_COMPOSE_THREAD` | 1 | composed presents on a per-swapchain thread |
| `MESA_WSI_COMPOSE_ADAPTER` | auto | D3D11 adapter index of the composed path |
| **Helios interface** | | |
| `NVK_HELIOS_RESID`, `NVK_HELIOS_FORMATS` | 1 | resource ids for shared images; the wider shared-format set |
| `NVK_HELIOS_MODIFIERS` | 1 | advertise `VK_EXT_image_drm_format_modifier` on Windows |
| `NVK_HELIOS_SHARED_IMPORT` | 1 | open another NVK process's shared surface |
| `HELIOS_ICD` | policy | `venus` or `nvk` forces the ICD for the process |

## Diagnostics

On Windows the output goes to `%ProgramData%\Helios\` (else `%TEMP%`), one
file per process.

| Variable | Output |
|---|---|
| `NVK_PASS_PROFILE=1` | GPU time per render pass signature, per operation outside render passes and per command buffer, every two seconds, with the shaders each pass used (`nvk-pass-<pid>.txt`, or `NVK_PASS_PROFILE_FILE`) |
| `NVK_PASS_PROFILE=2` | adds vertex, clipper and pixel shader invocations per pass |
| `NVK_PASS_PROFILE=3` | adds the ZCULL statistics per pass |
| `NVK_PASS_PROFILE=4` | adds the 24 most written 3D methods per pass and bind counts |
| `NVK_PASS_PROFILE=5` | dumps the draws of one render pass matching `NVK_PASS_DUMP`, after `NVK_PASS_DUMP_DELAY_S` (20) s (`nvk-prepass-dump-<pid>.txt`) |
| `NVK_SHADER_STATS=1` | NAK statistics of every uploaded shader (`nvk-shaders-<pid>.txt`) |
| `NVK_WAIT_STATS=1` | CPU wait counts and times per site, woken and timed-out waits, once a second (`nvk-waits-<pid>.txt`) |
| `NVK_DEBUG=vm` | VA and memory alloc/free/bind lines with times, threads and freeing call stacks (`NVK_VM_LOG`, else `nvk-vm-<pid>.log` in `%TEMP%`) |
| `HELIOS_VK_FRAMETIME=1` | per-present frame times of a native Vulkan app (`vkframes-<pid>.csv` with PresentMon v1 columns, readable by `guest/windows/ci/vmtest/pmpace.ps1`, and `vkframes-<pid>.log`); `HELIOS_VK_FRAMETIME_DIR` overrides the folder |
| `MESA_LOG_FILE` | Mesa's log in a file (honoured on Windows too) |
| `NVK_DEBUG=push_sync,push_dump` | syncs and dumps every submit |

The composed present writes `helios-wsi-<pid>.log`. No files at all from a
process means NVK's WSI never loaded in it (the Helios policy sent it to
Venus); `vkframes` files without rows mean it never presented through NVK.

## `patches/`: the RM backend

Each patch builds on its own.

| # | what |
|---|---|
| 0001 | prepare NVK for an nvkmd backend that is not DRM: physical-device creation split from DRM probing, instance `enumerate` hook (`nvkmd_enumerate_non_drm_pdevs`, DRM as fallback), `copy_sync_payloads` only with a DRM fd, fd/dma-buf external extensions and non-software WSI only with `has_dma_buf`, `nvkmd_info::has_host_visible_vram`. No change for nouveau |
| 0002 | `-Dnvk-rm` build option, `nvrm/nvrm_api.h` (RM classes, params, controls and host methods copied from open-gpu-kernel-modules 610.57.04, MIT, with size asserts), a copy of `rmclient.h`, the `dlopen` loader |
| 0003 | enumerate RM GPUs, fill the physical device (`nv_device_info` from RM controls) |
| 0004 | logical device and memory: per-device client, `FERMI_VASPACE_A`, usermode doorbell, non-stall event, VRAM / system memory |
| 0005 | GPU VA allocation and binding: NVK-chosen VAs via fixed `NV50_MEMORY_VIRTUAL`, `crm_map_dma2` with the PTE kind |
| 0006 | execution and bind contexts: TSG + subcontext + GPFIFO channel + engine objects, doorbell submission |
| 0007 | timeline syncs: `vk_sync` type on 64-bit semaphores in memory |
| 0008 | what GB202 with GSP needs: OS descriptors via `crm_alloc_os_descriptor`, RM's VA alignment rules, the device VA space, SYNC subcontext + channel bind, `cls_m2mf`, sync features |
| 0009 | ring progress tracked with a semaphore (USERD GP_GET is not written back), `move` for binary syncs |
| 0010 | VA ranges are reported at the size NVK asked for; RM's 2 MiB rounding stays internal (`rm_size_B`) |
| 0011 | a non-stall event whose data cannot be read is not polled; waits sleep instead of spinning through refused `NV_ESC_RM_GET_EVENT_DATA` escapes |
| 0012 | `wsi_device::wait_before_present`, set by NVK when the backend has dma-bufs but no sync_file export (RM): the WSI waits for rendering before presenting |
| 0013 | dma-buf export and import through nvidia-drm, DRM format modifiers (`has_dma_buf` + `has_alloc_tiled`): `OS_UNIX_EXPORT/IMPORT_OBJECT`, GEM import/export, DRM node discovery, `VK_EXT_image_drm_format_modifier` |
| 0014 | Linux version of host-visible VRAM, see `patches-windows/0022` and "Host-visible VRAM" |
| 0015 | Linux version of GPU-cacheable host-visible system memory (`NVK_RM_SYSMEM_CACHED`) |
| 0016 | Linux version of compressible VRAM for images (`NVK_RM_COMPRESSION`) |
| 0017 | Linux version of ZCULL (`NVK_RM_ZCULL`) |

## `patches-windows/`: the Windows build and the Helios integration

| # | what |
|---|---|
| 0014 | the backend calls neither `mmap` nor `poll`: `crm_event_wait`, `crm_alloc_pages`/`crm_free_pages` (librmclient transport ABI 2; an older librmclient on Linux gets the same code from `nvkmd_rm_lib.c`); `LoadLibrary` of `librmclient.dll` next to the ICD |
| 0015 | `vk_image::drm_format_mod` exists on every OS (`DRM_FORMAT_MOD_INVALID` on Windows) |
| 0016 | build without libelf (`nv_cubin_nolibelf.c`, CUDA modules rejected) |
| 0017 | driver build id from the Mesa version and module timestamp (`disk_cache_get_function_identifier`) |
| 0018 | NAK leaves nouveau's winsys and DRM out of its bindings |
| 0019 | build with the RM backend only: `with_nouveau_drm` (false on Windows) drops the nouveau winsys and `nvkmd/nouveau`; chipset limits in `nouveau_device_limits.[ch]`; the DRM side in `nvkmd_rm_drm.c` (Linux only, stubs otherwise); `VK_EXT_physical_device_drm` and syncobj copies Linux only; empty `<sys/ioccom.h>`; `vulkan_nouveau.dll` with `vulkan_api.def` exports |
| 0020 | `VK_KHR_win32_surface` + swapchain through Mesa's win32 WSI as a software device (GDI copy per present); the fallback path |
| 0021 | zero-copy present by Helios scanout: swapchain images in VRAM, imported once as GEM objects on a host render node, shown with the KMD's scanout source; GDI is the fallback (see "Presentation on Windows") |
| 0022 | host-visible VRAM, a BAR heap (generic RM code) |
| 0024 | block-linear scanout swapchains with NVIDIA's DRM modifier; `NVK_HELIOS_WSI_LINEAR=1` or a refused modifier: linear |
| 0025 | a BAR-heap allocation whose CPU map the host refuses is backed by system pages |
| 0025-s3 | `helios_icd_interface` v2 exported from `vulkan_nouveau.dll`: `memory_res_id` (image memory to a Venus-holder resource id by `IMPORT_RM`, with the image's layout), `scanout_present`, `deviceLUID` = the Helios adapter's LUID; `HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO` in `vkAllocateMemory`; `NVK_HELIOS_RESID=0` off |
| 0026 | multi-file shader cache on Windows (see "Shader cache on Windows") |
| 0027 | host-visible system memory mapped GPU-cacheable, L2 sysmem invalidate at the start of every submit (`NVK_RM_SYSMEM_CACHED`) |
| 0028 | compressible VRAM for images on GB20x (`NVK_RM_COMPRESSION`) |
| 0029 | ZCULL from `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` (`NVK_RM_ZCULL`) |
| 0030 | presents retire on RM fences, no CPU wait; `helios_icd_interface` v3 (`queue_rm_fence`, `rm_fence_wait`, `rm_fence_close`, `scanout_present_fenced`); `vk_queue_signal_sync` exported by the runtime (`NVK_RM_FENCE`, `NVK_RM_FENCE_KMD`) |
| 0031 | open another NVK process's shared surface by its Helios resource id (`HELIOS_ICD_CAP_SHARED_IMPORT`, `HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO`, `crm_win_rm_resource_import`; needs KMD 22.22.313+) |
| 0032 | Windows: RM device on by default under the Helios ICD policy (`HELIOS_ICD`, `HKLM\SOFTWARE\Helios` `Icd` / `NvkDenyList` / `NvkAllowList`), exported as `nvk_helios_process_allowed()`; a 32-bit build loads `librmclient32.dll` first |
| 0033 | tiled shadows for `TILING_LINEAR` color-attachment images (the Win32 WSI's linear swapchain images rendered with a depth buffer: vkcube, Zink) |
| 0034 | Zink loads a Vulkan ICD directly on Windows (see "OpenGL on NVK: Zink") |
| 0035 | video decode on an NVDEC channel: `cls_vdec` from the class list (0xc4b0 to 0xcfb0), `NVKMD_ENGINE_VDEC` contexts on the NVDEC0 runlist, `SET_OBJECT` with the device's class; H.264 only, bit-exact against libavcodec (`docs/video.md`; `NVK_RM_VIDEO=0` off) |
| 0036 | scanout images come back when the host releases them (KMD 22.22.315+, `NVK_SCANOUT_RELEASE=0` off) |
| 0037 | `util_queue_finish` destroys its barrier only after every thread has left it (Windows mutex + condvar barrier) |
| 0038 | a Helios shared-surface import goes on the device's memory list |
| 0039 | the Win32 surface reports `minImageCount` 2 when the driver flips; a one-image scanout chain gets its image back at once |
| 0040 | scanout surfaces report `minImageCount` 3 (`MESA_WSI_SCANOUT_MIN_IMAGES` overrides) |
| 0041 | resource ids for every shared format (8/16/32/64 bpp RGB, YUYV, NV12/P010/P016 as two planes in one memory); `helios_icd_interface` v4 (`HELIOS_ICD_CAP_LAYOUT_FORMATS`, `memory_res_plane1`); needs KMD `HELIOS_FOREIGN_CAP_LAYOUT_FORMATS` (`NVK_HELIOS_FORMATS=0` off) |
| 0042 | `VK_EXT_image_drm_format_modifier` on Windows, so an imported LINEAR surface gets its recorded pitch and offset (`NVK_HELIOS_MODIFIERS=0` hides it) |
| 0043 | the device's memory list stays consistent; misuse is logged once with a module+offset stack (16 per process) |
| 0044 | a device whose KMD went away touches none of its mappings: librmclient registers every KMD view in the loss table it shares with the Venus ICD and the Helios UMD, the loss epoch (`crm_win_loss_epoch`) makes flush/exec/wait/signal/sync return `VK_ERROR_DEVICE_LOST` first |
| 0045 | the BAR heap is sized from BAR1 and is not a hard limit; an allocation past it goes to system memory |
| 0046 | `helios_icd_interface` v5, `scanout_frame`: the KMD's `out_seq` and `out_generation` of a memory's latest scanout frame |
| 0047 | a refused scanout export says why, once per device (`mesa_logw`) |
| 0048 | NAK creates its NIR instruction printer only for `NAK_DEBUG=annotate` (its temp file is not creatable in a sandboxed process) |
| 0049 | the Helios process policy comes from Conduit's `helios_icd_policy.h` (`helios_policy_decide()`), the implementation the UMDs use; decision and reason logged once |
| 0050 | no scanout swapchains while a DWM on NVK composes the desktop (`NVK_HELIOS_WSI_SCANOUT=1` forces them) |
| 0051 | on Windows UBO descriptors are not promoted to bound cbufs (`NVK_UBO_DESC_CBUF`); the choice is part of the shader cache key |
| 0052 | the Helios holder context is renewed when the KMD calls it stale (`BAD_CONTEXT`), and the import retried once |
| 0053 | `helios_icd_interface` v6, `queue_rm_fence_v3`: `queue_rm_fence` plus the producer's semaphore and the presented image's RM memory and layout (`HeliosRmSemaphoreLoc` / `HeliosRmCopySource`), for the copy-engine present |
| 0054 | `HELIOS_VK_FRAMETIME=1`: per-present frame-time CSV (PresentMon v1 columns), written by a flush thread; PresentMon sees no frames from a native Vulkan app on NVK |
| 0055 | the frame-time log writes `vkframes-<pid>.log` (environment as seen, files, swapchains, totals) and falls back to `%TEMP%` |
| 0056 | `helios_icd_interface` v7, ECL fences: `ecl_fence_reserve`, `ecl_fence_create`, `ecl_fence_signal` on a caller-keyed timeline, so D3D12 `ExecuteCommandLists` can name a fence before vkd3d has submitted the batch |
| 0057 | the first device to see a loss-epoch move logs it once; with `HELIOS_VK_FRAMETIME=1` failing acquire/present results are logged |
| 0058 | composed present through a D3D11 flip swap chain (`NVK_HELIOS_WSI_COMPOSE`, `wsi_common_win32_compose.cpp`) |
| 0059 | the composed path writes `helios-wsi-<pid>.log` |
| 0060 | the composed present waits for its copy at acquire, not at present |
| 0061 | Mesa honours `MESA_LOG_FILE` on Windows; composed acquire/present failures go to `helios-wsi-<pid>.log` |

## `patches-windows-dxvk/`

| # | what |
|---|---|
| 0001 | `VKAPI_CALL` on `nvk_CmdCopyMemoryToImageIndirectKHR` (32-bit Windows, `__stdcall`) |
| 0002 | no `VK_KHR_present_id` / `present_wait(2)` on Windows: the Win32 WSI has no `wait_for_present` |
| 0003 | R/B swizzle in the GDI present for R8G8B8A8 swapchains (the DIB is BGRA) |
| 0004 | `NVK_RM_WAIT_SPIN` and `NVK_RM_WAIT_POLL_MS` for the CPU wait loop |
| 0005 | composed present by default under a DWM on NVK (`NVK_HELIOS_WSI_COMPOSE=0` = GDI); with a Helios present path the surface offers IMMEDIATE, MAILBOX and FIFO (`MESA_WSI_WIN32_FIFO_ONLY=1` = FIFO only); composed presents on a per-swapchain thread (`MESA_WSI_COMPOSE_THREAD`) that waits for the frame's fence, copies and presents, so `vkQueuePresentKHR` does not wait for the GPU; three DXGI buffers; IMMEDIATE/MAILBOX show only the newest queued frame |
| 0006 | CPU waits without a host round trip per wake (the non-stall event is dataless, so the data read never finds anything; `NVK_RM_EVENT_DRAIN=1` reads it); CPU signals and raised pending values kick waiters through `crm_win_event_kick` (`NVK_RM_CPU_KICK=0` off); `NVK_RM_WAIT_SPIN_US` |

## `patches-common/`: generic NVK work

Apply on top of both lineages, unchanged. One patch (5) also touches the RM
backend. Numbers are file numbers (there is no 0016 or 0025).

| # | what |
|---|---|
| 0001 | direct draws without an MME macro on Turing+: `vkCmdDraw*`/`DrawIndexed*`/`DrawMulti*` set first vertex, base instance, draw index and view index from the CPU (shadow scratch, `SET_GLOBAL_BASE_*`, root table), only what changed since the last direct draw, then draw with `SET_DRAW_CONTROL_A/B` + `DRAW_*_BEGIN_END_A/B`; indirect, mesh, XFB, multiview draws, meta and generated commands drop the tracking |
| 0002 | no `NVK_MME_SELECT_CB0` call after cbuf binds: with the hardware root table nothing loads cb0 through the selector |
| 0003 | CPU shadow of the root table (valid bit per dword, per command buffer); a descriptor bind loads only the dwords that changed |
| 0004 | per group/slot memory of the bound cbuf range; rebinding the same set/offset emits nothing |
| 0005 | `NVKMD_MEM_GPU_READ_ONLY` on descriptor pools and the image/sampler tables; the RM backend maps it GPU-cacheable (otherwise every bound descriptor set is a cbuf fetched across PCIe). Nouveau ignores the flag |
| 0006 | vertex and index buffers bound with plain methods on Turing+ (no `NVK_MME_BIND_VB/IB`), the range already bound is skipped |
| 0007 | what changes per draw lives in one hardware root table bank (a second 256-byte bank costs ~5 ns per switch): the dynamic-offset dword moves into bank 0 with the draw parameters and `sets[0..3]`. **API-visible**: `NVK_MAX_DYNAMIC_BUFFERS` is 32, i.e. 16 dynamic UBOs + 16 dynamic SSBOs per layout (NVIDIA: 15 + 16) |
| 0008 | ZCULL for DXVK depth buffers and reverse Z (see "ZCULL and DXVK depth buffers") |
| 0009 | `NVK_RM_HUGE_PAGES=1`: VRAM allocations of 2 MiB or more that the CPU never maps use 2 MiB GPU pages; a bind uses a huge PTE when VA, offset and range are 2 MiB granular; a refused huge page falls back to 64 KiB, said once |
| 0010 | `NVK_PASS_PROFILE`: GPU time per render pass, secondary command buffer and primary command buffer, from timestamped reports in a mapped ring harvested every 200 ms; a pass signature is its size, samples, layers, attachment counts and the first color / depth attachment's format, layout, PTE kind and compression |
| 0011 | `NVK_RM_COMPRESS_ALL`: compression for images outside dedicated allocations |
| 0012 | `NVK_RM_COMPRESS_ZS`: compress separate depth/stencil images |
| 0013 | `NVK_RM_COMPRESS_TYPE`: a compressible device-local memory type |
| 0014 | `NVK_CPU_STATE_TRACKING=0` turns off 0001-0004 and 0006 (upstream MME macros, cb0 reselected, every root table load and cbuf bind emitted) to pin a rendering bug on them without a rebuild |
| 0015 | compression diagnostics: `NVK_RM_COMPRESS_CLEAR`, `NVK_RM_COMPRESS_UPGRADE`, `NVK_RM_COMPRESS_TYPE_SCOPE` |
| 0017 | `NVK_DEBUG=vm`: VA alloc/free/bind/unbind and memory create/destroy lines (`NVKVM <time> +<s> t<thread> <op> [0x<start>,0x<end>) <size> ...`) to `NVK_VM_LOG`, else `%TEMP%\nvk-vm-<pid>.log` on Windows, else stderr; buffered, flushed every 50 ms, after every free/unbind, on a failed submit and at exit. Answers what last owned a faulting VA |
| 0018 | `NVK_CPU_STATE_TRACKING` is one atomic word; the upstream VB/IB macro binds clear the CPU's record of the range and a full root table load updates the shadow, so either path is safe at any time |
| 0019 | `NVK_RM_DEFER_FREE`: every flush after an exec ends with a WFI release of a per-context retire counter; a memory or VA free while submitted work is unfinished queues its RM unmap/free until that work has landed (`NVK_RM_DEFER_FREE_MB` caps the pending bytes) |
| 0020 | a destroyed sync's semaphore slot returns to the pool through 0019's deferred-free queue, so a release still in flight cannot complete the next sync's waits early |
| 0021 | the vm log's thread column is `t<id>(<name>)` (`GetThreadDescription`) |
| 0022 | every `mem-` line of the vm log ends with ` stack:` and up to 24 `module+0xoffset` frames, to symbolize against helios_umd's PDB/map |
| 0023 | `NVK_PASS_PROFILE` also times operations outside render passes (dispatch, copies, fill, update, clears, blit, resolve, query reset/copy) on the engine that ran them; each window prints GPU ms/s per operation kind and the top 40 signatures with draws, indirect calls, queries and barriers per instance; `=2` adds VS/clipper/PS invocations and ZCULL statistics |
| 0024 | `NVK_MDI_BATCH=<n>`: a `vkCmdDraw[Indexed]Indirect` with `drawCount > 1` is split into calls of at most n records (and 1024 dwords) to `NVK_MME_DRAW[_INDEXED]_INDIRECT_BATCH` macros that fetch all their records with one `MME_DMA_READ_FIFOED`; `gl_DrawID` continues. Checked in the simulator against the per-record loop |
| 0026 | descriptor tables/pools (`NVKMD_MEM_GPU_READ_ONLY`) and command upload memory (`NVKMD_MEM_CPU_WRITE_ONLY`) can go to the BAR heap (`NVK_RM_DESC_TABLE_VRAM`, `NVK_RM_UPLOAD_VRAM`), with a warning and system memory when the heap is full or a map is refused; app host-visible VRAM that falls back to system memory is GPU-cacheable; `NVK_RM_SYSMEM_CACHED=desc`/`app`; `NVK_BLACKWELL_MME_MEMBAR=0` drops the sysmembar from Hopper+ indirect-read barriers |
| 0027 | `NVK_PASS_PROFILE` pass lines add the shaders the draws used (VS GPRs/instructions/static cycles, FS GPRs/instructions, calls with FS and with tess/GS, max spills+fills and SLM); `=2` counters at their own pipeline locations, ZCULL counters at `=3`; `NVK_SHADER_STATS=1` logs every uploaded shader's NAK statistics |
| 0028 | `NVK_INDIRECT_PUSH`: Turing+ indirect draws take their records as inline macro data from a pushbuffer segment pointing at the indirect buffer; only the first segment after a barrier, event wait, secondary or command buffer start is SYNC_WAIT; unused macros are empty so the MME stays within its RAM (0xc00 dwords) |
| 0029 | all macros are built and their total checked against the known-good 2976 dwords before upload (queue creation fails above it); mesh macros are empty unless mesh/task shaders are enabled; `NVK_MME_PACK_INSTR=1` (test) packs macros by instruction; `NVK_SKIP_DRAWID=1` (test) drops the per-draw draw-index write of indirect draws; `NVK_MDI_BATCH` reads at most 64 dwords on Blackwell |
| 0030 | the DEVICE_LOCAL \| HOST_VISIBLE type (the BAR heap) is off by default (`NVK_RM_BAR_MB`): DXVK writes its constant, dynamic and descriptor buffers there through BAR1, the host's shared window, which costs ~30 % in CS2 |
| 0031 | `NVK_INDIRECT_PUSH` is opt-in (`=1`): its inline record segments can raise Xid 32 under CS2 |
| 0032 | the RM exec path gives ring space to a run of incomplete pushes and the push that completes them together, so a full ring never flushes between an incomplete push (0028's macro call) and its record segment |
| 0033 | `NVK_PASS_PROFILE=4`: the 24 most written 3D methods per pass signature (MME calls by macro name), counted from the pushbuffer at record time |
| 0034 | `NVK_FS_STATE_TRACKING`: the subtiling knobs, early-Z, post-Z coverage, ZCULL bounds and shading-rate/anti-alias macro calls after a fragment shader's program are skipped when equal to the last emitted; `NVK_PASS_PROFILE` counts shader, FS and descriptor binds per pass (`=4`: index/vertex buffer binds and indirect calls pointing into system memory or BAR VRAM) |
| 0035 | `NVK_DRAW_SYSVAL_SKIP=1`: the draw macros write first vertex / base instance / draw index to the root table only when a bound shader reads `gl_BaseVertex` / `gl_BaseInstance` / `gl_DrawID` |
| 0036 | `NVK_RM_TIMESLICE_US` (`NVA06C_CTRL_CMD_SET_TIMESLICE`) and `NVK_RM_INTERLEAVE` 0/1/2 (`NVA06C_CTRL_CMD_SET_INTERLEAVE_LEVEL`, privileged, may be refused) for each context's TSG; unset keeps RM's defaults. All NVK GPU waits use `ACQUIRE_SWITCH_TSG` |
| 0037 | GPFIFO ring of 4096 entries; the ring-space wait yields for 4 ms before sleeping (a `Sleep(1)` is a ~15.6 ms tick on Windows) |
| 0038 | the CPU wait fallback, the full-ring wait and the KMD flip-queue retry sleep on a per-thread high-resolution waitable timer; `NVK_WAIT_STATS=1` logs per-site wait counts and times |
| 0039 | `NVK_PASS_PROFILE=5`: after `NVK_PASS_DUMP_DELAY_S` (20) s the next pass matching `NVK_PASS_DUMP` (default the 2x MSAA D24S8 depth prepass) is dumped per draw: call parameters, shader hashes, IA/raster/depth-stencil state, IB/VB bindings, vertex attributes and the indirect records |
| 0040 | `NVK_RM_WAIT_POLL_MS` is 1 on Windows; `NVK_WAIT_STATS` counts woken and timed-out waits |
| 0041 | `NVK_NULL_VB_ZERO_PAGE`: a null vertex buffer (robustness2, DXVK's unused D3D11 slots) is bound to the 4 KiB zero page; `NVK_ZERO_PAGE_VRAM`: the zero page (null descriptors) is VRAM instead of uncached system memory on RM |
| 0042 | `NVK_RM_EVENT_GEN`: librmclient keeps a wake generation per channel (`crm_win_event_gen`, `crm_win_event_wait_gen`); one thread blocks on the KMD event and moves the generation on, waking the other waiters through a condition variable; wait loops read the generation before checking their value. Needs a librmclient with the symbols, else the shared-event wait |
| 0043 | knobs that undo the wait changes one at a time: `NVK_RM_GPFIFO_ENTRIES`, `NVK_RM_RING_YIELD_MS`, `NVK_RM_HIRES_SLEEP`; with `NVK_RM_WAIT_POLL_MS=10`, `NVK_RM_EVENT_GEN=0`, librmclient's `CRM_EVENT_HIRES=0` and the UMD bridge's `HELIOS_HANDOFF_SLEEP1=1` every wait change can be undone at run time |

## Per-draw cost (common 0001-0007)

CPU-recorded draws on Turing+ avoid the MME and re-emit only what changed.
Steady-state streams (`NVK_DEBUG=push_dump`, `BENCH_NDRAWS=8`):

| case | stream |
|---|---|
| plain draws | 4 methods, 6 dwords, no MME |
| new dynamic UBO offset | 10 methods, 15 dwords: 1 root dword, cbuf bind, draw |
| descriptor set switch | 16 methods, 24 dwords: 2 root dwords (same bank), 2 cbuf binds, draw |
| VB + IB bound per draw | plain methods only when the range changes, 5-dword draw |

GPU ms for 20000 draws (`vk_perf_bench -r 21 -t alu,ubo,desc,rebind,vbib`,
2560x1440, RTX 5090, median of 3 runs) against NVIDIA 610.57.04:

| | plain draws | dyn. UBO offset | set switch | same set rebound, 2 pipelines | VB+IB per draw |
|---|---|---|---|---|---|
| NVK | 0.226 | 0.108 | 0.178 | 0.915 | 0.156 |
| NVIDIA | 0.230 | 0.121 | 0.143 | 2.735 | 0.163 |

The GPU is shared with the desktop and VMs and its clocks follow the load:
absolute numbers move by up to ~15 % between sessions, so compare within a
table.

Correctness: `BENCH_HASH=1 vk_perf_bench -t ubo,desc,rebind,vbib,dynidx,descupd,params,verify`
prints image hashes that match the upstream paths (`NVK_CPU_STATE_TRACKING=0`)
and NVIDIA's driver. `params` mixes direct, indexed, multi-draw and indirect
draws with varying first vertex, vertex offset, first instance and draw
index, 32- and 16-bit index buffers and a clear, and repeats a direct draw
right after each indirect one (dropping one invalidation in the driver
changes its hash); `dynidx` indexes dynamic UBO arrays in two sets at run
time; `descupd` rewrites a descriptor set between submits.

## ZCULL and DXVK depth buffers (common 0008)

- **ZCULL storage.** DXVK gives every D3D11 depth texture `TRANSFER_DST`
  and ends a render pass at every barrier, resolve or render target change,
  so most passes load depth. ZCULL storage is allocated also for depth
  images with `TRANSFER_DST` (`EXCLUSIVE` sharing only), and an empty render
  pass resets it to a conservative state after a copy, blit or resolve
  writes the image or another queue hands it over.
- **ZCULL direction.** Stored ZCULL has to be loaded with the direction it
  was stored with, so `SET_ZCULL_DIR_FORMAT` is fixed per image at its first
  application render pass (GREATER when it clears below 0.5, LESS otherwise)
  and never changed. `vkCmdClearDepthStencilImage` does not count: DXVK
  clears every new depth image to 0.0 that way.
- Uses no MME and no DMEM. Upstream Mesa MR !44088 (ZCULL save and restore
  through MME DMEM, with !44203 and !44414) raises Xid 13
  `DATA_RAM_ACCESS_OUT_OF_BOUNDS` (ESR 0x404490) in the first render pass with
  ZCULL on RM.

GPU time against NVIDIA in the D3D11-through-DXVK shapes of
`vk_perf_bench` (`NVK_RM=1 NVK_UBO_DESC_CBUF=0`, 2560x1440, GPU timestamps,
median of 11):

| test | NVK | NVIDIA |
|---|---|---|
| `zpass`: 32 front-to-back layers over 8 render passes that load depth | 0.109 ms | 0.102 ms |
| same over 2 passes | 0.078 ms | 0.079 ms |
| `zrev`: 32 layers in one pass with reverse Z (clear 0, GEQUAL) | 0.070 ms | 0.071 ms |
| `cb`: 20000 draws, each a new descriptor-buffer offset, 3 VS + 1 PS cbuffer | 0.144 ms | 0.088 ms |
| `cb`: pixel shader with 256 cbuffer reads, 4 fullscreen passes | 0.991 ms | 1.565 ms |

The per-draw cbuffer switch is as cheap as the push stream allows (one root
table dword and the draw); the remaining gap is the shader's bindless cbuf
loads. Bound cbufs (`NVK_UBO_DESC_CBUF=1`) cost far more, because the MME
reads each descriptor from memory; that is why they are off on Windows.

`vk_perf_bench -t zcoh` writes a depth image outside a render pass and
checks that a later pass is not culled by stale ZCULL: copy from buffer,
copy from image, blit and `vkCmdClearDepthStencilImage`, in both directions,
plus three direction-choice cases.

## Compression

GB20x facts (open-gpu-kernel-modules 615.78.08): block-linear memory has
only GMK (kind 0x6), GMK compressible (0x8) and GMK compressible without PLC
(0x9), depth included. Compression state is per physical page (GMMU format
v3 PTEs have no comptag line), so there is no comptag pool to run out of and
COMPR_ANY costs no extra VRAM. The kind must be compressible at allocation
time: a mapping with a compressible kind override over memory whose own
kind is not is quietly downgraded to the uncompressed kind. RM picks 0x8 or
0x9 for the allocation and applies its per-page PLC workaround when it
writes PTEs; NVK maps images with 0x8.

| knob (default) | what it compresses |
|---|---|
| `NVK_RM_COMPRESSION` (1) | images in dedicated allocations: the memory is COMPR_ANY (block linear, 32 bpp, as NVKMS does) and mapped with the image's compressible kind; if RM declines, the uncompressed GMK kind (`patches-windows/0028`, `patches/0016`) |
| `NVK_RM_COMPRESS_ALL` (0) | images in sub-allocated memory (common 0011); vkd3d-proton places every D3D12 resource in a heap, so images are rarely dedicated |
| `NVK_RM_COMPRESS_ZS` (0) | separate depth/stencil images (common 0012): Blackwell splits D24S8 and D32S8X24 into planes, which `nvk_image_can_compress` otherwise rejects; both planes are compressed and the draw path sets `SET_Z_COMPRESSION` and `SET_STENCIL_COMPRESSION` from `is_compressed` |
| `NVK_RM_COMPRESS_TYPE` (0) | images in a second DEVICE_LOCAL memory type (common 0013) |

`NVK_RM_COMPRESS_ALL`: device-local memory that is neither host-visible,
shared nor imported is allocated COMPR_ANY (`NVKMD_MEM_COMPRESSIBLE`,
`nvkmd_info::has_compressible_mem`), and a compressible image bound in it
gets its own VA with the compressible kind and `is_compressed`. Every other
GPU mapping of such memory is compressible too (the memory's own VA 0x8; an
image VA that would be 0x6 is mapped 0x8 with its 3D compression state off),
because compressible pages read through an uncompressed kind return raw
compressed data.

`NVK_RM_COMPRESS_TYPE` builds on 0011's code (and 0012 for depth/stencil)
and is independent of `NVK_RM_COMPRESS_ALL`, which should stay off with it.
It keeps buffers out of compressible memory, as NVIDIA's driver does: the
type is a second DEVICE_LOCAL type on the VRAM heap, before the plain one
(same flags; vkd3d-proton and DXVK take the lowest allowed type).
Optimal-tiling images report it (not sparse, protected, host-transfer,
external or video), sampled-only textures included (mapped 0x8, not
compressed); buffers only when transfer-only, never vertex, index,
indirect, uniform, storage or device-address buffers. In vkd3d-proton that
covers `ALLOW_ONLY_RT_DS_TEXTURES` / `ALLOW_ONLY_NON_RT_DS_TEXTURES` heaps
and sub-allocated committed textures (their global buffer is
`TRANSFER_DST`), not tier-2 mixed heaps (full-usage buffer) or buffer heaps.
A dedicated image that is not compressed itself is never made compressible.

Stays uncompressed in all modes: sparse resources, host-visible types (the
BAR heap, system memory) and sampled-only textures (they have no 3D
compression state). Render targets and UAV textures, typeless and mutable
formats included, are compressed (GB20x compression is generic, so a format
reinterpretation reads the same bytes). An RM refusal of COMPR_ANY leaves the
memory uncompressed (logged, counted). Logs (`mesa_logi`): compressible
memories and MiB at each power of two, image binds that got the compressible
kind, plane binds left uncompressed and why, and a summary at device
destruction.

Diagnostics (common 0015), inert unless set: `NVK_RM_COMPRESS_CLEAR=1`
writes every compressible memory once through its compressible VA at
allocation; `NVK_RM_COMPRESS_UPGRADE=0` keeps uncompressed images in
compressible memory on kind 0x6; `NVK_RM_COMPRESS_TYPE_SCOPE=attachments`
offers the type only to images that are compressed themselves (against the
spec, testing only); `NVK_RM_COMPRESSION=0` turns compression off.

Known issue: Counter-Strike 2 (D3D11 through DXVK) draws a stretched "beam"
with `NVK_RM_COMPRESS_ALL=1` and one agent model's body black with
`NVK_RM_COMPRESS_TYPE=1`, so both stay off. Suspects, separable with the
knobs above: the front end and PBDMA reading indirect arguments from
compressible memory (`ALL` only), stale compression state on reused pages
(RM scrubs freed VRAM without a PTE kind), and sub-allocated compressed
attachments or uncompressed images on kind 0x8.

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

By hand: `git am guest/nvk-rm/patches/*.patch guest/nvk-rm/patches-common/*.patch`
on the base commit, then the two commands above. librmclient is not needed
to build (only its header, and a copy is in patch 0002). The series builds
with `-Dnvk-rm=enabled` and without it (plain nouveau NVK).

The "series already applied" check compares the last patch's subject, so a
Mesa checkout that has an older stack with the same last patch is not
re-patched; use a fresh checkout, or reset its branch to the base first.

Build dependencies (Ubuntu 24.04): meson >= 1.4 (`pip install meson`),
`bindgen-cli` and `cbindgen` (`cargo install`), rustc >= 1.85,
`llvm-20-dev libclang-20-dev libclang-cpp20-dev libpolly-20-dev
libllvmspirvlib-20-dev llvm-spirv-20 libclc-20-dev` (for `mesa_clc`),
`libxshmfence-dev`, plus the usual Mesa deps (libdrm, libelf, wayland, xcb,
glslang, python3-mako/yaml).

## Windows build (cross-compiled)

NVK with the RM backend builds for **Windows x86_64** with MinGW-w64 on a
Linux host: `vulkan_nouveau.dll`, its ICD manifest and `librmclient.dll`.
librmclient's Windows transport (`guest/rmclient`, `src/transport_windows.c`)
sends RM escapes over `HELIOS_ESCAPE_NVRM` to the Helios KMD, which also
provides the CPU mappings, OS-descriptor pinning and event waits.

```sh
guest/nvk-rm/build-windows.sh            # ~/code/mesa-nvk-rm-windows, build dir build-win
guest/nvk-rm/build-windows.sh /path/to/mesa build-dir
MESA_CLC_DIR=/path/to/linux/build/bin guest/nvk-rm/build-windows.sh   # reuse host mesa_clc/vtn_bindgen2
GL=1 guest/nvk-rm/build-windows.sh       # also Zink (libgallium_wgl.dll) and opengl32.dll
ARCH=i686 OUT_DIR=dist32 guest/nvk-rm/build-windows.sh ~/code/mesa-nvk-rm-win32 build-win32   # 32-bit
```

The script applies the series on branch `nvk-rm-windows` with
`git am --3way` (skipped if already applied), builds the native `mesa_clc` +
`vtn_bindgen2` NVK's OpenCL kernels need (or takes them from
`MESA_CLC_DIR`, e.g. a Linux `build-rm`'s `src/compiler/clc` and
`src/compiler/spirv`), cross-builds `librmclient.dll` from `guest/rmclient`,
configures Mesa with `windows/mingw-<arch>.ini` and

```sh
meson setup build-win --cross-file guest/nvk-rm/windows/mingw-x86_64.ini \
    -Dvulkan-drivers=nouveau -Dnvk-rm=enabled -Dgallium-drivers= \
    -Dplatforms=windows -Dllvm=disabled -Dmesa-clc=system -Dprecomp-compiler=system \
    -Dvideo-codecs= -Dvulkan-layers= -Degl=disabled -Dgbm=disabled -Dglx=disabled \
    -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Dshader-cache=enabled \
    -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled -Dxmlconfig=disabled \
    -Dperfetto=false -Dbuildtype=debugoptimized
```

then switches the build to `BUILDTYPE` (default `release`, with `b_ndebug`)
and `-Dvideo-codecs=$VIDEO_CODECS` (default `h264dec`), and stages stripped
DLLs, `nouveau_icd.json` (`library_path` `.\vulkan_nouveau.dll`, relative to
the manifest), `imports.txt` and `exports.txt` in `BUILD_DIR/dist` (unstripped
DLLs in `OUT_DIR/debug`). Compiles run under
`systemd-run --user --scope -p MemoryMax=2500M` with `-j2` by default
(`MEMORY_MAX`, `JOBS`).

Host needs (Ubuntu 24.04): `gcc-mingw-w64-x86-64` (GCC 13, win32 threads),
`rustup target add x86_64-pc-windows-gnu`, meson >= 1.7, bindgen + libclang,
cbindgen; `wine64` only to run tests. For `ARCH=i686`: `gcc-mingw-w64-i686`
(dwarf2) and the `i686-pc-windows-gnu` rustup target for the toolchain meson
uses (`rustup target add --toolchain stable ...` if a `rust-toolchain` file
pins another one). LLVM is off for the Windows build (NAK is Rust and needs
no LLVM; only the host `mesa_clc` does).

The cross file uses **`-mno-ms-bitfields`** (C, C++ and bindgen): MinGW
defaults to MSVC bitfield layout, bindgen only models the GCC one, and NAK
and NIR share structs with mixed-type bitfields between C and Rust (NAK
asserts `sizeof(struct nak_nir_tex_flags) == 4`, which fails with the MSVC
layout). Nothing that crosses the DLL boundary has mixed-type bitfields
(Vulkan API, RM parameter structs).

`vulkan_nouveau.dll` exports `vk_icdGetInstanceProcAddr`,
`vk_icdGetPhysicalDeviceProcAddr` and `vk_icdNegotiateLoaderICDInterfaceVersion`
(plus the `helios_icd_interface` table); `librmclient.dll` exports every
`crm_*`. No MinGW runtime DLL is needed (`-static-libgcc`, no libstdc++ or
winpthread). `windows/icd_smoke.c` loads the driver as the loader does
(`vk_icdNegotiate...`, instance extensions, `vkCreateInstance`,
`vkEnumeratePhysicalDevices`).

Linux-only code and how the Windows build handles it:

| where | Linux-only thing | on Windows |
|---|---|---|
| `nvkmd_rm_lib.c` | `dlopen`/`dlsym` | `LoadLibrary`/`GetProcAddress`, `librmclient.dll` next to `vulkan_nouveau.dll` first |
| `nvkmd_rm_mem.c` | anonymous `mmap` + `MADV_DONTFORK` for OS-descriptor pages | `crm_alloc_pages` (librmclient: `VirtualAlloc`) |
| `nvkmd_rm_dev.c` | `poll()` on the non-stall event fd, `sched_yield` | `crm_event_wait` (`-ENOSYS` from a transport without events: waits sleep with backoff), `thrd_yield` |
| `nvkmd_rm_pdev.c`, `_mem.c`, `_dev.c` | nvidia-drm node discovery (libdrm, `stat`), `/dev/nvidiactl` export fds, PRIME/GEM ioctls, `lseek` on dma-bufs | `nvkmd_rm_drm.c` is not built; `nvkmd_rm_win.c` exports through the KMD (see "Presentation on Windows") |
| `nvkmd.c`, `nvkmd/nouveau/*`, `winsys/*` | nouveau DRM backend, libdrm | not built (`with_nouveau_drm`); `nouveau_device_limits.c` (chipset tables) is built |
| `nvk_physical_device.c` | `major()`/`minor()` of DRM nodes, `VK_EXT_physical_device_drm` | compiled out / extension off |
| `nvk_device.c` | `vk_drm_syncobj_copy_payloads` | compiled out |
| `nvk_instance.c` | ELF build-id | module timestamp hash |
| `nv_cubin.c` | libelf | stub, CUDA modules rejected |
| `nak_bindings.h` | `xf86drm.h`, nouveau winsys | left out (hardware tests only) |
| `nil.h` → `drm_fourcc.h` → `drm.h` | `<sys/ioccom.h>` | empty header in `src/nouveau/compat/win32` |

External memory/semaphore fd extensions are gated on `has_dma_buf`, which is
false on Windows.

## Running on Windows

Put `vulkan_nouveau.dll`, `librmclient.dll` and `nouveau_icd.json` in one
directory; the test programs come from `windows/build-tests.sh`. NVK is the
Helios adapter's Vulkan driver once installed through the driver package;
for a private copy:

- The Vulkan loader ignores `VK_DRIVER_FILES` / `VK_ICD_FILENAMES` (and
  `VK_ADD_DRIVER_FILES`) in an elevated process, which an administrator's
  ssh session is. The tests therefore take
  `VK_DIRECT_DRIVER=C:\...\vulkan_nouveau.dll` and hand the ICD to the loader
  through `VK_LUNARG_direct_driver_loading` (exclusive mode,
  `tests/vk_direct_driver.h`). From a normal desktop session
  `VK_DRIVER_FILES` works as on Linux.
- Without `librmclient.dll` next to the driver, NVK reports that librmclient
  could not be loaded.
- Windows is the only OS where `NVK_RM` defaults to on; `HELIOS_ICD=venus`
  hides NVK from the process.

Tests (`tests/`, `windows/`):

| test | checks |
|---|---|
| `icd_smoke` | loads the DLL without the loader: negotiation, instance, one physical device |
| `vk_summary` | device, heaps, queue families, extensions through the loader |
| `vk_compute_test` | compute on host-visible and device-local buffers (+ copy) |
| `vk_offscreen_test` | offscreen triangle, many submits |
| `vk_bar_test` | host-visible VRAM: CPU write, GPU read/write, CPU read; heap limit |
| `vk_coherence_test` | CPU/GPU coherence on every host-visible type (BAR and cached system memory), per iteration a new pattern |
| `vk_bl_readback` | block-linear scanout image layout, see "Block-linear scanout" |
| `vk_scanout_present` | spinning triangle on a hidden window through the scanout WSI: `[seconds] [width height] [images]` |
| `vk_rmfence_test` | RM fences never fire before the submit's own fence; present pipelining |
| `vk_video_probe`, `vk-video-decode.ps1` | NVDEC decode |
| `vk_loader_list` | what an ordinary app sees through the system loader |
| `vk_lost_test` | device loss: moves the loss epoch and checks for `VK_ERROR_DEVICE_LOST`, no hang, clean teardown |
| `helios_icd_test` | the Helios ICD interface without the UMD: `deviceLUID`, a dedicated exported image, its KMD resource id, optional scanout |
| `helios_share_test` | a shared surface between two NVK processes |
| `wgl_test` | Zink: `glReadPixels` frame, GL 4.3 compute, gears with swap interval 0 |

librmclient's own tests (`test_unit`, `crm_smoke`, `crm_pin_smoke`,
`crm_event_smoke`, `crm_semsurf_smoke`, ...) are in `guest/rmclient/tests`.

## Presentation on Windows

### Zero-copy scanout (Helios WSI)

On the Helios adapter the Win32 WSI does not copy frames to a window. The
swapchain images are images in dedicated VRAM that NVK makes presentable
once, the way patch 0013 makes a dma-buf on Linux, except that librmclient's
Windows transport carries the DRM side to the host:
`NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` into a fresh control channel
(`crm_win_open_device(255)`), `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on host
render node `NVK_HELIOS_DRI` (`crm_win_open_device(512 + n)`), one GEM handle
per image (`nvkmd_rm_win.c`). The window is never used, so a hidden window
on an invisible desktop (an ssh session) presents fine.

- KMD scanout source (`guest/windows/docs/foreign-scanout.md`):
  `crm_win_scanout_set` once per layout (render node, size, stride,
  `XRGB8888`/`XBGR8888`, modifier), `crm_win_scanout_present` with the
  image's GEM handle per frame (set again on `-ENOENT`, i.e. the source
  lapsed), `crm_win_scanout_release` when a swapchain that presented is
  destroyed and with the device. The KMD sends the ScanoutFlip itself and
  keeps the desktop's own flips off scanout 0. An older KMD (`-ENOSYS`)
  gets `crm_win_scanout_flip` (raw ScanoutFlip through FORWARD).
- Unpaced by default; `MESA_WSI_SCANOUT_HZ=N` caps presents per second with
  a high-resolution waitable timer.
- Formats B8G8R8A8 and R8G8B8A8 (UNORM/SRGB) as XRGB8888/XBGR8888.
- `NVK_HELIOS_WSI=0`, a device without the `crm_win_*` entry points or
  images that cannot be exported: the swapchain presents through GDI (a CPU
  copy, which fails with `VK_ERROR_MEMORY_MAP_FAILED` on an invisible
  desktop). The first refused export per device logs why.
- Under a DWM on NVK the desktop is composed by DWM, so window swapchains do
  not take scanout (`NVK_HELIOS_WSI_SCANOUT=1` forces it); see "Composed
  present".
- Scanout surfaces report `minImageCount` 3: one image on display, one
  being released, one to draw into. A one-image chain gets its image back
  at once.

### Block-linear scanout

NVK cannot render into a linear color image; a linear swapchain image would
need a hidden tiled shadow plus a full-screen copy per render pass. The
scanout images instead keep NVK's own tiling, named by NVIDIA's DRM format
modifier, which the host's display path reads as such (NVIDIA's driver
imports NVK-on-RM block-linear memory pixel-exact).

- The WSI asks the driver for its uncompressed NVIDIA block-linear 2D
  modifiers for the format, filters them by usage and extent (as
  `wsi_common_drm.c` does) and creates the images with
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and that list. NVK picks the
  tallest block, `0x0300000000606015` (kind 0x06, GOB kind generation 2,
  sector layout 1, h = 5) for B8G8R8A8 and R8G8B8A8 on GB20x.
- `scanout_export` returns the modifier the image got
  (`vkGetImageDrmFormatModifierPropertiesEXT` is Linux-only in the runtime).
  The dedicated allocation carries the image's PTE kind and tile mode, and
  the export passes them to NVKMS on `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` as
  block-linear surface params (`log2GobsPerBlock.y`, `genericMemory`); it
  refuses memory whose layout is not the modifier's.
- `SCANOUT_SET` carries the modifier; the KMD checks size, fourcc and
  `stride >= width * 4` only. The stride is the GOB-aligned row pitch (7680
  at 1920).
- Fallback to linear: `NVK_HELIOS_WSI_LINEAR=1`, or a `SCANOUT_SET` refused
  with `-EINVAL` for a non-linear modifier. The flip then returns
  `VK_ERROR_FORMAT_NOT_SUPPORTED`, and the WSI goes linear for later
  swapchains and returns `VK_ERROR_OUT_OF_DATE_KHR`. Linear swapchain images
  rendered with a depth buffer use NVK's tiled shadow (patch 0033).

`tests/vk_bl_readback.c` builds the WSI's image (B8G8R8A8, the modifier list,
dedicated device-local memory), writes a `(y << 16) | x` pattern with the
copy engine, clears an odd rectangle across GOB and block edges with
`vkCmdClearAttachments` (3D engine, image as color target), reads the raw
bytes back through a buffer bound to the same memory and checks every pixel
at the address the modifier gives it (NIL's TuringColor2D GOB). Reading
`...6015` as `...6014` or as linear fails, as it must.

### Presents on RM fences

A present on the Helios scanout does not wait on the CPU for the frame's
GPU work: it hands the flip an RM fence and the presenting thread returns at
once (KMD side: `guest/windows/docs/rm-fence-marker.md`).

1. **Present timeline.** Per queue that presents, a 64-bit semaphore that is
   one entry (32 bytes on GB20x, value at offset 0) of an RM
   `NV_SEMAPHORE_SURFACE` (class 0xda, under the subdevice) over 4 KiB of
   RM-allocated system memory (`NVKMD_RM_MEM_RM_SYSMEM`: RM maps it itself, an
   OS descriptor is refused). It is an ordinary `nvkmd_rm_sync` signalled
   with the usual `SEM_EXECUTE` release + `NON_STALL_INTERRUPT`; a context
   binds its channel to the surface before its first release into it
   (`NV_SEMAPHORE_SURFACE_CTRL_CMD_BIND_CHANNEL`, notifier
   `NV2080_NOTIFIERS_FIFO_EVENT_MTHD`), so RM checks the surface's waiters.
2. **Fence context.** The entry is imported on the host render node with
   nvidia-drm `SEMSURF_FENCE_CTX_CREATE` (0x54), once per timeline.
3. **Per present.** `vk_queue_signal_sync()` of the timeline's next value
   after everything submitted to the queue so far, then
   `SEMSURF_FENCE_CREATE` (0x55, `timeout_ms` 5000) for that value: a backend
   handle, recorded by the KMD as a fence of librmclient's NVRM device, which
   fires one `EventReady` when the GPU writes the value (or with an error
   after 5 s).
4. **Flip** (`nvkmd_rm_mem_scanout_flip_fenced()`). With
   `QueryCaps.supported_ops` bit 32 (`HELIOS_NVRM_CAP_SCANOUT_FENCE`),
   `SCANOUT_PRESENT` carries the handle (flag `RM_FENCE`, offset 52) and the
   KMD sends the flip from its worker when the fence fires (FIFO per source,
   ready prefix coalesced); on `QUEUE_FULL` (8 waiting) NVK waits for its own
   fence and retries, on any other refusal the flip thread takes over for
   good. Without the bit a flip thread per device waits on the fence
   (`EVENT_REGISTER`, no polling) and sends a plain `SCANOUT_PRESENT` in
   order, at most 8 frames behind. Without DRM fences on the host (config bit
   11), with an older librmclient or `NVK_RM_FENCE=0`: the CPU wait.

Consumers: the Win32 WSI (`wsi_device::win32.scanout_flip_fenced`; a scanout
swapchain then skips the CPU wait, `wsi_swapchain::gpu_ordered_present`) and
`helios_icd_interface` v3+, which the Helios UMD uses with DXVK's queue taken
under `lockSubmission()`. librmclient provides `crm_win_caps`,
`crm_win_semsurf_ctx_create`, `crm_win_semsurf_fence_create`,
`crm_win_fence_wait` and `crm_win_scanout_present_fenced` (optional symbols).
`NVK_RM_FENCE_KMD=0` never hands fences to the KMD.

Limits: the timeline is signalled on the present queue, so the flip is
ordered after work submitted to that queue; work of another queue is covered
when the present waits on it with a semaphore (on Windows NVK has no
`copy_sync_payloads`, so the WSI's pre-present submit really waits on the
present queue, before the timeline signal). A fence costs one escape (60-80
us): a trivial frame is faster with the CPU wait, any frame with real GPU
work is faster fenced. A fence for a value already reached fires after up to
~1 ms (the backend's event pump finds it on its 1 ms sweep); fences made
while the GPU still works fire within ~0.1 ms of the write.

### Image release

With KMD 22.22.315+ on a backend with `NVGPU_F_SCANOUT_RELEASE`
(`QueryCaps.supported_ops` bit 35; `guest/windows/docs/foreign-scanout.md`,
"Buffer release") the KMD reports when the host is done with a flipped
image, so an acquired image is one the host has stopped reading rather
than one shown two presents ago:

- every flip through the KMD's source (plain or fenced) records the
  `out_seq` it returned on the memory; a flip still queued in the flip
  thread has no seq yet, so a wait on that memory first waits for the thread;
- `vkAcquireNextImageKHR` takes the idle image shown longest ago and blocks
  (outside the swapchain lock) until `SCANOUT_STATUS`'s released floor
  reaches that image's seq, woken by the `SCANOUT_RELEASED` event (kind 3,
  handle 0) with the reset/ask/wait order that loses no wake (librmclient
  `crm_win_scanout_wait_released`). The wait honours the app's timeout
  (`VK_TIMEOUT`/`VK_NOT_READY`, the image stays idle) and is capped at 1 s:
  past the host's 500 ms forced release the image is written anyway and
  counted. A lost transport counts as released;
- without bit 35 (an older KMD or a host without bit 15) the two-presents
  rule applies. `NVK_SCANOUT_RELEASE=0` forces it.

### Composed present

Under a DWM on NVK the desktop is composed by DWM, so a window swapchain
cannot take scanout. With `NVK_HELIOS_WSI_COMPOSE` (default on there) the
WSI builds, per window, a D3D11 device on the Helios adapter
(`MESA_WSI_COMPOSE_ADAPTER` overrides the pick) and a DXGI flip swap chain
with three buffers (`wsi_common_win32_compose.cpp`):

- each swapchain image is a D3D11 shared texture whose resource id and layout
  the WSI reads with `D3DKMTOpenResource` (`HeliosWddmOpenIdentity`,
  `HeliosWddmAllocLayout`) and NVK imports by resource id;
- a present enqueues the frame on a per-swapchain thread
  (`MESA_WSI_COMPOSE_THREAD`) that waits for the frame's fence, copies the
  texture into the back buffer and calls `Present` (sync interval 1 for FIFO,
  0 for IMMEDIATE/MAILBOX); the present ends an event query after the copy and
  the acquire waits for the chosen image's copy (outside the swapchain lock,
  within the app's timeout; an infinite acquire gives up after 2 s with a log
  line);
- with a Helios present path the surface offers IMMEDIATE, MAILBOX and FIFO
  (`MESA_WSI_WIN32_FIFO_ONLY=1`: FIFO only); IMMEDIATE and MAILBOX show only
  the newest queued frame and hand older ones back unshown;
- a new swapchain stops its retired predecessor's present thread before the
  window's DXGI swap chain is resized; `ResizeBuffers` runs under the
  compose lock;
- every failure is logged (`MESA-WSI: compose: ...`, also in
  `helios-wsi-<pid>.log`) and falls back to GDI.

The KMD's copy-engine present (`guest/windows/docs/rm-copy-engine-present.md`)
takes `queue_rm_fence_v3`'s semaphore and image description for windowed Blt
presents.

## Host-visible VRAM

`patches-windows/0022` (Linux: `patches/0014`) adds the DEVICE_LOCAL |
HOST_VISIBLE | HOST_COHERENT type on a heap of its own, backed by VRAM the CPU
maps through BAR1, as NVIDIA's driver and NVK on nouveau without ReBAR do. It
is off by default (`NVK_RM_BAR_MB=0`): DXVK writes its constant, dynamic and
descriptor buffers there, and CPU writes to BAR1 go through the host-visible
window every CPU mapping in the guest shares, which costs more in CS2 than
the PCIe reads of GPU-cacheable system memory.

- Sizing: `bar_size_B` is BAR1 from `NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE`,
  kept one big page below VRAM so it never reads as a full ReBAR;
  `NVK_RM_BAR_MB=-1` takes all of BAR1, N caps it at N MiB. The heap is not
  a hard limit: an allocation past it goes to system memory, and so does one
  whose CPU map the host refuses (the shared window is full, or the KMD's
  per-process share of it is used up). The VRAM is freed and the allocation
  gets OS-descriptor system pages, which take no window space; the app still
  sees the same memory type. The first such fallback per device logs
  `NVK: host-visible VRAM: CPU map of N MiB failed ... using system memory`
  (`NVK_DEBUG=vm`: every one).
- Mapping: each allocation from the type is vidmem allocated without
  `MAP_NOT_REQUIRED` and mapped once (`crm_map_memory`, write-combined in the
  KMD) at allocation until freed; internal and client maps alias that
  mapping, so mapping per frame costs nothing. CPU reads through the
  write-combined mapping are slow.
- Only that type lands in the BAR: `nvkmd_info::host_visible_vram_is_pinned`
  makes NVK ask for `NVKMD_MEM_VRAM` for it, while NVK's own
  `LOCAL | CAN_MAP` buffers (push, queries, events, upload) stay in system
  memory, where the CPU also reads them fast, unless `NVK_RM_UPLOAD_VRAM` /
  `NVK_RM_DESC_TABLE_VRAM` (default 0) move upload memory and descriptor
  tables into the heap.
- Host-visible system memory (`NVK_RM_SYSMEM_CACHED`, default on) is mapped
  GPU-cacheable (`NVOS46_FLAGS_GPU_CACHEABLE_YES`) and every submit starts
  with an L2 sysmem invalidate after its semaphore waits; rings, semaphores,
  query reports, pushbuffers and shared memory stay uncached. The GPU then
  reads DXVK's dynamic buffers in system memory at NVIDIA's speed.
- `tests/vk_bar_test.c` (`vk_bar_test bar.comp.spv 4194304 fill`) checks CPU
  write, GPU read/write, CPU read, the heap limit and fallback;
  `tests/vk_coherence_test.c` checks coherence on every host-visible type.

## Shader cache on Windows

Mesa has no Windows disk cache of its own; `patches-windows/0026` implements
the multi-file cache (the default type) and `build-windows.sh` enables it
(`-Dshader-cache=enabled`; the option is auto-disabled on other Windows
builds such as MSVC or dozen). NVK's `vk_pipeline_cache` falls back to the
physical device's disk cache, keyed by the driver build id (Mesa version +
DLL timestamp) and the compiler flags (including `NVK_UBO_DESC_CBUF`). DXVK
keeps no state cache of its own, so persistence is this cache's job.

- Location: `MESA_SHADER_CACHE_DIR`, else `%LOCALAPPDATA%`, else `%TEMP%`,
  plus `\mesa_shader_cache`: per user, writable without setup.
  `MESA_SHADER_CACHE_DISABLE=1` turns it off.
- The index is a file mapping shared by all processes, like the `MAP_SHARED`
  mmap on Linux.
- A new entry is written to a `.tmp` file opened with no sharing (the
  `flock`), then renamed to its final name while still open
  (`FileRenameInfo`, never replacing), so no reader sees half an entry.
- Eviction deletes the least recently used tenth of a random subdirectory
  (1 GiB default size limit).
- Entries are stored uncompressed (the MinGW build has neither zlib nor
  zstd).
- The single-file and database caches stay unimplemented on Windows.
- NVK exposes `VK_EXT_graphics_pipeline_library` (with
  `graphicsPipelineLibraryIndependentInterpolationDecoration`), which DXVK
  turns on by default (`dxvk.enableGraphicsPipelineLibrary = Auto`).

## Helios integration

`vulkan_nouveau.dll` exports `helios_icd_interface` (a verbatim copy of
`guest/windows/protocol/include/helios_icd_interface.h`), the backend-neutral
table the Helios D3D11 and D3D12 UMDs use to run DXVK and vkd3d-proton on NVK
(`guest/windows/docs/dxvk-on-nvk.md`). Versions are appended to one export:

| version | adds |
|---|---|
| 2 | `memory_res_id` (exports the dedicated memory of an image to a GEM object of the host render node, creates one Venus holder context per process and asks the KMD for a resource id with `FOREIGN_RESOURCE IMPORT_RM`, passing pitch or block-linear modifier, row pitch and offset; the id is released with the memory unless a WDDM allocation adopted it), `scanout_present`, `deviceLUID` |
| 3 | `queue_rm_fence`, `rm_fence_wait`, `rm_fence_close`, `scanout_present_fenced`; caps `RM_FENCE`, `SCANOUT_FENCE_KMD`, `PRESENT_FENCE_KMD` |
| 4 | `HELIOS_ICD_CAP_LAYOUT_FORMATS`, `memory_res_plane1` (two-plane formats) |
| 5 | `scanout_frame` (`out_seq` and `out_generation` of the latest scanout frame; `VK_NOT_READY` without a live KMD source) |
| 6 | `queue_rm_fence_v3` (`queue_rm_fence` plus the producer's semaphore and the presented image's RM memory and layout; `VK_INCOMPLETE` = the fence without a description) |
| 7 | `ecl_fence_reserve`, `ecl_fence_create`, `ecl_fence_signal`: D3D12 `ExecuteCommandLists` orders the runtime's WDDM context behind the GPU work with an RM fence named before vkd3d has submitted the batch; the timeline is keyed by the caller (bit 0 set, so it never shares a present timeline's entry) |

`memory_res_id` needs librmclient's `crm_win_adapter_luid`,
`crm_win_venus_ctx_create`, `crm_win_foreign_caps`, `crm_win_import_rm` and
`crm_win_release_blob`; without them, or with the KMD's `IMPORT_RM` gate
closed, it answers `VK_ERROR_FEATURE_NOT_PRESENT`. Shared images are
uncompressed and carry their block-linear kind and tile mode into the
export (`HELIOS_STRUCTURE_TYPE_EXPORT_MEMORY_RESOURCE_INFO`).

Shared surfaces: the resource id is the only name that crosses processes
(the KMD writes it, with the layout it recorded at `IMPORT_RM`, into the
opener's WDDM open and never hands out the creator's RM handles). An opener
passes `HELIOS_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO` to
`vkAllocateMemory` with a dedicated image; librmclient's
`crm_win_rm_resource_import` (KMD `FOREIGN_RESOURCE RM_RESOURCE_IMPORT`) has
the host make the resource's memory a GEM object of the opener's render node,
which NVK imports like any nvidia-drm dma-buf (`GEM_EXPORT_NVKMS_MEMORY` to a
fresh control channel, `OS_UNIX_IMPORT_OBJECT_FROM_FD`), closes the GEM
handle and maps as shared (uncompressed VA). The image must already have the
creator's layout, which `VK_EXT_image_drm_format_modifier`'s explicit
pitch/offset create info provides (patch 0042). The import is on the device's
memory list like every other.

Process policy: on Windows the RM device can only be the Helios adapter's.
Whether a process gets NVK or Venus is decided once per process by
`helios_policy_decide()` from Conduit's `helios_icd_policy.h` (the same
code the UMDs use; `build-windows.sh` adds the include path to
`rmclient.pc`): `HELIOS_ICD` first, then the desktop follows an NVK DWM
unless `NvkDenyList` names the executable (`DesktopFollowsDwm=0` turns that
off), then `HKLM\SOFTWARE\Helios` `Icd=venus` unless `NvkAllowList` /
`NvkDefaults` names it, then the deny-lists (built-in: DWM, the shell,
browsers, ...). The decision and its reason are logged once, and exported as
`nvk_helios_process_allowed()` for Zink. A denied process gets Venus from the
loader, and a Vulkan instance in DWM on Venus never opens an RM client.

Device loss: a Windows device whose KMD went away (a live driver update or
device restart under a running process) reads its KMD views as zero pages
and reports `VK_ERROR_DEVICE_LOST` before touching a mapping (patch 0044);
after a loss librmclient sends no escape, and the process needs restarting to
use RM again. A stale Helios holder context after a KMD restart is renewed
and the import retried once.

## OpenGL on NVK: Zink

`GL=1 guest/nvk-rm/build-windows.sh` also builds Zink as Mesa's gallium WGL
ICD (`libgallium_wgl.dll`) and Mesa's `opengl32.dll` from the same tree
(`-Dgallium-drivers=zink -Dopengl=true`). Zink loads NVK itself and uses its
`vk_icdGetInstanceProcAddr` (the loader-less path the Helios D3D UMD takes for
DXVK), so the Vulkan loader and the Venus ICD are not involved. The ICD is,
in order:

- `ZINK_VULKAN_ICD`: a DLL path, or `loader`;
- `HKLM\SOFTWARE\Helios` `NvkIcdPath` (`NvkIcdPath32` for 32-bit);
- `vulkan_nouveau.dll` next to the module (32-bit: `vulkan_nouveau32.dll`
  first), the driver-store layout;
- `%ProgramFiles%\Helios\nvk\vulkan_nouveau.dll`.

Unless `ZINK_VULKAN_ICD` named it, the Helios process policy applies: a
process it sends to Venus keeps the loader (`HELIOS_ICD=venus` sends Zink to
Venus). Installed through the driver package, the adapter's
`OpenGLDriverName` points at the WGL DLL in the driver store, next to NVK.
Zink's default framebuffer is a linear swapchain image rendered with a depth
buffer, which needs patch 0033's tiled shadow; its swapchain asks for the
surface's `minImageCount`, which is 3 for scanout surfaces.

Registered as a Vulkan ICD (a manifest next to `vulkan_nouveau.dll` under
`HKLM\SOFTWARE\Khronos\Vulkan\Drivers`), NVK is what an ordinary app sees
through the system loader (`windows/vk_loader_list.c`): NVK first, with the
Helios adapter's LUID (librmclient's `crm_win_adapter_luid`), Venus second.
With `HELIOS_ICD=venus` NVK enumerates nothing and the app gets Venus.

## D3D11 games through DXVK in a private app copy

D3D11 -> DXVK -> NVK -> RM runs without touching the Helios WDDM driver or
registering anything system-wide; every file sits next to the game's
executable (`windows/run-heaven-nvk.bat`, `heaven-nvk-fps.ps1` and
`heaven-cache-run.ps1` do this for Unigine Heaven).

- **A 32-bit (WoW64) game needs a 32-bit NVK and librmclient**; the 64-bit
  `vulkan_nouveau.dll` fails to load in it (`LoadLibrary` error 193), and the
  shim falls back to the system loader, so DXVK silently runs on the Venus
  ICD. Check `logs\nvk-shim.log` and the DXVK log ("Found device: ... (NVK
  GB202)"). The RM transport works from WoW64 as it is: the escape ABI is
  pointer-free and the KMD's user mappings land below 4 GiB.
- `vulkan-1.dll` (`windows/vulkan_shim.c`) forwards to the system loader and,
  in `vkCreateInstance`, chains `VK_LUNARG_direct_driver_loading` (exclusive)
  with the `vulkan_nouveau.dll` next to it, which sidesteps the loader
  ignoring `VK_DRIVER_FILES` in elevated processes. `NVK_SHIM_FRAMES=file`
  logs every `vkQueuePresentKHR`; it is the only frame clock for an app-local
  `dxgi.dll`, which emits no DXGI ETW events, so PresentMon sees nothing.
- `NVK_SHIM_DRIVER` in `env.cmd` picks the driver DLL.

Build (host):

    ARCH=i686 OUT_DIR=dist32 guest/nvk-rm/build-windows.sh ~/code/mesa-nvk-rm-win32 build-win32
    # DXVK: the Helios fork, built plain with MinGW, no source changes
    # (its Helios hooks find no helios_* exports outside helios_umd.dll and stay off):
    cd guest/windows/third_party/dxvk && meson setup build32 --cross-file build-win32.txt \
        --buildtype release --strip -Denable_d3d8=false -Denable_d3d9=false \
        -Denable_d3d10=false -Db_vscrt=none && ninja -C build32
    guest/nvk-rm/windows/stage-dxvk-app.sh i686 dist32 <dir with the fork's d3d11.dll+dxgi.dll> stage

Guest: copy the game to a private directory, put the staged files in its
`bin\` and `run-heaven-nvk.bat` + `heaven-nvk-fps.ps1` in its root.
`run-heaven-nvk.bat [dir [w h [prof]]]` runs it on the desktop; an optional
`env.cmd` next to it is `call`ed first (e.g. `set NVK_HELIOS_WSI=0`).
`heaven-nvk-fps.ps1 [-Seconds 30] [-Warmup 45] [-Width] [-Height] [-Prof]`
starts it through a scheduled task in the user's session, prints fps,
frame-time median/p99 and 5 s buckets, and stops it by PID; `-Prof` adds
librmclient's per-escape table (`CRM_WIN_PROF_FILE`, which also counts event
waits) for the timed window. `heaven-cache-run.ps1` records the first 45 s
after launch, for cold and warm shader cache comparisons.

## Linux guests

### Running

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
- `NVK_DEBUG=vm` prints what RM reported for the GPU (classes, VRAM, GPCs),
  the DRM node used for dma-bufs (with its page kind, kind generation and
  sector layout), every export and every VA operation;
  `NVK_DEBUG=push_sync,push_dump` syncs and dumps every submit.
- A GNOME session in a Conduit guest exports `VK_DRIVER_FILES` (NVIDIA's
  ICD), which takes precedence over `VK_ICD_FILENAMES`: from a desktop
  terminal set `VK_DRIVER_FILES` to NVK's ICD as well, or apps silently run
  on NVIDIA's driver.
- Presentation is zero-copy whenever an nvidia-drm node is found;
  `MESA_VK_WSI_DEBUG=sw` forces the software WSI (CPU copy per frame), and
  `NVK_RM_DMABUF=0` turns dma-bufs off altogether (no external memory
  extensions, software WSI).

`guest/nvk-rm/nvk-cube.sh` does all of that (from ssh it picks the desktop
session's `DISPLAY=:0`, Xwayland auth and `wayland-0`):

```sh
nvk-cube.sh                      # vkcube --wsi xcb, zero-copy
nvk-cube.sh --wsi wayland        # Wayland, zero-copy
nvk-cube.sh --sw [...]           # software WSI
nvk-cube.sh --present_mode 0 --c 5000 --width 1920 --height 1080
```

### Zero-copy presentation

```
NVK (RM backend)                         guest kernel (conduit_gpu)            host
NV01_MEMORY_LOCAL_USER (VRAM) ──OS_UNIX_EXPORT_OBJECT_TO_FD──► fresh /dev/nvidiactl fd
          │                              (fd swapped for the backend's)
          └─DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on renderD128 ──► forwarded ──► host nvidia-drm GEM object
                                         guest GEM proxy ◄───────────────────┘
                     PRIME_HANDLE_TO_FD ─► dma-buf (guest)
                                              │ DRI3 PixmapFromBuffers / zwp_linux_dmabuf_v1
                                              ▼
                     Xwayland / gnome-shell (NVIDIA EGL/GBM): PRIME_FD_TO_HANDLE,
                     GEM_EXPORT_NVKMS_MEMORY ─► RM import into their client: same VRAM
                                              │ composited frame, guest KMS flip
                                              ▼
                     ScanoutFlip ─► backend PRIME export ─► conduit-viewer (docs/SCANOUT.md)
```

- **Export** (`nvkmd_rm_mem.c`): on the first `vkGetMemoryFdKHR` the memory
  object is exported to a freshly opened `/dev/nvidiactl`
  (`NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD`, 0x3d05), nvidia-drm imports
  that descriptor (`DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` with an
  `NvKmsKapiPrivImportMemoryParams`: block-linear or pitch, log2 GOBs per
  block from the image's tile mode) and the GEM handle is kept for the
  memory's lifetime, so every export is the same dma-buf. This is exactly
  how NVIDIA's own userspace hands RM memory to nvidia-drm, so Conduit's
  guest module and host backend already forward all of it (the fd inside
  the nested parameters is swapped for the backend's on both routes).
  Exportable system memory is RM's `NV01_MEMORY_SYSTEM`, never our own
  OS-descriptor pages.
- **Import**: `PRIME_FD_TO_HANDLE`, `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY` to a
  fresh `/dev/nvidiactl`, `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD`
  (0x3d06) into our client under a `crm_new_handle` handle, placement from
  `NV0041_CTRL_CMD_GET_SURFACE_INFO`; mapped into our VA space like our own
  memory. dma-bufs from other drivers fail at `GEM_EXPORT_NVKMS_MEMORY`.
- **DRM node** (`nvkmd_rm_pdev.c`): the DRM device at the GPU's PCI address
  (Conduit's PCI mirror `0010:01:00.0`, which RM also reports), named
  `nvidia-drm` by `DRM_IOCTL_VERSION`, answering `DRM_NVIDIA_GET_DEV_INFO`.
  Its render/primary dev_t become `VK_EXT_physical_device_drm`, so Mesa's
  WSI recognises the X server's DRI3 device and the compositor's
  linux-dmabuf main device as ours (no prime blit).
- **Modifiers**: `VK_EXT_image_drm_format_modifier` with NIL's list. For
  32 bpp color on GB202 that is `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1,
  2, 0x06, h)` = `0x030000000060601h` (h = log2 GOB height 0..5), plus
  LINEAR. nvidia-drm reports generic kind 0x06, kind generation 2, sector
  layout 1 for this GPU, which is what Conduit's guest KMS advertises and
  NVIDIA's EGL/GBM pick from, so the lists intersect; vkcube's 500x500
  images use h = 5 (`0x0300000000606015`). Kinds are still applied per
  mapping (the image's own VA); `has_alloc_tiled` only records the layout.
- **Sync**: RM attaches no fences to dma-bufs and our syncs cannot become
  sync_files, so the WSI waits on the CPU for each image's rendering before
  presenting it (`wait_before_present`, patch 0012). The compositor's reads
  are ordered by the WSI's buffer release / Present idle events.
  vk_sync_binary's emulated sync_file import/export is removed for RM: the
  WSI took it for real support and failed the first acquire.

No guest kernel module change and no host change is needed.

Properties checked with `vkcube` and the tests:

- `vkcube --wsi xcb` and `--wsi wayland` render correctly (the window read
  back through Xwayland's glamor, i.e. NVIDIA's EGL importing NVK's
  block-linear dma-buf); `NVK_DEBUG=vm` shows the swapchain images exported
  with kind 0x6, tile 0x50.
- No CPU copy: over 200 frames at 1920x1080, vkcube writes 23 KB in total to
  its sockets on the zero-copy path and one 8 MB `PutImage` per frame with
  `--sw`.
- With FIFO, X11 runs at the display rate (240 Hz). Wayland FIFO paces at
  ~235 fps for a 500x500 window but ~60 fps for larger ones, as it does for
  NVIDIA's own Vulkan driver: that is gnome-shell's frame-callback pacing.
  IMMEDIATE is not offered on Wayland.
- `tests/vk_dmabuf_test.c`: device A fills a device-local exportable buffer
  and exports it, device B (a second RM client) imports the dma-buf and
  reads back all 1 Mi values; two exports are the same dma-buf
  (`cc -O1 tests/vk_dmabuf_test.c -lvulkan -o vk_dmabuf_test`).
- With `conduit view <guest>` open, the viewer shows the cube at the display
  rate, all dma-buf ATTACH/COMMIT. Those are gnome-shell's composited
  frames: neither NVK's nor NVIDIA's vkcube gets direct scanout from
  gnome-shell, even fullscreen; the chain still has no CPU copy.

### Linux checks

Build on the host (`build.sh`), copy `build-rm/` and `librmclient.so` into
the guest, then:

1. **Enumeration and device creation**: `NVK_RM=1 NVK_DEBUG=vm vulkaninfo`
   lists the GB202 through NVK (`NVIDIA GeForce RTX 5090 (NVK GB202)`,
   `DRIVER_ID_MESA_NVK`, PCI 0x10de:0x2b85; 3D 0xce97, compute 0xcec0, copy
   0xcab5, GPFIFO 0xca6f, usermode 0xc761) with the VRAM limit and no VA
   colliding with RM's [4 GiB, 4.5 GiB). Without `NVK_RM` NVK declines; with
   both ICDs one process sees NVIDIA's driver and NVK side by side. Watch
   `conduit logs <guest>` for allowlist refusals.
2. **Submission**: `NVK_DEBUG=push_sync,push_dump` on a trivial compute
   dispatch; GP_GET must advance and the context semaphore must complete.
3. **Compute**: `tests/vk_compute_test.c` (host-visible buffer, device-local
   buffer + `vkCmdCopyBuffer`, 5000 submit + fence-wait round trips, 20000
   submits without waiting):

   ```sh
   glslc --target-env=vulkan1.3 tests/compute.comp -o compute.spv
   cc -O1 -g tests/vk_compute_test.c -lvulkan -o vk_compute_test
   ./vk_compute_test compute.spv [copy] [N]     # LOOPS=n, NOWAIT=1 for stress
   ```

4. **Graphics**: `vkcube` (zero-copy and `--sw`).
5. **Not run yet**: dEQP-VK (`compute.basic`, `api.smoke`,
   `synchronization2.basic`, `renderpass`), a deliberately bad pushbuffer
   (must give `VK_ERROR_DEVICE_LOST`, not a hang), sparse binding and many
   fences (semaphore pool growth).

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
`NV2080_CTRL_CMD_GR_GET_INFO_V2`; SM limits as nouveau computes them; ZCULL
from `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO`. Volta or newer is required
(doorbell submission). `kmd_info`: only `has_get_vram_used`.

**Device** (`nvkmd_rm_dev.c`). Its own RM client per `VkDevice`; the
device's own VA space (64 KiB big pages), named through `FERMI_VASPACE_A`
with index `GPU_DEVICE`, as NVIDIA's driver does. Not a new
`FERMI_VASPACE_A`: under GSP, `VA_INTERNAL_LIMIT` pins RM's internal range
to [4 GiB, 4.5 GiB), which a GSP client reserves entirely for GSP-RM, so GR
context buffers have nowhere to go (3D object: `NV_ERR_NO_MEMORY`); and
`RESTRICT_RESERVED_VALIMITS` on the device is refused by GSP. RM's internal
mappings land wherever its allocator puts them; NVK's heap skips
[4 GiB, 4.5 GiB). `*_USERMODE_A` (BAR1, write-only) is mapped through the
subdevice for the doorbell; an `NV01_EVENT_OS_EVENT` on the FIFO non-stall
interrupt via `crm_event_open` (created without event data) plus
`NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION` (repeat); timestamps via
`NV2080_CTRL_CMD_TIMER_GET_TIME`.

**Memory** (`nvkmd_rm_mem.c`). CPU-mappable and GART memory: our own
anonymous pages wrapped in `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, so the CPU map
is the allocation itself (zero-copy, coherent, no host window space). It is
registered with `crm_alloc_os_descriptor` (`NV_ESC_RM_ALLOC_MEMORY` on the
GPU channel): RM takes a user address only on that route, `NV_ESC_RM_ALLOC`
of the class answers `NV_ERR_NOT_SUPPORTED`; fallback `NV01_MEMORY_SYSTEM`.
Device-local memory: `NV01_MEMORY_LOCAL_USER`, 64 KiB pages (2 MiB with
`NVK_RM_HUGE_PAGES`), COMPR_ANY for dedicated image memory on GB20x
("Compression"). Host-visible VRAM is the optional BAR heap ("Host-visible
VRAM"). All memory is coherent.

**VA** (`nvkmd_rm_va.c`). NVK keeps picking addresses from its
`util_vma_heap` ([2 MiB, 256 GiB) minus RM's internal range; replay heap
[256 GiB, 512 GiB); all below 2^40 as GPFIFO entries require). Each
`nvkmd_va` is an `NV50_MEMORY_VIRTUAL` at exactly that address
(`FIXED_ADDRESS_ALLOCATE`, `SPARSE` for sparse ranges; RM collisions are
retried elsewhere). Ranges follow RM's own alignment for a default page
size virtual allocation, or RM moves them: 64 KiB, and offset and size
aligned to 2 MiB from 2 MiB up (RM then uses huge pages). Binds are
`crm_map_dma2` into it with `DMA_OFFSET_FIXED`, 64 KiB PTEs for VRAM,
snooped 4 KiB PTEs for system memory and `PAGE_KIND_OVERRIDE` with the VA's
PTE kind. RM unmaps whole mappings only, so each VA tracks its mappings and
partial unbinds unmap and re-map the remainders. With `NVK_RM_DEFER_FREE`
(default on Windows) frees wait for the GPU work submitted before them.

**Exec contexts** (`nvkmd_rm_ctx.c`), after `nvidia-push-init.c`:
`KEPLER_CHANNEL_GROUP_A` (GR, our VA space) → `FERMI_CONTEXT_SHARE_A`
(SYNC, VEID 0: GSP refuses the 3D object on an async subcontext) → GPFIFO
channel (4096 entries by default, `NVK_RM_GPFIFO_ENTRIES`; ring and push
slots in our system pages; error notifier + USERD in RM system memory) →
`NVA06F_CTRL_CMD_BIND` (GR) → 3D / compute / copy objects without
parameters → `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`, `GET_WORK_SUBMIT_TOKEN` →
`NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`. Submission has no RM call: GPFIFO entries
(`NO_PREFETCH` → `SYNC_WAIT`), GP_PUT in USERD, token to `usermode + 0x90`.
GP_GET in USERD is not written back while the channel runs (it stays 0), so
every kick ends with a semaphore release (no WFI) of a sequence number and
the ring counts as read up to the newest release that has landed. A full ring
waits for that (10 s, then `DEVICE_LOST`); a non-zero error notifier is
`VK_ERROR_DEVICE_LOST`. Video decode contexts (`NVKMD_ENGINE_VDEC`) are a
channel on the NVDEC0 runlist with the NVDEC object alone on it.

**Bind contexts**: synchronous; waits and signals on the CPU.

**Syncs** (`nvkmd_rm_sync.c`). Timeline `vk_sync` = 64-bit value in a
per-device pool of system memory; `vk_sync_binary` on top, with a `move`
added (the runtime needs it for binary semaphores in assisted mode). GPU
signal: `SEM_EXECUTE` release (64-bit, WFI) + `NON_STALL_INTERRUPT`; GPU
wait: `SEM_EXECUTE ACQ_STRICT_GEQ` with TSG switch. CPU wait: read the value,
spin briefly, then wait on the non-stall event through `crm_event_wait`
(Linux: `poll()`; bounded at `NVK_RM_WAIT_POLL_MS` per round, so a lost wakeup
costs at most that) or sleep with backoff without an event. On Windows
waiters share librmclient's wake generation (`NVK_RM_EVENT_GEN`), and CPU
signals kick them. Event payloads are never needed; on Linux (and on Windows
with `NVK_RM_EVENT_DRAIN=1`) the event is drained with
`NV_ESC_RM_GET_EVENT_DATA` to re-arm it, and a host that refuses that escape
leaves it readable for good, so after the first failed drain waits sleep
with backoff instead of polling. `WAIT_PENDING` uses a per-sync "highest
submitted value". `WAIT_BEFORE_SIGNAL` is not advertised, so Vulkan runs in
assisted timeline mode (a submit thread holds back waits on unsubmitted
values instead of leaving GPU acquires spinning). A destroyed sync's slot is
reused only after the GPU work submitted before it.

## What is stubbed or missing

| item | state |
|---|---|
| dma-buf / opaque-fd memory export and import | done (see "Zero-copy presentation"); cross-driver import (a dma-buf nvidia-drm cannot name) is refused |
| external semaphore/fence fds, explicit sync | not supported (no handle types); `NV_SEMAPHORE_SURFACE` + nvidia-drm's `SEMSURF_FENCE_*` would give sync_files and syncobjs on Linux (Windows uses them for present fences) |
| presentation | Linux: zero-copy (dma-buf + modifiers) with a CPU wait before each present; Windows: zero-copy scanout or composed present with RM fences |
| compression | dedicated allocations on; outside them opt-in, see "Compression" |
| video decode | H.264 on NVDEC (Windows build, `NVK_EXPERIMENTAL=video`); no encode |
| transfer queue (async CE channel) | off; needs a second TSG with `NV2080_ENGINE_TYPE_COPY(n)` |
| fixed CPU maps, overmap (`VK_EXT_map_memory_placed`) | off |
| sparse | code path present (`NVOS32_ALLOC_FLAGS_SPARSE`), unverified for an unprivileged client; NVK always advertises sparse binding |
| CPU waits | spin + wait on the shared non-stall event; per-sync events via `NV_SEMAPHORE_SURFACE` waiters would avoid waking every waiter on every interrupt |
| device-lost detection while idle | only when a call touches the context (and the Windows loss epoch); `NV2080_NOTIFIERS_RC_ERROR` event not used |

## librmclient

Used: the base contract plus `crm_map_dma2` (PTE kind), `crm_free_quiet`,
`crm_event_open/close/drain`, `crm_alloc_os_descriptor`, for dma-buf import
`crm_new_handle/crm_release_handle`, `crm_event_wait` and
`crm_alloc_pages/crm_free_pages` (so the backend never calls `mmap` or
`poll`), and on Windows the `crm_win_*` set (scanout, fences, semaphore
surfaces, resource ids, event generations, loss epoch). All additions are
looked up with `dlsym` (`GetProcAddress` on Windows) and optional: without
`crm_map_dma2` images get the physical (generic) kind, without events CPU
waits sleep-poll, and on Linux a librmclient without the `crm_event_wait` /
`crm_alloc_pages` calls gets the same `poll`/`mmap` code from
`nvkmd_rm_lib.c`.

Limitation: `crm_unmap_dma` has no size (NVOS47 `size`). RM supports partial
unmaps; the backend unmaps whole mappings and maps the remainders back,
which costs extra round trips on sparse unbinds.

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
0xc36f010a, 0xc36f0108, 0xa06c0101, 0xa06f0104; for dma-bufs 0x3d05,
0x3d06, 0x410110) and the `NV_ESC_RM_ALLOC_MEMORY` route for OS
descriptors; the nvidia-drm ioctls (`GET_DEV_INFO`, `GEM_IMPORT_NVKMS_MEMORY`,
`GEM_EXPORT_NVKMS_MEMORY`, PRIME) are the ones Conduit already serves for
NVIDIA's own userspace. Operationally:

- the VM needs `--caps graphics`;
- the backend's OS-descriptor translation has to accept the anonymous
  `MAP_PRIVATE | MAP_POPULATE` pages NVK passes (with `MADV_DONTFORK`), and
  RM-allocated system memory used for USERD and the error notifier has to be
  mappable within the window budget (8 KiB per queue);
- the non-stall event stays readable unless `GET_EVENT_DATA` drains it; a
  backend that refuses that escape makes NVK sleep in CPU waits.
