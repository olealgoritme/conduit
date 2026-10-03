# Conduit

**Share your NVIDIA GPU with a virtual machine, and see its desktop on yours, at full speed.**

Conduit lets a Linux VM use your real NVIDIA graphics card (games, Vulkan,
OpenGL, CUDA, video encoding) while your own desktop keeps using it too. The
VM's screen appears as a normal window on your desktop. Frames go straight
from GPU memory to your screen: no copying, no video compression.

- One GPU, shared. No second graphics card, no passthrough, no vGPU license.
- Up to your monitor's refresh rate (tested at 240 Hz).
- Mouse and keyboard just work when the window is focused.
- Close the window and the VM shuts down cleanly.

> **Status: early.** Works on the developer's machine (RTX 5090, Ubuntu 24.04,
> Hyprland). Expect rough edges. Windows guests are not supported yet
> (see [Roadmap](docs/ROADMAP.md)).

## What you need

| | |
|---|---|
| Host | Linux, x86-64, with KVM (`ls /dev/kvm` works) |
| GPU | NVIDIA, Turing (RTX 20xx) or newer |
| Host driver | NVIDIA **open** kernel modules, 580 or newer |
| Desktop | Any Wayland desktop (GNOME, KDE, Hyprland, Sway, …) |
| VM | Linux (Ubuntu 24.04 recommended) |

## Install

Download the package for your system from the
[latest release](../../releases/latest), then:

| System | Install |
|---|---|
| Ubuntu / Debian / Pop!_OS / Mint | `sudo apt install ./conduit_*_amd64.deb` |
| Fedora / RHEL / openSUSE | `sudo dnf install ./conduit-*.x86_64.rpm` (openSUSE: `sudo zypper install ./conduit-*.rpm`) |
| Arch / Manjaro / EndeavourOS | `sudo pacman -U ./conduit-*.pkg.tar.zst` |
| NixOS | `nix run github:olealgoritme/conduit` (flake) |
| Anything else | `tar xf conduit-*-x86_64-linux.tar.gz && sudo ./conduit/install.sh` |

Then:

```bash
# Create a ready-made Ubuntu VM with GNOME
conduit create myvm

# Open it
conduit view myvm
```

That's it. The VM gets your monitor's resolution and refresh rate
automatically.

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
make install   # installs to /opt/conduit
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
| `Ctrl+Alt+R` | Window size changes the VM's resolution / just scales it |

### Using virt-manager or virsh instead

Already have VMs in virt-manager? Add Conduit to one of them:

```bash
conduit attach myvm      # adds the GPU device to an existing libvirt VM
conduit view myvm
```

Inside that VM, install the guest driver: `sudo apt install ./conduit-guest_*.deb`.

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

## Credits

Conduit builds on [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu)
and [nesbox](https://github.com/nestrilabs/nesbox) (Nestri Labs), the display
broker from [nvkvm-pv](https://github.com/reindertpelsma/nvkvm-pv), and ideas
from gVisor's nvproxy and [kayfabe](https://github.com/reindertpelsma/kayfabe).
Licenses: see [LICENSE](LICENSE) and the `NOTICE` files in each component.

NVIDIA, GeForce and RTX are trademarks of NVIDIA Corporation. Conduit is not
affiliated with NVIDIA.
