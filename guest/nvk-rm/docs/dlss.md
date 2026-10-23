# DLSS and other upscalers in Windows guests

Status: **experimental, off by default, not yet run in a guest.** Nothing
changes unless `Dlss` is set or `conduit upscaler` is used.

Two routes:

- **DLSS on NVK** (NVIDIA hosts, RTX 20 to 50): NVIDIA's NGX runtime runs the
  game's DLSS DLL on our NVK. Needs NVK's CUDA module import (done), SASS for
  the GPU (shipped for RTX 20/30/40, host-compiled for RTX 50: done), the NGX
  runtime from the host driver (designed), and nvapi64 (designed).
- **OptiScaler** (any host GPU: NVIDIA, AMD, Intel): a per-game mod that takes
  a game's DLSS, FSR or XeSS calls and runs FSR or XeSS instead. Plain compute
  shaders on our D3D11/D3D12/Vulkan drivers, no NGX.

The viewer menu's **Upscaler in games** row and `conduit upscaler VM
default|dlss|fsr|xess` pick between them.

| Piece | State |
|---|---|
| NVK: CUDA cubins without libelf (patches-windows 0066) | done, tested on 1490 real cubins |
| NVK: `Dlss` / `NVK_DLSS` switch for VK_NVX_binary_import (patches-windows 0067) | done |
| DXVK: NVX extensions on Windows (`HELIOS_DXVK_NVX`, DXVK 0025) | done |
| NVK: a module's whole code, its globals and constant banks 2-4 (patches-common 0078) | done, compiles, not run on a GPU |
| NVK: SASS from the host-built cubin cache (patches-common 0079) | done, compiles; keys checked against the host tool on 240 real modules |
| Host tool `nvk-cubin-cache.py` (PTX to sm_120 SASS) | done; all 261 PTX-only modules of the 310.9.16 DLLs compile on the host, none with anything NVK refuses |
| `conduit upscaler`, ctl op `upscaler`, tray, viewer menu row | done, unit tests |
| `Install-OptiScaler.ps1` (per-game, user-downloaded OptiScaler) | written, not run |
| NGX runtime install (host's signed `nvngx.dll`, `_nvngx.dll`) | designed below |
| nvapi64 for NGX (dxvk-nvapi with a Windows adapter seam) | designed below |
| D3D11/D3D12 DLSS (NGX through the app's D3D device) | blocked: needs the nvapi object mapping |
| DLSS frame generation, Multi Frame Generation | blocked: HAGS, Reflex markers, flip metering |

## How DLSS reaches the GPU

NVIDIA's NGX runtime (`nvngx.dll` loader, `_nvngx.dll` core) loads the game's
`nvngx_dlss.dll`. That DLL holds the DLSS kernels as CUDA fatbins: SASS
(machine code) per SM version, plus PTX. In a Vulkan game NGX imports the
kernels and launches them on the game's own `VkDevice`:

- `VK_NVX_binary_import`: `vkCreateCuModuleNVX` (fatbin, cubin or PTX),
  `vkCreateCuFunctionNVX`, `vkCmdCuLaunchKernelNVX`.
- `VK_NVX_image_view_handle`: bindless handles and addresses of image views.

Upstream NVK implements both (`nvk_cubin.c`, `src/nouveau/cubin/`). NVK cannot
compile PTX, so it uses only SASS built for the GPU's exact SM version.

## Which GPU gets which code

Checked with `nvk-cubin-cache.py scan` on the newest DLSS SDK release,
310.9.16 (2026-10-16):

| DLL | SASS | PTX |
|---|---|---|
| `nvngx_dlss.dll` (super resolution) | sm_75, sm_80 (163 of 169 modules), sm_86, sm_89 | sm_80, sm_89 |
| `nvngx_dlssd.dll` (ray reconstruction) | sm_75 to sm_90; sm_120 for 24 of 59 modules | sm_89 |
| `nvngx_dlssg.dll` (frame generation) | sm_89 for 32 of 71 modules | sm_89; sm_120 for 32 |

- **RTX 20 (sm_75), RTX 30 (sm_86), RTX 40 (sm_89):** the DLL has SASS for the
  GPU, NVK uses it. The cubin cache is never read.
- **RTX 50 (sm_120):** no release so far has sm_120 SASS for super
  resolution; NVIDIA's driver JITs the PTX. NVK takes the SASS from a cache
  the host compiled from that PTX (next section). The PTX-only modules of
  dlssd and dlssg get the same treatment.

The shipped SASS always wins: NVK looks at the cache only for a module whose
fatbin has no SASS for the GPU.

### Why a host-built cache

Two options were checked:

1. **Wait for a DLSS release with sm_120 SASS.** 310.9.16, 310.9.1 and the
   310.x releases before them have none for super resolution. NVIDIA ships
   PTX for Blackwell on purpose (the driver JIT), so this may never change,
   and every game pins its own DLL version anyway.
2. **Compile the PTX on the host.** The Linux host already has NVIDIA's PTX
   compiler in its driver (`libcuda.so.1`, `cuLinkCreate` with
   `CU_JIT_TARGET`), the same JIT the Windows driver runs for these DLLs. The
   result is a normal sm_120 cubin, which NVK loads like shipped SASS.

Option 2 is what is implemented: it works for every DLL version a game
ships, needs no NVIDIA binaries in the repository, and the user runs it on
their own files. Each module is compiled once.

## The cubin cache

```sh
# on the host; the DLLs come from the game (conduit cp win11:'G:\...\nvngx_dlss.dll' .)
guest/nvk-rm/tools/cubin-cache/nvk-cubin-cache.py scan nvngx_dlss.dll
guest/nvk-rm/tools/cubin-cache/nvk-cubin-cache.py build -o cubin nvngx_dlss.dll nvngx_dlssd.dll nvngx_dlssg.dll
# into the guest (as administrator): C:\ProgramData\Helios\ngx\cubin\
```

- `scan` lists, per DLL, the SASS and PTX per SM, the fatbins without SASS for
  sm_120, and anything in the shipped cubins NVK would refuse.
- `build` takes, for every fatbin without SASS for the GPU (`--sm`, default
  120), its newest PTX not newer than the GPU, compiles it and writes
  `<key>.sm_120.cubin` plus `index.tsv` (key, PTX SM, source DLL and version,
  kernels, problems). The key is the BLAKE3 hash of the PTX entry's bytes as
  the fatbin stores them (compressed or not), so NVK finds the cubin from the
  module data alone; a second file under the hash of the PTX text covers a
  module passed as PTX. Existing files are kept (`--force` recompiles).
- Compilers: `--compiler cuda` (libcuda's JIT; needs the NVIDIA GPU, makes a
  CUDA context for the run), `--compiler ptxas --ptxas PATH` (CUDA toolkit,
  no GPU; not tried), `auto` (ptxas if found, else libcuda). The libcuda
  path compiles position-independent code
  (`CU_JIT_POSITION_INDEPENDENT_CODE`): without it the JIT patches absolute
  return and call addresses into the instructions of 19 of the 261 modules
  (the SR input/output kernels that call `__cuda_sm20_*` math slow paths),
  which NVK cannot relocate; with it those calls go through an address in
  bank 4, which NVK fills (0078).
- `--limit N` compiles at most N modules (a quick check).
- Python 3.8+, standard library only. Tests:
  `python3 -m unittest -v test_nvk_cubin_cache` (in the tool's folder; CI runs
  them): BLAKE3 against the official vectors, the fatbin LZ4 entries,
  truncated fatbins, the cubin checks, the build with a fake compiler.

The output is NVIDIA code compiled for your machine: keep it there, do not
redistribute it.

On the host (RTX 5090, driver 610.57.04) the 261 PTX-only modules of the
three 310.9.16 DLLs compile in about a minute. Their cubins use banks 2-4,
module globals and subroutine calls through bank 4, all of which 0078 loads;
`scan`/`build` report none that NVK would refuse.

In the guest, NVK (0079) looks in `NVK_CUBIN_CACHE_DIR`, else the registry
`HKLM\SOFTWARE\Helios` `CubinCache` (REG_SZ), else
`%ProgramData%\Helios\ngx\cubin`. A hit logs `NVK: CUDA module sm_120 SASS
from the cubin cache (<key>, PTX sm_89)`; a miss logs the file it looked for.
`CubinCacheDump` = 1 (or `NVK_CUBIN_CACHE_DUMP=1`) also writes the missed
module to `%LOCALAPPDATA%\Helios\cubin-miss\<key>.fatbin`, which `build` takes
as input like a DLL: so a DLL NGX downloads itself (over-the-air models) can be
covered too.

### What NVK loads from a module

Shipped and JIT cubins read more than code and kernel parameters: of the 169
modules in `nvngx_dlss.dll` 310.9.16, 66 read the addresses of module globals
(`.nv.global`, `.nv.global.init`) from constant bank 4, filled through
`R_CUDA_64` relocations, and others read compiler constants from bank 2 or
`__constant__` data from bank 3. Upstream NVK binds only bank 0, so those
kernels read zeros. 0078 puts the globals and banks 1-7 of each module in one
GPU buffer, applies the relocations into them and binds the banks at launch.
A relocation of another type, or one in kernel code, fails the module with a
message rather than running with a wrong address (none of the shipped or JIT
cubins of 310.9.16 has one).

## The NGX runtime and its loader

We never ship NVIDIA DLLs. The host driver has signed Windows copies, the same
files Proton copies into its prefixes: `nvidia/wine/nvngx.dll` and
`_nvngx.dll` next to the driver's libraries (`/usr/lib/x86_64-linux-gnu/nvidia/wine/`
on Debian-style hosts). Both carry NVIDIA's Authenticode signature, which the
loader checks.

What the NGX loader in a game does (`NGXLoadCoreLibrary(LUID)` in the DLSS
SDK's `nvsdk_ngx_s.lib` 310.9.16, read from its code), in order, taking the
first `_nvngx.dll` that loads and passes the signature check:

1. next to the game's executable;
2. `D3DKMTQueryAdapterInfo` on the adapter with the device's LUID: the
   registry value `Parameters\NGXCore` `NGXPath` (then `NGXCore` `NGXPath`)
   under that adapter's driver service key, i.e. ours,
   `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`;
3. `HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\Parameters\NGXCore` `NGXPath`;
4. `HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\NGXCore` `NGXPath`;
5. `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore` `FullPath`.

The `PCI\VEN_10DE&CC_0300` SetupAPI probe in the same library is not on this
path (it belongs to driver-store helpers the core load does not call), so our
PCI identity does not stop NGX from loading. Install plan (not implemented):

- host: `conduit` copies both DLLs from the loaded driver's `nvidia/wine/`
  directory into the guest share and records the driver version;
- guest installer: copies them to `C:\ProgramData\Helios\ngx\` and writes
  `helios_kmd_render\Parameters\NGXCore` `NGXPath` and `FullPath` (step 2 and
  5) to that folder; copies again when the recorded host driver version
  changes.

NGX's own log: `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore` `LogLevel`
(DWORD 2), `EnableLogPathOverride` (1), `LogPath` (directory).

## nvapi64

`_nvngx.dll` asks NvAPI for the GPU and its architecture before it offers a
feature; without nvapi64 NGX reports `NVSDK_NGX_GPU_Arch_Unknown`. The guest
has no nvapi64. Plan: dxvk-nvapi built for Windows (it builds with MinGW
already) with Helios patches:

- **Vulkan without DXVK's DXGI.** dxvk-nvapi gets its `VkInstance` from
  `IDXGIVkInteropFactory` and each adapter's `VkPhysicalDevice` from
  `IDXGIVkInteropAdapter`, which only DXVK's `dxgi.dll` has; our guest runs
  Microsoft's DXGI. The seam: create its own instance from `vulkan-1.dll`
  (the `Vk(PFN_vkGetInstanceProcAddr, ...)` constructor exists) and match
  each DXGI adapter to the physical device whose
  `VkPhysicalDeviceIDProperties::deviceLUID` equals the adapter's LUID (NVK on
  RM reports the Helios adapter's LUID, patches-windows 0025).
- **Architecture.** dxvk-nvapi reports GB20x when the driver has
  `VK_KHR_compute_shader_derivatives` with `meshAndTaskShaderDerivatives`,
  else Ada for any device ID >= 0x2600. Until NVK reports that feature,
  `DXVK_NVAPI_GPU_ARCH=GB200` must be set on RTX 50 hosts, or NGX picks the
  Ada (sm_89) kernels and NVK refuses them.
- **Configuration from the registry.** dxvk-nvapi reads only environment
  variables, which native games do not get from us. `env::getEnvVariable`
  falls back to `HKLM\SOFTWARE\Helios\Nvapi` (REG_SZ per variable name), and
  `Upscaler` = `dlss` maps to `DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE=on` and
  `DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION=render_preset_latest`
  (DRS 0x10E41DF3 = 0xffffff), the NVIDIA app's "latest model" override.
- **Install opt-in only.** Many games treat an `nvapi64.dll` in System32 as
  "this is an NVIDIA GPU" and take NVIDIA-only paths; it goes in with `Dlss`.

D3D11 and D3D12 DLSS also go through nvapi: NGX calls
`NvAPI_D3D11/D3D12_CreateCubinComputeShader` and friends on the game's device,
which dxvk-nvapi forwards to `ID3D11VkExtDevice1` (DXVK) and `ID3D12DeviceExt`
(vkd3d-proton). In our guest the game's device is Microsoft's runtime object
and the DXVK / vkd3d-proton objects sit behind the UMD DDI, so that
`QueryInterface` fails. It needs a private UMD escape from the runtime's
device to the UMD's Vulkan device (blocked until designed). Vulkan games are
the first target.

## Vendor identity

DXGI reports the adapter as PCI vendor 0x1AF4 (virtio), from the PCI IDs
Windows enumerated. Most games offer DLSS only for vendor 0x10DE. NGX itself
does not check it (see above), the DLSS SDK sample does not either. For games
that do: OptiScaler's DXGI and Vulkan spoofing (on by default for non-NVIDIA
vendors), or a UMD-side vendor override (not designed yet).

## The upscaler setting

`conduit upscaler VM [default|dlss|fsr|xess]` sends `upscaler` over the
control channel ([GUEST-CONTROL.md](../../../docs/GUEST-CONTROL.md)); the
viewer menu's **Upscaler in games** row runs the same command
(`--vm-upscaler-cmd`). The Conduit tray in the guest:

- writes `HKLM\SOFTWARE\Helios` `Upscaler` (REG_SZ);
- with `dlss` also sets `Dlss` = 1 (NVK's VK_NVX_binary_import);
- rewrites the `OptiScaler.ini` of every game listed under
  `HKLM\SOFTWARE\Helios\OptiScaler` (REG_SZ per game: the ini's path):

| Mode | `Dx11Upscaler` | `Dx12Upscaler` | `VulkanUpscaler` | `[DLSS]` preset |
|---|---|---|---|---|
| `default` | auto | auto | auto | auto |
| `dlss` | dlss | dlss | dlss | override, K (11) for all modes |
| `fsr` | fsr31 | fsr31 | fsr31 | auto |
| `xess` | xess_12 | xess | xess | auto |

Games read it at their next start. FSR on DX11 uses OptiScaler's native DX11
FSR 3.1 rather than `fsr31_12` (D3D11on12), which needs resources shared
between our D3D11 and D3D12 drivers; XeSS on DX11 has only `xess_12` on
non-Intel GPUs, with the same caveat. `dlss` reaches the real DLSS only once
the NGX runtime and nvapi above are in place; until then it sets `Dlss` and the
preset for OptiScaler games with an NVIDIA host.

## OptiScaler

OptiScaler (github.com/optiscaler/OptiScaler) is **GPL-3.0**: Conduit neither
ships nor links it. The user downloads a release, extracts it (7-Zip) and runs,
in an elevated PowerShell in the guest:

```powershell
.\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Source "$env:USERPROFILE\Downloads\OptiScaler_0.9.4"
.\Install-OptiScaler.ps1 -Game 'G:\...\Game.exe' -Uninstall
```

(`guest/windows/packaging/windows/Install-OptiScaler.ps1`; copy it in with
`conduit cp`.) It copies the release next to the game as `dxgi.dll`
(`-Proxy winmm|version|...` when the game has its own), refuses to replace
files without `-Force`, lists what it copied in `conduit-optiscaler.txt` for
`-Uninstall`, and registers the game's `OptiScaler.ini` for the upscaler
setting. Not for online games with anti-cheat.

How it runs on our stack (from OptiScaler's documentation; not run yet):

- Our adapter is vendor 0x1AF4, so OptiScaler treats the GPU like AMD/Intel:
  DXGI spoofing on (games see an RTX 4090 and offer DLSS), its bundled
  fakenvapi (MIT) answers the game's NvAPI calls (Reflex becomes LatencyFlex
  or XeLL), and the game's DLSS calls go to FSR or XeSS. No NGX and no real
  nvapi needed.
- FSR 3.1 and XeSS are HLSL compute shaders: on DX12 through vkd3d-proton,
  DX11 natively through DXVK, Vulkan natively, all on NVK. XeSS on non-Intel
  GPUs uses its DP4a path (SM 6.4 dot products: vkd3d-proton maps them to
  `VK_KHR_shader_integer_dot_product`, which NVK has).
- FSR4 needs RDNA3/4 hardware; not on our GPUs.
- OptiFG (OptiScaler's frame generation, DX12 only) and its FSR3-FG, XeFG
  outputs are compute plus a swapchain hook; their present pacing on our
  composed/flip present path is untested.
- `dlss` as output needs the real NGX (above).

## DLSS 4: newest model and frame generation

| Feature | Needs | On our stack |
|---|---|---|
| Transformer model (presets J/K, 310.1+; L/M in 310.5+) | DLL 310.x, a preset override (NGX DRS or OptiScaler) | possible: override via `Upscaler` = `dlss` once nvapi is in; the preset is a DLL feature, no extra driver work |
| Super resolution / DLAA | above | Vulkan: needs NGX runtime + nvapi; D3D: the nvapi object mapping |
| Ray reconstruction (dlssd) | same, plus sm_120 SASS for 35 of 59 modules (cache) | as SR |
| Frame generation (dlssg) | D3D12 or Vulkan; hardware-accelerated GPU scheduling (HAGS) on Windows; Reflex latency markers (`NvAPI_D3D_SetLatencyMarker` / `VK_NV_low_latency2`); present timing through NGX's swapchain hooks; DLSS 4 FG estimates motion with its own network (no optical flow accelerator, so no `nvofapi64`) | blocked: our KMD has no HAGS, NVK has no `VK_NV_low_latency2`, dxvk-nvapi's markers need it |
| Multi Frame Generation (RTX 50, 3x/4x) | as FG, plus Blackwell flip metering (frames paced by the display engine) | blocked: needs flip metering in our KMD's present path and the host scanout |

Frame generation for games without an upscaler (NVIDIA Smooth Motion, AMD
AFMF) is a driver feature we do not have; the host-side viewer frame
generation covers that case and is not part of this work.

## Research note: "DLSS 5 neural rendering" on AMD

Community OptiScaler forks are reported to run a "DLSS 5 neural rendering"
model on AMD GPUs (D3D12, unofficial). Not checked or implemented here. What
it would need on our stack, in principle: the model's D3D12 shaders through
vkd3d-proton, which for matrix and cooperative-vector operations (SM 6.9
linear algebra / wave matrix) means mapping DXIL to
`VK_KHR_cooperative_matrix` or `VK_NV_cooperative_vector` on NVK, plus
whatever weights and DLLs the fork loads (licensing unclear). Nothing in this
series depends on it.

## Tests

Run:

- `nv_cubin_elf` (0066): synthetic cubins and 1490 real cubins (DLSS 3.5.10
  and 310.9.1) parse the same as with libelf, under ASan/UBSan.
- 0078 and 0079 compile (`-fsyntax-only`, `-Wall`) for x86_64 and i686 MinGW
  and native Linux.
- 0078's ELF reader, placement and relocation pass (extracted into a test
  program, ASan/UBSan) accept all 1021 shipped cubins of the three 310.9.16
  DLLs (sm_75 to sm_120) and all 522 host-compiled cubins; every address it
  writes points into the module's data or code.
- 0079's fatbin walk and Mesa's BLAKE3 give the same key as the host tool for
  all 240 PTX modules of `nvngx_dlss.dll` and `nvngx_dlssg.dll` 310.9.16.
- `nvk-cubin-cache.py build --compiler cuda` on the host (RTX 5090, driver
  610.57.04): all 261 PTX-only modules of the 310.9.16 DLLs, sm_120, nothing
  NVK refuses.
- The tool's unit tests; viewer `test-ui` and `test-vmpause` (the Upscaler
  row and its runner); `conduit-ctl`, CLI and tray unit tests (the `upscaler`
  op, mode words, OptiScaler.ini rewriting).
- The series applies in build-windows.sh order; DXVK patches 0001-0022 + 0025
  apply; `gen-windows-knobs.py --check` passes.

Not run: anything in a guest; NVK with these patches on a GPU; the tray on
Windows; `Install-OptiScaler.ps1`.

## Guest test plan (Vulkan first)

1. Install a driver package built from this branch. `Dlss` absent: nothing
   changes (`vulkaninfo` lists no `VK_NVX_binary_import`).
2. `conduit upscaler win11 dlss` (sets `Dlss` = 1); a new process:
   `vulkaninfo --summary` lists `VK_NVX_binary_import` and
   `VK_NVX_image_view_handle`; `MESA_LOG_FILE` shows `NVK: DLSS on`.
3. RTX 50 host: build the cubin cache from the game's DLLs and copy it to
   `C:\ProgramData\Helios\ngx\cubin\`. RTX 20/30/40 hosts skip this.
4. NGX runtime and nvapi64 installed (plans above), NGX logging on.
5. The DLSS SDK sample (github.com/NVIDIA/DLSS) in Vulkan mode with the DLSS
   indicator (`NGXCore` `ShowDlssIndicator` = 1024). Success: the NGX log
   shows the feature created, the indicator shows "DLSS SR", NVK logs cache
   hits (RTX 50) and no `Fatbin does not have compatible bytecode`, and DLSS
   Quality runs faster than native at the same output resolution.
6. A Vulkan game with DLSS (DOOM: The Dark Ages, No Man's Sky); then the
   OptiScaler route on a DX12 game: install it, `conduit upscaler win11 fsr`,
   start the game, its DLSS option should run FSR (OptiScaler overlay: Insert).
