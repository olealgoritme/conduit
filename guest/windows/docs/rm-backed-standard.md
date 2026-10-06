# KMD-made STANDARD allocations for DWM on NVK: RM-backed or dma-buf (scoping and design)

Status: written on `kmd/rm-backed-standard-design` over `8aba6f4` (the tip of `worktree-kmd-start-debug`: it has the
`ForeignFlip` arm and section 15.18 of `kmd-rm-client.md`; the commit named in the request, `aeda363`, is its
ancestor and has neither). **Design only.** The only code is the pure, host-tested
`kmd_logic/src/rm_standard.rs` (14 tests; 888 in the crate), which nothing in `kmd_render` calls. Nothing here was
built into a driver or run in a guest.

Evidence rules. "Read" is `file:line` in this tree. "Branch" is read from another branch, named, and not part of
this tree (`feat/dwm-on-nvk`, `feat/umd-nvk-combined`, `feat/rm-export-map-blob`, the Mesa worktree `mesa-nvk-s6`,
branch `s6-order2`); treat those as secondary evidence. "Unknown" is said where it is. Section numbers 12, 14, 15
refer to `kmd-rm-client.md`; `shared-foreign-surfaces.md`, `zero-copy-present.md` and `nvrm-escape.md` are in this
directory.

## 0. The answer in ten lines

1. What DWM on NVK cannot open today is the CPU-visible KMD standard buffer (shadow, staging, every GDI surface type
   but `TEXTURE`): a Venus present-buffer blob, STANDARD identity, 96 bytes of private data, no layout trailer, so the
   NVK opener finds no layout and substitutes a blank texture (`feat/umd-nvk-combined` `umd/bridge/dxvk_bridge.cpp:1634-1662`).
   The one GPU-only kind, `GDISURFACE_TEXTURE`, is a tiled Venus image whose layout is the host driver's own and is recorded nowhere.
2. **Nobody has the census** of which standard types, sizes and formats DWM actually opens; T1 says only
   "mem_type 0". Stage S-A0 gets it first (a UMD log grep, plus 14 KMD counters whose slots and names are
   `rm_standard::hist_slot` / `hist_name`).
3. **Recommended first code step, S-A: the dma-buf route (D), not an RM allocation.** Keep the Venus blob, give it a
   foreign record with a LINEAR layout, count its opens, let `RM_RESOURCE_IMPORT` serve it, flag the identity. About
   300 lines in 5 or 6 files; the host verb for a Venus dma-buf blob exists (`rm.rs:226-239`, branch). It cannot
   break a Venus DWM, which ignores the new bit; the RM variant can (see 2.3).
4. **S-B is the RM-backed step (R)**, for the same kinds, from CACHED RM system memory through the level 5 service,
   behind a new sub-knob, after the level 5 hardware checklist (15.13 steps 2 to 5) has passed, `NvDupHarden` is 1
   and the Venus UMD refuses the new bit cleanly. About 600 to 800 lines in 10 to 12 files. It is the way to remove
   Venus from the KMD; D is not.
5. Cached, not write-combined: these allocations get `Cached` from dxgkrnl (CPU view write-back), so cached RM memory
   behind them makes no alias; the primary's write-combined default exists only because dxgkrnl refuses `Cached` on a
   primary (15.5). WC would make a 1920x1080 GDI read 0.1 s (about 80 MB/s) against 0.3 ms cached (26.8 GB/s).
6. The pitch the KMD authors (256-aligned) is not the pitch NVK gives a LINEAR image (128-aligned) for widths whose
   row is an odd multiple of 128 bytes (800 px: 3328 against 3200). Whoever opens must pass the stride, or the KMD
   must author the 128-aligned one. Pinned by tests; the choice is the NVK owner's.
7. NVK's shared-surface import hardcodes "VRAM, big pages" (`nvkmd_rm_win.c:764`, branch); both routes need it to ask
   RM whether the memory is system memory, as the Linux dma-buf import already does (`nvkmd_rm_drm.c:250-265,360-375`).
8. The UMD must derive "foreign" from the STANDARD identity bit (private data has no room for the trailer), or the KMD
   reports 128 bytes for standard allocations (`PRIV_SIZE`, `create_allocation.rs:4048`).
9. R makes window pixels reachable through `RM_DUP_OBJECT` while `NvDupHarden` is its default 2 (log-only): a gate.
10. S-C (GPU-only `TEXTURE`) waits for the census. Open questions for the host session are in section 11.

## 1. What creates these allocations today

### 1.1 The entry: `DxgkDdiGetStandardAllocationDriverData`

The OS (dxgkrnl on behalf of win32k, DWM, IddCx) asks the KMD for private data and a pitch; the KMD answers, and the
OS hands the same bytes back to `CreateAllocation`. Read: `ddi/create_allocation.rs:4034-4248`.

| step | what | line |
|---|---|---|
| size query (`pAllocationPrivateDriverData` null) | per-allocation and per-resource size are both `PRIV_SIZE` = `HeliosWddmAllocPrivate` 48 + `HeliosWddmAllocMeta` 48 = **96** bytes, for every type | 4048-4059 |
| the union arm by `StandardAllocationType` | `SHAREDPRIMARYSURFACE` (1), `SHADOWSURFACE` (2), `STAGINGSURFACE` (3, format forced `A8R8G8B8`), `GDISURFACE` (4). Anything else (`VGPU`, `FENCESTORAGE`, unknown): `STATUS_NOT_SUPPORTED` | 4078-4118 |
| pitch out-field | shadow, staging and GDI: `sd.Pitch = RowPitch::linear(0, width)` = `cross_adapter_pitch(width)` = `width * 4` rounded to **256**, whatever the format's real bits per pixel. The OPTIMAL GDI texture reports 0 | 4085, 4090, 4102-4111, `kmd_logic/src/lib.rs:199-219` |
| size | `paging::linear_blob_size(pitch, height)` = `pitch * align(height, 128) + 64 KiB`: deliberately LARGER than pitch x height (0x1E10000 against 0x1C20000 at 5120x1440, `kmd_logic/src/paging.rs:450-460, 881-888`); the OPTIMAL texture uses `w*h*4` as a placeholder | 4142-4151, 463-478 |
| cache word | `map_cache` WC for the primary, CACHED for everything else | 4153-4157 |
| the private record | `HeliosWddmAllocPrivate` (`kind` STANDARD, `blob_id` 0 so the host makes the memory, `ctx_id` the KMD's own Venus context, `HOST3D`, `USE_MAPPABLE`) and the meta: width, height, D3DDDIFORMAT, pitch, bind 0x28 (shader resource and render target), `misc_flags` = standard type in bits 24-27, GDI type in bits 20-23, plus `PRIMARY` (bit 31) or `OPTIMAL_GDI_TEXTURE` (bit 29); `dxgi_format` 87/88 for the primary and for A8R8G8B8/X8R8G8B8, and 0 ("legacy zero hint") for `A8B8G8R8` and everything else | 4158-4213 |

`protocol/src/wddm.rs:85-108` names the same bit ranges; `misc` in a UMD log line decodes as
`std = (misc >> 24) & 0xF`, `gdi = (misc >> 20) & 0xF`.

### 1.2 `CreateAllocation`: classify, then one of four arms

`create_one` (2769) reads the private data (2791-2864; a STANDARD allocation with `ap.ctx_id == 0` takes the KMD's own
context), calls `helios_protocol::classify` (2869; `protocol/src/wddm.rs:911-961`: adoption first, then primary, then
OPTIMAL texture, then any other standard, then raw), and `build_backing` (2431) makes the memory.

| StdType / GdiType | arm (`build_backing`) | memory | CPU-visible | VidMm placement | identity word | line |
|---|---|---|---|---|---|---|
| 1 `SHAREDPRIMARYSURFACE` | `KmdLinearPrimary`: level 5 RM sysmem when the service takes it, else a Venus LINEAR scanout image blob | RM system memory (WC), or Venus | yes (WC, no `Cached`: dxgkrnl refuses it on a primary) | BAR segment preferred + aperture; `AccessedPhysically` | STANDARD; bit 1 `FOREIGN_SYSMEM` when RM | 2565-2622, 2085-2091 |
| 4 / 1 `GDISURFACE_TEXTURE` | `KmdOptimalGdiTexture`: a cross-context OPTIMAL Venus image (DMA_BUF export), `pitch` 0, `venus_image_id` set | Venus device-local image | **no** | aperture only, `CpuVisible` 0, no `Cached`, not `bar_eligible`; paging content ops skipped | STANDARD, `OPTIMAL_GDI_TEXTURE` in `misc` | 2623-2668, 2065-2067, 3068-3070 |
| 2 shadow, 3 staging, 4 / {0, 2, 3, 4, 5, 6, 7, 8} | `KmdStandardBuffer { primary: false }`: `allocate_present_buffer_blob`: a Venus `VkBuffer` + `VkDeviceMemory` (host-visible, cached, DMA_BUF export, dedicated), an initial family release submitted and fenced, the buffer registered in the Present-buffer ownership table | Venus host-visible memory, CACHED | **yes** | BAR segment preferred + aperture (`display_half` or primary), `CpuVisible`, `Cached` (when `AllocCached`, default 1) | STANDARD, bit 0 `DEDICATED_PRESENT_BUFFER` | 2669-2726, `venus/commands.rs:300-499`, `create_allocation.rs:2709-2714, 3222-3248` |

Three facts about the third row. (a) Every non-primary standard buffer is a dedicated Present buffer: the identity
bit 0 is what makes the Venus UMD import it as a linear image over its own memory (`umd/src/forward/resource.rs:1481-1511`,
this tree), and what makes the KMD authorize the open per process (3761-3786) and wait for consumers at destroy
(2232-2239). (b) Of the GDI types the OS may ask for, the KMD treats every one but `TEXTURE` as the same buffer,
including `LOOKUPTABLE` (a palette surface) and `EXISTINGSYSMEM` (dxgkrnl supplies the pages): the 4-byte pitch and the
blob size are authored regardless of the format. (c) The memory-type index in the identity is the KMD's Venus
client's (`kernel_mti`, 2691-2701), 0 in a T1 run; an NVK opener does not consult it.

### 1.3 The identity and the open (what DWM sees)

`write_open_identity` (1881-1932) stamps `HeliosWddmOpenIdentity` (48 bytes) over the per-allocation private data at
create (2985-3011) and at every open (3849-3860), and over the resource-level copy once per call (3897-3905). For
STANDARD the two reserved words are the contract flags: bit 0 dedicated present buffer, bit 1
`HELIOS_WDDM_STANDARD_CONTRACT_FOREIGN_SYSMEM` (`protocol/src/wddm.rs:437-450,520-526`), set only from the KMD's own
record (`ident.foreign`, 3003, 3684). The `HeliosWddmAllocLayout` trailer sits at offset 96 and needs 128 bytes
(`protocol/src/wddm.rs:307-309`); a standard allocation has 96, so `write_foreign_layout_trailer` finds no room
(1728) and the open counts `FgOpNoRm` (3874). `OpenAllocation` also calls `foreign_open` for every identified
allocation (3670-3705): for an adopted foreign record it counts a per-process open row (the lifetime and
`RM_RESOURCE_IMPORT` rules of `shared-foreign-surfaces.md` 3 and 6.1 then apply), for anything else it is
`NotForeign`. The call-level `Pitch` out-field is `RowPitch::resolve(misc, meta.pitch, width)` (3913): the authored
pitch, 0 for the OPTIMAL texture.

What the opener's UMD does with it:

* **Venus UMD** (this tree, `umd/src/forward/resource.rs:1325-1572`): `read_open_identity`, then
  `dev.dxvk.open_texture2d(width, height, dxgi, bind, misc, hKM, resid, venus_alloc_size, memory_type_index, tracker,
  scanout_linear=false, ..., cross_context_optimal, dedicated_present_buffer)`: a Venus import by resource id (an
  OPTIMAL image for the texture, a linear image over the dedicated buffer for the rest). Works today; this is DWM's
  baseline (T0).
* **NVK UMD** (`feat/umd-nvk-combined`): `foreign_layout` comes from the 128-byte trailer
  (`umd/src/forward/resource.rs:1566-1583`) and `nvk_can_open = foreign && ICD cap SHARED_IMPORT && not scanout`
  (`umd/bridge/dxvk_bridge.cpp:1634-1662`). For anything else, in `dwm.exe`, it makes a **blank placeholder** of the
  same size (so a window composes black and DWM lives); any other NVK process fails the open. A KMD standard
  allocation has no trailer, so it is always the placeholder. That is the problem this document is about.

### 1.4 Who decides pitch, format and tiling

| property | decided by | value | line |
|---|---|---|---|
| row pitch of a pitched standard buffer | `GetStandardAllocationDriverData` (so before `CreateAllocation`, and returned to win32k as `Pitch`) | `cross_adapter_pitch(width)` (256-aligned) | 4085-4141 |
| row pitch of the primary | the creating arm: Venus `scanout.row_pitch` (not a function of width), or the RM layout | 256-aligned for RM | 2581, `kmd_logic/src/rm_sysmem.rs:210-230` |
| format | the OS's `D3DDDIFORMAT` -> `d3dddi_to_dxgi` | 87, 88, 28; else 0 | 532-545, 4201-4210 |
| tiling | pitched buffers: LINEAR bytes, by construction. `TEXTURE`: Vulkan OPTIMAL, the host driver's own layout, unknown to anyone else (no modifier recorded) | | 2635-2667 |
| size | Venus: the guess of `linear_blob_size`, authoritative for these arms (`blob_size: HostAuthoritative(ap.size)`); RM: RM's answer, page-rounded (15.4) | | 2708, 4142 |

### 1.5 CPU-visible and GPU-only: who touches the bytes

* **CPU-visible (every pitched buffer).** dxgkrnl CPU-maps it through the CPU host aperture
  (`ddi/cpu_host_aperture.rs`: whole allocation, consecutive pages, `RESOURCE_MAP_BLOB` at the offset dxgkrnl chose in the
  host-visible window, region 3 = `SHM_ID_VENUS`, `virtio/pci_caps.rs:101-107`; the older prose in 5.1 and 12.1 says
  region 0). A `Lock` and win32k GDI raster (read-modify-write) land on that view. With `Cached` it is write-back
  (create_allocation.rs:3222-3248), so GDI reads run at memory speed. The KMD's paging content ops copy through a
  transient `MmMapIoSpace` of the blob with the host's `map_info` attribute (`build_paging_buffer.rs:728-778`).
  The KMD's own Present Blt writes into it with an image-to-buffer GPU copy and mirrors into the eviction copy
  (15.17 table, `PresentLinearBuffer` policy).
* **GPU-only (`TEXTURE`).** Written and read by GPUs only (the app's device renders into it, DWM samples it). The KMD
  never maps it; paging ops skip it (`BAR_DEVICE_OP_SKIPS`, `build_paging_buffer.rs:979-985, 1232-1239`).

The KMD's own Blt arm treats a pitched standard buffer as a registered Present buffer destination
(`begin_present_buffer_write_legacy`, `virtio/ctrl.rs:2285`), a standard buffer as a source as `0xE6` (15.17).

### 1.6 What nobody knows: the census

`StdType` and `GdiType` are registry values holding the LAST allocation's types (`create_allocation.rs:4045, 4096`),
and T1 recorded none of it. Which of {shadow, staging, GDI `STAGING_CPUVISIBLE`, `TEXTURE_CPUVISIBLE`,
`TEXTURE_CROSSADAPTER`, ...} DWM opens, at which sizes, and how many are alive is **unknown**. The cross-adapter
types are likely: `CrossAdaptCaps` exists so that legacy BLT-model swap chains get a redirection surface
(`query_adapter_info.rs:638-652`), and the T1 counters show the KMD attempting the foreign
copy of a 5120x1440 NVK buffer as a Blt source (`CpImpSt 16`, `PBRet 0xC000000D`, `docs/dwm-on-nvk.md` section 6,
branch); the destination was not recorded. That is a guess, not a measurement.

## 2. Facts that decide the design

### 2.1 What an NVK opener needs (read from the NVK patches and Mesa; branch)

* **The layout is CHECKED, not assumed.** NVK on Windows has no `VK_EXT_image_drm_format_modifier`
  (`has_tiled_bos` false); the opener builds the image from the D3D description and `nvk_helios_check_import_layout`
  (patch 0031) compares modifier, row stride and offset with the open's record; a mismatch is
  `VK_ERROR_INVALID_EXTERNAL_HANDLE`.
* **LINEAR stride.** NIL gives a LINEAR image, when nobody asks for a stride, `width_bytes.next_multiple_of(128)`
  (`mesa-nvk-s6`: `src/nouveau/nil/image.rs:241-274`; 256 on Kepler). With an explicit stride (the modifier path,
  `nvk_image.c:938-951`) 32-byte alignment is accepted. The KMD authors 256. They agree only when
  `row mod 256` is 0 or above 128, that is for every width that is a multiple of 64 pixels (704, 1024, 1920, 3840,
  5120) and for others by luck; they disagree at 800 (3328 / 3200), 1366, 32 and 96.
  `kmd_logic::rm_standard::nvk_default_stride_agrees` and its test pin this. Whether the Windows build can ask for
  an explicit stride is **unknown** (the extension is not advertised there).
* **The import is VRAM-only as written.** `nvkmd_rm_mem_import_resource` sets `mem->vram = true`, big pages, no
  GART flag, `NVKMD_MEM_LOCAL` (`nvkmd_rm_win.c:764` and the allocation below it); the Linux dma-buf import asks RM
  (`query_is_vram`, `nvkmd_rm_drm.c:250-265`, uses 4 KiB pages and `NVKMD_VA_GART` for system memory, 360-375) and the
  VA bind uses `CACHE_SNOOP` for it (`nvkmd_rm_va.c:361-369`). Both routes below hand NVK system memory.
* **The opener finds out it is foreign from the 128-byte trailer** (UMD, 1.3). A STANDARD allocation has 96. Either
  the UMD derives it from the identity bit and the meta (LINEAR, stride = `meta.pitch`, offset 0, fourcc from
  `dxgi_format`), or the KMD reports 128 for standard allocations. `read_standard_meta` already accepts any trailer of
  48 or more bytes (`kmd_logic/src/lib.rs:297-305`), so 128 does not trip `MetaLen`; the UMD's side of a size change
  is unread. The first is recommended (no change visible to the Venus UMD).

### 2.2 CPU speed, and the cache view

Measured by the host session and in 15.1: write-combined reads about 75 to 80 MB/s, writes about 28 MB/s; cached
reads about 26.8 to 28 GB/s. 1920x1080x4 (8.29 MB): 0.1 s to read write-combined, 0.3 ms cached; 5120x1440 (29.5 MB):
0.37 s against 1.1 ms. GDI is read-modify-write, so a write-combined view of a GDI surface is unusable (12.2).

15.5's alias problem belongs to the primary alone: dxgkrnl refuses `Cached` together with `Primary`, so its view stays
write-combined whatever memory sits behind it. Every other CPU-visible standard allocation gets `Cached`
(`vidmm_placement` 2085, applied 3242 when `AllocCached`), so dxgkrnl maps it write-back and **cached RM memory behind
it is no alias**. The one exception is the `AllocCached=0` kill switch: then dxgkrnl maps it write-combined, and
cached memory behind it IS an alias (`rm_sysmem::PrimaryCache::aliases`). `rm_standard::decide` refuses that case
(`Why::NotCached`) so the allocation stays on Venus.

### 2.3 The Venus DWM fallback must keep working

`feat/dwm-on-nvk` `docs/dwm-on-nvk.md` 4.1: a crash-loop guard (`DwmNvkMaxStarts` 2 in 600 s) sends the next DWM start
to Venus, and a failed `OpenResource` takes DWM down (4.2: dwmcore 0x8898008d). A Venus DWM opening an RM-backed
STANDARD surface sees kind STANDARD and plain Venus memory (`memory_type_index` 0, no flag it reads) and imports it as a
Venus resource; 15.8 already says that import is "unverified and the likeliest first failure". So an RM-backed
standard surface that exists when the guard falls back kills the fallback DWM too. The KMD cannot know which ICD a
DWM uses. Route D has no such hazard (the Venus memory is still what a Venus opener imports; the new bit is ignored).

### 2.4 `RM_DUP_OBJECT` and window pixels

R puts window contents in RM objects of the KMD's own client, at handles that are public constants
(`H_BASE + slot` = `0x4b4d2000 + n`, `rm_sysmem.rs:76`) in a client number that is sequential
(`shared-surfaces.md` 2, branch). `NvDupHarden` defaults to 2, log-only (`nvrm-escape.md` 12.3): another process's
`NV_ESC_RM_DUP_OBJECT` of one of them is judged `Deny`-grade, counted (`NvDupWould`) and **forwarded**. Venus blobs
have no such route. In mode 1 the dup is refused (the KMD's client is never in the caller's table). Not a problem for D.

### 2.5 The host

* `RmResourceImport` (msg 31) for a Venus blob: `venus/rm.rs:226-239` (`feat/rm-export-map-blob`): `ENOENT` for an
  unknown resource, `EINVAL` for a blob that is not a dma-buf (an `OPAQUE_FD`), modifier unknown (`None`), size from
  the dma-buf. The KMD's Venus client already exports present-buffer memory and GDI textures as DMA_BUF
  (`venus/commands.rs:996, 1025, 622-625`), so the precondition holds on paper. **Never run for a KMD present buffer.**
  Whether NVIDIA's host Vulkan driver's DMA_BUF export of HOST-VISIBLE memory imports into an RM client the way the
  device-local one did in spike X4 is **unknown**.
* RM system memory as a mappable foreign blob: `feat/rm-export-map-blob` (15.1); cached sysmem maps CACHED, 28 GB/s;
  never run in a guest.

## 3. The two candidates

| | R: RM-backed (level 5 service) | D: dma-buf (the Venus blob stays) |
|---|---|---|
| memory | `NV01_MEMORY_SYSTEM`, cached, made by the KMD's own RM client (`sysmem.rs:509-647`) | the present-buffer `VkDeviceMemory`, exported DMA_BUF as today |
| who holds the bytes | KMD_RM client (GEM on its DRI file) + the foreign resource | the Venus context (KMD's own) |
| foreign record | `creator KMD_RM`, then adopted (`creator None`); `sysmem` bit | a record with no `rm_handle` / `gem`, class "KMD-made Venus", adopted from birth |
| GDI / Present / paging | changes: Present arm, eviction policy, `sysmem_blt` | **unchanged** (it is the same Venus buffer) |
| Venus DWM fallback | **breaks** for these surfaces unless the Venus UMD refuses cleanly | unchanged |
| KMD lines (estimate) | about 600 to 800 in 10 to 12 files, tests about 600 | about 300 in 5 to 6 files, tests about 300 |
| new host behaviour | create / map RM sysmem (15.1, never run in a guest) | msg 31 on a host-visible Venus blob (never run) |
| removes Venus from the KMD | yes | **no** |
| GPU-only `TEXTURE` | VRAM RM alloc plus NIL layout in the KMD: large | the Venus client creates the image with an explicit modifier and the host reports it: medium, needs the host driver to create DRM-modifier images |
| shared with the other | NVK sysmem import, stride, UMD derives foreign from the identity, 128 or bit, census | the same |

## 4. S-B in detail: the RM-backed standard buffer (what R means exactly)

This section answers the request's question 2 for R; section 6 says what D is. Both are behind one new service-key knob
(call it `KmdRmStd`, REG_DWORD, read once per transport generation like `KmdRmSysCache`; 0 or absent = today, 1 =
route D, 2 = route R). Value 2 also needs `KmdRmClient >= 5` (the service); value 1 needs no RM client at all.

**Which allocations** (`rm_standard::decide`, tested): StdType 2 `SHADOWSURFACE`, 3 `STAGINGSURFACE`, and 4
`GDISURFACE` of GDI types 0, 2, 3, 4, 6, 7, 8; with a `D3DDDIFORMAT` of `A8R8G8B8` (21), `X8R8G8B8` (22) or
`A8B8G8R8` (32); width and height 1 to 16384; pitch x height rounded to a page at most 1 GiB (the foreign record's
bound) and the live total of foreign standard bytes inside a budget (default 1 GiB, the BAR segment's size); and
`AllocCached` on. Refused to Venus, counted with a `Why` code: knob off (1), primary (2, it has its own arm), `TEXTURE`
(3), unsupported type (4), `EXISTINGSYSMEM` (5, dxgkrnl supplies the pages), format (6: the 8 and 16 bit and palette
surfaces, until the record can name them; `dwm-on-nvk.md` 4.2.2), extent (7), size (8), write-combined view (9),
budget (10). The order is the table's order and a test pins it.

**Memory type.** RM SYSTEM memory, CACHED (`ATTR_CACHED` 0x3a000000, RM writes back 0x2a800000; the trial map must see
`map_info` CACHED, `rs::host_cache_ok`). Not the primary's write-combined default (2.2). Not VRAM: GDI reads these
surfaces, and a CPU view of VRAM is the write-combined RM window (12.2; NVK reports no host-visible VRAM). The KMD makes it exactly as level 5 makes the
primary (15.4, stages 3 to 11) with three differences: the kind in `route` (today `LinearPrimary` only,
`rm_sysmem.rs:153-164`), a layout function that accepts extents from 1 (the flip floor of 64 is the primary's,
`rm_sysmem.rs:211`) and the pitch the policy authors, and the cache choice.

**CPU access.**

* `Lock` and GDI: dxgkrnl's write-back aperture view (the `Cached` flag), served by `RESOURCE_MAP_BLOB` of the
  RM-export resource at the offset dxgkrnl chose, exactly as for the primary (15.6; `blob_map_begin` lifts its
  refusal for a record with the `sysmem` bit, `resource_tables.rs:294`, and `blob_remap_begin` at 420).
* The KMD's own copies (paging, Present Blt): through `MmMapIoSpace` with the host's `map_info` (`MmCached`), so they run
  at the cached 26.8 GB/s. 15.17's CPU copy applies automatically: `sysmem_blt::primary` is "any adopted RM sysmem
  record at level 5" (`sysmem_blt.rs:105-117`). Its cost model (28 MB/s) was for the write-combined primary; here the
  destination bytes cost about a millisecond a frame, and the GPU whole-source copy, its fence wait and the read of the
  Venus staging image (its mapping's cache attribute is the host's, unread) dominate.
* The display engine cannot scan out system memory (section 15, introduction): irrelevant, these are never scanned out.

**Layout record, meta and identity.** Record: plane 0, offset 0, `MOD_LINEAR`, stride = the authored pitch, fourcc
`ARGB8888` / `XRGB8888` / `ABGR8888` by format, extent as asked (`StdLayout::foreign_layout`). Meta: width, height,
D3DDDIFORMAT, pitch, bind 0x28, `misc_flags` as now, `dxgi_format` 87 / 88 / **28** (the legacy zero hint for
`A8B8G8R8` is dropped for these so an opener can derive a fourcc without the trailer). Identity: STANDARD,
`blob_size = venus_alloc_size` = the adopted size, bit 1 `FOREIGN_SYSMEM` (renamed "foreign sysmem standard"; the
accessor is still called `foreign_sysmem_primary`), bit 0 **clear** (these are no longer Venus Present buffers: no
`register_present_buffer`, no `authorize_present_buffer_open`, `dedicated_present_buffer` false, no teardown wait),
`memory_type_index` 0 (not consulted). The creator's trailer room is promised as for the primary (`trailer_room: true`,
`sysmem.rs:617`); the open path writes it only where there is room (`FgOpNoRm`).

**Lifetime and ownership.** The KMD_RM client owns the RM memory (one `NV01_MEMORY_SYSTEM` per allocation, handle
`H_BASE + slot`), the service's DRM file owns the GEM, the foreign record is `creator KMD_RM` then adopted; the WDDM
allocation owns the resid; opens are counted per process (S6); a destroyed allocation with opens alive defers the release
to the last close (`shared-foreign-surfaces.md` 3). Free order, unchanged from 15.10: `retire_scanout_allocation`, host
unref (`release_allocation_resource`), `target_gone` (a no-op for a never-flipped buffer), GEM close, `RM_FREE`
(`sysmem.rs:953-1004`). The `close_wait` of 15.7 never holds a never-flipped GEM. An NVK opener has its OWN RM
object by then (`RM_RESOURCE_IMPORT` -> GEM on its DRI file -> NVKMS export -> 0x3d06); `shared-foreign-surfaces.md` 6.1
says it "lives independently of the GEM handle, of the resource and of A", measured only for `RM_DUP_OBJECT`
(`shared-surfaces.md` 2, branch): for the 0x3d06 import path it is **unmeasured** (14.2 item 4 doubted it).

**Quota and tables.** The limits that matter are (a) `Svc::TABLE_CAP` = 32 live allocations
(`rm_sysmem.rs:431`; "table full" gives Venus, no strike): must grow to the order of 512; (b) the foreign table's
`MAX_FOREIGN_TOTAL` = 512, which counts every ADOPTED record, DWM-on-NVK's swap chains and every NVK app's shared
surface included (`foreign_resource.rs:81`): must grow with (a); (c) the KMD owner's per-owner quota (64, 4 GiB) only
counts a record between import and adoption, so it bounds creations in flight; (d) the KMD owner's 128 backend
handles (3 for the service, plus one transient export file per creation in flight, 15.3): bound the concurrency;
(e) no byte budget exists for adopted memory (`shared-foreign-surfaces.md` 10, "the quota hole"): `Policy.budget_bytes`.

**Paging and eviction.** `bar_eligible` is true (`HostAuthoritative`, 3068), so VidMm places it in the BAR segment
(1 GiB, `bar_segment.rs`) and may evict it. The eviction copy (blob to VidMm's system pages) is useless for RM memory
that never goes away, and the page-in is dangerous: the KMD's Blt CPU copy writes the blob while VidMm believes the
system copy is current, and the page-in then writes the older system copy over it (the Venus buffers avoid this with
the retained mirror, `SystemBackingPolicy::PresentLinearBuffer`, `build_paging_buffer.rs:1061-1195`). The primary
lives with the same hazard (it is `AccessedPhysically`; whether VidMm ever evicts it is unread). The design: **the RM memory is authoritative**,
so skip both directions with machinery that exists: a skipped eviction (`note_skipped_eviction`, the answer is still
`STATUS_SUCCESS`, `paging_failure()` at 172) marks the system copy invalid and the matching page-in is skipped
(`page_in_decision`, `kmd_logic/src/paging.rs:384`). No data moves, the system pages VidMm reserved stay unused (a
cost of up to the surface size in guest RAM per evicted surface, only under VidMm pressure). A new policy value or flag
on the allocation context selects it. The mark set is bounded (`InvalidSet`): its overflow counter must stay 0.

**Present Blt, flips.** Never flipped. A Blt Present with an RM standard destination is 15.17's arm (above), with
`PBCpy` 3. A windowed-blit snapshot source is skipped as for the primary (`Skip::Snapshot`). `Edge::PresentBlt` is
raised and costs `RmSysEdOther` (the allocation is not the shown one). `sysmem_flip::program` and `foreign_flip` must
not see these records as primaries: today the `sysmem` bit means "the primary", and `program` answers `BadLayout` for a
sysmem record whose extent is not the mode's (`sysmem_flip.rs:240-243`); the bit becomes a class.

**Memory cost.** Host pinned system memory of RM, one object per allocation (the VidMm charge is the same number);
8.29 MB for a 1080p surface, 33 MB at 4K. The Venus buffer it replaces is host-visible Vulkan memory of the same size
(plus the buffer, command pool and fence it needed). Guest RAM: none, except the eviction pages above.

**Creation cost.** About ten control-queue messages on dxgkrnl's thread (alloc, open export file, export, GEM import,
close, create-and-attach, map, unmap), a few milliseconds each, bounded by 6 s plus 3 s of undo (15.3). The Venus path
it replaces is about a dozen ring commands, an initial submit with a fence wait and one blob create
(`venus/commands.rs:300-499`): comparable. The trial map can be skipped after the first success of a generation
(saves two messages per surface; the first real map checks the attribute anyway).

## 5. What is missing in the KMD, and how much it touches

For R (request question 3), against the primary's machinery:

| missing | where | est. lines |
|---|---|---|
| a kind and a decision: `route` for `StandardBuffer`, `Why` codes, the layout function, the budget | `kmd_logic/src/rm_sysmem.rs` (`Kind`, `route`, `layout`), `rm_standard.rs` (new, done) | 100 + tests |
| the service takes `(kind, extent, format, cache)` instead of `(width, height, dxgi)`; `try_create_primary` keeps its name for the primary | `kmd_render/src/virtio/rm_client/sysmem.rs` (`try_create_primary` 273, `create_primary` 306, `build_steps` 509) | 120 |
| table 32 -> 512 and a live-bytes total | `rm_sysmem.rs` `TABLE_CAP`, `Svc` | 40 + tests |
| the arm: `KmdStandardBuffer { primary: false }` asks the service first, Venus on any refusal (the primary's arm is the template, 2565-2593) | `create_allocation.rs` `build_backing` | 60 |
| pitch, format and identity authored with the decision (the knob read at `GetStandardAllocationDriverData`, the decision made once there and recorded in `misc_flags`, so `Pitch` and the creation agree and a failed RM creation falls back to Venus at the SAME pitch) | `create_allocation.rs` 4085-4213, `write_open_identity` 1911 | 50 |
| `Cached` flag applies (already), `AllocCached` refusal, no dedicated-buffer flag | `create_allocation.rs` 3239-3248, 2714 | 10 |
| eviction authoritative policy | `build_paging_buffer.rs` (two arms), `SystemBackingPolicy` | 50 |
| `sysmem` bit -> class (primary / standard); `program`, `foreign_flip::decide`, `sysmem_blt` read the class | `foreign_resource.rs` (`Entry::sysmem`, `mark_sysmem`, `sysmem_source`), `foreign_tables.rs`, `sysmem_flip.rs`, `sysmem_blt.rs` | 80 + tests |
| user-mode adoption of a KMD-made record refused (the R5 gap: `foreign_resource.rs` adopt, `foreign_tables.rs:225-230`; `shared-foreign-surfaces.md` 5 R5). Today a pending KMD record lives for the trial map (tens of ms) once per run; with hundreds of creations it is a real window | `foreign_resource.rs`, `AdoptRequest` gains a `kmd_internal` field | 30 + tests |
| foreign table caps | `foreign_resource.rs:81` | 5 |
| counters (`RmStd*`), the knob, the census | `diag.rs`, `sysmem.rs` | 80 |
| the UMD / NVK / DXVK half (not this tree, not mine): derive foreign from the identity, sysmem import, stride | `umd/`, Mesa | n/a |

"How the KMD creates the foreign record itself" is already written: `import_resource` (`sysmem.rs:810-882`) is the
reservation / `alloc_blob_errno_within` / commit sequence with creator `KMD_RM` and the KMD's own Venus context,
then `adopt_for_allocation` inside the same creation (`sysmem.rs:604-640`). The shared-open rule for another process
needs nothing new: the cap `HELIOS_FOREIGN_CAP_SHARED_OPEN` is set by every KMD, the open is counted per process by
`foreign_open` and `RM_RESOURCE_IMPORT` is authorized by "the caller's process holds an open row"
(`rm_resource_import::authorize`, `kmd_logic/src/rm_resource_import.rs:188-216`). Adoption: done by the KMD, never by
a user process (the one new rule above).

## 6. D in detail: the dma-buf route

What it needs (request question 4).

**Guest, KMD (about 300 lines).** After `allocate_present_buffer_blob` succeeds and `decide` says yes: insert a foreign
record of a new class "KMD-made Venus" (resid, the KMD's Venus context, size = the Venus allocation size, layout from
`StdLayout`, adopted from birth, no DRM file or GEM); give it the open counting that already exists. Three places read
"foreign" as "no CPU view / has a DRM file": `blob_map_begin` and `blob_remap_begin` (`resource_tables.rs:294, 420`: the
record must be CPU-mappable, or dxgkrnl's map of every GDI surface breaks) and `foreign_flip::decide` (a record with no
GEM must be refused, a new row); `sysmem_source` is unaffected (the class never carries the `sysmem` bit). The identity
gets bit 2, "importable by RM" (a new word beside bits 0 and 1), set from the record; `write_open_identity` today turns
`ident.foreign` into bit 1 for STANDARD (`create_allocation.rs:1911-1921`), so it must pick the bit by the record's class;
`RM_RESOURCE_IMPORT` needs no change (`authorize` wants a record and an open row; both exist). Destroy needs none:
`foreign_allocation_destroyed` -> `release_allocation_resource` is the generic Venus teardown.

**Host.** Msg 31 for a Venus blob exists (2.5). Needed: confirmation that it works for host-visible Venus memory; the
modifier is unknown to the host, which is fine for a LINEAR buffer (the NVK check is skipped when the host reply has
no modifier, `nvkmd_rm_win.c` `check_modifier && (flags & 1)`; the KMD's record is the authority). For the GPU-only
`TEXTURE` the Venus client would create the image with an explicit NVIDIA block-linear modifier
(`MOD_NVIDIA_BLOCK_LINEAR_BASE | h`), which the host driver must support for creation, and the host must report it.

**Why it is or is not smaller.** For the CPU-visible buffers: smaller by about 500 lines and, more important, by every
behaviour change (paging policy, Present arm, caps, budget, the Venus DWM fallback, the DUP exposure), because nothing
that GDI, Present and VidMm do changes. It is not smaller on the NVK, DXVK and UMD side (2.1 applies to both). It
does not remove Venus from the KMD, so it is a bridge, and for `TEXTURE` it needs more host work than R needs KMD work.
It is also not a throwaway: the record class, the identity bit, `rm_standard` and the whole opener side are shared.

## 7. Interaction with ForeignFlip, the Blt CPU fallback and NvDupHarden

* **ForeignFlip (15.18).** Standard buffers are never flipped. R: the `sysmem` bit must become a class, or
  `foreign_flip::decide` row 3 hands a standard record to the level 5 arm, whose extent check then answers
  `BadLayout` (a refused VidPn programming) instead of `NotOurs`. D: a record without a GEM must be refused in
  `decide` by a new row, and the order test `refusal_precedence_is_the_tables_order` extended. The `Ff*` poison
  hooks match records by the DRM file a device closes; the files the service closes are its transient export files,
  not the records' long-lived DRM file, as for the level 5 primary.
* **Blt CPU fallback (15.17).** R: applies, as above, with a faster copy. Its open item 3 (the snapshot source) and
  the `Skip` reasons are unchanged. D: not involved (the legacy arm still writes the Venus buffer). Both: a BLT-model
  producer on NVK needs the KMD's foreign copy (`ForeignCopy`, off in T1: `FcOff`, `PBRet 0xC000000D`) to read an NVK
  source, or the NVK UMD to blit into the opened destination itself before presenting; **unknown which the T1
  workload was**.
* **NvDupHarden.** D: no KMD RM objects, the opener's RM calls name its own client and file (`Allow`). R: 2.4. Set
  `NvDupHarden=1` before any R run (`nvrm-escape.md` 12.3 recipe). `RM_RESOURCE_IMPORT` is outside `FORWARD`
  (message 31 is not forwardable) and unchanged. The shared-surface rules R1 to R4 hold: no RM handle crosses; the
  resid is the only currency. R5 is the new adoption rule above.

## 8. Stages, tests, counters

All counters are REG_DWORD values of the service key, at most 14 characters, written throttled.

### S-A0: the census (no behaviour change)

* Read the T1 UMD log: `grep "DDI OpenResource allocation" umd-<dwm pid>.log` and keep `kind=2`; decode `misc`
  (1.1), group by (std, gdi, d3dfmt, width x height), count live ones over a desktop session. Free, today.
* KMD: in `GetStandardAllocationDriverData` phase 2, count with `rm_standard::hist_slot(std, gdi)` into 14 atomics named
  by `rm_standard::hist_name` (`StdNPrimary`, `StdNShadow`, `StdNStaging`, `StdNGdi0` .. `StdNGdiTexCXa`, `StdNGdiOther`,
  `StdNOther`), plus `StdBytesMiB` (cumulative) and `StdMaxMiB` (largest). About 40 lines. Pass: the numbers answer
  1.6; and which types DWM opens (`FgOpen` does not count them today: add one `StdOpenN` total at
  `dxgkddi_open_allocation`, and log the slot).

### S-A: make the existing Venus standard buffers importable (route D)

* Scope: section 6. Knob `KmdRmStd` = 1 (R, S-B, adds the value 2).
* Pass: `StdRec` (records made) equals the CPU-visible creations, `FgOpen` grows with DWM's opens of them, `FgRiOk` grows (DWM's NVK imports), `FgRiErr` 0, `FgRiRef` 0, `FgOpRf` 0; the host log shows msg 31 for
  those resids and no `EINVAL`; the NVK log (`NVK_DEBUG=vm`) prints "Helios shared surface N imported as RM memory";
  a GDI window (notepad) is NOT black in an NVK DWM; the same box with a Venus DWM (`DwmIcd=venus`) composes the
  same windows unchanged (the regression gate).
* Fail fast: the first msg 31 for a present buffer. `EINVAL` = the host driver exports it opaque; stop, go to S-B.
* Rollback: knob 0, restart the adapter; the records are never made.

### S-B: RM-backed (route R)

* Preconditions: 15.13 steps 2 to 5 (level 5 shows the primary), `NvDupHarden=1` with a clean `NvDupWould`, the Venus
  UMD refuses bit 1 with a placeholder instead of `E_FAIL`, the NVK sysmem import.
* Scope: section 4 and 5. Knob `KmdRmStd` = 2.
* Counters: `RmStdTry`, `RmStdOk`, `RmStdVenus`, `RmStdWhy` (the last `rm_standard::Why` or the service's), `RmStdLive`,
  `RmStdMiB` (live), `RmStdMsMax`, `RmStdFull` (table or budget refusals), `RmStdEvSk` (evictions skipped), `RmStdBlt`.
* Pass: `RmStdOk` grows, `RmStdVenus` small and explained by `RmStdWhy` (format 6 only for 8 and 16 bit surfaces);
  `RmSysLeak` 0, `RmSysSoft` 0; `FgMapRf` 0; a window drag with GDI apps: `RmStdMsMax` below a few hundred ms;
  `RmStdFull` 0 in a normal session; a 30-minute soak; a mode change and a DWM restart leave `RmStdLive` equal to
  the live surfaces; paging pressure (start many windows) leaves `PgInvOvf` 0.
* Fail fast: `RmSysCache` is not 1 for these (`Why::Cache`), or the trial map fails.
* Rollback: knob 0; the allocations of the generation are Venus again.

### S-C: GPU-only `GDISURFACE_TEXTURE`

Only if the census shows DWM opening it in volume. Either D (the Venus client creates the image with an explicit
modifier; the host reports it) or R with VRAM (the KMD would have to produce NIL's layout; blind). Not designed
further here.

## 9. Risks

1. The level 5 service has never run on hardware (15 status; no T1-style result for it exists in this tree). S-B
   inherits every 15.13 unknown; D does not.
2. R breaks the Venus DWM fallback (2.3). D does not.
3. R's DUP exposure of window pixels at `NvDupHarden` 2 (2.4).
4. The stride mismatch (2.1): a black window per odd width with no error, or `VK_ERROR_INVALID_EXTERNAL_HANDLE`
   per window, depending on who checks.
5. Table and quota caps (4): 32 slots and 512 foreign records are the first walls a busy desktop hits.
6. The present-buffer ownership protocol does not exist for an NVK consumer: a KMD Present Blt can write a surface
   DWM is sampling (tearing inside one surface, not a memory hazard). R drops the protocol entirely.
7. Creation latency on dxgkrnl's thread when many windows open together (15.3 bounds one creation, not a burst).
8. `GetStandardAllocationDriverData` is called in two phases; whether the union data is valid in the size-query phase
   is not read anywhere in this tree (the code reads it only in phase 2). The recommended derive-from-identity route
   needs no size change; the 128-byte route needs the answer.
9. `MAX_FOREIGN_OPEN_ROWS` is 2048 (`foreign_resource.rs:92`) and counts (resource, process) pairs; DWM is one process,
   so a few hundred windows are far inside it.

## 10. Recommendation

Do S-A0 now. Then S-A by route D, because it is the smallest change that stops DWM on NVK composing black windows for
KMD-made surfaces, it carries no allocation, paging, Present or fallback risk, it runs on machinery that has run (Venus
blobs, GDI mapping, `RM_RESOURCE_IMPORT` between NVK processes), and its opener half is the half R needs too. Then S-B
(R) as the way Venus leaves the KMD, once level 5 is proven on hardware and the gates in S-B hold. Do S-C only on census
evidence. If the single test of S-A (msg 31 on a KMD present buffer) fails, go straight to S-B: nothing in S-A is
needed by it except the shared opener work.

## 11. Open questions for the host session

1. Has level 5 (`KmdRmClient=5`) run on win11? Any `RmSysOk`, `RmSysTrial`, `RmSysCache` or the 15.13 steps? The doc
   still says never run.
2. What did T1 (and T2) show for KMD-made standard allocations: the `kind=2` lines of the UMD log (`misc`, size, format),
   and how many are alive in a normal session? (S-A0 asks for exactly this.)
3. Does msg 31 work for a KMD present buffer (host-visible Venus memory exported DMA_BUF)? One manual call settles
   route D.
4. Stride: may the KMD author 128-aligned pitches for these standard surfaces, or must NVK / DXVK pass an explicit
   stride? (The 256 rule is D3D12's cross-adapter requirement; `CrossAdaptCaps` is declared for IddCx and BLT-model
   redirection.)
5. Can NVK on Windows create a LINEAR image with an explicit row pitch (`VK_EXT_image_drm_format_modifier` is not
   advertised there)? Who changes `nvkmd_rm_mem_import_resource` for system memory?
6. Does the UMD derive "foreign" from the STANDARD identity bit (no KMD change), or should the KMD report 128 bytes?
   Is the size-query phase of `GetStandardAllocationDriverData` given the union data?
7. For BLT-model producers on NVK: is the KMD's foreign copy (`ForeignCopy`) going to be on, or does the NVK UMD blit
   into the opened redirection surface before presenting?
8. Who accepts that R needs `NvDupHarden=1`, and is a Venus UMD that refuses an RM-backed standard open with a
   placeholder acceptable in the same change?
9. After a KMD `RM_FREE`, does an NVK client's 0x3d06-imported object of the same memory survive (refcount)? Measured
   only for `RM_DUP_OBJECT`.
10. Is a 1 GiB budget for foreign standard bytes (the BAR segment's size) acceptable, and what should the table caps
    be?

## 12. Verified here, and not

Verified on the host: `cargo test` in a scratch copy of `guest/windows/kmd_logic`: 888 tests pass, 14 of them
`rm_standard`'s: the class of every (standard type, GDI type) pair against what `GetStandardAllocationDriverData` and
`protocol::classify` do; the three formats and their fourccs; the pitch against `cross_adapter_pitch` at twelve widths;
the 128 / 256 disagreement at every width 1 to 2048; the order of the refusals; the extent, size and budget bounds
(including exactly 1 GiB, and no overflow); the layout record validating for exactly its size; the census slots and
names (distinct, at most 14 bytes). Nothing else was built or run; `kmd_render` and `protocol` are untouched (the
protocol crate was not re-tested). Every line number above was read in this tree on `8aba6f4`; branch citations were
read through `git show` and not built.

**Not verified by anything:** every statement about the host's behaviour (msg 31 on a host-visible Venus blob, RM
sysmem create and map), NVK's behaviour (stride, import), dxgkrnl's two-phase standard allocation protocol, the UMD
changes, the type census, the eviction interplay under VidMm pressure, the cost of a burst of creations.
