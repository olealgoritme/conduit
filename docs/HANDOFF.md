# Windows on NVK-on-RM: handoff

Everything a new session needs to continue the Windows work: the goal and the
bar, what runs, the shipped configuration, what is still open, and how things
are tested. The stages and architecture are in
[NVK-ROADMAP.md](NVK-ROADMAP.md); the frame path hop by hop and the
performance plan in [PERF-ROADMAP.md](PERF-ROADMAP.md); every guest knob and
its default in [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md); the windowed-Present
design in [zero-copy-present.md](../guest/windows/docs/zero-copy-present.md)
and [vram-redirection.md](../guest/windows/docs/vram-redirection.md); DWM on
NVK in [dwm-on-nvk.md](dwm-on-nvk.md); the KMD design records in
[guest/windows/docs](../guest/windows/docs/); the test scripts in
[guest/windows/ci/vmtest](../guest/windows/ci/vmtest/README.md).

## Goal and the bar

- The whole Windows 11 desktop and every app and game (D3D11, D3D12, OpenGL,
  Vulkan) on NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko,
  zero-copy, at bare-metal performance. Venus is removed at the end.
- Desktop: must feel like a Linux guest, smooth at the monitor's 240 Hz.
- Games: frame rate and frame pacing close to bare metal on the same host.
  Unigine Heaven D3D11, windowed 1600x900, Medium-ish settings, is the
  windowed (DWM-composed) reference: the same host, bare metal, shows about
  576 fps.
- Performance first, zero-copy from the start, security last.

## What runs

Guest driver **22.22.405.84**
([driver-version.env](../guest/windows/kmd_render/driver-version.env)); host
from main. Setup: RTX 5090, win11 guest at 5120x1440@240.

- The desktop: DWM composes on NVK (`HKLM\SOFTWARE\Helios!DwmIcd=nvk`, written
  by `install.ps1`); the KMD shows its buffers zero-copy (`ForeignFlip`).
- Apps: D3D11 (DXVK), D3D12 (vkd3d-proton), Vulkan and OpenGL (Zink) run on
  NVK, global with a deny-list; Venus is the fallback for processes the policy
  keeps off NVK.
- Full-screen flip-model games are scanned out from their own buffers
  (independent flip, `IndepFlip` 1); windowed apps are composed by DWM.

Measured, code defaults:

| Workload | Result |
|---|---|
| Counter-Strike 2, 1920x1080 | ~330-380 fps |
| Counter-Strike 2, 5120x1440, with bots | ~350 fps |
| Counter-Strike 2, lower resolutions | ~400-500 fps |
| 3DMark Steel Nomad (DX12) | score 5823 |

## Shipped configuration

A fresh install needs no KMD service-key values: the absent value is the
shipped configuration
([shipping_defaults.rs](../guest/windows/kmd_logic/src/shipping_defaults.rs)).

| Knob | Default | What it does |
|---|---|---|
| `ForeignFlip` | 1 | the KMD flips the NVK DWM's buffers itself (`ScanoutFlip`); `FlipAnnForeign` and `FfAsyncWin` follow it |
| `ForeignCopy` | 1 | the explicit-modifier copy of a foreign (NVK-on-RM) source |
| `BltAsync` | 1 | the composed foreign Blt returns without a CPU wait |
| `RedirVram` | 1 | GPU-only GDI surfaces in RM video memory, the redirected Blt into them on the KMD's RM copy-engine channel; the UMD's `NvkSkipBltCopy` follows it |
| `MsiMode` | 2 | MSI-X, with the boot-loop breaker and the delivery latch putting INTx back on a failure |
| `IndepFlip` | 1 | independent flip for full-screen flip-model swap chains; `HwCursor` follows it, the UMD's `DirectFlipSupport` is 5 |

Opt-in, not the default:

- `GuestBlob` (and the host's `venus.guest_blobs`), `RmCopyEngine` (routes
  the other windowed Present copies through the copy-engine channel),
  `CopyQueue`,
  `GdiAccel`, `FlipDoneHost`: experiments, default 0.

Host side, all defaults:

- Guest GPU timeslice 100 µs (`backend.gpu_timeslice_us`), so host display
  work waits at most one short slice behind a busy guest
  ([HOST-TUNING.md](HOST-TUNING.md#gpu-time-the-guests-timeslice)).
- Host isolation on (`host.isolation auto`): host work, device interrupts and
  kernel workqueues stay off a running libvirt VM's CPUs
  ([HOST-TUNING.md](HOST-TUNING.md#cpu-host-isolation)).
- Backend round-trip latency options on (`backend.latency all`).
- Keyboard and mouse over virtio-input (Conduit Mouse, Keyboard, Tablet,
  served by the backend; needs the guest's `vioinput` driver)
  ([INPUT.md](INPUT.md)).
- The viewer presents immediately in fullscreen and with vsync in a window
  (`--sync fullscreen`) ([VIEWER.md](VIEWER.md)).

`conduit up`/`view` need `--venus` for a Windows guest (the TUI adds it).

## Where the windowed frame time goes

Per-stage budget of every frame, both present paths, guest and host on one
clock: [frame stage timing](TRACING.md#frame-stage-timing) (`StageTrace=1`
in the guest driver, then `conduit trace VM stages` or
`guest/windows/ci/vmtest/stages.sh VM`).

A windowed NVK app's Present is a Blt the KMD runs: the source is the app's
NVK image (the UMD does not copy it, `NvkSkipBltCopy`), the destination the
window's redirection surface. With `RedirVram` a GDI redirection texture
lives in RM video memory and the copy into it runs on the KMD's RM
copy-engine channel ([vram-redirection.md](../guest/windows/docs/vram-redirection.md)
5.9); a CPU-visible destination takes the Venus copy. dxgkrnl serialises a
window's next Blt behind the previous Blt packet's retirement, so the Present
costs a round trip through the guest per frame, where bare metal returns
after queueing.

Levers, most promising first (details and code references:
[PERF-ROADMAP.md](PERF-ROADMAP.md)):

1. Event-driven completion of the copy-engine copy: completion is polled
   (a settle spin after dispatch, then the HPD worker's 0.5 ms poll).
2. A non-blocking D3D11 present: the present gate waits for DXVK's CS and
   submit threads on the app thread.
3. Host round-trip latency of the completion path (backend event pump,
   interrupt delivery).
4. Independent flip already removes the composite for full-screen games; a
   windowed app keeps the copy.

## Removing Venus: what still uses it

Apps and DWM run on NVK regardless of `--venus`; Venus is still carried for
the pieces below. Stage S6d ("Venus removed") is the end state.

| Still on Venus | Why it is there | Replacement on NVK/RM |
|---|---|---|
| The windowed Present copy into a CPU-visible KMD standard buffer (a destination that is not an RM video-memory surface) | the copy engine the KMD could reach first | the KMD RM copy-engine channel, which already carries the `RedirVram` copies |
| Processes not on NVK (deny-list, DXR titles, cases that fail) | NVK gaps | shrink the deny-list (S6c); DXR waits on NVK ray tracing |
| The Venus scanout path (`ForeignFlip` 0, Venus DWM via `install.ps1 -VenusDesktop`) | the first working desktop | `ForeignFlip` with DWM on NVK, the default |
| Venus fences and ring used by KMD paths (paging copies, the present ready-poll, GuestBlob) | built on Venus first | RM fences and semaphores (presents already retire on RM fences) |
| Host: conduit-venus, the virglrenderer patches, `--venus-guest-blobs` | the above | removed with the last row |

Order: (1) every windowed copy on the KMD RM copy-engine channel with
event-driven completion, (2) the remaining KMD waits on RM fences,
(3) deny-list down to DXR, (4) the Venus scanout path removed, (5) the host
side removed. Each step keeps Venus as a runtime fallback until the next
restart-device and stress pass is clean.

## Settled findings

| Finding | Consequence |
|---|---|
| `BltNoMirror=1` leaves the window stale: DWM reads the CPU view of a CPU-visible redirection surface | do not use it; GPU-only surfaces (`RedirVram`) are the way around the CPU view |
| Flip-model upgrade of windowed apps via per-app `HKCU\...\DirectX\UserGpuPreferences` is gated by DXGI's game classification (`REASON_NONGAME`) | not a route for windowed apps |
| NVK UBO-descriptor promotion costs GPU time per draw on Windows | off on Windows (Mesa patch 0051) |
| Most KMD knobs are read at StartDevice | restart the device (or reboot, per the Read column of [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md)) for every A/B row; a reading without it is not a result |
| zsh does not word-split `"label K=V"` | loops that split arguments live in bash scripts |
| The display idle timeout looks like a DWM freeze | set the guest's power timeouts to 0 on a test VM |
| An unbounded pagefile can fill the guest's system drive | fix its size on a test VM |

## Plan, in order

1. Event-driven completion for the KMD copy-engine copies, then move the
   remaining windowed copies off the Venus fallback (PERF-ROADMAP T4).
2. The D3D11 present gate off the app thread.
3. Host-paced vblank for the Windows guest (PR #78, draft) and the guest
   cursor drawn by the viewer under a grab (PR #81, draft).
4. Stability: recovery after `pnputil /restart-device` (NVK holder renewal,
   Mesa patch 0052) and Explorer repaint after a DWM restart (the `Dw*`
   counters, `zero-copy-present.md` "DWM restart and stale Explorer").
5. Edge video on NVK: check for the per-frame stall reported on older
   packages. Repro `guest/windows/tools/chromium_like.cpp` (Chromium's DComp
   NV12/BGRA video path with keyed-mutex / shared-fence hand-off).
6. Unigine Heaven x86 OpenGL (Zink 32-bit) white scene.
7. Venus removal (S6d, the order above).
8. Second machine (RTX 4070 SUPER, Ada): a Linux guest on NVIDIA's own
   user-mode driver runs (closed modules 565.77, see GPU-SUPPORT.md
   "Measured"); NVK on Ada and the Windows guest are untested
   ([SECOND-MACHINE.md](SECOND-MACHINE.md),
   [GPU-SUPPORT.md](GPU-SUPPORT.md): per-generation matrix, first-hour plan).

## Where things are

- **Releases**: GitHub releases carry the host `.deb`, the Linux guest `.deb`
  and the signed Windows driver package (`helios-<version>.zip` with
  `install.ps1`).
- **Driver builds** run in a Windows build VM from a checkout
  (`guest/windows/ci/vm/win-build.sh`, set up once with `Setup-BuildVm.ps1`);
  the version is edited only in `driver-version.env`.
- **Host deploys** are staged and installed through the cycle script's
  `PRESTART` hook while the VM is off
  ([vmtest](../guest/windows/ci/vmtest/README.md)).
- **Host tuning**: [HOST-TUNING.md](HOST-TUNING.md) (memory, CPU placement,
  host isolation, the GPU timeslice, direct scanout).
- **Guest registry**: the recommended configuration is the code defaults
  plus what `install.ps1` writes ([WINDOWS-KNOBS.md](WINDOWS-KNOBS.md)).
  Values left from a test keep acting until deleted; KMD knobs live under
  `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, UMD knobs
  under `HKLM\SOFTWARE\Helios`.
- **Open pull requests**: #78 host-paced vblank (draft), #81 cursor position
  for the viewer (draft), #90 threaded D3D11 front end on by default (draft).

## Working rules that cost time to learn

- One session drives all VM testing and installs; agents write code and build.
- Restart the test VM only with the cycle script (`win11-cycle.sh`); run the
  watchdog (`watch-win11.sh`) during any guest work; check host disk space (a
  full root disk pauses the VM).
- Never `pkill -f`; kill by PID. Never `New-Item -Force` on an existing
  registry key (it wipes the values); use `Set-ItemProperty`.
- Before any KMD push: kmd_logic tests, the protocol crate tests (scratch copy
  with an empty `[workspace]`), the C ABI check; after every merge, not just
  before the push.
- Keep host builds off the VM's CPUs and at low priority (`taskset`, `nice`,
  a low `-j`): they share cores with QEMU and the backend.
- Verify live state before describing it (a rejected command may still have
  run; a fallback to Venus looks like success).
