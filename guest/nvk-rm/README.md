# NVK on RM

A first cut of an NVK backend that drives the GPU through NVIDIA's Resource
Manager (RM, the open kernel modules' `/dev/nvidiactl` interface, forwarded
by Conduit in a guest) instead of nouveau. It is a patch series against
upstream Mesa that adds a second implementation of NVK's kernel abstraction,
`src/nouveau/vulkan/nvkmd/rm/`, next to `nvkmd/nouveau/`. It talks to RM
through [librmclient](../rmclient), which it loads at runtime.

Status: **runs on an RTX 5090 (GB202, GSP firmware, RM 610.57.04) in the
`lab` guest.** `vulkaninfo` enumerates the GPU through NVK, a compute test
(storage buffer, dispatch, device-local buffer + copy, 5000 back-to-back
submits) passes, and `vkcube` presents **zero-copy** on the guest desktop
through Mesa's native X11 (DRI3/Present via Xwayland) and Wayland
(linux-dmabuf) WSI: the swapchain images stay in VRAM, block-linear with
NVIDIA DRM format modifiers, and go to the compositor as dma-bufs through
Conduit's nvidia-drm node (see "Zero-copy presentation"). See "First run"
and "Zero-copy run" at the end for what was run and what is still open.
dEQP has not been run yet.

Experimental and opt-in (`NVK_RM=1`); nothing changes for a guest that does
not set it. Design background: [docs/research/nvk-rm.md](../../docs/research/nvk-rm.md).

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
| 8 | `nvk/rm: fixes from the first run on an RTX 5090 (GB202, GSP)` | OS descriptors via `crm_alloc_os_descriptor`, RM's VA alignment rules, the device VA space, SYNC subcontext + channel bind, `cls_m2mf`, sync features |
| 9 | `nvk/rm: track GPFIFO progress with a semaphore; binary sync move` | ring progress without USERD GP_GET, `move` for binary syncs |
| 10 | `nvk/rm: report VA ranges at the size NVK asked for` | RM's 2 MiB rounding stays internal (`rm_size_B`); fixes the bind assert for images of 2 MiB and more (vkcube at 1280x720+) |
| 11 | `nvk/rm: stop polling a non-stall event whose data cannot be read` | a refused `NV_ESC_RM_GET_EVENT_DATA` leaves the event readable for good; waits sleep instead of spinning through refused escapes |
| 12 | `vulkan/wsi, nvk: wait for rendering before presenting without implicit sync` | `wsi_device::wait_before_present`, set by NVK when the backend has dma-bufs but no sync_file export (RM) |
| 13 | `nvk/rm: dma-buf export and import through nvidia-drm, DRM format modifiers` | `has_dma_buf` + `has_alloc_tiled` for RM: export/import of RM memory as dma-bufs via `OS_UNIX_EXPORT/IMPORT_OBJECT` and nvidia-drm's GEM import/export; DRM node discovery; `VK_EXT_image_drm_format_modifier` |

Each patch builds on its own.

### Common patches (`patches-common/`): per-draw cost

Generic NVK patches (one also touches the RM backend) that apply on top of
**both** lineages, unchanged:

- Linux: base + `patches/0001-0017` + `patches-common/*` (`build.sh` does
  this).
- Windows: base + `patches/0001-0013` + `patches-windows/*` +
  `patches-windows-dxvk/*` + `patches-common/*` (`build-windows.sh` does
  this). Verified with `git am` (no 3-way needed) and by building that
  stack for Linux and running the tests below natively.

| # | patch | what |
|---|---|---|
| 1 | `nvk: direct draws without an MME macro on Turing+` | `vkCmdDraw*`/`DrawIndexed*`/`DrawMulti*` set first vertex, base instance, draw index and view index from the CPU (shadow scratch, `SET_GLOBAL_BASE_*`, root table), only what changed since the last direct draw, then draw with `SET_DRAW_CONTROL_A/B` + `DRAW_*_BEGIN_END_A/B`; indirect, mesh, XFB, multiview draws, meta and generated commands drop the tracking |
| 2 | `nvk: don't reselect cb0 after binding constant buffers on Turing+` | no `NVK_MME_SELECT_CB0` call after cbuf binds: with the hardware root table nothing loads cb0 through the selector |
| 3 | `nvk: skip root table loads of dwords the GPU already has` | CPU shadow of the root table (valid bit per dword, per command buffer); a descriptor bind loads only the dwords that changed |
| 4 | `nvk: skip binding a constant buffer range that is already bound` | per group/slot memory of the bound range; rebinding the same set/offset emits nothing |
| 5 | `nvk, nvk/rm: let the GPU cache descriptor pools and tables on RM` | new `NVKMD_MEM_GPU_READ_ONLY`, set on descriptor pools and the image/sampler tables; the RM backend maps it GPU-cacheable (it was uncached system memory, so every descriptor set bound was a cbuf fetched across PCIe). RM-specific in effect, generic in form; nouveau ignores the flag |
| 6 | `nvk: bind vertex and index buffers with plain methods on Turing+` | no `NVK_MME_BIND_VB/IB` for CPU-recorded binds, and the range already bound is skipped |
| 7 | `nvk: keep what changes per draw in one hardware root table bank` | changing a second 256-byte root table bank between draws costs ~5 ns; the dynamic-offset dword of the dynamic buffer descriptors moves into bank 0 with the draw parameters and `sets[0..3]`. **API-visible**: `NVK_MAX_DYNAMIC_BUFFERS` 64 -> 32, i.e. 16 dynamic UBOs + 16 dynamic SSBOs per layout (NVIDIA: 15 + 16) |
| 8 | `nvk: ZCULL for DXVK depth buffers and reverse Z` | ZCULL storage also for depth images with `TRANSFER_DST` (DXVK sets it on every D3D11 depth texture; `EXCLUSIVE` sharing only), reset to a conservative state by an empty render pass after a copy, blit or resolve writes them or another queue hands them over; `SET_ZCULL_DIR_FORMAT` per image (GREATER when the first application render pass clears below 0.5, LESS otherwise, never changed afterwards) instead of always LESS. See "GPU time against NVIDIA in D3D11-through-DXVK shapes" |
| 11 | `nvk, nvk/rm: compression for images outside dedicated allocations on GB20x` | `NVK_RM_COMPRESS_ALL=1` (default off): device-local memory that is neither host-visible nor shared nor imported is allocated COMPR_ANY (`NVKMD_MEM_COMPRESSIBLE`, `nvkmd_info::has_compressible_mem`), and a compressible image bound anywhere in it gets its own VA with the compressible GMK kind and `is_compressed`; every other mapping of such memory uses a compressible kind too. Applies after 9-10 of the build branches and without them. See "Compression outside dedicated allocations" |
| 12 | `nvk, nvk/rm: compress separate depth/stencil images on GB20x` | `NVK_RM_COMPRESS_ZS=1` (default off while it is measured, `nvkmd_info::has_zs_compression`): combined depth/stencil formats (D24S8, D32S8X24), which Blackwell splits into separate depth and stencil planes, pass `nvk_image_can_compress`; both planes are compressed in a dedicated allocation (the memory's own VA) and in compressible memory from patch 11 (each plane's own VA). Needs 11. See "Compression outside dedicated allocations" |
| 13 | `nvk, nvk/rm: a compressible device-local memory type for images on GB20x` | `NVK_RM_COMPRESS_TYPE=1` (default off): a second DEVICE_LOCAL type on the VRAM heap, before the plain one, whose memory is COMPR_ANY. Optimal-tiling images report it (not sparse, protected, host-transfer, external or video); buffers only when transfer-only (the clear buffers vkd3d-proton and DXVK put over image memory), never vertex, index, indirect, uniform, storage or device-address buffers. Needs 11 (and 12 for depth/stencil). See "A compressible memory type (patch 13)" |
| 14 | `nvk: NVK_CPU_STATE_TRACKING=0, upstream 3D state setting for A/B` | an A/B lever, no change by default: `NVK_CPU_STATE_TRACKING=0` turns off 1, 2, 3, 4 and 6 (draws and VB/IB binds through the upstream MME macros, cb0 reselected, every root table load and cbuf bind emitted), to pin a rendering bug on them or rule them out without a rebuild. Numbered after the build branches' 9-13 |
| 15 | `nvk, nvk/rm: compression diagnostics and a clear-on-allocate knob` | `NVK_RM_COMPRESS_CLEAR=1` writes every compressible memory once through its compressible VA at allocation (candidate fix for stale compression state); `NVK_RM_COMPRESS_UPGRADE=0` keeps uncompressed images in compressible memory on kind 0x6; `NVK_RM_COMPRESS_TYPE_SCOPE=attachments` offers patch 13's type only to images that are compressed themselves (against the spec, testing only). All inert unless set. Numbered 15: the build branches have their own 14. See "Corruption with patches 11 and 13" |
| 17 | `nvkmd: NVK_DEBUG=vm log through a buffered file, with times and threads` | `NVK_DEBUG=vm` writes VA alloc/free/bind/unbind and memory create/destroy lines (`NVKVM <local time> +<s since start> t<thread> <op> [0x<10 hex>,0x<10 hex>) <size> ...`) to `NVK_VM_LOG`, else `%TEMP%\nvk-vm-<pid>.log` on Windows, else stderr; 1 MiB buffer flushed every 50 ms, after every free/unbind, on a failed submit and at exit. Answers what last owned a faulting VA (awk one-liner in `nvkmd.c`). Numbered after `nvk/compress-all`'s 16 |
| 18 | `nvk: NVK_CPU_STATE_TRACKING, read race closed and mixed paths kept safe` | the knob is one atomic word; the upstream VB/IB macro binds clear the CPU's record of the range and a full root table load updates the shadow, so taking either path at any time is safe. No method changes on the default path |
| 19 | `nvk/rm: free memory and VAs only after the GPU work submitted before the free` | `NVK_RM_DEFER_FREE` (default on on Windows): every flush after an exec ends with a WFI release of a per-context retire counter; a memory or VA free that comes while submitted work is unfinished queues its RM unmap/free until that work has landed (reaped at exec, free and allocation; `NVK_RM_DEFER_FREE_MB`, default 2048, caps the pending bytes). Fixes CS2's Xid 31 FAULT_PDE (a 109 MiB buffer unmapped while shaders still wrote it). Log line at powers of two and a summary at device destruction |
| 20 | `nvk/rm: a destroyed sync's semaphore slot is reused only after the GPU work` | a destroyed sync's semaphore slot goes back to the pool through 0019's deferred-free queue, so a release still in flight can't land in the next sync given the slot and complete its waits early |
| 21 | `nvkmd: NVK_DEBUG=vm log names the calling thread` | the thread column of 0017's log is `t<id>(<name>)` with the GetThreadDescription name (dxvk-cs, dxvk-queue, ...), so a capture says which thread freed a memory object |
| 22 | `nvkmd: NVK_DEBUG=vm mem- lines carry the freeing call stack` | every `mem-` line of the vm log ends with ` stack:` and up to 24 `module+0xoffset` frames (RtlCaptureStackBackTrace), to symbolize against helios_umd's PDB/map |
| 23 | `nvk: NVK_PASS_PROFILE times work outside render passes, more signatures` | patch 10's profile also times runs of operations outside render passes (dispatch, indirect dispatch, buffer/image copies, fill, update, image clears, blit, resolve image, resolve at the end of a pass, query reset/copy) with the report on the engine that ran them (3D, compute after `WAIT_FOR_IDLE`, copy engine non-pipelined); meta operations and the driver's own render passes (image clears, resolves, ZCULL reseeds) count towards the operation that records them. Each window: summary (command buffers = render passes + operations + between them), GPU ms/s per operation kind, top 40 signatures with draw calls, draws (multi-draw and indirect `drawCount` counted), indirect calls/draws, queries and barriers per instance; pass signatures add load/store ops and ZCULL storage. `NVK_PASS_PROFILE=2` adds VS/clipper/PS invocations and the four ZCULL statistics per pass. 4096 signatures (hashed), 65536 slots, drops counted per window. Numbered after 405.6's 22 |
| 24 | `nvk: NVK_MDI_BATCH reads indirect multi-draw records in batches` | `NVK_MDI_BATCH=<n>` (default 0, off; suggested 64, max 204): a `vkCmdDraw[Indexed]Indirect` with `drawCount > 1` is split into calls of at most n records (and 1024 dwords, the MME data FIFO) to new `NVK_MME_DRAW[_INDEXED]_INDIRECT_BATCH` macros that fetch all their records with one `MME_DMA_READ_FIFOED` instead of one read-and-wait per draw; padding of larger strides dropped, `gl_DrawID` continues. Simulator-checked against the per-record loop (identical method streams, no overread). Aimed at CS2's 0.6-0.9 ms 2x MSAA depth pass (DXVK merges `DrawIndexedInstancedIndirect` runs into multi-draws) |
| 26 | `nvk/rm: descriptor and upload memory in host-visible VRAM` | `NVK_RM_DESC_TABLE_VRAM` / `NVK_RM_UPLOAD_VRAM` (default 1): texture/sampler header tables, descriptor pools (`NVKMD_MEM_GPU_READ_ONLY`) and command buffer upload memory (new `NVKMD_MEM_CPU_WRITE_ONLY`) go to the BAR heap instead of system memory the GPU reads over PCIe; heap full or map refused keeps them in system memory with a warning. App host-visible VRAM that falls back to system memory is now GPU-cacheable. `NVK_BLACKWELL_MME_MEMBAR=0` drops the sysmembar from Hopper+ indirect-read barriers (A/B). `NVK_DEBUG=vm` `mem+` lines end with vram/bar/sysmem[,cached] |

Per draw, steady state (`NVK_DEBUG=push_dump`, `BENCH_NDRAWS=8`):

| test | before | after |
|---|---|---|
| plain draws | 1 MME call (`DRAW`, 5 dwords) | 4 methods, 6 dwords, no MME |
| new dynamic UBO offset | 27 methods, 36 dwords: 5 root loads (12 dwords), cbuf bind, 2 MME calls (`SELECT_CB0`, `DRAW`) | 10 methods, 15 dwords: 1 root dword, cbuf bind, draw |
| descriptor set switch | 36 methods, 48 dwords: 6 root loads, 2 cbuf binds, 2 MME calls | 16 methods, 24 dwords: 2 root dwords (same bank), 2 cbuf binds, draw |
| VB + IB bound per draw | 3 MME calls (`BIND_VB`, `BIND_IB`, `DRAW_INDEXED`) | plain methods only when the range changes, 5-dword draw |

GPU ms for 20000 draws (`vk_perf_bench -r 21 -t alu,ubo,desc,rebind,vbib`,
2560x1440, RTX 5090, median of 3 runs per build, every commit built and
measured in one session; ratio to NVIDIA 610.57.04 in parentheses):

| after patch | plain draws | dyn. UBO offset | set switch | same set rebound, 2 pipelines | VB+IB per draw |
|---|---|---|---|---|---|
| `patches/0017` (before) | 0.509 (2.21x) | 0.667 (5.51x) | 0.712 (4.98x) | 2.258 (0.83x) | 0.808 (4.96x) |
| 1 direct draws | 0.226 (0.98x) | 0.224 (1.85x) | 0.317 (2.22x) | 1.079 (0.39x) | 0.318 (1.95x) |
| 2 no cb0 reselect | 0.226 | 0.189 (1.56x) | 0.322 | 1.070 | 0.318 |
| 3 root shadow | 0.226 | 0.109 (0.90x) | 0.299 (2.09x) | 0.926 | 0.318 |
| 4 cbuf dedupe | 0.226 | 0.110 | 0.285 (1.99x) | 0.914 (0.33x) | 0.318 |
| 5 cached descriptors | 0.225 | 0.107 | 0.218 (1.52x) | 0.914 | 0.318 |
| 6 plain VB/IB binds | 0.226 | 0.107 | 0.219 | 0.913 | 0.156 (0.96x) |
| 7 root bank layout | 0.226 (0.98x) | 0.108 (0.89x) | 0.178 (1.24x) | 0.915 (0.33x) | 0.156 (0.96x) |
| NVIDIA | 0.230 | 0.121 | 0.143 | 2.735 | 0.163 |

The GPU is shared with the desktop and VMs, and its clocks follow the
load: absolute numbers move by up to ~15% between sessions (NVIDIA's set
switch measured 0.123-0.143 ms), so compare within a table. The other
categories (ALU, texturing, fill, blend, ZCULL, clears, meshes, dynamic
UBO meshes, tessellation, terrain, copies) are unchanged, within 1-2%.

Correctness: `BENCH_HASH=1 vk_perf_bench -t ubo,desc,rebind,vbib,dynidx,descupd,params,verify`
prints image hashes; all are identical before and after the series, and
every per-draw test (`ubo`, `desc`, `rebind`, `vbib`, `dynidx`, `params`,
`descupd`) also hashes the same as on NVIDIA's driver. `params` mixes direct, indexed,
multi-draw and indirect draws with varying first vertex, vertex offset,
first instance and draw index, 32- and 16-bit index buffers and a clear,
and repeats a direct draw right after each indirect one (dropping one
invalidation in the driver changes its hash); `dynidx` indexes dynamic UBO
arrays in two sets at run time; `descupd` rewrites a descriptor set between
submits. dEQP-VK is not installed on the host and was not run.

### Compression outside dedicated allocations (patch 11)

NVK compresses an image only in a dedicated allocation. vkd3d-proton
places every D3D12 resource in a heap and native engines sub-allocate, so
in the VM every Basemark render target (Vulkan and D3D12) was mapped with
kind 0x6 (GMK), never 0x8 (GMK compressible).

What GB20x and RM need (open-gpu-kernel-modules 615.78.08):

- GB20x has only GMK (0x6), GMK compressible (0x8) and GMK compressible
  without PLC (0x9) for block-linear memory, depth included
  (`mem_mgr_gb202.c`, `mem_mgr_gb202_base.c`).
- Compression state is per physical page: GB20x uses GMMU format v3,
  whose PTEs have no comptag line (`virt_mem_allocator_gm107.c` writes
  comptag lines only for format <= 2; `memmgrGetKindComprForGpu_KERNEL`
  marks such memory `bPhysBasedComptags`). There is no comptag pool to
  run out of, and COMPR_ANY costs no extra VRAM.
- The kind still has to be compressible at allocation time: a mapping
  whose kind override is compressible over memory whose own kind is not
  is quietly downgraded to the uncompressed kind (`virtual_mem.c`,
  "downgrading pteKind ... over uncompressed physical backing"). That is
  why sub-allocated images stayed 0x6 even with a 0x8 kind.
- PLC: RM picks 0x8 or 0x9 for the allocation and applies its per-page
  PLC workaround itself when it writes PTEs; NVK maps images with 0x8 as
  the dedicated path does.

The patch: NVK asks for `NVKMD_MEM_COMPRESSIBLE` on plain device-local
allocations (not host-visible, not exported or imported). The RM backend
allocates those like dedicated image memory (block linear, 32 bpp,
COMPR_ANY) and keeps the flag only if RM granted compression. At bind
time a `can_compress` image in such memory gets its own VA with
`compressed_pte_kind` and `is_compressed` (the draw path already turns
on color/Z compression from it). Accessing compressible pages through an
uncompressed kind would read raw compressed data, so every GPU mapping of
such memory is compressible: the memory's own VA is mapped 0x8 (buffers,
linear images, ZCULL), and an image VA with kind 0x6
(sampled-only textures, sparse images, other images NVK does not
compress) is mapped 0x8 with its 3D compression state left off.

Risks:

- Clients other than the 3D and copy engines reading compressible GMK
  through the memory's own VA: indirect draw/dispatch arguments and
  generated command streams read by the front end and PBDMA from
  buffers in a compressed heap. NVKMS allocates scanout surfaces
  COMPR_ANY, so hub clients reading compressible GMK is the hardware's
  normal case, but this is the first thing to check if
  something misrenders or faults (`NVK_RM_COMPRESS_ALL=0` to compare).
  Counter-Strike 2 (D3D11 through DXVK) showed broken geometry with it
  on, so it is default off again while that is investigated; patch 13 is
  the safer design.
- Image/buffer aliasing in one heap: both go through compressible kinds,
  so reinterpreting the bytes is as undefined as the Vulkan spec says and
  no more.
- No CPU access: host-visible types (the BAR heap, system memory) are
  never made compressible, and host image copies are already excluded by
  `can_compress`.
- RM refusing COMPR_ANY: the memory falls back to uncompressed (logged,
  counted) and images in it stay 0x6.

Logs (`mesa_logi`): the number of compressible memories and MiB as it
reaches each power of two, image binds that got the compressible kind,
image plane binds in compressible memory left uncompressed and why (not a
render target, depth/stencil or storage image; a separate depth/stencil
plane; other), and a summary at device destruction (memories, MiB,
refusals, compressed image binds, 0x6 binds made 0x8). The 0x6 -> 0x8
count also includes every tile bind of a sparse image, which the
NVK-side breakdown does not see.

Measured in the VM (RTX 5090, Basemark at 1080p, windowed), off against
on: Vulkan 268 compressed image binds in 11 compressible memories
(2565 MiB), none refused, calibration 7-9 % faster, demo-scene median
20.4 -> 19.2 ms; D3D12 (vkd3d-proton) 67 compressed binds in 6 memories
(1971 MiB), frame 21.8 -> 20.7 ms. No faults or device loss; a compressed
vkcube renders correctly.

What stays uncompressed (D3D12 had 306 binds made 0x8 against 67
compressed):

- Sampled-only textures (no `ALLOW_RENDER_TARGET`, `ALLOW_DEPTH_STENCIL`
  or `ALLOW_UNORDERED_ACCESS`, so vkd3d-proton gives them neither
  attachment nor storage usage): `nvk_image_can_compress` leaves them
  out. They are already mapped 0x8, though, so copy-engine and shader
  writes to them go through the compressible kind; the 3D compression
  state only matters for attachments. Little to gain.
- Depth/stencil formats (D24S8, D32S8X24): on Blackwell NVK splits them
  into separate depth and stencil planes, and `can_compress` rejects
  every image with more than one plane, dedicated or not. These are the
  main render targets still left uncompressed. Patch 12
  (`NVK_RM_COMPRESS_ZS=1`, default off) allows `separate_zs` images: they
  are never disjoint, each plane gets its own VA in the sub-allocated path
  and the memory's compressible VA in a dedicated one, NIL computes a
  compressed kind for both planes, and the draw path sets
  `SET_Z_COMPRESSION` and `SET_STENCIL_COMPRESSION` from `is_compressed`.
  `NVK: N depth/stencil images (separate planes) bound compressed` counts
  them; the "separate depth/stencil planes" figure of the uncompressed
  breakdown should drop to 0. As compressible images they also prefer a
  dedicated allocation now (`prefersDedicatedAllocation`), which
  vkd3d-proton follows for committed resources.
- Reserved (tiled) resources: sparse, excluded by `can_compress`; every
  tile bind counts as one 0x6 -> 0x8.
- Render targets and UAV textures (`COLOR_ATTACHMENT`, `STORAGE`,
  single-plane depth such as D32/D16), with typeless/mutable formats
  included: already compressed. GB20x compression is generic, so a format
  reinterpretation reads the same bytes.


### A compressible memory type (patch 13)

Patch 11 makes every plain device-local allocation compressible, buffers
included. Patch 13 keeps buffers out the way NVIDIA's driver does, with
a memory type of its own (`NVK_RM_COMPRESS_TYPE=1`; independent of
`NVK_RM_COMPRESS_ALL`, which should stay off with it).

- Type order: compressible VRAM is type 0, plain VRAM type 1, both on the
  VRAM heap with the same flags (the spec allows any order for equal
  flags). vkd3d-proton and DXVK take the lowest type an allocation
  allows (`vkd3d_try_allocate_device_memory`, DXVK's type iteration).
- Images: for a color format the spec only lets `memoryTypeBits` depend
  on the tiling, the sparse, protected and split-instance flags,
  `HOST_TRANSFER` and the external handle types, not on the usage or
  format. So every optimal image reports the type, sampled-only textures
  too; those are mapped 0x8 and not compressed (as with patch 11).
  Linear, sparse, host-transfer, external and (a small deviation) video
  images do not.
- Buffers: a buffer with fewer usage bits may allow more types, so
  transfer-only buffers report the type and nothing else does. That
  matters because vkd3d-proton binds a buffer over almost every
  allocation it sub-allocates (to clear it), and the types it may use are
  intersected with that buffer's:
  - D3D12 heaps without `DENY_BUFFERS` (tier-2 mixed heaps,
    `ALLOW_ALL_BUFFERS_AND_TEXTURES`) get a full-usage global buffer:
    plain VRAM, **not covered**.
  - `ALLOW_ONLY_RT_DS_TEXTURES` and `ALLOW_ONLY_NON_RT_DS_TEXTURES` heaps
    get a `TRANSFER_DST` global buffer (image heap sub-allocation, which
    vkd3d-proton allows when the driver has no pageable device memory, as
    NVK does): compressible type, covered.
  - `ALLOW_ONLY_BUFFERS` heaps: plain VRAM.
  - Committed textures: dedicated when large (already compressed by
    patch 0028 when they are render targets, depth or UAVs), otherwise
    sub-allocated from chunks per heap category with a `TRANSFER_DST`
    buffer: compressible type, covered.
  - DXVK: images come from chunks of the type they allow (the
    compressible one), with at most a transfer-only global buffer;
    buffers never use it.
- A dedicated allocation of an image that is not compressed itself is
  never made compressible: nothing to gain, and Helios may export it with
  the uncompressed layout.

How much of Basemark D3D12 this covers depends on the heap flags it
creates (not visible from here): committed and `RT_DS`-only placed render
targets are covered, render targets placed in tier-2 mixed heaps are not.
The log tells: `memory type 0 is compressible VRAM`, then patch 11's
counters (compressible memories and MiB, compressed image binds) against
the patch 11 run (6 memories, 1971 MiB, 67 binds).


### Corruption with patches 11 and 13

Counter-Strike 2 (D3D11 through DXVK), driver 403.1/404.1:
`NVK_RM_COMPRESS_ALL=1` drew a huge stretched "beam" (vertex or draw
parameters read wrong); `NVK_RM_COMPRESS_TYPE=1` draws one agent model's
body black while its mask, trousers and the other models are fine (a
per-texture fault). Both default off.

Candidates, from the sources:

1. **Host (PBDMA) reads of compressible memory (patch 11 only).** NVK
   feeds indirect draw and dispatch arguments, draw counts, conditional
   rendering values and generated commands to the GPU as GPFIFO segments
   that point into the application's buffer
   (`nvk_cmd_buffer_push_indirect`): the PBDMA fetches them as pushbuffer
   data. Neither NVIDIA's driver nor upstream NVK ever puts such buffers
   in compressible memory (upstream compresses dedicated images only), and
   nothing says the host fetch path decompresses. Indirect arguments
   written by a culling compute shader through kind 0x8 and fetched raw
   would give exactly garbage draws. Patch 13 keeps every buffer except
   transfer-only ones out of compressible memory, and the beam is gone
   with it, which fits.
2. **Stale compression state on reused pages (11 and 13).** On GB20x RM
   scrubs freed VRAM with copy-engine writes in physical mode
   (`memmgrScrubRegistryOverrides_GA100` only sets
   `bUseVasForCeMemoryOps` for SR-IOV heavy), i.e. without a PTE kind,
   so the compression state of those pages is not reset. A new
   compressible allocation read before it is written through kind 0x8
   (or written partially) can then return garbage instead of zeros. NVK
   does not clear new memory unless asked.
3. **Sub-allocated compressed render targets or UAVs (13).** New with 13:
   a compressed image on its own VA in a shared allocation; dedicated
   compressed images (patch 0028, on by default) have been fine.
4. **Uncompressed images on kind 0x8 (11 and 13).** Sampled-only
   textures in compressible memory are mapped 0x8 with compression off in
   their state; texture headers carry no compression field (only
   `SECTOR_PROMOTION`), so the descriptor is not the problem, but the
   copy-engine uploads and texture reads through 0x8 are new.
5. **I2M (11).** `vkCmdUpdateBuffer` of up to 2012 bytes is written by
   the 3D class's inline-to-memory DMA, which DXVK uses for small buffer
   updates. Not ruled out, but patch 13 keeps those buffers out too.

Test matrix (Counter-Strike 2, same spot; patch 15 gives the knobs):

| run | settings | reading |
|---|---|---|
| A | `NVK_RM_COMPRESS_TYPE=1 NVK_RM_COMPRESS_CLEAR=1` | model fixed: stale compression state (2); keep `CLEAR` as the fix |
| B | `NVK_RM_COMPRESS_TYPE=1 NVK_DEBUG=no_compression` | fixed: compression of sub-allocated images (3); still black: the memory or kind itself (2 or 4) |
| C | `NVK_RM_COMPRESS_TYPE=1 NVK_RM_COMPRESS_TYPE_SCOPE=attachments` | fixed: something about uncompressed images in compressible memory (4) |
| D | `NVK_RM_COMPRESS_TYPE=1 NVK_RM_COMPRESS_UPGRADE=0` | fixed (with C fixed too): reads or writes of those images through kind 0x8 |
| E | `NVK_RM_COMPRESS_ALL=1 NVK_RM_COMPRESS_CLEAR=1` | beam gone: stale state (2) was also the beam; beam stays: host reads (1) or I2M (5), and patch 11 stays retired in favour of 13 |

### GPU time against NVIDIA in D3D11-through-DXVK shapes (patch 8)

Measured on the host (RTX 5090, no VM): NVK built from the Windows stack
(`patches/0001-0013` + `patches-windows/*` + `patches-common/*`) for Linux
and run with `NVK_RM=1 NVK_UBO_DESC_CBUF=0` (what Windows runs) against
NVIDIA 610.57.04 on the same GPU, `vk_perf_bench`, GPU timestamps, median
of 11, 2560x1440.

Every older `vk_perf_bench` category is within ±10% of NVIDIA. The
gaps appear only in the shapes D3D11 takes through DXVK, which the new
tests cover:

| test | NVK before | NVK with patch 8 | NVIDIA |
|---|---|---|---|
| `zpass`, DXVK depth usage: 32 front-to-back layers over 8 render passes that load depth | 0.162 ms | 0.109 ms | 0.102 ms |
| same over 2 passes | 0.111 ms | 0.078 ms | 0.079 ms |
| `zrev`: the 32 layers in one pass with reverse Z (clear 0, GEQUAL) | 0.141 ms | 0.070 ms | 0.071 ms |
| `cb`: 20000 draws, each a new descriptor-buffer offset, 3 VS + 1 PS cbuffer | 0.144 ms | unchanged | 0.088 ms |
| `cb`: pixel shader with 256 cbuffer reads, 4 fullscreen passes | 0.991 ms | unchanged | 1.565 ms |

1. **ZCULL storage.** DXVK gives every D3D11 depth texture
   `TRANSFER_DST`, and NVK allocated ZCULL storage only for images written
   by nothing but depth attachments. ZCULL then worked only in render
   passes that clear depth. DXVK ends a render pass at every barrier,
   resolve or render target change, so most passes load depth.
2. **ZCULL direction.** `SET_ZCULL_DIR_FORMAT` was always LESS, so ZCULL
   never culled with reverse Z. Stored ZCULL has to be loaded with the
   direction it was stored with, so patch 8 fixes the direction per image
   at its first application render pass. `vkCmdClearDepthStencilImage`
   does not count, because DXVK clears every new depth image to 0.0 that
   way.
3. **Per-draw cbuffer switch** (not fixed, about 10 µs per frame at
   Heaven's draw counts). The push stream per draw is already minimal
   (one root table dword and the draw), so the remaining cost is the
   shader's bindless cbuf loads. Bound cbufs (`NVK_UBO_DESC_CBUF=1`) cost
   10 ms here, because the MME reads each descriptor from memory. That is
   why patch 0051 turned them off on Windows.

Correctness: `vk_perf_bench -t zcoh` writes a depth image outside a render
pass and then checks that a later pass is not culled by stale ZCULL. It
covers a copy from a buffer, a copy from an image, a blit and
`vkCmdClearDepthStencilImage`, in both directions, plus three
direction-choice cases. With patch 8 all 11 cases pass. With the reset after
transfers left out, the copy cases draw nothing, which shows the test
catches stale ZCULL. The image hashes of `ubo,desc,rebind,vbib,dynidx,descupd,params,verify,cb`
are identical before and after. `vk_summary`, `vk_offscreen_test`,
`vk_compute_test`, `vk_bl_readback`, `vk_bar_test` and `vk_coherence_test`
pass. No Xid was logged.

Known bad on RM: upstream Mesa MR !44088 (ZCULL save and restore through
MME DMEM, which includes !44203 and !44414), cherry-picked onto this stack,
raises Xid 13 `DATA_RAM_ACCESS_OUT_OF_BOUNDS` (ESR 0x404490) in the first
render pass with ZCULL. Patch 8 uses no MME and no DMEM.

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

## Windows build (cross-compiled, first bring-up)

NVK with the RM backend builds for **Windows x86_64** with MinGW-w64 on a
Linux host: `vulkan_nouveau.dll`, its ICD manifest and `librmclient.dll`.
With librmclient's real Windows transport (RM escapes through the Helios
KMD) it **runs on the RTX 5090 in the `win11` guest**: enumeration, compute
and offscreen rendering pass (see "First run on Windows" below); presenting
is the next step. Nine more Mesa patches on top of the 13 above, in `patches-windows/` (Mesa branch `nvk-rm-windows`):

| # | patch | what |
|---|---|---|
| 14 | `nvk/rm: event waits and host pages through librmclient, LoadLibrary on Windows` | the backend no longer calls `mmap`/`poll`: `crm_event_wait`, `crm_alloc_pages`/`crm_free_pages` (librmclient transport ABI 2), Linux compat fallback for an older librmclient; `LoadLibrary` of `librmclient.dll` next to the ICD |
| 15 | `vulkan/runtime: keep vk_image::drm_format_mod on every OS` | the field exists on Windows too (always `DRM_FORMAT_MOD_INVALID` there) |
| 16 | `nvk: build without libelf on Windows (no CUDA modules)` | `nv_cubin_nolibelf.c` |
| 17 | `nvk: driver build id without an ELF build-id note` | Mesa version + module timestamp (`disk_cache_get_function_identifier`), as dozen |
| 18 | `nak: leave nouveau's winsys and DRM out of the bindings on Windows` | only NAK's Linux hardware tests use them |
| 19 | `nvk: build for Windows with the RM backend only` | `with_nouveau_drm` (false on Windows): no nouveau winsys / `nvkmd/nouveau`; chipset limits split into `nouveau_device_limits.[ch]`; the RM backend's DRM side moved to `nvkmd_rm_drm.c` (Linux only, stubs otherwise); `VK_EXT_physical_device_drm` and DRM syncobj copies Linux only; empty `<sys/ioccom.h>` for `drm.h`; `TRUE`/`FALSE` from `<windows.h>`; `vulkan_nouveau.dll` with `vulkan_api.def` exports |
| 20 | `nvk: Win32 WSI` | `VK_KHR_win32_surface` + swapchain through Mesa's win32 WSI, as a software device (CPU copy per present) |
| 21 | `nvk/rm, wsi: Win32 zero-copy present by Helios scanout` | swapchain images in VRAM, imported once on a host render node as GEM objects and shown with ScanoutFlip (see "Zero-copy present on Windows" below); GDI stays the fallback |
| 22 | `nvk/rm: host-visible VRAM (a BAR heap)` | a DEVICE_LOCAL \| HOST_VISIBLE \| HOST_COHERENT type on a heap of its own, backed by vidmem mapped once through BAR1 (see "Host-visible VRAM" below). Generic RM code, Linux too |
| 24 | `nvk/rm, wsi: block-linear Win32 scanout swapchains with NVIDIA's modifier` | the zero-copy swapchain images keep NVK's tiling (block-linear, `0x0300000000606015` on GB20x) and are presented with that DRM modifier, so NVK no longer renders through a tiled shadow plus a copy into a linear image every frame; `NVK_HELIOS_WSI_LINEAR=1` or a refused modifier: linear as before (see "Block-linear scanout" below). 23 is S3's, 25+ follow |
| 26 | `util/disk_cache: multi-file shader cache on Windows` | Mesa's disk cache had no Windows code (`-Dshader-cache` was refused): the multi-file cache through Win32 calls, in `%LOCALAPPDATA%\mesa_shader_cache` (see "Shader cache on Windows" below). 23-25 are other branches' |
| 27 | `nvk/rm: let the GPU cache coherent host-visible system memory in L2` | host-visible system memory mapped GPU-cacheable, L2 sysmem invalidate at the start of every submit (`NVK_RM_SYSMEM_CACHED=0` off). Generic RM code (Linux series: patch 15 on perf/nvk-rm-efficiency) |
| 28 | `nvk/rm: compressible VRAM for images on GB20x` | `has_compression`: dedicated image memory allocated COMPR_ANY and mapped with the compressible GMK kind (`NVK_RM_COMPRESSION=0` off). Generic RM code (Linux: patch 16) |
| 29 | `nvk/rm: ZCULL from NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` | `has_zcull_info` (`NVK_RM_ZCULL=0` off). Generic RM code (Linux: patch 17) |
| 35 | `nvk/rm: video decode on an NVDEC channel` | NVK's H.264 Vulkan Video decode on GB20x's NVDEC (NVCFB0): `cls_vdec` from the class list, `NVKMD_ENGINE_VDEC` contexts on the NVDEC0 runlist, SET_OBJECT with the device's class. Needs `-Dvideo-codecs=h264dec` and `NVK_EXPERIMENTAL=video`; bit-exact in `win11` and on the host (see `docs/video.md`). Generic RM code |
| 37 | `util/queue: destroy the finish barrier after every thread has left it` | fixes the intermittent 0xC0000005 in ntdll (`RtlpWaitOnCriticalSection+0xbd`) at `vkDestroyInstance`: `util_queue_finish` (from `disk_cache_destroy`) let a queue thread `DeleteCriticalSection` the barrier mutex while another thread was still waking up in it. Mesa's mutex + condvar barrier (Windows, macOS) returned "serial thread" in every thread. Hit whenever the shader cache queue had two threads, i.e. on runs that wrote new cache entries (111 of 150 `vk_offscreen_test` runs with an empty cache; 0 of 500 after). Generic Mesa code |
| 38 | `nvk/rm: a Helios shared-surface import goes on the device's memory list` | patch 31's `nvkmd_rm_mem_import_resource` made the memory outside the `nvkmd_dev_*` wrappers, so it never went on `dev->mems`, and `nvkmd_mem_unref` took it off: an access violation in `nvkmd_mem_unref` when the opener freed an imported shared surface (`d3d11_share` on NVK, every mode, KMD 22.22.318.1). `nvkmd_dev_track_imported_mem` puts it on the list like every other import |
| 39 | `wsi/win32: scanout swapchains get two images; a one-image one does not hang` | Zink's kopper creates its swapchain with the surface's `minImageCount`, which the Win32 surface reported as 1. After patch 36 a one-image scanout chain kept its only image QUEUED after the present, so the next acquire waited forever (wgl_test on NVK hung in its first `SwapBuffers`, also with `NVK_RM_FENCE=0` and `NVK_SCANOUT_RELEASE=0`; `NVK_HELIOS_WSI=0` passed). The surface now reports 2 when the driver flips, and a one-image chain gets its image back at once, as before patch 36 |
| 43 | `nvkmd: keep the device's memory list consistent; log misuse with a stack` | `nvkmd_dev_alloc_mapped_mem`'s map-failure path freed a listed memory without unlinking it; add/remove on `dev->mems` are idempotent and `nvkmd_rm_mem_free` unlinks a still-listed memory, each misuse logged (`mesa_loge`, module+offset stack, 16 per process). Turns the ledger-on exit crash (`nvkmd_mem_unref` `list_del` on a freed neighbour) into a log line that names the culprit |
| 46 | `nvk/rm: helios_icd_interface version 5, scanout_frame (the KMD's seq and generation of a scanout frame)` | `scanout_frame(device, memory, &sequence, &generation)`: the `out_seq` the KMD returned for the memory's latest `SCANOUT_PRESENT` and the live user source's `SCANOUT_SET` `out_generation` (already recorded in `nvkmd_rm_mem::scanout_seq` / `nvkmd_rm_dev::source`), `VK_NOT_READY` without a live KMD source or minted seq. The D3D11 UMD names the frame with them in the already-on-scanout present tag (`helios_onscanout.h`) |
| 53 | `nvk/rm: helios_icd_interface version 6, queue_rm_fence_v3 (the semaphore and copy source of a composed present)` | `queue_rm_fence_v3(device, queue, memory, image, &fence, &value, &copy)`: `queue_rm_fence` plus `struct helios_icd_rm_copy`, the producer's semaphore (root client, `hSemaphoreMem` of the timeline's semaphore surface, `entry * entry_size`, the fence's value) and the presented image's RM memory and `helios_image_layout` (one plane, dedicated, uncompressed, block-linear only with the PTE kind its modifier names). `VK_INCOMPLETE` = the fence without a description. The D3D11 UMD wraps it in the `'HEF3'` record of the copy-engine Present (`guest/windows/docs/rm-copy-engine-present.md` 12) |
| 54 | `wsi/win32: opt-in per-present frame-time log (HELIOS_VK_FRAMETIME=1)` | PresentMon sees no frames from a native Vulkan app on NVK: the Helios scanout present is a `D3DKMTEscape` (librmclient) and the GDI fallback a `StretchBlt`, so there is no DXGI `Present` and no DxgKrnl present/flip/blit event for the process. With `HELIOS_VK_FRAMETIME=1` each `vkQueuePresentKHR` stores two QPC reads per swapchain in a lock-free ring; a thread writes `%ProgramData%\Helios\vkframes-<pid>.csv` about once a second (`HELIOS_VK_FRAMETIME_DIR` overrides the folder; the rest at swapchain destruction and process exit) with PresentMon v1 column names `Application,ProcessID,SwapChainAddress,Runtime,TimeInSeconds,MsBetweenPresents,MsInPresentAPI,Result`, readable by `guest/windows/ci/vmtest/pmpace.ps1`. Off by default, Windows only |
| 55 | `wsi/win32: the frame-time log says what it did, and falls back to %TEMP%` | The log starts at `wsi_device_init` (vkEnumeratePhysicalDevices) instead of the first present and writes `vkframes-<pid>.log` next to the CSV: the variable as the environment block (`GetEnvironmentVariableA`) and the CRT see it, the files, each swapchain (size, present mode, images, Helios scanout / DXGI / GDI), the first present, the flush thread starting, totals at exit. An unwritable folder falls back to `%TEMP%`; failures also go to `OutputDebugString` and stderr. No files at all = NVK's WSI never loaded in that process (e.g. the Helios policy sent it to Venus); files without rows = it never presented through NVK |
| 57 | `nvk/rm, wsi/win32: say where a VK_ERROR_DEVICE_LOST comes from` | The loss-epoch check made sync waits/signals/reads and CPU wait steps return `VK_ERROR_DEVICE_LOST` silently: the first device to see the epoch move now logs it once per process (`NVK: RM device lost: librmclient loss epoch A -> B`). With `HELIOS_VK_FRAMETIME=1`, failing `vkAcquireNextImageKHR` (and its semaphore/fence signal) and `vkQueuePresentKHR` results go to `vkframes-<pid>.log` and `OutputDebugString` (first 32). For Basemark GPU Vulkan's exit -4 before its first present (395.1) |
| 58 | `wsi/win32, nvk: composed present through a D3D11 flip swap chain (NVK_HELIOS_WSI_COMPOSE=1)` | Opt-in replacement for the GDI path under a DWM on NVK (patch 50): per window a D3D11 device on the Helios adapter (`MESA_WSI_COMPOSE_ADAPTER=<n>` overrides the pick) and a DXGI flip swap chain; each swapchain image is a D3D11 shared texture whose resource id and layout the WSI reads with `D3DKMTOpenResource` (`HeliosWddmOpenIdentity`, `HeliosWddmAllocLayout`) and NVK imports by resource id (patch 31); a present copies the texture into the back buffer and calls `Present` (sync interval 1 for FIFO). Every failure is logged (`MESA-WSI: compose: ...`) and falls back to scanout or GDI. `wsi_common_win32_compose.cpp` |
| 59 | `wsi/win32, nvk: the composed present writes its own log (helios-wsi-<pid>.log)` | MESA_LOG_FILE output does not appear in the guest: with `NVK_HELIOS_WSI_COMPOSE=1` every composed-path milestone and failure also goes to `%ProgramData%\Helios\helios-wsi-<pid>.log` (`HELIOS_VK_FRAMETIME_DIR` overrides, `%TEMP%` fallback), independent of `HELIOS_VK_FRAMETIME`; failures marked `FAIL` and sent to `OutputDebugString`; a refused import logs the KMD record next to the image's size, row pitch and offset |
| 60 | `wsi/win32: the composed present waits for the copy at acquire, not at present` | The present ends an event query after the D3D11 copy into the back buffer and returns; acquire waits for the chosen image's copy (outside the swapchain lock, within the app's timeout; an infinite acquire gives up after 2 s with a log line). The immediate context runs under its own lock |
| 61 | `wsi/win32, util/log: report composed acquire and present failures; MESA_LOG_FILE on Windows` | Mesa honoured `MESA_LOG_FILE` only outside Windows (the file logger setup was under `!DETECT_OS_WINDOWS`), so NVK's errors never reached a file in the guest: Windows now opens it (append). With the composed path in use, every failing acquire/present (and the composed acquire's own failures: no idle image within the timeout with the idle/held counts, an unfinished copy, a broken swapchain) goes to `helios-wsi-<pid>.log`. For Basemark GPU Vulkan exiting before its first present under compose (398.1) |
| 45 | `nvk/rm: host-visible VRAM has no fixed budget; a full heap falls back to system memory` | Replaces patch 22's 256 MiB budget: the BAR heap is sized from BAR1 (one big page below VRAM so it never reads as a full ReBAR) and is no longer a hard limit. An allocation past the reported size goes to system memory, as one whose CPU map the host refused (patch 25) already did, and neither is charged to the heap. The host's window is the real limit. A full heap used to return `VK_ERROR_OUT_OF_DEVICE_MEMORY`, which DXVK latched in the command buffer it was recording and `vkEndCommandBuffer` then failed. `NVK_RM_BAR_MB` still overrides the reported size (0 = off) |
| 44 | `nvk/rm: a device whose KMD went away touches none of its mappings` | Windows device loss (a live driver update or device restart under a running NVK process): librmclient registers every KMD view of RM memory in the loss table it shares with the Venus ICD and the Helios UMD (`guest/windows/umd_common/bridge/helios_kmdmap.h`), so a vanished view reads as zero pages, and the loss epoch moves (`crm_win_loss_epoch`, optional). A device records the epoch at creation; once it moves, exec-context flush/exec/wait/signal/sync, every CPU wait step and sync signal/get_value return `VK_ERROR_DEVICE_LOST` before touching a mapping, and new syncs are CPU-only. Fixes the crash in `nvkmd_rm_exec_ctx_flush` writing GP_PUT into a USERD view the KMD had unmapped (323.1, `vulkan_nouveau.dll+0x5dbf61`). After a loss librmclient sends no escape: the process needs restarting to use RM again |

| 32 | `nvk/rm: Windows: RM device on by default under the Helios ICD policy; librmclient32.dll` | Windows only: `NVK_RM` defaults to on (`NVK_RM=0` off). The Helios ICD policy of the D3D UMD (`HELIOS_ICD`, `HKLM\SOFTWARE\Helios` `Icd` / `NvkDenyList` / `NvkAllowList`, the UMD's built-in deny-list) hides the device from processes sent to Venus, so the loader hands them Venus; exported as `nvk_helios_process_allowed()`. A 32-bit build loads `librmclient32.dll` first (one driver-store directory for both architectures) |
| 33 | `nvk: tiled shadows for linear swapchain images` | the Win32 WSI's swapchain images are `TILING_LINEAR`; rendering to one with a depth buffer (vkcube, Zink) needs NVK's tiled shadow, whose layout was only set up for `DRM_FORMAT_MOD_LINEAR` images (zero-sized shadow, `NV_ERR_INVALID_ARGUMENT`, `vkEndCommandBuffer` = `VK_ERROR_OUT_OF_DEVICE_MEMORY`) |
| 34 | `zink: load a Vulkan ICD directly on Windows (NVK on RM for the Helios adapter)` | Zink (Mesa's gallium WGL ICD, `GL=1` builds) loads NVK itself: `ZINK_VULKAN_ICD`, `NvkIcdPath`/`NvkIcdPath32`, NVK next to the WGL DLL, `%ProgramFiles%\Helios\nvk`; the loader for processes the Helios policy sends to Venus. See "OpenGL on NVK: Zink" below |

Linux behaviour is unchanged: the full series (20 patches) builds the Linux
NVK (nouveau + RM) as before, with the same `.so` exports; the patches apply
with `git am` on the base commit and give exactly branch `nvk-rm-windows`.

```sh
guest/nvk-rm/build-windows.sh            # ~/code/mesa-nvk-rm-windows, build dir build-win
guest/nvk-rm/build-windows.sh /path/to/mesa build-dir
MESA_CLC_DIR=/path/to/linux/build/bin guest/nvk-rm/build-windows.sh   # reuse host mesa_clc/vtn_bindgen2
```

The script applies `patches/` and `patches-windows/` on branch
`nvk-rm-windows` (skipped if already applied), builds the native
`mesa_clc` + `vtn_bindgen2` NVK's OpenCL kernels need (or takes them from
`MESA_CLC_DIR`, e.g. a Linux `build-rm`'s `src/compiler/clc` and
`src/compiler/spirv`), cross-builds `librmclient.dll` from `guest/rmclient`,
configures Mesa with `windows/mingw-x86_64.ini` and

```sh
meson setup build-win --cross-file guest/nvk-rm/windows/mingw-x86_64.ini \
    -Dvulkan-drivers=nouveau -Dnvk-rm=enabled -Dgallium-drivers= \
    -Dplatforms=windows -Dllvm=disabled -Dmesa-clc=system -Dprecomp-compiler=system \
    -Dvideo-codecs= -Dvulkan-layers= -Degl=disabled -Dgbm=disabled -Dglx=disabled \
    -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Dshader-cache=enabled \
    -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled -Dxmlconfig=disabled \
    -Dperfetto=false -Dbuild-tests=false -Dbuildtype=debugoptimized
```

and stages stripped DLLs, `nouveau_icd.json` (`library_path`
`.\vulkan_nouveau.dll`, relative to the manifest), `imports.txt` and
`exports.txt` in `BUILD_DIR/dist`. Compiles run under `systemd-run --user
--scope -p MemoryMax=2500M` with `-j2` by default (`MEMORY_MAX`, `JOBS`).

Host needs (Ubuntu 24.04): `gcc-mingw-w64-x86-64` (GCC 13, win32 threads),
`rustup target add x86_64-pc-windows-gnu`, meson >= 1.7, bindgen + libclang,
cbindgen; `wine64` only to run the tests. LLVM is off for the Windows build
(NAK is Rust and needs no LLVM; only the host `mesa_clc` does).

The cross file uses **`-mno-ms-bitfields`** (C, C++ and bindgen): MinGW
defaults to MSVC bitfield layout, bindgen only models the GCC one, and NAK
and NIR share structs with mixed-type bitfields between C and Rust (NAK
asserts `sizeof(struct nak_nir_tex_flags) == 4`, which fails with the MSVC
layout). Nothing that crosses the DLL boundary has mixed-type bitfields
(Vulkan API, RM parameter structs).

Result (debugoptimized):

| file | size (stripped / with DWARF) | exports | imports |
|---|---|---|---|
| `vulkan_nouveau.dll` | 18.3 MB / 145 MB | `vk_icdGetInstanceProcAddr`, `vk_icdGetPhysicalDeviceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion` | GDI32, KERNEL32, msvcrt, ntdll, USER32, USERENV, WS2_32, bcryptprimitives, api-ms-win-core-synch-l1-2-0 |
| `librmclient.dll` | 71 KB / 254 KB | every `crm_*` (MinGW auto-export) | KERNEL32, msvcrt |

No MinGW runtime DLL is needed (`-static-libgcc`, no libstdc++ or
winpthread). `windows/icd_smoke.c` loads the driver as the loader does
(`vk_icdNegotiate...`, instance extensions, `vkCreateInstance`,
`vkEnumeratePhysicalDevices`); under wine with `NVK_RM=1`:

```
vk_icdNegotiateLoaderICDInterfaceVersion: 0, interface version 7
vkCreateInstance: 0
MESA: warning: NVK_RM: crm_open failed: -40          # -ENOSYS: stub transport
vkEnumeratePhysicalDevices: 0, 0 physical devices
```

### First run on Windows (2026-10-06, `win11`, KMD 22.22.307.0, RTX 5090)

The series above, unchanged, with librmclient's real Windows transport
(`guest/rmclient`, `src/transport_windows.c`: RM escapes over
`HELIOS_ESCAPE_NVRM`, CPU mappings, OS-descriptor pinning and event waits
through the KMD). Files in one directory: `vulkan_nouveau.dll`,
`librmclient.dll`, `nouveau_icd.json`; the tests from
`windows/build-tests.sh`. Run with `NVK_RM=1`.

The Vulkan loader ignores `VK_DRIVER_FILES` / `VK_ICD_FILENAMES` (and
`VK_ADD_DRIVER_FILES`) in an elevated process, which an administrator's ssh
session is ("Loader is running with elevated permissions. Environment
variable VK_DRIVER_FILES will be ignored"): `vulkaninfo` there only shows
the registered Venus ICD. The tests therefore take
`VK_DIRECT_DRIVER=C:\...\vulkan_nouveau.dll` and hand the ICD to the loader
through `VK_LUNARG_direct_driver_loading` (exclusive mode,
`tests/vk_direct_driver.h`). From a normal (non-elevated) desktop session
`VK_DRIVER_FILES` works as on Linux.

| test | result |
|---|---|
| `icd_smoke.exe vulkan_nouveau.dll` (no loader) | 1 physical device: `NVIDIA GeForce RTX 5090 (NVK GB202) (0x10de:0x2b85)` |
| `vk_summary.exe` (loader 1.4.309) | NVK, API 1.4.363, driver `Mesa 26.3.0-devel`, conformance 1.4.3.0, heap 0 32146 MiB VRAM, heap 1 15356 MiB system (host-visible), queue family flags 0xf, 183 device extensions |
| `vk_compute_test.exe compute.spv` | `PASS: 4096/4096 values correct (host-visible)` |
| `vk_compute_test.exe compute.spv copy 1048576` | `PASS: 1048576/1048576 values correct (device-local + copy)` |
| `vk_offscreen_test.exe 5000` | `PASS: offscreen triangle 256x256, 5000 frame(s)`, 1.4 s in all (~0.2 ms per submit + fence wait); the readback is the expected RGB triangle |

No RM refusal in the host backend log for any of these (no
"not in the ABI profile", no failed `RM_ALLOC`/`RM_CONTROL`). The first run
after copying new binaries takes a few seconds longer (Defender scanning the
new files), later runs do not.

(and "librmclient could not be loaded" without the DLL next to the driver).
librmclient's unit tests (`test_unit.exe`, 300 checks) pass under wine.

### Zero-copy present on Windows (patch 21, Helios WSI)

On the Helios adapter the Win32 WSI does not copy frames to a window: the
swapchain images are linear images in dedicated VRAM which NVK makes
presentable once, the way patch 13 makes a dma-buf on Linux, except that
librmclient's Windows transport carries the DRM side to the host:
`NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` into a fresh control channel
(`crm_win_open_device(255)`), `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on host
render node `NVK_HELIOS_DRI` (default 0, `crm_win_open_device(512 + n)`),
one GEM handle per image. `vkQueuePresentKHR` waits for the image's
rendering on the CPU, then shows the image:

- KMD 22.22.308 and later (QUERY_CAPS ops 9..11, "Option B",
  `guest/windows/docs/foreign-scanout.md`): `crm_win_scanout_set` once per
  layout (render node, size, stride, `XRGB8888`/`XBGR8888`, linear),
  `crm_win_scanout_present` with the image's GEM handle per frame (set
  again when it answers `-ENOENT`, i.e. the source lapsed), and
  `crm_win_scanout_release` when a swapchain that presented is destroyed
  and with the device. The KMD sends the ScanoutFlip itself and keeps the
  desktop's own flips off scanout 0 meanwhile, so they no longer alternate
  with ours (the flicker when the mouse moves).
- Older KMD (`-ENOSYS`) or librmclient: `crm_win_scanout_flip` (raw
  ScanoutFlip through FORWARD, increasing seq).

The host exports the GEM object as a dma-buf for the viewer. The window is
never used, so a hidden window on an invisible desktop (an ssh session)
presents fine.

- `NVK_HELIOS_WSI=0`: off (GDI copy, which fails with
  `VK_ERROR_MEMORY_MAP_FAILED` on an invisible desktop).
- Unpaced by default (FIFO pacing is to come from the KMD's present path,
  not a timer); `MESA_WSI_SCANOUT_HZ=N` caps presents at N per second.
- No release event from the host yet: an image comes back two presents
  after it was shown, so use three or more images.
- If the images cannot be exported (no render node, old librmclient without
  `crm_win_*`) the swapchain falls back to GDI.

`tests/vk_scanout_present.c` (from `windows/build-tests.sh`): a spinning
triangle on a hidden window, `vk_scanout_present [seconds] [width height]
[images]`. In `win11` (RTX 5090, KMD 22.22.307.0), 1920x1080, three images:

| run | result |
|---|---|
| KMD 22.22.308, scanout source (SET/PRESENT/RELEASE), unpaced, 10 s | 1234 fps, 12338 flips, 0 failed; source 1920x1080 stride 7680 XR24 linear, lapse 2000 ms, released on destroy |
| KMD 22.22.307, raw ScanoutFlip, unpaced, 10 s | 2784 fps, 27841 flips, 0 failed; KMD `NvFlip` +27841 |
| KMD 22.22.307, `MESA_WSI_SCANOUT_HZ=60`, 30 s | 60.0 fps, 1801 flips, 0 failed; `NvFlip` +1801 |
| `NVK_HELIOS_DRI=99` | "cannot open host render node 99", falls back to GDI |

Next: a host release event instead of the fixed hold, vblank pacing.

### Block-linear scanout (patch 24)

NVK cannot render into a linear color image. With linear swapchain images
(patch 21) every render pass drew into a hidden tiled shadow
(`linear_tiled_shadows`, `nvk_cmd_draw.c`) and copied it into the linear
image. Patch 24 keeps NVK's own tiling instead, named by NVIDIA's DRM format
modifier, which the host's display path reads as such. NVIDIA's driver
imports NVK-on-RM block-linear memory pixel-exact (`spike/host-nvk-import`,
`host_import_spike.c`).

- The Win32 WSI asks the driver for its uncompressed NVIDIA block-linear 2D
  modifiers for the format, filters them by usage and extent (as
  `wsi_common_drm.c` does) and creates the images with
  `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` and that list. This works even
  though NVK on Windows does not advertise `VK_EXT_image_drm_format_modifier`.
  NVK picks the tallest block, `0x0300000000606015` (kind 0x06, GOB kind
  generation 2, sector layout 1, h = 5) for B8G8R8A8 and R8G8B8A8 on GB20x.
- `scanout_export` returns the modifier the image got.
  `vkGetImageDrmFormatModifierPropertiesEXT` is Linux-only in the runtime,
  so the WSI cannot ask for it itself. The dedicated allocation already
  carries the image's PTE kind and tile mode, and the export passes them to
  NVKMS on `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` as block-linear surface
  params (`log2GobsPerBlock.y`, `genericMemory`), as `nvkmd_rm_drm.c` does
  on Linux. The export refuses memory whose layout is not the modifier's.
- `SCANOUT_SET` carries the modifier. KMD 22.22.308 to 311
  (`kmd_logic/src/foreign_scanout.rs`, `Layout::validate`) checks only the
  size, the fourcc and `stride >= width * 4`, so block-linear passes. Its
  unit test has `modifier = 0x0300_0000_0000_0010; // block linear is
  allowed`. No KMD change is needed. The stride is the GOB-aligned row
  pitch, which is 7680 at 1920.
- Fallback to linear: `NVK_HELIOS_WSI_LINEAR=1`, or a `SCANOUT_SET` refused
  with `-EINVAL` for a non-linear modifier. In that case the flip returns
  `VK_ERROR_FORMAT_NOT_SUPPORTED` to the WSI, which goes linear for later
  swapchains and returns `VK_ERROR_OUT_OF_DATE_KHR`.

Correctness: `tests/vk_bl_readback.c` builds the WSI's image: B8G8R8A8,
the modifier list, dedicated device-local memory. It writes a `(y << 16) | x`
pattern with the copy engine and clears an odd rectangle across GOB and
block edges with `vkCmdClearAttachments`, which uses the 3D engine with the
image as a color target. It then reads the memory's raw bytes back through
a buffer bound to the same memory and checks every pixel at the address the
modifier gives it, using NIL's TuringColor2D GOB as in `host_import_spike.c`.
These raw bytes are what NVKMS exports. In `win11` with KMD 22.22.311, a
release build:

| run | result |
|---|---|
| 1920x1080, `...6015` (what the WSI picks) | PASS: 2073600 pixels, 524835 of them cleared by the 3D engine, 0 wrong through the image, 0 wrong raw |
| 1920x1080 `...6014`, 1280x720 `...6015` | PASS |
| LINEAR (control) | PASS |
| `...6015` read as `...6014` (negative control) | FAIL as expected, 1799059 wrong |
| `...6015` read as linear (negative control) | FAIL as expected, 1833118 wrong |

`NVK_DEBUG=vm` in the demo and in Heaven shows the swapchain memory as
`kind 0x6, tile 0x50` and the scanout source as `modifier
0x300000000606015` (Heaven: 1600x900, stride 6400, XB24, two images). There
were 0 failed flips.

Performance, the same release build (`buildtype=release`, on top of patch
22), KMD 22.22.311, linear through `NVK_HELIOS_WSI_LINEAR=1`. Host
`nvidia-smi dmon -s pu` was sampled during each run:

| run | linear (patch 21) | block-linear (patch 24) |
|---|---|---|
| `vk_scanout_present 15 1920 1080 3`, unpaced | 6573 fps, SM 11-13 % (one sample 21), ~102-129 W | **11748 fps**, SM 6-8 % (two samples 19, 24), ~108-116 W |
| Heaven 32-bit, 1600x900 Medium, zero-copy WSI unpaced, `heaven-nvk-fps.ps1` 30 s after 25 s | 355.7 fps, median 2.38 / p99 5.18 ms, SM 90-95 %, 162-226 W | 344.1 fps, median 2.43 / p99 5.19 ms, SM 86-97 %, 164-252 W |

The demo nearly doubles. It is a triangle per frame, so the shadow copy was
most of its GPU work, and SM time per frame drops by about 3x. Heaven does
not change: the two runs follow the same 5 s buckets within noise (e.g.
45 s: 514 vs 515 fps). It is bound by NVK's own rendering at 90-95 % SM. A
1600x900 copy is small against a ~2.8 ms frame. Heaven with GDI present and
the BAR heap was 297 fps (patch 22's measurement).

### Linux-only code and how the Windows build handles it

| where | Linux-only thing | on Windows |
|---|---|---|
| `nvkmd_rm_lib.c` | `dlopen`/`dlsym` | `LoadLibrary`/`GetProcAddress`, `librmclient.dll` next to `vulkan_nouveau.dll` first |
| `nvkmd_rm_mem.c` | anonymous `mmap` + `MADV_DONTFORK` for OS-descriptor pages | `crm_alloc_pages` (librmclient: `VirtualAlloc`) |
| `nvkmd_rm_dev.c` | `poll()` on the non-stall event fd, `sched_yield` | `crm_event_wait` (stub: `-ENOSYS` → waits sleep with backoff), `thrd_yield` |
| `nvkmd_rm_pdev.c`, `_mem.c`, `_dev.c` | nvidia-drm node discovery (libdrm, `stat`), `/dev/nvidiactl` export fds, PRIME/GEM ioctls, `lseek` on dma-bufs | moved to `nvkmd_rm_drm.c`, not built; stubs: no dma-buf, no import, `has_alloc_tiled = false` (no DRM modifiers) |
| `nvkmd.c`, `nvkmd/nouveau/*`, `winsys/*` | nouveau DRM backend, libdrm | not built (`with_nouveau_drm`); `nouveau_device_limits.c` (chipset tables) is built |
| `nvk_physical_device.c` | `major()`/`minor()` of DRM nodes, `VK_EXT_physical_device_drm` | compiled out / extension off |
| `nvk_device.c` | `vk_drm_syncobj_copy_payloads` | compiled out |
| `nvk_instance.c` | ELF build-id | module timestamp hash |
| `nv_cubin.c` | libelf | stub, CUDA modules rejected |
| `nak_bindings.h` | `xf86drm.h`, nouveau winsys | left out (hardware tests only) |
| `nil.h` → `drm_fourcc.h` → `drm.h` | `<sys/ioccom.h>` | empty header in `src/nouveau/compat/win32` |
| `nvk_descriptor_table.c`, `nvk_device_memory.c` | `<sys/mman.h>` (unused) | dropped / `<unistd.h>` |

External memory/semaphore fd extensions were already gated on
`has_dma_buf` (patch 1), which is false on Windows. The WSI is Mesa's
win32 one (patch 20) as a software device, since there are no dma-bufs:
instance extensions under wine are `VK_KHR_surface`, `VK_KHR_win32_surface`,
`VK_KHR_get_surface_capabilities2`, the surface/swapchain maintenance ones and
the usual capability queries.

### What the KMD and the Windows transport must provide next

librmclient's `src/transport_windows.c` documents it per callback; in short:

1. **Escape ABI** (`D3DKMTEscape` to the Conduit adapter): a header
   `{ magic, version, channel, nr, size }` + the NV escape payload in place,
   RM status in the payload. The KMD copies nested user pointers (NVOS54
   params, NVOS21 alloc params, NVOS41 event data, ...) the way the Linux
   guest module does, and forwards to the host's RM.
2. **Channels**: open/close of the control channel and per-GPU channels,
   returning small integer ids; the KMD resolves those ids where payloads
   carry an fd (`register_fd.ctl_fd`, `nvos33_with_fd.fd`,
   `alloc_os_event.fd`, `NV0005_ALLOC_PARAMETERS.data`).
3. **CPU mappings** (`map_memory`/`unmap_memory`): `NV_ESC_RM_MAP_MEMORY`
   plus a mapping into the calling process (BAR1 doorbell, RM system memory
   such as USERD and error notifiers), returning the user address and RM's
   cookie. There is no channel `mmap` on Windows.
4. **OS descriptors**: `NV_ESC_RM_ALLOC_MEMORY` with a user VA from
   `VirtualAlloc` (`crm_alloc_pages`); the KMD pins it (`MmProbeAndLockPages`)
   and hands RM the page list.
5. **Events** (`event_wait`): a Win32 event per event channel that the KMD
   signals when RM posts the OS event (the non-stall interrupt NVK waits
   on), plus `NV_ESC_RM_GET_EVENT_DATA`. Without it NVK still works, with
   sleeping CPU waits.
6. **Presentation, zero-copy**: the Windows WSI in this build copies
   through the CPU (no dma-bufs). Zero-copy on Windows needs a
   shared-resource path instead: RM memory exported as an NT handle / D3DKMT
   shared allocation the compositor (DWM) can scan out or compose, and the
   WSI taught to use it (the dma-buf path of patch 13 is the Linux
   counterpart). That is the next design item after the transport.

Not tried yet: building on Windows itself (MSVC/clang-cl would need the
same NAK bitfield question answered with the MSVC layout), presenting
through the win32 WSI, `vkcube` (it cannot be pointed at NVK from an
elevated session, see above) and dEQP.

### D3D11 games through DXVK: Unigine Heaven (2026-10-06, `win11`, KMD 22.22.308.0)

Heaven 4.0 runs as D3D11 -> DXVK -> NVK -> RM, without touching the Helios
WDDM driver or registering anything system-wide. Every file sits next to
`Heaven.exe` in a private copy of Heaven.

**Heaven is a 32-bit (WoW64) application.** That means a 32-bit NVK and
librmclient. The 64-bit `vulkan_nouveau.dll` fails to load in it
(`LoadLibrary` error 193). In that case the shim below falls back to the
system loader, and DXVK silently runs on the Venus ICD instead. Check
`logs\nvk-shim.log` and `Heaven_dxgi.log` ("Found device: ... (NVK GB202)").
The RM transport works from WoW64 as it is: the escape ABI is
pointer-free, and the KMD's user mappings land below 4 GiB. The 32-bit
`crm_smoke`, `crm_pin_smoke` and `crm_event_smoke` all pass.

Build (host):

    ARCH=i686 OUT_DIR=dist32 guest/nvk-rm/build-windows.sh ~/code/mesa-nvk-rm-win32 build-win32
    # DXVK: the Helios fork, built plain with MinGW, no source changes
    # (its Helios hooks find no helios_* exports outside helios_umd.dll and stay off):
    cd guest/windows/third_party/dxvk && meson setup build32 --cross-file build-win32.txt \
        --buildtype release --strip -Denable_d3d8=false -Denable_d3d9=false \
        -Denable_d3d10=false -Db_vscrt=none && ninja -C build32
    guest/nvk-rm/windows/stage-dxvk-app.sh i686 dist32 <dir with the fork's d3d11.dll+dxgi.dll> stage

Requirements for `ARCH=i686`: `gcc-mingw-w64-i686` (dwarf2) and the
`i686-pc-windows-gnu` rustup target for the toolchain meson uses. Install it
with `rustup target add --toolchain stable ...` if a `rust-toolchain` file
pins another one. `patches-windows-dxvk/` is applied after
`patches-windows/`:

| # | patch | why |
|---|---|---|
| 1 | VKAPI_CALL on `nvk_CmdCopyMemoryToImageIndirectKHR` | 32-bit Windows (`__stdcall`) build error |
| 2 | no `VK_KHR_present_id`/`present_wait(2)` on Windows | the Win32 WSI has no `wait_for_present`; DXVK uses present wait when offered and hit `assert(swapchain->wait_for_present)` |
| 3 | R/B swizzle in the GDI present for R8G8B8A8 swapchains | the DIB is BGRA; DXVK picked `R8G8B8A8_UNORM`, so the sky came out orange |
| 4 | `NVK_RM_WAIT_SPIN`, `NVK_RM_WAIT_POLL_MS` | knobs for measuring the CPU wait path |
| 6 | CPU waits without a host round trip per wake; CPU signals wake waiters | a wake no longer reads the event's data (`NV_ESC_RM_GET_EVENT_DATA`, a synchronous host escape of ~60-100 us, ~10 per frame in a D3D11 game, that never finds anything: the non-stall event is allocated `NV01_EVENT_WITHOUT_EVENT_DATA`; `NVK_RM_EVENT_DRAIN=1` reads as before); a host signal or a raised pending value kicks the process's wait handle through librmclient's `crm_win_event_kick` instead of waiting for the next GPU interrupt or the 10 ms poll (`NVK_RM_CPU_KICK=0` off); `NVK_RM_WAIT_SPIN_US=N` polls N us before blocking (default 0). Needs librmclient with `crm_win_event_kick` for the kick (older: as before) |

The upstream DXVK 3.1.1 release `d3d11.dll` is quarantined by Windows
Defender in the guest (a false positive). The fork build is not.

Guest: copy `W:\Heaven` to `C:\Users\Public\heaven-nvk`, then put the staged
files in its `bin\` and `run-heaven-nvk.bat` + `heaven-nvk-fps.ps1` in
its root. Double-clicking `run-heaven-nvk.bat` runs it on the desktop. Its
arguments are `[dir [w h [prof]]]`. An optional `env.cmd` next to it is
`call`ed first (e.g. `set HEAVEN_TESS=TESSELLATION_DISABLED`,
`set NVK_HELIOS_WSI=0`).

- `vulkan-1.dll` (`windows/vulkan_shim.c`) forwards to the system loader.
  In `vkCreateInstance` it chains `VK_LUNARG_direct_driver_loading`
  (exclusive) with the `vulkan_nouveau.dll` next to it. The loader ignores
  `VK_DRIVER_FILES` in elevated processes, and this sidesteps that.
  `NVK_SHIM_FRAMES=file` logs every `vkQueuePresentKHR`. It is the only
  frame clock here: an app-local `dxgi.dll` emits no DXGI ETW events, so
  PresentMon sees nothing.
- `heaven-nvk-fps.ps1 [-Seconds 30] [-Warmup 45] [-Width] [-Height] [-Prof]`
  starts it through a scheduled task in the user's session, prints fps,
  frame-time median/p99 and 5 s buckets, and stops it by PID. `-Prof` adds
  librmclient's per-escape table (`CRM_WIN_PROF_FILE`, which also counts
  event waits) for the timed window.

Results, 1600x900 Medium, tessellation normal, the same RTX 5090. The NVK
rows are a 30 s window after 45 s; Venus is `heaven-fps.ps1`, 30 s after 25 s:

| path | fps | median / p99 ms | host SM % (nvidia-smi dmon) | power |
|---|---|---|---|---|
| Venus, Helios UMD (embedded DXVK) | 138.8 | 6.75 / 13.3 | 25-40 | ~165 W |
| NVK, GDI copy present | 62.5-62.8 | 15.2-17.1 / 22-41 | 68-94 | ~160 W |
| NVK, GDI, 800x450 | 85.2 | 10.5 / 18.6 | 93-99 | ~155 W |
| NVK, GDI, pure spin waits (`NVK_RM_WAIT_SPIN=1e8`) | 61.8 | 17.7 / 22.6 | | |
| NVK, zero-copy Helios WSI (patch 21, SCANOUT_PRESENT, unpaced) | 59.7 | 15.6 / 57.6 | 72-99 | ~150-170 W |
| NVK, zero-copy, tessellation disabled | 96.6 | 8.7 / 18.7 | | |
| app-local DXVK fork on Venus (64-bit NVK DLL failed to load) | ~50 | | 16-20 | ~100 W |

Rendering is correct: geometry, textures, tessellation, and colors after
patch 3. The per-bucket fps follows the camera path identically from run
to run, 48-175 fps.

RM traffic per frame (GDI run): 10.4 `NV_ESC_RM_GET_EVENT_DATA` at ~100 us
each, from the event drain in `nvkmd_rm_wait_step()`, so ~1 ms. There are
10.4 event waits, almost all woken by the event, not the 10 ms timeout.
Zero-copy adds one `SCANOUT_PRESENT` (op 10) per frame, 0.1-1.9 ms.
Everything else is under 0.05 per frame: no per-frame mmap, Open/Close,
alloc/free or event registration.

What limits NVK here is NVK's GPU work, not the transport or the present:

- removing the GDI copy changes nothing;
- removing event waits (pure spinning) changes nothing;
- the GPU is busy 70-99% of the time at 60 fps, where the NVIDIA driver
  (Venus) needs 25-40% at 139 fps. That is ~5-6x more GPU time per frame,
  at low power, so stalls more likely than math;
- a quarter of the pixels only goes from 62 to 85 fps;
- turning tessellation off gives +60%.

NVK reports no host-visible VRAM (`bar_size_B = 0` in
`nvkmd_rm_pdev.c`). The memory types are type 0 DEVICE_LOCAL (heap 0,
31.4 GiB) and type 1 HOST_VISIBLE|HOST_COHERENT|HOST_CACHED (heap 1,
15 GiB sysmem). So DXVK keeps its dynamic and upload buffers in snooped
system memory, which the GPU reads over PCIe on every draw. This is the
first suspect (`feat/nvk-rm-bar-heap`). After that come NVK/NAK on
Blackwell itself and tessellation.

It was the cause: see the next section.

### Host-visible VRAM (patch 22, 2026-10-06, `win11`, KMD 22.22.310.0)

Patch 22 adds the BAR heap NVK on nouveau (without ReBAR) and NVIDIA's own
driver have. On the RTX 5090 in `win11`:

| | heaps | types |
|---|---|---|
| before (`NVK_RM_BAR_MB=0`) | 0: 32146 MiB VRAM; 1: 15356 MiB sysmem | 0: DEVICE_LOCAL (heap 0); 1: HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED (heap 1) |
| after (default) | 0: 32146 MiB VRAM; **1: 256 MiB VRAM (BAR)**; 2: 15356 MiB sysmem | 0: DEVICE_LOCAL (heap 0); **1: DEVICE_LOCAL \| HOST_VISIBLE \| HOST_COHERENT (heap 1)**; 2: HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED (heap 2) |

- Why it was 0: the first cut set `bar_size_B = 0` on purpose. CPU maps of
  VRAM go through BAR1 and, under Conduit, through the host-visible window
  every CPU mapping in the guest shares (then 1 GiB, now the host GPU's
  BAR1; `NvWinMb`), and each map
  cost a host round trip (5.7 ms before KMD 309, 0.4 ms now).
- What it uses now (patch 45): `bar_size_B` = BAR1 from
  `NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE`, kept one big page below VRAM
  (`NVK_RM_BAR_MB` overrides, 0 = off; patch 22 had a 256 MiB default and
  BAR1 / 2). NVK's other mappable memory is its own system pages (OS
  descriptors) and takes no window space. Each allocation from the type
  is mapped once (`crm_map_memory`, write-combined in the KMD) when it is
  allocated and stays mapped until freed; internal and client maps alias
  that mapping, so mapping per frame costs nothing. The heap size is not a
  hard limit: an allocation past it goes to system memory, like one whose
  map the host refused (below). Until patch 45 it failed with
  `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
- When the CPU map fails (patch 0025: the shared window is full, or the
  KMD's per-process share of it is used up), the allocation still succeeds.
  The VRAM is freed and the allocation gets system pages (OS descriptors,
  which take no window space). The app still sees the same memory type, the
  GPU just reads it more slowly, and it still counts against the heap. The
  first such fallback per device logs `NVK: host-visible VRAM: CPU map of N
  MiB failed ... using system memory` (`NVK_DEBUG=vm`: every one). Patch
  order: 0022, 0023 (S3 Helios ICD interface) if present, 0024 (block-linear
  WSI) if present, then 0025, then `patches-windows-dxvk/`. The patch applies
  with or without 0023.
- Only that type lands in the BAR: `nvkmd_info::host_visible_vram_is_pinned`
  makes NVK ask for `NVKMD_MEM_VRAM` there, while NVK's own
  `LOCAL | CAN_MAP` buffers (push, queries, events, upload) stay in system
  memory, where the CPU can also read them fast.

`tests/vk_bar_test.c` (`vk_bar_test bar.comp.spv 4194304 fill`, 64-bit):

    host-visible VRAM: type 1, heap 1 (256 MiB)
    vkAllocateMemory 16 MiB (incl. its BAR mapping): 1.54 ms
    map+unmap: 0.039 us each (1000)
    CPU write 16 MiB: 13.49 ms (1244 MB/s)
    CPU read 16 MiB: 1147.64 ms (15 MB/s; reads through a WC mapping are slow)
    PASS: 4194304/4194304 values correct (CPU write -> GPU read/write -> CPU read, host-visible VRAM)
    fill: 15 x 16 MiB more, then -2 (256 MiB in use, heap 256 MiB), 2.37 ms per block
    PASS: heap limit enforced, freed space reusable

`vk_summary`, `vk_compute_test copy` and `vk_offscreen_test` pass as before.

Heaven 4.0 (32-bit build, `ARCH=i686`), 1600x900 Medium, tessellation
normal, GDI present, `heaven-nvk-fps.ps1` (the same 30 s window after 25 s
warm-up for every run), host `nvidia-smi dmon -s pu` over the window. The
same `vulkan_nouveau.dll`, BAR heap off through `env.cmd`
(`set NVK_RM_BAR_MB=0`) or on (default):

| run | fps | median / p99 ms | SM % | power |
|---|---|---|---|---|
| BAR heap off, run 1 | 95.3 | 9.05 / 20.5 | 96-100 | 159-182 W |
| BAR heap off, run 2 | 92.9 | 9.72 / 20.4 | 98-99 | 162-181 W |
| **BAR heap on, run 1** | **298.1** | **3.05 / 6.0** | 77-97 | 183-232 W |
| **BAR heap on, run 2** | **296.2** | **3.11 / 6.5** | 89-97 | 184-240 W |

3.1x, on the same camera path (5 s buckets 143-432 fps on, 44-154 off).
The GPU was busy all the time either way, but at higher power: it was
stalled on PCIe reads of DXVK's dynamic buffers in system memory, not
computing. (The "off" rows are faster than the 62 fps in the table above
because KMD 22.22.309/310 and the backend got faster in between.)

### Shader cache on Windows (patch 26, 2026-10-06, `win11`, KMD 22.22.311.0)

What was off, and why:

- **NVK's disk shader cache.** Mesa has no Windows disk cache:
  `disk_cache_os.c` is a `TODO` there, and meson refuses
  `-Dshader-cache` on Windows. `build-windows.sh` also passed
  `-Dshader-cache=disabled`. So `pdev->vk.disk_cache` was never created
  and NVK compiled every shader again in every process. NVK itself was
  ready: `vk_pipeline_cache` falls back to the physical device's disk cache,
  and patch 17's build id (Mesa version + DLL timestamp) keys it.
- **DXVK's state cache** does not exist any more. DXVK 3.0.2 (the fork)
  has no `dxvk.state` / `DXVK_STATE_CACHE_PATH` and no `VkPipelineCache` of
  its own. Persistence is the driver's job, which is NVK's disk cache.
- **Graphics pipeline libraries were already on.** NVK exposes
  `VK_EXT_graphics_pipeline_library` with
  `graphicsPipelineLibraryIndependentInterpolationDecoration`, so
  `dxvk.enableGraphicsPipelineLibrary = Auto` turns them on.
  `Heaven_d3d11.log` says "Graphics pipeline libraries supported" and
  lists the extension as enabled.

Patch 26 implements Mesa's multi-file cache (the default type) for Windows
and enables it in `build-windows.sh`:

- Location: `MESA_SHADER_CACHE_DIR`, else `%LOCALAPPDATA%`, else `%TEMP%`,
  plus `\mesa_shader_cache`. That is per user and writable without setup.
  `MESA_SHADER_CACHE_DISABLE=1` turns it off.
- The index is a file mapping shared by all processes, like the
  `MAP_SHARED` mmap on Linux.
- A new entry is written to a `.tmp` file opened with no sharing. That
  plays the part of the `flock`. The file is renamed to its final name while
  still open (`FileRenameInfo`, never replacing), so no reader sees half an
  entry.
- Eviction deletes the least recently used tenth of a random subdirectory
  (the 1 GiB default size limit).
- Entries are stored uncompressed: the MinGW build has neither zlib nor
  zstd. Heaven needs 5.3 MB for 635 entries.

The single-file and database caches stay unimplemented on Windows. Other
Windows builds (MSVC, dozen) are unchanged: the option is auto-disabled
there, not refused.

Measured with `windows/heaven-cache-run.ps1` (next to `run-heaven-nvk.bat`). It
launches Heaven, records 45 s from launch with the shim frame log, then
kills Heaven by PID. The setup is the same as above (1600x900 Medium,
tessellation normal, GDI present, BAR heap) on a **release** build
(`-Dbuildtype=release -Db_ndebug=true`, NAK at opt-level 3). "Cold" deletes
the cache first; "warm" is the next launch. "Scene" is counted from the end
of Heaven's ~3.6 s loading screen, which every run has.

| run | launch -> first present | first 30 s from launch: fps / p99 | first 10 s of the scene: p99 / worst frame | time lost in frames > 40 ms, first 10 s | new cache entries |
|---|---|---|---|---|---|
| cold 1 | 0.73 s | 241 / 6.6 ms | 23.6 / 837 ms | 1037 ms | 635 |
| cold 2 | 0.62 s | 232 / 6.5 ms | 16.8 / 715 ms | 941 ms | 635 |
| warm 1 | 0.61 s | 248 / 5.2 ms | 16.0 / 576 ms | 817 ms | 0 |
| warm 2 | 0.61 s | 234 / 7.3 ms | 22.1 / 603 ms | 1588 ms* | 0 |
| cold, GPL off | 0.62 s | 237 / 6.5 ms | 17.3 / 1105 ms | 1335 ms | 403 |
| warm, GPL off | 0.62 s | 241 / 5.2 ms | 19.5 / 638 ms | 942 ms | 0 |

\* includes two 350 ms hitches at 6.8 s and 7.5 s. Hitches like these
show up at random in cold and warm runs alike (also at 14-21 s in other
runs), so they are not compiles.

- The cache works: a warm run writes nothing new, so every NVK compile is a
  hit.
- With GPL on, Heaven's start-up stutter is one long frame ~0.4 s into the
  scene, plus a few 40-120 ms frames in the first 1.5 s. The cache shortens
  the long frame from 715-837 ms to 576-603 ms. Without GPL it is 1105 ms
  cold and 638 ms warm. So NVK compiles cost ~150-250 ms of it with GPL
  and ~470 ms without. The remaining ~600 ms is not NVK compiling, since it
  happens on full cache hits. DXVK's own DXBC translation, which is not
  cached anywhere, or resource creation are the next suspects.
- Time to steady fps is the same cold and warm, about 1.5 s into the scene
  (~5.7 s after launch). From the first 5 s bucket on, both follow the same
  camera-path fps within 5% (cold 158 208 275 365 381, warm 171 215 282 374
  383). The last 10 s of each 45 s run reach 284-311 fps.
- An earlier set on KMD 310, with other agents' tests running, had 1-4.6 s
  to the first present cold and 0.6-1.1 s warm. On 311 with an idle GPU, it
  is 0.6-0.7 s either way. On a release build, the missing cache was not
  what made launches "slow for many seconds". The debugoptimized build
  (asserts on, NAK debug assertions) compiles more slowly; it was not
  measured here.

### Integration stack (branch `nvk-rm/integration`, 2026-10-06, `win11`)

The canonical Windows build: every finished NVK-on-RM patch in one series.
`build-windows.sh` applies, with `git am --3way` on `MESA_BASE`:

| order | patches | what |
|---|---|---|
| 1 | `patches/0001-0013` | NVK on RM (Linux backend, generic) |
| 2 | `patches-windows/0014-0022` | Windows build, Win32 WSI, zero-copy Helios scanout (21), BAR heap (22) |
| 3 | `patches-windows/0024` | block-linear scanout swapchains |
| 4 | `patches-windows/0025` | BAR heap falls back to system memory when the CPU map fails |
| 47 | `nvk/rm: a refused scanout export says why, once per device` | the WSI only printed `scanout images unavailable (-8)`: `nvkmd_rm_mem_export_scanout`'s reasons go through `vk_error*()`, silent in release builds. The first refusal per device is now a `mesa_logw` naming the cause (no Helios scanout, system-page backing, or a PTE kind / tile mode that does not match the modifier). For Superposition GL through Zink losing zero-copy on 22.22.320.1 |
| 5 | `patches-windows/0026` | shader cache on Windows |
| 6 | `patches-windows/0027-0029` | L2-cached sysmem, compression, ZCULL info (`NVK_RM_SYSMEM_CACHED=0`, `NVK_RM_COMPRESSION=0`, `NVK_RM_ZCULL=0`) |
| 7 | `patches-windows/0035` | H.264 decode on NVDEC (`NVK_EXPERIMENTAL=video`) |
| 8 | `patches-windows-dxvk/0001-0004` | what DXVK needs |
| 8a | `patches-windows-dxvk/0005` | window swapchains: composed present by default under a DWM on NVK (`NVK_HELIOS_WSI_COMPOSE=0` = GDI), IMMEDIATE/MAILBOX offered (`MESA_WSI_WIN32_FIFO_ONLY=1` = FIFO only), composed presents on a thread with no CPU wait in `vkQueuePresentKHR` (`MESA_WSI_COMPOSE_THREAD=0` = on the app thread) |
| 8b | `patches-windows-dxvk/0006` | CPU wait path: no event-data read per wake (`NVK_RM_EVENT_DRAIN=1` restores it), CPU signals kick waiters (`NVK_RM_CPU_KICK=0` off), `NVK_RM_WAIT_SPIN_US` |
| 9 | `patches-common/0001-0007` | per-draw cost (shared with the Linux series) |

0023 (S3's Helios ICD interface) is not in this stack: S3 stages its own
build on top. 0027-0029 are the efficiency patches from
`perf/nvk-rm-efficiency-win` (there 23-25), renumbered and rebased onto 0025.
Release build (`b_ndebug`), shader cache on, `-Dvideo-codecs=h264dec`.

Note: the "series already applied" check compares the last patch's subject,
so a Mesa checkout that has an older stack with the same last patch is not
re-patched. Use a fresh checkout, or reset its branch to `MESA_BASE` first.

Tests, x86_64, KMD 22.22.312.0: `vk_summary` (184 extensions), `vk_compute_test`
(host-visible and copy), `vk_offscreen_test 5000`, `vk_bar_test ... fill`,
`vk_coherence_test`, `vk_bl_readback`, `vk_scanout_present` (1920x1080,
9749 fps, 0 failed flips) and `vk_video_probe` all pass. One run of
`vk_offscreen_test` (and earlier one of `vk_coherence_test`) died with an
access violation in ntdll before printing anything. It did not reproduce in
9 reruns, and it is still open.

`tests/vk_coherence_test.c` checks patch 27 on Windows, where the GPU-cacheable
mapping flags go through the KMD. Per iteration, for every HOST_VISIBLE |
HOST_COHERENT type: the CPU writes a new pattern, compute reads and rewrites
it, the CPU checks it, the CPU rewrites the even elements, compute runs
again, and the CPU checks again. Each iteration uses a new pattern and a new
multiplier. Result: 1000 iterations, 0 bad, on the BAR type and on cached
sysmem. That holds alone and while another process runs compute over 32 MiB
of host-visible sysmem plus an offscreen draw loop.

Heaven 4.0 (32-bit), 1600x900 Medium, tessellation normal,
`heaven-nvk-fps.ps1 -Warmup 25 -Seconds 30`, host `nvidia-smi dmon -s pu`
over the window. The same DXVK and shim throughout; the driver is picked with
`NVK_SHIM_DRIVER` in `env.cmd`.

Zero-copy present (unpaced), KMD 312, full stack against 0022 alone,
interleaved:

| build | fps | median / p99 ms | SM % avg | W avg |
|---|---|---|---|---|
| full stack, first pass (fills the shader cache) | 363.3 | 2.31 / 5.02 | 91 | 187 |
| full stack | 356.3, 352.1 | 2.51 / 5.06-5.41 | 90-91 | 199-204 |
| 0022 alone | 316.1, 346.8 | 2.48-2.87 / 5.2 | 93 | 200-201 |

Zero-copy, linear (before 0024/0026/patches-common), KMD 311: 0022 alone
336.2 / 328.1 / 293.4. 0022+0025+0027-0029: 287.5 / 306.8 / 352.2. With
`NVK_RM_COMPRESSION=0`: 338.3 / 329.5. With `NVK_RM_SYSMEM_CACHED=0`: 326.4.
With `NVK_RM_ZCULL=0`: 324.4.

GDI present, KMD 311: every build lands at 200-227 fps (0022 alone, +0025,
+0027-0029, each of 27/28/29 off), at 70-77% SM. vkQueuePresentKHR averages
4.4 ms there, so the GDI copy is the limit.

Reading: Heaven at Medium on the BAR heap is GPU-bound (90-93% SM). The same
build varies by about ±10% from run to run. Within that, 0027-0029 change
nothing measurable either way. The full stack is about 8% above 0022 alone
on zero-copy (357 against 331 fps on average), which is what block-linear
scanout (0024) gives on its own (344-356). Per-draw cost does not limit this
benchmark.

### Presents on RM fences, no CPU wait (patch 30, dxvk-on-nvk S4)

Before patch 30 every present on the Helios scanout waited on the CPU for the
frame's GPU work and then flipped: the Win32 WSI through
`wait_before_present`, the Helios D3D11 UMD (S3) through its frame gate. Patch
30 hands the flip an RM fence instead and the presenting thread returns at
once. The KMD side is `guest/windows/docs/rm-fence-marker.md`.

How a present gets its fence:

1. **Present timeline.** Per queue that presents, a 64-bit semaphore that is
   one entry (32 bytes on GB20x, value at offset 0) of an RM
   `NV_SEMAPHORE_SURFACE` (class 0xda, under the subdevice) over 4 KiB of
   RM-allocated system memory (`NVKMD_RM_MEM_RM_SYSMEM`: RM maps the memory
   itself, and an OS descriptor is refused with `NV_ERR_NOT_SUPPORTED`). It is
   an ordinary `nvkmd_rm_sync` whose value lives in the surface, so the queue
   signals it with the usual `SEM_EXECUTE` release + `NON_STALL_INTERRUPT`. A
   context binds its channel to the surface before its first release into it
   (`NV_SEMAPHORE_SURFACE_CTRL_CMD_BIND_CHANNEL`, notifier
   `NV2080_NOTIFIERS_FIFO_EVENT_MTHD`, the host engine's non-stall interrupt
   that the method raises), so RM checks the surface's waiters on it.
2. **Fence context.** The entry is imported on the host render node with
   nvidia-drm `SEMSURF_FENCE_CTX_CREATE` (0x54, NVKMS block
   `{hClient, hSemaphoreSurface, size}`, index = entry), once per timeline.
3. **Per present.** `vk_queue_signal_sync()` (exported by the runtime in
   patch 30) of the timeline's next value after everything submitted to the
   queue so far, then `SEMSURF_FENCE_CREATE` (0x55, `timeout_ms` 5000) for that
   value: a backend handle, recorded by the KMD (22.22.311+) as a fence of
   librmclient's NVRM device, which fires one `EventReady` when the GPU writes
   the value (or with an error after 5 s).
4. **Flip.** `nvkmd_rm_mem_scanout_flip_fenced()`:
   - KMD with `QueryCaps.supported_ops` bit 32 (`HELIOS_NVRM_CAP_SCANOUT_FENCE`):
     `SCANOUT_PRESENT` with flag `RM_FENCE` and the handle at offset 52; on OK
     the KMD owns the handle and sends the flip from its worker when the fence
     fires (FIFO per source, ready prefix coalesced). `QUEUE_FULL` (8 waiting):
     wait for our own fence (all older ones are then ready) and retry, so a
     plain flip never overtakes queued ones. Any other refusal: the handle is
     still ours, and the flip thread below takes over for good.
   - KMD without the bit (22.22.311): a flip thread per device waits on the
     fence (`EVENT_REGISTER` on the handle, no polling), closes it and sends a
     plain `SCANOUT_PRESENT`, in order, at most 8 frames behind (the producer
     blocks beyond that).
   - No DRM fences on the host (config bit 11), an older librmclient, or
     `NVK_RM_FENCE=0`: the CPU wait as before.

Consumers: the Win32 WSI (`wsi_device::win32.scanout_flip_fenced`; a scanout
swapchain then skips the CPU wait, `wsi_swapchain::gpu_ordered_present`) and
`helios_icd_interface` version 3 (`queue_rm_fence`, `rm_fence_wait`,
`rm_fence_close`, `scanout_present_fenced`; caps `RM_FENCE`,
`SCANOUT_FENCE_KMD`, `PRESENT_FENCE_KMD`), which the Helios UMD uses on
`feat/s4-rm-fences` (DXVK's queue taken with `lockSubmission()`; the frame
only made SUBMITTED, not complete). librmclient adds `crm_win_caps`,
`crm_win_semsurf_ctx_create`, `crm_win_semsurf_fence_create`,
`crm_win_fence_wait` and `crm_win_scanout_present_fenced` (all loaded as
optional symbols).

Knobs: `NVK_RM_FENCE=0` (off), `NVK_RM_FENCE_KMD=0` (never hand fences to the
KMD: flip thread).

Image reuse: the swapchain keeps its rule (an image comes back two presents
later). With the KMD carrying the fence the rule the KMD documents is "do not
render into the image of present P before present P+1's fence fired and its
call returned"; on one queue the GPU write into P's image is ordered after
P+1's and P+2's work, so the remaining window is one KMD worker wake.

Limits: the timeline is signalled on the present queue, so the flip is
ordered after work submitted to *that* queue. Work of another queue is
covered when the present waits on it with a semaphore: on Windows NVK has no
`copy_sync_payloads`, so the WSI's pre-present submit really waits on the
present queue, before the timeline signal. The UMD signals on DXVK's graphics
queue, which already waits for DXVK's transfer queue. A fence costs one escape (60-80 us): a
trivial frame (vk_scanout_present's triangle) is faster with the CPU wait
(5645 vs 4899 fps), any frame with real GPU work is faster fenced. A fence for
a value already reached fires after up to ~1 ms (the backend's event pump
misses the edge of a sync_file that signalled before it was watched and finds
it on its 1 ms sweep, `conduit-backend.rs` `event_pump`); fences made while the
GPU still works fire within ~0.1 ms of the write.

`win11`, KMD 22.22.311 (no bit 32, so the flip thread), RTX 5090:

- `crm_semsurf_smoke` (librmclient only, CPU `SET_VALUE`): fence create 62 us
  median, `SET_VALUE` to event 932 us median (p99 976), Close 67 us; already
  reached 0.83 ms; a 200 ms timeout fires after 201.5 ms.
- `vk_rmfence_test` (NVK, GPU release): no fence ever fired before the
  submit's own `VkFence` (checked every round). 2 GiB fill: wake 2091 us after
  submit (RM fence) vs 2157 us (`vkWaitForFences`); 64 MiB fill 298 vs 361 us.
- `vk_rmfence_test` presenting 1920x1080 on scanout 0 with three images and
  frame pipelining (wait for frame P-2): 1 GiB fill per frame 808 -> 982 fps,
  presenting thread 1228 -> 73 us per frame in present; 64 MiB fill 3711 ->
  6481 fps. 0 failed flips, 0 fences that did not fire.
- KMD counters after 38561 fences: `NvFence` = `NvFenceCl` = `NvFenceSig` =
  38561, `NvFenceEarly` 638, `NvFenceErr` 0.

Patch order: 0022, 0023 (Helios ICD interface, required: patch 30 extends
`nvk_helios.c`), 0024 if present, 0025, 0027-0029, **0030**, then
`patches-windows-dxvk/`. It does not apply without 0025 and 0027-0029.

### OpenGL on NVK: Zink (patches 32-34, 2026-10-06, `win11`, KMD 22.22.311.0 and 312.0)

`GL=1 guest/nvk-rm/build-windows.sh` also builds Zink as Mesa's gallium WGL
ICD (`libgallium_wgl.dll`) and Mesa's `opengl32.dll`, from the same tree as
NVK (`-Dgallium-drivers=zink -Dopengl=true`; it builds with MinGW as is).
Zink loads NVK directly (patch 34), so the Vulkan loader and the Venus ICD
are not involved. Installed through the driver package
(`feat/helios-nvk-package`), the adapter's `OpenGLDriverName` points at the
WGL DLL in the driver store, next to NVK.

`windows/wgl_test.c` (built by `windows/build-tests.sh`) checks a frame
with glReadPixels, runs a GL 4.3 compute shader over 64 Ki values and spins
glxgears' gears with swap interval 0. App-local `opengl32.dll` +
`libgallium_wgl.dll` + `vulkan_nouveau.dll` + `librmclient.dll`, run in the
desktop session (scheduled task, `/it`):

| KMD | build | renderer | readback | compute | gears 1280x720 |
|---|---|---|---|---|---|
| 311.0 | 64-bit, NVK | `zink Vulkan 1.4(NVIDIA GeForce RTX 5090 (NVK GB202) (MESA_NVK))`, GL 4.6 compat | pass | pass | 406-447 fps |
| 311.0 | 32-bit (WoW64), NVK | same | pass | pass | 440 fps |
| 311.0 | 64-bit, `ZINK_VULKAN_ICD=loader` (Venus) | `zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY))` | pass | pass | 353 fps |
| 312.0 | 64-bit, NVK, driver-store layout (`stage-helios-package.sh`) | NVK, as above | pass | pass | 4148-4763 fps |
| 312.0 | 32-bit, NVK, `vulkan_nouveau32.dll` + `librmclient32.dll` | NVK | pass | pass | 4555 fps |
| 312.0 | 64-bit, NVK, `NVK_HELIOS_WSI=0` (GDI present) | NVK | pass | pass | 1561 fps |
| 312.0 | 64-bit, `ZINK_VULKAN_ICD=loader` (Venus) | Venus | pass | pass | 831 fps |

On KMD 311.0 the gears were present-bound: NVK's Helios scanout flip was
refused with EBUSY ("frames dropped"), and the GDI path gave the same rate.
On 312.0 the flip works: zero-copy present to the scanout (the window's
content goes to scanout 0, as for every NVK swapchain without
ForeignImport), 5x Venus. `HELIOS_ICD=venus` sends Zink to the loader
(Venus). Without patch 33 the first frame failed: Zink renders its default
framebuffer, a linear swapchain image, with a depth buffer.

The same build registered as a Vulkan ICD (a manifest next to
`vulkan_nouveau.dll` under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, for
the test only) is what an ordinary app sees through the system loader
(`windows/vk_loader_list.c`, desktop session): NVK first, with the Helios
adapter's LUID (librmclient with `crm_win_adapter_luid`), Venus second;
the first discrete GPU, NVK, creates a device and completes a submit. With
`HELIOS_ICD=venus` NVK enumerates nothing and the app gets Venus.

### Scanout images back on host release (patch 36, KMD 22.22.315)

With KMD 22.22.315 on a backend with `NVGPU_F_SCANOUT_RELEASE` (feature bit
15) the KMD reports when the host is done with a flipped image
(`guest/windows/docs/foreign-scanout.md` on `worktree-kmd-start-debug`,
"Buffer release"; `QueryCaps.supported_ops` bit 35). Patch 36 replaces the
swapchain's "an image comes back two presents later" with it:

- every flip through the KMD's source (plain or fenced) records the
  `out_seq` it returned on the memory; a flip still queued in the flip
  thread has no seq yet, so a wait on that memory first waits for the thread;
- `vkAcquireNextImageKHR` gets an image as soon as a later one is shown,
  takes the idle image shown longest ago and blocks (outside the swapchain
  lock) until `SCANOUT_STATUS`'s released floor reaches that image's seq,
  woken by the `SCANOUT_RELEASED` event (kind 3, handle 0) with the
  reset/ask/wait order that loses no wake (librmclient
  `crm_win_scanout_wait_released`). The wait honours the app's timeout
  (`VK_TIMEOUT`/`VK_NOT_READY`, the image stays idle) and is capped at 1 s:
  past the host's 500 ms forced release the image is written anyway and
  counted. A lost transport counts as released;
- without bit 35 (any KMD before 315, or a host without bit 15) the old rule,
  unchanged. `NVK_SCANOUT_RELEASE=0` forces the old rule.

`win11`, KMD 22.22.312 (no bit 35): "not tracked", `vk_scanout_present` with
two and three images as before, 0 release waits. The tracked path is untested
(needs KMD 315 and the backend with bit 15).

Order: after 0030, before `patches-windows-dxvk/`.

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

In `lab`, `~/nvk-cube.sh` (a copy is `guest/nvk-rm/nvk-cube.sh`) does all of that (from ssh it picks the desktop
session's `DISPLAY=:0`, Xwayland auth and `wayland-0`):

```sh
~/nvk-cube.sh                      # vkcube --wsi xcb, zero-copy
~/nvk-cube.sh --wsi wayland        # Wayland, zero-copy
~/nvk-cube.sh --sw [...]           # software WSI, the first-run path
~/nvk-cube.sh --present_mode 0 --c 5000 --width 1920 --height 1080
```

## Zero-copy presentation

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
  sync_files yet, so the WSI waits on the CPU for each image's rendering
  before presenting it (`wait_before_present`, patch 12). The compositor's
  reads are ordered by the WSI's buffer release / Present idle events.
  vk_sync_binary's emulated sync_file import/export is removed for RM: the
  WSI took it for real support and failed the first acquire.

No guest kernel module change and no host change was needed.

## Zero-copy run (2026-10-05, `lab`, RTX 5090, GNOME Shell 50.1 on Wayland, Xwayland 24.1.10)

- `vkcube --wsi xcb` and `--wsi wayland` render correctly (an `xwd` of the
  window read back through Xwayland's glamor, i.e. NVIDIA's EGL importing
  NVK's block-linear dma-buf); `NVK_DEBUG=vm` shows the three swapchain
  images exported with kind 0x6, tile 0x50.
- No CPU copy: over 200 frames at 1920x1080, vkcube writes 23 KB in total to
  its sockets on the zero-copy path and 1.66 GB (one 8 MB `PutImage` per
  frame) with `--sw`.
- Frame rate, 5000 frames, uncapped present modes (IMMEDIATE on X11,
  MAILBOX on Wayland; IMMEDIATE is not offered on Wayland):

  | window | X11 zero-copy | X11 `--sw` | Wayland zero-copy | Wayland `--sw` |
  |---|---|---|---|---|
  | 500x500 | 3560 fps | 2200 fps | 5510 fps | 2290 fps |
  | 1920x1080 | 2900 fps | 630 fps | 4880 fps | 590 fps |
  | 3840x1400 | 3060 fps | 230 fps | 4090 fps | 240 fps |

  With FIFO, X11 runs at the 240 Hz display rate (229-237 fps) either way.
  Wayland FIFO paces at ~235 fps for a 500x500 window but ~60 fps for larger
  ones; NVIDIA's own Vulkan driver gets 55 fps in the same windows, so that
  is gnome-shell's frame-callback pacing, not NVK.
- `tests/vk_dmabuf_test.c`: device A fills a device-local exportable buffer
  and exports it, device B (a second RM client) imports the dma-buf and
  reads back all 1 Mi values; two exports are the same dma-buf. Passes
  (`cc -O1 tests/vk_dmabuf_test.c -lvulkan -o vk_dmabuf_test`, run with the
  environment above). The compute test still passes.
- Host: with `conduit view lab` open, the viewer shows the cube at 240 fps
  (`commit 240.0 fps ... 0 rejected`), all dma-buf ATTACH/COMMIT. Those are
  gnome-shell's composited frames: neither NVK's nor NVIDIA's vkcube gets
  direct scanout from gnome-shell here, even fullscreen, so NVK's buffer
  itself is not what the guest KMS flips; the whole chain still has no CPU
  copy.
- No refusals in the backend log, apart from `GET_EVENT_DATA` (escape
  0x52; the running backend predates the change that serves it): patch 11
  turns the ~5800/s of those into one per device.

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

**Device** (`nvkmd_rm_dev.c`). Its own RM client per `VkDevice`; the
device's own VA space (64 KiB big pages), named through `FERMI_VASPACE_A`
with index `GPU_DEVICE`, as NVIDIA's driver does. Not a new
`FERMI_VASPACE_A`: under GSP, `VA_INTERNAL_LIMIT` pins RM's internal range
to [4 GiB, 4.5 GiB), which a GSP client reserves entirely for GSP-RM, so GR
context buffers have nowhere to go (3D object: `NV_ERR_NO_MEMORY`); and
`RESTRICT_RESERVED_VALIMITS` on the device is refused by GSP. RM's internal
mappings land wherever its allocator puts them; NVK's heap skips
[4 GiB, 4.5 GiB);
`*_USERMODE_A` (BAR1, write-only) mapped through the subdevice for the
doorbell; an `NV01_EVENT_OS_EVENT` on the FIFO non-stall interrupt via
`crm_event_open` (created without event data) plus
`NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION` (repeat); timestamps via
`NV2080_CTRL_CMD_TIMER_GET_TIME`.

**Memory** (`nvkmd_rm_mem.c`). CPU-mappable and GART memory: our own
anonymous pages wrapped in `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, so the CPU map
is the allocation itself (zero-copy, coherent, no host window space). It is
registered with `crm_alloc_os_descriptor` (`NV_ESC_RM_ALLOC_MEMORY` on the
GPU channel): RM takes a user address only on that route, `NV_ESC_RM_ALLOC`
of the class answers `NV_ERR_NOT_SUPPORTED`;
fallback `NV01_MEMORY_SYSTEM`. Device-local memory: `NV01_MEMORY_LOCAL_USER`,
64 KiB pages, no compression. Host-visible VRAM (patch 22, Windows
series): a 256 MiB BAR heap (`NVK_RM_BAR_MB`), vidmem mapped once through
BAR1 with `crm_map_memory` at allocation; see "Host-visible VRAM". All
memory is coherent.

**VA** (`nvkmd_rm_va.c`). NVK keeps picking addresses from its
`util_vma_heap` ([2 MiB, 256 GiB) minus RM's internal range; replay heap
[256 GiB, 512 GiB); all below 2^40 as GPFIFO entries require). Each
`nvkmd_va` is an `NV50_MEMORY_VIRTUAL` at exactly that address
(`FIXED_ADDRESS_ALLOCATE`, `SPARSE` for sparse ranges; RM collisions are
retried elsewhere). Ranges follow RM's own alignment for a default page
size virtual allocation, or RM moves them: 64 KiB, and offset and size
aligned to 2 MiB from 2 MiB up (RM then uses huge pages). Binds are `crm_map_dma2` into it with
`DMA_OFFSET_FIXED`, 64 KiB PTEs for VRAM, snooped 4 KiB PTEs for system
memory and `PAGE_KIND_OVERRIDE` with the VA's PTE kind. RM unmaps whole
mappings only, so each VA tracks its mappings and partial unbinds unmap and
re-map the remainders.

**Exec contexts** (`nvkmd_rm_ctx.c`), after `nvidia-push-init.c`:
`KEPLER_CHANNEL_GROUP_A` (GR, our VA space) → `FERMI_CONTEXT_SHARE_A`
(SYNC, VEID 0: GSP refuses the 3D object on an async subcontext) → GPFIFO
channel (1024 entries; ring and push slots in our system pages; error
notifier + USERD in RM system memory) → `NVA06F_CTRL_CMD_BIND` (GR) → 3D /
compute / copy objects without parameters → `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`,
`GET_WORK_SUBMIT_TOKEN` → `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`. Submission has
no RM call: GPFIFO entries (`NO_PREFETCH` → `SYNC_WAIT`), GP_PUT in USERD,
token to `usermode + 0x90`. GP_GET in USERD is not written back while the
channel runs (it stays 0), so every kick ends with a semaphore release
(no WFI) of a sequence number and the ring counts as read up to the newest
release that has landed. A full ring waits for that (10 s, then
`DEVICE_LOST`); a non-zero error notifier is `VK_ERROR_DEVICE_LOST`.

**Bind contexts**: synchronous; waits and signals on the CPU.

**Syncs** (`nvkmd_rm_sync.c`). Timeline `vk_sync` = 64-bit value in a
per-device pool of system memory; `vk_sync_binary` on top, with a `move`
added (the runtime needs it for binary semaphores in assisted mode). GPU signal:
`SEM_EXECUTE` release (64-bit, WFI) + `NON_STALL_INTERRUPT`; GPU wait:
`SEM_EXECUTE ACQ_STRICT_GEQ` with TSG switch. CPU wait: read the value, spin
briefly, then wait on the non-stall event fd with `crm_event_wait` (Linux: `poll()`; bounded at 10 ms per round, so
a lost wakeup costs at most that) or sleep with backoff without an event.
Event payloads are never needed, but the event has to be drained to re-arm:
a host that refuses `NV_ESC_RM_GET_EVENT_DATA` leaves it readable for good,
so after the first failed drain waits sleep with backoff instead of polling
(patch 11). `WAIT_PENDING` uses a
per-sync "highest submitted value". `WAIT_BEFORE_SIGNAL` is not advertised,
so Vulkan runs in assisted timeline mode (a submit thread holds back waits on
unsubmitted values instead of leaving GPU acquires spinning).

## What is stubbed or missing

| item | state | needs |
|---|---|---|
| dma-buf / opaque-fd memory export and import | done (see "Zero-copy presentation") | cross-driver import (a dma-buf nvidia-drm cannot name) is refused |
| external semaphore/fence fds, explicit sync | not supported (no handle types) | `NV_SEMAPHORE_SURFACE` + nvidia-drm's `SEMSURF_FENCE_*` (Conduit forwards them) for sync_files and syncobjs; then drop `wait_before_present` and use Wayland explicit sync / DRI3 syncobj |
| presentation | zero-copy (dma-buf + modifiers), CPU wait before each present | explicit sync, above |
| host-visible VRAM, BAR heap | BAR1, not a hard limit: past it, or when the host refuses a map, system memory (patches 22, 25, 45; `NVK_RM_BAR_MB` overrides) | a heap size that tracks the host window's free space |
| compression | off | comptags (`NVOS32_ATTR_COMPR_REQUIRED`) and compressed modifiers |
| transfer queue (async CE channel), video decode | off | a second TSG with `NV2080_ENGINE_TYPE_COPY(n)` |
| zcull info | not queried | `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` |
| fixed CPU maps, overmap (`VK_EXT_map_memory_placed`) | off | |
| sparse | code path present (`NVOS32_ALLOC_FLAGS_SPARSE`), unverified for an unprivileged client | test; NVK always advertises sparse binding |
| proper CPU waits | spin + `poll()` on the non-stall event; payload not read | fine as is; per-sync events via `NV_SEMAPHORE_SURFACE` waiters would avoid waking every waiter on every interrupt |
| device-lost detection while idle | only when a call touches the context | `NV2080_NOTIFIERS_RC_ERROR` event |

## librmclient

Used: the base contract plus `crm_map_dma2` (PTE kind), `crm_free_quiet`,
`crm_event_open/close/drain`, `crm_alloc_os_descriptor`, and for dma-buf
import `crm_new_handle/crm_release_handle`, and (patch 14) `crm_event_wait`
and `crm_alloc_pages/crm_free_pages`, so the backend itself never calls
`mmap` or `poll`. All additions are looked up with `dlsym` (`GetProcAddress`
on Windows) and optional: without `crm_map_dma2` images get the physical
(generic) kind, without events CPU waits sleep-poll, and on Linux a
librmclient without the patch 14 calls gets the same `poll`/`mmap` code from
`nvkmd_rm_lib.c`.

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
0xc36f010a, 0xc36f0108, 0xa06c0101, 0xa06f0104; for dma-bufs 0x3d05,
0x3d06, 0x410110) and the `NV_ESC_RM_ALLOC_MEMORY` route for OS
descriptors; the nvidia-drm ioctls (`GET_DEV_INFO`, `GEM_IMPORT_NVKMS_MEMORY`,
`GEM_EXPORT_NVKMS_MEMORY`, PRIME) are the ones Conduit already serves for
NVIDIA's own userspace. Confirmed in the first run: nothing NVK sends is
refused. Operationally:

- the VM needs `--caps graphics`;
- worth checking in the first run: that the backend's OS-descriptor
  translation accepts the anonymous `MAP_PRIVATE | MAP_POPULATE` pages NVK
  passes (with `MADV_DONTFORK`), and that RM-allocated system memory used
  for USERD and the error notifier is mappable within the window budget
  (8 KiB per queue);
- the non-stall event stays readable unless `GET_EVENT_DATA` drains it; a
  backend built before `5b0ed72` refuses that escape (NVK then sleeps in
  CPU waits, patch 11), a newer one serves it.

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

## First run (2026-10-05, `lab`, RTX 5090, RM 610.57.04 with GSP)

Setup: the `lab` guest (Ubuntu 26.04) at 3 GiB RAM because the host was
short of memory (win11 holds 20 GiB). Mesa was built on the host (Ubuntu
24.04, older glibc than the guest; `-Ddisplay-info=disabled` because the
guest's libdisplay-info soname differs), installed with `--destdir` and
copied to `~/nvk-prefix` in the guest; librmclient was built in the guest
with its Makefile. Environment:

```sh
export NVK_RM=1
export VK_ICD_FILENAMES=$HOME/nvk-prefix/share/vulkan/icd.d/nouveau_icd.x86_64.json
export NVK_RMCLIENT_LIB=$HOME/nvk-rm/rmclient/build-make/librmclient.so
```

The compute test is `tests/vk_compute_test.c`:

```sh
glslc --target-env=vulkan1.3 tests/compute.comp -o compute.spv
cc -O1 -g tests/vk_compute_test.c -lvulkan -o vk_compute_test
./vk_compute_test compute.spv [copy] [N]     # LOOPS=n, NOWAIT=1 for stress
```

Results:

- `vulkaninfo --summary`: `NVIDIA GeForce RTX 5090 (NVK GB202)`,
  `DRIVER_ID_MESA_NVK`, API 1.4.363, conformance version 1.4.3.0, PCI
  0x10de:0x2b85; `NVK_DEBUG=vm` shows 3D 0xce97, compute 0xcec0, copy
  0xcab5, GPFIFO 0xca6f, usermode 0xc761, 32146 MiB VRAM, 12 GPCs, 85 TPCs.
  Without `NVK_RM` NVK declines; with both ICDs one process sees NVIDIA's
  driver and NVK side by side.
- Compute test (one storage buffer, a 64-wide shader writing
  `i * 3 + 7`, read back): host-visible buffer (4096 and 1 Mi values),
  device-local buffer + `vkCmdCopyBuffer` (4096 and 16 Mi values),
  `NVK_DEBUG=push_sync`, 5000 submit + fence-wait round trips (0.22 s for the
  whole process) and 20000 submits without waiting: all pass.
- `vkcube --wsi xcb --c 20000` on Xvfb: renders the textured cube
  (screenshot checked), about 3500 frames per second, exits cleanly.
- No refusals in the backend log, no NVRM errors in the host kernel log
  once the fixes were in.

What failed on the way (all fixed in patches 8 and 9, plus
`crm_alloc_os_descriptor` in librmclient): OS descriptor via RM_ALLOC
(`NV_ERR_NOT_SUPPORTED`); `NV50_MEMORY_VIRTUAL` moved by RM's alignment;
the timeline sync type's binary-only feature (assert); GR context buffers
with no VA (`NV_ERR_NO_MEMORY`, `kgraphicsMapCtxBuffer`); 3D object on an
async subcontext (`NV_ERR_INVALID_OBJECT` from GSP); `cls_m2mf` 0 (Fermi
path, push assert); binary semaphores without `move` (assert); GPFIFO wrap
with GP_GET never written back (`DEVICE_LOST` after 1023 entries).

Still to do: dEQP-VK (not built: the host had about 2 GiB of memory to
spare), test plan steps 5 (bad pushbuffer → `DEVICE_LOST`) and 7 (sparse,
many fences), a Mesa build inside the guest. (The non-stall event does not
re-arm without `GET_EVENT_DATA`; found in the zero-copy run, patch 11.)
