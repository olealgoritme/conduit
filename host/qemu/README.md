# host/qemu: a QEMU build for the virtio-nvgpu device

`build-qemu.sh` fetches QEMU, checks the tarball, applies the patches in
`patches/`, and builds a small `qemu-system-x86_64`: x86_64-softmmu only,
KVM (no TCG), vhost-user, virtio, slirp, VNC, PipeWire/PulseAudio audio. GTK, SDL, SPICE and OpenGL are
off, because the guest display is the zero-copy nvgpu scanout shown by the
Conduit viewer.

```sh
host/qemu/build-qemu.sh                      # build into host/qemu/build
host/qemu/build-qemu.sh --install            # then install to /opt/conduit
host/qemu/build-qemu.sh --prefix ~/.local --install
host/qemu/build-qemu.sh --stock              # no patches (cannot host nvgpu)
```

Options: `--version X.Y.Z` (default 11.1.2; anything older than 11.1 is
refused), `--prefix DIR` (default `/opt/conduit`), `--no-slirp`, `--no-audio`, `--jobs N`.
The compile runs under `nice -n 10`. Nothing is installed without
`--install`.

The tarball is checked twice: its SHA-256 against the pin in the script, and
its signature against the QEMU release key
`CEACC9E15534EBABB82D3FA03353C9CEF108B584` (Michael Roth). The key is
fetched into a throwaway keyring, so your own keyring is not touched.

## Why QEMU 11.1

11.1 is the first release with vhost-user **VIRTIO Shared Memory Regions**:
protocol feature `VHOST_USER_PROTOCOL_F_SHMEM` (bit 22),
`VHOST_USER_GET_SHMEM_CONFIG` (front-end request 44), and the back-end
requests `VHOST_USER_BACKEND_SHMEM_MAP` and `_UNMAP` (9 and 10). It also adds
shared-memory support to the generic vhost-user device. The backend uses
these for its two regions: the window (shmid 1) and the UVM aperture
(shmid 2). In 11.1 the generic device is called **`vhost-user-test-device-pci`**.
There is no `vhost-user-device-pci`.

## Patches

Stock 11.1 can't host virtio-nvgpu. Patch 0001 is required. The others fix
correctness and performance.

| patch | why |
| --- | --- |
| 0001 vhost-user: max config size 4096 | QEMU asserts that vhost-user device config is at most 256 bytes (`VHOST_USER_MAX_CONFIG_SIZE`). nvgpu's config is 4036 bytes, so stock QEMU aborts on the guest's first config read. 4096 matches the virtio-pci config window and the rust-vmm limit. |
| 0002 vhost: keep shmem mappings out of the mem table | QEMU sends every fd-backed RAM region back to the backend as guest memory, including the mappings the backend placed with SHMEM_MAP. The backend would then mmap its own `/dev/nvidia*` fds a second time. That can fail, and if it does, `vhost-user-backend` exits. It also uses up the 8 memory-table slots. |
| 0003 vhost-user: fixed-VA shmem mappings | nvidia-uvm only accepts a mapping whose address equals its file offset. QEMU picks the mapping address itself, so CUDA semaphore pools could never be mapped into the aperture. This patch adds a non-spec `flags` bit 1 meaning "map at `fd_offset`" (with `MAP_FIXED_NOREPLACE`, and `MADV_POPULATE_WRITE` on the first page only, since a managed allocation can be gigabytes UVM places lazily), which is what nesbox does. The backend sets the bit only for a frontend that sent `GET_SHMEM_CONFIG`. |
| 0004 vhost-user-test-device-pci: vectors | The stock device hard-codes 1 MSI-X vector. A guest driver with 2 queues then falls back to INTx. With this patch the default is `num_vqs + 1`, and a `vectors=` property is added. |
| 0005 vhost-user: commit a shmem mapping before replying | QEMU acks `SHMEM_MAP` before it commits the memory transaction, so the KVM memory slot appears only after the backend has told the guest the mapping exists. Guest writes in that gap land in a hole and are dropped: on virtio-nvgpu that showed up as corrupted push buffers and shader headers (Xid 32 / Xid 13, every Vulkan submit failed its fence). Committing first is safe because patch 0002 keeps these mappings out of the vhost memory table, so the commit sends nothing to the backend that is waiting for the reply. |
| 0006 vhost-user-test-device-pci: power-of-two shmem BAR | The generic device sizes its shared-memory BAR as the sum of the regions, and `pci_register_bar` asserts a power of two. The window (1 GiB) plus the UVM aperture (32 GiB) is 33 GiB, so stock QEMU aborts at startup. The BAR is rounded up; the tail stays unbacked address space. |

## Build dependencies (Ubuntu 24.04)

```sh
sudo apt install build-essential ninja-build python3-venv pkg-config \
  libglib2.0-dev libpixman-1-dev zlib1g-dev flex bison \
  libslirp-dev libfdt-dev curl gnupg xz-utils patch \
  libpipewire-0.3-dev libpulse-dev
```

Audio: the build has the PipeWire and PulseAudio backends (`-audiodev
pipewire` / `pa`), so the VM gets a virtio-sound card with speakers and a
microphone. `--no-audio` builds without them (and drops the two -dev packages).

QEMU ships its own meson. `libslirp-dev` is only needed when slirp is
enabled, i.e. without `--no-slirp`.

Verified here on Ubuntu 24.04 (gcc 13, glibc 2.39, Python 3.12) with 11.1.2:
configure, a full build in about a minute on 32 threads, and
`vhost-user-test-device-pci` present in `-device help`.

To run a VM with it, see `docs/QEMU.md`.
