# D3D11 and D3D12 on NVK (Windows guest)

Status: design, nothing here is implemented. Written 2026-10-06 against Conduit `ccc3b88`
(origin/main), the Helios submodules at their pinned commits (mesa-helios `89bd067`,
`third_party/dxvk`, `third_party/vkd3d-proton`), Mesa NVK-on-RM for Windows at
`feat/nvk-rm-windows` `908a050` (tree `~/code/mesa-nvk-rm-windows`), the librmclient Windows
transport at `feat/nvk-rm-windows-transport` `c787be6`, and the KMD branches
`kmd/zero-copy-present` `70cb31d` and `kmd/foreign-scanout` `6b31a83`.

Paths without a prefix are relative to `guest/windows/`. `dxvk:` and `vkd3d:` are the
submodules under `third_party/`, `mesa-helios:` is the Venus ICD fork, `nvk:` is
`src/nouveau/vulkan` in the NVK-on-RM tree. **Verified** means read in code; **proposed** means
design. Nothing has run on a Windows guest except what section 1 says was tested.

Background, read first:
- `docs/research/nvk-rm-windows.md` on `research/nvk-rm-windows`: the RM transport, the NVK
  Windows build, and plan steps P0 to P6. Its section 5 and step P6 ("a Helios system-wide ICD
  switch") are what this document expands.
- `guest/windows/docs/zero-copy-present.md` on `kmd/zero-copy-present`: how an RM-exported
  buffer becomes a KMD resource id (`HELIOS_ESCAPE_FOREIGN_RESOURCE`, `IMPORT_RM`). This design
  builds on it.
- `docs/VENUS.md`, `docs/SCANOUT.md`, `docs/SYNC.md`.

## 1. The question, and the short answer

Today a D3D11 or D3D12 game on Windows renders like this: the D3D runtime calls the Helios UMD
(`umd/`, `umd12/`), which runs DXVK or vkd3d-proton on Venus (`vulkan_virtio.dll`), which
serialises Vulkan to the host, where virglrenderer replays it on NVIDIA's Vulkan driver. The
goal is to run DXVK and vkd3d-proton on NVK instead, talking to the host RM directly through
`HELIOS_ESCAPE_NVRM`, for near bare-metal performance. Later, DWM moves to NVK too and Venus goes
away.

**Short answer.** The Helios UMDs cannot switch ICDs as they are: every WDDM allocation, every
present, cross-process open and the present-ordering timeline is keyed on a Venus resource id
and a Venus timeline (section 2). The resource-id half has a clean fix. The zero-copy design
already makes an RM-exported buffer a first-class KMD resource id (`IMPORT_RM`), and every KMD
path downstream of creation accepts it unchanged. So the plan is:

1. **First light, no Helios change:** app-local upstream DXVK on NVK. Heaven runs with DXVK's
   `d3d11.dll`/`dxgi.dll` next to its executable and presents through NVK's GDI software WSI
   (two copies). This shows whether NVK is fast enough to be worth the rest (section 5, S1).
2. **Zero-copy present for app-local DXVK:** NVK's WSI hands an `IMPORT_RM` resource id to the
   Helios present vehicle, exactly the zero-copy design (S2).
3. **The Helios D3D11 UMD on NVK ("hybrid"):** the UMD loads NVK. A small backend-neutral
   interface in the ICD returns, for a `VkDeviceMemory`, a KMD resource id. On NVK that is a
   foreign resource minted through `IMPORT_RM`. DWM stays on Venus and composes those buffers
   through a host-side import of the same memory into NVIDIA's Vulkan driver. Present ordering
   starts as a CPU wait and moves to a KMD boundary that RM semaphores retire (S3, S4).
4. **D3D12 through `umd12` on NVK** needs that KMD boundary (S4) because `ExecuteCommandLists`
   ordering against WDDM fences depends on it (S5).
5. **DWM on NVK, Venus removed:** the KMD's own allocations (primary, GDI surfaces, staging)
   move to RM memory, scanout goes through `ScanoutFlip` from the KMD ("Option B"), and the Mesa
   Vulkan WSI, Zink and the Venus-only escapes are retired (S6).

## 2. How the Helios UMDs use Vulkan today (verified)

### 2.1 Getting a VkInstance and VkDevice

| | D3D11 (`umd`) | D3D12 (`umd12`) |
|---|---|---|
| entry | `umd/src/adapter.rs:create_device` calls `bridge::BridgeDevice::create(0, 0)` (line 376) → `umd/bridge/dxvk_bridge.cpp:helios_dxvk_create_device` (around 1729-1855) | `adapter12.rs:OpenAdapter12` → `caps12::native_optional_caps` → `bridge12.rs:probe_optional_caps` → `umd12/bridge/vkd3d_bridge.cpp` (699-850) → `vkd3d:libs/d3d12core/helios_entry.c:helios_vkd3d_create_device` (112-188) |
| Vulkan loading | upstream DXVK `DxvkInstance` with the default `LibraryLoader`: `LoadLibraryA("winevulkan.dll")`, then `vulkan-1.dll` (`dxvk:src/vulkan/vulkan_loader.cpp:12-46`). The Khronos loader, not a direct `vk_icdGetInstanceProcAddr`. | `helios_load_vulkan_once` (`helios_entry.c:78-110`): same two DLLs, `vkGetInstanceProcAddr` from the loader |
| ICD registration | `packaging/windows/Install-Helios.ps1` (213, 415, 429) registers `mesa\vulkan_virtio.dll` under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers` | same |
| physical device | `_putenv_s("DXVK_FILTER_DEVICE_NAME", "Virtio-GPU Venus")` once per process (`dxvk_bridge.cpp:1747`). The LUID path (`findAdapterByLuid`) exists but the UMD passes LUID `(0, 0)` | `vk_physical_device = VK_NULL_HANDLE`: vkd3d's `vkd3d_select_physical_device` (`vkd3d:libs/vkd3d/device.c:3509`) picks by `VKD3D_FILTER_DEVICE_NAME`, else discrete first. LUID `(0, 0)` (`device12.rs:288-301`) |
| extensions | DXVK's own list (section 4.6) | instance `VK_KHR_surface` + `VK_KHR_win32_surface`, device `VK_KHR_swapchain` required (`helios_entry.c:118-142`); minimum FL 11_0 (169) |

Next to the loader path, both UMDs **find the Venus ICD module themselves** and call private
exports on the loader-returned handles:

- `umd_common/bridge/bridge_icd_anchor.cpp:find_venus_icd_module` (130) walks loaded modules
  for the export `helios_venus_memory_alloc_info` (`kVenusIcdProbeExport`,
  `bridge_icd_anchor.h:73`). `umd/bridge/bridge_icd_exports.cpp:load_helios_icd_from_manifests`
  (231) falls back to the registry, `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` and
  `C:\ProgramData\HeliosVulkan\virtio_devenv_icd.x86_64.json`, and `LoadLibraryA`s the
  candidate.
- `reconcile_icd_anchor` (187) and the export `helios_icd_anchor_v1` enforce one ICD per
  process. D3D11 device creation fails if the anchor disagrees (`dxvk_bridge.cpp:1820`).

### 2.2 Private interfaces the UMDs depend on

**Venus ICD exports** (`mesa-helios:src/virtio/vulkan/vn_renderer_helios.c`, resolved in
`bridge_icd_exports.cpp:321-336` and `vkd3d_bridge.cpp:399-451`):

| export | used for | Venus-specific? |
|---|---|---|
| `helios_venus_current_ctx_id`, `helios_venus_instance_ctx_id` (HEL 732, 742) | the Venus context id stamped into every WDDM allocation's private data and every present stream | yes |
| `helios_venus_memory_id` (749) | `blob_id` of a `VkDeviceMemory` | yes |
| `helios_venus_memory_res_id` (759) | the KMD resource id behind a `VkDeviceMemory` | the concept is generic, the implementation is Venus |
| `helios_venus_memory_alloc_info` (775) | exact allocation size and memory type (virglrenderer's exact-size import rule); also the probe symbol | yes (exact-size rule) |
| `helios_venus_memory_transfer_resource_ownership` (904) | hand the resource id to the KMD allocation; the ICD stops releasing it | generic |
| `helios_venus_memory_vidmm_*` (798-824) | a VidMm tracking allocation for memory accounting | generic pattern |
| `helios_venus_producer_interface` (`vn_renderer_helios_producer.h:218`) | `helios_producer_api_v1` table (`stream`, `bind`, `publish`, `status`, `wait`, `retain`, `release`, `abort`) over escape `0x13` | yes: keyed on Venus ctx + host timeline |
| `helios_venus_register_present_stream`, `helios_venus_claim_present_buffer_read` (`vn_queue.c:1808, 1851`) | escapes `0x10`, `0x12` | yes |
| `helios_venus_queue_gpu_fence` (HEL 2071) | D3D12 GPU-completion boundary as a Venus wire fence | yes |

DXVK resolves `helios_venus_producer_interface` itself from the module that owns
`vkGetSemaphoreCounterValue` (`dxvk:src/dxvk/dxvk_helios_producer.cpp:16`); vkd3d does the same
(`vkd3d:libs/vkd3d/helios_producer.h:42-46, 337-341`). The scanout-acquire helpers find the
present-stream exports by walking modules (`dxvk:src/dxvk/dxvk_helios_scanout_acquire.cpp:98,
142-144`).

**A private Vulkan struct.** `VkImportMemoryResourceInfoMESA {sType 1000384002, resourceId}` is
declared locally in `dxvk:src/dxvk/dxvk_image.cpp:10-17` and `dxvk_memory.cpp:1448-1454` and
chained into `vkAllocateMemory` to import by resource id (`dxvk_image.cpp:654-678`,
`importVenusStagingBuffer`). Venus accepts it from applications (`vn_device_memory.c:694-731`).

**Export handle types that only mean something on Venus.** DXVK's fork exports shared and
scanout images as `OPAQUE_FD`/`DMA_BUF` on Windows (`dxvk_image.cpp:443-462`) because
"virglrenderer will only bind a VkDeviceMemory to a HOST3D blob if that memory was allocated as
exportable". vkd3d's fork does the same through the private heap flag
`VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT` (`vkd3d:libs/vkd3d/vkd3d_private.h:1120-1172`,
`resource.c:189-197, 762-773, 4512-4537`), which `umd12` ORs into every committed resource
(`resource12.rs:2287-2340`).

**UMD exports the Venus WSI calls** (the present "vehicle", `umd/src/vehicle_exports.rs`):
`helios_umd_set_present_source_v2/v3/v4`, `helios_umd_wait_last_present`,
`helios_umd_wait_present_copy_v2`, `helios_umd_clear_present_source_v2`. Mesa-helios'
`wsi_common_win32.cpp` (default "dcomp vehicle", lines 330-353, 900-905) creates a D3D11
swapchain on the Helios adapter and hands each Vulkan frame's resource id
(`PresentSource{resid, fence_value, semaphore_handle, …}`, `umd/src/forward/vehicle.rs:22`) to
DXVK inside the UMD, which imports and copies it into the back buffer.

**Escapes the UMDs send themselves:** only display ones, `MAP_READ_LEDGER` 0x0E,
`SCANOUT_EVENT` 0x0F and `SNAPSHOT_STATUS` 0x15 (`umd/src/scanout_acquire.rs:181-320`). All GPU
work goes out through the ICD's own escapes (`SUBMIT_VENUS` and friends,
`protocol/src/escape.rs:29-85`), not through WDDM DMA buffers.

### 2.3 Where WDDM allocations and present images come from

- **Which D3D11 resources get a WDDM allocation:** primaries, `BIND_PRESENT`, `SHARED`,
  `SHARED_KEYEDMUTEX` (`umd/src/forward/state.rs:813 needs_wddm_texture_allocation`). Everything
  else is an ordinary DXVK suballocation that WDDM never sees. D3D12 gives every committed
  resource one (`resource12.rs:adopt_committed_allocation`, 1659).
- **How (D3D11, `umd/src/forward/resource.rs:finish_wddm_tex2d`, 477-658):** DXVK creates the
  image with dedicated exportable Venus memory; `get_resource_memory_info`
  (`dxvk_bridge.cpp:495`) gets its blob id and resource id; `allocate_wddm_resource` (187-468)
  calls `pfnAllocateCb` with `HeliosWddmAllocPrivate{kind = DEVICE_MEMORY, ctx_id, blob_id,
  size, HOST3D, USE_SHAREABLE, adopt_resource_id}` plus `HeliosWddmAllocMeta`
  (`protocol/src/wddm.rs:121`); then ownership is transferred and
  `stamp_dxvk_resource_kmt_handles` builds a producer binding, which **throws without the
  producer interface** (`dxvk_bridge.cpp:460`). Without an importable backing the create is
  refused (`resource.rs:547-558`).
- **KMD side:** `kmd_render/src/ddi/create_allocation.rs:create_one` (2394) →
  `helios_protocol::classify` (`wddm.rs:757`) → `AdoptedUmdResource` →
  `resource_tables.rs:adopt_blob_for_allocation` (548), which re-owns the blob slot to the KMD.
  The KMD writes `HeliosWddmOpenIdentity` back (2567-2617) for openers.
- **Open (DWM, other processes):** `DxgkDdiOpenAllocation` (gated on `resource_is_live`) →
  the D3D11 UMD's `open_ddi_texture2d` (`dxvk_bridge.cpp:609`) imports by resource id through
  `VkImportMemoryResourceInfoMESA`. D3D12 `pfnOpenHeapAndResource` is refused
  (`resource12.rs:3425-3435`).
- **Present (D3D11, `umd/src/forward/present.rs:dxgi_present_impl`, 1281-1658):** a vehicle
  frame is imported and copied (`present_vehicle_copy`); a direct primary goes with no copy; else
  `CopySubresourceRegion` into the DXGI surface. Then `publish_present_order`, `Flush`, the frame
  gate, and a typed `HeliosPresentRenderCmd` through `pfnRenderCb` (814-946) followed by
  `pfnPresentCb` (995). D3D12 does the same with `HeliosPresentRenderCmd` and
  `BroadcastSrcAllocation` (`present12.rs:359-516`).
- **KMD present:** `ddi/display.rs:dxgkddi_present` (179); scanout of the app's own resource
  (`ScanoutTarget::from_snapshot_descriptor`/`from_direct_primary`, zero copy) or a Venus GPU copy
  into the adapter's LINEAR image (`venus/scanout.rs:prepare_optimal_scanout_copy`, 101); then
  `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH` (`virtio/ctrl.rs:715, 814`). The host exports the blob as a
  dma-buf and flips it (`host/backend/device/src/venus/scanout.rs`).

### 2.4 Sync

- **No WDDM monitored fences in the KMD.** `CreateHwQueue`/`SubmitCommandToHwQueue` return
  `STATUS_NOT_SUPPORTED` (`kmd_render/src/ddi/scheduler.rs:182, 200, 240`). DMA completion is the
  legacy submission fence, tied to Venus wire fences (`virtio/gpu/mod.rs:enqueue_submit_inner`,
  `submit_command.rs:signal_dma_completed`).
- **Present ordering ("producer streams").** Each D3D11 device has one exported timeline
  `VkSemaphore` registered with `producer->stream` (escape 0x13); every present signals
  `++value` and `publish`es it on the allocation (`dxvk_bridge.cpp:1367-1449`). The KMD's present
  marker carries `(ctx, value, cookie)` and `stage_worker_scanout_bind` (`gpu/mod.rs:3670`) defers
  the bind until that Venus point retires (`present_stream_marker_boundary`, 5513).
- **D3D12 ECL** (`umd12/src/forward12/queue.rs:185-197, 2847-2870, 2930+`): each
  `ExecuteCommandLists` sends an `HE12` record `{ctx_id, value, cookie, gpu_wire_fence}`
  (`protocol/src/wddm.rs:594-608`) through `pfnRenderCb`; the KMD withholds DMA completion until
  that host stream value retires, which orders the runtime's monitored-fence signals behind the
  real GPU work. D3D12 fences themselves stay with dxgkrnl (`fence.rs:1-9`).
- **Scanout reuse:** the READ LEDGER (`adapter/read_ledger.rs`, escapes 0x0E/0x0F), keyed by
  resource id.

### 2.5 What breaks if the ICD under the UMDs is NVK

1. The device filter: `DXVK_FILTER_DEVICE_NAME="Virtio-GPU Venus"` filters NVK out; D3D11
   `CreateDevice` returns `E_FAIL`. vkd3d picks by device type and may pick either.
2. ICD discovery: no `helios_venus_memory_alloc_info`, so no module and the anchor refuses (D3D11)
   or ctx id 0 (D3D12).
3. Every WDDM-backed texture (D3D11) and every committed resource (D3D12) fails: no resource
   id, no alloc info, no ownership transfer, no producer interface.
4. `OPAQUE_FD`/`DMA_BUF` export flags: NVK on Windows advertises no external handle types at all
   (section 4.6).
5. Cross-process open imports by Venus resource id.
6. Present markers, `HE12` and the present streams name Venus timelines.

None of this is about rendering. DXVK and vkd3d themselves would run on NVK; the Helios glue
around them would not.

## 3. Design

### 3.1 The principle: keep the KMD's resource id as the currency

The KMD's resource tables are the single authority for "a buffer that WDDM, DWM and the scanout
can name" (`resource_tables.rs`). `zero-copy-present.md` section 2 shows every consumer
(attach, adopt, open, scanout bind, flush, ledger, teardown) accepts any live id regardless of
how it was made, and `IMPORT_RM` adds RM-exported memory as such an id. So instead of teaching
the UMDs and the KMD a second kind of buffer, **NVK learns to produce resource ids**, and the
UMDs stop assuming that a resource id came from Venus.

Rejected: giving NVK WDDM allocations of its own (a real `D3DKMTCreateAllocation`-backed memory
manager). Helios' VidMm segment is decorative (`build_paging_buffer.rs:1-29`, research doc
section 2.2) and RM owns the real GPU VA; a WDDM-native NVK would be a new KMD.

### 3.2 A backend-neutral ICD interface

Proposed: one export, `helios_icd_interface_v2(uint32_t version, struct helios_icd_api *out)`,
implemented by both ICDs, replaces the scattered `helios_venus_*` lookups. Its table:

| entry | Venus implementation | NVK implementation |
|---|---|---|
| `backend` | `HELIOS_BACKEND_VENUS` | `HELIOS_BACKEND_NVK_RM` |
| `ctx_id(VkInstance)` | today's ctx id | the holder Venus context NVK creates on its D3DKMT device for `IMPORT_RM` (zero-copy doc section 6, D6), or 0 once S6 removes Venus |
| `memory_res_id(VkDeviceMemory)` | today's | lazily: export the RM memory (`OS_UNIX_EXPORT_OBJECT_TO_FD` 0x3d05), `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` on a forwarded DRM node, `IMPORT_RM` (`protocol/src/foreign.rs`); cached on the memory object. The code exists on Linux (`nvk:nvkmd/rm/nvkmd_rm_drm.c:export_to_gem`, 166) and was exercised from Windows by `guest/rmclient/tests/crm_scanout_smoke.c` |
| `memory_alloc_info` | today's | the RM object size; the memory type index is NVK's |
| `memory_layout(VkDeviceMemory, VkImage)` | none (inferred on the host) | pitch or block-linear, log2 GOBs per block, DRM modifier; needed by the KMD for scanout (zero-copy doc O3, H3) |
| `transfer_ownership` | today's | stop calling `RELEASE_BLOB` for the resid; the RM handle stays NVK's |
| `producer` | today's `helios_producer_api_v1` | a v1 table where `publish` records the RM semaphore point (section 3.5) |
| `queue_gpu_fence` | today's | an RM semaphore `(sync, value)` pair |

The anchor (`bridge_icd_anchor.h`) probes `helios_icd_interface_v2` first and the old export
second, so a new UMD keeps working with an old Venus ICD.

**Export memory.** A resource id needs whole-object exportable memory. DXVK already allocates
WDDM-backed images with dedicated memory; under NVK the fork's `OPAQUE_FD`/`DMA_BUF` export
request (`dxvk_image.cpp:443-462`) becomes a Helios-private pNext (or
`VkExportMemoryAllocateInfo` with a handle type NVK accepts only on Windows) that makes NVK
allocate `NV01_MEMORY_LOCAL_USER` as a standalone, exportable RM object (`nvk:nvkmd/rm/
nvkmd_rm_mem.c:alloc_rm_memory`, 90-138). vkd3d's `VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT` maps to
the same thing. NVK does not expose `KHR_external_memory_fd` on Windows; this stays private.

### 3.3 Instance, device and ICD selection

- **Loader.** No change: DXVK and vkd3d keep using `vulkan-1.dll`. NVK's ICD JSON
  (`nouveau_icd.json`, `vulkan_nouveau.dll`, with `librmclient.dll` next to it,
  `guest/nvk-rm/build-windows.sh:127-141`) is registered under `Khronos\Vulkan\Drivers` by the
  installer as an opt-in.
- **Selection.** A Helios knob `HeliosIcd` (`venus` default, `nvk`), read in
  `umd_common/src/knobs.rs`, optionally per executable. The bridge sets
  `DXVK_FILTER_DEVICE_NAME` to `NVK` instead of `Virtio-GPU Venus` (`dxvk_bridge.cpp:1747`) and
  `helios_entry.c` passes a filter or the chosen physical device to vkd3d instead of
  `VK_NULL_HANDLE` (179).
- **LUID.** NVK must report `VkPhysicalDeviceIDProperties.deviceLUID` = the Helios adapter's
  LUID, `deviceLUIDValid = VK_TRUE`, `deviceNodeMask = 1`. Today NVK never sets them (no "LUID" in
  `nvk:`). librmclient's `find_adapter` (`transport_windows.c:327`) already holds the adapter
  from `D3DKMTEnumAdapters2`; it should expose its LUID (`crm_win_adapter_luid`), and
  `nvk_physical_device.c` fills the properties. This is what Venus does
  (`vn_renderer_helios.c:helios_init_renderer_info`, 4945-4970). It matters because DXVK opens a
  D3DKMT adapter from `deviceLUID` for its KMT bookkeeping (`dxvk:src/dxvk/dxvk_adapter.cpp:32-39`)
  and the Mesa runtime's `vk_icdEnumerateAdapterPhysicalDevices` matches on it.
- **One process, two ICDs.** The loader loads both `vulkan_virtio.dll` and `vulkan_nouveau.dll`.
  That is fine (research doc section 5, "Coexistence"); the anchor then has to allow exactly one
  *selected* backend, not one loaded module.
- **32-bit games** stay on Venus: there is no 32-bit NVK or librmclient yet (research doc P6).

### 3.4 Memory backing WDDM allocations

With section 3.2, `finish_wddm_tex2d` and `adopt_committed_allocation` stay as they are, except:

- `HeliosWddmAllocPrivate.blob_mem` is `HELIOS_BLOB_MEM_RM_EXPORT` (0x80000001) and `blob_id` is
  0 for an NVK resource. `classify` (`wddm.rs:757`) already routes a nonzero `adopt_resource_id`
  with `DEVICE_MEMORY` to `AdoptedUmdResource`; the adopt path needs only to accept a resid that
  has a foreign record (`ForeignTable::adopt`, already written on `kmd/zero-copy-present`).
- The `ctx_id` stamped into the private data is the holder context of section 3.2.
- `HeliosWddmAllocMeta` gains the layout from `memory_layout` (pitch, block height, modifier),
  so the KMD and DWM's importer do not infer it from the size.
- `umd12`'s refusal of suballocated committed resources (`resource12.rs:1702-1718`) is kept: an
  exportable RM object is dedicated anyway.

Private resources (most of a game's memory) never touch any of this: they are NVK
suballocations in VRAM or OS-descriptor system memory, invisible to WDDM, exactly as DXVK's
private resources on Venus are invisible today.

Known gap: NVK on RM has no host-visible VRAM (`has_host_visible_vram = false`, `bar_size_B = 0`,
`nvk:nvkmd/rm/nvkmd_rm_pdev.c:360`). DXVK and vkd3d fall back to cached system memory for upload
heaps. This costs GPU reads over PCIe for dynamic buffers but no extra copies.

### 3.5 Sync between NVK and the KMD's present bookkeeping

NVK's timelines are 64-bit RM semaphores in a system-memory pool, signalled by `SEM_EXECUTE`
release with `NON_STALL_INTERRUPT` (`nvk:nvkmd/rm/nvkmd_rm_sync.c`, `nvkmd_rm_ctx.c:279-300`).
The KMD knows nothing of them. Three levels, in order:

1. **CPU-complete (v1).** Before the UMD submits the present marker, it waits on the CPU for the
   frame's NVK timeline point and sends the marker with `value = 0` ("already complete"). This is
   the zero-copy doc's D9 applied to the D3D11 UMD. It costs one frame of pipelining at most when
   the game is GPU-bound, and nothing in the KMD changes except accepting `value = 0` in
   `present_stream_marker_boundary`. The vehicle's `set_present_source` currently refuses
   `fence_value == 0` and `semaphore_handle == 0` (zero-copy doc section 6); it needs the same
   mode.
2. **An RM-fence boundary in the KMD (v2, needed for D3D12).** The host already turns an RM
   semaphore value into a one-shot event: nvidia-drm `SEMSURF_FENCE_CTX_CREATE` (0x54) and
   `SEMSURF_FENCE_CREATE` (0x55) return a backend handle that sends `EventReady` when the
   semaphore reaches the value (`docs/SYNC.md`, gated on `NVGPU_CFG_DRM_FENCES`, bit 11). The KMD
   adds a producer boundary of kind "RM fence handle" next to the Venus `(ctx, ring, fence)` kind,
   retired from the `nvrm_events` DPC (`virtio/gpu/nvrm_events.rs` on `kmd/nvrm-events`). Present
   markers and `HE12` records then carry `{rm_fence_handle}` instead of `(ctx, value, cookie)`,
   and `stage_worker_scanout_bind` and DMA-completion wait on it. Today a handle created through
   `FORWARD` is not recorded as owned (only `Open` registers handles), so `EVENT_REGISTER` on it
   returns `NOT_OWNED`; the KMD must recognise the 0x55 reply or offer a dedicated verb.
3. **WDDM-native fences (later, optional).** Real monitored fences or hardware queues
   (`DxgkDdiCreateHwQueue`) so dxgkrnl's own scheduler sees NVK completion. Not needed for games;
   it is what a "real" WDDM driver would do and may be needed for some D3D12 fence-sharing
   corner cases (section 7).

DXVK's and vkd3d's producer code (`dxvk_helios_producer.cpp`, `vkd3d:helios_producer.h`) stays:
NVK's `producer` table implements `stream` (allocate a stream id from the KMD or locally),
`publish` (v1: CPU wait then publish 0; v2: `SEMSURF_FENCE_CREATE` on the timeline point and pass
the handle) and `status`/`wait` (CPU waits on the RM semaphore via `crm_event_wait`).

### 3.6 Present

Three paths, each mapping onto something that exists:

| path | today on Venus | NVK |
|---|---|---|
| windowed (DWM composes) | `CopySubresourceRegion` into the DXGI surface; DWM opens it by resid and imports through Venus | the DXGI surface is NVK memory with a foreign resid. **DWM stays on Venus** and imports it through `VkImportMemoryResourceInfoMESA`; the host must import the dma-buf-origin resource into NVIDIA's Vulkan driver (zero-copy doc H5, O1). Zero CPU copies, one GPU copy inside the app (the same copy the Venus path makes) |
| direct flip / fullscreen | `from_direct_primary` or a snapshot descriptor → `SET_SCANOUT_BLOB` of the app's resid | same verbs with the foreign resid; the host exports it from the GEM object with the layout NVK reported (H1, H3), so no Venus import is needed on this path |
| Vulkan apps through the vehicle | Venus WSI → `helios_umd_set_present_source_v4` | NVK's Helios WSI mode (research doc P5) → same exports with a foreign resid, CPU-complete |

**`ScanoutFlip` and "Option B".** `kmd/foreign-scanout` today only lets user mode send
`ScanoutFlip` (msg 20) through `HELIOS_NVRM_OP_FORWARD` (`kmd_render/src/virtio/nvrm.rs:forward`,
arm `MSG_SCANOUT_FLIP`); the KMD's present path never sends one, and there is no commit
implementing a KMD-driven flip yet. A user-mode flip races the KMD's own Venus flushes on the
same `DisplayLink` (`host/backend/device/src/display.rs:flip` 1515, `flip_dmabuf` 1555), and the
zero-copy doc rejects it as the production path (D12). The KMD-driven version this design wants:
in `program_vidpn_source_inner` (`ddi/display.rs:2326`), when the flipped allocation adopted a
foreign resid, send `ScanoutFlip{owner_handle, host_handle, stride, fourcc, modifier, seq}` from
the foreign record (which then has to carry the layout) instead of `SET_SCANOUT_BLOB` +
`RESOURCE_FLUSH`. It needs no Venus at all, which is why S6 depends on it.

Two host gaps on that path: `handle_scanout_flip` (`host/backend/device/src/nvidia/scanout.rs:26`)
answers a bare header with no completion, and the viewer's `EV_RELEASE` (`display.rs:261`) is
not forwarded to the guest. The KMD's READ LEDGER needs one of them to know when a flipped
buffer can be reused.

### 3.7 Sharing between processes and with DWM

- **DXGI shared handles and NT handles** are WDDM allocation-level objects
  (`D3DKMTShareObjects`, `OpenResourceFromNtHandle`). They work for foreign resids unchanged,
  because the opener reads `HeliosWddmOpenIdentity` and gets a resid.
- **The opener must be able to import that resid.** A Venus process (DWM) imports a foreign resid
  through the host import of section 3.6. An **NVK process cannot import a Venus resid**: there
  is no Venus/host-Vulkan → RM direction in the design (research doc 4.3, "Rejected
  alternative"). For a game this mostly does not matter (it produces and DWM consumes), but it
  breaks an NVK app that opens a surface DWM or another Venus process created: GDI-compatible
  surfaces (`KmdOptimalGdiTexture`), D3D9On12/D3D11On12 interop with a Venus device, video
  surfaces. Those apps stay on Venus until S6. One unknown could fix it early: whether the
  NVIDIA Vulkan driver's `OPAQUE_FD` export on the host is an RM object fd that
  `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD` (0x3d06) can import into the guest's RM client
  (spike X4).
- **Keyed mutex.** DXVK uses upstream `D3DKMT*KeyedMutex` (`dxvk:src/util/util_gdi.cpp:144-183`),
  a dxgkrnl object; it does not depend on the ICD. Its GPU-side ordering does, and with NVK it
  degenerates to CPU waits (v1 sync).
- **RM handles across processes are not shareable** and must not become so. The KMD keys NVRM
  ownership per D3DKMT device (`nvrm_tables.rs`, `DeviceOwner`). Handles inside payloads (the
  0x3d05/0x3d06 fd, NVKMS `memFd`, `RM_DUP_OBJECT`'s `hClientSrc`) are not checked today, so
  cross-process sharing would "work" by accident (`host/backend/device/src/nvidia/nested.rs`
  around 143-185). The sanctioned route across processes is always a resid.

## 4. What NVK must provide for DXVK and vkd3d-proton

### 4.1 DXVK

DXVK requires Vulkan 1.3 and a fixed feature list (`dxvk:src/dxvk/dxvk_device_info.cpp:
getFeatureList`, 822-1117; extensions `EXT_depth_clip_enable`, `EXT_robustness2`,
`KHR_load_store_op_none`, `KHR_maintenance5`, `KHR_maintenance6`, `KHR_swapchain`). NVK on
Blackwell has all of them (`nvk:nvk_physical_device.c:nvk_get_device_extensions`, 113-362; no
extension is gated above `TURING_A`, and `nvk_is_conformant` lists `BLACKWELL_B`). DXVK
recognises `VK_DRIVER_ID_MESA_NVK` and takes its descriptor heap/buffer paths
(`dxvk_device_info.cpp:500, 522`). The fork adds no required extension; its Helios paths are the
ones section 3 replaces.

### 4.2 vkd3d-proton

Hard requirements (`vkd3d:libs/vkd3d/device.c`: Vulkan 1.3, vertex attribute divisor, transform
feedback queries, robustness2 with `robustImageAccess2`, `KHR_push_descriptor`, maintenance5/6,
descriptor indexing, then mutable descriptors or descriptor buffer): NVK has all of them
(`KHR_push_descriptor` line 186, `EXT_descriptor_buffer` 240, `EXT_mutable_descriptor_type` 280,
`EXT_robustness2` 304).

The Helios fork adds an FL12 admission check (`helios_vkd3d_validate_native_feature_level`,
`device.c:11476-11542`): native `VK_EXT_device_generated_commands`, binding tier 3,
**conservative rasterization tier 3**, `maintenance8`, `EXT_depth_range_unrestricted`, int8,
storage-image MSAA and write-without-format. NVK has DGC (243), `maintenance8` (172) and
`depth_range_unrestricted` (246, Volta+), but reports
`fullyCoveredFragmentShaderInputVariable = false` (`nvk_physical_device.c`, around 1146-1155), so
`d3d12_device_determine_conservative_rasterization_tier` (`device.c:10030`) returns tier 2 and
**the fork's admission refuses FL12 on NVK**. Upstream vkd3d-proton does not have this check;
the fork added it to avoid admitting FL12 on hosts without its own emulation. Either the check is
relaxed for NVK (decision D4) or D3D12 on NVK is FL 11_x.

Further, from the engine's feature-level derivation (`device.c:10721-10755`) and `umd12`'s caps
(`caps12.rs`):

| D3D12 capability | NVK | effect |
|---|---|---|
| FL 12_0 (tiled tier 2, binding tier 2, typed UAV loads) | sparse residency advertised (`nvk_physical_device.c:398-405`); sparse on the RM backend is unverified for an unprivileged client and partial unbinds remap (`nvkmd_rm_va.c:25`) | likely, needs X3 |
| FL 12_1 (ROVs + conservative raster tier 1) | no `EXT_fragment_shader_interlock`, so no ROVs | FL 12_0 at most |
| DXR (`KHR_ray_tracing_pipeline`, `acceleration_structure`, `ray_query`) | absent in NVK, also upstream | no DXR; DXR games stay on Venus |
| mesh shaders, VRS, barycentrics | `EXT_mesh_shader`, `KHR_fragment_shading_rate`, `KHR_fragment_shader_barycentric` present | `umd12` hard-zeroes them today (`caps12.rs:895-925`); can be enabled later |
| `TotalLaneCount` | `NV_shader_sm_builtins` present (336) | `umd12`'s Venus guess (`lib.rs:271-276`) can be replaced by the real number |
| ExecuteIndirect | `EXT_device_generated_commands`; vkd3d needs `supportedIndirectCommandsShaderStages` to cover all stages (`device.c:3413-3426`) | check on GB202 |

### 4.3 Windows-specific gaps in NVK

| gap | needed by | fix |
|---|---|---|
| `deviceLUID` not reported | adapter matching, DXVK KMT, Mesa runtime | section 3.3 |
| no external handle types on Windows (`has_dma_buf` and `has_alloc_tiled` are false, `nvkmd_rm_pdev.c:350-356`; the DRM side is stubbed, `nvkmd_rm.h:262-278`) | resource ids | the private export path of section 3.2; on Windows `nvkmd_rm_drm.c` must be built for the export half only, through librmclient's DRM ioctl forwarding |
| no `KHR_external_memory_win32`, `external_semaphore_win32`, `win32_keyed_mutex` | upstream DXVK's shared resources when not using Helios' KMT path; app-level Vulkan interop | not needed for S1-S5; S6 or later |
| WSI is GDI software only (patch 0020; `nvk_wsi.c:27` sets `sw_device`) | app-local present | S1 accepts it; S2 adds the Helios mode |
| no host-visible VRAM | upload-heap performance | later; depends on the window question in the research doc section 2.1 |
| no 32-bit build | 32-bit games | research doc P6 |

## 5. Staged plan

Sizes are relative (S = days, M = 1-2 weeks, L = 3-6 weeks, XL = months), for one person on the
named side, after the NVK bring-up now in progress (vulkaninfo, compute, vkcube) is done.

### S1: Heaven on app-local DXVK and NVK (smallest first step; S)

No Helios, KMD or host change. It needs only what the bring-up delivers.

1. Build or download upstream DXVK (mingw, a release whose requirements section 4.1 checks) and
   copy its x64 `d3d11.dll` and `dxgi.dll` next to the 64-bit Heaven executable under
   `W:\Heaven` (verify the bitness of the binary the launcher starts; the x86 one cannot use NVK).
   An app-local `d3d11.dll` is loaded before the system runtime, so the Helios UMD is never
   loaded in that process.
2. Run with `NVK_RM=1`, `VK_DRIVER_FILES=<path>\nouveau_icd.json` (so the loader does not even
   load Venus), `DXVK_HUD=devinfo,fps,frametimes`, `DXVK_LOG_LEVEL=info`, windowed at 1920x1080
   first, DX11 renderer.
3. Present is NVK's GDI WSI: GPU blit to OS-descriptor sysmem, CPU copy into a DIB,
   `StretchBlt`, then DWM on Venus. Expect the copies to cap fps; the HUD's GPU time and
   Heaven's own numbers still show NVK's render speed.
4. Compare with the same Heaven scene on Venus (about 150 fps at the test resolution,
   `HELIOS.md`), and check the KMD's pin/map counters and backend handles after a long run (no
   leaks; research doc P4).

Done when: Heaven renders correctly through NVK, with numbers. This is research doc step P4
applied to Heaven.

### S2: zero-copy present for app-local DXVK (L, mostly host + KMD + NVK WSI)

The zero-copy design as written: host H1-H7 (`RESOURCE_CREATE_BLOB` with
`HELIOS_BLOB_MEM_RM_EXPORT`, layout, lifetime, feature bit), KMD opens the `IMPORT_RM` gate,
librmclient gets an `IMPORT_RM` call, NVK gets a Helios WSI mode that hands a CPU-complete foreign
resid to `helios_umd_set_present_source_v4`, and the vehicle accepts it. A cheap step before it
(S, optional): make the DIB the blit target through host-pointer import (research doc P3b), which
removes one CPU copy with no KMD or host change. Done when Heaven windowed on NVK matches its
render-only fps within a few percent.

### S3: the Helios D3D11 UMD on NVK, hybrid (L)

- `helios_icd_interface_v2` in both ICDs (section 3.2); the anchor and `bridge_icd_exports.cpp`
  switch to it.
- `HeliosIcd` knob, device filter, NVK `deviceLUID` (section 3.3).
- DXVK fork: the export request for WDDM-backed images goes through the interface instead of
  `OPAQUE_FD`/`DMA_BUF`; the producer binding uses the interface's table.
- v1 CPU-complete present (section 3.5); vehicle and present marker accept `value = 0`.
- KMD: adopt foreign resids (already written), layout in `HeliosWddmAllocMeta`.
- Host: H5, Venus import of a dma-buf-origin resource, so DWM can compose the NVK back buffer.

Done when Heaven runs with no app-local DLLs, `HeliosIcd=nvk`, windowed (DWM imports the NVK
surface) and fullscreen (direct flip of the foreign resid), with no CPU copy.

### S4: RM-fence boundary (M-L, mostly KMD)

Section 3.5 level 2: `SEMSURF_FENCE_CREATE` handles owned by the device, a boundary kind retired
from `nvrm_events`, present markers and `HE12` carrying it. NVK's producer `publish` uses it. Done
when the S3 present no longer CPU-waits and frame pacing is at least as good as Venus.

### S5: D3D12 through `umd12` on NVK (L)

- vkd3d fork: `VKD3D_HEAP_FLAG_HELIOS_VENUS_EXPORT` maps to NVK's export request; physical device
  selection by filter or LUID (`helios_entry.c:179`); producer through the interface.
- `HE12` records with the S4 boundary so DMA completion (and so the runtime's fence signals)
  follow NVK's work.
- FL12 admission relaxed for NVK (D4), caps for FL 12_0, no DXR, real `TotalLaneCount`.
- Done when a D3D12 sample and then a D3D12 game render with `HeliosIcd=nvk`, and their fences
  are correct under load (no early signal: a test that reads back after `Signal`/`SetEventOnCompletion`).

### S6: DWM on NVK, Venus removed (XL)

What still needs Venus after S5, and its replacement:

| still Venus | replacement |
|---|---|
| DWM's D3D11 device | DWM loads the Helios UMD like any app; `HeliosIcd=nvk` for DWM |
| the KMD's own allocations: `KmdLinearPrimary`, `KmdOptimalGdiTexture`, `KmdStandardBuffer` (`wddm.rs:701`), the LINEAR scanout image and its GPU copy (`venus/scanout.rs`) | the KMD needs its own RM client (the KMD can issue `FORWARD` itself) to allocate RM memory for them and export it to a GEM handle; GDI paging transfers (`build_paging_buffer.rs`) on the CPU or the copy engine |
| scanout through `SET_SCANOUT_BLOB` / `RESOURCE_FLUSH` | KMD-driven `ScanoutFlip` (Option B, section 3.6), plus a release signal from the host |
| Mesa Vulkan WSI vehicle, Zink (OpenGL), the Venus-only escapes (0x01, 0x04, 0x10, 0x12, 0x13) | NVK's WSI on the D3D11 swapchain of the Helios UMD; Zink on NVK; escapes retired |
| `KHR_external_memory_win32` etc. for apps that use Vulkan/D3D interop | NT-handle export of a foreign resid through a WDDM allocation, in NVK |
| WDDM-visible scheduling and TDR | optional: hardware queues and monitored fences (section 3.5 level 3) |

### Effort and order

| stage | size | depends on | confidence |
|---|---|---|---|
| S1 | S | NVK bring-up | high, unless NVK misrenders Heaven |
| S2 | L | zero-copy host half, KMD gate | medium (O1 below) |
| S3 | L | S2's host import, H5 | medium |
| S4 | M-L | KMD, host `SEMSURF` path (exists) | medium |
| S5 | L | S3, S4 | medium-low (vkd3d breadth, sparse) |
| S6 | XL | S3-S5 | low |

S1 and the spikes below can start now. S2 and S3 share their hardest piece (host import of an
RM-exported buffer), so it should be retired once, first.

## 6. Risks and spikes

| # | risk | spike that retires it | size |
|---|---|---|---|
| 1 | **NVK is not near bare-metal on Blackwell for games.** Venus already reaches about 150 fps in Heaven on NVIDIA's own compiler. If NAK and NVK are much slower on GB202, S3-S6 buy nothing. This is the strategic risk. | **X1:** S1 itself, plus the same DXVK + Heaven under Wine on the Linux `lab` guest with NVK-on-RM (no Windows WSI copies there, dma-buf present works), against the host's proprietary driver and against Venus. | S |
| 2 | **Cross-driver import on the host.** The hybrid (S3) and the vehicle path (S2) need NVIDIA's Vulkan driver in `conduit-venus` to import an nvidia-drm dma-buf of RM memory, block-linear, as a `VkImage` that matches NVK's layout (zero-copy doc O1, H5). | **X2:** host-only: allocate RM memory in a test client, export to GEM, `PRIME_HANDLE_TO_FD`, import into a Vulkan device of the proprietary driver with `VK_EXT_image_drm_format_modifier` and the NVIDIA block-linear modifier, sample it, compare with what NVK wrote. Also a `ScanoutFlip` of the same buffer. | S-M |
| 3 | **Sync without Venus fences.** WDDM's present ordering and D3D12's ECL fences need NVK completion that the KMD can see; the CPU-wait v1 may cost too much pacing; `SEMSURF` events via virtio may add latency. | **X3:** from Windows, `SEMSURF_FENCE_CTX_CREATE`/`SEMSURF_FENCE_CREATE` through `FORWARD` (on a KMD build that records the handle), then `EVENT_REGISTER`, and measure semaphore-release-to-`KeSetEvent` latency with `crm_event_smoke`. Also run sparse binding from the RM client here (`NVOS32_ALLOC_FLAGS_SPARSE`), since vkd3d's FL 12_0 depends on it. | S |
| 4 | An NVK process cannot import a Venus resid (GDI surfaces, interop, DWM-created shared surfaces). | **X4:** on the host, does NVIDIA Vulkan's `OPAQUE_FD` export import into an RM client with 0x3d06? If yes, the KMD can offer the reverse import and many app-compatibility cases disappear. | S |
| 5 | The vkd3d fork's FL12 admission refuses NVK (conservative raster tier 2); no ROVs, no DXR. | decision D4; DXR titles stay on Venus. | — |
| 6 | Two ICDs in one process (loader loads both) disturb the anchor or each other's D3DKMT state. | covered by S3 bring-up; `VK_DRIVER_FILES` isolates S1. | — |
| 7 | GPU contention: DWM on Venus and the game on NVK are two host contexts time-sliced by RM. | measure in S3 (frame pacing with DWM composing). | — |
| 8 | Escape cost for RM memory binds in map-heavy workloads (sparse, many small allocations). | measure in S1/S5; NVK's suballocation already limits binds (research doc risk 8). | — |

## 7. Who does what

### KMD (Windows driver session)

- S2: open the `IMPORT_RM` gate once the host feature bit exists; vehicle-side acceptance of a
  CPU-complete foreign resid.
- S3: adoption of foreign resids in `create_one` (written), layout in `HeliosWddmAllocMeta` and
  the foreign record, `value = 0` in present markers.
- S4: the RM-fence boundary: own the 0x55 reply handle (or a dedicated verb), a boundary kind
  retired from `nvrm_events`, `HE12` and present markers that carry it.
- S6: KMD-driven `ScanoutFlip` for foreign resids in `program_vidpn_source_inner` (Option B), a
  KMD RM client for KMD-owned allocations, READ LEDGER fed by the host release signal.
- Hardening (security last): check handles inside `FORWARD` payloads (the 0x3d05/0x3d06 fd,
  `memFd`, `RM_DUP_OBJECT`); ownership checks on `ATTACH_RESOURCE` for foreign resids (zero-copy
  doc section 8).

### UMD, NVK, librmclient (this side)

- librmclient: `crm_win_adapter_luid`; `IMPORT_RM`; the DRM ioctls the export path needs (as in
  `crm_scanout_smoke.c`); `SEMSURF` fence create for S4.
- NVK: `deviceLUID`; the private export request and resid cache (`nvkmd_rm_drm.c` export half on
  Windows); `helios_icd_interface_v2`; the producer table; the Helios WSI mode (S2).
- Helios UMDs: anchor and ICD lookup through the interface; `HeliosIcd` knob and device
  filters; CPU-complete present (v1); `umd12` FL and caps for NVK.
- DXVK and vkd3d forks: export requests and producer binding through the interface; FL12
  admission (D4); vkd3d physical-device selection.
- Mesa-helios (Venus ICD): implement `helios_icd_interface_v2` as a thin wrapper of today's
  exports.

### Host backend

- S2/S3: zero-copy doc H1-H7, most importantly H5 (Venus import of an RM-exported dma-buf).
- S4: nothing new if the `SEMSURF` path in `docs/SYNC.md` works for Windows callers as it does for
  Linux ones; check the `NVGPU_CFG_DRM_FENCES` bit is set for `win11`.
- S6: a completion or release for `ScanoutFlip` (`handle_scanout_flip` replies with a bare
  header; `EV_RELEASE` is not forwarded), and the layout carried per GEM object.
- None for S1.

## 8. Decision list

| # | decision | why |
|---|---|---|
| D1 | The KMD resource id stays the only cross-component buffer name; NVK produces resids through `IMPORT_RM`. | Every KMD consumer accepts it unchanged; no second buffer model in the KMD or the UMDs. |
| D2 | One backend-neutral ICD export table (`helios_icd_interface_v2`) instead of NVK imitating `helios_venus_*`. | The Venus names encode Venus concepts (blob id, exact-size rule, Venus ctx); a table with a backend tag lets the UMDs branch where they must. |
| D3 | Hybrid first: games on NVK, DWM on Venus, until S6. | DWM, GDI and the KMD's own allocations are the largest part of the Venus dependency, and none of them is performance-critical for games. |
| D4 | **Proposed, needs the user's decision:** relax the vkd3d fork's FL12 admission for `VK_DRIVER_ID_MESA_NVK` (accept conservative raster tier 2), as upstream vkd3d-proton does. | Otherwise D3D12 on NVK is FL 11_x and most D3D12 games refuse to start. |
| D5 | v1 sync is a CPU wait; the RM-fence KMD boundary follows. | Smallest thing that works for D3D11; D3D12 waits for S4. |
| D6 | Opt-in per executable through `HeliosIcd`; Venus remains the default and the fallback for 32-bit, DXR and interop-heavy apps. | NVK lacks DXR, 32-bit and Venus-resid import. |

## 9. Open questions for the user

- **Q1 (D4):** may the vkd3d fork admit FL12 on NVK with conservative raster tier 2?
- **Q2:** S1 needs upstream DXVK binaries in the guest. Use an upstream release, or build the
  Helios fork without its Helios paths so that S1 and S3 compare the same DXVK?
- **Q3:** is the per-executable opt-in (D6) the right granularity, or should `HeliosIcd=nvk` be
  global with an app deny-list?
- **Q4:** S6 (Venus removal) needs a KMD-side RM client. Is that the KMD session's direction, or
  should KMD-owned allocations stay on Venus indefinitely while everything else moves?
