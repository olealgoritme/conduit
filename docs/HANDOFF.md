# Windows on NVK-on-RM: handoff (2026-10-07)

Everything a new session needs to continue the Windows work: the goal and the
bar, what runs today, what was tried (and what did not work), the plan, and how
things are tested. The staged plan and architecture are in
[NVK-ROADMAP.md](NVK-ROADMAP.md); the windowed-Present design and the
guest-blob contract in
[zero-copy-present.md](../guest/windows/docs/zero-copy-present.md) (24.x) and
[VENUS.md](VENUS.md) "Guest-memory blobs"; DWM on NVK in
[dwm-on-nvk.md](dwm-on-nvk.md); the KMD session's own handoff (latency budget, counter traps, MSI-X plan,
next KMD steps) in
[kmd-handoff-2026-10.md](../guest/windows/docs/kmd-handoff-2026-10.md); the test scripts in
[guest/windows/ci/vmtest](../guest/windows/ci/vmtest/README.md).

## Goal and the bar

- The whole Windows 11 desktop and every app and game (D3D11, D3D12, OpenGL,
  Vulkan) on NVK → librmclient → Helios KMD → conduit-backend → nvidia.ko,
  zero-copy, at bare-metal performance. Venus is removed at the end.
- Desktop: must feel like a Linux guest, smooth at the monitor's 240 Hz.
- Games: Unigine Heaven D3D11, **windowed 1600x900**, Medium-ish settings,
  must **easily exceed 400 fps in the first 5 seconds**; the same host, bare
  metal, shows ~576 (4-500 in the opening).
- Performance first, zero-copy from the start, security last.

## What runs today (driver 22.22.343.1, host from main, RTX 5090)

| | Result |
|---|---|
| Desktop / DWM on NVK (`HKLM\SOFTWARE\Helios DwmIcd=nvk`, KMD `ForeignFlip=1`) | 237 fps at 240 Hz (flip announce, `FlipAnnForeign` default with ForeignFlip, `FfAsyncWin=2`) |
| Start menu, search, Explorer, Edge, shell sandboxes on NVK | work (Mesa 0048: NAK printer only with `NAK_DEBUG=annotate`) |
| Heaven D3D11 1600x900, app owns the scanout | 285-511 fps (Mesa 0051) |
| Heaven D3D11 1600x900 **windowed** (composed), GuestBlob 0 | 214 fps |
| same, `GuestBlob=1` (host `venus.guest_blobs true`) | **247 fps**, CPU mirror gone (`BltMirrorN 0`), `GbLeak 0` |
| same, `GuestBlob=1 BltAsync=1 ForeignCopy=1` | 225 fps; KMD Blt DDI 0.135 ms per call (was 1.28 ms) |
| D3D12 (vkd3d-proton UMD12), Vulkan, OpenGL (Zink) | run on NVK; less measured than D3D11 |

GuestBlob, BltAsync and ForeignCopy are **off by default** in 343.1 until the
stress and fallback checks are signed off (see the plan).

## Where the windowed frame time goes now (the next target)

With guest blobs and async Blt the KMD is no longer the cost, but Heaven still
spends ~1.8 ms in `Present` per frame (interval 4.4 ms). A DxgKrnl ETW trace of
the same path shows the app thread waiting in dxgkrnl
(`VIDMM_BEGINCPUACCESS_WAIT`, events 41→42) for the **previous frame's Blt
packet to retire** before the next redirected Blt may take CPU access:

- producer wait (deferral until Heaven's GPU work is done): mean **614 µs**
  (`BltDeferUs / BltAsyncDefer`);
- copy round trip, KMD submit → completion DPC: 98% in **0.5-1 ms**
  (`BltAsyncLat2`), of which the GPU copy itself is ~0.2 ms;
- DMA-completion delivery to dxgkrnl.

Sum ≈ 1.4-1.7 ms = the Present cost. Bare metal returns from Present after
queueing; here each frame waits for a GPU round trip through the guest.
Still to confirm on Heaven itself: per frame Σ(id 42 − id 41) on the app
thread versus the previous Blt packet's id 178 → 180 span.

Levers, most promising first:
1. **GPU-side dependency on the producer's RM fence** (best done on the KMD's RM copy-engine channel, see "Removing Venus"; a host-side wait inside a Venus command is the interim alternative): submit the copy at
   Present time; the host queue waits for the producer's fence and starts the
   copy the moment it is done. Removes the 0.61 ms deferral and the worker hop;
   the round trip overlaps the producer. Host feature (wait on an RM fence
   value inside a Venus command). Estimated 4.4 ms → ~3 ms.
2. **Host round-trip latency**: measure in the backend/renderer kick →
   vkQueueSubmit → fence → used-ring + interrupt; 0.3-0.8 ms of the round trip
   is overhead beyond the 0.2 ms copy (thread hops, fence-wait thread, irqfd).
3. **MSI-X** instead of INTx for the device (branch `kmd/msix-default`,
   [msi-interrupts.md](../guest/windows/docs/msi-interrupts.md)): cuts
   interrupt-to-DPC time on every completion and every RM call (~55 µs per RM
   call today). Needs the vhost-user device to expose MSI-X.
4. dxgkrnl serialises each frame behind the previous copy into the one
   redirection surface; not changeable from the KMD. The real way around the
   copy is the flip model (below, parked).

## Removing Venus: what still uses it, and the RM replacement

`conduit up win11 --venus` is still required for Windows guests: the flag
starts `conduit-venus` in the backend. Apps and DWM run on NVK regardless
(`DwmIcd=nvk`, per-process policy); Venus is still carried for the pieces
below. Roadmap stage S6d ("Venus removed") is the end state; this is the
inventory it needs. Small fix in the meantime: `conduit attach`/`up` should
enable Venus automatically for a Windows guest so nobody types `--venus`.

| Still on Venus | Why it is there | Replacement on NVK/RM |
|---|---|---|
| Windowed (blit-model) Present copy: KMD `Blt` → Venus copy into the guest blob / redirection surface | the copy engine the KMD could reach first | The KMD's own RM client (already exists, "level 5" lane) submits the copy on an RM copy-engine channel. The copy-engine channel waits on the producer's RM semaphore **on the GPU**, so lever 1 above (no CPU deferral, no worker hop) comes for free, without a new host feature. Destination: the same guest-memory pages, mapped to the GPU through RM (system-memory allocation over guest pages) instead of a Venus blob. |
| Fallback for processes not on NVK (deny-list, DXR titles, 32-bit cases that fail) | NVK gaps | shrink the deny-list (S6c); DXR waits on NVK ray tracing |
| The Venus scanout path (`ForeignFlip` off, Venus-DWM) | first working desktop | ForeignFlip with DWM on NVK is the default; drop once restart recovery on NVK is solid |
| Venus fences / ring used by KMD paths (paging, present ready-poll) | built on Venus first | RM fences (S4 done for presents); move the remaining KMD waits to RM semaphores |
| Host: conduit-venus, virglrenderer patches 0001/0002, `--venus-guest-blobs` | the above | removed with the last row |

Open (not done yet): the KMD-side audit that checks this table. Brief: list
every KMD path that touches Venus (ring/instance bring-up, fences and waits,
host-visible blobs and the BAR/aperture CPU view, paging copies, present
buffers, the present ready-poll/worker, scanout image and
SET_SCANOUT_BLOB/flush, ForeignCopy, snapshot/WindowedBlt, read ledger), and
judge the RM copy-engine route against the KMD RM client as it stands. Today
that client has the client/device/subdevice/memory classes and
export/GEM/ScanoutFlip, but no GPFIFO channels, CE objects or push-buffer
submission, and the OS-descriptor mapping of guest pages is UNVERIFIED
([kmd-rm-client.md](../guest/windows/docs/kmd-rm-client.md) 5.3). So the route
is a new subsystem, not a small step; the audit should size it.

Order: (1) windowed copy on the KMD RM client with a GPU semaphore wait (this
is also the windowed-performance fix, so it goes first), (2) KMD waits on RM
fences, (3) deny-list down to DXR, (4) Venus scanout path removed, (5) host
side removed. Each step keeps Venus as a runtime fallback until the next
restart-device/stress pass is clean.

## Tried, and what came of it

| Tried | Outcome |
|---|---|
| `BltNoMirror=1` (skip the KMD CPU mirror) | Stale window: DWM reads the CPU view. Do not use; the guest blob replaced it. |
| Flip-model upgrade of windowed apps via per-app `HKCU\...\DirectX\UserGpuPreferences` | Inconsistent (some apps), never Heaven: DXGI's game classification gates it (`REASON_NONGAME`). Parked. |
| KMD's own RM client ("level 5") as the windowed fix | Not the fix (the cost is the dxgkrnl Blt retire); on hold. |
| Venus fence timing (plain queue-sync fences) | 10 ms → ~1 ms per fence; merged. |
| NVK UBO-descriptor promotion | Off on Windows (0051): GPU per draw 2.7 → 0.7 µs. |
| Restart-per-row testing (reboot between A/B rows) | Interrupted in-guest builds twice; use `pnputil /restart-device` rows (blrow.sh) and reboot only for host installs. |
| Readings without a device restart after changing `FfKnob` etc. | Fake results (knobs are read at StartDevice). Always restart the device per row. |
| zsh loops splitting `"label K=V"` | Knobs silently not applied; arg-splitting loops live in bash scripts. |
| `restart-device` | Leaves the display degraded until a reboot (wrong buffer on scanout, zero-copy-present.md 25); deprioritised by the user. |
| Pagefile unbounded | Grew to 12.7 GB, filled C: twice; fixed at 4096 MB. Dump file on C: (W: dedicated dump was never written). |
| Display idle timeout | Looked like DWM freezes; power timeouts set to 0 in the guest. |

## Plan, in order

1. Sign off 343.1: 10-minute window stress (`stressrun.sh`: GbLeak 0, GbStrike
   0, GbFail flat, no BuildPagingBuffer hang, GbDrainMax ≪ 250 ms), the
   fallback (host without `--venus-guest-blobs` → GbFeat 0, behaves like
   GuestBlob 0), restart-device with a live guest blob. Then make GuestBlob +
   BltAsync + ForeignCopy the defaults (host `venus.guest_blobs` default true).
2. Heaven DxgKrnl ETW attribution (above), then lever 1 (host waits on the
   producer's RM fence) with the KMD session; lever 2 measurements; lever 3
   (MSI-X) A/B.
3. Stability: restart-device recovery, NVK holder renewal (0052) across a real
   restart, Explorer repaint after a DWM restart.
4. Heaven x86 OpenGL (Zink 32-bit) white scene.
5. Flip-model route for windowed apps (parked), level 5 (on hold), Venus
   removal (S6d).
6. Second machine (RTX 4070, Ada): never tested; RM ABI tables and NVK on Ada
   are the risk ([SECOND-MACHINE.md](SECOND-MACHINE.md),
   [GPU-SUPPORT.md](GPU-SUPPORT.md): per-generation matrix, first-hour plan).

## Where things are

- **Release** `v0.1.5-rc1` (pre-release): host `.deb` from main 5ba86d7, Linux
  guest `.deb`, `helios-22.22.343.1.zip` (signed driver + `install.ps1`).
- **Driver builds** run in the win11 VM's W: drive
  (`guest/windows/ci/vm/win-build.sh`, `WIN_ROOT=W:\s315`, incremental);
  packages in `W:\s315\out\pkg-<ver>`, 341.3 kept in `Release-341.3`.
- **Host deploys** are staged (host-deploy4-build.sh) and installed through
  the cycle script's `PRESTART` hook while the VM is off.
- **Host tuning** (swappiness 10, zram, shmem THP within_size, QEMU
  oom_score_adj, iothread pinning; hugepages staged for a host reboot):
  [HOST-TUNING.md](HOST-TUNING.md).
- **Guest registry**: `HKLM\SOFTWARE\Helios DwmIcd=nvk, DwmNvkMaxStarts=60`;
  KMD knobs under `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`
  (read at StartDevice). Leftover test values: per-app
  `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` (remove).
- **Branches not on main**: `kmd/msix-default` (MSI-X opt-in),
  `kmd/independent-flip-design` (flip design doc), `kmd/std-census`,
  `kmd/dwm-restart-repaint`, `build/s6-pkg`, and the per-change `kmd/*` lanes
  already folded into v343.

## Working rules that cost time to learn

- One session drives all VM testing and installs; agents write code and build.
- Restart win11 only with the cycle script; run the watchdog during any guest
  work; check host disk space (a full root disk pauses the VM).
- Never `pkill -f`; kill by PID. Never `New-Item -Force` on an existing
  registry key (wipes values); use `Set-ItemProperty`.
- Before any KMD push: kmd_logic tests, the protocol crate tests (scratch copy
  with an empty `[workspace]`), the C ABI check; after every merge, not just
  before the push.
- Host builds pinned: `taskset -c 0-7,16-23 nice -n 19`, -j4.
- Verify live state before describing it (a rejected command may still have
  run; a fallback to Venus looks like success).
