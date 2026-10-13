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
22.22.405.84 (`guest/windows/kmd_render/driver-version.env`, the only place
the version is set). The adapter shows as "Conduit Helios", the monitor as
"Conduit".

**Video memory the guest sees.** The adapter's dedicated video memory (dxdiag,
Task Manager, DXGI `DedicatedVideoMemory`, the budgets games size textures
from) is the host GPU's video memory: the largest device-local heap the
host's Vulkan driver lists, read when the driver starts, up to 64 GiB. An
RTX 5090 shows about 32 GB. It does not depend on `--venus-hostmem-mib`, which
sizes only the CPU-visible blob window, and `--vram-limit-mib` does not lower
it. The figure is accounting: VidMm charges
each allocation of host GPU memory against it, and the host GPU (or
`--vram-limit-mib`) decides what can actually be allocated. "Shared memory" is
Windows' own half-of-RAM figure. `VidMmVramMB` overrides the size
([Registry knobs](#registry-knobs)); `VidVram`, `VidVSrc` and `VidVHeap` in the
service key show what the driver chose and from where.

**Install from a release.** Every release carries
`conduit-windows-gpu-driver-<version>.zip` (built and test-signed by
`.github/workflows/windows.yml`). Unzip it in the guest and, from an
Administrator prompt in that folder:

```bat
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The first run turns on test-signing and asks for a reboot; run it again after
the reboot (it trusts the package's test certificate, installs the driver and
sets `DwmIcd=nvk`), then reboot once more. No other registry value needs to be
set: the code defaults are the recommended configuration
([Registry knobs](#registry-knobs)). `-VenusDesktop` keeps DWM on Venus.

**Guest extras.** The release's `conduit-windows-gpu-setup-<version>.zip`
holds `HeliosSetup.exe`, built from the same driver. Run once after
`install.ps1`, it adds the GPU tray, the shared-folder mounts, the display
topology task and the virtio-input driver, and keeps `DwmIcd`
([guest/windows/packaging/windows/README.md](../guest/windows/packaging/windows/README.md)).

**Building the driver yourself: the local build VM**
([guest/windows/ci/vm/README.md](../guest/windows/ci/vm/README.md)):

```sh
git submodule update --init --recursive guest/windows/third_party/dxvk guest/windows/third_party/vkd3d-proton
guest/nvk-rm/windows/stage-helios-package.sh          # NVK + Zink, cross-built -> dist/nvk-windows
WIN_SSH=user@127.0.0.1 guest/windows/ci/vm/win-build.sh Release
```

The package is signed with a development certificate
(`helios-dev-test.cer`, in the package). Copy `install.ps1` next to it and
run it as above, or, from an administrator prompt:

```bat
bcdedit /set testsigning on
certutil -addstore -f Root helios-dev-test.cer
certutil -addstore -f TrustedPublisher helios-dev-test.cer
pnputil /add-driver helios_kmd_render.inf /install
```

then reboot. pnputil keeps an installed driver of the same version: bump
`HELIOS_KMD_VERSION` for every build installed over an earlier one.

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

Off in the code while new; each one was measured on the test machine
([NVK-ROADMAP.md](NVK-ROADMAP.md)). `install.ps1` sets `DwmIcd=nvk` (unless
run with `-VenusDesktop`); `venus.guest_blobs` is a host setting. Defaults of
these and every other knob: [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md).

| Where | Setting | What it does |
|---|---|---|
| Host | `conduit config set venus.guest_blobs true` | the backend serves guest-memory blobs (`--venus-guest-blobs`, [VENUS.md](VENUS.md) "Guest-memory blobs"): Venus copy destinations over the guest's own pages, for the KMD's windowed Present. Applies when the VM's backend next starts. Needs the patched virglrenderer (patch 0002) |
| Guest, `HKLM\SOFTWARE\Helios` | `DwmIcd` = `nvk` (REG_SZ) | DWM on NVK, read only by `dwm.exe` when it starts. A crash-loop guard sends DWM back to Venus after `DwmNvkMaxStarts` (2) starts within `DwmNvkGuardSeconds` (600) ([dwm-on-nvk.md](dwm-on-nvk.md) 4.1). `install.ps1` sets it unless `-VenusDesktop` is given |

On by default, with a way back (the values in [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md)):

| Where | Setting | What it does |
|---|---|---|
| Guest, KMD service key | `IndepFlip` (0 off; 2 also completes the unregistered DMA flip instead of failing it) | independent flip: a borderless or exclusive full-screen flip-model window is scanned out from its own buffers ("Hardware: Independent Flip") instead of being composed. Advertises `SupportDirectFlip`, the segment `DirectFlip` flag and `FlipIndependent \| DdiPresentForIFlip`, and counts every flip (`Idf*`). Read at StartDevice; reboot per change. `HwCursor`, when not set, follows it (the hardware cursor is on with independent flip, off without). [independent-flip.md](../guest/windows/docs/independent-flip.md) 13.10 |
| Guest, `HKLM\SOFTWARE\Helios` | `DirectFlipSupport` (or `HELIOS_DIRECT_FLIP_SUPPORT` per process) | what the D3D11.1 `CheckDirectFlipSupport` DDI answers, which DWM uses for independent flip: 5 = yes when dxgkrnl reports DirectFlip for the adapter and size and format match, two different 8-bit scan-out formats also pairing (an R8G8B8A8 game on the B8G8R8A8 desktop); 1 = the same with equal formats only; 3 = as 1 and only for formats the KMD scans out as they are; 2 = whenever size and format match (test lever); 0 = never |
| Guest, KMD service key | `ImmFlip` (a bit mask) | immediate flips (interval 0, `ALLOW_TEARING`, MMIO `FlipImmediate`, and a promoted swap chain's flips) are sent to the host at once (bit 0), retired as soon as their address is published (bit 1), `FlipImmediateMmIo` is reported (bit 2), and promoted flips count as immediate (bit 3). 0 paces them like composed flips. [independent-flip.md](../guest/windows/docs/independent-flip.md) 13.10 |
| Guest, `HKLM\SOFTWARE\Helios` | `NvkRmFencePresent` (or `HELIOS_NVK_RM_FENCE_PRESENT`) | composed NVK presents retire on the RM fence; 0 restores the CPU wait |
| Guest, KMD service key | `ForeignFlip` (0 off) | the KMD flips the NVK DWM's swap-chain buffers to the scanout itself, zero-copy (block-linear, NVIDIA modifier). With 0 an NVK DWM's desktop reaches the host as a LINEAR buffer with alpha 0 and shows black. `FlipAnnForeign` and `FfAsyncWin` (2) follow it |
| Guest, KMD service key | `ForeignCopy` (0 off) | the KMD copies a foreign (NVK) Present source into its destination (windowed NVK content composed by the NVK DWM); 0 skips such copies (`FcOff`) |
| Guest, KMD service key | `BltAsync` (0 off) | such a copy returns from the Present without a CPU wait |
| Guest, KMD service key | `RedirVram` (0 off) | CDD's CPU-written GDI surfaces in the aperture segment; the UMD leaves the windowed NVK copy to the KMD (`NvkSkipBltCopy` follows it) |
| Guest, KMD service key | `MsiMode` (1 INTx; 0 follows the INF) | MSI-X interrupts. The INF writes `MSISupported` = 0 at every install, so the first start after an install runs on INTx and the driver asks for MSI-X for the next one; a start that never becomes healthy latches INTx ([msi-interrupts.md](../guest/windows/docs/msi-interrupts.md)) |

These five defaults live in `guest/windows/kmd_logic/src/shipping_defaults.rs`; a fresh install
needs no KMD service-key values.

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
- **Connector.** The monitor reports DisplayPort (`OutputTech`, [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md)); as
  analog VGA, Windows applied analog frequency rules and stayed at 60 Hz.
- **Topology.** Windows keeps the boot display adapter as DISPLAY1 and adds
  Helios as a second monitor. The installer registers a logon task that runs
  `Set-HeliosDisplay.ps1`, which makes Helios the only active display and
  leaves the other adapter enabled as a fallback
  ([packaging README](../guest/windows/packaging/windows/README.md#display-topology);
  `ManageDisplay=0` opts out).

## Shared folders

Every attached VM shares `~/Conduit/NAME` from the host (more with
`conduit share add`, docs/CLI.md). Each folder is a virtiofs device tagged
`conduit-NAME`. The guest needs the VirtIO FS driver (virtio-win), the
VirtIO-FS service binary (`virtiofs.exe`, installed with the virtio-win guest
tools) and WinFsp. The package installs WinFsp from the MSI it carries when the
guest has none (the MSI is not kept in the repository: CI downloads the pinned
release from https://github.com/winfsp/winfsp/releases, checks its SHA-256 and
passes it to `ci/windows/Assemble-Package.ps1 -WinFspMsi`).

`Install-ConduitShares.ps1` (run by the installer, or by hand as administrator
with `-WinFspMsi path\to\winfsp.msi`) registers the startup task
`ConduitShares`, which runs `Mount-ConduitShares.ps1` as SYSTEM from
`Program Files\Conduit`. The VirtIO-FS service serves one device per process,
so the script starts one `virtiofs.exe -t conduit-NAME -m X:` for every
`conduit-*` device that has no such process yet (it reads the `-t` tag from the
running processes' command lines), stops a second process for the same tag,
rescans every 5 seconds (a folder added while Windows runs appears without a
reboot), and gives each tag a drive letter counting down from Z:, remembered under
`HKLM\SOFTWARE\Conduit\ShareDrives`. The drive's label is the tag
(`conduit-NAME`), which is what virtiofs reports as the volume name. The log is
`%ProgramData%\Conduit\shares.log`. A stock `VirtioFsSvc` without `-t` would
take the first device, so the installer sets it to manual start.

The tray app lists the mounted folders under Shared folders in its menu (each
opens in Explorer) and as Open default share, shows a Shared folders row in the
popup, and copies files dropped on the popup into the default folder (in a
helper started with the user's normal, non-elevated token). A drive letter
counts as a share only while the volume on it is that share (virtiofs file
system, the tag as its label). The mounts belong to SYSTEM, so every user of
the VM sees the shared folders, with the same access to the host folders. Turn on
Keep popup open in the menu to drag files onto it from Explorer. While the
default folder (`Conduit`) has a drive letter, the tray app also keeps a
shortcut `Conduit host.lnk` to its root in the user's SendTo folder, so
Explorer's **Send to → Conduit host** copies the selection there; it is
removed when the folder is gone and by `Uninstall-Helios.ps1`.

**Windows 11 context menu.** Windows 11's right-click menu shows Send to only
under Show more options, so the package also adds **Send to Conduit host**,
with the Conduit icon, at the top level of the menu for files and folders
(one or many). It is an `IExplorerCommand` in-process COM server,
`conduit_shell_menu.dll` (`guest/windows/tools/conduit-shell-menu`), that
Explorer loads through a sparse package, `ConduitShellMenu.msix` (identity
`Conduit.ShellMenu`, `desktop4:FileExplorerContextMenus` plus a
`com:SurrogateServer` class; its external location is `Program Files\Conduit`,
where the DLL and the tray exe live). The command shows while a Conduit shared
folder is mounted (`HKLM\SOFTWARE\Conduit\ShareDrives`) and hands the selected
paths to `conduit-gpu-tray.exe --send-list <file>`, which copies them into the
default folder with Explorer's copy dialog, as the Send To shortcut does. The
msix is signed with the package certificate, and its publisher is that
certificate's subject, so it installs because the installer trusts the
certificate (`Root`, `TrustedPublisher`). Packages register per user: the tray
app registers it when it starts (through the Windows package manager API with
the install folder as the external location, once per user and package file;
the logon task covers every user), and `Uninstall-Helios.ps1` removes it for
all users. The command starts the tray exe without handing it any of the
host process's handles. To build it by hand on a
Windows machine with the SDK: `cargo build --release` in
`tools/conduit-shell-menu`, then `New-ShellMenuPackage.ps1 -Publisher
"CN=<signing certificate subject>" -Version <a.b.c.d> -OutFile
ConduitShellMenu.msix` and `signtool sign /fd SHA256 ...` with that
certificate.

## GPU stats tray

**Conduit GPU** is a small tray app (`guest/windows/tools/conduit-gpu-tray`,
built for `x86_64-pc-windows-gnu`, under 1 MB, no runtime) that shows the
host GPU's state inside the VM.

**Data path.** `conduit attach` adds a virtio-serial channel,
`org.conduit.stats.0`, whose host end is a unix socket QEMU binds (`stats.sock`
in the VM's runtime folder), and installs `conduit-stats@VM.service`. That unit
runs `conduit _stats VM`: once a second it writes one JSON line of the host
GPU's NVML readings (name, driver, temperature, load, VRAM, clocks, power, fan,
P-state, PCIe, encoder) to the socket, and reconnects when the VM restarts. The
channel is added when the domain is defined, so a VM attached earlier needs
`conduit attach VM` again and one cold restart. The guest needs the VirtIO
serial driver (the one the QEMU guest agent uses); the app opens
`\\.\Global\org.conduit.stats.0`.

**Install and uninstall.** The driver package copies the exe, the context
menu's `conduit_shell_menu.dll` and `ConduitShellMenu.msix` to
`Program Files\Conduit` and registers the logon task `ConduitGpuTray`;
`Uninstall-Helios.ps1` stops the app and removes the task, the folder, the
Send to shortcut and the context menu package. Run as administrator without
the package, the exe can
register the task itself from its menu (Start with Windows).

**Start at logon.** The VirtIO serial port is admin-only, so the app has to
run elevated. The task `ConduitGpuTray` runs it at every logon with the
highest privileges for members of `BUILTIN\Users`, which starts it elevated
without a UAC prompt for administrators, one copy per user session. Started by
hand by an administrator without elevation, the app relaunches itself through
that task (one UAC prompt if the task is missing) and exits once the elevated
copy's window is up; if none shows within a few seconds it keeps running. A
standard user's copy stays up and says the stats channel needs administrator
rights. Start with Windows registers the task only for the exe in
`Program Files\Conduit`.
On Windows 11 the first run pins the tray icon next to the clock
(`HKCU\Control Panel\NotifyIconSettings`, `IsPromoted`); hiding it afterwards
sticks.

**Use.** The icon shows the temperature (or load) as a colour-coded number. A
click opens a popup: load, power and temperature, 60-second graphs, and VRAM,
clock, power and fan bars, plus the Shared folders row. The right-click menu
has Show (which metric the icon shows), the shared folders, Open default share,
Keep popup open, Start with Windows, and Exit.

**Troubleshooting.** The popup says "Waiting for Conduit host feed" until the
first line arrives. If it stays:

- On the host, `systemctl --user status conduit-stats@VM` must be active
  (`systemctl --user enable --now conduit-stats@VM.service` starts it).
- The VM must have been restarted (cold) after `conduit attach`, which adds the
  channel; `virsh dumpxml VM` shows `org.conduit.stats.0`.
- In the guest, the VirtIO serial driver must be installed (Device Manager:
  "VirtIO Serial Driver" under System devices).
- "Access denied" means the app is not elevated; start it from the
  `ConduitGpuTray` task (`schtasks /run /tn ConduitGpuTray`).

## Registry knobs

Every knob the guest reads, with its type, default and class, is in
[WINDOWS-KNOBS.md](WINDOWS-KNOBS.md), generated from the code. The
recommended configuration is the code defaults: no registry value needs to
be set beyond the `DwmIcd` that `install.ps1` writes. KMD knobs are
`REG_DWORD` values under
`HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`, mostly read
when the adapter starts (`pnputil /restart-device` applies them); the KMD
writes counters of what it did to the same key. UMD knobs are under
`HKLM\SOFTWARE\Helios` and read once by each new process.

Counters for the display mode: `EdTimW`/`EdTimH`/`EdTimR`/`EdTimD` (what the
host EDID said, refresh in mHz, and from which block), `AdoptPath` (1 host
EDID, 2 `GET_DISPLAY_INFO` size, 3 fallback), `VpRfr`/`MmRfr` (the rate
offered), `VpCRf` (the committed rate).

## Performance

- NVK's knobs and its diagnostics (`NVK_PASS_PROFILE`, `NVK_SHADER_STATS`,
  `NVK_WAIT_STATS`) are in [guest/nvk-rm/README.md](../guest/nvk-rm/README.md).
- For processes on Venus, fence latency decides the frame rate; see
  [VENUS.md](VENUS.md) "Fence latency" for the virglrenderer patch Venus
  needs on NVIDIA and the latency lines both host processes log.
- `guest/windows/tools/scanout_timeline_dump.c` reads the KMD's scanout
  timeline (an escape; it never submits work) to CSV around a workload, to
  see where frame time goes at high refresh rates.
- `guest/windows/tools/rawcount.c` counts the mouse reports Windows
  delivers per second, per device, in the background (raw input). With
  `host/latency/vmouse.c` it measures the input path's report rate; see
  [INPUT.md](INPUT.md) "Measuring".
- Windowed apps (DWM composes them) learn that their frame was copied to the
  desktop from a per-device event the KMD signals. The KMD's table holds 64
  of these (DWM uses about a dozen; most apps one to three). The `AqLive`
  counter in the KMD service key is how many are in use, `AqRgF` how many
  registrations were refused. A refused app polls instead: every 1 ms while
  it has a frame waiting, every 10 ms when idle. Its UMD log
  (`%ProgramData%\Helios\umd-<pid>.log`) says `event table FULL` when that
  happens.
