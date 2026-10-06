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
matching window for a VM that is already running. After `conduit up/view
--venus` it also finds `libvirt-next-venus`, starts `conduit-venus` and runs
the backend with `--venus` ([VENUS.md](VENUS.md); a Windows VM:
[WINDOWS.md](WINDOWS.md)). For an attached VM with a display it
passes `--console-vnc` with the boot console's socket (below).

System domains get the same units in `/etc/systemd/system`, with
`SocketUser=libvirt-qemu` (or `qemu`), sockets in `/run/conduit/NAME/`, and
`User=` you for the helpers. The boot console's socket is the other way round
(QEMU listens, the backend connects), so attach also installs
`/etc/tmpfiles.d/conduit-NAME.conf`: `/run/conduit/NAME/console`, owned by
the QEMU user, group yours, mode 2750. libvirt runs QEMU with umask 002, so the
socket it creates there is group-writable in your group.

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
- `<memoryBacking>` memfd + shared: the backend reads guest RAM (huge
  pages, CPU pinning and other host tuning: [HOST-TUNING.md](HOST-TUNING.md)).
- `<cpu mode='host-passthrough'>` with `<maxphysaddr mode='passthrough'/>`:
  the GPU's shared-memory BAR is 64-bit and large.
- direct kernel boot: `vms/NAME/boot/{vmlinuz,initrd.img}`, copied out of
  the disk's `/boot` (refreshed after every run and before `conduit up`).
- the raw disk (virtio, `cache=none`, `discard=unmap`), the tap, a virtio RNG,
  a serial console (virt-manager shows it; logged to `logs/vm.log`), no
  emulated display (the screen is `conduit view`; with direct kernel boot
  there is no firmware screen to show, so no boot console either).
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
as `pc-q35-noble` belongs to the old QEMU). Conduit's QEMU has VNC but no
SPICE, OpenGL or USB redirection, so attach also changes the display side:

- every `<graphics>` (SPICE, VNC on a port, ...) becomes one
  `<graphics type='vnc' socket='SOCK'/>`: the **boot console**, which the
  backend shows in the Conduit window until the guest driver displays (see
  [SCANOUT.md](SCANOUT.md#boot-console)). SOCK is
  `/run/user/UID/conduit/NAME/console.sock` (session) or
  `/run/conduit/NAME/console/vnc.sock` (system);
- SPICE-only devices go: `<channel>`, `<redirdev>`, `<smartcard>` (and any
  other device) of type `spicevmc` / `spiceport`, and `<redirfilter>`;
- `<audio type='spice'>` becomes `type='none'` (the sound card stays, silent);
- QXL video becomes `virtio` (QXL exists only with SPICE) and
  `<acceleration accel3d>` is dropped (no virgl). The emulated video device
  itself stays: firmware, boot menu and disk-unlock prompt draw on it.
- `<tpm>`, the guest agent channel, inputs and everything else are kept.

A Windows domain also gets the Hyper-V enlightenments it lacks (`vpindex`,
`runtime`, `synic`, `stimer` with `direct`, `reset`, `frequencies`,
`tlbflush`, `ipi`, besides virt-manager's `relaxed`, `vapic`, `spinlocks`)
and `<timer name='hypervclock' present='yes'/>`; settings the domain has stay
([WINDOWS.md](WINDOWS.md)). attach takes a domain for Windows when its
`<metadata>` names a libosinfo Windows OS (`http://microsoft.com/win/...`,
what virt-manager records), when it has `<features><hyperv>`, or when the
running VM's guest agent answers `guest-get-osinfo` with `mswindows`. That is
decided before anything changes.

virt-manager can still open the VM's console (it connects to the same
socket); the Conduit window is the main screen. The
definition from before the first attach is saved as
`vms/NAME/libvirt-backup-TIME.xml`; `conduit detach` defines it again (libvirt
may print elements in another order; the content is the same). Each step is
validated first, the domain is defined in one `virsh define --validate`, and
if libvirt refuses it nothing stays changed.

## Guest side (attach)

`conduit attach` builds `vms/NAME/guest-setup.tar` (the `conduit-guest`
packages, the share mount unit, udev and loader files) before it changes the
domain and, if the VM runs and answers on the QEMU guest agent, uploads it
and runs its `setup.sh` as root. Otherwise it prints one
`ssh ... < guest-setup.tar` command; `--guest-later` asks for that directly.
For a Windows domain none of this happens: attach prints the steps for the
Helios driver package instead (`HeliosSetup.exe`, [WINDOWS.md](WINDOWS.md)). `setup.sh` picks the distribution from
`/etc/os-release` (`ID`, `ID_LIKE`); `sh setup.sh --dry-run` prints what it
would run.

| guest | what `setup.sh` does |
|---|---|
| Debian, Ubuntu (and `ID_LIKE` debian/ubuntu) | apt: `linux-headers-$(uname -r)`, then `conduit-guest.deb` (DKMS build) |
| Arch and `ID_LIKE=arch` (Omarchy, EndeavourOS, Manjaro, CachyOS) | pacman: `dkms`, the running kernel's headers (`/usr/lib/modules/$(uname -r)/pkgbase` + `-headers`: `linux-headers`, `linux-lts-headers`, `linux-zen-headers`, ...), `wl-clipboard`, `libglvnd`, `vulkan-icd-loader`, then `conduit-guest.pkg.tar.zst` (DKMS build); replaces the AUR's `conduit-guest-dkms` |
| anything else (Fedora, ...) | stops with an error; install the `conduit-guest` .rpm by hand |

Both then install the same files: `conduit-guest.service` (loads
`conduit_gpu`, mounts the `nvidia` virtiofs share read-only at `/mnt/nvidia`,
runs `ldconfig`), the udev rules, `/etc/ld.so.conf.d/zz-conduit-nvidia.conf`
and the loader paths into the share (`/etc/profile.d/conduit-nvidia.sh`,
`/etc/environment.d/90-conduit-nvidia.conf`, and the same variables as a
`# >>> conduit >>>` block in `/etc/environment`, which also reaches a display
manager's greeter):

- the Vulkan ICD and GLVND EGL vendor files from the share;
- EGL external platforms from the share first, then the distribution's
  (`/usr/share/egl/egl_external_platform.d`, so an installed egl-wayland is a
  fallback);
- GBM: NVIDIA's backend from the share first, then the distribution's
  (`/usr/lib/x86_64-linux-gnu/gbm`, `/usr/lib/gbm`), which the emulated
  display card needs;
- `AQ_DRM_DEVICES=/dev/dri/conduit-card`: an attached VM has two DRM cards
  (Conduit's and the emulated one), so a udev rule names Conduit's
  `/dev/dri/conduit-card` and Hyprland is pointed at it.
The package itself brings the module autoload and modprobe entries, the
user-namespace sysctl, the logind power-key drop-in and the clipboard agent
(user unit enabled globally, plus an XDG autostart entry). Hyprland started
through uwsm (Omarchy) reaches `graphical-session.target`, which starts it; a
Hyprland started without uwsm runs neither, so add
`exec-once = conduit-clipboard-agent` to `hyprland.conf` there.

pacman runs without `-y` (no partial upgrade): if the headers are no longer
on the mirror, update the VM (`sudo pacman -Syu`), reboot, and attach again.

**Arch guest agent**: `sudo pacman -S qemu-guest-agent` and reboot the VM (or
`sudo systemctl start qemu-guest-agent`); udev starts it on every boot when the
VM has the `org.qemu.guest_agent.0` channel, which virt-manager adds to new
VMs.

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
| Force Off / `virsh destroy` / `conduit poweroff NAME` | same, without asking the guest |
| `conduit pause` / `resume` / `reboot` / `reset` / `shutdown` | the same as the virt-manager buttons (`virsh suspend` / `resume` / `reboot` / `reset` / `shutdown`) |

A clean shutdown that does not finish within 30 s (`conduit down --timeout N`,
also when the window of a VM `conduit view` started closes) is forced off, and
the command says so. GNOME takes the power button over and ignores it, so the
`conduit-guest` package sets `HandlePowerKey=poweroff` and
`PowerKeyIgnoreInhibited=yes` for logind
(`/usr/lib/systemd/logind.conf.d/50-conduit-powerkey.conf`): the power button
powers the VM off even with a desktop session open.

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
