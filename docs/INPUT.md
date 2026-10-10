# Input

The viewer's keyboard and mouse reach the guest on one of three paths. The
backend picks the path event by event.

| Path | Used for | How |
|---|---|---|
| Conduit input events | Linux guests with the Conduit driver | The GPU device's event queue; the guest module feeds its own evdev devices (`conduit-gpu keyboard/mouse/tablet`). |
| virtio-input | Every guest whose virtio-input driver runs (Windows with `vioinput`, Linux `virtio_input`) | Three `vhost-user-input-pci` devices served by the backend: **Conduit Mouse** (relative), **Conduit Keyboard**, **Conduit Tablet** (absolute). |
| Boot console | Firmware, boot loaders, a guest with neither of the above | QEMU's VNC server (keys, absolute pointer to the USB tablet) and QMP `input-send-event` (relative motion to the PS/2 mouse). |

## virtio-input

The backend writes each event into the device's event queue on the thread
that reads the viewer's socket. It then interrupts the guest through the
queue's call eventfd, which is KVM's irqfd. QEMU is not on the path, and no
JSON is parsed. No PS/2 port is read byte by byte, and no event waits for
another. Each `SYN_REPORT` the viewer sends becomes one report in the guest.

* **Mouse**: under a grab (Ctrl+Alt+G), it gets relative motion, the
  left/right/middle/side/extra buttons and the wheel. A HID report from
  Windows' `vioinput` carries at most ±127 per axis. A larger step is sent
  as several reports that add up to it exactly.
* **Tablet**: without a grab, it gets positions in `0..=32767` on each axis,
  the same values the boot console's USB tablet gets (the picture the viewer
  shows, pixel-centred). Buttons and wheel go with them.
* **Keyboard**: it gets every key from `KEY_ESC` to `KEY_MICMUTE`, presses
  and releases only. The guest repeats a held key itself. LED reports from
  the guest are taken and dropped.

A device takes input only while its guest driver runs it: the event queue
is live and the driver has posted buffers. Before that, and after a reset
or a driver unload, its input goes to the boot console. A release always
goes where its press went, so a key held across a switch never sticks.
Losing the grab lets go of the mouse buttons held under it. The viewer
releases everything on focus loss, and the backend does the same when a
viewer disconnects.

The backend logs each device's start and stop. Every 10 s it logs a count
line (`virtio-input, last 10 s: ... mouse reports ...`).

### In the VM definition

`conduit attach` adds the devices when the installed backend serves them
(`conduit-backend --help` lists `--input-socket`):

```
-chardev socket,id=conduit-input-mouse,path=$XDG_RUNTIME_DIR/conduit/VM/input-mouse.sock
-device vhost-user-input-pci,chardev=conduit-input-mouse,id=conduit-input-mouse,bus=pcie.0,addr=0x..
```

The keyboard and tablet get the same lines. Each device gets a fixed slot on
the main bus, kept across attaches. The PS/2 keyboard and mouse and the USB
tablet stay for the firmware. `CONDUIT_VIRTIO_INPUT=0 conduit attach VM`
leaves the virtio-input devices out. `conduit detach` restores the original
definition.

QEMU connects to the sockets when it creates its chardevs, before the GPU
starts the backend. The sockets therefore belong to a socket unit of their
own, `conduit-input@VM.socket`, which listens from attach on. The backend's
service inherits them (`Sockets=`). The backend tells them apart by file
name (`input-mouse.sock`, `input-keyboard.sock`, `input-tablet.sock`).
Outside socket activation, `--input-socket KIND=PATH` serves one.

### Guest drivers

* **Linux**: `virtio_input` (in every distribution kernel).
* **Windows**: Red Hat's signed `vioinput` from virtio-win, pinned in CI
  (`guest/windows/ci/windows/Get-VirtioInput.ps1`) and installed by the
  Windows package. By hand: `pnputil /add-driver vioinput.inf /install`.
  The devices appear after the VM's next start.

## Measuring

`host/latency/inputlat.py` measures from the viewer's `send()` to the guest
kernel's event and to the guest's `read()`. It plays a viewer on the VM's
stream socket. In the guest, `guest/tests/inputlat_guest.py` records evdev
timestamps and maps them onto the host's clock with `ptp_kvm`.

```
# host
python3 host/latency/inputlat.py send $XDG_RUNTIME_DIR/conduit/VM/stream.sock > host.csv
# guest (Linux, root), started first
python3 inputlat_guest.py --seconds 30 --match "Conduit Mouse" --match "Conduit Keyboard" > guest.csv
# host
python3 host/latency/inputlat.py analyze host.csv guest.csv
```

`CONDUIT_INPUT_ROUTE=console` in the backend's environment sends a Linux
guest's input down the boot console path (virtio-input when its driver runs,
QEMU's emulated devices otherwise). Use it to compare the paths on one
guest.
