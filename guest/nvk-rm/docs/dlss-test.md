# DLSS test in a Windows guest

Experimental, never run on a GPU yet. Background: [dlss.md](dlss.md).

## What to test with

Native Vulkan first (no D3D mapping involved), single-player, no anti-cheat:

| Game | API | Note |
|---|---|---|
| No Man's Sky | Vulkan | DLSS in graphics settings |
| DOOM Eternal | Vulkan | DLSS 2.x |
| DOOM: The Dark Ages, Indiana Jones and the Great Circle | Vulkan | DLSS 4; heavy, ray tracing required |
| Baldur's Gate 3 (Vulkan renderer) | Vulkan | DLSS in the Vulkan mode |
| Red Dead Redemption 2 (Vulkan) | Vulkan | DLSS 2.x |

Then a D3D12 game (Cyberpunk 2077, Control, any UE5 game). D3D11 DLSS does
not work yet (dlss.md). Ask the user before starting any game.

## Install

Needs a driver package with this branch (NVK `Dlss`, the D3D12 meta-commands).

1. Host: put into the guest share (for example `Z:\dlss`):
   `/usr/lib/x86_64-linux-gnu/nvidia/wine/nvngx.dll` and `_nvngx.dll`,
   `nvapi64.dll` (`guest/windows/tools/build-dxvk-nvapi.sh`), and on RTX 50
   the cubin cache built from the game's own DLSS DLLs:
   `nvk-cubin-cache.py build -o cubin <game>\nvngx_dlss*.dll`.
2. Guest, elevated PowerShell, game and Steam closed:
   ```powershell
   .\Install-Dlss.ps1 -NgxSource Z:\dlss -CubinCache Z:\dlss\cubin -Game '<game.exe>' -Log
   ```
   It prints the `_nvngx.dll` signature status; it should be `Valid`.
3. Undo: `.\Uninstall-Dlss.ps1` (restores every value and file it changed).

## Logs

| Log | Where | Turned on by |
|---|---|---|
| NGX | `C:\ProgramData\Helios\logs\dlss\nvngx*.log` | `-Log` (`NGXCore` `LogLevel` 2, `LogPath`) |
| DXVK-NVAPI | `C:\ProgramData\Helios\logs\dlss\nvapi64.log`, also DebugView | `-Log` (`Helios\Nvapi` `DXVK_NVAPI_LOG_LEVEL`, `DXVK_NVAPI_LOG_PATH`) |
| NVK | DebugView (Sysinternals, "Capture Win32"); or `MESA_LOG_FILE` when the game is started from PowerShell | always |
| D3D12 driver | `umd12-<pid>.log` in the Helios log folder | always |
| Loader search | Process Monitor, filter Path contains `_nvngx` or `NGXCore` | — |

## What success looks like

- NVK (DebugView): `NVK: DLSS on (VK_NVX_binary_import)`; on RTX 50
  `NVK: CUDA module sm_120 SASS from the cubin cache (...)`, and no
  `Fatbin does not have compatible bytecode`.
- nvapi64.log: `Created a Vulkan instance of our own`, `Reporting the Vulkan
  device's PCI IDs 10de:...`, `NvAPI Device: NVIDIA GeForce RTX ...`; D3D12:
  `D3D12: vkd3d-proton device reached through the Helios UMD`.
- umd12 log (D3D12): `DLSS: engine device handed out`.
- NGX log: the `_nvngx.dll` it loaded, the GPU arch (Blackwell on RTX 50),
  DLSS SR feature created without errors.
- In game: the DLSS option is selectable, the indicator (bottom left) shows
  DLSS SR and its version, the image is sharp, and DLSS Quality runs faster
  than native at the same output resolution.

## Known blockers and things to check

- The loader order and the signature check of the Linux driver's
  `_nvngx.dll` on Windows are unconfirmed (Process Monitor).
- No nvapi64.log at all: NGX did not load our nvapi64 from the game folder;
  try `-SystemNvapi`.
- Games that offer DLSS only for DXGI vendor 0x10de (ours is 0x1af4) hide
  the option, mostly D3D games; Vulkan games usually read the Vulkan vendor
  (0x10de on NVK). OptiScaler with `Dx12Upscaler=dlss` spoofs the vendor and
  passes the calls on to the real DLSS.
- D3D12: whether the runtime hands the app the driver's descriptor handles
  unchanged (cubin texture handles), and vkd3d-proton's 64-bit cubin path.
- Frame generation (dlssg) and Multi Frame Generation: not supported.
- After a host driver update, copy `nvngx.dll` / `_nvngx.dll` again.
