# DLSS and other upscalers in Windows guests

Status: **experimental, off by default, not yet run in a guest.** Nothing
changes unless `Dlss` is set or `conduit upscaler` is used.

Two routes:

- **DLSS on NVK** (NVIDIA hosts, RTX 20 to 50): NVIDIA's NGX runtime runs the
  game's DLSS DLL on our NVK. Needs NVK's CUDA module import (done), SASS for
  the GPU (shipped for RTX 20/30/40, host-compiled for RTX 50: done), the NGX
  runtime from the host driver and nvapi64 (both done, `Install-Dlss.ps1`).
  Vulkan games first; D3D12 through the D3D12 driver's engine-object
  meta-commands; D3D11 not yet.
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
| NVK: a module's whole code, its globals and constant banks 2-4 (patches-common 0084) | done, compiles, not run on a GPU |
| NVK: SASS from the host-built cubin cache (patches-common 0085) | done, compiles; keys checked against the host tool on 240 real modules |
| Host tool `nvk-cubin-cache.py` (PTX to sm_120 SASS) | done; all 261 PTX-only modules of the 310.9.16 DLLs compile on the host, none with anything NVK refuses |
| `conduit upscaler`, ctl op `upscaler`, tray, viewer menu row | done, unit tests |
| `Install-OptiScaler.ps1` (per-game, user-downloaded OptiScaler) | written, not run |
| NGX runtime install (host's signed `nvngx.dll`, `_nvngx.dll`): `Install-Dlss.ps1` / `Uninstall-Dlss.ps1` | written, not run |
| nvapi64 for NGX: dxvk-nvapi + `third_party/patches/dxvk-nvapi` 0001-0004, `tools/build-dxvk-nvapi.sh` | builds (MinGW, 64-bit), not run |
| D3D12 DLSS: engine-object meta-commands in the D3D12 driver (`umd12/src/forward12/dlss12.rs`) + nvapi 0004 | done, host `cargo check`, not run |
| D3D11 DLSS | blocked: no channel from the runtime's objects to DXVK's (below) |
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
  bank 4, which NVK fills (0084).
- `--limit N` compiles at most N modules (a quick check).
- Python 3.8+, standard library only. Tests:
  `python3 -m unittest -v test_nvk_cubin_cache` (in the tool's folder; CI runs
  them): BLAKE3 against the official vectors, the fatbin LZ4 entries,
  truncated fatbins, the cubin checks, the build with a fake compiler.

The output is NVIDIA code compiled for your machine: keep it there, do not
redistribute it.

On the host (RTX 5090, driver 610.57.04) the 261 PTX-only modules of the
three 310.9.16 DLLs compile in about a minute. Their cubins use banks 2-4,
module globals and subroutine calls through bank 4, all of which 0084 loads;
`scan`/`build` report none that NVK would refuse.

In the guest, NVK (0085) looks in `NVK_CUBIN_CACHE_DIR`, else the registry
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
kernels read zeros. 0084 puts the globals and banks 1-7 of each module in one
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

Where the NGX loader in a game (`nvsdk_ngx_s.lib`, linked into the game)
is expected to look for `_nvngx.dll`, in order, taking the first that loads
and passes the signature check (the expected order, to be confirmed with
Process Monitor in the guest: filter on `_nvngx.dll` and on `NGXCore`):

1. next to the game's executable;
2. `D3DKMTQueryAdapterInfo` on the adapter with the device's LUID: the
   registry value `Parameters\NGXCore` `NGXPath` (then `NGXCore` `NGXPath`)
   under that adapter's driver service key, i.e. ours,
   `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`;
3. `HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\Parameters\NGXCore` `NGXPath`;
4. `HKLM\SYSTEM\CurrentControlSet\Services\nvlddmkm\NGXCore` `NGXPath`;
5. `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore` `FullPath`.

Whether our PCI identity (virtio, not `PCI\VEN_10DE`) matters to the
loader is also to be confirmed in the guest.

`Install-Dlss.ps1` (guest, elevated) covers steps 1, 2 and 5: it copies both
DLLs to `C:\ProgramData\Helios\ngx\` (and next to the game with `-Game`),
and writes `NGXPath` under `helios_kmd_render\NGXCore` and
`helios_kmd_render\Parameters\NGXCore` and `FullPath` under
`NVIDIA Corporation\Global\NGXCore`. Every change is recorded in
`C:\ProgramData\Helios\ngx\conduit-dlss.json`; `Uninstall-Dlss.ps1` undoes
it. The files come from the host (`/usr/lib/x86_64-linux-gnu/nvidia/wine/`
on Debian-style hosts) through a share; copy them again after a host driver
update.

NGX's own log: `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore` `LogLevel`
(DWORD 2), `EnableLogPathOverride` (1), `LogPath` (directory);
`ShowDlssIndicator` (1024) draws the on-screen DLSS indicator.
`Install-Dlss.ps1 -Log` sets them (logs to `C:\ProgramData\Helios\logs\dlss`).

## nvapi64

`_nvngx.dll` asks NvAPI for the GPU and its architecture before it offers a
feature; without nvapi64 NGX reports `NVSDK_NGX_GPU_Arch_Unknown`. The guest
gets DXVK-NVAPI (MIT), built on the host with
`guest/windows/tools/build-dxvk-nvapi.sh` (pinned upstream commit, MinGW,
`--arch 64|32|both`; NGX is 64-bit only) with
`guest/windows/third_party/patches/dxvk-nvapi`:

| Patch | What |
|---|---|
| 0001 | `DXVK_NVAPI_*` settings also from `HKLM\SOFTWARE\Helios\Nvapi` (REG_SZ or DWORD named like the variable; the environment wins); log lines also to OutputDebugString |
| 0002 | Microsoft's DXGI has no DXVK interop: DXVK-NVAPI creates its own Vulkan 1.1 instance and matches each DXGI adapter to the physical device with the same `deviceLUID` (NVK on RM reports the Helios adapter's LUID, patches-windows 0025); reports that device's PCI IDs (0x10de:...) instead of the virtio adapter's |
| 0003 | Architecture: NVK has no `meshAndTaskShaderDerivatives`, so device IDs >= 0x2900 on NVK report GB200 (Blackwell), and NGX picks its sm_120 path |
| 0004 | D3D12: reaches vkd3d-proton's `ID3D12DeviceExt` / `ID3D12GraphicsCommandListExt` through the D3D12 driver's meta-commands (next section) |

`nvapi64.dll` goes next to the game (`Install-Dlss.ps1 -Game`) or into
System32 (`-SystemNvapi`). Many games treat an NvAPI as "this is an NVIDIA
GPU" and take NVIDIA-only paths, so it is per game by default. Useful values
under `Helios\Nvapi`: `DXVK_NVAPI_LOG_LEVEL` (`info`), `DXVK_NVAPI_LOG_PATH`
(folder for `nvapi64.log`), `DXVK_NVAPI_DRIVER_VERSION` (NVK reports 999.99),
`DXVK_NVAPI_GPU_ARCH`, `DXVK_NVAPI_DRS_SETTINGS` (DLSS preset overrides, e.g.
`0x10E41DF3=0xffffff` for the latest SR preset).

## D3D12 and D3D11

NGX's D3D paths call `NvAPI_D3D12_CreateCubinComputeShader*`,
`NvAPI_D3D12_LaunchCubinShader`, `NvAPI_D3D12_GetCudaTextureObject` and the
D3D11 equivalents on the game's device and command list. Under Proton the
game's objects are vkd3d-proton's / DXVK's, and DXVK-NVAPI reaches their
`...Ext` interfaces with `QueryInterface`. In our guest the game holds
Microsoft's runtime objects; vkd3d-proton and DXVK sit behind the WDDM user
mode drivers, and the runtime never forwards an unknown `QueryInterface`.

**D3D12.** Meta-commands are the IHV channel the D3D12 runtime does forward.
With `Dlss` = 1 the D3D12 driver publishes two (`umd12/src/forward12/dlss12.rs`):

| Meta-command | Parameters (UINT64) | Effect |
|---|---|---|
| `HeliosEngineDevice` `{f4c98fb7-...}` | creation: `Magic`, `Out` | writes the engine (vkd3d-proton) `ID3D12Device*` to `*Out` |
| `HeliosEngineCommandList` `{8c8f8982-...}` | creation: `Magic`; execution: `Magic`, `Out` | writes the list's engine `ID3D12GraphicsCommandList*` to `*Out`, records nothing |

DXVK-NVAPI 0004 uses them when the `QueryInterface` fails and caches the
result on the runtime object (as upstream does). The engine objects live as
long as the runtime objects, and the driver records every command on the
engine list on the calling thread, so a cubin launch lands in order.
Descriptor handles need no mapping: the driver hands the runtime vkd3d's own
handle values (`forward12/descriptors.rs`). vkd3d-proton enables
`VK_NVX_binary_import` / `VK_NVX_image_view_handle` when NVK offers them,
which `Dlss` does. To verify in the guest: that the runtime passes the CPU
descriptor handles to the app unchanged, and the cubin 64-bit path
(`SupportsCubin64bit`, NVX spec versions).

**D3D11.** No such channel: the D3D11 runtime has no meta-commands, and
NGX's D3D11 calls pass runtime SRVs, UAVs, samplers and resources, which
would each need mapping to DXVK's. A possible route (not designed further):
bind the object to a slot so the driver sees its DDI handle, read the DXVK
object back through a device-dependent counter's `GetData`. Until then D3D11
games use OptiScaler.

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
- 0084 and 0085 compile (`-fsyntax-only`, `-Wall`) for x86_64 and i686 MinGW
  and native Linux.
- 0084's ELF reader, placement and relocation pass (extracted into a test
  program, ASan/UBSan) accept all 1021 shipped cubins of the three 310.9.16
  DLLs (sm_75 to sm_120) and all 522 host-compiled cubins; every address it
  writes points into the module's data or code.
- 0085's fatbin walk and Mesa's BLAKE3 give the same key as the host tool for
  all 240 PTX modules of `nvngx_dlss.dll` and `nvngx_dlssg.dll` 310.9.16.
- `nvk-cubin-cache.py build --compiler cuda` on the host (RTX 5090, driver
  610.57.04): all 261 PTX-only modules of the 310.9.16 DLLs, sm_120, nothing
  NVK refuses.
- The tool's unit tests; viewer `test-ui` and `test-vmpause` (the Upscaler
  row and its runner); `conduit-ctl`, CLI and tray unit tests (the `upscaler`
  op, mode words, OptiScaler.ini rewriting).
- The series applies in build-windows.sh order; DXVK patches 0001-0022 + 0025
  apply; `gen-windows-knobs.py --check` passes.

- dxvk-nvapi 0001-0004 apply to the pinned commit and build with MinGW
  (`build-dxvk-nvapi.sh`); the D3D12 driver with `dlss12.rs` passes
  `tools/umd12-host-check.sh`.

Not run: anything in a guest; NVK with these patches on a GPU; the tray on
Windows; `Install-OptiScaler.ps1`, `Install-Dlss.ps1`; DXVK-NVAPI on Windows.

## Guest test plan

[dlss-test.md](dlss-test.md): Vulkan game first, then D3D12.
