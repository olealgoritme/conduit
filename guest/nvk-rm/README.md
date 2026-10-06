# NVK on RM

A first cut of an NVK backend that drives the GPU through NVIDIA's Resource
Manager (RM, the open kernel modules' `/dev/nvidiactl` interface, forwarded
by Conduit in a guest) instead of nouveau. It is a patch series against
upstream Mesa that adds a second implementation of NVK's kernel abstraction,
`src/nouveau/vulkan/nvkmd/rm/`, next to `nvkmd/nouveau/`. It talks to RM
through [librmclient](../rmclient), which it loads at runtime.

Status: **runs on an RTX 5090 (GB202, GSP firmware, RM 610.57.04) in the
`lab` guest.** `vulkaninfo` enumerates the GPU through NVK, a compute test
(storage buffer, dispatch, device-local buffer + copy, 5000 back-to-back
submits) passes, and `vkcube` presents **zero-copy** on the guest desktop
through Mesa's native X11 (DRI3/Present via Xwayland) and Wayland
(linux-dmabuf) WSI: the swapchain images stay in VRAM, block-linear with
NVIDIA DRM format modifiers, and go to the compositor as dma-bufs through
Conduit's nvidia-drm node (see "Zero-copy presentation"). See "First run"
and "Zero-copy run" at the end for what was run and what is still open.
dEQP has not been run yet.

Design background: `docs/research/nvk-rm.md` (on the `feat/nvk-rm` branch).

## Base and patches

Mesa `main` at **`70c4c018cbe5b78a1db7e9413bc7e511b366fd95`**
("lavapipe: drop LVP_SNORM_BLEND workaround", 2026-10-05).

| # | patch | what |
|---|---|---|
| 1 | `nvk: prepare for an nvkmd backend that is not DRM` | split physical-device creation from DRM probing; instance `enumerate` hook (`nvkmd_enumerate_non_drm_pdevs`, falls back to DRM); `copy_sync_payloads` only with a DRM fd; fd/dma-buf external extensions and non-software WSI only with `has_dma_buf`; new `nvkmd_info::has_host_visible_vram`; export `nouveau_ws_device_info_init_limits()`. No change for nouveau. |
| 2 | `nvk/rm: build glue, RM API subset and librmclient loader` | `-Dnvk-rm` option, `nvrm/nvrm_api.h` (RM classes, params, controls and host methods copied from open-gpu-kernel-modules 610.57.04, MIT, with size asserts), verbatim copy of `rmclient.h` as a fallback, `dlopen` loader |
| 3 | `nvk/rm: enumerate RM GPUs and fill in the physical device` | device/subdevice, `nv_device_info` from RM controls |
| 4 | `nvk/rm: logical device and memory` | per-device client, `FERMI_VASPACE_A`, usermode doorbell, non-stall event, VRAM / system memory |
| 5 | `nvk/rm: GPU VA allocation and binding` | NVK-chosen VAs via fixed `NV50_MEMORY_VIRTUAL`, `crm_map_dma2` with PTE kind |
| 6 | `nvk/rm: execution and bind contexts` | TSG + subcontext + GPFIFO channel + engine objects, doorbell submission |
| 7 | `nvk/rm: timeline syncs on RM semaphores` | `vk_sync` type on 64-bit semaphores in memory |
| 8 | `nvk/rm: fixes from the first run on an RTX 5090 (GB202, GSP)` | OS descriptors via `crm_alloc_os_descriptor`, RM's VA alignment rules, the device VA space, SYNC subcontext + channel bind, `cls_m2mf`, sync features |
| 9 | `nvk/rm: track GPFIFO progress with a semaphore; binary sync move` | ring progress without USERD GP_GET, `move` for binary syncs |
| 10 | `nvk/rm: report VA ranges at the size NVK asked for` | RM's 2 MiB rounding stays internal (`rm_size_B`); fixes the bind assert for images of 2 MiB and more (vkcube at 1280x720+) |
| 11 | `nvk/rm: stop polling a non-stall event whose data cannot be read` | a refused `NV_ESC_RM_GET_EVENT_DATA` leaves the event readable for good; waits sleep instead of spinning through refused escapes |
| 12 | `vulkan/wsi, nvk: wait for rendering before presenting without implicit sync` | `wsi_device::wait_before_present`, set by NVK when the backend has dma-bufs but no sync_file export (RM) |
| 13 | `nvk/rm: dma-buf export and import through nvidia-drm, DRM format modifiers` | `has_dma_buf` + `has_alloc_tiled` for RM: export/import of RM memory as dma-bufs via `OS_UNIX_EXPORT/IMPORT_OBJECT` and nvidia-drm's GEM import/export; DRM node discovery; `VK_EXT_image_drm_format_modifier` |

Each patch builds on its own.

## Build

```sh
guest/nvk-rm/build.sh                 # ~/code/mesa-nvk-rm, build dir build-rm
guest/nvk-rm/build.sh /path/to/mesa build-dir
```

The script clones Mesa if needed, checks out the base commit on a local
branch `nvk-rm`, applies the series with `git am` (skipped if already
applied), points meson at `guest/rmclient/include/rmclient.h` through a
throwaway `rmclient.pc`, and builds only NVK:

```sh
meson setup build-rm -Dvulkan-drivers=nouveau -Dgallium-drivers= \
    -Dnvk-rm=enabled -Dbuildtype=debugoptimized
ninja -C build-rm src/nouveau/vulkan/libvulkan_nouveau.so \
    src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json
```

By hand: `git am guest/nvk-rm/patches/*.patch` on the base commit, then the
two commands above. librmclient is not needed to build (only its header, and
a copy is in patch 2).

Build dependencies (Ubuntu 24.04; this is what the host needed on top of its
existing packages): meson >= 1.4 (`pip install meson`), `bindgen-cli` and
`cbindgen` (`cargo install`), rustc >= 1.85, `llvm-20-dev libclang-20-dev
libclang-cpp20-dev libpolly-20-dev libllvmspirvlib-20-dev llvm-spirv-20
libclc-20-dev` (for `mesa_clc`), `libxshmfence-dev`, plus the usual Mesa
deps (libdrm, libelf, wayland, xcb, glslang, python3-mako/yaml).

Verified on the host: the series applies to the base commit and builds, both
with `-Dnvk-rm=enabled` and without it (plain nouveau NVK).

## Windows build (cross-compiled, first bring-up)

NVK with the RM backend builds for **Windows x86_64** with MinGW-w64 on a
Linux host: `vulkan_nouveau.dll`, its ICD manifest and `librmclient.dll`.
With librmclient's real Windows transport (RM escapes through the Helios
KMD) it **runs on the RTX 5090 in the `win11` guest**: enumeration, compute
and offscreen rendering pass (see "First run on Windows" below); presenting
is the next step. Seven more Mesa patches on top of the 13 above, in `patches-windows/` (Mesa branch `nvk-rm-windows`):

| # | patch | what |
|---|---|---|
| 14 | `nvk/rm: event waits and host pages through librmclient, LoadLibrary on Windows` | the backend no longer calls `mmap`/`poll`: `crm_event_wait`, `crm_alloc_pages`/`crm_free_pages` (librmclient transport ABI 2), Linux compat fallback for an older librmclient; `LoadLibrary` of `librmclient.dll` next to the ICD |
| 15 | `vulkan/runtime: keep vk_image::drm_format_mod on every OS` | the field exists on Windows too (always `DRM_FORMAT_MOD_INVALID` there) |
| 16 | `nvk: build without libelf on Windows (no CUDA modules)` | `nv_cubin_nolibelf.c` |
| 17 | `nvk: driver build id without an ELF build-id note` | Mesa version + module timestamp (`disk_cache_get_function_identifier`), as dozen |
| 18 | `nak: leave nouveau's winsys and DRM out of the bindings on Windows` | only NAK's Linux hardware tests use them |
| 19 | `nvk: build for Windows with the RM backend only` | `with_nouveau_drm` (false on Windows): no nouveau winsys / `nvkmd/nouveau`; chipset limits split into `nouveau_device_limits.[ch]`; the RM backend's DRM side moved to `nvkmd_rm_drm.c` (Linux only, stubs otherwise); `VK_EXT_physical_device_drm` and DRM syncobj copies Linux only; empty `<sys/ioccom.h>` for `drm.h`; `TRUE`/`FALSE` from `<windows.h>`; `vulkan_nouveau.dll` with `vulkan_api.def` exports |
| 20 | `nvk: Win32 WSI` | `VK_KHR_win32_surface` + swapchain through Mesa's win32 WSI, as a software device (CPU copy per present) |

Linux behaviour is unchanged: the full series (20 patches) builds the Linux
NVK (nouveau + RM) as before, with the same `.so` exports; the patches apply
with `git am` on the base commit and give exactly branch `nvk-rm-windows`.

```sh
guest/nvk-rm/build-windows.sh            # ~/code/mesa-nvk-rm-windows, build dir build-win
guest/nvk-rm/build-windows.sh /path/to/mesa build-dir
MESA_CLC_DIR=/path/to/linux/build/bin guest/nvk-rm/build-windows.sh   # reuse host mesa_clc/vtn_bindgen2
```

The script applies `patches/` and `patches-windows/` on branch
`nvk-rm-windows` (skipped if already applied), builds the native
`mesa_clc` + `vtn_bindgen2` NVK's OpenCL kernels need (or takes them from
`MESA_CLC_DIR`, e.g. a Linux `build-rm`'s `src/compiler/clc` and
`src/compiler/spirv`), cross-builds `librmclient.dll` from `guest/rmclient`,
configures Mesa with `windows/mingw-x86_64.ini` and

```sh
meson setup build-win --cross-file guest/nvk-rm/windows/mingw-x86_64.ini \
    -Dvulkan-drivers=nouveau -Dnvk-rm=enabled -Dgallium-drivers= \
    -Dplatforms=windows -Dllvm=disabled -Dmesa-clc=system -Dprecomp-compiler=system \
    -Dvideo-codecs= -Dvulkan-layers= -Degl=disabled -Dgbm=disabled -Dglx=disabled \
    -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Dshader-cache=disabled \
    -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled -Dxmlconfig=disabled \
    -Dperfetto=false -Dbuild-tests=false -Dbuildtype=debugoptimized
```

and stages stripped DLLs, `nouveau_icd.json` (`library_path`
`.\vulkan_nouveau.dll`, relative to the manifest), `imports.txt` and
`exports.txt` in `BUILD_DIR/dist`. Compiles run under `systemd-run --user
--scope -p MemoryMax=2500M` with `-j2` by default (`MEMORY_MAX`, `JOBS`).

Host needs (Ubuntu 24.04): `gcc-mingw-w64-x86-64` (GCC 13, win32 threads),
`rustup target add x86_64-pc-windows-gnu`, meson >= 1.7, bindgen + libclang,
cbindgen; `wine64` only to run the tests. LLVM is off for the Windows build
(NAK is Rust and needs no LLVM; only the host `mesa_clc` does).

The cross file uses **`-mno-ms-bitfields`** (C, C++ and bindgen): MinGW
defaults to MSVC bitfield layout, bindgen only models the GCC one, and NAK
and NIR share structs with mixed-type bitfields between C and Rust (NAK
asserts `sizeof(struct nak_nir_tex_flags) == 4`, which fails with the MSVC
layout). Nothing that crosses the DLL boundary has mixed-type bitfields
(Vulkan API, RM parameter structs).

Result (debugoptimized):

| file | size (stripped / with DWARF) | exports | imports |
|---|---|---|---|
| `vulkan_nouveau.dll` | 18.3 MB / 145 MB | `vk_icdGetInstanceProcAddr`, `vk_icdGetPhysicalDeviceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion` | GDI32, KERNEL32, msvcrt, ntdll, USER32, USERENV, WS2_32, bcryptprimitives, api-ms-win-core-synch-l1-2-0 |
| `librmclient.dll` | 71 KB / 254 KB | every `crm_*` (MinGW auto-export) | KERNEL32, msvcrt |

No MinGW runtime DLL is needed (`-static-libgcc`, no libstdc++ or
winpthread). `windows/icd_smoke.c` loads the driver as the loader does
(`vk_icdNegotiate...`, instance extensions, `vkCreateInstance`,
`vkEnumeratePhysicalDevices`); under wine with `NVK_RM=1`:

```
vk_icdNegotiateLoaderICDInterfaceVersion: 0, interface version 7
vkCreateInstance: 0
MESA: warning: NVK_RM: crm_open failed: -40          # -ENOSYS: stub transport
vkEnumeratePhysicalDevices: 0, 0 physical devices
```

### First run on Windows (2026-10-06, `win11`, KMD 22.22.307.0, RTX 5090)

The series above, unchanged, with librmclient's real Windows transport
(`guest/rmclient`, `src/transport_windows.c`: RM escapes over
`HELIOS_ESCAPE_NVRM`, CPU mappings, OS-descriptor pinning and event waits
through the KMD). Files in one directory: `vulkan_nouveau.dll`,
`librmclient.dll`, `nouveau_icd.json`; the tests from
`windows/build-tests.sh`. Run with `NVK_RM=1`.

The Vulkan loader ignores `VK_DRIVER_FILES` / `VK_ICD_FILENAMES` (and
`VK_ADD_DRIVER_FILES`) in an elevated process, which an administrator's ssh
session is ("Loader is running with elevated permissions. Environment
variable VK_DRIVER_FILES will be ignored"): `vulkaninfo` there only shows
the registered Venus ICD. The tests therefore take
`VK_DIRECT_DRIVER=C:\...\vulkan_nouveau.dll` and hand the ICD to the loader
through `VK_LUNARG_direct_driver_loading` (exclusive mode,
`tests/vk_direct_driver.h`). From a normal (non-elevated) desktop session
`VK_DRIVER_FILES` works as on Linux.

| test | result |
|---|---|
| `icd_smoke.exe vulkan_nouveau.dll` (no loader) | 1 physical device: `NVIDIA GeForce RTX 5090 (NVK GB202) (0x10de:0x2b85)` |
| `vk_summary.exe` (loader 1.4.309) | NVK, API 1.4.363, driver `Mesa 26.3.0-devel`, conformance 1.4.3.0, heap 0 32146 MiB VRAM, heap 1 15356 MiB system (host-visible), queue family flags 0xf, 183 device extensions |
| `vk_compute_test.exe compute.spv` | `PASS: 4096/4096 values correct (host-visible)` |
| `vk_compute_test.exe compute.spv copy 1048576` | `PASS: 1048576/1048576 values correct (device-local + copy)` |
| `vk_offscreen_test.exe 5000` | `PASS: offscreen triangle 256x256, 5000 frame(s)`, 1.4 s in all (~0.2 ms per submit + fence wait); the readback is the expected RGB triangle |

No RM refusal in the host backend log for any of these (no
"not in the ABI profile", no failed `RM_ALLOC`/`RM_CONTROL`). The first run
after copying new binaries takes a few seconds longer (Defender scanning the
new files), later runs do not.

(and "librmclient could not be loaded" without the DLL next to the driver).
librmclient's unit tests (`test_unit.exe`, 300 checks) pass under wine.

### Linux-only code and how the Windows build handles it

| where | Linux-only thing | on Windows |
|---|---|---|
| `nvkmd_rm_lib.c` | `dlopen`/`dlsym` | `LoadLibrary`/`GetProcAddress`, `librmclient.dll` next to `vulkan_nouveau.dll` first |
| `nvkmd_rm_mem.c` | anonymous `mmap` + `MADV_DONTFORK` for OS-descriptor pages | `crm_alloc_pages` (librmclient: `VirtualAlloc`) |
| `nvkmd_rm_dev.c` | `poll()` on the non-stall event fd, `sched_yield` | `crm_event_wait` (stub: `-ENOSYS` → waits sleep with backoff), `thrd_yield` |
| `nvkmd_rm_pdev.c`, `_mem.c`, `_dev.c` | nvidia-drm node discovery (libdrm, `stat`), `/dev/nvidiactl` export fds, PRIME/GEM ioctls, `lseek` on dma-bufs | moved to `nvkmd_rm_drm.c`, not built; stubs: no dma-buf, no import, `has_alloc_tiled = false` (no DRM modifiers) |
| `nvkmd.c`, `nvkmd/nouveau/*`, `winsys/*` | nouveau DRM backend, libdrm | not built (`with_nouveau_drm`); `nouveau_device_limits.c` (chipset tables) is built |
| `nvk_physical_device.c` | `major()`/`minor()` of DRM nodes, `VK_EXT_physical_device_drm` | compiled out / extension off |
| `nvk_device.c` | `vk_drm_syncobj_copy_payloads` | compiled out |
| `nvk_instance.c` | ELF build-id | module timestamp hash |
| `nv_cubin.c` | libelf | stub, CUDA modules rejected |
| `nak_bindings.h` | `xf86drm.h`, nouveau winsys | left out (hardware tests only) |
| `nil.h` → `drm_fourcc.h` → `drm.h` | `<sys/ioccom.h>` | empty header in `src/nouveau/compat/win32` |
| `nvk_descriptor_table.c`, `nvk_device_memory.c` | `<sys/mman.h>` (unused) | dropped / `<unistd.h>` |

External memory/semaphore fd extensions were already gated on
`has_dma_buf` (patch 1), which is false on Windows. The WSI is Mesa's
win32 one (patch 20) as a software device, since there are no dma-bufs:
instance extensions under wine are `VK_KHR_surface`, `VK_KHR_win32_surface`,
`VK_KHR_get_surface_capabilities2`, the surface/swapchain maintenance ones and
the usual capability queries.

### What the KMD and the Windows transport must provide next

librmclient's `src/transport_windows.c` documents it per callback; in short:

1. **Escape ABI** (`D3DKMTEscape` to the Conduit adapter): a header
   `{ magic, version, channel, nr, size }` + the NV escape payload in place,
   RM status in the payload. The KMD copies nested user pointers (NVOS54
   params, NVOS21 alloc params, NVOS41 event data, ...) the way the Linux
   guest module does, and forwards to the host's RM.
2. **Channels**: open/close of the control channel and per-GPU channels,
   returning small integer ids; the KMD resolves those ids where payloads
   carry an fd (`register_fd.ctl_fd`, `nvos33_with_fd.fd`,
   `alloc_os_event.fd`, `NV0005_ALLOC_PARAMETERS.data`).
3. **CPU mappings** (`map_memory`/`unmap_memory`): `NV_ESC_RM_MAP_MEMORY`
   plus a mapping into the calling process (BAR1 doorbell, RM system memory
   such as USERD and error notifiers), returning the user address and RM's
   cookie. There is no channel `mmap` on Windows.
4. **OS descriptors**: `NV_ESC_RM_ALLOC_MEMORY` with a user VA from
   `VirtualAlloc` (`crm_alloc_pages`); the KMD pins it (`MmProbeAndLockPages`)
   and hands RM the page list.
5. **Events** (`event_wait`): a Win32 event per event channel that the KMD
   signals when RM posts the OS event (the non-stall interrupt NVK waits
   on), plus `NV_ESC_RM_GET_EVENT_DATA`. Without it NVK still works, with
   sleeping CPU waits.
6. **Presentation, zero-copy**: the Windows WSI in this build copies
   through the CPU (no dma-bufs). Zero-copy on Windows needs a
   shared-resource path instead: RM memory exported as an NT handle / D3DKMT
   shared allocation the compositor (DWM) can scan out or compose, and the
   WSI taught to use it (the dma-buf path of patch 13 is the Linux
   counterpart). That is the next design item after the transport.

Not tried yet: building on Windows itself (MSVC/clang-cl would need the
same NAK bitfield question answered with the MSVC layout), presenting
through the win32 WSI, `vkcube` (it cannot be pointed at NVK from an
elevated session, see above) and dEQP.

## Running (in a guest)

```sh
export NVK_RM=1                                  # opt in (see below)
export VK_ICD_FILENAMES=$PWD/build-rm/src/nouveau/vulkan/nouveau_devenv_icd.x86_64.json
export NVK_RMCLIENT_LIB=/path/to/librmclient.so  # default: librmclient.so.0, librmclient.so
vulkaninfo --summary
```

- `NVK_RM=1` is required: with NVIDIA's own driver stack installed, the RM
  backend would otherwise show the same GPU twice. Without it (or if
  librmclient or `/dev/nvidiactl` is missing) NVK falls back to its normal
  DRM/nouveau enumeration.
- librmclient's `crm_open` checks the kernel's RM version strictly against
  610.57.04; `CRM_RM_VERSION=any` relaxes that.
- The VM needs `--caps graphics`: NVK allocates the 3D class on every queue,
  compute included.
- `NVK_DEBUG=vm` prints what RM reported for the GPU (classes, VRAM, GPCs),
  the DRM node used for dma-bufs (with its page kind, kind generation and
  sector layout), every export and every VA operation;
  `NVK_DEBUG=push_sync,push_dump` syncs and dumps every submit.
- A GNOME session in a Conduit guest exports `VK_DRIVER_FILES` (NVIDIA's
  ICD), which takes precedence over `VK_ICD_FILENAMES`: from a desktop
  terminal set `VK_DRIVER_FILES` to NVK's ICD as well, or apps silently run
  on NVIDIA's driver.
- Presentation is zero-copy whenever an nvidia-drm node is found;
  `MESA_VK_WSI_DEBUG=sw` forces the software WSI (CPU copy per frame), and
  `NVK_RM_DMABUF=0` turns dma-bufs off altogether (no external memory
  extensions, software WSI).

In `lab`, `~/nvk-cube.sh` (a copy is `guest/nvk-rm/nvk-cube.sh`) does all of that (from ssh it picks the desktop
session's `DISPLAY=:0`, Xwayland auth and `wayland-0`):

```sh
~/nvk-cube.sh                      # vkcube --wsi xcb, zero-copy
~/nvk-cube.sh --wsi wayland        # Wayland, zero-copy
~/nvk-cube.sh --sw [...]           # software WSI, the first-run path
~/nvk-cube.sh --present_mode 0 --c 5000 --width 1920 --height 1080
```

## Zero-copy presentation

```
NVK (RM backend)                         guest kernel (conduit_gpu)            host
NV01_MEMORY_LOCAL_USER (VRAM) ──OS_UNIX_EXPORT_OBJECT_TO_FD──► fresh /dev/nvidiactl fd
          │                              (fd swapped for the backend's)
          └─DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY on renderD128 ──► forwarded ──► host nvidia-drm GEM object
                                         guest GEM proxy ◄───────────────────┘
                     PRIME_HANDLE_TO_FD ─► dma-buf (guest)
                                              │ DRI3 PixmapFromBuffers / zwp_linux_dmabuf_v1
                                              ▼
                     Xwayland / gnome-shell (NVIDIA EGL/GBM): PRIME_FD_TO_HANDLE,
                     GEM_EXPORT_NVKMS_MEMORY ─► RM import into their client: same VRAM
                                              │ composited frame, guest KMS flip
                                              ▼
                     ScanoutFlip ─► backend PRIME export ─► conduit-viewer (docs/SCANOUT.md)
```

- **Export** (`nvkmd_rm_mem.c`): on the first `vkGetMemoryFdKHR` the memory
  object is exported to a freshly opened `/dev/nvidiactl`
  (`NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD`, 0x3d05), nvidia-drm imports
  that descriptor (`DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY` with an
  `NvKmsKapiPrivImportMemoryParams`: block-linear or pitch, log2 GOBs per
  block from the image's tile mode) and the GEM handle is kept for the
  memory's lifetime, so every export is the same dma-buf. This is exactly
  how NVIDIA's own userspace hands RM memory to nvidia-drm, so Conduit's
  guest module and host backend already forward all of it (the fd inside
  the nested parameters is swapped for the backend's on both routes).
  Exportable system memory is RM's `NV01_MEMORY_SYSTEM`, never our own
  OS-descriptor pages.
- **Import**: `PRIME_FD_TO_HANDLE`, `DRM_NVIDIA_GEM_EXPORT_NVKMS_MEMORY` to a
  fresh `/dev/nvidiactl`, `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD`
  (0x3d06) into our client under a `crm_new_handle` handle, placement from
  `NV0041_CTRL_CMD_GET_SURFACE_INFO`; mapped into our VA space like our own
  memory. dma-bufs from other drivers fail at `GEM_EXPORT_NVKMS_MEMORY`.
- **DRM node** (`nvkmd_rm_pdev.c`): the DRM device at the GPU's PCI address
  (Conduit's PCI mirror `0010:01:00.0`, which RM also reports), named
  `nvidia-drm` by `DRM_IOCTL_VERSION`, answering `DRM_NVIDIA_GET_DEV_INFO`.
  Its render/primary dev_t become `VK_EXT_physical_device_drm`, so Mesa's
  WSI recognises the X server's DRI3 device and the compositor's
  linux-dmabuf main device as ours (no prime blit).
- **Modifiers**: `VK_EXT_image_drm_format_modifier` with NIL's list. For
  32 bpp color on GB202 that is `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1,
  2, 0x06, h)` = `0x030000000060601h` (h = log2 GOB height 0..5), plus
  LINEAR. nvidia-drm reports generic kind 0x06, kind generation 2, sector
  layout 1 for this GPU, which is what Conduit's guest KMS advertises and
  NVIDIA's EGL/GBM pick from, so the lists intersect; vkcube's 500x500
  images use h = 5 (`0x0300000000606015`). Kinds are still applied per
  mapping (the image's own VA); `has_alloc_tiled` only records the layout.
- **Sync**: RM attaches no fences to dma-bufs and our syncs cannot become
  sync_files yet, so the WSI waits on the CPU for each image's rendering
  before presenting it (`wait_before_present`, patch 12). The compositor's
  reads are ordered by the WSI's buffer release / Present idle events.
  vk_sync_binary's emulated sync_file import/export is removed for RM: the
  WSI took it for real support and failed the first acquire.

No guest kernel module change and no host change was needed.

## Zero-copy run (2026-10-05, `lab`, RTX 5090, GNOME Shell 50.1 on Wayland, Xwayland 24.1.10)

- `vkcube --wsi xcb` and `--wsi wayland` render correctly (an `xwd` of the
  window read back through Xwayland's glamor, i.e. NVIDIA's EGL importing
  NVK's block-linear dma-buf); `NVK_DEBUG=vm` shows the three swapchain
  images exported with kind 0x6, tile 0x50.
- No CPU copy: over 200 frames at 1920x1080, vkcube writes 23 KB in total to
  its sockets on the zero-copy path and 1.66 GB (one 8 MB `PutImage` per
  frame) with `--sw`.
- Frame rate, 5000 frames, uncapped present modes (IMMEDIATE on X11,
  MAILBOX on Wayland; IMMEDIATE is not offered on Wayland):

  | window | X11 zero-copy | X11 `--sw` | Wayland zero-copy | Wayland `--sw` |
  |---|---|---|---|---|
  | 500x500 | 3560 fps | 2200 fps | 5510 fps | 2290 fps |
  | 1920x1080 | 2900 fps | 630 fps | 4880 fps | 590 fps |
  | 3840x1400 | 3060 fps | 230 fps | 4090 fps | 240 fps |

  With FIFO, X11 runs at the 240 Hz display rate (229-237 fps) either way.
  Wayland FIFO paces at ~235 fps for a 500x500 window but ~60 fps for larger
  ones; NVIDIA's own Vulkan driver gets 55 fps in the same windows, so that
  is gnome-shell's frame-callback pacing, not NVK.
- `tests/vk_dmabuf_test.c`: device A fills a device-local exportable buffer
  and exports it, device B (a second RM client) imports the dma-buf and
  reads back all 1 Mi values; two exports are the same dma-buf. Passes
  (`cc -O1 tests/vk_dmabuf_test.c -lvulkan -o vk_dmabuf_test`, run with the
  environment above). The compute test still passes.
- Host: with `conduit view lab` open, the viewer shows the cube at 240 fps
  (`commit 240.0 fps ... 0 rejected`), all dma-buf ATTACH/COMMIT. Those are
  gnome-shell's composited frames: neither NVK's nor NVIDIA's vkcube gets
  direct scanout from gnome-shell here, even fullscreen, so NVK's buffer
  itself is not what the guest KMS flips; the whole chain still has no CPU
  copy.
- No refusals in the backend log, apart from `GET_EVENT_DATA` (escape
  0x52; the running backend predates the change that serves it): patch 11
  turns the ~5800/s of those into one per device.

## NVK on Blackwell (Mesa main)

GB20x is supported on main. `BLACKWELL_B` (0xCE97) is on NVK's conformant
list (`nvk_is_conformant()` in `nvk_physical_device.c`, together with Kepler
to Ada); Mesa has the class headers `clcd97/clce97` (3D), `clcdc0/clcec0`
(compute, QMD v5), `clc96f/clca6f` (GPFIFO), `clc9b5/clcab5` (copy);
`nouveau_device.c` maps chipset 0x1b0+ to SM 120 with its warp, block and
shared-memory limits; NAK has SM120 latencies and NIL has the Blackwell GOB
and tiling rules. An RTX 5090 (GB202, chipset 0x1b2) gets 3D 0xCE97,
compute 0xCEC0, copy 0xCAB5, GPFIFO 0xCA6F, usermode 0xC761 from this
backend's class selection.

## What is implemented (and how)

**Enumeration / physical device** (`nvkmd_rm_pdev.c`). One RM client per
GPU from `crm_gpu_count/crm_gpu_info`; `NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2`
for the device instance; `NV01_DEVICE_0` + `NV20_SUBDEVICE_0`; chipset from
`NV2080_CTRL_CMD_MC_GET_ARCH_INFO` (`architecture | implementation`); classes
from `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2` (highest one Mesa has headers
for); PCI ids/revision from `NV2080_CTRL_CMD_BUS_GET_PCI_INFO`, bus location
from `NV0000_CTRL_CMD_GPU_GET_PCI_INFO`; name from `GPU_GET_NAME_STRING`;
VRAM size and usage from `NV2080_CTRL_CMD_FB_GET_INFO_V2` (`HEAP_SIZE`,
`HEAP_FREE`; the values Conduit clamps to `--vram-limit-mib`); GPC/TPC from
`NV2080_CTRL_CMD_GR_GET_INFO_V2`; SM limits as nouveau computes them. Volta
or newer is required (doorbell submission). `kmd_info`: only
`has_get_vram_used`.

**Device** (`nvkmd_rm_dev.c`). Its own RM client per `VkDevice`; the
device's own VA space (64 KiB big pages), named through `FERMI_VASPACE_A`
with index `GPU_DEVICE`, as NVIDIA's driver does. Not a new
`FERMI_VASPACE_A`: under GSP, `VA_INTERNAL_LIMIT` pins RM's internal range
to [4 GiB, 4.5 GiB), which a GSP client reserves entirely for GSP-RM, so GR
context buffers have nowhere to go (3D object: `NV_ERR_NO_MEMORY`); and
`RESTRICT_RESERVED_VALIMITS` on the device is refused by GSP. RM's internal
mappings land wherever its allocator puts them; NVK's heap skips
[4 GiB, 4.5 GiB);
`*_USERMODE_A` (BAR1, write-only) mapped through the subdevice for the
doorbell; an `NV01_EVENT_OS_EVENT` on the FIFO non-stall interrupt via
`crm_event_open` (created without event data) plus
`NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION` (repeat); timestamps via
`NV2080_CTRL_CMD_TIMER_GET_TIME`.

**Memory** (`nvkmd_rm_mem.c`). CPU-mappable and GART memory: our own
anonymous pages wrapped in `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, so the CPU map
is the allocation itself (zero-copy, coherent, no host window space). It is
registered with `crm_alloc_os_descriptor` (`NV_ESC_RM_ALLOC_MEMORY` on the
GPU channel): RM takes a user address only on that route, `NV_ESC_RM_ALLOC`
of the class answers `NV_ERR_NOT_SUPPORTED`;
fallback `NV01_MEMORY_SYSTEM`. Device-local memory: `NV01_MEMORY_LOCAL_USER`,
64 KiB pages, no compression. No host-visible VRAM type is exposed
(`bar_size_B = 0`, `has_host_visible_vram = false`); an explicit
`VRAM | CAN_MAP` request would map through BAR1 with `crm_map_memory`. All
memory is coherent.

**VA** (`nvkmd_rm_va.c`). NVK keeps picking addresses from its
`util_vma_heap` ([2 MiB, 256 GiB) minus RM's internal range; replay heap
[256 GiB, 512 GiB); all below 2^40 as GPFIFO entries require). Each
`nvkmd_va` is an `NV50_MEMORY_VIRTUAL` at exactly that address
(`FIXED_ADDRESS_ALLOCATE`, `SPARSE` for sparse ranges; RM collisions are
retried elsewhere). Ranges follow RM's own alignment for a default page
size virtual allocation, or RM moves them: 64 KiB, and offset and size
aligned to 2 MiB from 2 MiB up (RM then uses huge pages). Binds are `crm_map_dma2` into it with
`DMA_OFFSET_FIXED`, 64 KiB PTEs for VRAM, snooped 4 KiB PTEs for system
memory and `PAGE_KIND_OVERRIDE` with the VA's PTE kind. RM unmaps whole
mappings only, so each VA tracks its mappings and partial unbinds unmap and
re-map the remainders.

**Exec contexts** (`nvkmd_rm_ctx.c`), after `nvidia-push-init.c`:
`KEPLER_CHANNEL_GROUP_A` (GR, our VA space) → `FERMI_CONTEXT_SHARE_A`
(SYNC, VEID 0: GSP refuses the 3D object on an async subcontext) → GPFIFO
channel (1024 entries; ring and push slots in our system pages; error
notifier + USERD in RM system memory) → `NVA06F_CTRL_CMD_BIND` (GR) → 3D /
compute / copy objects without parameters → `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`,
`GET_WORK_SUBMIT_TOKEN` → `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE`. Submission has
no RM call: GPFIFO entries (`NO_PREFETCH` → `SYNC_WAIT`), GP_PUT in USERD,
token to `usermode + 0x90`. GP_GET in USERD is not written back while the
channel runs (it stays 0), so every kick ends with a semaphore release
(no WFI) of a sequence number and the ring counts as read up to the newest
release that has landed. A full ring waits for that (10 s, then
`DEVICE_LOST`); a non-zero error notifier is `VK_ERROR_DEVICE_LOST`.

**Bind contexts**: synchronous; waits and signals on the CPU.

**Syncs** (`nvkmd_rm_sync.c`). Timeline `vk_sync` = 64-bit value in a
per-device pool of system memory; `vk_sync_binary` on top, with a `move`
added (the runtime needs it for binary semaphores in assisted mode). GPU signal:
`SEM_EXECUTE` release (64-bit, WFI) + `NON_STALL_INTERRUPT`; GPU wait:
`SEM_EXECUTE ACQ_STRICT_GEQ` with TSG switch. CPU wait: read the value, spin
briefly, then wait on the non-stall event fd with `crm_event_wait` (Linux: `poll()`; bounded at 10 ms per round, so
a lost wakeup costs at most that) or sleep with backoff without an event.
Event payloads are never needed, but the event has to be drained to re-arm:
a host that refuses `NV_ESC_RM_GET_EVENT_DATA` leaves it readable for good,
so after the first failed drain waits sleep with backoff instead of polling
(patch 11). `WAIT_PENDING` uses a
per-sync "highest submitted value". `WAIT_BEFORE_SIGNAL` is not advertised,
so Vulkan runs in assisted timeline mode (a submit thread holds back waits on
unsubmitted values instead of leaving GPU acquires spinning).

## What is stubbed or missing

| item | state | needs |
|---|---|---|
| dma-buf / opaque-fd memory export and import | done (see "Zero-copy presentation") | cross-driver import (a dma-buf nvidia-drm cannot name) is refused |
| external semaphore/fence fds, explicit sync | not supported (no handle types) | `NV_SEMAPHORE_SURFACE` + nvidia-drm's `SEMSURF_FENCE_*` (Conduit forwards them) for sync_files and syncobjs; then drop `wait_before_present` and use Wayland explicit sync / DRI3 syncobj |
| presentation | zero-copy (dma-buf + modifiers), CPU wait before each present | explicit sync, above |
| host-visible VRAM, BAR heap | off | Conduit's 1 GiB mapping window question |
| compression | off | comptags (`NVOS32_ATTR_COMPR_REQUIRED`) and compressed modifiers |
| transfer queue (async CE channel), video decode | off | a second TSG with `NV2080_ENGINE_TYPE_COPY(n)` |
| zcull info | not queried | `NV2080_CTRL_CMD_GR_GET_ZCULL_INFO` |
| fixed CPU maps, overmap (`VK_EXT_map_memory_placed`) | off | |
| sparse | code path present (`NVOS32_ALLOC_FLAGS_SPARSE`), unverified for an unprivileged client | test; NVK always advertises sparse binding |
| proper CPU waits | spin + `poll()` on the non-stall event; payload not read | fine as is; per-sync events via `NV_SEMAPHORE_SURFACE` waiters would avoid waking every waiter on every interrupt |
| device-lost detection while idle | only when a call touches the context | `NV2080_NOTIFIERS_RC_ERROR` event |

## librmclient

Used: the base contract plus `crm_map_dma2` (PTE kind), `crm_free_quiet`,
`crm_event_open/close/drain`, `crm_alloc_os_descriptor`, and for dma-buf
import `crm_new_handle/crm_release_handle`, and (patch 14) `crm_event_wait`
and `crm_alloc_pages/crm_free_pages`, so the backend itself never calls
`mmap` or `poll`. All additions are looked up with `dlsym` (`GetProcAddress`
on Windows) and optional: without `crm_map_dma2` images get the physical
(generic) kind, without events CPU waits sleep-poll, and on Linux a
librmclient without the patch 14 calls gets the same `poll`/`mmap` code from
`nvkmd_rm_lib.c`.

Still missing / wanted:

- **`crm_unmap_dma` has no size** (NVOS47 `size`). RM supports partial
  unmaps; the backend works around it by unmapping whole mappings and mapping
  the remainders back, which costs extra round trips on sparse unbinds. A
  `crm_unmap_dma2(..., gpu_va, size)` would remove that.
- **Event payloads**: not needed by this design.
- Nice to have: a versioned soname (`librmclient.so.0`) and an installed
  `rmclient.pc`, so distributions can ship it next to Mesa (the backend
  already tries `librmclient.so.0` first).

## Host changes needed

None in the allowlist: every class and control used is in the 610.57.04
table with the parameter sizes the backend sends (`NV01_ROOT_CLIENT`,
`NV01_DEVICE_0`, `NV20_SUBDEVICE_0`, `NV01_MEMORY_LOCAL_USER`,
`NV01_MEMORY_SYSTEM`, `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`,
`NV50_MEMORY_VIRTUAL`, `FERMI_VASPACE_A`, `KEPLER_CHANNEL_GROUP_A`,
`FERMI_CONTEXT_SHARE_A`, `BLACKWELL_CHANNEL_GPFIFO_B`, `BLACKWELL_B`,
`BLACKWELL_COMPUTE_B`, `BLACKWELL_DMA_COPY_B`, `BLACKWELL_USERMODE_A`,
`NV01_EVENT_OS_EVENT`; controls 0x205, 0x21b, 0x800292, 0x20801701,
0x20801801, 0x20800110, 0x20801303, 0x20801228, 0x20800403, 0x20800301,
0xc36f010a, 0xc36f0108, 0xa06c0101, 0xa06f0104; for dma-bufs 0x3d05,
0x3d06, 0x410110) and the `NV_ESC_RM_ALLOC_MEMORY` route for OS
descriptors; the nvidia-drm ioctls (`GET_DEV_INFO`, `GEM_IMPORT_NVKMS_MEMORY`,
`GEM_EXPORT_NVKMS_MEMORY`, PRIME) are the ones Conduit already serves for
NVIDIA's own userspace. Confirmed in the first run: nothing NVK sends is
refused. Operationally:

- the VM needs `--caps graphics`;
- worth checking in the first run: that the backend's OS-descriptor
  translation accepts the anonymous `MAP_PRIVATE | MAP_POPULATE` pages NVK
  passes (with `MADV_DONTFORK`), and that RM-allocated system memory used
  for USERD and the error notifier is mappable within the window budget
  (8 KiB per queue);
- the non-stall event stays readable unless `GET_EVENT_DATA` drains it; a
  backend built before `5a4b99c` refuses that escape (NVK then sleeps in
  CPU waits, patch 11), a newer one serves it.

## Test plan (Linux `lab` guest, RTX 5090)

Build on the host (`build.sh`), copy `build-rm/` and `librmclient.so` into
the guest, then:

1. **Enumeration**: `NVK_RM=1 NVK_DEBUG=vm vulkaninfo --summary` lists the
   GB202 through NVK with the classes above and the VRAM limit. Also check
   that without `NVK_RM` nothing changes (NVK declines, NVIDIA's ICD works).
2. **Device creation**: `vulkaninfo` (full) or any app reaching
   `vkCreateDevice`. This creates the VA space, the usermode doorbell, the
   TSG/channel/engine objects and runs `nvk_queue_init_context_state`, i.e.
   the first GPFIFO submission and the first semaphore wait. Watch
   `conduit logs lab` for allowlist refusals. Check with `NVK_DEBUG=vm` that
   no VA collides with RM's [4 GiB, 4.5 GiB).
3. **Submission**: `NVK_DEBUG=push_sync,push_dump` on a trivial compute
   dispatch; GP_GET must advance and the context semaphore must complete.
4. **Compute**: `dEQP-VK.compute.basic.*`, `dEQP-VK.api.smoke.*`,
   `dEQP-VK.synchronization2.basic.*`, then vkpeak / a SPIR-V compute sample.
5. **Errors**: a deliberately bad pushbuffer (invalid method) must give
   `VK_ERROR_DEVICE_LOST`, not a hang.
6. **Graphics**: `dEQP-VK.renderpass.*` subsets, then `vkcube` (software
   WSI, CPU copy).
7. **Stress**: GPFIFO wrap (more than 1024 submits without waiting), many
   fences (semaphore pool growth), sparse binding tests.

## First run (2026-10-05, `lab`, RTX 5090, RM 610.57.04 with GSP)

Setup: the `lab` guest (Ubuntu 26.04) at 3 GiB RAM because the host was
short of memory (win11 holds 20 GiB). Mesa was built on the host (Ubuntu
24.04, older glibc than the guest; `-Ddisplay-info=disabled` because the
guest's libdisplay-info soname differs), installed with `--destdir` and
copied to `~/nvk-prefix` in the guest; librmclient was built in the guest
with its Makefile. Environment:

```sh
export NVK_RM=1
export VK_ICD_FILENAMES=$HOME/nvk-prefix/share/vulkan/icd.d/nouveau_icd.x86_64.json
export NVK_RMCLIENT_LIB=$HOME/nvk-rm/rmclient/build-make/librmclient.so
```

The compute test is `tests/vk_compute_test.c`:

```sh
glslc --target-env=vulkan1.3 tests/compute.comp -o compute.spv
cc -O1 -g tests/vk_compute_test.c -lvulkan -o vk_compute_test
./vk_compute_test compute.spv [copy] [N]     # LOOPS=n, NOWAIT=1 for stress
```

Results:

- `vulkaninfo --summary`: `NVIDIA GeForce RTX 5090 (NVK GB202)`,
  `DRIVER_ID_MESA_NVK`, API 1.4.363, conformance version 1.4.3.0, PCI
  0x10de:0x2b85; `NVK_DEBUG=vm` shows 3D 0xce97, compute 0xcec0, copy
  0xcab5, GPFIFO 0xca6f, usermode 0xc761, 32146 MiB VRAM, 12 GPCs, 85 TPCs.
  Without `NVK_RM` NVK declines; with both ICDs one process sees NVIDIA's
  driver and NVK side by side.
- Compute test (one storage buffer, a 64-wide shader writing
  `i * 3 + 7`, read back): host-visible buffer (4096 and 1 Mi values),
  device-local buffer + `vkCmdCopyBuffer` (4096 and 16 Mi values),
  `NVK_DEBUG=push_sync`, 5000 submit + fence-wait round trips (0.22 s for the
  whole process) and 20000 submits without waiting: all pass.
- `vkcube --wsi xcb --c 20000` on Xvfb: renders the textured cube
  (screenshot checked), about 3500 frames per second, exits cleanly.
- No refusals in the backend log, no NVRM errors in the host kernel log
  once the fixes were in.

What failed on the way (all fixed in patches 8 and 9, plus
`crm_alloc_os_descriptor` in librmclient): OS descriptor via RM_ALLOC
(`NV_ERR_NOT_SUPPORTED`); `NV50_MEMORY_VIRTUAL` moved by RM's alignment;
the timeline sync type's binary-only feature (assert); GR context buffers
with no VA (`NV_ERR_NO_MEMORY`, `kgraphicsMapCtxBuffer`); 3D object on an
async subcontext (`NV_ERR_INVALID_OBJECT` from GSP); `cls_m2mf` 0 (Fermi
path, push assert); binary semaphores without `move` (assert); GPFIFO wrap
with GP_GET never written back (`DEVICE_LOST` after 1023 entries).

Still to do: dEQP-VK (not built: the host had about 2 GiB of memory to
spare), test plan steps 5 (bad pushbuffer → `DEVICE_LOST`) and 7 (sparse,
many fences), a Mesa build inside the guest. (The non-stall event does not
re-arm without `GET_EVENT_DATA`; found in the zero-copy run, patch 11.)
