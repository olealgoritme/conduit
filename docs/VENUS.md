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
`--venus`; without it the config bit is clear and `GpuCmd` is refused.

## Device

| | |
|---|---|
| Config `features` bit | `NVGPU_CFG_VENUS = 1 << 10`. Set only with `--venus`. A guest sends `GpuCmd` only when set. |
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

**Fences.** A command with `VIRTIO_GPU_FLAG_FENCE` set completes (its chain is
returned to the used ring) only after the renderer signals that fence, as QEMU
does. With `VIRTIO_GPU_FLAG_INFO_RING_IDX` the fence is on `(ctx_id, ring_idx)`,
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

**Commands served** (the set Helios's KMD sends). Anything else answers
`RESP_ERR_UNSPEC`.

| Command | Host action |
|---|---|
| `GET_DISPLAY_INFO` | one scanout, from the config display size; enabled iff a display is configured |
| `GET_CAPSET_INFO` | index 0 → `VIRTIO_GPU_CAPSET_VENUS` (4); other indices `RESP_ERR_INVALID_PARAMETER` |
| `GET_CAPSET` | Venus capset from the renderer |
| `CTX_CREATE` | context with `context_init` capset Venus only |
| `CTX_DESTROY` | destroys the context and detaches its resources |
| `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` | renderer attach/detach |
| `SUBMIT_3D` | Venus command stream to the context |
| `RESOURCE_CREATE_BLOB` | `blob_mem = HOST3D` only (guest-memory blobs refused for now); renderer allocates, returns an fd and map info |
| `RESOURCE_MAP_BLOB` | the blob's fd placed in region 3 at the guest's offset; reply `RESP_OK_MAP_INFO` with the cache type |
| `RESOURCE_UNMAP_BLOB` | withdrawn from region 3 |
| `RESOURCE_UNREF` | unmapped if mapped, then freed |
| `SET_SCANOUT_BLOB` | records scanout 0's resource, size, format, stride, offset; a format with no DRM fourcc is `RESP_ERR_INVALID_PARAMETER` |
| `RESOURCE_FLUSH` | renderer exports the scanout resource as a dma-buf with the guest's layout (cached per resource and layout), sent to the viewer as a frame |

**Scanout layout.** The dma-buf the viewer gets is described entirely by the
guest's `SET_SCANOUT_BLOB`: `width`, `height`, `strides[0]`, `offsets[0]`, and
the format as the DRM fourcc of the same memory layout (`B8G8R8A8` →
`ARGB8888`, `B8G8R8X8` → `XRGB8888`, `R8G8B8A8` → `ABGR8888`, `R8G8B8X8` →
`XBGR8888`, and the other four `VIRTIO_GPU_FORMAT_*`), with modifier
`DRM_FORMAT_MOD_LINEAR`. A Venus blob carries no image layout the host can
read back, so **Venus scanout images must be linear for now**: guest drivers
must allocate scanout images with linear tiling.

**Checks** (backend, before the renderer sees anything): command length
matches the type; `ctx_id` and `resource_id` exist and belong together;
resource ids unique; blob size page-aligned and ≤ region 3; map offset
page-aligned, inside region 3, not overlapping another mapping; scanout only
0; at most 1024 contexts and 65536 resources per VM.

**Windows/OVMF guests.** BAR 4 (the shared-memory BAR) is 64 GiB with or
without `--venus`, and 128 GiB when `--venus-hostmem-mib` is above 31 GiB.
OVMF places it only if the guest sees the host's physical address width:
QEMU `-cpu host,host-phys-bits=on`, libvirt `<maxphysaddr mode='passthrough'/>`
(`conduit up` and `conduit attach` already set this). Failing that, give OVMF
a larger 64-bit MMIO window with
`-fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=131072` (`262144` for a 128 GiB
BAR).

**Reset and close.** Device reset, backend exit or renderer death: every
context and resource is destroyed, region 3 emptied, held fenced chains
returned with `RESP_ERR_UNSPEC`. Renderer death is noticed either by a call
failing or, between commands, by the fence thread (`Renderer::signalled`
returns `Disconnected`); from then on every `GpuCmd` is answered
`RESP_ERR_UNSPEC`.

## Backend ↔ renderer

`conduit-venus` runs as its own process: the NVIDIA Vulkan driver needs more
than the backend's sandbox allows (its libraries, shader cache, more ioctls).
The backend talks to it over a Unix socket, with blob and dma-buf fds passed
by `SCM_RIGHTS`. Both sides use the `conduit_venus::Renderer` trait
(`host/venus/src/lib.rs`): the backend through the IPC client, tests through
`conduit_venus::mock::Mock`.
