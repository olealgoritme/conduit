# Windows guests on NVK-on-RM: goal, state and plan

The goal: the whole Windows desktop and every app and game in a Windows guest
render through Mesa's NVK driver talking to the host's NVIDIA RM through
Conduit (NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko), with
zero-copy presentation, smooth frame pacing and performance at bare-metal
level. Venus (Vulkan command encoding replayed on the host) is removed once
every workload has an NVK path.

This file is the working plan. It is updated as stages land; the numbers are
measurements, with the setup named.

## Where it stands (2026-10-06, evening)

Measured in the win11 guest (5120x1440@240) through the installed driver
package 22.22.320.1 (KMD v320), NVK chosen per process unless noted.

| | Venus path (today's default) | NVK-on-RM |
|---|---|---|
| Unigine Heaven, D3D11, 1600x900 Medium, through the installed UMD | 171 fps | 377 fps (render thread 2.6% kernel time; native OpenGL Heaven on the host ~350 fps) |
| D3D11 spin test: zero-copy scanout / composed by DWM | 7900–8800 fps (Venus) | 4400–4900 / 9600–13500 fps |
| D3D12 triangle + compute, 1280x720, zero-copy present | 900–1270 fps | 1100–4450 fps |
| OpenGL gears (wgl, Zink), 1280x720 | 775–873 fps | 4200–4350 fps |
| H.264 decode through the UMD's D3D11 video DDI | | 5 of 6 clips bit-exact (custom scaling matrices unsupported by NVK) |
| Cross-process shared surfaces and keyed mutex | pass | pass (releaser CPU wait; the GPU-ordered hand-off ledger is being fixed) |
| Live driver replacement | DWM and the shell survive (device-lost handling in the Venus ICD and the UMD) | |
| DWM on NVK (`DwmIcd=nvk`, `ForeignFlip=1`) | | runs; its first frames reach the screen with zero copies, then it stalls on flip completion (KMD v321 fix) |

What made NVK fast, in the order it was found:

1. A host-visible VRAM heap (BAR): DXVK's per-frame buffers left system
   memory. Heaven 94 → 297 fps.
2. Zero-copy present through the KMD's scanout source, block-linear swapchain
   images: 297 → ~350 fps; the demo doubled.
3. RM backend fixes measured on the host against NVIDIA's driver: cacheable
   GPU mappings of system memory, compression, ZCULL, release builds.
4. NVK per-draw cost (patches-common 0001–0007): from 2.2–5.5x NVIDIA's to
   0.89–1.24x, bit-identical output.
5. RM forwarding: a CPU map + unmap went 52 ms → 0.4 ms (QEMU patch 0008);
   channel open/close 2.2 ms → 0.11 ms.
6. The installed UMD stopped sending frames NVK already flipped to scanout
   through DWM's redirection blit as well (a full-frame copy per frame on the
   render thread): Heaven 198 → 377 fps.

## Architecture

```
D3D11/D3D12 app ─► Helios UMD (DXVK / vkd3d-proton) ─► NVK (Mesa) ─► librmclient
                                                              │ HELIOS_ESCAPE_NVRM
                                                       Helios KMD (WDDM)
                                                              │ virtio (vhost-user)
                                                       conduit-backend ─► nvidia.ko (RM)
present: NVK swapchain image ─► KMD SCANOUT_SET/PRESENT ─► ScanoutFlip ─► dma-buf ─► viewer
```

The Helios KMD stays: Windows reaches a display device only through a WDDM
kernel driver. On this path it forwards RM calls (~0.7 µs inside it) and
flips frames.

## Stages

| Stage | What | State | Branch |
|---|---|---|---|
| S1 | Heaven via app-local DXVK on NVK | done | `spike/heaven-dxvk-nvk`, `feat/nvk-rm-bar-heap` |
| S2 | Zero-copy present for NVK apps | done (linear and block-linear; three scanout images) | `feat/nvk-rm-windows-wsi`, `feat/nvk-rm-wsi-blocklinear` |
| S3 | The Helios D3D11 UMD runs DXVK on NVK, global with a deny-list, Venus fallback | done, in the combined package | `feat/umd-nvk-combined` |
| S4 | RM fences as the present boundary | done | `feat/s4-rm-fences` |
| S5 | D3D12 on NVK (vkd3d-proton in UMD12) | done (NVK and Venus) | `feat/umd-nvk-combined` |
| S6a | Cross-process shared surfaces and keyed mutex on NVK | done with a releaser CPU wait; GPU-ordered hand-off ledger in progress | `fix/s6-handoff-ledger` |
| S6b | DWM on NVK | DWM runs on NVK; KMD `ForeignFlip` flips its buffers zero-copy; flip completion for every foreign flip in KMD v321 | `feat/dwm-on-nvk`, KMD `worktree-kmd-start-debug` |
| S6c | Shrink the deny-list (each entry exists only because of NVK↔Venus sharing) | system processes, task manager and RGB video players can move now; the shell needs 8 bpp shared ids, browsers need NV12/P010/fp16 ids (guest side done, KMD v321) | `feat/nvk-share-formats`, `docs/dwm-on-nvk.md` |
| S6d | Venus removed | after S6c; DXR titles stay on Venus until NVK has ray tracing | |

Measured and decided on the way: the KMD's own RM-memory desktop primary
(level 5) works (allocation, map, flips) but is only shown before DWM starts,
so it does not carry the desktop; DWM opens no KMD-made surfaces, so
RM-backed GDI redirection is not needed; the KMD authors 128-byte-aligned
pitches for anything NVK must import.

Decisions taken: NVK is global with a deny-list; the Helios DXVK fork is used;
vkd3d's FL12 conservative-raster check is relaxed for NVK; the KMD has its own
RM client; KMD builds are installed live.

## Workloads outside D3D11 and the desktop

| Workload | Plan |
|---|---|
| D3D12 games | S5, done |
| Native Vulkan games | NVK registered per adapter by the driver package (done, `feat/helios-nvk-package`) |
| OpenGL apps | Zink on NVK as the adapter's OpenGL ICD (done; OpenGL 4.6) |
| Video decode/encode (browsers, players) | H.264 decode through the UMD's D3D11 video DDI on NVK works (bit-exact); browsers also need NV12/P010 shared ids (S6c) |
| Ray tracing (DXR, VK_KHR_ray_tracing) | NVK has none; these stay on Venus until upstream adds it |
| Fullscreen exclusive, multi-monitor, HDR, sleep/resume | Test once the desktop is on NVK |

## Performance work still open

- Venus present path: Heaven at 171 fps through the UMD (snapshot/WindowedBlt route), not yet profiled.
- MSI-X for the GPU device (3 vectors are exposed; the KMD path is merged but
  dormant) to cut the ~55 µs per RM call.
- DXVK's own shader translation at start-up (~0.6 s hitch on every launch;
  NVK's compiled shaders are now cached on disk, patch 0026).
- RM window: 4 GiB by default (`feat/rm-window-size`) so many NVK processes fit
  their BAR heaps; per-device quota of a quarter of the window in the KMD; NVK
  falls back to system memory when a map is refused (patch 0025).

## Stability work still open

- NVK crash at process exit when the hand-off ledger is on (imported-memory
  lifetime, near `nvkmd_mem_unref`); timing-dependent.
- Venus ICD: a NULL object on the D3D device teardown path after a device
  loss (seen once at a live swap).
- SearchHost.exe crash loop in the guest (a C++ exception in Windows' search
  UI; not in the Helios drivers as far as the logs show).
- A full host disk pauses the VM; after the resume the backend's NVIDIA side
  needs a full restart.

## NVK patch stack (Windows)

Canonical branch: `nvk-rm/integration`.

| Patches | What |
|---|---|
| `patches/0001–0013` | the RM backend (Linux and shared) |
| `patches-windows/0014–0021` | Windows build, librmclient events and host pages, Win32 WSI, zero-copy present (0021) |
| `0022` | host-visible VRAM heap (BAR) |
| `0023` | S3: Helios adapter LUID, `helios_icd_interface_v2`, resource ids |
| `0024` | block-linear swapchain images on the scanout |
| `0025` | system-memory fallback when a BAR map is refused |
| `0026` | Mesa disk shader cache on Windows |
| `0027–0029` | cached system-memory GPU mappings, compression, ZCULL |
| `0030` | S4 fences (in progress) |
| `0031` | S6 shared surfaces (in progress) |
| `patches-windows-dxvk/0001–0004` | i686 build fix, no present-wait advertised, R/B in the GDI path, wait knobs |

## How changes get tested

- Driver-only updates: build from a verified worktree, check the boot-stack
  frame sizes (ceiling 17936 B), install live with `pnputil /install`, wait for
  the new DriverVersion at InitStg 7, run the RM, pin, event, scanout and
  import smoke tests (`guest/rmclient/tests`).
- Host changes (backend, QEMU, conduit-venus): one checked restart that
  verifies shutdown, the binaries, the backend's flags, conduit-venus, SSH and
  the Helios mode.
- NVK changes: `guest/nvk-rm/tests` (vk_summary, compute, offscreen, BAR,
  coherence, block-linear readback, scanout present), then Heaven 1600x900
  Medium with the same 30 s window after warm-up and host `nvidia-smi dmon`.
- A watchdog polls the guest for reboots and new crash dumps while anything
  runs there.
