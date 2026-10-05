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
`packaging/build.sh` (so every package) builds the backend with it, `make`
does not (`cargo build --release -p device --features vhost-user,venus --bin
conduit-backend` in `host/backend`, then `CONDUIT_BACKEND=PATH`).

Setting up a Windows VM, installing the guest driver and tuning it:
[WINDOWS.md](WINDOWS.md).

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

Both sides log fence latency every 2 s while fences flow: `conduit-venus`
the time from `create_fence` to virglrenderer's signal (count, p50, p90,
max, in `logs/venus.log`), the backend the time from holding a fenced
command to the signal reaching it, per `(ctx_id, ring)`, at `info`
(`venus: fence latency ctx C ring R: ...`).

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
| `RESOURCE_CREATE_BLOB` | `blob_mem = HOST3D` only (guest-memory blobs refused for now); renderer allocates, returns an fd and map info |
| `RESOURCE_MAP_BLOB` | the blob's fd placed in region 3 at the guest's offset; reply `RESP_OK_MAP_INFO` with the cache type |
| `RESOURCE_UNMAP_BLOB` | withdrawn from region 3 |
| `RESOURCE_UNREF` | unmapped if mapped, then freed |
| `SET_SCANOUT_BLOB` | records scanout 0's resource, size, format, stride, offset, and the modifier its blob's size implies (below); resource 0 turns the scanout off (`ScanoutDisable` to the viewer); a format with no DRM fourcc is `RESP_ERR_INVALID_PARAMETER`, a scanout other than 0 `RESP_ERR_INVALID_SCANOUT_ID` |
| `RESOURCE_FLUSH` | renderer exports the scanout resource as a dma-buf with the guest's layout (cached per resource and layout), sent to the viewer as a frame |

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
  connector), and without them kept it at 60 Hz;
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

**Checks** (backend, before the renderer sees anything): command length
matches the type; `ctx_id` and `resource_id` exist and belong together;
resource ids unique; blob size nonzero and ≤ region 3 once rounded up to a page (any size, as QEMU takes it; a mapping covers whole pages); map offset
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
The backend talks to it over a Unix `SOCK_SEQPACKET` socket
(`host/venus/src/ipc.rs`): one request per call, replies in order, messages
sent in 64 KiB fragments (a submit can be 4 MiB), blob and dma-buf fds passed
by `SCM_RIGHTS`, and fence signals pushed by the renderer on their own. Both
sides use the `conduit_venus::Renderer` trait (`host/venus/src/lib.rs`): the
backend through the IPC client, tests through `conduit_venus::mock::Mock`.

One `conduit-venus` per VM: `conduit up/view --venus` (and the libvirt
backend unit) starts it before the backend, on `venus.sock` in the VM's run
directory, logging to `logs/venus.log`; it serves that one backend and exits
when it hangs up. `packaging/build.sh venus` builds it for the packages, as
`/opt/conduit/bin/conduit-venus` with its virglrenderer in `/opt/conduit/lib`
([PACKAGING.md](PACKAGING.md)); the release workflow, the RPM spec and the
PKGBUILD do not run that step yet, so release packages come without it. In a
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
code. `--no-sandbox` is for debugging only.
