# Windows guests (experimental)

A Windows 11 guest renders on the host GPU through NVK-on-RM: Mesa's NVK
Vulkan driver in the guest talks to the host's NVIDIA kernel driver (RM)
through librmclient, the Helios KMD and `conduit-backend`. The desktop and
DWM, D3D11 (DXVK in the Helios UMD), D3D12 (vkd3d-proton in UMD12), Vulkan
and OpenGL (Zink) run on NVK, with zero-copy presentation. Venus (Vulkan
command encoding executed by the host's NVIDIA Vulkan driver in
`conduit-venus`) is still there as the fallback for processes the policy
keeps off NVK. The guest drivers come from Helios
([guest/windows/HELIOS.md](../guest/windows/HELIOS.md)); the host side is in
[VENUS.md](VENUS.md); state and measurements are in
[NVK-ROADMAP.md](NVK-ROADMAP.md). It is opt-in (`--venus`), tested on one
machine (RTX 5090, a 5120×1440 240 Hz monitor), and has the limits listed in
[KNOWN-ISSUES.md](KNOWN-ISSUES.md#windows-guests). For a fresh setup on
another host, follow [SECOND-MACHINE.md](SECOND-MACHINE.md).

## Host

- A backend built with the `venus` feature and `conduit-venus`. The release
  packages (and the flake) have both; from a checkout, build them as
  [VENUS.md](VENUS.md) describes and point `CONDUIT_BACKEND` /
  `CONDUIT_VENUS` at them.
- QEMU (the bundled one); `--vmm builtin` has no region 3.

## The VM

Make a Windows 11 VM in virt-manager as usual (UEFI/OVMF, Secure Boot off:
the driver is test-signed), then give it Conduit's GPU with the VM shut off:

```bash
conduit attach win11                  # recognizes Windows: no Linux guest setup
conduit view win11 --venus            # or: conduit up win11 --venus
```

[examples/win11.xml](examples/win11.xml) is an example domain (q35, OVMF with
Secure Boot off, TPM 2.0, virtio disk and network), with the parts `attach`
adds shown in comments; [SECOND-MACHINE.md](SECOND-MACHINE.md) section 3 says
where to get the Windows and virtio-win ISOs.

`attach` tells a Windows VM from a Linux one by the OS virt-manager recorded
(libosinfo `http://microsoft.com/win/...` in `<metadata>`), Hyper-V features
in the definition, or, for a running VM, the guest agent's
`guest-get-osinfo`. It decides that before it changes anything; for Windows
it skips the Linux guest setup (the guest driver is the Helios package,
below) and prints what to do instead. `attach` sets a host-passthrough CPU
with the host's physical address width, which OVMF needs to place the 64 GiB
shared-memory BAR ([VENUS.md](VENUS.md), "Windows/OVMF guests"). The display
mode is your monitor's, as for a Linux
guest; the guest learns it from the EDID the backend serves. Keyboard and
pointer reach Windows through the boot console's emulated PS/2 keyboard and
USB tablet ([SCANOUT.md](SCANOUT.md#boot-console)). The backend sends a guest
Conduit `InputEvent`s only when its driver acks the virtio feature
`NVGPU_CFG_TAKES_INPUT` (bit 12) and runs the event queue; the Helios KMD
never acks it, so starting the event queue (for `EventReady`) leaves input
where it is.

**Hyper-V enlightenments.** `attach` adds these to a Windows domain (on the
test machine they made frame pacing noticeably steadier); each one the domain
already has, on or off, stays as it is, one whose prerequisite is off
(`synic`, `tlbflush` and `ipi` need `vpindex`, `stimer` needs `synic`) is
left out, `<hyperv mode='passthrough'>` is left alone, and a domain without
`<clock>` gets `offset='localtime'`:

```xml
<features>
  <hyperv mode='custom'>
    <relaxed state='on'/>
    <vapic state='on'/>
    <spinlocks state='on' retries='8191'/>
    <vpindex state='on'/>
    <runtime state='on'/>
    <synic state='on'/>
    <stimer state='on'><direct state='on'/></stimer>
    <reset state='on'/>
    <frequencies state='on'/>
    <tlbflush state='on'/>
    <ipi state='on'/>
  </hyperv>
</features>
<clock offset='localtime'>
  <timer name='hypervclock' present='yes'/>
  <!-- the rtc, pit and hpet timers virt-manager wrote are kept -->
</clock>
```

(virt-manager already writes `relaxed`, `vapic` and `spinlocks` for a Windows
VM.) `conduit attach --dry-run win11` prints the result. Pinning the vCPUs to
one CCD made no measurable difference.

## Guest driver

The driver package is the WDDM KMD, the x64/x86 D3D11 and D3D12 UMDs, NVK on
RM with librmclient (64- and 32-bit) and Zink. The current version is
22.22.405.24 (`guest/windows/kmd_render/driver-version.env`, the only place
the version is set). The adapter shows as "Conduit Helios", the monitor as
"Conduit".

**First install: the full package.** Build `HeliosSetup.exe` with
`.github/workflows/windows.yml` (KMD, UMDs, NVK, Zink, the Mesa Venus ICD,
Khronos loaders) and run it once in the guest
([guest/windows/packaging/windows/README.md](../guest/windows/packaging/windows/README.md)).
The first run turns on test-signing and asks for a reboot; run it again after
the reboot, then reboot once more.

**Driver updates: the local build VM**
([guest/windows/ci/vm/README.md](../guest/windows/ci/vm/README.md)):

```sh
git submodule update --init --recursive guest/windows/third_party/dxvk guest/windows/third_party/vkd3d-proton
guest/nvk-rm/windows/stage-helios-package.sh          # NVK + Zink, cross-built -> dist/nvk-windows
WIN_SSH=user@127.0.0.1 guest/windows/ci/vm/win-build.sh Release
```

The package is signed with a development certificate
(`helios-dev-test.cer`, in the package). In the guest, from an administrator
prompt:

```bat
bcdedit /set testsigning on
certutil -addstore -f Root helios-dev-test.cer
certutil -addstore -f TrustedPublisher helios-dev-test.cer
pnputil /add-driver helios_kmd_render.inf /install
```

then reboot (or `pnputil /restart-device` on the adapter). pnputil keeps an
installed driver of the same version: bump `HELIOS_KMD_VERSION` for every
build installed over an earlier one.

**Which driver a process gets.** One policy, read by the UMDs, NVK and Zink
(`guest/windows/protocol/include/helios_icd_policy.h`), decides per process,
from values under `HKLM\SOFTWARE\Helios`: NVK by default, except the built-in
deny-list (DWM, the shell, browsers and other interop-heavy apps stay on
Venus); `NvkDenyList` / `NvkAllowList` (executable names, `;`-separated)
adjust it; `Icd=venus` puts D3D, Vulkan and OpenGL back on Venus. When DWM
is on NVK (`DwmIcd=nvk`, below) the rest of the desktop follows it, and only
`NvkDenyList` still keeps a process on Venus (`DesktopFollowsDwm=0` turns
that off).

## Opt-ins

Off by default while new; each one was measured on the test machine
([NVK-ROADMAP.md](NVK-ROADMAP.md)).

| Where | Setting | What it does |
|---|---|---|
| Host | `conduit config set venus.guest_blobs true` | the backend serves guest-memory blobs (`--venus-guest-blobs`, [VENUS.md](VENUS.md) "Guest-memory blobs"): Venus copy destinations over the guest's own pages, for the KMD's windowed Present. Applies when the VM's backend next starts. Needs the patched virglrenderer (patch 0002) |
| Guest, `HKLM\SOFTWARE\Helios` | `DwmIcd` = `nvk` (REG_SZ) | DWM on NVK, read only by `dwm.exe` when it starts. A crash-loop guard sends DWM back to Venus after `DwmNvkMaxStarts` (2) starts within `DwmNvkGuardSeconds` (600) ([dwm-on-nvk.md](dwm-on-nvk.md) 4.1). Use with `ForeignFlip=1` |
| Guest, KMD service key | `ForeignFlip` = 1 | the KMD flips the NVK DWM's swap-chain buffers to the scanout itself, zero-copy. Read at adapter start (`pnputil /restart-device`) |
| Guest, `HKLM\SOFTWARE\Helios` | `DirectFlipSupport` (default 0; or `HELIOS_DIRECT_FLIP_SUPPORT` per process) | what the D3D11.1 `CheckDirectFlipSupport` DDI answers, which DWM uses for independent flip: 0 = never; 1 = yes when dxgkrnl reports DirectFlip for the adapter and size and format match; 3 = as 1 and only for formats the KMD scans out as they are; 5 = as 1, and two different 8-bit scan-out formats also pair; 2 = whenever size and format match (test lever). Use 1 together with `IndepFlip=1` |
| Guest, KMD service key | `IndepFlip` (default 0; 2 also completes the unregistered DMA flip instead of failing it) | independent flip: a borderless full-screen flip-model window is scanned out from its own buffers ("Hardware: Independent Flip") instead of being composed. Advertises `SupportDirectFlip`, the segment `DirectFlip` flag and `FlipIndependent \| DdiPresentForIFlip`, and counts every flip (`Idf*`). Read at StartDevice; reboot per change. `HwCursor`, when not set, follows it (the hardware cursor is on with independent flip, off without). [independent-flip.md](../guest/windows/docs/independent-flip.md) sections 11 and 13 |

On by default, with a way back: `NvkRmFencePresent` (composed NVK presents
retire on the RM fence; 0, or `HELIOS_NVK_RM_FENCE_PRESENT=0`, restores the
CPU wait).

## Display

- **Mode.** At start the KMD asks the host for scanout 0's EDID (`GET_EDID`)
  and takes the native timing from it, DisplayID Type VII (or I) first, then
  the base block's detailed timing, for the VidPn mode, the monitor modes and
  the vsync timer, and gives the EDID to Windows unchanged. A host without
  `GET_EDID` gets the old behaviour: the size from `GET_DISPLAY_INFO`, a
  generated EDID (1920×1080 when the size does not fit a 128-byte EDID),
  60 Hz.
- **Refresh rates.** The monitor and target mode sets offer the host's rate
  (preferred) and 144, 120 and 60 Hz below it; Display Settings can switch
  between them, and the KMD follows the committed mode's rate (primary
  surface and vsync heartbeat). Rates go to WDDM in lowest terms
  (240000/1000 → 240/1).
- **Connector.** The monitor reports DisplayPort (`OutputTech`, below); as
  analog VGA, Windows applied analog frequency rules and stayed at 60 Hz.
- **Topology.** Windows keeps the boot display adapter as DISPLAY1 and adds
  Helios as a second monitor. The installer registers a logon task that runs
  `Set-HeliosDisplay.ps1`, which makes Helios the only active display and
  leaves the other adapter enabled as a fallback
  ([packaging README](../guest/windows/packaging/windows/README.md#display-topology);
  `ManageDisplay=0` opts out).

## GPU stats tray

`conduit attach` adds a virtio-serial channel, `org.conduit.stats.0`, whose host
end is a unix socket QEMU binds (`stats.sock` in the VM's runtime folder), and
installs `conduit-stats@NAME.service`. That unit runs `conduit _stats NAME`:
once a second it writes one JSON line of the host GPU's NVML readings (name,
driver, temperature, load, VRAM, clocks, power, fan, P-state, PCIe, encoder) to
the socket, and reconnects when the VM restarts. The channel is added when
the domain is defined, so a VM attached earlier needs `conduit attach NAME`
again and one cold restart. The guest needs the VirtIO serial driver (the one
the QEMU guest agent uses).

The package installs **Conduit GPU** (`guest/windows/tools/conduit-gpu-tray`,
built for `x86_64-pc-windows-gnu`, about 350 KB, no runtime) to
`Program Files\Conduit` and starts it at logon through the machine Run key. It
reads `\\.\Global\org.conduit.stats.0`, shows the temperature (or load) as a
colour-coded number in the tray, and opens a popup on click: load, power and
temperature, 60-second graphs and VRAM, clock, power and fan bars. The
right-click menu has Show, which metric the icon shows, Start with Windows,
and Exit. Until the first line arrives the popup says "Waiting for Conduit host
feed".

## Registry knobs

KMD knobs are `REG_DWORD` values under
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, read when the
adapter starts, so `pnputil /restart-device` applies them; the KMD writes
counters of what it did to the same key. All are in
`guest/windows/kmd_render/src/diag.rs` (`knobs`); the ones for display work:

| Value | Default | |
|---|---|---|
| `OutputTech` | 1 | connector type: 1 DisplayPort, 2 HDMI, 3 DVI, 4 internal, 0 analog VGA (HD15). Counter `OutTech` |
| `VsyncRateMhz` | 0 | diagnostic: force the vsync heartbeat rate in mHz (60000 = 60 Hz) whatever the mode says; 0 follows the mode. Counter `VsRate` |
| `FlipCapsX` | 0 | diagnostic: replace the advertised `DXGK_FLIPCAPS` word (0 = the driver's own) |
| `FlipQueueN` | 1 | `MaxQueuedFlipOnVSync`, flips dxgkrnl may queue. Counter `FlipQueV` |
| `DiagLevel` | 0 | breadcrumb ring in the same key; 0 off |

Counters for the display mode: `EdTimW`/`EdTimH`/`EdTimR`/`EdTimD` (what the
host EDID said, refresh in mHz, and from which block), `AdoptPath` (1 host
EDID, 2 `GET_DISPLAY_INFO` size, 3 fallback), `VpRfr`/`MmRfr` (the rate
offered), `VpCRf` (the committed rate).

UMD knobs are under `HKLM\SOFTWARE\Helios` and read by each new process
(`guest/windows/umd/src/knobs.rs`): `UmdTrace` (1: per-call trace, default
off), `ScanoutAcquire` (0 disables the GPU-side wait before rewriting a
buffer the host is still reading, default on), `UmdD3D12` (0 disables
D3D12).

## Performance

- NVK's knobs and its diagnostics (`NVK_PASS_PROFILE`, `NVK_SHADER_STATS`,
  `NVK_WAIT_STATS`) are in [guest/nvk-rm/README.md](../guest/nvk-rm/README.md).
- For processes on Venus, fence latency decides the frame rate; see
  [VENUS.md](VENUS.md) "Fence latency" for the virglrenderer patch Venus
  needs on NVIDIA and the latency lines both host processes log.
- `guest/windows/tools/scanout_timeline_dump.c` reads the KMD's scanout
  timeline (an escape; it never submits work) to CSV around a workload, to
  see where frame time goes at high refresh rates.
