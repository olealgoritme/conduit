# Windows guests (experimental)

A Windows 11 guest renders on the host GPU through Venus: D3D11 through
DXVK, D3D12 through vkd3d-proton and Vulkan through Mesa's Venus driver in
the guest, executed by the host's NVIDIA Vulkan driver in `conduit-venus`.
The guest drivers come from Helios ([guest/windows/HELIOS.md](../guest/windows/HELIOS.md));
the host side is in [VENUS.md](VENUS.md). It is opt-in (`--venus`),
tested on one machine (RTX 5090, a 5120×1440 240 Hz monitor), and has the
limits listed in [KNOWN-ISSUES.md](KNOWN-ISSUES.md#windows-guests-venus).

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
USB tablet ([SCANOUT.md](SCANOUT.md#boot-console)).

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

Build it with `.github/workflows/windows.yml` (the full package:
`HeliosSetup.exe` with the KMD, UMDs, Mesa Venus ICD, Zink, loaders) or, for
the driver alone, in a local build VM ([guest/windows/ci/vm/README.md](../guest/windows/ci/vm/README.md)).
Install the full package once with `HeliosSetup.exe`
([guest/windows/packaging/windows/README.md](../guest/windows/packaging/windows/README.md));
a driver-only build then replaces the driver with `pnputil` (version bump
needed, see the build VM README). The current driver is 22.22.297.0
(`guest/windows/kmd_render/driver-version.env`). The adapter shows as
"Conduit Helios", the monitor as "Conduit".

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

- Fence latency decides the frame rate; see [VENUS.md](VENUS.md) "Fence
  latency" for the virglrenderer patch that took Unigine Heaven from 36 to
  about 150 fps, and for the latency lines both host processes log.
- `guest/windows/tools/scanout_timeline_dump.c` reads the KMD's scanout
  timeline (an escape; it never submits work) to CSV around a workload, to
  see where frame time goes at high refresh rates.
