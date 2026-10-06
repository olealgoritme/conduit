# Windows guests on NVK-on-RM: goal, state and plan

The goal: the whole Windows desktop and every app and game in a Windows guest
render through Mesa's NVK driver talking to the host's NVIDIA RM through
Conduit (NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko), with
zero-copy presentation, smooth frame pacing and performance at bare-metal
level. Venus (Vulkan command encoding replayed on the host) is removed once
every workload has an NVK path.

This file is the working plan. It is updated as stages land; the numbers are
measurements, with the setup named.

## Where it stands (2026-10-06)

| | Venus path (today's default) | NVK-on-RM |
|---|---|---|
| Unigine Heaven, D3D11, 1600x900 Medium | 139 fps | 357 fps mean, p99 ~5 ms (full NVK stack, zero-copy present, release) |
| Host, native OpenGL Heaven, for reference | | ~350 fps |
| `vk_scanout_present` demo, 1080p | | 11748 fps (block-linear, zero copies) |
| OpenGL gears (wgl), 1280x720 | 831 fps | 4148–4763 fps (Zink on NVK) |
| D3D12 triangle + compute, 1280x720 | (was broken; host fix pending) | ~5300 fps (vkd3d on NVK, standalone) |
| H.264 decode, 1080p (Vulkan Video) | | ~925 fps, bit-exact (patch 0035) |

What made NVK fast, in the order it was found:

1. A host-visible VRAM heap (BAR): DXVK's per-frame buffers left system
   memory. Heaven 94 → 297 fps.
2. Zero-copy present through the KMD's scanout source, block-linear swapchain
   images: 297 → ~350 fps; the demo doubled.
3. RM backend fixes measured on the host against NVIDIA's driver: cacheable
   GPU mappings of system memory, compression, ZCULL, release builds. On the
   host these bring NVK to parity with NVIDIA in every measured category; in
   Heaven at Medium they are within run-to-run noise (Heaven is GPU-bound).
4. NVK per-draw cost (patches-common 0001–0007): from 2.2–5.5x NVIDIA's to
   0.89–1.24x, bit-identical output.
5. RM forwarding: a CPU map + unmap went 52 ms → 0.4 ms (QEMU patch 0008, one
   memslot per shared-memory region; KMD v309 registry counters off the escape
   path); channel open/close 2.2 ms → 0.11 ms. An RM control still costs
   ~55 µs from the guest (INTx path; MSI-X is next).

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

| Stage | What | State | Owner / branch |
|---|---|---|---|
| S1 | Heaven via app-local DXVK on NVK | done (297 → ~350 fps) | `spike/heaven-dxvk-nvk`, `feat/nvk-rm-bar-heap` |
| S2 | Zero-copy present for NVK apps | done (linear and block-linear) | `feat/nvk-rm-windows-wsi`, `feat/nvk-rm-wsi-blocklinear` |
| S3 | The Helios D3D11 UMD runs DXVK on NVK for every app, global with a deny-list, Venus fallback | built; going in as one combined package with S4/S5/S6/packaging on KMD v314 | `feat/s3-umd-on-nvk` → `feat/umd-nvk-combined` |
| S4 | GPU fences instead of the CPU wait at present (RM semaphore-surface fences as the WDDM present boundary) | KMD marker in v313; UMD/NVK done (present thread 1228 → 73 µs per frame under load) | `feat/s4-rm-fences` |
| S5 | D3D12 on NVK (vkd3d-proton in the Helios UMD12); FL 12_0, SM 6.8, no DXR | built; standalone D3D12 on NVK works | `feat/s5-d3d12-on-nvk` |
| S6 | DWM on NVK, the KMD's own RM client, Venus removed | KMD RM client levels 1–2 pass in win11 (v312), levels 3–4 in v314; cross-process NVK sharing built (KMD v313 op + host msg 31); DWM design written | `kmd/rm-client*`, `feat/s6-shared-surfaces`, `feat/s6-backend` |

Decisions taken: NVK is global with a deny-list (not per app); the Helios DXVK
fork is used (built without its Venus paths on NVK); vkd3d's FL12
conservative-raster check is relaxed for NVK; the KMD moves to its own RM
client; KMD builds are installed live (an occasional reboot during the swap is
accepted).

## Workloads outside D3D11 and the desktop

| Workload | Plan |
|---|---|
| D3D12 games | S5 |
| Native Vulkan games | NVK registered per adapter by the driver package (done, `feat/helios-nvk-package`) |
| OpenGL apps | Zink on NVK as the adapter's OpenGL ICD (done; OpenGL 4.6) |
| Video decode/encode (browsers, players) | H.264 Vulkan Video decode on NVK works (0035); a DXVK D3D11VA decoder for browsers is in progress; until then video apps stay on the deny-list (Venus) |
| Ray tracing (DXR, VK_KHR_ray_tracing) | NVK has none; these stay on Venus until upstream adds it |
| Fullscreen exclusive, multi-monitor, HDR, sleep/resume | Test once the desktop is on NVK |

## Performance work still open

- NVK per-draw cost: 2–5x NVIDIA's (~25 ns vs ~6 ns per draw; dynamic UBO
  offsets and descriptor switches worst). `perf/nvk-per-draw`.
- MSI-X for the GPU device (3 vectors are exposed; the KMD path is merged but
  dormant) to cut the ~55 µs per RM call.
- DXVK's own shader translation at start-up (~0.6 s hitch on every launch;
  NVK's compiled shaders are now cached on disk, patch 0026).
- RM window: 4 GiB by default (`feat/rm-window-size`) so many NVK processes fit
  their BAR heaps; per-device quota of a quarter of the window in the KMD; NVK
  falls back to system memory when a map is refused (patch 0025).

## Stability work still open

- Cross-process keyed-mutex hand-offs misorder on Venus and NVK (found
  2026-10-06); the UMD cannot see keyed-mutex calls, so the fix is a GPU-side
  completion boundary on flushes of devices holding shared resources
  (`kmd/flush-completion`).
- An intermittent access violation in ntdll at start-up/exit of NVK test
  processes (~2 in 25) is being hunted (`fix/nvk-rm-win-crash`).
- D3D12 on Venus failed device creation: the conduit-venus sandbox blocked
  libcuda when VK_KHR_acceleration_structure is enabled. Fixed on
  `fix/d3d12-venus-create-device`, not yet installed.

- KMD v312: paging data-safety fixes (a skipped eviction must not be followed
  by a stale page-in), the pin leak on process exit. v311 already stops the
  0x10E bugcheck (BuildPagingBuffer no longer returns a status VidMm treats as
  illegal; an oversized transfer is clamped).
- Live driver replacement: survived on v311; the stop/start breadcrumbs
  (StopStg, StopMs, StopBlobs, StopSwept, StartStg) make the next failure
  readable.

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
