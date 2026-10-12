# DLSS on NVK (Vulkan route)

Status: **experimental, off by default, not yet run in a guest.** This page
covers what the NVK side implements, what DLSS still needs outside NVK, and
how to test it.

## How DLSS reaches the GPU

NVIDIA's NGX runtime (`nvngx.dll` loader, `_nvngx.dll` core) loads the
game's `nvngx_dlss.dll`. That DLL holds the DLSS kernels as CUDA fatbins:
SASS (machine code) per SM version, plus PTX. In a Vulkan game NGX imports
the kernels and launches them on the game's own `VkDevice`:

- `VK_NVX_binary_import`: `vkCreateCuModuleNVX` (fatbin or cubin),
  `vkCreateCuFunctionNVX`, `vkCmdCuLaunchKernelNVX`.
- `VK_NVX_image_view_handle`: bindless handles and addresses of the game's
  image views.

Upstream NVK implements both (`nvk_cubin.c`, `src/nouveau/cubin/`, Mesa
26.2). NVK cannot compile PTX, so it uses only the SASS for the GPU's exact
SM version (`NVK_EXPERIMENTAL=dlss_backwards_compat` also accepts a lower
minor version of the same major).

## What our series adds

| Patch | What |
|---|---|
| `patches-windows/0064` | `nv_cubin_elf.c`: cubin parsing without libelf (there is no libelf for MinGW; 0016's stub refused every module). A bounds-checked ELF64 reader under nv_cubin's interface. The module keeps its own copy of the image, so it does not depend on `pData` after `vkCreateCuModuleNVX` returns. |
| `patches-windows/0065` | `NVK_DLSS=1/0`, else the registry value `HKLM\SOFTWARE\Helios` `Dlss` (DWORD), adds or removes `dlss` in `NVK_EXPERIMENTAL`. Native Vulkan games get no environment from us, so the registry value is the switch in a guest. Default off. With DLSS on, instance creation logs `NVK: DLSS on (VK_NVX_binary_import)`. |
| DXVK `0023` | DXVK drops the NVX extensions outside Wine (always on Windows). `HELIOS_DXVK_NVX=1` keeps them in 64-bit processes. The UMD bridge sets it for NVK processes when `Dlss` is 1. This matters only for D3D11 DLSS, which also needs the nvapi object mapping (not done). |

vkd3d-proton already enables both extensions unless `VKD3D_CONFIG=no_nvx`.

### Tests

`src/nouveau/cubin/tests/nv_cubin_elf_test.c` (meson test `nv_cubin_elf`,
built with `-Dbuild-tests=true`) builds `nv_cubin_elf.c` with its functions
renamed, so with libelf the same program also runs upstream's `nv_cubin.c`
and compares the results field by field, including the code bytes:

- a synthetic cubin with two kernels (both `.nv.info` formats, kernel
  parameters, NOBITS shared memory, a local function and a data object that
  must be skipped), its old-ABI and extended-section-numbering variants, an
  unaligned buffer, and every truncation of it;
- twelve broken images that must be refused (bad magic, ELF32, big-endian,
  not EM_CUDA, section headers or data past the end, a bad `e_shstrndx`, a
  function past its section or in a missing section, a truncated `.nv.info`
  record, a missing `.nv.info.<kernel>`, symbol `entsize` 0);
- given files, every CUDA ELF embedded in them.

Standalone, with ASan and UBSan, from a Mesa tree with the series:

```sh
R="-Dnv_cubin_module_init=nv_cubin_elf_module_init -Dnv_cubin_module_fini=nv_cubin_elf_module_fini \
   -Dnv_cubin_module_find_function=nv_cubin_elf_module_find_function \
   -Dnv_cubin_function_get_param_info=nv_cubin_elf_function_get_param_info"
CF="-O1 -g -fsanitize=address,undefined -Isrc -Iinclude -Isrc/nouveau/cubin"
gcc $CF $R -c src/nouveau/cubin/nv_cubin_elf.c -o elf.o
gcc $CF -c src/nouveau/cubin/nv_cubin.c -o libelf.o
gcc $CF -DUSE_LIBELF src/nouveau/cubin/tests/nv_cubin_elf_test.c elf.o libelf.o -lelf -o nv_cubin_elf_test
./nv_cubin_elf_test path/to/nvngx_dlss.dll path/to/nvngx_dlssd.dll
```

Result on the DLLs at hand: 1490 cubins (nvngx_dlss 3.5.10 and 310.9.1,
nvngx_dlssd and nvngx_dlssg 310.9.1, sm_75 to sm_120) parse the same as with
libelf, and all synthetic cases pass. The parser also builds with
x86_64-w64-mingw32-gcc.

## What DLSS still needs outside NVK

In the order a test would hit them:

1. **NGX runtime from the host driver.** We never ship NVIDIA DLLs. The
   host's own driver has signed copies, the same files Proton copies into
   its prefixes: `/usr/lib/x86_64-linux-gnu/nvidia/wine/nvngx.dll` and
   `_nvngx.dll` (Authenticode-signed by NVIDIA, which NGX's loader checks).
   Proposed install step, run by the user on their machine:
   - host: `conduit` copies both files from the directory of the loaded
     `libGLX_nvidia.so.0` (`nvidia/wine/` next to it, as Proton's
     `find_nvidia_wine_dll_dir()` does) into the guest share, and records
     the host driver version;
   - guest installer: copies them to `C:\ProgramData\Helios\ngx\` and to
     `System32` (as Proton does), and writes where the NGX loader in the
     DLSS SDK 310.9 looks (strings in `nvsdk_ngx_s.lib`):
     `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore` `FullPath` (REG_SZ,
     the directory), and under the KMD's service key
     `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`:
     `NGXCore` `NGXPath` and `Parameters\NGXCore` `NGXPath` (the value the
     loader asks dxgkrnl for through `D3DKMTQueryAdapterInfo`
     `KMTQAITYPE_QUERYREGISTRY` / `D3DDDI_QUERYREGISTRY_SERVICEKEY`);
   - copy again when the host driver changes (the guest compares the
     recorded version).
   Which of these the loader takes first is **unknown**. It also enumerates
   `PCI\VEN_10DE&CC_0300` / `CC_0302` through SetupAPI; with our PCI
   identity that step finds nothing, and whether it falls back is also
   unknown.
   NGX's own log: `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore`
   `LogLevel` (DWORD 2), `EnableLogPathOverride` (1), `LogPath` (directory).
2. **nvapi64.dll.** `_nvngx.dll` asks NvAPI for the GPU and its
   architecture (`NvAPI_GPU_GetArchInfo` and others). The guest has no
   nvapi64 today. dxvk-nvapi with the adapter seam (Vulkan LUID instead of
   `IDXGIVkInteropAdapter`, GB20x arch) is the planned answer. Without it
   NGX logs `GPUArchitecture == NVSDK_NGX_GPU_Arch_Unknown` and DLSS stays
   unavailable.
3. **SASS for sm_120 in the DLSS DLL.** NVK runs only SASS built for the
   exact GPU. Checked with the test above:

   | DLL | SASS | PTX |
   |---|---|---|
   | `nvngx_dlss.dll` 3.5.10 (super resolution) | sm_75, sm_86 | sm_80 |
   | `nvngx_dlss.dll` 310.9.1 (super resolution, DLSS SDK) | sm_75, sm_80, sm_86, sm_89 | sm_80, sm_89 |
   | `nvngx_dlssd.dll` 310.9.1 (ray reconstruction) | sm_75 to sm_90, sm_120 for 24 of 59 modules | sm_89 |
   | `nvngx_dlssg.dll` 310.9.1 (frame generation) | sm_86, sm_89 | sm_89, sm_120 |

   So DLSS needs a DLL of version 310 or later, but **no super resolution
   DLL checked so far has Blackwell (sm_120) SASS**. On NVIDIA's driver,
   Blackwell runs the sm_89 PTX through the driver's JIT, which NVK does not
   have. On an RTX 5090 every SR module fails with "Fatbin does not have
   compatible bytecode" until a DLSS release ships sm_120 SASS (or NGX's
   over-the-air model updates do; not checked). Turing to Ada GPUs have
   SASS, so NVK's path can work there.
4. **Vendor identity.** Most games offer DLSS only when DXGI reports vendor
   0x10DE. The DLSS SDK sample does not check.

D3D11/D3D12 DLSS also needs the app-object to UMD-object mapping in nvapi;
frame generation needs hardware scheduling (HAGS), which the KMD does not
have.

## Guest test plan (Vulkan)

Not run yet. Order:

1. Install a driver package built from this branch. `Dlss` absent: nothing
   changes (`vulkaninfo` shows no `VK_NVX_binary_import`).
2. `reg add HKLM\SOFTWARE\Helios /v Dlss /t REG_DWORD /d 1 /f`, then a new
   process: `vulkaninfo --summary` lists `VK_NVX_binary_import` and
   `VK_NVX_image_view_handle` on the NVK device, and `MESA_LOG_FILE` shows
   `NVK: DLSS on (VK_NVX_binary_import)`.
3. NGX runtime installed (step 1 above), nvapi64 present, NGX logging on.
4. The DLSS SDK sample (github.com/NVIDIA/DLSS, MIT sample code; the
   `nvngx_dlss.dll` it ships may be redistributed with applications under
   the DLSS SDK license) in Vulkan mode, DLSS indicator on (`NGXCore`
   `ShowDlssIndicator` = 1024, or `DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS`).
   Success: the NGX log shows the feature created; the indicator shows
   "DLSS SR ..."; NVK logs no `Fatbin does not have compatible bytecode` or
   `Incompatible cubin module`; fps at the same output resolution is higher
   with DLSS Quality than native. On an RTX 5090 with the 310.9.1 DLL, the
   expected result today is the bytecode error from item 3.
5. A Vulkan game with DLSS (DOOM Eternal / The Dark Ages, No Man's Sky):
   needs the vendor identity too.
