# guest/linux

Conduit's guest kernel module, `virtio_gpu_nv.ko`. Licensed GPL-2.0
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
- keyboard, mouse and power-key input from the viewer,
- `/dev/conduit-clipboard` for `guest/agent/conduit-clipboard-agent`
  (`docs/CLIPBOARD.md`).

The module does not interpret RM calls. It only does what needs the guest
kernel: swapping file descriptors for backend handles, pinning memory a
process registers by address, and mapping what the backend placed.

## Install (DKMS)

The `conduit-guest` package (`.deb` / `.rpm`) installs the source to
`/usr/src/conduit-guest-<version>/` and DKMS rebuilds it for every installed
kernel. `conduit create` and `conduit attach` install it for you. To build the
package: `make guest-deb` or `make guest-rpm` at the repo root. Linux 6.4 or
newer is required.

## Build by hand

```sh
make -C guest/linux                                   # this machine's kernel
make -C guest/linux KDIR=/lib/modules/<ver>/build     # another kernel's headers
```

`KDIR` is the build tree of the kernel the module will be *loaded* into (the
guest's). Any distro kernel with headers installed works; CI builds against
Ubuntu 24.04 (GA and HWE), Debian 13 and Fedora with zero warnings.
`nvgpu_compat.h` covers API differences between kernel versions. In a kernel
tree, `CONFIG_VIRTIO_GPU_NV` builds it in tree (`Kconfig`);
`guest-kernel.config` is a minimal config for a custom guest `vmlinux`.

`gen/` and `rmctrl/` are generated from NVIDIA's open kernel modules by
`host/backend/gen/` (see its README). Don't edit them by hand.
