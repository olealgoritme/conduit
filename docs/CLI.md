# The `conduit` command

Source: `cli/` (Rust, one static binary). Build: `cargo build --release -p conduit`
(static: add `--target x86_64-unknown-linux-musl`). Tests: `cargo test -p conduit`.

| Command | What it does |
|---|---|
| `conduit doctor` | Checks KVM, the NVIDIA open driver and whether Conduit supports its version, the Wayland session, sudo, tools, Conduit's own parts, disk space, and the display mode VMs will get. Prints fixes. |
| `conduit create NAME [--size 64G] [--desktop gnome\|xfce\|none] [--ram 4G] [--cpus 4] [--user U] [--tarball FILE] [--no-libvirt]` | Downloads Ubuntu 24.04's cloud root tarball (checked against Ubuntu's signed `SHA256SUMS`), then builds a sparse ext4 disk with Ubuntu's stock kernel (`linux-image-generic` + headers), DKMS and the `conduit-guest` package (DKMS builds the guest driver for that kernel, and again on every kernel update in the VM), the NVIDIA share setup, udev/seat rules, ssh keys and autologin (GDM on Wayland for GNOME). Building uses a loop mount and chroot, so it asks for sudo and says why. The steps are in `cli/assets/build-disk.sh`, and the files installed in the guest are in `cli/assets/guest/`. |
| `conduit import PATH NAME [--move] [--ram 4G] [--cpus 4] [--user U] [--kernel VMLINUX] [--share DIR] [--net N] [--no-libvirt]` | Adopts an existing raw ext4 disk image. It is copied sparsely, or moved with `--move`. Without `--kernel` the VM boots the kernel in the disk's `/boot`. |
| `conduit stock-kernel NAME` | Moves a stopped VM to its distro's own kernel: installs `linux-image-generic`, headers, DKMS and `conduit-guest` into its disk (loop mount + chroot, asks for sudo), disables an old `nvgpu.service`, and removes `"kernel"` from vm.json. See "Moving a VM to the stock kernel" below. |
| `conduit list` | Your VMs, their state, disk use and address. |
| `conduit up NAME [--display WxH@HZ \| --headless] [--vmm qemu\|builtin]` | For a libvirt VM: starts the domain (`virsh start`; resumes a paused one) with that display. Otherwise: starts the network, GPU backend and VM in the background. The VM runner is the bundled QEMU (with a virtio-sound card through PipeWire/PulseAudio; set `CONDUIT_AUDIO=off` for none); `--vmm builtin` uses the small built-in runner, which has no sound and can only boot a `"kernel"` file (ELF vmlinux), not the disk's own kernel. Without QEMU, conduit falls back to the built-in runner and says so. It keeps a display ready so `conduit view` can attach later. |
| `conduit view [NAME] [WxH@HZ] [--tune-hyprland] [--fullscreen] [--keep-running] [--clipboard both\|to-host\|to-guest\|off] [--vmm qemu\|builtin]` | Opens the viewer window (Wayland, or X11 when there is no Wayland session) and starts the VM if it is not running. Clipboard sharing defaults to `both` (docs/CLIPBOARD.md). With no mode given, the VM gets your monitor's mode (Hyprland, then wlr-randr, then the kernel's preferred size, then 2560x1440@60). Closing the window shuts the VM down only when this command started it (not with `--keep-running`, or after `conduit config set view.close_stops_vm false`); a VM started by `conduit up`, virt-manager or virsh keeps running, and `conduit view NAME` reattaches any time. The window title and the command's output say which applies. Without NAME it opens the only VM, or the most recently used one (and says so; `conduit view NAME` picks another). |
| `conduit down NAME [--timeout N] [--force]` | Shuts the VM down cleanly: the ACPI power button (QMP `system_powerdown`, or `virsh shutdown` for a libvirt VM), plus `systemctl poweroff` over ssh after 10 s for a VM `conduit up` runs; after `--timeout` seconds (default 30) it is forced off (QMP `quit` / `virsh destroy`) and the command says so. `--force` turns it off at once. Then stops the backend, virtiofsd and viewer, restores Hyprland and (not for libvirt VMs, whose network stays) removes the network. A command started while another one stops or starts the VM waits for it (up to 90 s) and says so. |
| `conduit shutdown NAME [--timeout N]` | Presses the power button and waits (default 60 s); never forces, and fails with a hint if the VM is still on. |
| `conduit reboot NAME` | Restarts the guest cleanly (`virsh reboot`; a VM `conduit up` runs: `systemctl reboot` over ssh). |
| `conduit reset NAME` | Hard reset (`virsh reset` / QMP `system_reset`). |
| `conduit poweroff NAME` | Turns the VM off at once (`down --force`). |
| `conduit pause NAME` / `conduit resume NAME` | Freezes / continues the vCPUs (`virsh suspend`/`resume`, QMP `stop`/`cont`); memory, GPU state and the window stay. Not with the built-in runner. |
| `conduit status [NAME]` | Shows the processes, display, network and whether the guest can be reached. |
| `conduit logs NAME [backend\|vm\|viewer\|watcher\|share\|virtiofsd\|create] [-f] [-n N]` | Shows the logs (default 40 lines). |
| `conduit ssh NAME [-u USER] [CMD...]` | Opens a terminal in the VM, or runs a command there. |
| `conduit libvirt enable\|disable NAME` | Makes a Conduit VM a libvirt domain in `qemu:///session` (virt-manager: File > Add Connection > QEMU/KVM user session), or removes that domain again. `create` and `import` enable it by default when libvirt is installed (`--no-libvirt` skips it). Installs the socket-activated `conduit-backend@NAME` / `conduit-virtiofsd@NAME` user units and the root `conduit-net-NAME.service` (sudo, once). See docs/LIBVIRT.md. |
| `conduit attach NAME [-c URI] [--guest-later] [--dry-run]` | Gives an existing libvirt VM (session or system) Conduit's GPU: backs its definition up to `vms/NAME/libvirt-backup-TIME.xml`, installs the units, defines the edited domain in one step (rolled back if libvirt refuses it), and installs the guest driver through the QEMU guest agent when the VM runs one (else prints the command). A Windows VM (libosinfo metadata, Hyper-V features, or the guest agent's `guest-get-osinfo`, checked before anything changes) also gets the Hyper-V enlightenments and no Linux guest setup; attach prints the Helios driver steps instead (docs/WINDOWS.md). SPICE graphics and devices are replaced by Conduit's boot console (a VNC socket the window shows until the guest driver displays; docs/LIBVIRT.md). Running it again changes nothing. |
| `conduit detach NAME` | Restores the definition from before `attach` (VM shut off) and removes the units. |
| `conduit config get [KEY] \| set KEY VALUE \| unset KEY` | Settings in `~/.config/conduit/config.json`. `view.close_stops_vm` (true/false, default true): closing the window of a VM that `conduit view` started shuts it down. `gpu.window_mib` (`auto`, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072 or 262144; unset = `auto`): the shared window every guest CPU mapping of GPU memory goes through, in MiB, from the next backend start. `auto` is the host GPU's BAR1 rounded up to a power of two, as Resizable BAR gives a bare-metal driver (32768 on an RTX 5090, 131072 on an RTX PRO 6000), clamped to what the guest's 64-bit MMIO window can hold (2 TiB on a CPU with 46 or more physical address bits, see [ARCHITECTURE.md](ARCHITECTURE.md#limits)), and 4096 when no NVIDIA GPU is found. The backend resolves it (`conduit-backend --print-window-mib` prints the number); `conduit up` gives that one number to the backend as `--window-mib` and to conduit-vmm, and under QEMU the backend reports it to QEMU itself. Address space, not memory. |
| `conduit stream NAME [--preset top\|balanced\|compat] [--port N] [--display WxH@HZ] [--service \| --stop] [--link] [--video-encryption] [--link-mbps N]` | Streams the VM to Moonlight (and, with `--link`, to `conduit remote`). Also `conduit stream pair PIN`, `clients`, `unpair CLIENT`, `status`, `token`. See docs/STREAMING.md. |
| `conduit remote HOST[:PORT] [--token T] [--lossless] [--codec C] [--bitrate B] [--fps N] [--yuv444] [--fullscreen]` | Shows a VM another computer streams with `--link` (default port 48100). |
| `conduit trace NAME [-f] [--filter WHAT] [--summary] [-o FILE] [--format json\|bin] [--duration S]` | Records or watches the GPU requests of a running VM. Also `conduit trace NAME status\|on\|off` and `conduit trace analyze FILE`. See docs/TRACING.md. |
| `conduit doctor NAME` | Checks one VM's whole chain: disk and kernel, backend, QEMU, virtiofsd, and for libvirt VMs the domain (metadata, memfd, GPU device, share, emulator and AppArmor), the sockets, the network unit, the state, the running backend and the guest driver version. |

**Memory.** `up` and `view` refuse to start a VM when its RAM, plus host overhead
(a quarter of it, at least 1 GiB) and a 2 GiB margin, exceeds `MemAvailable` minus the RAM the
other running Conduit VMs have not touched yet; `--no-mem-check` overrides that. Guest RAM is
faulted in as the guest uses it, never committed at boot. Each VM's backend, runner and
virtiofsd run in one systemd user slice, `conduit-NAME.slice`, with `MemoryMax` = RAM +
overhead and `memory.oom.group=1` (an OOM kill takes the whole VM, which is what frees its
shared RAM), and with `oom_score_adj=500`, so under memory pressure the kernel kills a VM
before your desktop. `conduit down` stops the slice. Without a systemd user session (or with
`CONDUIT_NO_SCOPE=1`) the processes run unconfined.

`--venus` (experimental, `up`/`view`, also libvirt VMs): the GPU backend serves Venus for a Windows guest (`conduit-backend --venus`, docs/VENUS.md), rendered by a `conduit-venus` started with it (logs/venus.log; `CONDUIT_VENUS_LD_LIBRARY_PATH` becomes its `LD_LIBRARY_PATH`). Off by default. It needs a backend built with the `venus` cargo feature (the packages and `make backend` have one; docs/VENUS.md, or point `CONDUIT_BACKEND` at one) and `conduit-venus`, which the packages have (`packaging/build.sh venus`): conduit looks for it in `$CONDUIT_PREFIX/bin`, next to the `conduit` binary, and in a checkout in `host/venus/target/{release,debug}` (or `CONDUIT_VENUS=PATH`); build it with `host/venus/build-virglrenderer.sh`, then `cargo build --release --features renderer` in `host/venus`. Region 3 needs QEMU, not `--vmm builtin`. Setting up a Windows VM: docs/WINDOWS.md.

`--tune-hyprland` is opt-in. While the viewer runs, it sets `misc:no_direct_scanout 0`, `general:allow_tearing 1` and an `immediate` rule for the viewer, then restores the old values. If the viewer supports `--direct-hook`, Ctrl+Alt+D turns these settings on and off.

## Where things are

| | |
|---|---|
| Settings | `~/.config/conduit/` (`ssh/id_ed25519` is the key used for VMs) |
| VMs | `~/.local/share/conduit/vms/NAME/{disk.img, vm.json, logs/}` |
| Running state | `/run/user/$UID/conduit/NAME/` (pid files, `gpu.sock`, `display.sock` (viewer), `stream.sock` (stream host), `trace.sock`, `vfs.sock`, `state.json`, `qmp.sock`, `qemu.args` or `vmm.json`, `venus.sock` with `--venus`; libvirt VMs: `gpu-libvirt.sock`, `vfs-libvirt.sock`, `libvirt-mode`, and `console.sock` (the boot console) for attached session VMs) |
| libvirt | `vms/NAME/libvirt.json` (which domain), `vms/NAME/libvirt-backup-*.xml` (attach), `~/.config/systemd/user/conduit-{backend,virtiofsd}@*`, `/etc/systemd/system/conduit-net-NAME.service`, `~/.local/share/applications/conduit-NAME.desktop` |
| Boot files | `~/.local/share/conduit/vms/NAME/boot/{vmlinuz,initrd.img}`: the newest kernel in the disk's `/boot`, copied out with `debugfs` before every start |
| Cache | `~/.cache/conduit/` (Ubuntu image, the NVIDIA user-space files staged for each driver version) |
| Programs | `$CONDUIT_PREFIX` (default `/opt/conduit`): `bin/conduit-{backend,vmm,viewer,userspace,stream}`, `bin/conduit-venus` and `lib/libvirglrenderer.so.1` (when packaged), `bin/qemu-system-x86_64`, `share/conduit/guest/conduit-guest.deb` (and `.pkg.tar.zst` for `attach` on Arch guests), `share/conduit/supported-drivers.txt`. In a source checkout it falls back to the build outputs (`host/*/target/release/…`, `host/viewer/conduit-viewer`, `host/qemu/build/…`, `dist/out/conduit-guest_*_all.deb` / `conduit-guest-*-any.pkg.tar.zst`, built on demand with nfpm). |

To override a single part, set one of `CONDUIT_BACKEND`, `CONDUIT_VMM`, `CONDUIT_VIEWER`,
`CONDUIT_USERSPACE`, `CONDUIT_STREAM`, `CONDUIT_VENUS`, `CONDUIT_GUEST_DEB`, `CONDUIT_GUEST_ARCH`,
`CONDUIT_QEMU` or `CONDUIT_VIRTIOFSD`.

Each VM gets its own network: tap `conduitN`, with the host at `172.30.N.1` and the VM at
`172.30.N.2`. NAT uses MASQUERADE without naming an uplink, so it keeps working when a VPN
connects or disconnects. Setting up and removing the network are both idempotent.

Processes are tracked only by pid file. Before sending a signal, conduit checks the
process name, so it never matches processes by pattern.

By default, the guest's NVIDIA user-space is staged from the host's loaded driver (`conduit-userspace --stage`).
To use a prepared folder instead, set `"share"` in `vm.json`, or pass `import --share`.

## Moving a VM to the stock kernel

VMs made before Conduit booted the distro's kernel name a kernel file in their
vm.json (`"kernel": ".../vmlinux"`) and load a guest module built for it. To
move one (here `lab`) to Ubuntu's stock kernel with the driver from DKMS:

```sh
conduit down lab
cp --sparse=always --reflink=auto ~/.local/share/conduit/vms/lab/disk.img ~/lab-disk-backup.img   # optional backup
conduit stock-kernel lab      # installs linux-image-generic, headers, dkms, conduit-guest
conduit up lab                # boots 6.8.0-*-generic from the disk, under QEMU
conduit ssh lab -- 'uname -r; lsmod | grep conduit_gpu'
```

To go back, put the `"kernel"` line back into vm.json (the command prints it)
and use the old disk or keep this one: the custom kernel still boots it.
