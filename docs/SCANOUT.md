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
| 24 | `CursorUpdate` | guest → host | control, fire-and-forget like `ScanoutFlip` (reply: header only); a Windows guest uses `CMD_SET_CURSOR_BLOB` in a `GpuCmd` instead ([below](#hardware-cursor-windows-guests)) |
| 25 | `ClipboardFromHost` | host → guest | event (see docs/CLIPBOARD.md) |
| 26 | `ClipboardToHost` | guest → host | control (reply: header only, status) |
| 27 | `ClipboardRequest` | guest → host | control, no payload: resend the host clipboard (reply: header only, status) |
| 28 | `ScanoutReleased` | host → guest | event, only to a guest that acked `NVGPU_F_SCANOUT_RELEASE` (see [Buffer release](#buffer-release)) |
| 33 | `ScanoutPresented` | host → guest | event, only to a guest that acked `NVGPU_F_SCANOUT_PRESENTED` (see [Presentation feedback](#presentation-feedback)) |

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

Windows guests scan out through Venus (`SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`,
[VENUS.md](VENUS.md)) and reach the same broker path with a dma-buf. Its
modifier is inferred from the blob's size, except for an RM-export blob
(`NVGPU_CFG_RM_IMPORT = 1<<13`, memory NVK on RM rendered into): that one is
shown with the modifier NVK gave nvidia-drm when it imported the memory
(`DRM_FORMAT_MOD_LINEAR` for a pitch layout, NVIDIA block-linear 2D with the
render node's page kind, kind generation and sector layout and NVK's block
height otherwise; VENUS.md "RM-export blobs"), from the dma-buf the backend
holds for it, with no renderer export.

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
  - windowed, `--resize=scale` → "restore": the configured mode; the viewer
    places the guest picture per its display settings.
  - a fixed guest resolution (`--res WxH`, Ctrl+Alt+R, the menu) → that mode,
    whatever the window; a native one is the picture area's size in buffer
    pixels (the window unless a smaller area is chosen). See
    [VIEWER.md](VIEWER.md).
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
- Until the guest has switched the viewer scales per its scale mode (fit by
  default), and shows the buffer 1:1 (no viewport) once sizes match.

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

### Hardware cursor, Windows guests

```
DxgkDdiSetPointerShape ──ARGB into a slot of the KMD's cursor blob──► CMD_SET_CURSOR_BLOB{res, rect, stride, offset, hot}
                                                                        │ GpuCmd on the cursor queue (virtqueue 2), round trip
                                                                        ▼
                                  backend venus/cursor.rs: export the blob once (as a scanout) ──► DisplayLink::cursor
                                                                        │ CMD_CURSOR + dma-buf, as above
                                                                        ▼
                                                    viewer / stream host: unchanged
```

A Windows guest has no GEM pair for its cursor, so its KMD names a Venus
blob instead: `CMD_SET_CURSOR_BLOB` (`0x0380`, a Conduit extension outside
the virtio-gpu ranges; `host/backend/protocol/src/venus.rs`
`SetCursorBlob`, 72 bytes):

```c
struct set_cursor_blob {       /* after the virtio-gpu ctrl header */
    u32 scanout_id;            /* 0 */
    u32 resource_id;           /* the blob; 0 = hide */
    u32 width, height;         /* <= 256 */
    u32 format;                /* VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM (DRM AR24), premultiplied */
    u32 stride, offset;        /* the rectangle in the blob */
    u32 hot_x, hot_y;          /* inside the image */
    s32 x, y;                  /* informational, as crtc_x/y */
    u32 flags;                 /* bit 0 VISIBLE; clear = hidden */
};
```

- Served only with `NVGPU_CFG_VENUS_CURSOR = 1<<18` (set when the backend
  serves Venus and `--display-cursor on`, the default); without it
  `RESP_ERR_UNSPEC`, and the KMD's default keeps the software cursor.
  Refused: a scanout other than 0 (`INVALID_SCANOUT_ID`), an unknown
  resource (`INVALID_RESOURCE_ID`), any other format, a size of 0 or above
  256, a hotspot outside, a stride below `width * 4` or rows past the blob
  (`INVALID_PARAMETER`).
- The blob is exported once (the renderer's scanout export; an RM-export
  blob is its own dma-buf), whatever rectangle it is first named with; each
  update carries its rectangle. The KMD keeps two slots in one blob and
  writes the one not on screen, so the viewer's import cache (keyed by
  dma-buf and offset) holds both and a change is a re-attach.
- Sent for a shape or a visibility change, never for a move, as for Linux.
  A hide (resource 0 or `VISIBLE` clear) is `CMD_CURSOR` without a buffer.
  Unref of the cursor's resource and a device reset hide it.
- The KMD side, its knob (`HwCursor`), counters and the test recipe:
  `guest/windows/docs/independent-flip.md` section 12.


**The cursor queue.** A shape change must never wait behind rendering. The backend serves a third
virtqueue (index 2, like virtio-gpu's cursorq) with the control queue's messages and announces it
with config `features` bit 20 (`NVGPU_CFG_CURSOR_QUEUE`, set with the Venus cursor). It drains the
cursor queue first and again after every control request, so a cursor command waits for at most one
control request, never for the control queue's backlog of Venus `GpuCmd`s. The VMM has to expose the
queue (QEMU `vhost-user-test-device-pci,num_vqs=3`; the Linux driver finds its two queues and ignores
the third). The Windows KMD uses it when both hold and its `HwCursorQ` knob is not 0 (`CurQ` 1); it
polls the queue for the answer (no interrupt). Otherwise the cursor commands stay on the control queue.
A command that times out is not a refusal: the host runs it late and the KMD keeps the host cursor
(`CurTmo`); only an error answer leaves the shape to the software cursor.

## Backend → viewer

The backend connects to the broker socket given by `--display-socket PATH`
(repeatable; see "Several display clients" below)
and speaks the nvkvm broker wire protocol
(`host/viewer/common/nvkvm_broker_proto.h`, version 2): one dma-buf per distinct
`(owner_handle, host_handle)`, exported once and cached, then frame/flip
messages referencing it. Input from the broker is turned into `InputEvent`
batches, mode hints into `DisplayMode`. A dead or absent broker never blocks
the guest: flips are acked and dropped. With no client that wants frames
the flip is not even exported (see below).

Conduit additions to the broker protocol — capabilities and appended
types, no version bump; record sizes unchanged:

| what | direction | meaning |
|---|---|---|
| `CAP_MODE_HINTS` (HELLO w1 bit 10) | broker → backend | the broker sends `EV_MODE_HINT` |
| `EV_MODE_HINT` = 17 | broker → backend | x,y = mode in buffer pixels (0,0 = configured), w0 = refresh mHz (0 = configured), w1 = reason 0 restore / 1 fullscreen / 2 window / 3 fixed |
| `CAP_CURSOR` (HELLO w1 bit 11) | broker → backend | the broker takes `CMD_CURSOR` |
| `CMD_CURSOR` = 7 | backend → broker | with a dma-buf fd: the cursor image (ATTACH's fields, ≤256², AR24, `seq` = hot_x \| hot_y<<16); without: hide, all fields 0 |
| `CMD_CAPS` width bit `CLIENT_SEQ_USEC` (1<<1) | backend → broker | ATTACH/COMMIT `seq` is the backend's CLOCK_MONOTONIC µs at the flip (lets the viewer measure flip → screen) |
| `CAP_CLIP_LARGE` (HELLO w1 bit 12) / `CMD_CAPS` width bit `CLIENT_CLIP_LARGE` (1<<2) | both | clipboard transfers up to 1 MiB in chunks (docs/CLIPBOARD.md) |
| `CAP_GAMEPAD` (HELLO w1 bit 13) | broker → backend | the broker may send `EV_PAD` (a stream host) |
| `EV_PAD` = 18 | broker → backend | one gamepad event: x = evdev code, y = value, w0 = pad << 16 \| evdev type; only to a client that declared `CLIENT_GAMEPAD` (1<<3) |
| `CAP_IDLE` (HELLO w1 bit 14) | broker → backend | the broker starts idle: no frames, no cursor until it sends `EV_ACTIVE`; it is a session client for the mode policy |
| `EV_ACTIVE` = 19 | broker → backend | x = 1 send frames from now on, 0 stop (only from a `CAP_IDLE` broker) |
| `CMD_CAPS` width bit `CLIENT_IDLE` (1<<4) | backend → broker | the backend honours `EV_ACTIVE` and arbitrates the mode between clients, so a session that ends goes idle instead of asking for a restore |
| `CAP_RELEASE_SEQ` (HELLO w1 bit 15) | broker → backend | `EV_RELEASE` (4) is exact: x = the `seq` of the newest ATTACH of that buffer (w0,w1 = its dma-buf inode) the release covers, and every ATTACH is released eventually, a refused or dropped one at once (see [Buffer release](#buffer-release)) |
| `CAP_PRESENTED` (HELLO w1 bit 16) / `EV_PRESENTED` = 21 | broker → backend | the broker answers each commit the display showed: x = the ATTACH's `seq`, y = the `wp_presentation_feedback` kind bits, w0,w1 = the presentation time in `CLOCK_MONOTONIC` ns (0 unknown) (see [Presentation feedback](#presentation-feedback)) |

## Several display clients

A VM can be shown in the local viewer and streamed (conduit-stream:
Moonlight, `conduit remote`) at the same time. The backend takes
`--display-socket` once per client; the CLI passes `display.sock` (the
viewer's) and `stream.sock` (the stream host's), both in the VM's run
directory, so `conduit view` and `conduit stream` work in either order and
closing one leaves the other alone.

```
                                ┌──► display.sock ──► conduit-viewer   (always active)
guest flip ─► backend ─ export ─┤
              (once, cached)    └──► stream.sock  ──► conduit-stream   (active only while a client watches)
```

- **Frames.** Each flip is exported once (the same per-buffer cache) and the
  same dma-buf goes to every client that wants frames, with the fd passed
  per client. Every client has its own non-blocking socket: a full one costs
  that client this frame (latest frame wins, per client) and the link thread
  re-sends the newest frame as soon as that socket drains; a dead one is
  dropped and reconnected on its own. Neither ever delays the guest or the
  other client.
- **Nobody watching costs nothing.** A client that is not connected, or that
  declared `CAP_IDLE` and has not said `EV_ACTIVE` (a stream host with no
  session), gets nothing. With no client that wants frames the flip path is
  one atomic load: no export, no fd dup, no send. The link only remembers the
  buffer's GEM identity (and a dup of its drm file, taken once per file) so
  that a client that becomes active is shown the current picture at once; it
  is exported then, on the link thread. The cursor is handled the same way.
- **Cursor and clipboard** are per client: each gets the cursor when it
  becomes active and whenever it changes; the guest's clipboard goes to every
  connected client, and the host clipboard from any client goes to the guest.
- **Input** from any client goes to the guest (one that takes Conduit
  input; see [Boot console](#boot-console)). Keys and buttons held through
  a client are released when that client disconnects or loses focus, without
  touching what the other holds.
- **Buffer release.** A buffer becomes reusable only once every client that
  was sent it has released it ([Buffer release](#buffer-release)).

### Buffer release

A guest that wants to know when it may draw into a buffer it flipped again
(the Windows KMD's read ledger for KMD-driven flips, NVK's WSI) acks the
**virtio device feature** `NVGPU_F_SCANOUT_RELEASE = 1 << 15` (offered by
the backend whenever it has a display; like `NVGPU_CFG_TAKES_INPUT`, a
device feature the guest acks, not a config `features` bit, and config bit 15
stays unused). Only then does it get `ScanoutReleased` events on the event
queue; a guest that does not ack it (the Linux module, every driver written
before it) gets none, and the backend does no release bookkeeping at all.

```c
struct scanout_released {      /* 32 bytes, event queue, after the header
                                * (msg_type 28, handle 0, status 0) */
    u32 scanout;               /* 0 */
    u32 flags;                 /* SCANOUT_RELEASED_* below */
    u32 owner_handle;          /* ScanoutFlip.owner_handle; 0 for Venus */
    u32 host_handle;           /* ScanoutFlip.host_handle, or the Venus resource id */
    u64 seq;                   /* ScanoutFlip.seq of the buffer's latest flip; 0 for Venus */
    u64 reserved;              /* 0 */
};
#define SCANOUT_RELEASED_RESOURCE  (1u << 0)  /* a Venus SET_SCANOUT_BLOB resource */
#define SCANOUT_RELEASED_NOT_SHOWN (1u << 1)  /* no client was sent its latest flip */
#define SCANOUT_RELEASED_FORCED    (1u << 2)  /* a client did not answer within 500 ms */
```

Meaning: the buffer's latest flip (`seq`) was **replaced** by a flip of a
different buffer (or by `ScanoutDisable`), and every display client that was
sent it has finished reading it. The buffer on the scanout is never
released; it is read until it is replaced. One event per release: a buffer
flipped again is released again later. A `ScanoutFlip` buffer is named as
the flip named it, `(owner_handle, host_handle)`; a Venus scanout
(`SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`) by its resource id with
`SCANOUT_RELEASED_RESOURCE`. A buffer whose GEM handle, file or resource the
guest closes before its release is forgotten (no event).

When a buffer counts as done, per display client:

- **no client wants frames** (none connected, or a stream with no session):
  at once when the next flip replaces it (`SCANOUT_RELEASED_NOT_SHOWN`; the
  same if every socket was full and the frame went to nobody);
- **a client with `CAP_RELEASE_SEQ`** (conduit-viewer's Wayland backend,
  conduit-stream): when it says so with `EV_RELEASE`. The viewer sends it on
  `wl_buffer.release` (the compositor is done), and at once for an ATTACH it
  will never show (refused, dropped by a probe, evicted from its import
  cache); conduit-stream when its pipeline moves on to another buffer (its
  encode, which reads the buffer, has finished with `glFinish`) or when a
  frame is superseded before the pipeline took it. `x` names the newest
  ATTACH covered, so a release crossing a newer send of the same buffer on
  the socket does not count for the newer one;
- **an older client** (no `CAP_RELEASE_SEQ`: an older viewer or stream, the
  viewer's X11 backend): once the backend has sent it a different buffer, the
  "release on next flip" rule everything followed before;
- a client that disconnects or goes idle holds nothing;
- a client that never answers is overruled 500 ms after the buffer was
  replaced (`SCANOUT_RELEASED_FORCED`, counted).

Latency: a release with no client to wait for is put on the event queue by
the thread serving the guest's flip, before the flip's reply; a client's
`EV_RELEASE` is turned into the event by the link thread as soon as it is
read. With no event buffer posted the event waits (a 2 ms retry). With release
on, a `ScanoutDisable` also drops the kept copy of the last frame, so a client
that attaches later is not shown a buffer the guest may be drawing into.

The Linux module would only warn about an unknown event type, but it never
acks the bit, so it never sees one.

### Presentation feedback

A guest that wants to complete a flip when the host display really showed it
(the Windows KMD's `FlipDoneHost`, guest/windows/docs/independent-flip.md
section 13) acks the **virtio device feature** `NVGPU_F_SCANOUT_PRESENTED =
1 << 19` (offered whenever the backend has a display; config bit 19 stays
unused, bit 17 is kept for the proposed host vblank feature). Only then does
it get `ScanoutPresented` events, and only then does the backend keep the
bookkeeping for them.

```c
struct scanout_presented {     /* 48 bytes, event queue, after the header
                                * (msg_type 33, handle 0, status 0) */
    u32 scanout;               /* 0 */
    u32 flags;                 /* SCANOUT_PRESENTED_* below */
    u32 owner_handle;          /* ScanoutFlip.owner_handle; 0 for Venus */
    u32 host_handle;           /* ScanoutFlip.host_handle, or the Venus resource id */
    u64 seq;                   /* ScanoutFlip.seq of the flip shown; 0 for Venus */
    u64 present_ns;            /* host CLOCK_MONOTONIC when it reached the screen (TIMED) */
    u64 sent_ns;               /* host CLOCK_MONOTONIC when the backend sent this event */
    u64 reserved;              /* 0 */
};
#define SCANOUT_PRESENTED_RESOURCE  (1u << 0)  /* a Venus SET_SCANOUT_BLOB resource */
#define SCANOUT_PRESENTED_VSYNC     (1u << 1)  /* vblank-synchronised */
#define SCANOUT_PRESENTED_ZERO_COPY (1u << 2)  /* the guest's buffer itself was scanned out */
#define SCANOUT_PRESENTED_TIMED     (1u << 3)  /* present_ns is known */
```

Where it comes from: a display client that declared `CAP_PRESENTED` (the
viewer's Wayland backend, when the compositor offers `wp_presentation`)
answers every commit the compositor presented with `EV_PRESENTED`, carrying
the ATTACH's `seq` stamp; the backend remembers which guest flip each stamp
(per client, per send) carried and turns the first report of each flip into
the event. One event per flip at most: a second client's report of the same
flip, or a report of an older flip than one already reported, is dropped. A
flip nobody reports (no presenting client, the compositor replaced the commit
before showing it, `discarded`) produces nothing; the guest falls back to its
own timer for it. `sent_ns - present_ns` is the host-side delay, free of any
clock offset.

Delivery: from the link thread as soon as the report is read, into one posted
event buffer; with none posted the event is **dropped** (counted,
`presented_dropped`), never retried, because it is stale within a frame. The
X11 backend and conduit-stream do not declare the capability.

### Mode policy with several clients

The backend keeps the last request (`EV_MODE_HINT`, or the legacy
`EV_SURFACE` rule) of every client and applies exactly one:

1. An **active session client** (one with `CAP_IDLE` that said `EV_ACTIVE`,
   i.e. a stream with a client attached) wins. Its request is the stream's
   resolution and refresh.
2. Otherwise the **most recent** request of any active client wins.
3. Requests that do not win are remembered, not applied. The viewer keeps
   scaling the guest picture into its window (aspect kept), so a viewer next
   to a running stream shows the stream's resolution, and its own resizes
   never re-mode the guest while the stream is on: no ping-pong.
4. When the session ends the stream goes idle (`EV_ACTIVE` 0), which
   withdraws its request: the viewer's most recent request applies again, or
   the configured mode if no other client asked for anything.
5. A client disconnecting withdraws its request the same way, except that
   when it was the last one with a request the guest keeps its current mode
   (as it always did when the only viewer closed).

A stream host talking to an older backend (no `CLIENT_IDLE` in `CMD_CAPS`)
asks for the configured mode at session end instead, as before.

## Boot console

Until the guest's Conduit driver displays, there is nothing to scan out:
firmware, the boot menu and a disk-unlock (LUKS) prompt draw on the VM's
emulated video device. For VMs that have one, QEMU runs a VNC server on a
Unix socket and the backend, started with `--console-vnc PATH`, connects to
it as an RFB client (`host/backend/device/src/console/`) and shows that screen
instead. Input from the window goes to the emulated keyboard and tablet
meanwhile. When the guest driver's first scanout arrives, the backend switches
to it and input goes to the guest driver as usual. The console takes over
again after a device reset, when the event queue stops, or when a
`ScanoutDisable` is not followed by a flip within 250 ms.

A device reset (a guest reboot, or a driver reload such as Windows'
`pnputil /restart-device`) ends the guest's generation
(`DisplayLink::guest_gone`): the backend drops every dma-buf it exported or
kept for it, everything parked by GEM identity, and every buffer awaiting a
release, and sends the viewer a black shared-memory frame at once, so it
never presents the old buffer again (its memory was owned by the guest's RM
clients and is reissued to the next generation). A flip that arrives late
names a file or resource of the old generation and is refused. The log says
`display: device reset: guest scanout generation ended; ...`.

Input follows the picture only for a guest that takes Conduit input. The
rule (`device::display::guest_takes_input`) has two parts, both required:

1. **The guest declares it.** The backend offers the virtio device feature
   bit 12, `NVGPU_CFG_TAKES_INPUT` (a driver feature the guest acks, not a
   config `features` bit; config bit 12 stays unused). The Linux module lists
   it in its feature table and so acks it; the backend sees the acked
   features at device start. A Linux module from before the bit acks only
   `VIRTIO_F_VERSION_1`, but it is recognised anyway: it is the only guest
   that sends `GetSysFiles`/`GetProcFiles` before any `Open`, `Ioctl`,
   `ScanoutFlip` or `GpuCmd`, which every version does at probe. Both are
   forgotten when the device restarts or resets.
2. **Its event queue is live**: the guest has started it and posted buffers
   on it (the Linux driver posts them at probe; cleared when the queue
   stops).

A guest that does not declare it -- Windows, whose Helios KMD acks only
`VIRTIO_F_VERSION_1` and runs the event queue for `EventReady`, but sends
`GetSysFiles` only when an application's NVK on RM forwards one, long after
its own scanout and Venus traffic -- keeps its keyboard and pointer on the emulated
PS/2 keyboard and USB tablet through the console's VNC connection even while
its own frames are shown, since that is all it understands. The frontend must
pass device feature bits through: QEMU's generic vhost-user device does;
conduit-vmm offers the guest only `VIRTIO_F_VERSION_1`, so a guest there is
taken for a Linux one by its `GetSysFiles` (the only guest conduit-vmm runs). The pointer is then
placed by the guest's picture: absolute positions as the same fraction of
QEMU's screen (which QEMU scales onto the tablet's range), relative motion in
guest-frame pixels. Held keys and buttons are released on the side input
leaves at every switch; gamepads only ever go to the guest (dropped, with a
debug line, for one that takes no input). Without `--console-vnc` all input
goes to the guest as before.

There is no dma-buf behind that screen, so its frames are the one exception to
zero copy: XRGB8888 in a sealed memfd, sent with the ATTACH flag `F_SHM`
(`nvkvm_broker_proto.h`). `conduit-viewer` presents them in every present mode
(Wayland `wl_shm`, X11 `PutImage`); `conduit-stream` imports only dma-bufs and
does not show the console.

QEMU opens the VNC socket only after the GPU's vhost-user handshake, so the
socket appears after the backend has started.

Which VMs have it: `conduit attach`ed libvirt VMs (UEFI/BIOS firmware with an
emulated video device; attach replaces their `<graphics>` with
`<graphics type='vnc' socket=.../>`, see LIBVIRT.md). `conduit up` and
Conduit's own libvirt domains boot the kernel directly with no emulated video
(`-display none`), so they have no boot console: adding a VGA would give the
guest a second DRM device, and a desktop (GNOME) may pick the wrong one.
The CLI passes `--console-vnc` only when the VM has a display (not
`--headless`) and its `vms/NAME/libvirt.json` names the socket (VMs attached
before this existed get it on the next `conduit attach`).

## When the session ends

When the last file open on the guest's DRM node closes (the display manager
stopped, the compositor exited and nothing else holds the node), the guest
driver turns the display off, as a driver with fbdev emulation hands it back
to the console then: the host gets a `ScanoutDisable`. A compositor that
exits with `DRM_IOCTL_MODE_CLOSEFB` (mutter does) otherwise leaves its last
frame on the plane, and that framebuffer keeps the buffer's dma-buf, and with
it `conduit_gpu`, in use with no process holding anything. The next
compositor modesets as usual.

## Viewer (host/viewer, Wayland backend)

- The guest picture is placed per the display settings (picture area, scale
  mode; [VIEWER.md](VIEWER.md)) through the picture surface's `wp_viewport`
  (destination, and a source crop for 1:1 larger than the area). The main
  surface always covers the whole window (exact fit: it carries the guest
  buffer; anything else: it is a black backdrop and the guest buffer sits on a
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

- No CPU copy of a guest frame. If an import fails, log and drop the frame;
  never fall back to readback. (The boot console's shared-memory frames are
  QEMU's emulated screen, not a guest GPU buffer.)
- A client that wants no frames costs the flip path nothing (no export, no
  descriptor, no syscall).
- Nothing in this path calls NVKMS on the host. Only DRM PRIME export on the
  backend's own host drm fds.
- The cursor is no exception: its buffer is exported and imported like a
  frame's, never read back.
