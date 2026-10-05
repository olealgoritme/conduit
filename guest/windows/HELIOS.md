# Imported from Helios

The Windows guest driver starts from [Helios](https://github.com/winboat-org/helios)
by the WinBoat project (rupansh, TibixDev), used with the authors' permission.

Imported at Helios commit `52c02799a042ecab25b9e812db9c002ba98ddb7c` (2026-10-02).
The directories were copied unchanged; Conduit's changes since are listed
below. The READMEs inside them are upstream's and describe Helios on QEMU's
virtio-gpu; some (`kmd_render/README.md`, `metadata/README.md`) are older
bring-up and change notes. Using a Windows guest on Conduit:
[docs/WINDOWS.md](../../docs/WINDOWS.md).

| Here | Helios | What |
|---|---|---|
| `kmd_render/` | `kmd_render/` | WDDM render + display miniport (Rust, no_std) |
| `kmd_logic/` | `kmd_logic/` | host-testable KMD logic |
| `protocol/` | `protocol/` | UMD/ICD ↔ KMD escape ABI and wire structs |
| `umd/`, `umd12/`, `umd_common/` | same | D3D11 / D3D12 user-mode drivers (DXVK, vkd3d-proton bridges) |
| `installer/`, `packaging/` | same | Windows installer and install/verify scripts |
| `ci/windows/` | `ci/windows/` | build scripts for the Windows workflow (`.github/workflows/windows.yml`), paths pointed at the submodules below |
| `ci/vm/` | (Conduit's) | local build VM: toolchain setup, incremental build and signing ([ci/vm/README.md](ci/vm/README.md)) |
| `tools/`, `metadata/`, `icd/win-build/` | same (the files the build and packaging scripts use) | metadata check, signing, probes, Mesa build compat header |

Submodules, pinned to the commits Helios pins:

| Here | Repository |
|---|---|
| `third_party/mesa` | winboat-org/mesa-helios (Venus Vulkan ICD for Windows) |
| `third_party/dxvk` | winboat-org/dxvk |
| `third_party/vkd3d-proton` | winboat-org/vkd3d-proton |
| `../../host/venus/third_party/venus-protocol` | winboat-org/venus-protocol |
| `../../host/venus/third_party/virglrenderer` | upstream virgl/virglrenderer `main` (Helios's fork is not public; it adds native DGC for D3D12) |

Helios talks to QEMU's virtio-gpu; here the KMD talks to Conduit's device
(virtio id 45) and the Venus messages in `host/backend/protocol`. The escape
ABI between the user-mode drivers and the KMD stays Helios's, so Mesa, DXVK,
vkd3d-proton and the UMDs build unchanged. Conduit's other changes are in the
KMD's display side, the names and the packaging scripts (below).

## KMD transport on Conduit's device

What changed in `kmd_render/src/virtio`:

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
- **Region 3.** The host-visible blob window is shared memory region 3
  (`SHM_ID_VENUS`), found by `pci_caps::scan_host_visible_window`.
- **Limit.** Submit streams larger than a 4 MiB `GpuCmd` are refused.
- **PCI id.** virtio-drivers 0.13 knows device types only up to 25, so the
  KMD reports the virtio-gpu type for `DEV_106D` and `DEV_1069` itself.

## Display and other changes

Driver 22.22.297.0 (`kmd_render/driver-version.env`):

- **Mode from the host's EDID.** At init the KMD sends `GET_EDID` and adopts
  the native timing (DisplayID Type VII/I, else the base block's detailed
  timing; `kmd_logic/src/edid.rs`) for the VidPn mode, the monitor modes and
  the vsync timer, and hands the EDID to Windows unchanged. Without an EDID
  from the host it keeps Helios's behaviour (size from `GET_DISPLAY_INFO`,
  generated EDID, 60 Hz).
- **Refresh rates.** The host's rate plus 144/120/60 Hz below it are offered;
  `DxgkDdiDescribeAllocation` reports the active mode's rate instead of a
  fixed 60/1 (above 60 Hz DWM otherwise stopped flipping); the committed
  target mode's rate drives the primary and the vsync heartbeat; rates are
  reported in lowest terms.
- **Connector.** The monitor's child device reports DisplayPort, not analog
  VGA (`OutputTech` knob).
- **Paging.** A paging transfer with no transport (device restart, host loss)
  is reported as not ours instead of failing VidMm (which bugchecked 0x10E).
- **Diagnostics.** Knobs `OutputTech`, `VsyncRateMhz`, `FlipCapsX`,
  `FlipQueueN`, `DiagLevel` (KMD) and `UmdTrace`, `ScanoutAcquire` (UMD), and
  counters for the EDID and refresh decisions ([docs/WINDOWS.md](../../docs/WINDOWS.md#registry-knobs));
  `tools/scanout_timeline_dump.c` from upstream Helios reads the KMD's scanout
  timeline.
- **Names.** Adapter "Conduit Helios" (Mesa matches "Helios"), monitor
  "Conduit".
- **Topology.** `packaging/windows/Set-HeliosDisplay.ps1` and a logon task
  make Helios the only active display.

Built by `.github/workflows/windows.yml` (run by hand; Release and Debug,
test-signed) or locally in a build VM (`ci/vm/`). Runs in a Windows 11 guest
on QEMU with `--venus` (Unigine Heaven at about 150 fps on an RTX 5090).
