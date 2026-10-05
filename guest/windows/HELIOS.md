# Imported from Helios

The Windows guest driver starts from [Helios](https://github.com/winboat-org/helios)
by the WinBoat project (rupansh, TibixDev), used with the authors' permission.

Imported at Helios commit `52c02799a042ecab25b9e812db9c002ba98ddb7c` (2026-10-02),
directories copied unchanged:

| Here | Helios | What |
|---|---|---|
| `kmd_render/` | `kmd_render/` | WDDM render + display miniport (Rust, no_std) |
| `kmd_logic/` | `kmd_logic/` | host-testable KMD logic |
| `protocol/` | `protocol/` | UMD/ICD ↔ KMD escape ABI and wire structs |
| `umd/`, `umd12/`, `umd_common/` | same | D3D11 / D3D12 user-mode drivers (DXVK, vkd3d-proton bridges) |
| `installer/`, `packaging/` | same | Windows installer and install/verify scripts |
| `ci/windows/` | `ci/windows/` | build scripts for the Windows workflow (`.github/workflows/windows.yml`), paths pointed at the submodules below |
| `tools/`, `metadata/`, `icd/win-build/` | same (the files the build and packaging scripts use) | metadata check, signing, probes, Mesa build compat header |

Submodules, pinned to the commits Helios pins:

| Here | Repository |
|---|---|
| `third_party/mesa` | winboat-org/mesa-helios (Venus Vulkan ICD for Windows) |
| `third_party/dxvk` | winboat-org/dxvk |
| `third_party/vkd3d-proton` | winboat-org/vkd3d-proton |
| `../../host/venus/third_party/venus-protocol` | winboat-org/venus-protocol |
| `../../host/venus/third_party/virglrenderer` | upstream virgl/virglrenderer `main` (Helios's fork is not public; it adds native DGC for D3D12) |

Conduit's changes are confined to the KMD's transport: Helios talks to
QEMU's virtio-gpu; here the KMD talks to Conduit's device (virtio id 45) and
the Venus messages in `host/backend/protocol`. The escape ABI between the
user-mode drivers and the KMD stays Helios's, so Mesa, DXVK, vkd3d-proton and
the UMDs build unchanged.

## KMD transport on Conduit's device

What changed in `kmd_render/src/virtio` (everything else is Helios's):

- **Identity.** The INF and packaging scripts match virtio id 45 (`DEV_106D`)
  and id 41 (`DEV_1069`). Conduit's QEMU runs the device as id 45
  (`virtio-id=45`); id 41 is the alternative, as `conduit_gpu.ko virtio_id=41` is on Linux.
- **Negotiation.** Only `VIRTIO_F_VERSION_1` is offered or required. Config
  `features` must carry `NVGPU_CFG_VENUS` (offset 3884), i.e. the backend runs
  with `--venus`, or the KMD fails `StartDevice` with a message.
- **Framing.** Every control-queue message is `MsgHeader{GpuCmd}` + the
  virtio-gpu command, answered by `MsgHeader` + the virtio-gpu response. The two
  headers sit in a 32-byte tail of each `DmaBuffer` (`hal.rs`), added to the
  chain by `Chain::spans`. A response `MsgHeader.status < 0` is turned into
  `RESP_ERR_UNSPEC` in `drain_used`. Fences are unchanged: the same
  `VIRTIO_GPU_FLAG_FENCE` / `INFO_RING_IDX` commands complete out of order on
  the used ring.
- **Event queue.** Virtqueue 1 carries buffers the KMD posts empty and the host
  fills: `EventReady{handle}` (a bare 16-byte `MsgHeader`) for an RM backend
  handle that became readable. The DPC drains it and `KeSetEvent`s the event a
  process registered for that handle (`HELIOS_NVRM_OP_EVENT_REGISTER`,
  `kmd_render/src/virtio/gpu/nvrm_events.rs`). Nothing else is read from the queue
  (`InputEvent`, `DisplayMode` and clipboard messages are counted and dropped), and
  no feature bit is acked for it: in particular never `NVGPU_CFG_TAKES_INPUT` (bit
  12), which would move keyboard and mouse off the emulated devices.
- **Region 3.** The host-visible blob window is shared memory region 3
  (`SHM_ID_VENUS`), found by `pci_caps::scan_host_visible_window`.
- **Limit.** Submit streams larger than a 4 MiB `GpuCmd` are refused.

Built by `.github/workflows/windows.yml` (run by hand): the KMD and the D3D11/12
UMDs compile and are test-signed, Release and Debug. Not yet run on a Windows
guest; the first test is a Windows 11 QEMU guest with `--venus` and region 3.
