# ngx-smoketest

A Windows x64 console program that runs DLSS super resolution through
NVIDIA's NGX runtime on synthetic inputs, without a game. It tests the guest's
DLSS stack ([dlss.md](../../../nvk-rm/docs/dlss.md)) one step at a time:
the NGX loader finding `_nvngx.dll`, NGX on NVK (Vulkan) or on the D3D12
driver, feature creation, the DLSS kernels running, and the result.

It links `nvsdk_ngx_s.lib`, the static NGX library games link, so the loader
path (signature check, registry lookup) is the one a game takes.

## What it does

| Step | What |
|---|---|
| environment | prints where `nvngx_dlss.dll`, `nvapi64.dll`, `_nvngx.dll` are and the NGXCore / Helios registry values |
| NGX required extensions (vk) | `NVSDK_NGX_VULKAN_RequiredExtensions`, `..._GetFeatureInstance/DeviceExtensionRequirements` |
| vkCreateInstance, physical device, vkCreateDevice (vk) | every device (name, vendor/device id, driver, LUID), enables what NGX asks for and prints what is missing; checks the `VK_NVX_*` entry points |
| DXGI adapters, D3D12CreateDevice (d3d12) | every adapter (name, vendor/device id, LUID) |
| NGX feature requirements | `..._GetFeatureRequirements`: FeatureSupported, MinHWArchitecture |
| NGX init | `..._Init_with_ProjectID` (or `..._Init` with `--appid`), verbose NGX logging; then lists which `_nvngx.dll`, `nvngx_dlss.dll`, `nvapi64.dll` the process loaded |
| NGX capability parameters | `SuperSampling.Available`, `NeedsUpdatedDriver`, `MinDriverVersionMajor/Minor`, `FeatureInitResult` |
| render size | DLSS optimal settings for the output size and mode |
| resources | color RGBA8 (gradient, checkerboard, disc, thin lines), depth R32F, motion vectors RG16F (zero), output RGBA8 cleared to magenta (255,0,255) |
| create DLSS feature | `NGX_*_CREATE_DLSS_EXT`, submitted and waited for |
| evaluate | N evaluations, Halton jitter, reset on the first; GPU time per evaluation (timestamp queries), CPU time of the EvaluateFeature call and of submit to fence |
| readback | output to the CPU: FNV-1a checksum, sample pixels next to the input's, min/max/mean, mean difference to a nearest-neighbour upscale of the input; `ngx-output-<api>.bmp` and `ngx-input-<api>.bmp` in the log folder |
| shutdown | ReleaseFeature, DestroyParameters, Shutdown1, API objects |

With `--image FILE` (PNG, BMP, JPEG: anything the Windows Imaging Component
reads) the image is the color input of every frame instead of the pattern,
with the same jitter sequence. The render size is the image's size; the
output is the mode's upscale of it (Quality: 1.5x) unless `--out` is given.
Besides the `.bmp` files the log folder then gets `ngx-output-<api>.png`,
`ngx-input-<api>.png` and `ngx-bilinear-<api>.png` / `.bmp`, a plain
bilinear upscale of the input to the output size to compare with, and the
output check prints the mean difference to it.

## CUDA kernel checks (`--cutest`)

`--cutest cutest.ptx` (Vulkan only) skips NGX and runs the small CUDA
kernels of [cutest.ptx](cutest.ptx) through `VK_NVX_binary_import` on the
device, each checked against the values it must produce: the launch
constants (`blockDim`, `gridDim`), a kernel parameter, module globals and
`__constant__` data, a local array on the stack (also through its generic
address), shared memory (shared and generic addresses), texture fetches
and filtered lookups through `vkGetImageViewHandleNVX` handles with two
samplers, surface loads and stores, `cp.async`, `ldmatrix` and `mma.sync`.
Parameters go both ways NGX passes them (`pParams` and the packed
`CU_LAUNCH_PARAM_BUFFER_POINTER`). A failing check names the values that
are wrong.

NVK has no PTX compiler: compile the module for the GPU on the host first,
with the same tool as the DLSS cubin cache, and point NVK at the result:

```sh
guest/nvk-rm/tools/cubin-cache/nvk-cubin-cache.py build -o cutest-cache guest/windows/tools/ngx-smoketest/cutest.ptx
```

```bat
set NVK_CUBIN_CACHE_DIR=C:\ngx-smoketest\cutest-cache
ngx-smoketest.exe --cutest C:\ngx-smoketest\cutest.ptx
```

(A `.cubin` file is passed to NVK as is.)

Every NGX call prints its result code and name. The output check fails if
the output is all black, one constant value, or still the magenta clear
color (DLSS never wrote it).

It never waits forever: every fence wait has a timeout (`--timeout-ms`), and
a watchdog ends the run after `--watchdog-s`. A crash prints the exception,
the module and offset, and ends the process (no error dialog). After the
`RESULT:` line the process ends with `TerminateProcess` (NGX and the API
objects are shut down by then), so DLL unload code cannot hang the exit.

## Exit code

| Code | Meaning |
|---|---|
| 0 | every step succeeded (`RESULT: PASS`) |
| 1 | at least one step failed (`RESULT: FAIL ... first: '<step>'`); later steps still run where they can |
| 2 | bad arguments |
| 3 | a fence wait timed out, the device was lost/removed, or the watchdog fired; ends at once, no cleanup |
| 4 | crash (exception code, module+offset printed) |

## Output

Lines are `[ok]   <step>: ...`, `[FAIL] <step>: ...`, `[warn] <step>: ...`,
indented detail lines, and a last `RESULT:` line. NGX log lines containing
error/fail/warn are echoed as `  NGX| ...` (`-v`: all of them); all of them
go to `<logdir>\ngx-callback-<api>.log`. NGX also writes its own logs into
the log folder (and to the NGXCore `LogPath` when `Install-Dlss.ps1 -Log`
set it).

## Options

```
ngx-smoketest.exe [--api vk|d3d12] [--frames N] [--out WxH] [--in WxH]
                  [--quality perf|balanced|quality|ultraperf|ultraquality|dlaa]
                  [--preset default|J|K|L|M] [--logdir DIR] [--appid N | --project-id GUID]
                  [--timeout-ms N] [--watchdog-s N] [--device N] [-v]
                  [--image FILE]
ngx-smoketest.exe --cutest cutest.ptx|FILE.cubin [--device N]
```

Defaults: Vulkan, 100 frames, output 1920x1080, Quality, render size from
NGX's optimal settings (`--in` sets it; `--quality perf` gives 960x540),
log folder `.\ngxlog`, 10 s per fence wait, 300 s for the whole run, the
first device (Vulkan) or first hardware adapter (D3D12). `--preset` sets
the DLSS render preset hint for every mode. The NGX identity is a fixed
project id (engine type CUSTOM) unless `--appid` is given.

## Build (Linux host)

```sh
git clone --depth 1 --filter=blob:none --sparse https://github.com/NVIDIA/DLSS.git
git -C DLSS sparse-checkout set include lib/Windows_x86_64
NGX_SDK=$PWD/DLSS guest/windows/tools/ngx-smoketest/build.sh /tmp/ngx-smoketest
```

Needs clang, lld and the LLVM binutils (`llvm-ar`, `llvm-nm`,
`llvm-objcopy`, `llvm-dlltool`), the MinGW-w64 headers and import libraries
(`gcc-mingw-w64-x86-64`) and the Vulkan headers. The output folder gets
`ngx-smoketest.exe` and the SDK's `nvngx_dlss.dll`.

`nvsdk_ngx_s.lib` is an MSVC library built against the static UCRT. The
script links it with the MinGW tool chain as follows (details in
`build.sh`): the library's objects are extracted, their C runtime calls are
renamed and imported from `ucrtbase.dll` through a generated import library
(so the NGX code keeps the UCRT it was built for, and our code keeps
msvcrt), their linker directives are dropped, and `msvc_rt_shim.c` supplies
the few MSVC runtime pieces `ucrtbase.dll` does not export (stack cookie,
`__GSHandlerCheck*`, the CFG check, thread-safe statics, MSVC-mangled
`operator new`/`delete`, `std::_Xbad_alloc`/`_Xlength_error`). lld does
the link; a missing symbol is a link error. Vulkan is loaded at run time
(`vulkan-1.dll`), so no Vulkan import library is needed.

## Run in the guest

The folder with `ngx-smoketest.exe` needs `nvngx_dlss.dll` (from the build)
and the NGX runtime and `nvapi64.dll` set up by
`guest/windows/packaging/windows/Install-Dlss.ps1`. With the folder at
`C:\ngx-smoketest` and the runtime files (`nvngx.dll`, `_nvngx.dll`,
`nvapi64.dll`, on RTX 50 also the cubin cache) in `Z:\dlss`, elevated
PowerShell:

```powershell
.\Install-Dlss.ps1 -NgxSource Z:\dlss -CubinCache Z:\dlss\cubin -Game C:\ngx-smoketest\ngx-smoketest.exe -Log
cd C:\ngx-smoketest
.\ngx-smoketest.exe --api vk --frames 100 --logdir C:\ngx-smoketest\ngxlog
.\ngx-smoketest.exe --api d3d12 --frames 100 --logdir C:\ngx-smoketest\ngxlog
```

`Install-Dlss.ps1` sets `HKLM\SOFTWARE\Helios` `Dlss` = 1 (NVK's
`VK_NVX_*` extensions, the D3D12 driver's DLSS meta-commands); for Vulkan
`$env:NVK_DLSS = '1'` does the same for one process. In an elevated session
the Vulkan loader ignores `VK_DRIVER_FILES`; `VK_DIRECT_DRIVER=<path to
vulkan_nouveau.dll>` hands it NVK directly, as in the NVK-on-RM tests. The
cubin cache is the one built from the same `nvngx_dlss.dll` version
(`guest/nvk-rm/tools/cubin-cache/nvk-cubin-cache.py build`).
