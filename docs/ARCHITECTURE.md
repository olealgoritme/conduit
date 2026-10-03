# Architecture

How a Linux VM uses the host's NVIDIA GPU while the host keeps using it.

## Overview

```text
 Guest                                     Host
 ─────────────────────────────────         ─────────────────────────────────────
 app (Vulkan / GL / CUDA / NVENC)
 NVIDIA user-mode driver, unmodified        (the host's own files, shared
   │ ioctl / mmap on /dev/nvidia*            read-only over virtiofs)
   ▼
 conduit_gpu.ko  (guest/linux)
   │ virtio: control + event queue ─────►  conduit-backend (host/backend)
   │ shared memory: window, UVM aperture      one per VM, sandboxed
   │                                          │ checks, translates, calls
 guest compositor → KMS flip ────────────►    ▼
                                            host NVIDIA driver → GPU
                                              │ scanout buffer as dma-buf
                                              ▼
                                            conduit-viewer  (Wayland window)
                                         or conduit-stream  (Moonlight / network)
```

The VM runner between guest and backend is the bundled QEMU 11.1 (default,
`host/qemu`) or the built-in `conduit-vmm` (fallback, `host/vmm`). Both host
the GPU device as a vhost-user frontend; the backend is the vhost-user
backend. The `conduit` command (`cli/`) creates VMs, starts all processes and
registers each VM with libvirt's user session.

## Guest module

The guest runs NVIDIA's real user-mode driver: Vulkan, OpenGL/EGL, CUDA,
NVENC/NVDEC. Its userspace files are the host's own, staged by
`conduit-userspace` and mounted read-only over virtiofs, because the forwarded
ioctls are a private contract between one driver build's userspace and kernel
module.

`conduit_gpu.ko` creates `/dev/nvidiactl`, `/dev/nvidia0..N`,
`/dev/nvidia-uvm`, `/dev/nvidia-modeset` and a DRM device. It forwards each
`ioctl()` and `mmap()` without interpreting RM calls, swaps guest file
descriptors for backend handles, pins memory a process registers by address,
and maps what the backend placed. It also provides a KMS display, input
devices (keyboard, mouse, gamepads) and `/dev/conduit-clipboard`. It is packaged with DKMS
(`conduit-guest`).

## Backend

`conduit-backend` holds the real `/dev/nvidia*` descriptors for one VM. For
each request it:

- checks it against the ABI table of the host's driver release
  (`host/backend/gen`, generated from NVIDIA's open kernel modules and gVisor's
  nvproxy) and the RM allowlist;
- replaces what cannot cross unchanged: pointers (the backend supplies the
  buffers), handles (translated to the owning file) and file descriptors;
- makes the real call and returns the result.

Only setup crosses the queue. Work is submitted through memory both sides
map: when the guest maps GPU memory, the backend maps it on the host and
places it in the device's **window** (shared-memory region 1), which the guest
maps into the process. CUDA semaphore pools and managed memory need a mapping
at a fixed address, so they go into the **UVM aperture** (region 2, 32 GiB of
address space, committed only as touched). Pinned host memory is registered as
guest-physical page runs and mapped zero-copy. When a host descriptor becomes
ready (GPU interrupt), the backend sends an event so the guest's `poll()` wakes.

See [SECURITY.md](SECURITY.md) for the sandbox and what is refused.

## Display

The guest compositor flips through the module's KMS device. The backend
exports the flipped buffer once as a dma-buf (`PRIME_HANDLE_TO_FD`) and sends
it over a Unix socket (the broker protocol,
`host/viewer/docs/broker-protocol.md`) to:

- **`conduit-viewer`**, which hands it to the Wayland compositor with
  `zwp_linux_dmabuf_v1`: no copy, and in fullscreen the compositor can scan it
  out directly. Input, cursor, clipboard and mode hints (window size, refresh
  rate) go back over the same socket.
- **`conduit-stream`**, which takes the viewer's place on that socket, encodes
  frames with NVENC and serves Moonlight (GameStream) clients or a remote
  Conduit viewer ([STREAMING.md](STREAMING.md)).

Details: [SCANOUT.md](SCANOUT.md), [CLIPBOARD.md](CLIPBOARD.md).

## Runners and libvirt

`conduit up` / `view` start, per VM, the backend, virtiofsd and the runner in
one systemd user slice, plus a tap device with NAT. QEMU additionally gets a
virtio-sound card (PipeWire/PulseAudio) and a QMP socket for clean ACPI
shutdown. For libvirt, the domain XML adds the GPU device through
`<qemu:commandline>` and the backend starts by socket activation
(`conduit-backend@NAME.socket`), so virt-manager and `virsh` control the VM
normally. Saving memory, snapshots with memory and migration are not
supported: the GPU state lives in the host driver. Details:
[QEMU.md](QEMU.md), [LIBVIRT.md](LIBVIRT.md).

## Limits

- NVIDIA only, Linux guests only (Windows: [ROADMAP.md](ROADMAP.md)).
- Not hardware isolation; the host NVIDIA driver is trusted.
- The shared-memory window (1 GiB) is sized when the VM starts and cannot grow.
- No HMM / pageable memory access, MIG or SR-IOV.
- `--vram-limit-mib` does not count memory RM allocates internally.

The full list: [KNOWN-ISSUES.md](KNOWN-ISSUES.md).
