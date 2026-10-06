# Conduit

**Share your NVIDIA GPU with a virtual machine, and see its desktop on yours, at full speed.**

Conduit lets a Linux VM use your real NVIDIA graphics card (games, Vulkan,
OpenGL, CUDA, video encoding) while your own desktop keeps using it too. The
VM's screen appears as a normal window on your desktop. Frames go straight
from GPU memory to your screen: no copying, no video compression.

- One GPU, shared. No second graphics card, no passthrough, no vGPU license.
- Up to your monitor's refresh rate (tested at 240 Hz).
- Mouse, keyboard, clipboard (copy/paste both ways) and sound just work.
- Play it from another computer, phone or TV with [Moonlight](https://moonlight-stream.org).
- Start, pause and stop VMs from virt-manager / virsh, or with `conduit` commands.

### What works

| | |
|---|---|
| Vulkan, OpenGL, EGL (apps, games) | ✅ |
| CUDA (incl. large managed and pinned memory) | ✅ |
| Video encode/decode (NVENC/NVDEC) | ✅ |
| Desktop in a window, up to 240 Hz, zero-copy | ✅ |
| Clipboard both ways, sound (speakers + mic) | ✅ |
| Streaming to Moonlight (AV1/HEVC/H.264, up to 240 fps) | ✅ |
| virt-manager / virsh (start, pause, reboot, stop; attach to existing VMs) | ✅ |
| Windows VMs (D3D11/12, Vulkan) | experimental, opt-in (`--venus`, built by hand: [Windows guests](docs/WINDOWS.md)) |
| NVK (Mesa's open Vulkan driver) on NVIDIA's kernel driver, in a Linux VM | experimental, opt-in (`NVK_RM=1`: [guest/nvk-rm](guest/nvk-rm/README.md), [librmclient](guest/rmclient/README.md)) |

> Conduit is early software, tested mainly on an RTX 5090 with Ubuntu 24.04
> and Hyprland. Expect rough edges. Windows guests are experimental (`--venus`),
> and the guest driver is test-signed ([docs/WINDOWS.md](docs/WINDOWS.md),
> [Roadmap](docs/ROADMAP.md)).

## What you need

| | |
|---|---|
| Host | Linux, x86-64, with KVM (`ls /dev/kvm` works) |
| GPU | NVIDIA, Turing (RTX 20xx) or newer |
| Host driver | NVIDIA **open** kernel modules, 580 or newer, a release Conduit has ABI tables for (580.178.04, 595.71.05, 595.104.02, 610.57.04, 615.71.09) |
| Desktop | Any Wayland desktop (GNOME, KDE, Hyprland, Sway, …) |
| VM | Linux, kernel 6.4 or newer: Ubuntu 24.04 recommended (`conduit create`); `conduit attach` also sets up Debian and Arch-based VMs (Arch, Omarchy, EndeavourOS, Manjaro). Windows 11: experimental ([docs/WINDOWS.md](docs/WINDOWS.md)) |

## Quick start

1. **Download** the package for your Linux from the
   **[latest release](https://github.com/olealgoritme/conduit/releases/latest)** and install it (table below).
2. **Check your computer:** `conduit doctor`. Every line should say `ok`;
   if not, it tells you what to fix.
3. **Make a VM:** `conduit create myvm` (downloads Ubuntu, installs a GNOME
   desktop and the GPU driver; takes a few minutes).
4. **Open it:** `conduit view myvm`. A window with the VM's desktop appears.

## Install

Download the package for your system from the
**[latest release](https://github.com/olealgoritme/conduit/releases/latest)**, then:

| System | Install |
|---|---|
| Ubuntu / Debian / Pop!_OS / Mint | `sudo apt install ./conduit_*_amd64.deb` |
| Fedora / RHEL / openSUSE | `sudo dnf install ./conduit-*.x86_64.rpm` (openSUSE: `sudo zypper install ./conduit-*.rpm`) |
| Arch / Manjaro / EndeavourOS | `sudo pacman -U ./conduit-*-x86_64.pkg.tar.zst` |
| NixOS | `nix run github:olealgoritme/conduit` (flake) |
| Anything else | `tar xf conduit-*-x86_64-linux.tar.gz && sudo ./conduit/install.sh` |

Then:

```bash
conduit doctor            # checks KVM, the NVIDIA driver, your desktop
conduit create myvm       # a ready-made Ubuntu VM with GNOME
conduit view myvm         # open it in a window
```

That's it. The VM gets your monitor's resolution and refresh rate
automatically.

### Everyday commands

| Command | What it does |
|---|---|
| `conduit view myvm` | Open the VM in a window (starts it if needed) |
| `conduit up myvm` | Start in the background (`conduit view myvm` attaches a window any time) |
| `conduit down myvm` | Shut down cleanly; forced off after 30 s (`--timeout N`, `--force` = now) |
| `conduit shutdown myvm` / `conduit reboot myvm` | Press the power button and wait / restart the guest cleanly |
| `conduit pause myvm` / `conduit resume myvm` | Freeze / continue the VM |
| `conduit reset myvm` / `conduit poweroff myvm` | Hard reset / turn off immediately |
| `conduit doctor myvm` | Check everything one VM needs |
| `conduit status` | What is running |
| `conduit ssh myvm` | A terminal inside the VM |
| `conduit logs myvm` | Logs when something goes wrong |
| `conduit list` | Your VMs |

> **VM runner:** the package brings its own QEMU 11.1 (in `/opt/conduit`), so
> it works on systems whose QEMU is too old (for example Ubuntu 24.04). Your
> system QEMU is never touched. The VM gets a sound card (speakers and
> microphone) through your desktop's PipeWire or PulseAudio. If that QEMU is
> missing, Conduit falls back to its small built-in runner, which has no sound
> (`--vmm builtin` picks it on purpose).

### Build from source

```bash
git clone https://github.com/olealgoritme/conduit && cd conduit
make deps      # build dependencies (asks for sudo)
make           # backend, VM runner, CLI, viewer, guest module, bundled QEMU
make test      # offline tests
make install   # installs to /opt/conduit (+ /usr/local/bin/conduit)
```

Maintainers: `make release` tags the next version (`release-minor`, `release-major`,
or `packaging/release.sh X.Y.Z`); GitHub then builds every package for that tag.

## Keys inside the viewer

| Keys | What it does |
|---|---|
| `Ctrl+Alt+F` | Fullscreen on/off |
| `Ctrl+Alt+G` | Capture the mouse (for games) / release it |
| `Ctrl+Alt+O` | Performance overlay on/off (fps, frame times, latency) |
| `Ctrl+Alt+D` | Direct mode: lowest latency (fullscreen, no overlay) |
| `Ctrl+Alt+R` | Switch between the VM's resolution following the window (default) and scaling a fixed resolution into it |

## Play it from another computer (Moonlight)

Stream the VM to any device with [Moonlight](https://moonlight-stream.org)
(Windows, macOS, Linux, Android, iOS, TVs, Steam Deck). No Sunshine needed:
Conduit is the server.

1. On this computer: `conduit stream myvm`
2. In Moonlight: click **+**, type this computer's IP (or `localhost` on the
   same machine). Moonlight shows a **PIN**.
3. On this computer: `conduit stream pair 1234` (the PIN you see).
4. In Moonlight: click the computer, then the app named after your VM (**myvm**).

**Settings** (resolution, **FPS** up to 240, **bitrate**, **codec** AV1/HEVC/H.264):
the **gear icon ⚙** on Moonlight's start screen, before you start the stream.
The VM switches to the resolution you pick automatically.

In a stream: **Ctrl+Alt+Shift+Q** quits, **Ctrl+Alt+Shift+Z** captures or
releases mouse and keyboard. Keyboard, mouse and gamepads go to the VM.
`conduit view myvm` and `conduit stream myvm` can run at the same time, started in either order.

`conduit stream myvm --service` keeps streaming in the background. Conduit's
own viewer can connect too (`conduit stream myvm --link` here,
`conduit remote THIS-PC --lossless` there; lossless needs ~1 Gbit/s+).
Details: [docs/STREAMING.md](docs/STREAMING.md).

## virt-manager and virsh

Conduit VMs are ordinary libvirt VMs: start, pause, resume, reboot and shut
them down from virt-manager or `virsh` like any other, while the GPU, the
`conduit view` window and the clipboard keep working. They live in libvirt's
**user session** (`qemu:///session`, QEMU runs as you). In virt-manager, open it
once with **File > Add Connection > Hypervisor: QEMU/KVM user session**.

**A) Beginner: a new VM**

```bash
conduit create myvm      # builds it and registers it with libvirt (--no-libvirt: don't)
conduit view myvm        # starts it and opens its screen
```

**B) A VM you already have in virt-manager**

```bash
conduit attach myvm      # backs up its definition, adds the GPU, installs the guest driver
# start it in virt-manager (or: virsh -c qemu:///session start myvm)
conduit view myvm        # its screen, in a Conduit window
conduit detach myvm      # later, if you want: the original definition comes back
```

`attach` installs the guest driver through the QEMU guest agent when the VM
runs one (Debian/Ubuntu or Arch-based guests; on Arch install it first:
`sudo pacman -S qemu-guest-agent`, then reboot the VM); otherwise it prints
the one command to run. It works for VMs in
`qemu:///system` too (`conduit attach myvm -c qemu:///system`). Conduit's QEMU
has no SPICE: attach replaces the VM's SPICE display with Conduit's boot
console (firmware, boot menu and disk-unlock prompt in the Conduit window).
A VM made before this: `conduit libvirt enable myvm` (and `disable` to undo).

The window and the VM have separate lives: closing the window leaves a VM
running unless `conduit view` itself started it (then it shuts down, unless
you pass `--keep-running` or set `conduit config set view.close_stops_vm false`).
`conduit view myvm` reattaches at any time. Each VM also gets an app-menu entry,
"myvm (Conduit VM)".

Not supported for these VMs: saving their memory to disk (virt-manager "Save",
`virsh managedsave`), snapshots with memory, and migration. The GPU's state
lives in the host driver, and libvirt refuses these. `conduit doctor myvm`
checks the whole chain. Details: [docs/LIBVIRT.md](docs/LIBVIRT.md).

## How it works (short version)

```
 VM: apps → NVIDIA's own driver libraries → Conduit guest driver
                                              │  (virtio)
 Host:                                Conduit backend → your NVIDIA driver → GPU
                                              │  (zero-copy frame handoff)
 Host desktop:                        Conduit viewer window
```

The VM runs NVIDIA's real user-space driver. Conduit passes its requests to
your host's driver, so the VM renders on the real GPU. Finished frames already
live in GPU memory, so the viewer shows them directly. Details:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Is it safe?

The VM runs as an unprivileged, sandboxed process on your host and cannot
touch your display settings or other apps' GPU work. It is not hardware
isolation like a dedicated GPU: only run VMs you trust. See
[docs/SECURITY.md](docs/SECURITY.md).

## Troubleshooting

| Problem | Try |
|---|---|
| `conduit view` says no KVM | Enable virtualization (VT-x / AMD-V) in your BIOS |
| Black window | `conduit status myvm`, then `conduit logs myvm` |
| Low fps when idle | Normal: the VM only draws when something changes |
| Short stutters or freezes in the VM, benchmark numbers that swing | The host is starving it (swap, builds on its cores): see [host tuning](docs/HOST-TUNING.md) |
| "not enough free memory" | Close apps or give the VM less RAM; `--no-mem-check` skips the check |
| Overlay says `COMPOSITED` in fullscreen | Your desktop composites the window; see [direct scanout](docs/SCANOUT.md). On Hyprland: `conduit view myvm --tune-hyprland`, then `Ctrl+Alt+F`, `Ctrl+Alt+D` |
| Stream: "ports are in use" | Another `conduit stream` or Sunshine is running; stop it, or use `--port 48089` |
| Moonlight: Windows/Super key does nothing | Moonlight settings → *Capture system keyboard shortcuts* → **Always** |
| Steam window errors | Steam is an X11 app and needs XWayland in the VM's session; open an issue with `conduit logs myvm` |
| An app fails or is slow on the GPU inside the VM | `conduit trace myvm --follow --filter errors`, or `conduit trace myvm --summary` for latency; see [docs/TRACING.md](docs/TRACING.md) |
| Anything else | `conduit doctor myvm` and `conduit logs myvm` |

## Credits

Conduit builds on [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu)
and [nesbox](https://github.com/nestrilabs/nesbox) (Nestri Labs), the display
broker from [nvkvm-pv](https://github.com/reindertpelsma/nvkvm-pv), and ideas
from gVisor's nvproxy and [kayfabe](https://github.com/reindertpelsma/kayfabe).
The Windows guest components come from [Helios](https://github.com/winboat-org/helios)
by the WinBoat project (rupansh, TibixDev), with
[DXVK](https://github.com/doitsujin/dxvk),
[vkd3d-proton](https://github.com/HansKristian-Work/vkd3d-proton),
[Mesa](https://mesa3d.org) (Venus) and
[virglrenderer](https://gitlab.freedesktop.org/virgl/virglrenderer).
Licenses: see [LICENSE](LICENSE) and the `NOTICE` files in each component.

NVIDIA, GeForce and RTX are trademarks of NVIDIA Corporation. Conduit is not
affiliated with NVIDIA.
