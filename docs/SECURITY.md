# Security

Conduit narrows what a VM can do with the host's GPU. It is **not** hardware
isolation: run only VMs you trust.

## The model

A guest never touches the GPU. Its NVIDIA driver calls go through the guest
module to `conduit-backend` on the host, which checks them and makes the real
calls on the host's NVIDIA driver. The GPU's own MMU separates the VM's GPU
work from everything else; there is no IOMMU boundary. The host NVIDIA driver
and the backend are therefore trusted.

## What contains a VM

- **Ordinary VM boundary.** The guest kernel reaches the host only through the
  virtio devices of its VM runner (QEMU, or `conduit-vmm` with its own seccomp
  filter). Everything runs as your user, in libvirt's user session.
- **One unprivileged, sandboxed backend per VM.** It refuses to run as root or
  with `CAP_SYS_ADMIN`, drops its capabilities, and locks itself down with
  seccomp and Landlock before the first guest message: no `exec`, no files
  beyond the NVIDIA device nodes it needs, no sockets but AF_UNIX. It refuses
  to start on a kernel without seccomp or Landlock.
- **Memory limits.** The runner, the backend and virtiofsd of a VM share one
  systemd user slice (`conduit-NAME.slice`) with `MemoryMax` = guest RAM +
  overhead and `memory.oom.group`, and a raised `oom_score_adj`, so a VM is
  killed as a whole before your desktop under memory pressure.

## What the backend refuses

None of this can be switched off from the command line.

- **Unknown or mis-sized calls.** Every ioctl and UVM command is checked
  against the ABI table generated for the host's driver release
  (`host/backend/gen`). A call the release does not define, or one of the wrong
  size, is refused; a driver older than every table refuses to start.
- **Privileged RM calls.** RM judges privilege by its caller, which is the
  backend, so the backend applies RM's own rules itself: the RM allowlist is
  generated from RM's privilege tables, never written by hand.
- **Host snooping.** A handful of controls RM allows anyone are refused anyway:
  the host's GPU process list, GPU accounting, and sub-process identity /
  USERD isolation switches.
- **Guest pointers.** No guest address reaches RM. The backend supplies every
  pointer-carrying buffer itself, with size caps per pointer and per call.
- **Host display access (NVKMS filter).** On `/dev/nvidia-modeset` only what
  buffer sharing needs reaches the host display driver: device alloc/free and
  the five surface commands (register, unregister, grant, acquire, release).
  Everything that would act on the host's displays (modesets, flips, LUTs,
  vblank control) is refused; vblank semaphore setup is answered locally. The guest cannot change your
  monitors or touch other apps' output.
- **Dangerous UVM calls.** `/dev/nvidia-uvm-tools` is never served. UVM
  commands that carry raw user pointers (`TOOLS_READ/WRITE_PROCESS_MEMORY`,
  `TOOLS_GET_PROCESSOR_UUID_TABLE`) are refused, as is a VA space with
  pageable (HMM) access. UVM mappings in the aperture are checked for size,
  alignment, address and overlap by the backend and again by the VM runner.
- **Anything outside `--caps`.** No CUDA (`nvidia-uvm`) without `compute`, no
  encoders without `video`, no 3D without `graphics`.
- **VRAM past `--vram-limit-mib`.** Allocations over the limit fail, and the
  guest sees the limit as the card's memory size.

The backend counts refusals and logs the totals when the VM exits
(`conduit logs NAME`).

## What it is not

- **Not hardware isolation** like passthrough, SR-IOV or vGPU. A bug in the
  host NVIDIA driver that an allowed call can reach is reachable from the
  guest. Keep the host driver up to date.
- **DMA buffers are not validated** (gVisor's nvproxy does not either).
- **The VRAM limit is approximate.** Memory RM allocates internally (tens of
  MiB per guest) is not counted.
- **Processes inside one VM are not isolated from each other** by Conduit: one
  backend serves the whole VM.
- **The viewer and stream host** accept connections only from your user (the
  viewer checks `SO_PEERCRED`); `conduit stream` exposes the VM to paired
  Moonlight clients on the network (see `docs/STREAMING.md`).

## Reporting

Please report vulnerabilities privately through a GitHub security advisory on
this repository, not in a public issue.
