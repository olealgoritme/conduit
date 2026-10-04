# guest/linux

Conduit's guest kernel module, `conduit_gpu.ko`. Licensed GPL-2.0
(`LICENSE`).

Inside the VM it creates NVIDIA's device nodes (`/dev/nvidiactl`,
`/dev/nvidia0..N`, `/dev/nvidia-modeset`, `/dev/nvidia-uvm`) and a DRM device,
so NVIDIA's own user-mode driver runs unchanged. Each `ioctl()` and `mmap()` is
forwarded over virtio to `conduit-backend` on the host; GPU memory is mapped
straight from the device's shared-memory regions, never copied.

It also provides:

- a KMS display (virtual CRTC, plane, connector) whose flips go to
  `conduit-viewer` / `conduit-stream` as dma-bufs (`nvgpu_kms.h`,
  `docs/SCANOUT.md`),
- keyboard and mouse input from the viewer, and gamepads from the stream
  host (`nvgpu_pad.h`),
- `/dev/conduit-clipboard` for `guest/agent/conduit-clipboard-agent`
  (`docs/CLIPBOARD.md`),
- explicit sync: DRM syncobjs and nvidia-drm's semaphore-surface fences,
  backed by the host's fences (`nvgpu_fence.h`, `docs/SYNC.md`;
  `explicit_sync=0` turns it off),
- a PCI mirror of the host GPU, which NVIDIA's userspace looks for: a root
  bus in its own PCI domain (the first one at or above `0x10` the guest does
  not use; bus/device/function are the host's), with the domain translated
  between host and guest wherever userspace sees an address
  (`nvgpu_pcimap.h`, tested on the build host by `test/pcimap_test.c`).
  The mirror device gets a `driver_override` so no other driver binds it.

The module does not interpret RM calls. It only does what needs the guest
kernel: swapping file descriptors for backend handles, pinning memory a
process registers by address, and mapping what the backend placed.

## Install (DKMS)

The `conduit-guest` package (`.deb` / `.rpm` / Arch `.pkg.tar.zst`) installs
the source to `/usr/src/conduit-guest-<version>/` and DKMS rebuilds it for
every installed kernel. `conduit create` and `conduit attach` install it for
you. To build the package: `make guest-deb`, `make guest-rpm` or
`make guest-arch` at the repo root. Linux 6.4 or newer is required.

The package also installs:

- `/usr/lib/modules-load.d/conduit-gpu.conf`, which loads the module at boot,
- `/usr/lib/modprobe.d/conduit-gpu.conf` and `/usr/lib/conduit-guest/setup`
  (from `guest/system/`), described below; the modprobe file also
  blacklists `spi_virtio`, because the virtio spec has since given device
  ID 45 to SPI controllers and `spi_virtio` (Arch 7.2) would race
  `conduit_gpu` for the device,
- `/etc/sysctl.d/60-conduit-userns.conf`, which lifts Ubuntu's AppArmor
  restriction on unprivileged user namespaces (Steam's pressure-vessel,
  Flatpak and browser sandboxes need them),
- `/usr/lib/systemd/logind.conf.d/50-conduit-powerkey.conf` (from
  `guest/power/`), so the VM's power button shuts it down even with a desktop
  session open,
- the clipboard agent (`guest/agent/README.md`),

and its post-install sets `en_US.UTF-8` as the default locale when the VM has
no UTF-8 locale other than `C.UTF-8` (Steam's 32-bit client crashes in libc
under `C.UTF-8`).

### Upgrading from virtio_gpu_nv

The module was called `virtio_gpu_nv` before it was renamed to `conduit_gpu`.
Installing the new `conduit-guest` package moves a VM over: the post-install
script removes DKMS registrations and `.ko` files of the old module, drops it
from `/etc/modules-load.d/`, points `conduit-guest.service` and the seat udev
rule at the new name, and rebuilds any initramfs that carries it. The
modprobe.d file blacklists the old name and redirects `modprobe
virtio_gpu_nv` to `conduit_gpu`, so the two never load together. Reboot the
VM afterwards; the old module holds the device until then.

## Build by hand

```sh
make -C guest/linux                                   # this machine's kernel
make -C guest/linux KDIR=/lib/modules/<ver>/build     # another kernel's headers
```

`KDIR` is the build tree of the kernel the module will be *loaded* into (the
guest's). Any distro kernel with headers installed works; CI builds against
Ubuntu 24.04 (GA and HWE), Debian 13 and Fedora with zero warnings, and runs
`make check` (the plain-C unit tests in `test/`).
`nvgpu_compat.h` covers API differences between kernel versions. In a kernel
tree, `CONFIG_CONDUIT_GPU` builds it in tree (`Kconfig`);
`guest-kernel.config` is a minimal config for a custom guest `vmlinux`.

`gen/` and `rmctrl/` are generated from NVIDIA's open kernel modules by
`host/backend/gen/` (see its README). Don't edit them by hand.
