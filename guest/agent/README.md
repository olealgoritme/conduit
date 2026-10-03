# conduit-clipboard-agent

Shares the clipboard of a desktop session inside a Conduit VM with the host.
It moves text between the session's clipboard and `/dev/conduit-clipboard`,
which the guest module (`conduit_gpu`) connects to the host viewer. Design,
wire formats and the host side: `docs/CLIPBOARD.md`.

Python 3, standard library only (Xlib and XFixes are reached through ctypes).
Text only, `text/plain;charset=utf-8`, up to 1 MiB.

## Install

The `conduit-guest` package installs everything:

| File | Purpose |
|---|---|
| `/usr/bin/conduit-clipboard-agent` | the agent |
| `/usr/lib/systemd/user/conduit-clipboard.service` | systemd user unit, enabled globally |
| `/etc/xdg/autostart/conduit-clipboard.desktop` | autostart for desktops without a systemd session (XFCE…) |
| `/usr/lib/udev/rules.d/70-conduit-clipboard.rules` | device node: group `video`, `uaccess` |

Log out and back in (or run `conduit-clipboard-agent &` in the session) after
the first install. Both the unit and the autostart entry may start it; the
agent is single-instance per display (`$XDG_RUNTIME_DIR/conduit-clipboard-*.lock`).

By hand, without the package (module already new enough):

```bash
sudo install -m0755 conduit-clipboard-agent /usr/local/bin/
sudo install -m0644 70-conduit-clipboard.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=misc
install -Dm0644 conduit-clipboard.desktop ~/.config/autostart/conduit-clipboard.desktop
sed -i 's|/usr/bin/|/usr/local/bin/|' ~/.config/autostart/conduit-clipboard.desktop
```

## Which backend

| Session | Backend |
|---|---|
| X11 (XFCE, MATE, Cinnamon, i3, anything on Xorg) | `x11`: XFixes selection-owner notify, `ConvertSelection` (INCR too), owns `CLIPBOARD` to set it |
| Wayland with data-control (sway, labwc, Hyprland, KDE Plasma) | `wayland`: `wl-paste --watch` + `wl-copy` (package `wl-clipboard`) |
| GNOME on Wayland | `x11` through XWayland: mutter mirrors its clipboard to X11 both ways |

`conduit-clipboard-agent --print-backend` shows the order it would try;
`--backend wayland|x11` (or `CONDUIT_CLIPBOARD_BACKEND`) forces one. If the
first fails (no wl-clipboard, compositor without data-control) the next is
tried.

## Troubleshooting

- `cannot open /dev/conduit-clipboard: No such file`: the module is older than
  the clipboard, or the VM has no display (`up --headless`).
- `... Permission denied`: the udev rule is not applied yet (`sudo udevadm
  trigger --subsystem-match=misc`) or the user is neither on the active seat
  nor in `video`.
- Nothing arrives from the host: the viewer runs with `--clipboard off` or
  `to-host`; the viewer log says which. Host text reaches the VM when its
  window gains focus.
- Logs: `journalctl --user -u conduit-clipboard` (or the session's log for the
  autostart copy).
