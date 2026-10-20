# Host sleep: running VMs pause and continue

When the computer suspends (S3 or s2idle) while VMs run, Conduit pauses them
before the host sleeps and continues them after it wakes. Programs in the
guest, games included, carry on where they were, with their GPU state; the
guest's clock is set to the host's again.

This works for libvirt VMs (session and system connections) and for VMs
`conduit up` / `conduit view` run under QEMU, with systemd-logind or elogind.

## How it works

`conduit-sleep.service` is a systemd user unit (`conduit _sleep`). It holds a
logind *delay* inhibitor for sleep (`Inhibit("sleep", ..., "delay")` on
`org.freedesktop.login1`; elogind has the same interface) and listens for the
`PrepareForSleep` signal on the system bus.

Before the host sleeps (`PrepareForSleep(true)`), within 4 seconds in all (the
default `InhibitDelayMaxSec` is 5):

1. For each running VM, the hook asks its GPU backend to keep the guest's
   GPU objects across the pause (`keep` on the backend's power socket,
   `$XDG_RUNTIME_DIR/conduit/NAME/power.sock`). A VM whose backend does not
   answer (one started by an older conduit) is left running: pausing it would
   cost its GPU state, see [Why the backend is asked first](#why-the-backend-is-asked-first).
2. It pauses the VM's vCPUs: `virsh suspend` for a libvirt VM, QMP `stop` for
   a VM `conduit up` runs. A VM that was already paused (by you, or by QEMU
   after an I/O error) is left exactly as it is, and is not continued later.
3. It asks every paused VM's backend at once to `hold`: stop serving guest
   requests (they wait on the virtqueue) and wait, for up to 2 s, until every
   host fence handed to the guest has signalled and every fenced Venus command
   has been returned.
4. It writes down which VMs it paused
   (`$XDG_RUNTIME_DIR/conduit/sleep-paused.json`) and closes the inhibitor, so
   the host sleeps.

The NVIDIA driver saves video memory and restores it after the wake (see
[Requirements](#requirements)); the guest's GPU objects are the backend's RM
objects on the host and come back with it.

After the wake (`PrepareForSleep(false)`):

1. The hook waits, up to 30 s, until the driver's own resume
   (`nvidia-resume.service`) has finished.
2. It tells every backend to `release`. The backend asks RM about each of the
   guest's channel groups with a harmless control (`NVA06C_CTRL_CMD_GET_TIMESLICE`)
   and serves requests again. If the GPU did not come back (RM answers
   `NV_ERR_GPU_IS_LOST` or the device node fails), every fence the guest waits
   on is reported failed (`-ENODEV`) instead of never, so the guest's programs
   get a device-lost error rather than hanging, and the hook shows a desktop
   notification.
3. It continues exactly the VMs it paused (`virsh resume`, QMP `cont`), unless
   one was continued meanwhile.
4. It takes the inhibitor again, then sets each guest's clock, best effort:
   Windows guests through the Conduit tray (`settime` on the control channel,
   [GUEST-CONTROL.md](GUEST-CONTROL.md)); other libvirt guests through the QEMU
   guest agent when it is installed (`virsh domtime --sync`); VMs `conduit up`
   runs with `hwclock --hctosys` over ssh. Otherwise the guest's NTP client
   corrects it.

If the hook is stopped or crashes between sleep and wake, its next start
continues what the file says it paused.

What it did goes to the journal: `journalctl --user -u conduit-sleep`, and the
backend's side to `conduit logs NAME backend` (`power:` lines).

### Why the backend is asked first

QEMU stops a vhost-user device when the VM pauses and starts it again on
continue. Without being told, the backend takes a restart that comes after
served requests for a guest reboot and releases the guest's GPU objects. `keep`
(and `hold`) arm the backend to keep them for the next restart. After
`release` that lasts 60 s more, then a restart is a reboot again. `conduit
pause` and `conduit resume`, and the Pause / Resume button in the viewer's
menu (which runs them), use the same `keep` and `release`, so a paused VM
keeps its GPU state there too. `conduit resume` then sets the guest's clock
the same way as after a wake.

A kept restart keeps the display as well. The viewer goes on showing the
guest's last frame while the VM is paused and after it continues, until the
guest flips again; the boot console does not take over, as it does when a
guest stops the device's queues to reboot. The mode list is not sent to the
guest again, so its compositor sees no hotplug.

### The backend's power socket

One line per connection, one line of answer:

| Command | Answer |
|---|---|
| `hold MS` | `held idle fences=0 chains=0 waited_ms=3`, `held busy fences=N chains=M waited_ms=MS` (still in flight at the deadline), or `busy request in flight; not held` (a request was being served at the deadline) |
| `release` | `released healthy tsgs=N` or `released lost tsgs=N lost=K status=0x...` |
| `keep` | `kept` |
| `status` | `running keep=off fences=0 tsgs=2`, or `held for_ms=812 ...` |

A hold that is never released ends by itself after 120 s of monotonic time
(which does not advance while the host sleeps), so a hook that died cannot
leave a VM's GPU frozen.

## Requirements

- The NVIDIA driver must keep video memory across sleep:
  `NVreg_PreserveVideoMemoryAllocations=1`, with its sleep services enabled
  (`nvidia-suspend.service`, `nvidia-resume.service`, and
  `nvidia-hibernate.service` for hibernation), or kernel suspend notifiers
  (`NVreg_UseKernelSuspendNotifiers=1`) on drivers that have them. The driver
  writes the saved memory to `NVreg_TemporaryFilePath`; when that is a tmpfs
  (often `/tmp`), it goes to RAM. `conduit doctor` checks all of this
  (*GPU memory across sleep*) and prints the commands to fix it; it changes
  nothing itself. Example:

  ```sh
  echo 'options nvidia NVreg_PreserveVideoMemoryAllocations=1 NVreg_TemporaryFilePath=/var/tmp' \
    | sudo tee /etc/modprobe.d/nvidia-sleep.conf
  sudo systemctl enable nvidia-suspend.service nvidia-resume.service nvidia-hibernate.service
  # rebuild the initramfs if it loads the nvidia module (update-initramfs -u,
  # dracut -f or mkinitcpio -P), then restart
  ```

  On elogind systems without systemd, `nvidia-sleep.sh suspend` and `resume`
  must run from elogind's sleep hooks (`/usr/lib/elogind/system-sleep/`).
- A systemd user manager for `conduit-sleep.service`. `conduit up`,
  `conduit view`, `conduit attach` and `conduit libvirt enable` install and
  start it; `conduit doctor` says whether it runs (*Sleep*).
- A backend started by this version (it has the power socket). A VM that was
  already running when Conduit was updated keeps running through a sleep
  without being paused until its next start.

## Settings

```sh
conduit config set vm.pause_on_suspend false   # the hook does nothing
conduit config set vm.pause_on_suspend true    # the default
```

The hook reads the setting at every sleep; while it is false it holds no
inhibitor.

## Limits

- The built-in runner (`--vmm builtin`) cannot pause vCPUs: only its backend
  holds guest requests, and the kernel freezes the VM for the sleep.
- A VM that does not pause in time (QEMU or libvirt not answering within
  1.5 s, or the 4 s budget used up by others) is left running and is frozen by
  the kernel like any process.
- In-flight GPU work that does not finish within the hold's 2 s is left to the
  NVIDIA driver's own suspend, as for any host program.
- A guest channel that RM reports as errored after the wake is the guest
  driver's to recover (its programs see the error). Lost channels are not
  recreated on the host.
- Hibernation (S4) and hybrid sleep send the same signal and go through the
  same steps; the GPU state then also needs `nvidia-hibernate.service`.
- The clock of a Linux libvirt guest without the QEMU guest agent is corrected
  by its NTP client only. The Linux guest agent runs as the desktop user and
  cannot set the clock.
- A stream (`conduit stream`) is not part of this: its encoder session is
  not paused or checked.
