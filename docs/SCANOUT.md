# Zero-copy scanout

The guest's desktop appears in a window on the host's Wayland desktop at the
guest's refresh rate (tested at 240 Hz), with **no copy and no compression**. The guest's framebuffers already live in host VRAM (every guest
GEM object is a proxy for a host GEM object), so the host displays the very
same memory.

```
guest compositor ──atomic commit──► guest KMS (guest/linux/, virtual CRTC+plane+connector)
                                       │ ScanoutFlip{owner_handle, host_handle, fourcc, modifier, stride, ...}
                                       ▼ control queue
                                 backend (host/backend/device/)  ── PRIME_HANDLE_TO_FD on the owner's host drm fd ──► dma-buf fd
                                       │ unix socket, nvkvm broker wire protocol (SCM_RIGHTS)
                                       ▼
                                 viewer = conduit-viewer (host/viewer/), Wayland zwp_linux_dmabuf_v1 window
                                       │ keyboard / mouse events back over the same socket
                                       ▼
                                 backend ──InputEvent on the event queue──► guest input_dev (guest/linux/)
```

## Guest ↔ backend messages (host/backend/protocol/src/messages.rs; mirrored in guest/linux/)

All little-endian, `#[repr(C)]`, after the common message header. Display
`MsgType` values:

| value | name | direction | queue |
|---|---|---|---|
| 20 | `ScanoutFlip` | guest → host | control (reply: header only, status) |
| 21 | `ScanoutDisable` | guest → host | control (reply: header only, status) |
| 22 | `InputEvent` | host → guest | event |
| 23 | `DisplayMode` | host → guest | event |
| 24 | `CursorUpdate` | guest → host | control, fire-and-forget like `ScanoutFlip` (reply: header only) |
| 25 | `ClipboardFromHost` | host → guest | event (see docs/CLIPBOARD.md) |
| 26 | `ClipboardToHost` | guest → host | control (reply: header only, status) |

```c
struct scanout_flip {          /* 64 bytes */
    u32 scanout;               /* 0; one scanout for now */
    u32 owner_handle;          /* nvgpu_gem_object.owner_handle */
    u32 host_handle;           /* nvgpu_gem_object.host_handle */
    u32 width, height;
    u32 stride;                /* plane 0 pitch, bytes */
    u32 offset;                /* plane 0 offset, bytes */
    u32 fourcc;                /* DRM_FORMAT_* */
    u64 modifier;              /* DRM_FORMAT_MOD_* (NVIDIA block-linear allowed) */
    u64 seq;                   /* monotonically increasing per flip */
    u32 _reserved[4];
};
struct scanout_disable { u32 scanout; u32 _pad; };
struct input_event_batch {     /* event queue payload */
    u32 count; u32 _pad;
    struct { u16 type; u16 code; s32 value; } ev[];   /* linux input_event triple, EV_SYN included */
};
struct display_mode {          /* 16 bytes, event queue payload */
    u32 scanout;               /* 0 */
    u32 width, height;         /* 64..16384; the guest drops anything else */
    u32 refresh_mhz;           /* millihertz; 0 = keep the current rate */
};
struct cursor_update {         /* 64 bytes */
    u32 scanout;               /* 0 */
    u32 width, height;         /* <= 256 */
    u32 hot_x, hot_y;          /* inside the image */
    u32 owner_handle;          /* nvgpu_gem_object.owner_handle */
    u32 host_handle;           /* nvgpu_gem_object.host_handle */
    u32 stride, offset;
    u32 fourcc;                /* DRM_FORMAT_ARGB8888 */
    u64 modifier;              /* DRM_FORMAT_MOD_LINEAR */
    s32 crtc_x, crtc_y;        /* informational only */
    u32 flags;                 /* bit 0 VISIBLE; clear = hidden, buffer fields 0 */
    u32 seq;
};
```

Device config gains the preferred mode, set by the backend's
`--display WxH@HZ` (default 2560x1440@240), read by the guest at probe:
`u32 display_width, display_height, display_refresh_hz` appended after the
other config fields (gated by the config flag bit `NVGPU_CFG_DISPLAY = 1<<8`;
absent → no KMS). This is the
**configured mode**: what the guest boots with and what "restore" means below.

`NVGPU_CFG_CURSOR = 1<<9` (backend `--display-cursor on`, the default) gives
the head a cursor plane; without it there is none and the guest compositor
draws the cursor into its frames.

## Dynamic resolution

```
viewer window/fullscreen change ──EV_MODE_HINT──► backend ModePolicy ──DisplayMode──► guest KMS
                                                                                      preferred mode := it
                                                                                      drm_kms_helper_hotplug_event
```

- **Who decides**: the viewer, from host state only (window, output, the
  user's `--resize` choice) — never from a guest frame, so there is no loop.
  - fullscreen → the output's exact mode in buffer pixels (fractional scale
    applied, e.g. 5120x1440@240), immediately: covering the output 1:1 is the
    compositor's condition for direct scanout;
  - windowed, `--resize=guest` (default) → the window's own size, debounced
    150 ms after the last configure of a drag;
  - windowed, `--resize=scale` (CTRL+ALT+R toggles) → "restore": the
    configured mode; the viewer fits the **whole** guest picture into whatever
    window the WM gives (aspect kept, black bars, never cropped or stretched).
- **Backend** (`host/backend/device/src/display.rs` `ModePolicy`): turns the hint into a
  `DisplayMode` event, only when it differs from the last one (initially the
  configured mode). `0x0` = configured mode, refresh 0 = configured rate. A
  broker without `CAP_MODE_HINTS` gets the fallback rule: an `EV_SURFACE` flagged
  fullscreen is the output's mode, a windowed one means restore.
- **Guest** (`guest/linux/nvgpu_kms.h`): the event-queue interrupt records the mode,
  a work item makes it the connector's preferred mode (the configured mode
  stays in the list) and fires `drm_kms_helper_hotplug_event`. The connector
  carries `hotplug_mode_update = 1` and `suggested X/Y = 0` (as vmwgfx/qxl),
  which is what makes **mutter and KWin** apply the preferred mode on hotplug
  by themselves. A wlroots compositor (labwc) may only re-probe; switch it with
  `wlr-randr --output Virtual-1 --preferred` (or a kanshi profile).
- Until the guest has switched the viewer scales with aspect (no stretching),
  and shows the buffer 1:1 (no viewport) once sizes match.

## Hardware cursor

```
guest cursor plane commit ──CursorUpdate{owner,handle,hot}──► backend PRIME export (same cache as frames)
                                                                │ CMD_CURSOR + dma-buf fd (hotspot in seq)
                                                                ▼
                                       viewer: zwp_linux_dmabuf (async create) → cursor wl_surface
                                       wl_pointer.set_cursor(surface, hotspot) while over the guest
```

- The plane takes ARGB8888 LINEAR up to 256x256 (`DRM_CAP_CURSOR_WIDTH` 256)
  and the driver sets `DRIVER_CURSOR_HOTSPOT`: atomic compositors must declare
  `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` to see it (mutter 46+, wlroots 0.18+);
  others get no cursor plane and draw their own.
- `CursorUpdate` is sent when the framebuffer (GEM identity, size, pitch),
  hotspot or visibility changes — **never for a move**. The host pointer
  positions the cursor itself, so it moves at host rate with zero guest
  latency; guest `crtc_x/y` is carried for information and ignored.
- Grabbed (CTRL+ALT+G, relative pointer): the host cursor is hidden; the guest
  cursor plane is not composited anywhere, so there is no visible cursor under
  grab (games draw their own).
- The backend keeps the last cursor (a dup of its dma-buf) and re-sends it to a
  broker that connects later or whose socket was full; a broker without
  `CAP_CURSOR` never gets `CMD_CURSOR` (it would be a protocol violation).
- The viewer draws it at the picture's scale (guest pixel → window pixel)
  through a viewport, so it keeps its size relative to the guest desktop.

## Backend → viewer

The backend connects to the broker socket given by `--display-socket PATH`
and speaks the nvkvm broker wire protocol
(`host/viewer/common/nvkvm_broker_proto.h`, version 2): one dma-buf per distinct
`(owner_handle, host_handle)`, exported once and cached, then frame/flip
messages referencing it. Input from the broker is turned into `InputEvent`
batches, mode hints into `DisplayMode`. A dead or absent broker never blocks
the guest: flips are acked and dropped.

Conduit additions to the broker protocol — capabilities and appended
types, no version bump; record sizes unchanged:

| what | direction | meaning |
|---|---|---|
| `CAP_MODE_HINTS` (HELLO w1 bit 10) | broker → backend | the broker sends `EV_MODE_HINT` |
| `EV_MODE_HINT` = 17 | broker → backend | x,y = mode in buffer pixels (0,0 = configured), w0 = refresh mHz (0 = configured), w1 = reason 0 restore / 1 fullscreen / 2 window / 3 fixed |
| `CAP_CURSOR` (HELLO w1 bit 11) | broker → backend | the broker takes `CMD_CURSOR` |
| `CMD_CURSOR` = 7 | backend → broker | with a dma-buf fd: the cursor image (ATTACH's fields, ≤256², AR24, `seq` = hot_x \| hot_y<<16); without: hide, all fields 0 |
| `CMD_CAPS` width bit `CLIENT_SEQ_USEC` (1<<1) | backend → broker | ATTACH/COMMIT `seq` is the backend's CLOCK_MONOTONIC µs at the flip (lets the viewer measure flip → screen) |

## Viewer (host/viewer, Wayland backend)

- Default windowed behaviour: the whole guest picture fitted with aspect. The
  main surface always covers the whole window (exact fit: it carries the guest
  buffer; letterbox: it is a black backdrop and the guest buffer sits on a
  subsurface). Hyprland crops a main surface by the window geometry, so a
  picture surface smaller than the window would show smeared edge columns and
  the desktop behind it.
- Frame timing is always in the window title (~2/s): mode, fps, frame time
  avg/p99, flip→commit, commit→screen, DIRECT/COMPOSITED (from
  `wp_presentation` ZERO_COPY). The stats overlay (CTRL+ALT+O, `--overlay=
  always|windowed|off`, default always) adds frame time now/avg/p99/max, the
  latency segments, drops, input rate and a 2 s frame-time graph; redrawn ≤4/s
  from a timer, never from the frame path.
- Direct mode (CTRL+ALT+D, `--direct-mode=on`): overlay hidden, tearing ASYNC,
  the guest asked again for the output's exact mode, `--direct-hook CMD` run as
  `CMD on|off`. Off: overlay per `--overlay`, tearing VSYNC.

## Invariants

- No CPU copy anywhere. If an import fails, log and drop the frame; never fall
  back to readback.
- Nothing in this path calls NVKMS on the host. Only DRM PRIME export on the
  backend's own host drm fds.
- The cursor is no exception: its buffer is exported and imported like a
  frame's, never read back.
