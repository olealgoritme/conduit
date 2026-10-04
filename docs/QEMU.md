# Running the Conduit GPU device under QEMU

Conduit has two VM runners. QEMU 11.1 (patched, see `host/qemu`) is the
default: `conduit up NAME` / `conduit view NAME` start it with everything on
this page, plus a virtio-sound card and a QMP socket for a clean ACPI
shutdown. The built-in runner `conduit-vmm` (which has the device
built in) is the fallback when the bundled QEMU is missing, or with
`--vmm builtin`; it has no sound. This page is the QEMU command line `conduit`
uses (`$XDG_RUNTIME_DIR/conduit/NAME/qemu.args` holds the exact one of a
running VM), for running it by hand or from libvirt. The guest kernel, driver, userspace and disk are the same for
both.

## What you need

| piece | where |
| --- | --- |
| QEMU 11.1.x with the Conduit patches | `host/qemu/build-qemu.sh` (see `host/qemu/README.md`). It builds `host/qemu/build/qemu-system-x86_64`, and `--install` puts it in `/opt/conduit/bin`. |
| backend (`conduit-backend`) | `cd host/backend && cargo build --release -p device --features vhost-user --bin conduit-backend` |
| virtiofsd (NVIDIA userspace share) | `/usr/libexec/virtiofsd` (Ubuntu package `virtiofsd`) |
| guest kernel | the VM's own stock kernel: `conduit` copies the newest `/boot/vmlinuz-*` and its `initrd.img-*` out of the disk (`debugfs`) and passes them as `-kernel`/`-initrd`. The guest driver comes from DKMS (`conduit-guest`). A custom ELF `vmlinux` (`CONFIG_PVH=y`) also boots, without `-initrd`; that is what the commands below show. |
| guest disk | a `conduit create` disk (`~/.local/share/conduit/vms/NAME/disk.img`): a bare ext4 filesystem with no partition table or bootloader, mounted as `/dev/vda` |

**Stock QEMU 11.1 does not work.** It aborts on the guest's first device
config read, because it caps vhost-user config at 256 bytes and nvgpu's is
4036. Patch `0001` is required. Patches `0002` to `0006` fix correctness for
the window, CUDA in the aperture, MSI-X, mapping order (without 0005
every Vulkan submit fails: Xid 13/32) and the shared-memory BAR size (without
0006 QEMU aborts at startup). `host/qemu/README.md` explains each one.

Don't run this VM while another runner has the same disk open. Two
writers on one ext4 image corrupt it.

## Command line

Start the processes in this order: the backend (it listens), then
virtiofsd (it listens), then QEMU (it connects to both).

```sh
QEMU=/opt/conduit/bin/qemu-system-x86_64          # or host/qemu/build/qemu-system-x86_64
BACKEND=/opt/conduit/bin/conduit-backend          # or host/backend/target/release/conduit-backend
DISK=~/.local/share/conduit/vms/NAME/disk.img
KERNEL=/path/to/vmlinux                            # ELF, CONFIG_PVH=y
RUN=${XDG_RUNTIME_DIR:-/tmp}/conduit; mkdir -p "$RUN"
SHARE=$RUN/share; /opt/conduit/bin/conduit-userspace --stage "$SHARE"   # NVIDIA userspace

# 1. GPU backend. Add --display WxH@HZ --display-socket PATH for the scanout.
RUST_LOG=info "$BACKEND" --socket "$RUN/nvgpu.sock" \
    --caps graphics,video,utility,compute > "$RUN/backend.log" 2>&1 &

# 2. The share the guest mounts as tag "nvidia" (mounted by the guest image).
#    Ubuntu 24.04 restricts unprivileged user namespaces, so run unsandboxed
#    as yourself.
/usr/libexec/virtiofsd --socket-path="$RUN/vfs.sock" \
    --shared-dir="$SHARE" --sandbox=none > "$RUN/virtiofsd.log" 2>&1 &

# 3. The VM.
"$QEMU" \
  -machine q35,accel=kvm,memory-backend=mem \
  -cpu host,host-phys-bits=on -smp 4 -m 4G \
  -object memory-backend-memfd,id=mem,size=4G,share=on \
  -nodefaults -display none -serial mon:stdio \
  -kernel "$KERNEL" \
  -append "console=ttyS0 root=/dev/vda rw" \
  -drive file="$DISK",format=raw,if=virtio,cache=none \
  -netdev tap,id=net0,ifname=conduit0,script=no,downscript=no \
  -device virtio-net-pci,netdev=net0,mac=02:00:00:00:00:01 \
  -chardev socket,id=vfs,path="$RUN/vfs.sock" \
  -device vhost-user-fs-pci,chardev=vfs,tag=nvidia \
  -chardev socket,id=nvgpu,path="$RUN/nvgpu.sock" \
  -device vhost-user-test-device-pci,chardev=nvgpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036
```

Why each nvgpu-related argument is there:

- `memory-backend-memfd,...,share=on` is required. The backend reads the
  guest's requests out of guest RAM, so RAM has to be a shareable fd. The
  `size` must equal `-m`.
- `vhost-user-test-device-pci` is the generic vhost-user device. QEMU 11.1
  has no `vhost-user-device-pci`.
  - `virtio-id=45` is what the guest driver binds.
  - `num_vqs=2` gives the control queue and the event queue. The driver
    fails probe with fewer.
  - `vq_size=256` is the backend's `QUEUE_SIZE`. The default of 64 also
    works, but it is smaller.
  - `config_size=4036` is `sizeof(struct conduit_gpu_config)`, display
    fields included. A smaller value hides the display fields.
    `config_size=0` turns config reads off, and the guest driver then
    rejects the device.
  - With patch 0004, `vectors=` defaults to `num_vqs + 1` = 3, which is
    the same as `conduit-vmm`.
- The shared memory regions are not given on the command line. QEMU asks
  the backend for them with `GET_SHMEM_CONFIG`. The backend answers shmid 1
  (window, 1 GiB) and shmid 2 (UVM aperture, 32 GiB).
- `-cpu host,host-phys-bits=on` matters because the shared-memory BAR is
  64 GiB and 64-bit (128 GiB with a large `--venus-hostmem-mib`). The
  firmware places it above 4 GiB, which needs real physical-address width.
  For OVMF see [VENUS.md](VENUS.md) "Windows/OVMF guests".

Networking is the same as with `conduit-vmm`. `conduit up` creates the VM's
`conduitN` tap (N is the VM's network number; `conduit0` in the example),
owned by you, with the host at 172.30.N.1. The guest configures itself
statically to 172.30.N.2 on any `e*` interface, which includes QEMU's
virtio-net `enp0s*`. For a VM without the tap, use
`-netdev user,id=net0,hostfwd=tcp::2222-:22` and set the guest address
another way.

The console is `ttyS0`. `conduit-vmm` uses `hvc0`; systemd starts a getty on
whichever console the kernel command line names.

### Booting through OVMF instead

The disk is a bare filesystem with no ESP, so the firmware finds nothing to
boot on it. UEFI is still possible with a kernel given to OVMF. OVMF's
`-kernel` loader needs a PE/EFI-stub `bzImage` (a distro `vmlinuz` is one),
not the ELF `vmlinux`:

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$RUN/OVMF_VARS.fd"
# Replace "-kernel .../vmlinux" in the command above with:
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file="$RUN/OVMF_VARS.fd" \
  -kernel /path/to/bzImage \
```

OVMF sizes its 64-bit MMIO window from the CPU's physical-address bits, so
keep `host-phys-bits=on`. A disk that boots on its own needs a GPT image
with an ESP and a bootloader. In that case drop `-kernel`/`-append` and keep
the two pflash drives.

## libvirt

Conduit VMs are libvirt domains (`conduit libvirt enable`, done by `create` /
`import`), and `conduit attach` adds this device to an existing libvirt VM.
[LIBVIRT.md](LIBVIRT.md) has the domain XML Conduit writes, how the backend
and virtiofsd start with the domain (systemd socket activation: QEMU connects
to `conduit-backend@NAME.socket` and systemd starts the backend), and what
virt-manager can and cannot do with these VMs. The GPU still goes through
`<qemu:commandline>`, since libvirt has no element for a generic vhost-user
device:

```xml
  <qemu:commandline>
    <qemu:arg value='-chardev'/>
    <qemu:arg value='socket,id=conduit-gpu,path=/run/user/1000/conduit/NAME/gpu-libvirt.sock'/>
    <qemu:arg value='-device'/>
    <qemu:arg value='vhost-user-test-device-pci,chardev=conduit-gpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036,bus=pcie.0,addr=0x10'/>
  </qemu:commandline>
```

## What the guest sees: QEMU compared with conduit-vmm

The guest driver finds everything through virtio PCI capabilities. It calls
`virtio_get_shm_region()` for shmid 1 and 2, and reads config through the
device-config capability. It never uses a BAR number, so the different
layout below needs no driver change.

| | conduit-vmm | QEMU 11.1 (+ patches) |
| --- | --- | --- |
| PCI id / class | 1af4:106d rev 1, class 0x0380 (display) | 1af4:106d rev 1, class 0x0780 (communication). The driver binds by virtio id and builds its own PCI device for NVIDIA userspace, so the class does not matter. |
| virtio config structures | all in BAR 0 (32-bit, 16 KiB): common, isr, notify, MSI-X, device cfg at 0x1000 | BAR 2 (64-bit): common 0x0, isr 0x1000, device cfg 0x2000 (4 KiB window), notify 0x3000. MSI-X in BAR 1 |
| window (shmid 1) | BAR 2, 1 GiB | BAR 4 at offset 0, 1 GiB |
| aperture (shmid 2) | BAR 4, 32 GiB | BAR 4 at offset 1 GiB, 32 GiB (BAR 4 is 64 GiB, rounded up to a power of two by patch 0006) |
| MSI-X vectors | 3 | 3 with patch 0004, 1 stock (the guest then falls back to INTx) |
| unplaced window range | backed by zero pages (memfd), so writes stick | a hole: KVM exits to QEMU, reads return 0 and writes are dropped |
| window withdraw | the range is overwritten with zero pages | the range is unmapped and becomes a hole again |
| UVM pool | mapped at the pool's address (`MAP_FIXED_NOREPLACE`) | the same, with patch 0003. Stock QEMU's mmap is refused by nvidia-uvm, so CUDA semaphore pools fail |
| migration | none | blocked: QEMU refuses to migrate a device with shmem regions |

## Known issues

- **Mapping order (fixed by patch 0005).** The backend negotiates
  `REPLY_ACK` (the vhost crate always offers it), so `SHMEM_MAP` waits for
  QEMU's answer. Stock 11.1 answers before it commits the memory region, so
  the guest could touch a mapping before KVM had it, and those writes were
  dropped. Patch 0005 commits first.
- **A failed map stays invisible.** For the same reason, a mapping QEMU
  rejects (for example a UVM pool on stock QEMU) appears only in QEMU's
  stderr. The guest gets success and finds a hole.
- **Config reads are slow.** QEMU fetches all 4036 bytes over the socket
  on every guest config access. This costs time at probe and on each
  display-mode read, and nothing breaks.
- **Patch 0003 is outside the spec.** It is a Conduit extension. Until it
  is upstreamed, compute through UVM depends on the patched QEMU.
- **The built-in runner cannot boot a stock distro kernel** (it loads only an
  uncompressed ELF `vmlinux`, with no initrd), so VMs on their own kernel
  need QEMU.

Tested with an RTX 5090 (driver 610.57.04, Ubuntu 24.04 host, QEMU 11.1.2),
booting both a custom `vmlinux` and Ubuntu's stock kernel with the module from
DKMS.
