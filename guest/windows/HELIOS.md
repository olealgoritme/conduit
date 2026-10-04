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
