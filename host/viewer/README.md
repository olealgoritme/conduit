# conduit-viewer

The Wayland window that shows a Conduit VM's desktop. The guest's scanout
buffers arrive from `conduit-backend` as dma-bufs over a Unix socket and are
handed to the compositor with `zwp_linux_dmabuf_v1`: no copy, no readback, no
EGL in the viewer. Keyboard, mouse, clipboard and mode hints (window size,
refresh rate) go back over the same socket.

It is a modified copy of the display broker from
[nvkvm-pv](https://github.com/reindertpelsma/nvkvm-pv), Apache-2.0; see
`NOTICE` and `LICENSE`. Socket protocol:
[`docs/broker-protocol.md`](docs/broker-protocol.md)
(`common/nvkvm_broker_proto.h` is the definition). How frames get here:
[`docs/SCANOUT.md`](../../docs/SCANOUT.md).

## Build and test

```sh
make -C host/viewer            # conduit-viewer (+ test tools when GBM/EGL are present)
make -C host/viewer check      # selftests: no display, no GPU (CI runs this)
```

Build dependencies: `libwayland-dev wayland-protocols libxcb*-dev libgbm-dev`
(`make deps` at the repo root installs them).

## Run

Normally `conduit view NAME` starts it. By hand, against a backend started
with `--display-socket`:

```sh
host/viewer/run-viewer.sh                         # listens on $NVGPU_DISPLAY_SOCK
host/viewer/test-standalone.sh 5 240 2560x1440    # no VM: GPU test pattern at 240 Hz
```

`conduit-viewer --help` lists the options (`--overlay`, `--resize`,
`--direct-mode`, `--tearing`, `--stats`, ...). Only the invoking user and root
may connect (SO_PEERCRED; see `--allow-user`).

The window's app id (Wayland) and class (X11) are `conduit-viewer`, for
compositor window rules.

## Keys

| | |
|---|---|
| `Ctrl+Alt+F` | fullscreen (the guest switches to the output's exact mode, so the compositor can scan it out directly) |
| `Ctrl+Alt+G` | grab mouse and keyboard (games); released on focus loss |
| `Ctrl+Alt+O` | performance overlay |
| `Ctrl+Alt+D` | direct mode: overlay hidden, tearing allowed, lowest latency |
| `Ctrl+Alt+R` | the guest's resolution follows the window (default) / its fixed mode is scaled into the window |

The overlay and the window title show fps, frame times, latency and whether the
compositor scanned the buffer out directly (`DIRECT`) or composited it. For
direct scanout on Hyprland see [`docs/SCANOUT.md`](../../docs/SCANOUT.md).
