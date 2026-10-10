# conduit-ctl-agent

The Linux guest end of Conduit's control channel: the host runs programs in
the desktop session, lists its apps and icons, and copies files, through the
virtio-serial port `org.conduit.ctl.0` (`/dev/virtio-ports/org.conduit.ctl.0`).
No ssh, no network. Protocol and host side: `docs/GUEST-CONTROL.md`,
`host/ctl/src/lib.rs`.

Python 3, standard library only. It runs as the desktop user, in the session.

## Install

The `conduit-guest` package (deb, Arch, rpm) installs everything:

| File | Purpose |
|---|---|
| `/usr/bin/conduit-ctl-agent` | the agent |
| `/usr/lib/systemd/user/conduit-ctl.service` | systemd user unit, enabled globally |
| `/etc/xdg/autostart/conduit-ctl.desktop` | autostart for desktops without a systemd session (XFCE…) |
| `/usr/lib/udev/rules.d/70-conduit-ctl.rules` | the port: `uaccess` (the active local session's user) |

Log out and back in (or run `conduit-ctl-agent &` in the session) after the
first install. Both the unit and the autostart entry may start it; the agent
is single-instance per user.

## What it does

- Session: finds the user's graphical session (`systemctl --user
  show-environment`, `loginctl`, the session leader's and compositor's
  `/proc/PID/environ`, the runtime folder's `wayland-*` sockets and
  `/tmp/.X11-unix`) for `WAYLAND_DISPLAY`, `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`
  and friends, so it works on GNOME, KDE, Hyprland, Sway and other Wayland
  sessions and on X11. `conduit-ctl-agent --print-session` shows what it found.
- Run: `.desktop` files (through `gio launch`, else the Exec line), `steam://`
  URLs, plain programs; started with `systemd-run --user --scope` when
  available, else detached with the session's environment.
- Apps: `.desktop` files of the XDG data directories, Flatpak (system and
  user) and Snap exports, and Steam libraries (native and Flatpak).
- Icons: the freedesktop icon theme lookup (current theme, its parents,
  hicolor, pixmaps); PNG at 64 px when a scaler is installed (ImageMagick,
  rsvg-convert, inkscape, Pillow or GdkPixbuf), else the file as it is (SVG
  is returned as SVG).
- Files: copies through `NAME.conduit-part` and a rename, checked by size and
  SHA-256; an existing file is replaced only when the host says `--force`.

## Troubleshooting

- `systemctl --user status conduit-ctl` shows the log (or the session's log
  for the autostart copy).
- No `/dev/virtio-ports/org.conduit.ctl.0`: the VM has no control channel
  (`conduit attach NAME` on the host, then restart the VM).
- `Permission denied` on the port: the udev rule is not applied yet
  (`sudo udevadm trigger --subsystem-match=virtio-ports`) or the user is
  not in the active local session (the port follows the seat, like a
  webcam; a login outside a seat does not get it).

Tests: `python3 -m unittest test_ctl_agent` in this folder.
