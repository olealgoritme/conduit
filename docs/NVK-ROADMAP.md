# Windows guests on NVK-on-RM: goal, state and plan

The goal: the whole Windows desktop and every app and game in a Windows guest
render through Mesa's NVK driver talking to the host's NVIDIA RM through
Conduit (NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko), with
zero-copy presentation, smooth frame pacing and performance at bare-metal
level. Venus (Vulkan command encoding replayed on the host) is removed once
every workload has an NVK path.

This file is the working plan. It is updated as stages land; the numbers are
measurements, with the setup named.

## Where it stands (2026-10-07)

Driver package 22.22.341.3 (KMD v341) is on main and was tested on the
RTX 5090 (win11 guest, 5120x1440@240). The desktop and DWM run on NVK
(`DwmIcd=nvk`, KMD `ForeignFlip=1`); D3D11, D3D12, Vulkan and OpenGL (Zink)
run on NVK; Venus stays as the fallback.

| | NVK-on-RM |
|---|---|
| NVK DWM desktop at 240 Hz | up to 237 fps, with the flip-announce fix (KMD v333/v334: flip announce on by default, `FlipAnnForeign` default with `ForeignFlip`) |
| Unigine Heaven D3D11 1600x900, app owns the scanout | 285-511 fps after NVK patch 0051 (UBO descriptors no longer promoted to bound cbufs on Windows; GPU time per draw 2.7 -> 0.7 µs) |
| Unigine Heaven D3D11 1600x900, windowed (composed by DWM) | ~200-220 fps |
| RM-fence present (`NvkRmFencePresent`) | on by default: composed NVK presents retire on the RM fence, no CPU wait |

Remaining, in order:

1. The windowed blt path: a guest-memory blob as the Venus copy destination
   removes the CPU copy and the fence wait from the composed Present. Host
   side done (`venus.guest_blobs`, [VENUS.md](VENUS.md) "Guest-memory
   blobs"); KMD v343 in review.
2. Recovery after `pnputil /restart-device`: open issues (wrong buffer on
   scanout after a restart,
   [zero-copy-present.md](../guest/windows/docs/zero-copy-present.md) 25;
   NVK's holder context renewal, Mesa patch 0052).
3. Unigine Heaven x86 OpenGL (Zink, 32-bit) renders a white scene.
4. The KMD's own RM client at level 5 has not been run.
5. Venus removal (S6d).

## Earlier measurements (2026-10-06, afternoon)

Measured in the win11 guest (5120x1440@240) through the installed driver
package 22.22.325.1 (KMD v325), NVK chosen per process unless noted.

| | Venus path (today's default) | NVK-on-RM |
|---|---|---|
| Unigine Heaven, D3D11, 1600x900 Medium, installed UMD | 171 fps | 377 fps measured; ~200 fps seen live (scanout release tracking suspected, being measured) |
| Unigine Heaven High / Valley / Superposition D3D11 1080p Medium | | 267 / ~190 / 321 fps |
| Superposition OpenGL (Zink) 1080p Medium | | 204 fps (Zink falls back to GDI present; heap 3 exhaustion) |
| D3D11 spin: zero-copy scanout / composed by DWM | 7100–8800 fps | 4200–5450 / 5000–13500 fps |
| D3D12 triangle + compute, zero-copy present | 900–1270 fps | 1085–4450 fps |
| OpenGL gears (wgl, Zink) | 775–873 fps | 2000–4350 fps |
| H.264 decode through the UMD's D3D11 video DDI | | 5 of 6 clips bit-exact (custom scaling matrices unsupported by NVK) |
| Cross-process shared surfaces, keyed mutex | pass | pass, all 13 formats over KMT (A8, NV12, P010, fp16, 10-bit...) |
| Live driver replacement | DWM and shell survive (device-lost handling in the Venus ICD and the UMD; NVK's in 0044) | |
| DWM on NVK (`DwmIcd=nvk`, `ForeignFlip=1`) | | runs and composes correctly; flips zero-copy; stalls on flip completion (no vsync reported while the foreign primary is shown) |

Known gaps: blit-model apps are throttled by Unigine when unfocused (looked
like a Venus 10 fps bug; a test artefact); the D3D11 UMD advertised the
WDDM 1.3 DDI, so the runtime refuses typed-UAV BGRA textures (FFXIV's fatal
DirectX error) - the opt-in WDDM 2.3 DDI is built; Heaven's menu renders pink
on NVK; Basemark D3D12 hangs on NVK after the first frame.

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
| S4 | RM fences as the present boundary | done; RM-fence present on by default | `feat/s4-rm-fences`, `feat/umd-nvk-combined` |
| S5 | D3D12 on NVK (vkd3d-proton in UMD12) | done (NVK and Venus) | `feat/umd-nvk-combined` |
| S6a | Cross-process shared surfaces and keyed mutex on NVK | done with a releaser CPU wait; GPU-ordered hand-off ledger in progress | `fix/s6-handoff-ledger` |
| S6b | DWM on NVK | done: DWM on NVK composes the desktop, KMD `ForeignFlip` shows its buffers zero-copy, up to 237 fps at 240 Hz with flip announce (v333/v334) | `feat/dwm-on-nvk`, `feat/umd-nvk-combined` (KMD v320–v341) |
| S6c | Shrink the deny-list | shared ids for every desktop/browser format work (A8 for the shell, NV12/P010 for video, fp16/10-bit); category moves in progress | `feat/nvk-share-formats`, `docs/dwm-on-nvk.md` |
| S6d | Venus removed | after S6b/S6c; DXR titles stay on Venus until NVK has ray tracing | |

Measured and decided on the way:
- The KMD's own RM-memory desktop primary (level 5) works but is shown only
  before DWM starts, so it does not carry the desktop; parked.
- DWM opens no KMD-made surfaces while blit-model apps are occluded, so
  RM-backed GDI redirection is parked (revisit if CrossAdaptCaps is enabled).
- The ForeignFlip host round trip is ~80–140 µs, not a limit; the async flip
  window (FfAsyncWin) gave no gain and stays off.
- The KMD authors 128-byte-aligned pitches for anything NVK imports; GB20x
  block-linear modifiers come in three families by element size.
- The keyed-mutex hand-off ledger (GPU-ordered, no CPU wait) is correct but
  stalls under load; the releaser CPU wait stays the default.
- Two desktop freezes were traced to KMD paths fixed in v323 (a stuck
  windowed-blt ready queue after a killed app; a Venus ring wait that could
  hold the scanout lock for minutes).

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

- The windowed (composed) Present: guest-memory blob copy destination
  (KMD v343 in review); today ~200-220 fps windowed against 285-511 fps on
  the scanout in Heaven.
- MSI-X for the GPU device as the default, to cut the ~55 µs per RM call
  (KMD lane in progress).
- Scanout release tracking (Mesa 0036) caps NVK apps that own the screen;
  measure and decide (more images, or frames dropped).
- Zink: scanout image creation fails (-8) in big GL apps and heap 3 runs out.
- Display pacing from the host's presentation feedback (design in progress).
- DXVK's own shader translation at start-up (~0.6 s hitch on every launch).

## Stability work still open

- NVK device loss (patch 0044, librmclient) to be verified across a real
  device restart.
- NVK crash at process exit with the hand-off ledger on (imported memory).
- Explorer does not repaint (clock, icons) after a DWM restart.
- SearchHost.exe crash loop in the guest (Windows' search UI, not Helios).
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
| `0051` | UBO descriptors not promoted to bound cbufs on Windows (Heaven GPU time per draw 2.7 -> 0.7 µs) |
| `0052` | the Helios holder context renewed after a KMD restart |
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
