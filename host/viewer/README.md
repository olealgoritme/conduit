# host/broker — zero-copy scanout viewer

The host end of [`docs/SCANOUT.md`](../../docs/SCANOUT.md): a Wayland window
that shows the guest's scanout buffers **without copying them**. It is the
nvkvm-pv display broker, vendored (Apache-2.0, see `NOTICE` and `LICENSE`) with
a few changes for virtio-nvgpu.

```
vhost-user-nvgpu --display-socket SOCK ──ATTACH(dma-buf fd)+COMMIT──► viewer ──zwp_linux_dmabuf_v1──► Hyprland
                                       ◄──KEY/BTN/ABS/REL/WHEEL/SURFACE/FRAME/RELEASE──
```

Wire protocol: [`docs/broker-protocol.md`](docs/broker-protocol.md)
(`common/nvkvm_broker_proto.h` is the definition). Design and threat model:
[`docs/broker-design.md`](docs/broker-design.md).

## Build

```bash
sudo apt install libwayland-dev wayland-protocols libgbm-dev libegl-dev libgles-dev   # already present on this host
make -C host/broker            # prints which backends/tools it built
make -C host/broker check      # socket/validator/input/cursor selftests (--backend test: no display, no GPU)
                               # + test-overlay: frame-stats math and the letterbox fit
```

Outputs: `nvkvm-display-broker` (the viewer; links only libwayland-client and
xcb — no EGL, GL or GBM), `nvgpu-scanout-test` (test producer, GBM+EGL).

## Run the viewer

```bash
host/broker/run-viewer.sh
# == nvkvm-display-broker --backend wayland \
#      --socket /run/user/1000/nvgpu-display.sock --size 2560x1440 \
#      --title virtio-nvgpu --present-mode=native --scale aspect --persist --stats
```

Then start the backend with `--display-socket /run/user/1000/nvgpu-display.sock`.
Env: `NVGPU_DISPLAY_SOCK`, `NVGPU_VIEWER_SIZE`, `NVGPU_VIEWER_FULLSCREEN=1`.
Extra arguments are passed through (`--no-tearing`, `--trace-frames`, `--verbose`, …).
`--persist` keeps the window across backend/VM restarts. By default only the
invoking user and root may connect (SO_PEERCRED); see `--allow-user`.

### Hotkeys and input

| | |
|---|---|
| `CTRL+ALT+F` | fullscreen toggle. Fullscreen asks the guest for the output's exact mode (5120x1440@240 here) so Hyprland can scan its buffer out directly; leaving restores the configured mode |
| `CTRL+ALT+G` | grab toggle: keyboard-shortcuts-inhibit + pointer lock + relative motion (games). Released again by the same chord, and **automatically on focus loss** |
| `CTRL+ALT+O` | stats overlay on/off |
| `CTRL+ALT+D` | direct mode on/off: overlay hidden, tearing ASYNC, the guest re-asked for the output's mode, `--direct-hook CMD` run as `CMD on`/`CMD off` |
| `CTRL+ALT+R` | resize policy: `scale` (default — the whole guest picture fitted with aspect into whatever window the WM gives) ⇄ `guest` (the guest switches to the window's size, debounced while dragging) |

Ungrabbed (normal desktop use) nothing needs capturing: absolute pointer
position is sent whenever the pointer is over the window and keyboard input
whenever the window has keyboard focus (Hyprland focus-follows-mouse makes this
seamless). Keys are raw Linux evdev codes. All chords are consumed, never
forwarded.

**Cursor.** When the guest uses its cursor plane (backend `--display-cursor
on`, the default, and a guest compositor that sets cursor hotspots), the
broker receives the cursor image (`CMD_CURSOR`, a dma-buf) and makes it the
**host pointer's** cursor over the guest picture: it moves at host speed with
zero guest latency. Under grab it is hidden. Without a guest cursor plane the
host cursor is hidden over the content and the guest draws its own, as before.

### Frame timing: title and overlay

The window title always carries the numbers, ~2×/s:

    virtio-nvgpu 5120x1440@240 | 238 fps | frame 4.2 ms (p99 5.1) | flip→commit 9 µs | commit→screen 5.7 ms | DIRECT

`DIRECT`/`COMPOSITED` is `wp_presentation`'s ZERO_COPY flag: whether the
compositor actually scanned the guest's buffer out. The overlay (top-left,
CTRL+ALT+O) shows guest and presented fps, frame time now/avg/p99/max over 1 s,
a 2 s frame-time graph (green ≤1.5× the refresh interval, yellow ≤2.5×, red
beyond), the latency segments (flip→recv, recv→commit, commit→screen, total),
mode and scale, DIRECT/COMPOSITED, tearing ASYNC/VSYNC, dropped and superseded
frames per second and input events per second. It is a small shm subsurface
redrawn ≤4×/s from a timer; the frame path only adds counters.

`--overlay=always` (default) shows it windowed and fullscreen,
`--overlay=windowed` hides it in fullscreen, `--overlay=off` starts hidden.
**A visible overlay covers the guest, and a covered surface cannot be scanned
out directly** — in fullscreen that costs DIRECT (the broker logs a one-line
hint); direct mode (CTRL+ALT+D) hides it, the title keeps the numbers.

### Options added for virtio-nvgpu

| option | |
|---|---|
| `--resize=scale\|guest` | windowed resize policy (CTRL+ALT+R), default `scale` |
| `--overlay=always\|windowed\|off` | stats overlay, default `always` |
| `--direct-mode=on\|off` | start in direct mode, default off |
| `--direct-hook CMD` | run `CMD on` / `CMD off` via `/bin/sh` on each direct-mode change, not waited for (compositor tuning lives outside the broker) |
| `--tearing` / `--no-tearing` | ASYNC always / never; default ASYNC only in direct mode |
| `--hint-align=N` | round hinted widths down to a multiple of N (default 1 = exact) |

`run-viewer.sh` env: `NVGPU_VIEWER_RESIZE=guest`, `NVGPU_VIEWER_DIRECT=1`,
`NVGPU_VIEWER_DIRECT_HOOK=CMD`.

## Presentation path (what is zero-copy)

1. ATTACH: the dma-buf fd is validated (it must be a dma-buf, size/stride/offset
   bounded, `(fourcc, modifier)` must be one Hyprland advertised — NVIDIA
   block-linear modifiers included) and imported **once** per buffer with
   `zwp_linux_buffer_params_v1` (`create` for the first buffer of a
   format/modifier pair as an async probe, `create_immed` after). Cached by
   dma-buf inode, 8 slots, so the steady state does no imports at all.
2. COMMIT: `wl_surface_attach` + `damage_buffer` + opaque region +
   `wl_surface_commit` + flush, **immediately** — not held for a frame
   callback. Frame callbacks are forwarded to the client as `FRAME` (the pacing
   hint) and `wl_buffer.release` as `RELEASE`.
3. Latest frame wins: frames are never queued in the viewer; a commit
   superseded before the compositor latched it is reported by
   `wp_presentation` as discarded and counted (`superseded` in `--stats`).
4. `wp_tearing_control_v1` ASYNC is requested by default. Hyprland honours it
   only with
   ```
   general { allow_tearing = true }
   windowrulev2 = immediate, class:^(nvkvm-display-broker)$
   ```
   and only for a fullscreen window. Without it presentation is vsynced.
5. Fullscreen + opaque + no viewport scaling (guest mode == output mode) is
   what lets Hyprland put the buffer on a hardware plane (direct scanout).
   The log line `Present: FLIP` (from `wp_presentation` KIND_ZERO_COPY) says it
   happened; `COPY` means Hyprland composited it (still one GPU blit, no CPU).

There is no EGL, no readback and no shm in the default mode. If an import is
refused the frame is dropped with a loud log line (`ADVERTISED ... and then
refused`); `--present-mode=auto` re-enables upstream's shm fallback for
debugging only.

## Standalone test (no VM)

```bash
bash host/broker/test-standalone.sh 5 240 2560x1440     # SECONDS HZ SIZE
NVGPU_TEST_FULLSCREEN=1 bash host/broker/test-standalone.sh 5 240 2560x1440
```

Opens a viewer window on a private socket and streams a GPU-rendered moving
pattern (3 GBM buffers on `/dev/dri/renderD128`, block-linear, GLES clears,
`glFinish`, exported once) at the given rate, then prints both sides'
per-second lines:

- source: `sent N fps | viewer FRAME N/s | RELEASE N/s | render+finish avg …`
- viewer: `stats: commit N fps, presented N fps (K zero-copy), superseded S |
  recv->commit … | send->commit … | commit->present … | send->present …`

`send->present` is the end-to-end figure (sender's timestamp → scanout start
reported by the compositor). The script checks Hyprland survived and points to
`~/.cache/hyprland/` if it did not.

## Open issues

- Not yet run against the live compositor (host was in use); the import path
  is upstream's, proven on GNOME/sway/weston, not yet on Hyprland 610.57.04.
- `zwp_linux_dmabuf_v1` is bound at v3 (format/modifier events). A compositor
  that only sends v4 feedback would leave the advertised set empty.
- The guest cursor is invisible under grab (the cursor plane is shown only as
  the host pointer, which is locked and hidden then).
