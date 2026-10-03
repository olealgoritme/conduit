# Conduit VMs in libvirt (virt-manager, virsh)

Every VM `conduit create` / `conduit import` makes is also a libvirt domain,
and `conduit attach` gives an existing libvirt VM Conduit's GPU. Either way
libvirt runs QEMU, and you start, pause, resume, reboot and stop the VM from
virt-manager or `virsh` like any other. Conduit's GPU backend, the NVIDIA
share and the `conduit view` window follow the domain on their own.

Source: `cli/src/virt.rs` (domains, `conduit libvirt`), `cli/src/libvirt.rs`
(`attach` / `detach`), `cli/src/units.rs` (systemd units), `cli/src/lvrun.rs`
(`up` / `view` / `down` / `status` for these VMs, and the unit helpers),
`cli/src/guest.rs` (guest-side install).

## Which libvirt

Conduit's own VMs live in the **user session**, `qemu:///session`: QEMU runs
as you, so it reaches `/dev/nvidia*` (through the backend), the sockets in
`/run/user/UID`, your PipeWire, and the VM files in your home folder, with no
permission or AppArmor changes. virt-manager shows it after
**File > Add Connection > Hypervisor: QEMU/KVM user session** (once; it is
remembered). On the command line: `virsh -c qemu:///session list --all`.

`conduit attach` also works on VMs of the system daemon (`qemu:///system`,
`-c qemu:///system`). Those need Conduit installed from a package (QEMU in
`/opt/conduit`, which the `libvirt-qemu` user can read), and get system units
(below) whose sockets belong to that user.

## What starts the backend

The session daemon runs no hooks, so nothing in libvirt starts Conduit's
helpers. systemd does, through socket activation:

| unit (user manager) | listens on | starts |
|---|---|---|
| `conduit-backend@NAME.socket` | `/run/user/UID/conduit/NAME/gpu-libvirt.sock` | `conduit-backend@NAME.service` → `conduit _backend NAME` → `conduit-backend` |
| `conduit-virtiofsd@NAME.socket` | `/run/user/UID/conduit/NAME/vfs-libvirt.sock` | `conduit-virtiofsd@NAME.service` → `conduit _virtiofsd NAME` → `virtiofsd --fd=3` |

The sockets listen from login on (`WantedBy=sockets.target`). When QEMU starts
(virt-manager "Run", `virsh start`, `conduit up`), it connects to both as a
vhost-user client; systemd starts the helper and hands it the listening socket
(`LISTEN_FDS`, fd 3). The helper serves that one connection and exits when
QEMU goes away, so the backend lives exactly as long as the VM. A reboot of
the guest keeps QEMU, the connection and the backend. After the backend,
`ExecStopPost` (`conduit _stopped NAME`) copies the newest kernel out of the
VM's disk, so a kernel updated inside it boots next time. The services run in
the VM's slice `conduit-NAME.slice` with `OOMScoreAdjust=500`.

`conduit _backend` picks the display mode when it starts: the one `conduit up
--display` / `conduit view` asked for (they leave it in
`/run/user/UID/conduit/NAME/libvirt-next-mode`), else your monitor's. It
records the mode in `libvirt-mode`, which `conduit view` reads to open a
matching window for a VM that is already running.

System domains get the same units in `/etc/systemd/system`, with
`SocketUser=libvirt-qemu` (or `qemu`), sockets in `/run/conduit/NAME/`, and
`User=` you for the helpers.

## The network

The session daemon cannot create taps. A Conduit VM uses the tap `conduitN`
(host `172.30.N.1`, VM `172.30.N.2`, NAT) as `<interface type='ethernet'>
<target dev='conduitN' managed='no'/>`. `conduit libvirt enable` installs one
root oneshot unit, `conduit-net-NAME.service`, enabled at boot, that creates
the tap (owned by you) and the NAT rules; this is the only step that asks for
sudo. Attached VMs keep whatever network they had.

## The domain

`conduit libvirt enable NAME` writes the whole definition from `vm.json`
(and rewrites it when run again):

- `<emulator>`: Conduit's QEMU 11.1 (stock QEMU cannot run the GPU device;
  see [QEMU.md](QEMU.md)). Ubuntu confines `libvirtd` with AppArmor, the
  session daemon too, and its profile only lets it start `/usr/bin` QEMU;
  Conduit adds one rule to `/etc/apparmor.d/local/usr.sbin.libvirtd` (the
  packages do this on install, `conduit` does it for a source build).
- `<memoryBacking>` memfd + shared: the backend reads guest RAM.
- `<cpu mode='host-passthrough'>` with `<maxphysaddr mode='passthrough'/>`:
  the GPU's shared-memory BAR is 64-bit and large.
- direct kernel boot: `vms/NAME/boot/{vmlinuz,initrd.img}`, copied out of
  the disk's `/boot` (refreshed after every run and before `conduit up`).
- the raw disk (virtio, `cache=none`, `discard=unmap`), the tap, a virtio RNG,
  a serial console (virt-manager shows it; logged to `logs/vm.log`), no
  emulated display (the screen is `conduit view`).
- `<filesystem>` virtiofs, tag `nvidia`, on the socket above: the NVIDIA
  user-space share. libvirt's own virtiofsd launching is not used, because
  Ubuntu 24.04's virtiofsd 1.10 has no `--readonly`; `conduit _virtiofsd`
  adds it when the virtiofsd has it.
- `<qemu:commandline>`: the GPU (`-chardev socket` + `-device
  vhost-user-test-device-pci,...,bus=pcie.0,addr=0x10`; libvirt has no element
  for a generic vhost-user device), and the sound card (`-audiodev pipewire` +
  `virtio-sound-pci`; libvirt 10.0 has no `<sound model='virtio'>`), with
  `<qemu:env>` pointing QEMU at your runtime folder for PipeWire.
- `<metadata>` `conduit:vm`: marks the domain as Conduit's.

`conduit attach` edits an existing definition instead: the same emulator,
memfd, CPU, share, metadata and GPU (on the highest free slot of bus 0), and
the machine type becomes the plain `q35` / `pc` alias (a versioned type such
as `pc-q35-noble` belongs to the old QEMU). It keeps everything else. The
definition from before the first attach is saved as
`vms/NAME/libvirt-backup-TIME.xml`; `conduit detach` defines it again (libvirt
may print elements in another order; the content is the same). Each step is
validated first, the domain is defined in one `virsh define --validate`, and
if libvirt refuses it nothing stays changed.

## Guest side (attach)

`conduit attach` builds `vms/NAME/guest-setup.tar` (the `conduit-guest`
package, the share mount unit, udev and loader files) and, if the VM runs and
answers on the QEMU guest agent, uploads it and runs its `setup.sh` as root
(apt: headers for the running kernel, the package, DKMS build). Otherwise it
prints one `ssh ... < guest-setup.tar` command; `--guest-later` asks for that
directly. Debian/Ubuntu guests only for now.

## Lifecycle

| you do | what happens |
|---|---|
| virt-manager Run, `virsh start`, `conduit up NAME` | QEMU connects, backend and virtiofsd start |
| `conduit view NAME` | starts the VM if it is off, then opens the window; on a running VM it attaches (the backend re-sends the current frame) |
| close the window | a VM that `conduit view` started shuts down (unless `--keep-running` or `conduit config set view.close_stops_vm false`); any other keeps running |
| Pause / `virsh suspend` | vCPUs stop; backend and window stay (the window shows the last frame) |
| Resume / `virsh resume` | continues |
| Reboot / `virsh reboot` / `reboot` inside | same QEMU, same backend; the GPU works after it |
| Shut Down / `virsh shutdown` / `conduit down NAME` | ACPI power-off; QEMU exits; backend and virtiofsd exit; boot files refreshed; the window closes |
| Force Off / `virsh destroy` | same, without asking the guest |

**Not supported**: Save (`virsh save`, `managedsave`), snapshots that include
memory, and migration. The GPU's state lives in the host driver, outside
anything QEMU could save, and QEMU refuses ("non-migratable device",
"vhost-user backend lacks VHOST_USER_PROTOCOL_F_LOG_SHMFD"); internal disk
snapshots are refused because the disk is raw. The VM keeps running.

**Memory**: under libvirt, QEMU runs in libvirt's process tree, not in the VM's
slice, so the slice's `MemoryMax` covers only the helpers. `conduit up` /
`view` still refuse to start a VM the host has no room for; a start from
virt-manager does not check.

## Commands

```bash
conduit libvirt enable NAME     # Conduit VM -> libvirt domain (create/import do it by default)
conduit libvirt disable NAME    # remove the domain, units, network unit (the VM stays)
conduit attach NAME [-c URI] [--guest-later] [--dry-run]
conduit detach NAME             # VM shut off; the original definition comes back
conduit doctor NAME             # domain, emulator, AppArmor, sockets, network, backend, guest driver
conduit status NAME             # libvirt state, helpers, display, viewer
conduit logs NAME [backend|virtiofsd|viewer]   # QEMU's own log: ~/.cache/libvirt/qemu/log/NAME.log
```
