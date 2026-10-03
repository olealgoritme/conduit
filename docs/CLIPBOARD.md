# Clipboard

Copy in the VM, paste on the host, and the other way round. Text only for now
(`text/plain;charset=utf-8`, up to 1 MiB). On by default in `conduit view`:

```bash
conduit view NAME                        # --clipboard both (the default)
conduit view NAME --clipboard to-host    # VM -> host only
conduit view NAME --clipboard to-guest   # host -> VM only
conduit view NAME --clipboard off
```

The guest needs `conduit-guest` (module + `conduit-clipboard-agent`) and a
desktop session. Nothing else to configure.

## Path

```
host app ─copy─► host clipboard
                    │ viewer reads it on focus-in / change while focused
                    ▼ EV_CLIPBOARD chunks (broker protocol, CAP_CLIP_LARGE)
                 backend ── ClipboardFromHost chunks (event queue, msg 25) ──►
                 guest module ── /dev/conduit-clipboard (read) ──►
                 conduit-clipboard-agent ──► guest clipboard ──paste─► guest app

guest app ─copy─► guest clipboard ──► agent ── /dev/conduit-clipboard (write) ──►
                 guest module ── ClipboardToHost chunks (control queue, msg 26) ──►
                 backend ── CMD_CLIPBOARD records (paced) ──► viewer
                    │ applied while the viewer window is focused (held until then)
                    ▼
                 host clipboard
```

Who decides: the **viewer** (`--clipboard`), from host state only. The backend
and the guest move bytes; neither can widen the policy.

## Rules

- **Host → VM is pushed, not pulled.** The viewer sends the host clipboard on
  focus-in and whenever it changes while the window is focused. That is exactly
  when Wayland lets any client read the selection, and it means paste keys,
  menus and middle-click all just work in the guest. A background VM never sees
  what you copy elsewhere until you switch to it.
- **VM → host is applied only while the viewer is focused**; a copy made while
  it is not is held and applied on focus-in (Wayland requires an input serial
  anyway). The title bar says "THE VM CHANGED YOUR CLIPBOARD" each time.
- **No echo.** The viewer remembers the last text that crossed in either
  direction and does not send it back; the agent does the same on its side.
- **UTF-8 is validated** in the backend and again in the viewer; anything else
  is dropped. Images and files are not carried (yet).
- **Size**: 1 MiB both ways. A larger selection is not sent (logged), and the
  other side keeps what it had.

## Wire formats

Guest ↔ backend (`protocol/src/messages.rs`, `guest/linux/nvgpu_clipboard.h`):

| value | name | direction | queue |
|---|---|---|---|
| 25 | `ClipboardFromHost` | host → guest | event |
| 26 | `ClipboardToHost` | guest → host | control (reply: header, status 0 / -errno) |
| 27 | `ClipboardRequest` | guest → host | control, no payload: send the host clipboard again |

```c
struct clipboard_chunk {   /* 56 bytes after the message header, then `len` bytes */
    u64 generation;        /* per transfer, per direction, nonzero */
    u32 total_len;         /* 1 .. 1 MiB */
    u32 offset;            /* contiguous from 0; offset 0 starts a transfer */
    u32 len;
    u32 flags;             /* 0 */
    char mime[32];         /* "text/plain;charset=utf-8", NUL-padded */
};
```

**Late guests.** The viewer pushes on connect and focus-in, which is usually
before the VM has booted. The backend keeps the newest host clipboard (and
holds a transfer while no guest driver is attached); the guest module sends
`ClipboardRequest` once `/dev/conduit-clipboard` is up and whenever a reader
opens it before any host clipboard arrived, and the backend re-sends what it
has. The module keeps the newest host clipboard, so an agent that starts later
still gets it on open. An older backend answers 27 with an unknown-type error,
which the module ignores.

Host → guest chunks fill the guest's event buffers (464 data bytes each with
today's 520-byte payloads); input events always go first and clipboard chunks
take at most 16 buffers per pass. Guest → host chunks carry up to 4 KiB.
Errors: `-EINVAL` malformed / not UTF-8, `-EMSGSIZE` over 1 MiB, `-EOPNOTSUPP`
other MIME, `-ENODEV` the device has no display.

`/dev/conduit-clipboard` (misc device, 0660, group `video` + `uaccess` via
`70-conduit-clipboard.rules`): every read and write is one record, a 56-byte
header `{u32 magic 0x50494c43 "CLIP", u32 version 1, u64 generation, u32 len,
u32 flags, char mime[32]}` and `len` bytes. `read` blocks until a host
clipboard newer than this open file has seen exists (a new open gets the
current one at once), `EMSGSIZE` if the buffer is too small; `poll` works;
`write` sends one whole record to the host.

Backend ↔ viewer: the broker protocol's existing fixed-size `EV_CLIPBOARD` /
`CMD_CLIPBOARD` chunks, plus `CAP_CLIP_LARGE` (HELLO bit 12) and
`CLIENT_CLIP_LARGE` (CAPS bit 2) for 1 MiB transfers, see
`host/viewer/common/nvkvm_broker_proto.h`. The viewer streams host → guest
transfers into its event ring as it drains; the backend paces guest → host
records at ~32k/s (the broker allows 100k/s from such a client).

Viewer modes: `off`, `guest-to-host` (`to-host`), `host-to-guest`
(`to-guest`), `both`, and the inherited `consent` (host → guest only on a paste
key, 7 KiB). The viewer alone defaults to `off`; `conduit view` passes `both`.

## Desktop matrix

Host (viewer):

| Host session | Reads host clipboard | Writes host clipboard |
|---|---|---|
| Wayland (Hyprland, GNOME, KDE, sway…) | `wl_data_device` selection offer, on every keyboard focus-in and every new selection while focused (either order); text as `text/plain;charset=utf-8`, else `UTF8_STRING`, `text/plain`, `TEXT`, `STRING`; repeats dropped by content hash; a skipped push is logged with the reason | `wl_data_source` (needs the focus serial; held until focus) |
| X11 (any WM) | `ConvertSelection(CLIPBOARD, UTF8_STRING)` on focus-in, INCR supported | owns `CLIPBOARD`, serves `UTF8_STRING`/`STRING`/`TARGETS` |

`conduit view` starts the Wayland backend when `WAYLAND_DISPLAY` is set, else
the X11 one when `DISPLAY` is.

Guest (`conduit-clipboard-agent`, picked from `WAYLAND_DISPLAY`, `DISPLAY`,
`XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP`; override with `--backend` or
`CONDUIT_CLIPBOARD_BACKEND=wayland|x11`):

| Guest session | Backend | Needs |
|---|---|---|
| X11: XFCE, MATE, Cinnamon, i3, GNOME/KDE on Xorg | Xlib + XFixes (ctypes) | libX11, libXfixes (always there) |
| Wayland, wlroots (sway, labwc, Hyprland) | `wl-paste --watch` / `wl-copy` (wlr-data-control) | `wl-clipboard` |
| Wayland, KDE Plasma 6 | same (KWin has wlr-data-control) | `wl-clipboard` |
| Wayland, GNOME 46+ | Xlib + XFixes through XWayland; mutter mirrors its Wayland clipboard to the X11 `CLIPBOARD` both ways | XWayland (default; started on demand) |

GNOME's XWayland serves two displays: the public one (`DISPLAY`, usually
`:0`), whose `CLIPBOARD` mutter mirrors to Wayland, and a private one for its
own services (`GNOME_SETUP_DISPLAY`, usually `:1`). The agent uses the public
one; if it only sees the private one (or none) it takes `DISPLAY` from the
systemd user environment, and logs which display it uses.

GNOME has no data-control protocol, and a Wayland client only sees the
selection while it has keyboard focus, which a background agent never has.
mutter's own XWayland selection bridge is the supported way for a non-focused
client to follow the clipboard. Caveat: a GNOME session started with XWayland
disabled has no clipboard sharing (the agent says so and exits).

## Testing

Unit tests (no VM): `make -C host/viewer check` (`test/test_clipboard_push.py`:
push on focus-in, 300 KB streamed transfer, echo suppression, legacy 7 KiB
client, host-to-guest refusing guest writes), `cargo test` in `host/backend`
(chunk layout, reassembly, pacing, caps), `python3 -m unittest` in
`guest/agent` (record format, echo suppression, backend choice, device loss;
the X11 backend against Xvfb + xclip, including INCR, when they are installed).

End to end in a VM:

```bash
conduit view NAME                       # host
# guest:
ls -l /dev/conduit-clipboard            # crw-rw----+ root video
systemctl --user status conduit-clipboard   # or: pgrep -af conduit-clipboard-agent
journalctl --user -u conduit-clipboard -f   # "host -> guest: N bytes" / "guest -> host"
```

Copy on the host, click into the VM window, paste; copy in the VM, paste on the
host. `conduit logs NAME viewer` shows each transfer ("sending N bytes of the
host clipboard to the VM", "the VM put N bytes on YOUR clipboard").
