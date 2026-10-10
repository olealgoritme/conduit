# The viewer: resolution, picture area and scaling

`conduit view` opens the VM in `conduit-viewer`. Four settings decide what you
see. They work the same with any guest OS, because all of it happens on the
host:

| Setting | Values | Default |
|---|---|---|
| Guest resolution | `native`, or a fixed `WxH` the viewer asks the guest for | native |
| Picture area | the full window, an aspect preset (`21:9`, `16:9`, `16:10`, `4:3`), or a custom `WxH[+X+Y]` | full |
| Scale | `fit`, `stretch`, `integer`, `none` | fit |
| Filter | `linear` (smooth), `nearest` (sharp) | linear |

- **Native** resolution is the picture area in screen pixels: the window, or
  the smaller area you picked. The guest renders it 1:1, so pick a `16:9`
  area on an ultrawide and the guest runs 16:9 at full sharpness.
- A **fixed** resolution (e.g. `1280x960`) is asked of the guest with the same
  mode hint a window resize sends. If the guest cannot or does not switch, the
  viewer still shows whatever size arrives, scaled.
- The **picture area** is where the picture goes in the window. Presets are as
  large as fits and centred. A custom area is in screen pixels and is centred
  unless you give `+X+Y` (from the window's top-left). Everything outside the
  area is black.
- **Scale** decides how the guest picture fills the area:
  - `fit`: as large as fits with the aspect ratio kept, with black bars.
  - `stretch`: fills the area and ignores the aspect ratio. A 4:3 game
    stretched to 16:9 is `--res 1280x960 --area full --scale stretch`.
  - `integer`: the largest whole multiple that fits. A picture larger than
    the area falls back to `fit`.
  - `none`: 1:1 screen pixels, centred. A picture larger than the area is
    cropped to its middle.
- **Filter** applies to a scaled picture. The X11 viewer applies it
  (XRender `bilinear` or `nearest`). On Wayland the compositor scales the
  picture and picks its own filter, so the menu shows Sharp as unavailable.

## Controls

On the command line (each overrides the saved setting, see below):

```sh
conduit view win11 --res 1280x960 --area 4:3 --scale stretch
conduit view win11 --area 2560x1080+0+180 --scale integer --filter nearest
conduit view win11 --res native --area full --scale fit
```

Keys inside the viewer:

| Keys | What it does |
|---|---|
| `Ctrl+Alt+M` | Open or close the menu |
| `Ctrl+Alt+S` | Next scale mode: Fit, Stretch, Integer, Centered |
| `Ctrl+Alt+A` | Next picture area: Full, 21:9, 16:9, 16:10, 4:3, Custom (only once you have a custom area) |
| `Ctrl+Alt+R` | Next guest resolution: Native, 2560x1440, 1920x1080, 1600x900, 1280x960, your last custom one |
| `Ctrl+Alt+P` | Next saved profile |
| `Ctrl+Alt+arrows` | Move the picture area 8 px (`Shift`: 64 px). A preset area becomes a custom one where it is |
| `Ctrl+Alt+-` / `Ctrl+Alt+=` | Shrink or grow the picture area, keeping its aspect ratio (`Shift`: bigger steps) |
| `Ctrl+Alt+0` | Reset to native, full window, fit, linear |

Each change shows a short notice at the top of the window. The guest gets these
chords only when the viewer does not use them. Ctrl+Alt+F (fullscreen),
Ctrl+Alt+G (mouse grab), Ctrl+Alt+O (stats overlay) and Ctrl+Alt+D (direct
mode) are unchanged.

## The menu

`Ctrl+Alt+M` opens the menu. You can also click the small **Conduit** button
that fades in when the pointer touches the window's top edge. The menu has:

- **Guest resolution**: Native, the presets, your recent custom sizes, and a
  field to type one (`1720x1080`, Enter).
- **Picture area**: the presets and Reset area. While the menu is open, the
  area has a green outline. Drag its edges or corners to resize it, or drag
  inside it to move it, with a live preview. It snaps to the window's centre.
- **Scaling** and **Filter**.
- **Profiles**: save the current setup under a name (for example
  "CS2 4:3 stretched"), click one to load it, `x` to delete it.
  Ctrl+Alt+P cycles through them.
- **View**: the stats overlay and fullscreen.
- **Virtual machine**: Restart and Shut down, which run `conduit reboot` and
  `conduit shutdown` for the VM. Both ask for a second click to confirm.

While the menu is open, keyboard and pointer go to the menu and the area
editor, never to the guest. Keys the guest held are released when it opens,
and a mouse grab is dropped. `Esc` or `Ctrl+Alt+M` closes it.

## Saved per VM

`conduit view` passes `--view-state ~/.local/share/conduit/vms/NAME/viewer.conf`.
The viewer restores the settings, profiles and recent resolutions from that
file at start, and rewrites it on every change. `--res`, `--area`, `--scale`
and `--filter` given to `conduit view` override only the setting they name.
The file is plain `key=value` text:

```
scale=stretch
filter=linear
area=4:3
res=1280x960
recent=1720x1080
profile=CS2 4:3 stretched|scale=stretch filter=linear area=4:3 res=1280x960
```

## The mouse

The absolute pointer is mapped into the guest through the exact rectangle
the picture is drawn in, as the inverse of the draw. This holds for the
scale mode, the area and its offset, a crop, and HiDPI and fractional output
scales. The viewer sends guest pixel coordinates. When the pointer is over
the black bars, the guest pointer rests on the nearest edge of the picture.
In relative mode (`Ctrl+Alt+G`, games) the deltas go to the guest unscaled.
When the guest has a hardware cursor, it is drawn with the same per-axis
scale, so it lines up with the picture in `stretch` too.
`host/viewer/test/test_view.c` checks the mapping for every mode, area and
output scale.

## Cost

Scaling adds no work to the frame path. On Wayland, the guest buffer stays
on one surface. Its `wp_viewport` destination (plus a source crop for `none`)
and its subsurface position do the placing, and the compositor scales the
picture in the composite it does anyway. When the picture exactly fills the
window, it is on the main surface, as before, so direct scanout in fullscreen
is unaffected. On X11, a 1:1 picture is presented as before, and a scaled
picture is one XRender composite, paced on vblank.

The menu, the notice, the button and the area outline are separate layers:
`wl_subsurface`s with their own small shm buffers on Wayland, and child
windows on X11. They are painted only when their state changes and unmapped
when hidden. With the menu closed none of them exists on screen, and the
guest frame stays zero-copy while it is open.

Measured with `--stats` and the `nvgpu-scanout-test` pattern (1280x960 at
240 Hz in a 1691x1393 window), the viewer's own receive-to-commit time is
about 5 µs per frame, and 240 fps is presented with the menu closed or open.

## Display servers

Only core mechanisms are used, so the viewer runs on any desktop:

- **Wayland** (GNOME/Mutter, KDE/KWin, Hyprland, Sway and other wlroots
  compositors): the layers are `wl_subsurface`s (core `wl_subcompositor`) with
  `wl_shm` buffers, translucent. `wp_viewporter`, which scaling needs anyway,
  gives them physical-pixel buffers at fractional scales. Nothing uses
  layer-shell or a compositor-specific protocol.
- **X11** (any window manager, with or without a compositor): the layers are
  child windows painted opaque with `PutImage`, so they need no ARGB visual and
  no compositor. X11 has no per-window scale, so they are drawn at 1x.

Text is drawn with FreeType using the sans face fontconfig picks. A viewer
built without FreeType falls back to the built-in bitmap font.

## Linux guests and the mode hint

A fixed resolution or a native area is sent as the mode hint described in
[SCANOUT.md](SCANOUT.md#dynamic-resolution). The guest driver makes the hinted
mode its connector's preferred mode and sends a hotplug event. Mutter and KWin
switch to it by themselves. A wlroots compositor may only re-probe
(`wlr-randr --output Virtual-1 --preferred` switches it). Hyprland keeps the
mode its `monitor` rule names: with a fixed mode in that rule, the guest
stays at that mode and the viewer scales what arrives.
