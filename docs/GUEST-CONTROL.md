# Guest control: run apps, list them, copy files

`conduit run`, `conduit apps`, `conduit cp` and `conduit app` talk to a small
agent inside the VM. They need no ssh and no network: the VM has a second
virtio-serial port, `org.conduit.ctl.0`, and the host and the agent exchange
one JSON object per line over it. The dashboard's Apps tab uses the same
channel.

```
conduit run win11 -- notepad.exe            # a program, in the VM's desktop
conduit run win11 --app "Counter-Strike 2"  # an installed app, by name
conduit apps win11                          # what is installed
conduit cp report.pdf win11:                # to the VM's Downloads folder
conduit cp win11:C:\Users\me\save.dat .     # from the VM
conduit app add win11 "Counter-Strike 2"    # a launcher on this desktop
```

## How it works

```
conduit (host)  --unix socket-->  QEMU  --virtio-serial-->  agent in the VM
   ctl.sock        org.conduit.ctl.0
```

`conduit attach NAME` adds the channel to the domain (idempotently, next to the
GPU stats channel); QEMU binds `ctl.sock` in the VM's runtime folder
(`$XDG_RUNTIME_DIR/conduit/NAME/`, or `/run/conduit/NAME/console/` for a
system-daemon domain). A VM that was running when the channel was added needs a
restart to get it.

There is no broker. QEMU serves one connection at a time, so a host command
takes an advisory lock (`ctl.lock` beside the socket) for as long as it is
connected, connects, and gives every request a deadline. Two commands at once
(a file copy and the dashboard) take turns; the second waits up to 20 s. Nothing
holds the channel between commands, so a command that is killed leaves nothing
behind, and the agent simply sees the host side close and open again.

The protocol is the `conduit-ctl` crate (`host/ctl`): requests
`{"v":1,"id":N,"op":"run",...}`, responses `{"id":N,"ok":true,...}` or
`{"id":N,"ok":false,"error":"..."}`. Ops: `ping` (agent version, OS, user,
whether a desktop session exists), `run`, `stop`, `apps`, `icon`, `put`, `get`,
`ls`. Request ids grow across commands, so a late answer to an abandoned
request is never taken for the current one. Lines over 4 MiB are dropped, an
unknown op or a malformed line gets an error response, and each agent handles
every request separately, so one bad request cannot stop it.

## Commands

| Command | |
|---|---|
| `conduit run VM [--cwd DIR] [--env NAME=VALUE] [--start] [--no-view] -- CMD [ARGS...]` | Starts the program in the user's desktop session. `CMD` is whatever the guest's shell opens: a program, a document, a `.lnk` or `.desktop` file, a `steam://` URL. A running VM without a window gets one (`--no-view` skips that). A VM that is off is not started unless `--start` is given (then the command waits up to 4 minutes for the agent). Prints the process id. |
| `conduit run VM --app NAME` | Resolves NAME against `conduit apps` (exact name ignoring case, else the start of a name, else part of one; several matches are listed) and starts it. |
| `conduit apps VM [--json]` | The installed apps: name, where it was found, what `run` will start. |
| `conduit cp SRC DST [--force]` | One side is `VM:PATH`. `conduit cp f.txt win11:` goes to the guest user's Downloads folder; `win11:C:\Temp\` (trailing separator or an existing folder) keeps the file name; a bare `win11:name.txt` goes to Downloads. A local path containing a colon needs `./` in front. Prints a progress line and checks size and SHA-256 at the end. On the host a received file has no name until it is complete (an unnamed `O_TMPFILE`, or a fresh `NAME.PID-N.conduit-part` where the filesystem lacks that), and is linked into place only when everything agrees; a guest that changes the size it announced, sends more than it announced, or trickles slower than about 1 MiB/s (plus a minute) is cut off, and a file bigger than the free space is refused up front. Without `--force` an existing file, even one that appears during the copy, is never replaced. Copies to the guest write a new temporary file there (`NAME.<n>.conduit-part` on Windows) and rename it the same way. Files only, up to 8 GiB; for folders and big trees use `conduit share`. |
| `conduit app add VM NAME` | Writes `~/.local/share/applications/conduit-VM-SLUG.desktop` with the app's icon (fetched from the guest and saved under `~/.local/share/icons/conduit/` only if it is a PNG of at most 256x256 and 512 KiB). Its `Exec` is `conduit run VM --start --app NAME`, so the launcher also starts the VM. Errors from a launcher show as a desktop notification. |
| `conduit app rm VM NAME` / `conduit app list VM` | Remove / list the VM's launchers. |

## Dashboard

Key `4` (or Tab) opens Apps for the selected VM: name and source (Start Menu,
Steam, desktop, Flatpak, Snap). `Enter` runs the app, `a` adds a launcher,
`/` filters, `r` reloads, Left and Right switch VM. The list loads on a
background thread with a spinner. Each VM keeps its last list; while it
refreshes, or when a reload fails, the old list stays on screen dimmed with the
reason and the fix above it, and the tab retries every few seconds, so a
rebooting VM or a starting agent fills in by itself. A VM that is stopped, paused
or not attached says so instead.

## Windows guests

The Conduit GPU tray app (`guest/windows/tools/conduit-gpu-tray`) serves the
channel (`\\.\Global\org.conduit.ctl.0`); it is the app the installer already
runs at logon. It runs elevated (the serial port needs it) but starts programs
with the desktop shell's own, non-elevated token, so games and apps do not run
as administrator, and reads, writes and lists files (`cp`, `ls`) while
impersonating that token, so the host reaches exactly the files the desktop
user can. Apps are the Start Menu shortcuts of all users and of the
current user plus Steam games (`libraryfolders.vdf`, `appmanifest_*.acf`, started
as `steam://rungameid/ID`). The tray menu has a Recent launches submenu with the
last eight programs; clicking one stops it (shortcuts, documents and `steam://` URLs are handed to the shell, so they have no process of Conduit's to list or stop).

The port stays open for the life of the app. While no host client is
connected (the CLI connects per request) the VirtIO serial driver fails reads
with `ERROR_NO_SYSTEM_RESOURCES`; the app asks again every 100 ms instead of
closing the port, so a command is never sent into a closed port. State changes
(port opened, closed, open failed) go to `%ProgramData%\Conduit\ctl.log`.

## Linux guests

`conduit-ctl-agent` (`guest/agent`, Python 3, standard library only) runs as a
systemd user service (`conduit-ctl.service`, with an XDG autostart entry for
desktops that start no systemd session) in the user's desktop session. The
`conduit-guest` package installs it with a udev rule that gives the port to the
user of the active local session (`uaccess`) and to nobody else; `conduit attach` installs that package on Ubuntu/Debian and
Arch-based guests (Arch, Omarchy, EndeavourOS, Manjaro), and the `.rpm` carries
the same files for Fedora. It finds the graphical session (`conduit-ctl-agent
--print-session` shows what it found) from the user manager, `loginctl` and the
compositor's environment, so GNOME, KDE, Hyprland, Sway and other Wayland
sessions and X11 work, and starts programs in it. Apps are the `.desktop` files
of the XDG data directories, Flatpak and Snap exports, and Steam (native and
Flatpak). Icons come from the icon theme; a bitmap is scaled to 64 px and an
SVG converted to PNG when the guest has a converter (ImageMagick, rsvg-convert,
inkscape, Pillow or GdkPixbuf). The host takes PNG only, so an app whose icon
is an SVG the guest cannot convert gets a launcher without one. A failed copy
into the guest removes only the `.conduit-part` file that copy made; a file of
that name that was already there is left alone (and the copy refused).

## Files from the guest to the host

The channel only answers the host: nothing in the guest can push a file or a
command through it. Files go the other way through the default shared folder
(`conduit share`): on Windows, Explorer's **Send to Conduit host** (in the
Windows 11 context menu and under Send to) copies the selection into that
share's drive; on Linux, copy into the mounted share. On the host the files
appear in the shared folder (`~/Conduit/VM` by default).

## Security

The channel is a virtio-serial port, reachable only through the QEMU process's
unix socket, which belongs to the user who runs the VM; nothing listens on the
guest's network. The agent obeys the host and nothing else, which is no more
power than the host already has over the VM's disk and memory. In the other
direction the host never runs or interprets anything the guest sends. Every
guest string (app names, paths, arguments, error messages) is cleaned where it
enters the host: control characters, the Unicode line separators and
bidirectional overrides are dropped and the length capped, so nothing the guest
says can move the terminal's cursor or fake a line. In a launcher, the name is
quoted by the Exec rules and then key-file escaped; in a desktop notification
it is escaped as markup. Icons are written only after the PNG signature and
size checks. `cp` writes only to the path the user typed, never through a part
file someone else made. File copies are confined to what the guest user can
access.

## Troubleshooting

`conduit` names which of these applies:

- *is not running*: start the VM (`conduit view VM`).
- *has no control channel*: `conduit attach VM`, then restart the VM.
- *did not answer within N s*: the channel exists but no agent answers. On
  Windows check that the Conduit GPU tray is running and is a version with
  control support (reinstall the Windows package). On Linux run
  `systemctl --user status conduit-ctl` in the VM and check that
  `/dev/virtio-ports/org.conduit.ctl.0` exists and is yours.
- *closed the control channel*: the agent restarted or the VM stopped
  mid-command; run it again.
- *another conduit command is using the channel*: a copy is still running.
- A program starts but no window shows: the VM has no logged-in desktop
  session yet (`conduit run` warns about it).
