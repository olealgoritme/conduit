# The viewer: upscaling and frame generation

The viewer can upscale the guest's picture on the host and generate frames
between the guest's frames. Both run on the host GPU, after the guest has
rendered, so they work for any game in any VM, Linux or Windows guest, and on
NVIDIA, AMD and Intel host GPUs. Both are off by default. They are set per
VM in the menu (`Ctrl+Alt+M`) and saved in the VM's `viewer.conf`
([VIEWER.md](VIEWER.md#saved-per-vm)).

| Menu | Choices | `viewer.conf` / flag |
|---|---|---|
| **Upscaling** | Off, FSR 1, NIS, Sharpen only (CAS) | `upscale=off\|fsr\|nis\|cas`, `--upscale=` |
| Sharpness (shown when an upscaler is on) | 0, 25, 50, 75, 100 | `sharpness=0..100`, `--sharpness=` |
| **Frame generation** | Off, 2x, 3x, NVIDIA Smooth Motion (RTX 40/50 host only) | `framegen=off\|2x\|3x\|nvidia`, `--framegen=` |

## Upscaling

The guest renders fewer pixels and the host scales the picture up to the
window with an edge-aware upscaler, instead of the plain bilinear scaling
the compositor does. The game runs at the lower resolution's frame rate, and
the picture looks sharper than plain scaling.

**How to use it.** Set a lower guest resolution in the menu's **Guest
resolution** (for example 1920x1080 on a 3840x2160 or 5120x1440 screen, or
2560x1440 on 3840x2160), keep **Scaling** at Fit or Stretch, and choose an
upscaler. In Windows, set the game to fullscreen or borderless at that
resolution; the guest's desktop then runs at it too. Moderate factors work
best: 1.33x to 2x per axis.

| Mode | What it does | Passes at window size |
|---|---|---|
| FSR 1 | AMD FidelityFX Super Resolution 1: EASU (edge-adaptive upscaling), then RCAS (sharpening). Sharpness 0 is EASU alone, straight into the output. `CONDUIT_VIEWER_FSR_PASSES=1` runs both in one pass (RCAS over EASU computed where it reads it: no intermediate in memory, but five EASU evaluations per pixel; 4x slower on an AMD iGPU, so not the default). | 2 (1) |
| NIS | NVIDIA Image Scaling: a 6-tap scaler with directional filters and adaptive sharpening, one compute pass, then a 1:1 copy into the output buffer (a compute pass cannot write it). For 1x to 2x per axis; above 2x FSR 1 is used instead. | 2 |
| Sharpen only (CAS) | AMD FidelityFX Contrast Adaptive Sharpening, sharpening and scaling in one pass up to 4x the area; above that, or drawn smaller, sharpened at the guest's resolution, then the plain scale. | 1 |

The passes read the guest's buffer in place, as an ordinary texture, when the
driver allows it for the buffer's layout (it says so per modifier); otherwise
the frame is first copied into a texture at the guest's size. The black bars
around a letterboxed picture are cleared once per output buffer, not per
frame. When the window is smaller than the guest's picture, FSR and NIS step
aside for the plain pass; CAS still runs. The log line `upscaling: X asked, Y
runs: why` and the menu say when a different pass runs.

**Priority.** With an upscaler or frame generation on, the viewer's GPU work
runs at the lowest context priority the driver offers
(`EGL_IMG_context_priority` LOW; the Smooth Motion swapchain's queue at
`VK_KHR_global_priority` LOW), so it waits for the guest's GPU work instead of
preempting it; the plain scaler keeps its high priority.
`CONDUIT_VIEWER_GPU_PRIORITY=low|medium|high|realtime` overrides both, to
compare. The log says what was asked and what the driver granted.

Without `wp_viewporter` (some headless compositors) there is no GPU pass:
the viewer logs `upscaling/frame generation off: no wp_viewporter` and the
menu says so.

**Cost.** With an upscaler on, every frame goes through the viewer's GPU pass
(also in a window), so the frame is no longer handed to the compositor as it
is. The GPU pass runs on the same GPU as the guest, so its time is time the
guest's frames wait. The time per frame is in the `--stats` line (`gpu-scale
... pass avg`). `conduit-viewer --bench-enhance=1920x1080:3840x2160 --gpu
/dev/dri/renderD128` times every pass and variant on a GPU without a window or
a VM (GPU time from timer queries, and the wall time to the fence); compare
`CONDUIT_VIEWER_FSR_PASSES=1` (FSR in one pass) and
`CONDUIT_VIEWER_ENH_DIRECT=0` (always copy first) in a session the same way.
Shaders are compiled the first time a mode is used, which takes up to about
100 ms once.

**Latency.** The passes run when the frame arrives and are waited for before
the frame is committed, so they add their GPU time, no queueing.

**Known artifacts.** These are spatial upscalers. They do not add detail that
was not rendered: thin lines and small text at the low resolution stay
soft. High sharpness gives halos at strong edges, and NIS and RCAS sharpen
film grain and noise as well.

## Frame generation (2x, 3x)

The viewer keeps the last two guest frames on the GPU, estimates the motion
between them, and shows interpolated pictures in between: one at 2x, two at
3x. A 60 fps game is shown at 120 or 180 fps. Generated pictures are paced to
the measured guest frame interval.

**How it works.**

1. When a guest frame arrives, it is copied into a two-frame history, and a
   luma pyramid (1/2 to 1/16 of the frame) is built.
2. Optical flow, coarse to fine: an exhaustive block search at the coarsest
   level (up to 64 pixels of motion per frame), then at each finer level the
   coarser vectors of the block and its neighbours, refined by one texel.
   The match is symmetric about the generated moment, so every pixel of the
   generated picture has a vector: there are no holes to fill.
3. Interpolation, per pixel: of the four nearest vectors and zero motion, the
   one along which both frames agree best. Static overlays (a HUD, text)
   pick zero motion under a moving background. Where the frames disagree
   along every candidate (something was covered or uncovered), the pixel is
   taken from the frame nearer in time. Where the flow match itself was poor
   (motion beyond the search range, a scene cut), a plain cross-fade is used.
4. The generated picture goes through the upscaler, if one is on, into the
   same output buffers as guest frames.

All of it is compute and fragment shaders in GLES 3.1, so it runs on any GPU
with a current Mesa or NVIDIA driver.

**Latency: about one guest frame.** Generation can only show what lies
between frames it already has. Frame N is shown (m-1)/m of a frame after it
arrived (half a guest frame at 2x, two thirds at 3x), plus the GPU passes.
The guest itself is never slowed down or synchronised: its buffer goes back
as soon as it has been copied, and nothing waits for a vblank. Generated
pictures use the presentation setting in force (immediate in fullscreen by
default).

**When it steps aside.**

- The guest already runs near the display's refresh rate: a generated
  picture must be about one refresh after the last one, so 3x becomes 2x or
  off (120 fps on 240 Hz gives 2x).
- After a pause (a loading screen, a menu, an idle desktop) the first frame
  is shown as it is, so nothing slides from a stale frame.
- A new guest mode starts a new history.
- With Presentation **Latest per refresh** it is off: that mode commits one
  frame per refresh.

**The cursor.** On Wayland the guest's cursor is the host pointer's image,
drawn by the compositor over the picture, so it is never interpolated. On X11
the guest draws its cursor into the frame, and it is interpolated with the
rest.

**Cost.** Per guest frame, one copy, the pyramid and one flow estimate, all
at guest resolution, submitted together with the first picture (one GPU
submission). Per shown picture, the interpolation at guest resolution (only
for pictures that are shown) and the upscaler, if any; the newest frame
itself needs no interpolation. The `--stats` line adds `framegen Nx: shown
G/s guest + N/s generated`, the guest interval, and the output refresh the
pacing assumes: wp_presentation's, else the output's current mode
(`wl_output`), so an output without presentation feedback (a headless one)
still caps generation at its refresh.

**Known artifacts.**

- Halos and tearing at the edges of fast-moving objects, where the flow is
  only as fine as 2x2 pixels and the occlusion rule picks one side.
- Motion faster than about 64 pixels per frame at the guest's resolution is
  not tracked: those areas cross-fade (ghosting).
- Repeating patterns, transparency, particles and smoke can pick wrong
  vectors.
- Cut scenes cross-fade for one generated picture.
- Interpolated HUD elements that move with the scene, such as name tags,
  can wobble.

## NVIDIA Smooth Motion

On an NVIDIA RTX 40 or 50 series host GPU, the NVIDIA Linux driver has its
own frame generation, Smooth Motion. It is an implicit Vulkan layer
(`VK_LAYER_NV_present`) enabled with `NVPRESENT_ENABLE_SMOOTH_MOTION=1`, and
it only acts on a Vulkan swapchain's presents. In this mode the viewer
presents through a Vulkan swapchain: each guest frame is imported into
Vulkan, copied (scaled and letterboxed) into the swapchain image, and
presented once. The driver generates the frames in between. The swapchain's
present mode follows Presentation: immediate, mailbox for Latest per refresh,
FIFO for VSync. The viewer never waits for a free swapchain image: when none
is free, the frame is held and tried again shortly.

The choice is shown only when the host has an NVIDIA GPU of the RTX 40
generation or later and the driver's present layer is installed. On Wayland
the swapchain gets its own surface when the viewer starts, so choosing it in
the menu takes effect at the next viewer start (the menu says so). On X11 it
starts at once.

**Trade-offs.**

- The `--stats` line reads `smooth motion N frames through the Vulkan
  swapchain` with the copy's time; what the driver generates is not visible
  to the viewer.
- An extra copy per guest frame into the swapchain image, instead of handing
  the guest's buffer to the compositor. The upscalers do not apply in this
  mode (the copy is a plain bilinear or nearest scale).
- Latency and pacing are NVIDIA's: Smooth Motion also holds a frame back to
  interpolate, and how much it adds is up to the driver.
- The cursor: on Wayland the guest's cursor is the host pointer's image, on
  top of the swapchain, so it is not interpolated. The game's own HUD is part
  of the frame and is interpolated by the driver.
- Only on NVIDIA RTX 40/50. On every other GPU, use 2x or 3x.

## Files

| | |
|---|---|
| `host/viewer/nb_gpuscale.c` | the passes (GLES 3.1) |
| `host/viewer/nb_enhance.h` | which pass runs, sharpness, NIS constants |
| `host/viewer/nb_fg.h` | frame-generation pacing |
| `host/viewer/nb_vkpresent.c` | the Smooth Motion presenter (Vulkan) |
| `host/viewer/shaders/` | our shaders; `check.sh` compiles every program offline |
| `host/viewer/third_party/fsr1`, `cas`, `nis` | AMD FidelityFX FSR 1 and CAS, NVIDIA Image Scaling, MIT, unmodified |

The FidelityFX headers are compiled as they are; four constant-setup lines
get a `u` suffix at build time for GLSL ES (`shaders/embed.sh`). NIS is used
through `shaders/nis_scaler_gles.glsl`, a GLSL ES port of `NIS_Scaler.h`
(explicit conversions, same filters and coefficients).

## Not done yet

- **NVIDIA hardware optical flow** (`VK_NV_optical_flow`, Turing and later):
  the flow would come from the GPU's optical-flow engine, at 4x4 blocks and
  with less shader time. It needs the frames in Vulkan (a dma-buf import of
  the history textures, or doing the history in Vulkan) and an interop step
  back to the GLES passes. The shader flow above is the portable path and
  stays the fallback.
- **Bidirectional flow with a consistency check**, to tell covered from
  uncovered areas instead of taking the nearer frame.
- Frame generation on the Smooth Motion swapchain path, and upscaling there.
