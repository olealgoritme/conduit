# Windows guests on NVK-on-RM: goal, state and plan

The goal: the whole Windows desktop and every app and game in a Windows guest
render through Mesa's NVK driver talking to the host's NVIDIA RM through
Conduit (NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko), with
zero-copy presentation, smooth frame pacing and performance at bare-metal
level. Venus (Vulkan command encoding replayed on the host) is removed once
every workload has an NVK path.

This file is the plan; [HANDOFF.md](HANDOFF.md) has the shipped
configuration, the open items and how things are tested;
[PERF-ROADMAP.md](PERF-ROADMAP.md) the frame path and the performance plan.

## Where it stands

Guest driver 22.22.405.84, tested on the RTX 5090 (win11 guest,
5120x1440@240). The desktop and DWM run on NVK (`DwmIcd=nvk`, written by
`install.ps1`; the KMD flips DWM's buffers zero-copy with `ForeignFlip`, on by
default). D3D11, D3D12, Vulkan and OpenGL (Zink) run on NVK; Venus stays as
the fallback for processes the policy keeps off NVK. Full-screen flip-model
games are scanned out from their own buffers (independent flip, on by
default); composed NVK presents retire on the RM fence (`NvkRmFencePresent`,
on by default). The shipped KMD defaults need no registry values
([shipping_defaults.rs](../guest/windows/kmd_logic/src/shipping_defaults.rs),
[WINDOWS-KNOBS.md](WINDOWS-KNOBS.md)).

Measured, code defaults:

| Workload | Result |
|---|---|
| Counter-Strike 2, 1920x1080 | ~330-380 fps |
| Counter-Strike 2, 5120x1440, with bots | ~350 fps |
| Counter-Strike 2, lower resolutions | ~400-500 fps |
| 3DMark Steel Nomad (DX12) | score 5823 |

Remaining, in order:

1. The windowed (composed) Present: event-driven completion of the KMD's
   copy-engine copy, then every windowed copy off the Venus fallback
   ([HANDOFF.md](HANDOFF.md#where-the-windowed-frame-time-goes)).
2. Recovery after `pnputil /restart-device` (NVK's holder context renewal,
   Mesa patch 0052) and Explorer repaint after a DWM restart.
3. Unigine Heaven x86 OpenGL (Zink, 32-bit) renders a white scene.
4. Venus removal (S6d).

## What makes NVK fast here

- A host-visible VRAM heap (BAR): DXVK's per-frame buffers live in VRAM, not
  system memory.
- Zero-copy present through the KMD's scanout source, with block-linear
  swapchain images.
- RM backend features measured on the host against NVIDIA's driver: cacheable
  GPU mappings of system memory, compression, ZCULL.
- NVK per-draw cost close to NVIDIA's (patches-common 0001-0007), with
  bit-identical output.
- Cheap RM forwarding: CPU map/unmap (QEMU patch 0008) and channel open/close
  without long host round trips.
- Frames NVK already flipped to scanout are not also sent through DWM's
  redirection blit.

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
kernel driver. On this path it forwards RM calls, flips frames and runs the
windowed Present copy on its own RM copy-engine channel.

## Stages

| Stage | What | State |
|---|---|---|
| S1 | Heaven via app-local DXVK on NVK | done |
| S2 | Zero-copy present for NVK apps | done (linear and block-linear; three scanout images) |
| S3 | The Helios D3D11 UMD runs DXVK on NVK, global with a deny-list, Venus fallback | done |
| S4 | RM fences as the present boundary | done; RM-fence present on by default |
| S5 | D3D12 on NVK (vkd3d-proton in UMD12) | done (NVK and Venus) |
| S6a | Cross-process shared surfaces and keyed mutex on NVK | done with a releaser CPU wait (the default); the GPU-ordered hand-off ledger is correct but stalls under load |
| S6b | DWM on NVK | done: DWM on NVK composes the desktop, `ForeignFlip` shows its buffers zero-copy, both defaults |
| S6c | Shrink the deny-list | shared ids for every desktop/browser format work (A8 for the shell, NV12/P010 for video, fp16/10-bit); category moves continue ([dwm-on-nvk.md](dwm-on-nvk.md)) |
| S6d | Venus removed | after the windowed copy and the remaining KMD waits leave Venus; DXR titles stay on Venus until NVK has ray tracing |

Decided on the way:
- The KMD's own RM-memory desktop primary (level 5) works but is shown only
  before DWM starts, so it does not carry the desktop; parked.
- The KMD authors 128-byte-aligned pitches for anything NVK imports; GB20x
  block-linear modifiers come in three families by element size.
- NVK is global with a deny-list; the Helios DXVK fork is used; vkd3d's FL12
  conservative-raster check is relaxed for NVK; the KMD has its own RM
  client; KMD builds are installed live.

## Workloads outside D3D11 and the desktop

| Workload | State |
|---|---|
| D3D12 games | S5, done |
| Native Vulkan games | NVK registered per adapter by the driver package |
| OpenGL apps | Zink on NVK as the adapter's OpenGL ICD (OpenGL 4.6) |
| Video decode/encode (browsers, players) | H.264 decode through the UMD's D3D11 video DDI on NVK works (bit-exact; custom scaling matrices unsupported); browsers also need NV12/P010 shared ids (S6c). Details: [guest/nvk-rm/docs/video.md](../guest/nvk-rm/docs/video.md) |
| Ray tracing (DXR, VK_KHR_ray_tracing) | NVK has none; these stay on Venus until upstream adds it |
| Full-screen flip-model games | independent flip, on by default ([independent-flip.md](../guest/windows/docs/independent-flip.md)) |
| HDR, VRR | not supported yet ([KNOWN-ISSUES.md](KNOWN-ISSUES.md)) |
| Multi-monitor, sleep/resume | untested |

## Performance work still open

Ranked, with code references and how to measure each:
[PERF-ROADMAP.md](PERF-ROADMAP.md). In short:

- The windowed Present: event-driven copy-engine completion, dxgkrnl's
  serialisation behind the previous Blt.
- The D3D11 present gate on the app thread (the threaded front end,
  `UmdThreadedFrontend`, is on by default).
- The GPU→CPU wake chain (one shared non-stall event per device, a 16-buffer
  event ring).
- Display pacing from the host's presentation feedback (PR #78, draft).
- D3D12's software packet queue (no hardware queues).
- Zink: scanout image creation can fail in big GL apps.
- DXVK's own shader translation at start-up (a hitch on every launch).

## Stability work still open

- NVK device loss (patch 0044, librmclient) across a real device restart.
- NVK crash at process exit with the hand-off ledger on (imported memory).
- Explorer repaint after a DWM restart.
- A full host disk pauses the VM; after the resume the backend's NVIDIA side
  needs a full restart.

## NVK patch stack (Windows)

The series live in [guest/nvk-rm](../guest/nvk-rm/README.md#series-layout):
`patches/` (the RM backend), `patches-windows/` (the Windows build, Win32
WSI, Helios scanout, shared surfaces and composed present, RM fences, Zink,
NVDEC), `patches-windows-dxvk/` (what DXVK needs on Windows) and
`patches-common/` (per-draw cost, compression, ZCULL, deferred frees, waits).
The Windows stack is base + `patches/0001-0013` + `patches-windows/*` +
`patches-windows-dxvk/*` + `patches-common/*` (`build-windows.sh`); the
README describes each patch.

## How changes get tested

- Driver-only updates: build from a verified worktree, check the boot-stack
  frame sizes (`guest/windows/tools/kmd-frame-sizes.ps1`), install live with
  `pnputil /install`, wait for the new DriverVersion, run the RM, pin, event,
  scanout and import smoke tests (`guest/rmclient/tests`).
- Host changes (backend, QEMU, conduit-venus): one checked restart that
  verifies shutdown, the binaries, the backend's flags, conduit-venus, SSH and
  the Helios mode (`guest/windows/ci/vmtest/win11-cycle.sh`).
- NVK changes: `guest/nvk-rm/tests` (vk_summary, compute, offscreen, BAR,
  coherence, block-linear readback, scanout present), then a fixed game or
  benchmark window after warm-up with host `nvidia-smi dmon`.
- A watchdog polls the guest for reboots and new crash dumps while anything
  runs there.
