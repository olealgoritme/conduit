# Running the virtio-nvgpu device under QEMU

virtio-nvgpu has two hosts. One is nesbox, which has the device built in. The
other is QEMU 11.1 or later, through the generic vhost-user device. This page
covers QEMU. The guest kernel, driver, userspace and disk are the same for
both.

## What you need

| piece | where |
| --- | --- |
| QEMU 11.1.x with the Conduit patches | `host/qemu/build-qemu.sh` (see `host/qemu/README.md`). It builds `host/qemu/build/qemu-system-x86_64`, and `--install` puts it in `/opt/conduit/bin`. |
| backend with GET_SHMEM_CONFIG support | virtio-nvgpu branch `qemu11`, `cargo build --release -p device --features vhost-user --bin vhost-user-nvgpu` |
| virtiofsd (NVIDIA userspace share) | `/usr/libexec/virtiofsd` (Ubuntu package `virtiofsd`) |
| guest kernel | `~/code/nvgpu-lab/linux-7.2.9/vmlinux` (ELF with `CONFIG_PVH=y`, which QEMU `-kernel` boots directly) |
| guest disk | `~/code/nvgpu-lab/rootfs-ssh.ext4` (a bare ext4 filesystem with no partition table or bootloader. It mounts as `/dev/vda`) |

**Stock QEMU 11.1 does not work.** It aborts on the guest's first device
config read, because it caps vhost-user config at 256 bytes and nvgpu's is
4036. Patch `0001` is required. Patches `0002` to `0004` fix correctness for
the window, CUDA in the aperture, and MSI-X. `host/qemu/README.md` explains
each one.

Don't run this VM while nesbox has the same `rootfs-ssh.ext4` open. Two
writers on one ext4 image corrupt it.

## Command line

Start the processes in this order: the backend (it listens), then
virtiofsd (it listens), then QEMU (it connects to both).

```sh
LAB=$HOME/code/nvgpu-lab
QEMU=$HOME/code/conduit/host/qemu/build/qemu-system-x86_64   # or /opt/conduit/bin/...
BACKEND=$HOME/code/virtio-nvgpu-qemu/target/release/vhost-user-nvgpu
RUN=${XDG_RUNTIME_DIR:-/tmp}/conduit; mkdir -p "$RUN"

# 1. GPU backend. Add --display WxH@HZ --display-socket PATH for the scanout.
RUST_LOG=info "$BACKEND" --socket "$RUN/nvgpu.sock" \
    --caps graphics,video,utility,compute > "$RUN/backend.log" 2>&1 &

# 2. The share the guest mounts as tag "nvidia" (nvgpu.service in the image).
#    Ubuntu 24.04 restricts unprivileged user namespaces, so run unsandboxed
#    as yourself.
/usr/libexec/virtiofsd --socket-path="$RUN/vfs.sock" \
    --shared-dir="$LAB/share610" --sandbox=none > "$RUN/virtiofsd.log" 2>&1 &

# 3. The VM.
"$QEMU" \
  -machine q35,accel=kvm,memory-backend=mem \
  -cpu host,host-phys-bits=on -smp 4 -m 4G \
  -object memory-backend-memfd,id=mem,size=4G,share=on \
  -nodefaults -display none -serial mon:stdio \
  -kernel "$LAB/linux-7.2.9/vmlinux" \
  -append "console=ttyS0 root=/dev/vda rw" \
  -drive file="$LAB/rootfs-ssh.ext4",format=raw,if=virtio,cache=none \
  -netdev tap,id=net0,ifname=nesbox0,script=no,downscript=no \
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
  - `config_size=4036` is `sizeof(struct virtio_gpu_nv_config)`, display
    fields included. A smaller value hides the display fields.
    `config_size=0` turns config reads off, and the guest driver then
    rejects the device.
  - With patch 0004, `vectors=` defaults to `num_vqs + 1` = 3, which is
    the same as nesbox.
- The shared memory regions are not given on the command line. QEMU asks
  the backend for them with `GET_SHMEM_CONFIG`. The backend answers shmid 1
  (window, 1 GiB) and shmid 2 (UVM aperture, 1 GiB).
- `-cpu host,host-phys-bits=on` matters because the shared-memory BAR is
  2 GiB and 64-bit. The firmware places it above 4 GiB, which needs real
  physical-address width.

Networking is the same as with nesbox. `nvgpu-vm up` creates the `nesbox0`
tap, owned by you, with the host at 172.30.0.1. The guest configures itself
statically to 172.30.0.2 on any `e*` interface, which includes QEMU's
virtio-net `enp0s*`. For a VM without the tap, use
`-netdev user,id=net0,hostfwd=tcp::2222-:22` and set the guest address
another way.

The console is `ttyS0`. nesbox uses `hvc0`; systemd starts a getty on
whichever console the kernel command line names.

### Booting through OVMF instead

This disk can't boot from its own kernel. `rootfs-ssh.ext4` is a bare
filesystem with an empty `/boot` and no ESP, so the firmware finds nothing
to boot on it. UEFI is still possible with a kernel given to OVMF. OVMF's
`-kernel` loader needs a PE/EFI-stub `bzImage`, not the ELF `vmlinux`, so
run `make bzImage` in `linux-7.2.9` first:

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd "$RUN/OVMF_VARS.fd"
# Replace "-kernel .../vmlinux" in the command above with:
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file="$RUN/OVMF_VARS.fd" \
  -kernel "$LAB/linux-7.2.9/arch/x86/boot/bzImage" \
```

OVMF sizes its 64-bit MMIO window from the CPU's physical-address bits, so
keep `host-phys-bits=on`. A disk that boots on its own needs a GPT image
with an ESP and a bootloader. In that case drop `-kernel`/`-append` and keep
the two pflash drives.

## libvirt

libvirt has no element for a generic vhost-user device, so the nvgpu device
goes through `qemu:commandline`. Everything else is native.

Use the session daemon (`virsh -c qemu:///session`). It runs QEMU as you, so
QEMU can reach the backend socket, the tap you own, and the files in your
home directory. The system daemon runs QEMU as `libvirt-qemu` under an
AppArmor profile that knows nothing about the socket; to use it you would
have to fix ownership and AppArmor yourself. Ubuntu 24.04 ships libvirt
10.0. Point `<emulator>` at the patched QEMU.

```xml
<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <name>conduit-nvgpu</name>
  <memory unit='GiB'>4</memory>
  <vcpu>4</vcpu>
  <memoryBacking>
    <source type='memfd'/>
    <access mode='shared'/>
  </memoryBacking>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <kernel>/home/user/code/nvgpu-lab/linux-7.2.9/vmlinux</kernel>
    <cmdline>console=ttyS0 root=/dev/vda rw</cmdline>
  </os>
  <features><acpi/></features>
  <cpu mode='host-passthrough'>
    <maxphysaddr mode='passthrough'/>
  </cpu>
  <devices>
    <emulator>/opt/conduit/bin/qemu-system-x86_64</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='raw' cache='none'/>
      <source file='/home/user/code/nvgpu-lab/rootfs-ssh.ext4'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <interface type='ethernet'>
      <mac address='02:00:00:00:00:01'/>
      <target dev='nesbox0' managed='no'/>
      <model type='virtio'/>
    </interface>
    <!-- virtiofsd started by you, as above -->
    <filesystem type='mount'>
      <driver type='virtiofs' queue='1024'/>
      <source socket='/run/user/1000/conduit/vfs.sock'/>
      <target dir='nvidia'/>
    </filesystem>
    <serial type='pty'/>
    <console type='pty'><target type='serial'/></console>
  </devices>
  <qemu:commandline>
    <qemu:arg value='-chardev'/>
    <qemu:arg value='socket,id=nvgpu,path=/run/user/1000/conduit/nvgpu.sock'/>
    <qemu:arg value='-device'/>
    <qemu:arg value='vhost-user-test-device-pci,chardev=nvgpu,virtio-id=45,num_vqs=2,vq_size=256,config_size=4036,bus=pcie.0,addr=0x10'/>
  </qemu:commandline>
</domain>
```

The pinned `addr=0x10` keeps the device out of the slots libvirt assigns
itself. Start the backend and virtiofsd before `virsh start`. libvirt
neither starts nor supervises them.

## What the guest sees: QEMU compared with nesbox

The guest driver finds everything through virtio PCI capabilities. It calls
`virtio_get_shm_region()` for shmid 1 and 2, and reads config through the
device-config capability. It never uses a BAR number, so the different
layout below needs no driver change.

| | nesbox | QEMU 11.1 (+ patches) |
| --- | --- | --- |
| PCI id / class | 1af4:106d rev 1, class 0x0380 (display) | 1af4:106d rev 1, class 0x0780 (communication). The driver binds by virtio id and builds its own PCI device for NVIDIA userspace, so the class does not matter. |
| virtio config structures | all in BAR 0 (32-bit, 16 KiB): common, isr, notify, MSI-X, device cfg at 0x1000 | BAR 2 (64-bit): common 0x0, isr 0x1000, device cfg 0x2000 (4 KiB window), notify 0x3000. MSI-X in BAR 1 |
| window (shmid 1) | BAR 2, 1 GiB | BAR 4 at offset 0, 1 GiB |
| aperture (shmid 2) | BAR 4, 1 GiB | BAR 4 at offset 1 GiB, 1 GiB (BAR 4 is 2 GiB) |
| MSI-X vectors | 3 | 3 with patch 0004, 1 stock (the guest then falls back to INTx) |
| unplaced window range | backed by zero pages (memfd), so writes stick | a hole: KVM exits to QEMU, reads return 0 and writes are dropped |
| window withdraw | the range is overwritten with zero pages | the range is unmapped and becomes a hole again |
| UVM pool | mapped at the pool's address (`MAP_FIXED_NOREPLACE`) | the same, with patch 0003. Stock QEMU's mmap is refused by nvidia-uvm, so CUDA semaphore pools fail |
| migration | none | blocked: QEMU refuses to migrate a device with shmem regions |

## Open risks

- **Mapping is asynchronous.** The backend doesn't negotiate `REPLY_ACK`,
  so `SHMEM_MAP` is fire-and-forget. The guest's mmap reply can arrive
  before QEMU's main loop has placed the memory, and an access in that gap
  reads the hole. nesbox has the same gap, but QEMU handles backend
  requests on its main loop under the BQL, so the gap may be wider.
  Turning on `REPLY_ACK` naively risks a deadlock. The vring thread waits
  for QEMU while holding the backend's write lock, and QEMU's vCPU thread
  can at the same moment be waiting on a `GET_CONFIG`, whose reply needs
  the read lock.
- **A failed map stays invisible.** For the same reason, a mapping QEMU
  rejects (for example a UVM pool on stock QEMU) appears only in QEMU's
  stderr. The guest gets success and finds a hole.
- **Config reads are slow.** QEMU fetches all 4036 bytes over the socket
  on every guest config access. This costs time at probe and on each
  display-mode read, and nothing breaks.
- **Patch 0003 is outside the spec.** It is a Conduit extension. Until it
  is upstreamed, compute through UVM depends on the patched QEMU.
- **Nothing here has run yet.** QEMU and the backend are built and the unit
  tests pass. No VM was started for this page, because the GPU host was in
  use.
