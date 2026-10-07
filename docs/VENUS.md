# Venus (Windows guests)

How a Windows guest renders on the host GPU: Vulkan calls serialized with the
Venus protocol, carried over Conduit's device, executed by the host's NVIDIA
Vulkan driver. D3D9-11 reach Vulkan through DXVK, D3D12 through vkd3d-proton,
in the guest. The guest driver stack comes from Helios
([guest/windows/HELIOS.md](../guest/windows/HELIOS.md)).

```text
 Windows guest                                  Host
 ─────────────────────────────────────────      ─────────────────────────────────
 game (D3D11 / D3D12 / Vulkan)
 UMD (DXVK / vkd3d-proton) → Mesa Venus ICD
   │ D3DKMTEscape (Helios escape ABI, unchanged)
 conduit KMD  (guest/windows/kmd_render)
   │ GpuCmd on the control queue ────────────►  conduit-backend  (--venus)
   │ region 3: host-visible blobs  ◄────────────   │ checks, ids, region 3 placement
                                                   ▼
                                                conduit-venus  (separate sandboxed process)
                                                   virglrenderer Venus → NVIDIA Vulkan
                                                   │ scanout image as dma-buf
                                                   ▼
                                                conduit-viewer / conduit-stream (unchanged)
```

Nothing here changes Linux guests. The backend serves Venus only with
`--venus`; without it the config bit is clear and `GpuCmd` is refused. The
flag exists only in a backend built with the device crate's `venus` feature:
`packaging/build.sh` (so every package) and `make backend` build the backend
with it (by hand: `cargo build --release -p device --features
vhost-user,venus --bin conduit-backend` in `host/backend`).

Setting up a Windows VM, installing the guest driver and tuning it:
[WINDOWS.md](WINDOWS.md).

## Device

| | |
|---|---|
| Config `features` bit | `NVGPU_CFG_VENUS = 1 << 10`. Set only with `--venus`. A guest sends `GpuCmd` only when set. |
| Config `features` bit | `NVGPU_CFG_RM_RESOURCE_IMPORT = 1 << 14`, set with `NVGPU_CFG_RM_IMPORT`: `RmResourceImport` (MsgType 31) is served (below). |
| Config `features` bit | `NVGPU_CFG_RM_IMPORT = 1 << 13`, only with `NVGPU_CFG_VENUS`: RM-export blobs are served (below). Set when the renderer imports dma-bufs (`conduit-venus` from this release on). Bit 12 is not used in the config word. |
| Config `features` bit | `NVGPU_CFG_GUEST_BLOB = 1 << 16`, only with `NVGPU_CFG_VENUS`: guest-memory blobs are served (below). Set with `--venus-guest-blobs` when the renderer imports host memory. Bit 15 is not used in the config word. |
| Shared memory region 3 | `SHM_ID_VENUS`, host-visible Venus blobs, `--venus-hostmem-mib` (default 8192, power of two). Advertised only with `--venus`. Offsets inside it are chosen by the guest, as with virtio-gpu's host-visible region. QEMU only (conduit-vmm has fixed BARs). |
| Queues | unchanged: 0 control, 1 event. No cursor queue. |

## `GpuCmd` (MsgType 30)

Guest → host, control queue.

```text
request:  MsgHeader{msg_type=30, handle=0, status=0, padding=0} | virtio-gpu command
response: MsgHeader{msg_type=30, status}                         | virtio-gpu response
```

The embedded command is a virtio-gpu control command exactly as the VIRTIO 1.3
spec (§5.7.6) lays it out: `virtio_gpu_ctrl_hdr` (24 bytes) and its body.
`MsgHeader.status` is a transport result (0, or a negative errno when the
message itself is malformed); the virtio-gpu result is the response's
`ctrl_hdr.type` (`RESP_OK_*` / `RESP_ERR_*`).

Limits: a request is at most 4 MiB (Venus command streams are batched by the
guest, a larger submit is split); a response fits the existing 64 KiB.
The response may be split across several device-writable descriptors of the
chain; the host fills them in order.

**Fences.** A command with `VIRTIO_GPU_FLAG_FENCE` set that succeeds with
`RESP_OK_NODATA` completes (its chain is returned to the used ring) only after
the renderer signals that fence, as QEMU does; a fenced command that fails, or
answers with data (`GET_CAPSET`, `RESOURCE_MAP_BLOB`), is answered at once. With `VIRTIO_GPU_FLAG_INFO_RING_IDX` the fence is on `(ctx_id, ring_idx)`,
otherwise on the context's ring 0. Completions may therefore be out of order.
Unfenced commands complete immediately.

Fence rules, which follow virglrenderer:

- Fence ids are the guest's, passed through unchanged, and need not increase.
- On one `(ctx_id, ring_idx)` the renderer retires fences in submission order
  and reports each one. A signal completes that ring's held commands in
  submission order up to and including the first with the signalled id; ids
  are never compared.
- `ring_idx` must be below 64 (virglrenderer's proxy has 64 timelines). A
  fenced command naming ring 64 or above is refused with
  `RESP_ERR_INVALID_PARAMETER` before it runs.
- A fence on a ring other than 0 with no queue bound to it makes virglrenderer
  destroy the whole context, which the backend cannot detect. Bind a queue to
  a ring before fencing on it.

**Fence latency.** A Windows frame waits on several ring fences, so their
latency sets the frame rate. virglrenderer creates each queue's sync fences
exportable as `SYNC_FD` when the driver offers it, and NVIDIA's driver waits
on such a fence in steps of about 10 ms (measured: 10.08 ms per fence,
0.03 ms for a plain one). The fences are only waited on by virglrenderer's
own sync thread, never exported, so Conduit's patch
`host/venus/patches/0001-vkr-queue-plain-sync-fences.patch` makes them plain;
`build-virglrenderer.sh` applies `host/venus/patches/*.patch` to the pinned
submodule in order (a patch already applied is skipped). Unigine Heaven in
a Windows guest went from 36 to about 150 fps with it.

Both sides log fence latency in 2 s windows while fences flow:
`conduit-venus` the time from `create_fence` to virglrenderer's signal
(count, p50, p90, max, in `logs/venus.log`), the backend the time from
holding a fenced command to the signal reaching it, per `(ctx_id, ring)`,
at `info` (`venus: fence latency ctx C ring R: N fences in S s, ...`). A
window opens with its first fence and closes 2 s later, on time: the
backend's fence pump asks for completions at least every 100 ms, and
`conduit-venus`'s serve loop waits no longer than the open window has left
(`Renderer::tick`). `S` is the window's span from its first fence to its
last, so a burst that stopped early shows as such; a fence after an idle
spell opens a new window. An idle guest logs nothing
(`host/venus/src/latency.rs`, shared by both).

**Commands served** (the set Helios's KMD sends). Anything else answers
`RESP_ERR_UNSPEC`.

| Command | Host action |
|---|---|
| `GET_DISPLAY_INFO` | one scanout, from the config display size; enabled iff a display is configured |
| `GET_EDID` | scanout 0's EDID for the configured `--display WxH@HZ` (below): `RESP_OK_EDID` with `size` 256 and the rest of the 1024-byte `edid` zero; another scanout `RESP_ERR_INVALID_SCANOUT_ID`; no display configured `RESP_ERR_UNSPEC`. Served without `VIRTIO_GPU_F_EDID`: the guest just sends it and uses its own EDID on any error |
| `GET_CAPSET_INFO` | index 0 → `VIRTIO_GPU_CAPSET_VENUS` (4); other indices `RESP_ERR_INVALID_PARAMETER` |
| `GET_CAPSET` | Venus capset from the renderer |
| `CTX_CREATE` | context with `context_init` capset Venus only |
| `CTX_DESTROY` | destroys the context and detaches its resources |
| `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` | renderer attach/detach |
| `SUBMIT_3D` | Venus command stream to the context |
| `RESOURCE_CREATE_BLOB` | `blob_mem = HOST3D`: renderer allocates, returns an fd and map info. `blob_mem = BLOB_MEM_RM_EXPORT` (`0x80000001`): an RM-export blob (below). `blob_mem = GUEST`: a guest-memory blob (below), only with `NVGPU_CFG_GUEST_BLOB`; refused otherwise |
| `RESOURCE_MAP_BLOB` | the blob's fd placed in region 3 at the guest's offset; reply `RESP_OK_MAP_INFO` with the cache type |
| `RESOURCE_UNMAP_BLOB` | withdrawn from region 3 |
| `RESOURCE_UNREF` | unmapped if mapped, then freed |
| `SET_SCANOUT_BLOB` | records scanout 0's resource, size, format, stride, offset, and its modifier: an RM-export blob's own, else the one its blob's size implies (below); resource 0 turns the scanout off (`ScanoutDisable` to the viewer); a format with no DRM fourcc is `RESP_ERR_INVALID_PARAMETER`, a scanout other than 0 `RESP_ERR_INVALID_SCANOUT_ID` |
| `RESOURCE_FLUSH` | renderer exports the scanout resource as a dma-buf with the guest's layout (cached per resource and layout), sent to the viewer as a frame; an RM-export blob is sent as the dma-buf the backend already holds |

**EDID.** `GET_EDID` (`0x010a`: header, `scanout_id`, padding) is answered
with `RESP_OK_EDID` (`0x1104`): header, `size` = 256, padding, then
`edid[1024]`, zero after the first 256 bytes; 16 + 1056 bytes with the
`MsgHeader`. The 256 bytes (`host/backend/device/src/venus/edid.rs`) are:

- an EDID 1.4 base block: manufacturer `CDT`, product 1, made 2026, digital
  8 bpc, size unknown, sRGB, the monitor name `Conduit` (`0x0A`, then
  spaces), a Display Range Limits descriptor (below), one dummy
  descriptor, one extension. Its first detailed timing
  is the configured mode if a detailed timing can hold it (each side at most
  4095, clock at most 655.35 MHz; a vertical front porch over 63 lines gives
  the rest to the back porch), and the preferred-is-native feature bit is
  then set. Otherwise it is a stand-in: the size halved until it fits,
  keeping the aspect ratio, then the refresh lowered a hertz at a time until
  the clock fits (5120×1440@240 → 2560×720@240, 3840×2160@144 →
  3840×2160@74, 7680×4320@60 → 3840×2160@60);
- the range limits (`0xFD`, "range limits only", no GTF/CVT formula): 24 Hz
  up to the highest refresh, the lowest to the highest line rate in kHz and
  the highest pixel clock (10 MHz units, at most 2550 MHz) of the base
  timing and the configured mode, with the EDID 1.4 "+255" offsets for
  rates above 255 (5120×1440@240: 24–240 Hz, 194–389 kHz, 2030 MHz).
  Windows checks modes against them when it treats the monitor as
  continuous-frequency (as it did while the KMD reported an analog
  connector; it reports DisplayPort by default now, `OutputTech` in
  [WINDOWS.md](WINDOWS.md#registry-knobs), and analog only with
  `OutputTech=0`), and without them kept it at 60 Hz;
- a DisplayID 2.0 extension (tag `0x70`, version `0x20`, primary use
  "generic display") with Product Identification (`0x20`, no OUI), Display
  Parameters (`0x21`, native size = the configured one), one Type VII
  detailed timing (`0x22`, revision 0) with the configured mode, marked
  preferred, and Display Interface Features (`0x26`, RGB 8 bpc, sRGB).

The Type VII descriptor is 20 bytes, little-endian, every field stored as
its value minus one (Linux `drm_mode_displayid_detailed`, edid-decode
`parse_displayid_type_1_7_timing`): 0–2 pixel clock in kHz; 3 options (bit 7
preferred, 6:5 stereo, 4 interlaced, 3:0 aspect: 0 1:1, 1 5:4, 2 4:3, 3 15:9,
4 16:9, 5 16:10, 6 64:27, 7 256:135, 8 other); 4–5 H active; 6–7 H blank;
8–9 H front porch (bit 15: H sync positive); 10–11 H sync width; 12–13 V
active; 14–15 V blank; 16–17 V front porch (bit 15: V sync positive); 18–19
V sync width.

Every timing is CVT reduced blanking v2: H blank 80 (front porch 8, sync 32,
back porch 40, sync positive); V sync 8, back porch 6 (sync negative), and
`V blank = max(floor(460 µs / ((1/HZ − 460 µs) / H)) + 1, 15)` lines, so the
blanking is at least 460 µs; clock = `(W + 80) × (H + V blank) × HZ`,
rounded down to a kHz (the refresh comes out a hair under `HZ`).
5120×1440@240 is 5200 × 1619 at 2020.512 MHz. With no refresh known, 60.
A sample and `edid-decode --check`'s reading of it (PASS; the one warning is
the zero OUI) are in `host/backend/device/src/venus/testdata/`. (edid-decode
releases from 2023 print the Display Parameters' 8 bpc as 10 bpc; later
ones read it correctly.)

**Scanout layout.** The dma-buf the viewer gets is described entirely by the
guest's `SET_SCANOUT_BLOB`: `width`, `height`, `strides[0]`, `offsets[0]`, and
the format as the DRM fourcc of the same memory layout (`B8G8R8A8` →
`ARGB8888`, `B8G8R8X8` → `XRGB8888`, `R8G8B8A8` → `ABGR8888`, `R8G8B8X8` →
`XBGR8888`, and the other four `VIRTIO_GPU_FORMAT_*`). A Venus blob carries
no image layout the host can read back, so the backend infers the modifier
from the blob's size (`host/backend/device/src/venus/scanout.rs`):

- A blob with room for `offset + stride × roundup(height, 8 × 2^h)` bytes,
  where `height` is not already a whole number of blocks and `stride` is a
  multiple of 64, is NVIDIA block-linear: `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0,
  s=1, g=2, k=0x06, h)`. That is how the NVIDIA driver lays out a
  `VK_IMAGE_TILING_OPTIMAL` image: blocks of `2^h` GOBs (8 rows × 64 bytes),
  `h = min(4, ceil(log2(ceil(height / 8))))`, so 4 for any screen-sized image
  (`0x0300000000606014`). A 1920×1080 `XRGB8888` image is 7680 × 1152 bytes.
  The host driver advertises this modifier (with `h` 0 to 5) for every scanout
  format on Turing and later.
- Anything else is `DRM_FORMAT_MOD_LINEAR`: a linear image is exactly
  `stride × height` bytes, perhaps rounded up to a page or 64 KiB.

When `height` is a whole number of blocks (768, 1024) the two layouts have
the same size, and linear is assumed: a guest scanning out an optimal image
of that height shows garbage. Linear tiling is always safe.

`SET_SCANOUT_BLOB` logs the resource, blob size, geometry, format and the
modifier chosen at `debug` (`RUST_LOG=device::venus=debug`).

**Debugging: `CONDUIT_VENUS_SCANOUT_MODIFIER`.** In the backend's
environment, forces the modifier of every Venus scanout: `linear`, or a hex
modifier such as `0x0300000000606010` (block-linear, one-GOB blocks; the last
hex digit is `h`). It is for trying layouts without a rebuild, not for
production; the backend logs it at start. With `conduit up` the backend
inherits the shell's environment (`CONDUIT_VENUS_SCANOUT_MODIFIER=0x... conduit
up NAME --venus`); for a libvirt VM it comes from the
`conduit-backend@NAME.service` unit: `systemctl --user edit
conduit-backend@NAME.service` (add `[Service]` and
`Environment=CONDUIT_VENUS_SCANOUT_MODIFIER=0x...`; without `--user` for a
`qemu:///system` VM), then restart the VM.

### RM-export blobs

Memory NVK on RM rendered into, made a Venus resource so Venus contexts (DWM,
the D3D bridge, the KMD's own copies) read it with no CPU copy: the windowed
path of NVK in a Windows guest (`guest/windows/docs/zero-copy-present.md`
H1 to H7). Served only with `NVGPU_CFG_RM_IMPORT`.

How the object comes to exist: NVK exports the image's RM memory to a
control-file descriptor (`NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD`) and
imports that on a render node it opened through the backend
(`DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`, forwarded as any nvidia-drm ioctl).
That makes a host GEM object; its handle comes back to NVK unchanged.

**Wire contract.** `RESOURCE_CREATE_BLOB` with

| Field | Value |
|---|---|
| `hdr.ctx_id` | the guest context the resource is attached to |
| `resource_id` | chosen by the guest, not yet in use |
| `blob_mem` | `BLOB_MEM_RM_EXPORT = 0x80000001` |
| `blob_flags` | 0, or `USE_MAPPABLE` (and/or `USE_SHAREABLE`, which changes nothing): mappable into region 3, for system memory only (see "Mapping an RM-export blob" below) |
| `blob_id` | `rm_handle << 32 \| gem_handle`: `rm_handle` is the backend handle of the render node (the `Open` reply's handle for a `device_type >= 512` node), `gem_handle` what the GEM import returned on that file |
| `size` | nonzero, at most the object's size (the dma-buf's; RM rounds allocations up to 64 KiB, so the image's own size is fine) |
| `nr_entries` | 0 |

The backend checks the context and resource id as for any blob, that
`rm_handle` is a render node this guest has open (`EBADF` otherwise), exports
the GEM handle with `PRIME_HANDLE_TO_FD` on that file (`ENOENT` for a handle
the file does not have), compares `size` with the dma-buf's (`ERANGE` when
larger), and hands the dma-buf to the renderer
(`virgl_renderer_resource_import_blob`, fd type dma-buf), which attaches it
to `ctx_id`. The guest's own `CTX_ATTACH_RESOURCE` after the create is then a
no-op, and other contexts attach it as any resource.

**Errors.** The response is the usual `RESP_ERR_*`, and for this blob type
the header's three `padding` bytes carry the errno (24-bit little-endian,
zero when there is none; every other response leaves them zero):

| Response | errno | Why |
|---|---|---|
| `RESP_ERR_UNSPEC` | `EOPNOTSUPP` | not served (no `NVGPU_CFG_RM_IMPORT`) |
| `RESP_ERR_INVALID_PARAMETER` | `EINVAL` | `blob_flags` other than `USE_MAPPABLE`/`USE_SHAREABLE`, `nr_entries`, `size` 0, or a mappable `size` past region 3 |
| `RESP_ERR_INVALID_PARAMETER` | `EOPNOTSUPP` | `USE_MAPPABLE` of video memory, or of memory whose RM allocation the backend did not see |
| `RESP_ERR_INVALID_PARAMETER` | `EBADF` | `rm_handle` is not a render node this guest has open |
| `RESP_ERR_INVALID_PARAMETER` | `ENOENT` | no such GEM handle on that file |
| `RESP_ERR_INVALID_PARAMETER` | `ERANGE` | `size` larger than the object |
| `RESP_ERR_UNSPEC` | other | the export failed otherwise, or the renderer refused (`EIO`) |
| `RESP_ERR_INVALID_CONTEXT_ID` / `RESP_ERR_INVALID_RESOURCE_ID` / `RESP_ERR_OUT_OF_MEMORY` | 0 / 0 / `ENOMEM` | as for any blob |

**Lifetime.** The backend keeps its own dma-buf descriptor for the resource
and the renderer its own; either keeps the memory alive, so the resource
outlives the render node, the GEM handle and NVK's RM objects. Both go at
`RESOURCE_UNREF`, and every one goes at device reset, backend exit or
renderer death.

**Mapping an RM-export blob (system memory).** A blob created with
`USE_MAPPABLE` of RM **system** memory (`NV01_MEMORY_SYSTEM`) is mapped like
a HOST3D blob: `RESOURCE_MAP_BLOB` places the backend's dma-buf of the object
at the guest's offset in region 3 (the frontend `mmap`s it there: the dma-buf's
own mapping, nvidia-drm's, with the CPU caching the memory has), and answers
`RESP_OK_MAP_INFO` with `map_info` = `VIRTIO_GPU_MAP_CACHE_CACHED` for cached
or write-back memory, `MAP_CACHE_WC` for write-combined, `MAP_CACHE_UNCACHED`
for anything else. `RESOURCE_UNMAP_BLOB` withdraws it; the resource and the
object live on until `RESOURCE_UNREF`, which withdraws a mapping still there
first.

- Offsets and sizes as for any blob: the offset page-aligned, the mapping
  `size` rounded up to whole pages, inside region 3, overlapping no other
  mapping (`RESP_ERR_INVALID_PARAMETER` otherwise); a blob maps once at a
  time.
- Where the memory lives is followed by the backend, since neither nvidia-drm
  nor any RM control a user client may make on the object says (its
  `GET_SURFACE_INFO` `PHYS_ATTR` reads 0, `RM_MAP_MEMORY` echoes the caching
  it was asked for): a forwarded `RM_ALLOC` of `NV01_MEMORY_SYSTEM` or
  `NV01_MEMORY_LOCAL_USER` records the `attr` RM writes back into
  `NV_MEMORY_ALLOCATION_PARAMS` (cached PCI sysmem answers `0x2a800000`), a
  forwarded `OS_UNIX_EXPORT_OBJECT_TO_FD` carries it to the export descriptor,
  and the `GEM_IMPORT_NVKMS_MEMORY` that names that descriptor to the GEM
  object. Video memory (behind BAR1) and objects allocated any other way
  (`NV_ESC_RM_ALLOC_MEMORY`, OS descriptors; those cannot be GEM-imported
  anyway) are refused `USE_MAPPABLE` with `EOPNOTSUPP` at create.
- Coherence: the GPU reads cached system memory snooped (RM gives such memory
  the `SYSTEM_COHERENT` PTE aperture, `kgmmuGetHwPteApertureFromMemdesc`), so
  a guest writing through a cached mapping needs no cache maintenance before
  the compositor samples it; only ordering: the writes must be done before the
  flip that names the buffer is sent. Write-combined memory is read
  non-snooped; the guest must make its WC writes visible (`sfence`) before the
  flip. Measured on the host (`guest/nvk-rm/tests/rm_sysmem_flip.c ... dmabuf`):
  a pattern written through a shared mapping of the dma-buf, cached and WC,
  linear and block-linear, sampled by EGL right after with no flush: 0 of
  2,073,600 pixels differ. CPU reads through the cached mapping run at about
  28 GB/s, through the WC one at about 75 MB/s.
- Guest unmap: with the frontend's in-place region mappings (QEMU patch 0008)
  the range is replaced by fresh anonymous memory before the unmap is
  answered, which drops the dma-buf's pages from the guest's view (the KVM MMU
  notifier on the replaced mapping); the range then reads zeros.

**Layout (modifier).** A dma-buf carries no layout. The backend takes it from
the GEM import: when a forwarded `DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY`
succeeds on a render node it reads the `NvKmsKapiPrivImportMemoryParams` the
guest passed (`layout`, `log2GobsPerBlock`) and keeps, per (file, GEM
handle), until `GEM_CLOSE` or the file's `Close`:

- `layout = PITCH` (1): `DRM_FORMAT_MOD_LINEAR`;
- `layout = BLOCK_LINEAR` (0) with `log2GobsPerBlock = {0, h, 0}`, `h` ≤ 5:
  `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0, s, g, k, h)` with `k`, `g`, `s`
  the node's own `generic_page_kind`, `page_kind_generation`,
  `sector_layout` (`DRM_NVIDIA_GET_DEV_INFO`), i.e.
  `0x0300000000606010 | h` on GB20x; NVK's 1920×1080 swapchain image is
  `h = 5`, `0x0300000000606015`;
- anything else (3D blocks): none.

The blob takes the modifier its object had when it was created.
`SET_SCANOUT_BLOB` uses it instead of the size rule above (which is wrong for
heights that are a whole number of blocks); a blob whose import the backend
did not see falls back to the size rule. `CONDUIT_VENUS_SCANOUT_MODIFIER`
still overrides both.

**What the guest must do to read it.** The host cannot tell a Venus context
the modifier (virtio-gpu has no resource query), so the guest supplies it:
NVK knows the layout it chose (`vkGetImageDrmFormatModifierPropertiesEXT` on
its own image, or NIL's choice) and must hand modifier and row pitch over with
the resource id. The importing context then creates the image with
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`,
`VkImageDrmFormatModifierExplicitCreateInfoEXT` (that modifier, one plane,
offset 0, `rowPitch` = the pitch: `width × 4` for linear, the GOB-aligned row
size for block-linear) and `VkExternalMemoryImageCreateInfo{DMA_BUF}`, enables
`VK_EXT_image_drm_format_modifier`, and allocates with
`VkImportMemoryResourceInfoMESA{resource_id}` (plus
`VkMemoryDedicatedAllocateInfo`; no `VkExportMemoryAllocateInfo`, or one
naming `DMA_BUF` only). vkr turns the import into a
`VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` import of the resource's fd.
NVIDIA's driver refuses the same memory as an `OPTIMAL` image or as
`OPAQUE_FD`, so `prepare_optimal_scanout_copy`'s `OptimalImageTransport::OpaqueFd`
path cannot be used for these resources.

Checked on the host, without a guest: `guest/nvk-rm/tests/rm_export_exec.c`
allocates and exports RM memory as nvk-rm does and runs
`host/venus/examples/venus-rm-import.rs` against a sandboxed `conduit-venus`,
which imports it as above and copies it to a linear image; LINEAR and
block-linear `h` = 5, 4 and 0 at 1920×1080 are pixel-exact (RTX 5090,
610.57.04).

**Checks** (backend, before the renderer sees anything): command length
matches the type; `ctx_id` and `resource_id` exist and belong together;
resource ids unique; blob size nonzero and ≤ region 3 once rounded up to a page (any size, as QEMU takes it; a mapping covers whole pages); map offset
page-aligned, inside region 3, not overlapping another mapping; scanout only
0; at most 1024 contexts and 65536 resources per VM.

**Windows/OVMF guests.** BAR 4 (the shared-memory BAR) is window + 32 GiB +
region 3 rounded up to a power of two: 128 GiB with the `auto` window on an
RTX 5090 (32 GiB) and `--venus`, 256 GiB on an RTX PRO 6000 (128 GiB), 64 GiB
with a window of 16 GiB or less.
OVMF places it only if the guest sees the host's physical address width:
QEMU `-cpu host,host-phys-bits=on`, libvirt `<maxphysaddr mode='passthrough'/>`
(`conduit up` and `conduit attach` already set this). OVMF's 64-bit MMIO
window is then the top eighth of `min(bits, 46)`, 8 TiB on most hosts, and
`auto` keeps the BAR within half of it. Failing that, give OVMF a larger
64-bit MMIO window with `-fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=N`, N at
least twice the BAR in MiB (`262144` for a 128 GiB BAR), or set
`conduit config set gpu.window_mib 4096`.

### Guest-memory blobs

A Venus resource whose memory is the guest's own pages, so a copy on the host
GPU writes straight into memory the guest reads. It is for the Windows KMD's
windowed Present blt. The blt destination is a guest allocation that dxgkrnl
and DWM read through guest system pages. Today the KMD copies the frame into a
Venus present buffer with the GPU, waits on the fence, and then copies that
buffer into those pages with the CPU (0.4 to 0.7 ms a frame, plus a wait of
about 1 ms). With a guest-memory blob over the pages, the GPU copy writes them
directly, and the CPU copy and the wait go
(`guest/windows/docs/rm-backed-standard.md` 13.3, "candidate B"). Served
only with `NVGPU_CFG_GUEST_BLOB`.

**Measured** (RTX 5090, driver 610.57; `host/venus/examples/venus-guest-blob.rs`
through a sandboxed `conduit-venus`). A 1600x900 BGRA `vkCmdCopyImageToBuffer`
from an OPTIMAL image into 1407 scattered 4 KiB pages of a sealed memfd takes
0.207 ms of GPU time. That is the same as into a Venus HOST_VISIBLE buffer, at
about 28 GB/s. Every pixel was right when read through a separate mapping of
the memfd. The import costs about 2 ms for the resource plus about 3 ms for
`vkAllocateMemory`, once per destination. NVIDIA does not import a udmabuf
dma-buf (`vkAllocateMemory` gives `VK_ERROR_OUT_OF_DEVICE_MEMORY`), nor a host
pointer into a udmabuf mapping (`VM_PFNMAP`). It does import a host pointer into
memfd pages (`VK_EXT_external_memory_host`, `minImportedHostPointerAlignment`
4096), including a span stitched from many separate mappings. That is the path
used.

**Detection.** Config `features` bit `NVGPU_CFG_GUEST_BLOB = 1 << 16`, only
with `NVGPU_CFG_VENUS`. The backend sets it only when it runs with
`--venus-guest-blobs` (opt-in while new) and the renderer reports
`FEATURE_IMPORT_GUEST_PAGES`. The renderer reports that when every host Vulkan
device has `VK_EXT_external_memory_host`. Without the bit, `BLOB_MEM_GUEST`
is refused as before (`RESP_ERR_INVALID_PARAMETER`).

**Wire contract.** `RESOURCE_CREATE_BLOB` in a `GpuCmd`, with

| Field | Value |
|---|---|
| `hdr.ctx_id` | the guest's Venus context, to which the resource is attached |
| `resource_id` | chosen by the guest, not yet in use |
| `blob_mem` | `VIRTIO_GPU_BLOB_MEM_GUEST = 1` |
| `blob_flags` | 0 or `USE_SHAREABLE` (which changes nothing). `USE_MAPPABLE` and `USE_CROSS_DEVICE` are refused: there is no region 3 mapping, because the guest already has the pages |
| `blob_id` | 0 |
| `size` | the sum of the entries' lengths: a nonzero multiple of 4096, at most `GUEST_BLOB_MAX_BYTES` (256 MiB) |
| `nr_entries` | 1 to `GUEST_BLOB_MAX_ENTRIES` (4096) |

followed in the same payload by `nr_entries` `virtio_gpu_mem_entry`s
(`{ le64 addr; le32 length; le32 padding = 0 }`, 16 bytes each). `addr` is a
guest **physical** address as the VM sees it (for the KMD, the PFN of an
MDL-locked page shifted left by 12). `length` is a nonzero multiple of 4096.
`addr` is page-aligned, so there are no sub-page offsets. The entries may come
in any order. They are the blob's pages in that order: byte `i` of the blob is
byte `i - (sum of the earlier lengths)` of the entry it falls in. Each entry
must lie wholly inside one region of guest RAM, as the vhost-user memory table
describes it (RAM above 4 GiB included). All entries must be in the same guest
RAM file, which they always are with QEMU's single `memory-backend-memfd`. The
backend merges entries that are adjacent in guest RAM, so the guest should
coalesce where it can: each merged run is one host mapping.

The backend resolves the entries through the memory table. It sends the
renderer the guest RAM file and the runs (`Renderer::import_guest_pages`, IPC
op `IMPORT_GUEST_PAGES`). The renderer maps the runs, in order, as one span of
its own address space and makes it a `VIRGL_RESOURCE_HOST_PTR` resource
(`host/venus/patches/0002-vkr-host-pointer-resources.patch`). The backend then
attaches the resource to `ctx_id`. The guest's own `CTX_ATTACH_RESOURCE` after
the create is then a no-op, and other Venus contexts attach it as any resource.

**Importing it** (in the guest's Venus context):

1. `vkGetMemoryResourcePropertiesMESA(resourceId)` gives `memoryTypeBits`.
   These are the host-pointer memory types, HOST_VISIBLE | HOST_COHERENT (on
   the RTX 5090: types 2 and 3, the latter also CACHED). With
   `VkMemoryResourceAllocationSizePropertiesMESA` chained, it also gives the
   blob's size.
2. `vkAllocateMemory` with `VkImportMemoryResourceInfoMESA { resourceId }`,
   a `memoryTypeIndex` from those bits, and `allocationSize` = the blob size.
   vkr rounds a smaller size up to the import alignment (4096) and refuses one
   larger than the blob. It turns the import into a
   `VkImportMemoryHostPointerInfoEXT` of the span and drops any
   `VkExportMemoryAllocateInfo` (host memory is not exported again).
3. `vkCreateBuffer` (plain; `TRANSFER_DST` is enough, and no
   `VkExternalMemoryBufferCreateInfo` is needed), then bind it at offset 0.
4. Use it as the destination of `vkCmdCopyImageToBuffer`. After the copy,
   record a barrier from `TRANSFER` / `TRANSFER_WRITE` to `HOST` /
   `HOST_READ` before the fence. The CPU may read the pages once the fence has
   signalled.

Do not `vkMapMemory` it through Venus. The guest has the pages already, and a
Venus mapping would need a region 3 placement this resource does not have.

**Coherence.** The memory types are HOST_COHERENT: the GPU's writes to host
RAM are snooped on x86, so the guest's ordinary write-back mapping of the
pages sees them once the fence has signalled. This is verified page by page
through a separate mapping of the memfd. A host whose driver offers no
host-pointer import does not set the bit.

**Lifetime.** The guest must keep the pages locked, at the same guest physical
addresses, from the create until after `RESOURCE_UNREF`. The host maps the
pages at create time and keeps that mapping until the unref. The order is:

1. Wait for the last copy's fence.
2. `vkDestroyBuffer` and `vkFreeMemory`.
3. `RESOURCE_UNREF`.
4. Unlock the pages.

Venus ring commands run asynchronously to control commands. For the strong
guarantee that nothing on the host still refers to the pages, make sure the
`vkFreeMemory` has executed before the `UNREF` (a ring fence or seqno after
it). After both, the host holds no mapping and no pin of the pages.

An `UNREF` first is still safe for the host. NVIDIA's import keeps the pages
it pinned until the `VkDeviceMemory` is freed or the context dies, and only
the guest's own data is at risk: the GPU could write pages the guest has
already reused. The renderer's spans come out of an address range reserved
for guest pages only. A freed span goes back to an inaccessible reservation
and is handed out again only for guest pages, so an import that races an
`UNREF` reaches this guest's memory or nothing, never the renderer's own.
Device reset, backend exit and renderer death release everything.

**Limits.** Per VM, at once: `GUEST_BLOB_MAX_LIVE` (1024) guest blobs,
`GUEST_BLOB_MAX_LIVE_RUNS` (32768) merged runs and
`GUEST_BLOB_MAX_LIVE_BYTES` (32 GiB). Beyond any of these the create gets
`RESP_ERR_OUT_OF_MEMORY` / `ENOMEM`. The constants are in
`host/backend/protocol/src/venus.rs`.

**Errors.** As for RM-export blobs, the header's three `padding` bytes carry
the errno:

| Response | errno | Why |
|---|---|---|
| `RESP_ERR_INVALID_PARAMETER` | 0 | not served (no `NVGPU_CFG_GUEST_BLOB`): refused as any unknown `blob_mem` |
| `RESP_ERR_INVALID_PARAMETER` | `EINVAL` | shape: `blob_flags`, `blob_id`, `nr_entries` 0 or over 4096, `size` 0, not pages, over 256 MiB or not the sum of the entries, an entry not whole pages |
| `RESP_ERR_INVALID_PARAMETER` | `EFAULT` | an entry outside guest RAM, or running past the end of its RAM region |
| `RESP_ERR_INVALID_PARAMETER` | `EXDEV` | entries in more than one guest RAM file (a VM with several memory backends) |
| `RESP_ERR_OUT_OF_MEMORY` | `ENOMEM` | the live limits above, or the resource limit |
| `RESP_ERR_UNSPEC` | `EOPNOTSUPP` | the guest's memory table is not known yet |
| `RESP_ERR_UNSPEC` | `EIO` | the renderer refused the import or the attach |
| `RESP_ERR_INVALID_CONTEXT_ID` / `RESP_ERR_INVALID_RESOURCE_ID` | 0 | as for any blob |

On the import side, `vkAllocateMemory` gives `VK_ERROR_INVALID_EXTERNAL_HANDLE`
(the resource is not a guest blob, the size is too large, or the renderer
cannot import host pointers) or `VK_ERROR_OUT_OF_DEVICE_MEMORY` (the driver
refused the pin). Any refusal means: use the legacy copy for that destination.

**Security.** The renderer receives the guest RAM file to map the runs from,
which gives it the whole of guest RAM, not only the runs. The backend has the
same file already, and the renderer is sandboxed (below). It closes the
descriptor after mapping, so only the mapped runs stay reachable.

### RM-export resources in a second process (`RmResourceImport`, MsgType 31)

An RM-export blob is memory one NVK process rendered into. A second NVK
process of the same guest (a D3D app that opened the first one's shared
surface; later DWM on NVK composing it) must map the same RM memory in its
own RM client, without learning the first process's RM handles: the Helios
KMD lets the resource id, and nothing else, cross processes
(`guest/windows/docs/shared-surfaces.md`). The backend already holds the
object as a dma-buf for the resource's life, so it imports that dma-buf on a
render node of the caller as a GEM object. NVK then does what it does for any
dma-buf: `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY` to a control descriptor of its
own, `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD` into its client, and
`GEM_CLOSE` (all forwarded as today).

Served with `NVGPU_CFG_RM_RESOURCE_IMPORT = 1 << 14`, which is set whenever
`NVGPU_CFG_RM_IMPORT` is. Control queue, not a `GpuCmd`: it names a DRM file,
which only the RM side of the backend has.

```text
request:  MsgHeader{msg_type=31, handle=0, status=0, padding=0}
          | RmResourceImport{owner_handle u32, resource_id u32, flags u32 = 0, reserved u32 = 0}   16 bytes
response: MsgHeader{msg_type=31, handle=0, status=0}
          | RmResourceImportReply{gem_handle u32, flags u32, size u64, modifier u64}             24 bytes
          or MsgHeader{status = -errno} alone
```

| Field | Meaning |
|---|---|
| `owner_handle` | backend handle of a render node the guest opened (an `Open` reply's handle for `device_type >= 512`): the file the GEM handle will belong to |
| `resource_id` | an RM-export resource (`RESOURCE_CREATE_BLOB` with `BLOB_MEM_RM_EXPORT`), or a Venus blob (`BLOB_MEM_HOST3D`) whose renderer export is a dma-buf (below), that still exists, whoever created it |
| `gem_handle` | GEM handle in `owner_handle`'s file, the guest's to close (`DRM_IOCTL_GEM_CLOSE`, or the file's `Close`). The same resource on the same file answers the same handle (GEM handles are per object per file), so one close undoes any number of imports |
| `flags` | bit 0 (`RM_RESOURCE_IMPORT_MODIFIER`): `modifier` is known |
| `size` | the object's size (the dma-buf's), bytes |
| `modifier` | the modifier the resource was created with (its GEM import's layout); also remembered for the new handle, so a `ScanoutFlip` of it or a second RM-export blob made from it carries it. Never known for a Venus blob (flags 0) |

**Venus blobs.** A Venus app's surface (DWM on NVK composing a Venus
process's window) is host Vulkan memory, held as the descriptor the renderer
exported when the blob was made (`virgl_renderer_resource_export_blob`).
When that descriptor is a dma-buf (its file system is dma-buf's,
`DMA_BUF_MAGIC`), the backend imports it on the caller's render node exactly
like an RM-export blob's: spike X4 (`guest/nvk-rm/tests/vk_dmabuf_to_rm.c`)
showed memory NVIDIA's Vulkan driver exports as `DMA_BUF` imports into an RM
client through the same nvidia-drm calls, exactly. An `OPAQUE_FD` export (a
driver handle, which nvidia-drm cannot take) is refused with `EINVAL`, as
before. The reply carries no modifier: the image layout belongs to the Venus
context that created the image, the memory does not record it, and the
backend does not guess; the importer takes it from the surface's metadata
(which then must carry it, `dxvk-on-nvk.md` 3.2). For this to work the Venus
side must allocate such surfaces exportable as `DMA_BUF`, as
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` images with an explicit modifier
(NVIDIA imports `DMA_BUF` memory only into such images), and record that
modifier and row pitch where the importer reads them.

What the backend checks: the shape (16 bytes, `flags` and `reserved` 0:
`EINVAL`), that the backend serves RM-export blobs (`EOPNOTSUPP`), that
`owner_handle` is a render node of this guest (`EBADF`), that the resource
exists (`ENOENT`) and is an RM-export blob or a Venus blob exported as a
dma-buf (`EINVAL` otherwise: an `OPAQUE_FD` is not nvidia-drm's to import).
`PRIME_FD_TO_HANDLE`'s own errno otherwise. Resource ids are the VM's, so "a resource of the same guest" holds
by construction; which process may name which resource is the KMD's to decide
(it forwards the message only for a resource the caller created or opened).

Lifetime: the GEM handle holds the object as long as the caller keeps it, and
the RM memory NVK imports from it holds it for as long as that lives,
independently of the resource, of the creator's process and of the GEM handle
itself once the import is done. `RESOURCE_UNREF` of the resource does not
take it away from an importer.

**Reset and close.** Device reset, backend exit or renderer death: every
context and resource is destroyed, region 3 emptied, held fenced chains
returned with `RESP_ERR_UNSPEC`. Renderer death is noticed either by a call
failing or, between commands, by the fence thread (`Renderer::signalled`
returns `Disconnected`); from then on every `GpuCmd` is answered
`RESP_ERR_UNSPEC`.

## Backend ↔ renderer

`conduit-venus` runs as its own process: the NVIDIA Vulkan driver needs more
than the backend's sandbox allows (its libraries, shader cache, more ioctls).
The backend talks to it over a Unix `SOCK_SEQPACKET` socket
(`host/venus/src/ipc.rs`): one request per call, replies in order, messages
sent in 64 KiB fragments (a submit can be 4 MiB), blob and dma-buf fds passed
by `SCM_RIGHTS` (from the renderer, and to it for `IMPORT_DMABUF`, the
RM-export blob's dma-buf, and for `IMPORT_GUEST_PAGES`, the guest RAM file of
a guest-memory blob), and fence signals pushed by the renderer on their
own. What a renderer can do beyond the base calls (`Renderer::features`:
`FEATURE_IMPORT_DMABUF`, `FEATURE_IMPORT_GUEST_PAGES`) is asked once as `CAPSET_INFO` with index
`0xffffffff`: a `conduit-venus` from before it refuses that index like any
other, so the backend reads "no features" and never sends it an op it does
not know (an unknown op ends the connection). Both
sides use the `conduit_venus::Renderer` trait (`host/venus/src/lib.rs`): the
backend through the IPC client, tests through `conduit_venus::mock::Mock`.

**Latency options** (off by default; `conduit config set backend.latency`,
[research/host-roundtrip-latency.md](research/host-roundtrip-latency.md)).
`fused-submit`: a fenced `SUBMIT_3D` is one call, `Renderer::submit_fenced`
(IPC op `SUBMIT_FENCED` (15), sent only to a server with `FEATURE_SUBMIT_FENCED` (bit 3),
which every server from this release on adds itself), instead of `SUBMIT`
and `CREATE_FENCE`, each waiting for its reply. `direct-fences`:
`conduit-venus --direct-fences` sends each `FENCES` message from the
virglrenderer thread that retired the fence, under the same send lock as the
serve loop's replies, so the fragments of a message never interleave. The
backend's reader thread then returns the signalled chains itself when the
backend lock is free (`Renderer::set_fence_hook`), and otherwise leaves them
to the fence pump. A fence can now reach the backend before the reply to
its `CREATE_FENCE`. It is taken only under the backend lock, which the queue
thread holds until it has recorded the chain.

One `conduit-venus` per VM: `conduit up/view --venus` (and the libvirt
backend unit) starts it before the backend, on `venus.sock` in the VM's run
directory, logging to `logs/venus.log`; it serves that one backend and exits
when it hangs up. `packaging/build.sh venus` builds it for the packages, as
`/opt/conduit/bin/conduit-venus` with its virglrenderer in `/opt/conduit/lib`
([PACKAGING.md](PACKAGING.md)); the release workflow, the RPM spec, the
PKGBUILD and the flake all build it, so every release install has it. In a
checkout: `host/venus/build-virglrenderer.sh` (which applies
`host/venus/patches`), then `cargo build --release --features renderer` in
`host/venus` (`CONDUIT_VENUS=PATH` points the CLI at it).

**Open files.** Every guest GPU buffer holds a descriptor or two in
`conduit-venus` (shared memory, a dma-buf, the NVIDIA driver's handle) and
one in the backend, so both raise their soft `RLIMIT_NOFILE` to the hard
limit at start (logged): a Windows desktop with one 3D application passes the
usual 1024, and past it buffer creation fails and the guest's Vulkan driver
aborts.

**Sandbox** (`host/venus/src/sandbox.rs`), entered before the first request:
the backend's posture (no root or `CAP_SYS_ADMIN`, capabilities dropped,
`no_new_privs`), Landlock limited to the system library and driver
configuration paths, the NVIDIA and `/dev/dri` device nodes and a per-VM
shader cache (`$XDG_CACHE_HOME/conduit/venus/VM`), nothing executable; and a
seccomp denylist (exec, fork, ptrace, mounts and namespaces, module loading,
io_uring, bpf, ...; no sockets but AF_UNIX, and no new connections). It is
wider than the backend's: the driver loads libraries and patches its own
code, and a device with `VK_KHR_acceleration_structure` (every vkd3d-proton
D3D12 device, which enables DXR) brings up libcuda inside the driver, which
opens `/dev/nvidia-uvm` and binds and listens on an abstract socket; refused,
that `vkCreateDevice` fails with `VK_ERROR_INITIALIZATION_FAILED`.
`--no-sandbox` is for debugging only.
