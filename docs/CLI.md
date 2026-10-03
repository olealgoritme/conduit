# The `conduit` command

Source: `cli/` (Rust, one static binary). Build: `cargo build --release -p conduit`
(static: add `--target x86_64-unknown-linux-musl`). Tests: `cargo test -p conduit`.

| Command | What it does |
|---|---|
| `conduit doctor` | Checks KVM, the NVIDIA open driver and whether Conduit supports its version, the Wayland session, sudo, tools, Conduit's own parts, disk space, and the display mode VMs will get. Prints fixes. |
| `conduit create NAME [--size 64G] [--desktop gnome\|xfce\|none] [--ram 8G] [--cpus 4] [--user U] [--tarball FILE]` | Downloads Ubuntu 24.04's cloud root tarball (checked against Ubuntu's signed `SHA256SUMS`), then builds a sparse ext4 disk with the guest driver, the NVIDIA share setup, udev/seat rules, ssh keys and autologin (GDM on Wayland for GNOME). Building uses a loop mount and chroot, so it asks for sudo and says why. The steps are in `cli/assets/build-disk.sh`, and the files installed in the guest are in `cli/assets/guest/`. |
| `conduit import PATH NAME [--move] [--user U] [--kernel VMLINUX] [--share DIR] [--net N]` | Adopts an existing raw ext4 disk image. It is copied sparsely, or moved with `--move`. |
| `conduit list` | Your VMs, their state, disk use and address. |
| `conduit up NAME [--display WxH@HZ \| --headless]` | Starts the network, GPU backend and VM in the background. It keeps a display ready so `conduit view` can attach later. |
| `conduit view NAME [WxH@HZ] [--tune-hyprland] [--fullscreen]` | Opens the viewer window and starts the VM if it is not running. With no mode given, the VM gets your monitor's mode (Hyprland, then wlr-randr, then the kernel's preferred size, then 2560x1440@60). Closing the window shuts the VM down. |
| `conduit down NAME` | Shuts the guest down cleanly over ssh, then stops the VM, backend and viewer, restores Hyprland and removes the network. |
| `conduit status [NAME]` | Shows the processes, display, network and whether the guest can be reached. |
| `conduit logs NAME [backend\|vm\|viewer] [-f] [-n N]` | Shows the logs. |
| `conduit ssh NAME [-u USER] [CMD...]` | Opens a terminal in the VM, or runs a command there. |
| `conduit attach NAME [--dry-run] [-c URI]` | Changes a libvirt VM to use the GPU: sets the emulator (system QEMU at 11.1 or newer, else `/opt/conduit/bin`), adds memfd shared memory and `vhost-user-device-pci,virtio-id=45`, and installs a libvirt hook that runs the backend. **Gated** until the backend supports QEMU: only `--dry-run` works, unless you set `CONDUIT_EXPERIMENTAL_QEMU=1`. |

`--tune-hyprland` is opt-in. While the viewer runs, it sets `misc:no_direct_scanout 0`, `general:allow_tearing 1` and an `immediate` rule for the viewer, then restores the old values. If the viewer supports `--direct-hook`, Ctrl+Alt+D turns these settings on and off.

## Where things are

| | |
|---|---|
| Settings | `~/.config/conduit/` (`ssh/id_ed25519` is the key used for VMs) |
| VMs | `~/.local/share/conduit/vms/NAME/{disk.img, vm.json, logs/}` |
| Running state | `/run/user/$UID/conduit/NAME/` (pid files, `gpu.sock`, `display.sock`, `vmm.json`) |
| Cache | `~/.cache/conduit/` (Ubuntu image, the NVIDIA user-space files staged for each driver version) |
| Programs | `$CONDUIT_PREFIX` (default `/opt/conduit`): `bin/conduit-{backend,vmm,viewer,userspace}`, `share/conduit/vmlinux`, `share/conduit/guest/virtio_gpu_nv.ko`, `share/conduit/supported-drivers.txt`. In a source checkout it falls back to the build outputs (`target/release/…`, `host/viewer/…`, `guest/…`). |

To override a single part, set one of `CONDUIT_BACKEND`, `CONDUIT_VMM`, `CONDUIT_VIEWER`,
`CONDUIT_USERSPACE`, `CONDUIT_KERNEL`, `CONDUIT_GUEST_MODULE` or `CONDUIT_QEMU`.

Each VM gets its own network: tap `conduitN`, with the host at `172.30.N.1` and the VM at
`172.30.N.2`. NAT uses MASQUERADE without naming an uplink, so it keeps working when a VPN
connects or disconnects. Setting up and removing the network are both idempotent.

Processes are tracked only by pid file. Before sending a signal, conduit checks the
process name, so it never matches processes by pattern.

By default, the guest's NVIDIA user-space is staged from the host's loaded driver (`conduit-userspace --stage`).
To use a prepared folder instead, set `"share"` in `vm.json`, or pass `import --share`.
