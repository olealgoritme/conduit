# Upscaling and frame generation in the composed present

NVK can upscale an application's frames to its window and generate pictures
between them, from the presented images only (no motion
vectors from the application), in the spirit of Lossless Scaling, AMD Fluid
Motion Frames and NVIDIA Smooth Motion. It runs inside the composed present
(`NVK_HELIOS_WSI_COMPOSE`, the default under a DWM on NVK), on the
application's own Vulkan queue, so it needs nothing from the host and works
on every NVIDIA GPU NVK on RM runs on. Off by default; off, nothing of it
runs and the present behaves as without it.

Patches: `patches-windows-dxvk/0007` (FidelityFX FSR 1, vendored), `0008`
(the passes and the Vulkan swapchain path) and `0009` (D3D11 and D3D12),
files `src/vulkan/wsi/wsi_common_win32_enhance.[ch]`,
`wsi_common_win32_dxgi*.{cpp,h}` and `src/vulkan/wsi/enhance/` in the Mesa
tree.

## Which applications

- **Vulkan**: every swapchain that presents through NVK's Win32 WSI with the
  composed present: native Vulkan applications, and OpenGL through Zink,
  which presents the same way (not measured here).
- **D3D11 and D3D12**: their presents go through DXGI, the Helios UMDs and
  the KMD, never through a Vulkan swapchain (DXVK and vkd3d-proton render
  for the UMDs). In processes whose D3D driver runs on NVK, with the
  settings on for the process and `EnhanceD3D` not 0, NVK hooks DXGI's swap
  chain creation and gives the application a swap chain of its own (see "D3D
  swap chains"). With the settings off for the process, nothing is hooked.

By default the global settings apply to windows whose client area covers
their monitor (borderless and fullscreen); per-exe profiles,
`EnhanceWindows=all` and the environment variables apply to every window of
the process.

## Settings

The registry values are read when a swapchain is made and again whenever
`HKLM\SOFTWARE\Helios` changes (a registry change notification; no polling
of the registry itself). The environment wins over a profile, a profile over
the global values. A change of the scaler on or off, frame generation on or
off, the fit, or a multiplier needing more output slots makes the next
present return `VK_SUBOPTIMAL_KHR`, and the application makes a new
swapchain; the scaler filter, sharpness, multiplier within the slots and
pacing settings take effect at once.

| Global (`HKLM\SOFTWARE\Helios`) | Profile (`...\Helios\Enhance\<exe>`) | Environment | Values | Default |
|---|---|---|---|---|
| `EnhanceScaler` (REG_SZ) | `Scaler` | `MESA_WSI_ENHANCE_SCALER` | `off`, `fsr`, `bicubic`, `bilinear`, `nearest`, `integer` | `off` |
| `EnhanceSharpness` (DWORD) | `Sharpness` | `MESA_WSI_ENHANCE_SHARPNESS` | 0..100 (FSR's RCAS; 0: EASU alone) | 50 |
| `EnhanceFrameGen` (REG_SZ) | `FrameGen` | `MESA_WSI_ENHANCE_FG` | `off`, `2x`, `3x`, `4x`, `adaptive` | `off` |
| `EnhanceFgTarget` (DWORD) | `FgTarget` | `MESA_WSI_ENHANCE_FG_TARGET` | adaptive: pictures per second to fill to; 0 = the display's refresh | 0 |
| `EnhanceFgQuality` (REG_SZ) | `FgQuality` | `MESA_WSI_ENHANCE_FG_QUALITY` | `fast` (flow down to 1/4 of the frame), `quality` (down to 1/2, one more coarse level: twice the reach) | `fast` |
| `EnhanceLowLatency` (DWORD) | `LowLatency` | `MESA_WSI_ENHANCE_LOW_LATENCY` | 1: pictures one refresh apart (the real frame is held less, the cadence is uneven); with fixed multipliers and `FgTarget`, no generation while the game reaches the target | 0 |
| `EnhanceFit` (REG_SZ) | `Fit` | `MESA_WSI_ENHANCE_FIT` | `aspect` (black bars), `stretch` | `aspect` |
| `EnhanceWindows` (REG_SZ) | | `MESA_WSI_ENHANCE_WINDOWS` | `fullscreen`, `all` | `fullscreen` |
| `EnhanceStats` (DWORD) | | `MESA_WSI_ENHANCE_STATS` | 1: a CSV row per frame | 0 |
| `EnhanceD3D` (DWORD) | `D3D` | `MESA_WSI_ENHANCE_D3D` | 0: D3D11/D3D12 swap chains are left to DXGI | 1 |
| | | `MESA_WSI_ENHANCE_SHOW` | `generated`: only generated pictures are shown (to look at them) | all |

The Conduit tray (`conduit-gpu-tray`, elevated) sets the global values from
its menu **Upscaling and frame generation**, and a profile per application
with an open window (**Profile for <exe>**; **Remove profile** goes back to
the global settings). In a profile, a setting without a check mark follows
the global one.

## Upscaling

With a scaler on, a Vulkan surface reports no current extent
(`currentExtent` 0xFFFFFFFF, `minImageExtent` 1x1): DXVK, vkd3d-proton and
native applications that take the size from the swapchain then render at
their own resolution, and the window's DXGI flip swap chain is the window's
size. Each present scales the image into the picture region of an output
slot (a D3D11 shared texture of the window's size, imported into the
application's device like the swapchain images), and the present thread
copies that into the DXGI back buffer as before.

| Scaler | What |
|---|---|
| `fsr` | AMD FidelityFX Super Resolution 1: EASU, then RCAS at the sharpness (two passes, the intermediate at the picture's size). Upscaling only; smaller than the image, bilinear |
| `bicubic` | Catmull-Rom, nine bilinear taps |
| `bilinear`, `nearest` | the plain filters |
| `integer` | the largest whole factor that fits, nearest: sharp pixels for fixed low-resolution games |

`aspect` keeps the image's aspect ratio, centred, with black bars (cleared
once per output slot); `stretch` fills the window.

## Frame generation

Per real frame, in the present's submission:

1. the scaler writes the frame into a history image at the picture's size;
2. a luma pyramid of the frame (1/2 to 1/32) is made from the application's
   image;
3. optical flow between the last two frames, coarse to fine: an exhaustive
   block search at the coarsest level (4 texels, 64 frame pixels at `fast`,
   128 at `quality`), then at each finer level the coarser vectors of the
   texel and its neighbours, refined by one texel. The match is symmetric
   about the generated moment, so every pixel of a generated picture has a
   vector;
4. each generated picture at phase k/n, at the output's resolution, straight
   into its slot: per pixel, of the four nearest vectors and zero motion,
   the one along which both frames agree best (a static HUD keeps zero
   motion over a moving scene); where they disagree, the frame nearer in
   time; where the flow match was poor, a cross-fade;
5. the real frame is copied from the history into its slot.

The present thread waits for the submission's fence (the application's
thread does not), gives the application's image back at once, and presents
the slots: generated pictures first, the real frame last, a frame interval
apart in all (a high-resolution waitable timer and a short spin). A real
frame is shown (n-1)/n of a frame after it was presented: half a frame at
2x. A newer frame arriving during a cycle cuts it short: its real frame is
shown at once and the rest of the cycle is skipped. Nothing waits for a
vblank: the pictures are presented with the swapchain's own sync interval
(0 for IMMEDIATE and MAILBOX).

`adaptive` makes as many pictures per frame as fill the target (the
refresh by default), up to 8: a 30 fps game on a 240 Hz display gets 8, a
60 fps one 4.

It steps aside (shows the real frame alone):

- for the first frame and after a pause (a frame interval over 100 ms or
  four times the average): nothing slides from a stale frame;
- when the game runs near the refresh: pictures stay at least about one
  refresh apart (120 fps on 240 Hz still gets 2x, 200 fps none);
- with low latency and `FgTarget`, while the game reaches the target;
- on a new swapchain (a new history).

Known artifacts: halos at the edges of fast-moving objects (the flow is one
vector per 4x4 frame pixels at `fast`, 2x2 at `quality`), cross-fading
beyond the search reach, wrong vectors on repeating patterns, particles and
transparency, and HUD elements that move with the scene. The cursor DWM
draws is never interpolated.

## D3D swap chains

`wsi_common_win32_dxgi.cpp` (`_dxgi12.cpp` for D3D12). When NVK starts in a
process that has a Helios UMD loaded (not DWM) and the settings are on for
it, a helper thread patches `IDXGIFactory::CreateSwapChain` (slot 10) and
`IDXGIFactory2::CreateSwapChainForHwnd` (slot 15). The UMD's own FlipUpgrade
hooks are installed earlier, when the UMD loads, so NVK's sit in front of
them: a chain NVK does not take goes on through FlipUpgrade as before, and so
does the composed present's own flip swap chain.

NVK takes a chain when the settings apply to its window, its format is
8-bit BGRA or RGBA (sRGB included) and it is not multisampled. The
application then gets an `IDXGISwapChain4` of NVK's:

- **Back buffers** are textures of the application's size on its device: one
  for D3D11 (`GetBuffer(0)`, the flip model's always-current buffer), the
  chain's buffer count for D3D12 (`GetCurrentBackBufferIndex` rotates).
- **Present** copies the back buffer into one of three shared ring textures
  on the application's own queue and returns: D3D11 `CopyResource`, an event
  query and a flush (the immediate context is made multithread-protected, so
  the present thread may ask whether the copy is done); D3D12 a command list
  of its own and a fence signal. It waits only when all three ring textures
  are still being read, at most 100 ms (then the frame is dropped).
- **A thread per swap chain** waits for the copy, runs the passes on an NVK
  device of its own in the process (the ring textures and output slots
  imported by their Helios resource ids: KMT handles for D3D11 textures, NT
  handles for D3D12 resources), and presents the slots paced through the
  composed present's D3D11 device and DXGI flip swap chain on the window,
  exactly as for a Vulkan swapchain. A newer frame cuts a cycle short; older
  queued frames are dropped (only the newest is shown).
- **ResizeBuffers** waits for the thread, rebuilds the buffers, ring, slots
  and passes at the new size (and the output at the window's size).
- **Exclusive fullscreen** is not entered: `SetFullscreenState` is recorded
  and reported back, the window stays as it is (borderless at the desktop's
  mode, as for any frame-generation layer). `GetFrameLatencyWaitableObject`
  is a semaphore released whenever a frame's ring texture has been read.

The copy costs the application's queue one copy of its back buffer per
frame; `Present` itself took 0.25 ms (D3D11, the copy and a flush) and 0.06
ms (D3D12) on average in the tests below.

## Statistics

Every 5 s the composed present's log (`helios-wsi-<pid>.log` in
`%ProgramData%\Helios`, `HELIOS_VK_FRAMETIME_DIR` overrides) gets a line:

```
enhance: 60.9 real + 60.9 generated pictures/s shown (0 generated skipped), 60.0 frames/s in,
  interval 16.66 ms, refresh 4.17 ms; GPU us per frame: scale 98, pyramid 48, flow 80,
  interp 47, copy 33, total 307
```

GPU times come from timestamps around each stage of the submission. With
`EnhanceStats=1` there is also `enhance-<pid>-<id>.csv` with a row per
frame (frame, pictures, interval, the stage times). Generated pictures are
ordinary DXGI presents of the process: PresentMon counts them.

## Cost

Vulkan swapchains: RTX 5090, NVK on RM in the guest, a 240 Hz 5120x1440 desktop, the test
pattern at 60 fps (`vk_enhance_pattern.exe`), GPU clocks not locked (light
load: times vary up to about 2x between runs), microseconds per real frame:

| Swapchain -> picture | Mode | scale | pyramid | flow | interp (all pictures) | copy | total |
|---|---|---|---|---|---|---|---|
| 1280x720 -> 1920x1080 | FSR, 2x | 59-63 | 37-38 | 68 | 25-28 | 20 | 211-216 |
| 1920x1080 -> 1920x1080 | 2x | 28 | 45 | 157-175 | 26-28 | 39-59 | 313-317 |
| 1920x1080 -> 2560x1440 | FSR | 90-97 | | | | | 90-97 |
| 1920x1080 -> 2560x1440 | FSR, 2x | 93-98 | 47-48 | 80 | 39-47 | 33 | 293-307 |
| 1920x1080 -> 2560x1440 | FSR, 2x, `quality` | 92 | 52 | 194 | 49 | 34 | 421 |
| 1920x1080 -> 2560x1440 | FSR, 3x | 89-95 | 54-56 | 82 | 69-77 | 39-44 | 336-351 |
| 1920x1080 -> 2560x1440 | FSR, 4x at 30 fps | 97-107 | 61-80 | 88-146 | 121-134 | 35-75 | 402-541 |
| 1920x1080 -> 2560x1440 | FSR, adaptive at 30 fps (8 pictures) | 99-101 | 61-69 | 114-115 | 297-312 | 43-50 | 615-646 |
| 2560x1421 -> 2560x1421 | 2x | 39-43 | 46-47 | 95 | 83-86 | 34 | 300-301 |
| 2560x720 -> 5120x1440 | FSR, 2x | 167-178 | 49-50 | 80-82 | 70-88 | 63-65 | 430-460 |
| 5120x1440 -> 5120x1440 | 2x | 63-66 | 66-70 | 154-155 | 103-109 | 122-123 | 513-518 |
| 640x360 -> 2560x1440 | integer | 41-44 | | | | | 41-44 |

The present thread's work (one D3D11 copy and `Present` per picture) is on
top. `vkQueuePresentKHR` itself stayed at 0.02 ms on average (0.5 ms worst)
with every mode: the application's thread never waits for a picture's time.

D3D swap chains (`d3d11_enhance_pattern.exe`, `d3d12_enhance_pattern.exe`;
PresentMon over 3 s; GPU microseconds per real frame):

| API | Swapchain -> picture | Mode, game fps | Shown per second | PresentMon | GPU total |
|---|---|---|---|---|---|
| D3D11 | 1280x720 -> 2560x1440 | FSR + 2x, 60 | 60 + 60 | 359 presents, independent flip, 0 dropped, 8.33 ms apart (7.5-8.9) | 280 |
| D3D11 | 1920x1080 -> 2560x1440 | FSR + 3x, 60 | 59 + 118 | 539, 5.55 ms apart (4.6-6.3) | 585 |
| D3D11 | 1920x1080 -> 2560x1440 | FSR, 60 | 60 | 178, 16.66 ms apart | 91 |
| D3D12 | 1280x720 -> 2560x1440 | FSR + 2x, 60 | 60 + 60 | 360, 8.33 ms apart (8.1-8.6) | 277 |
| D3D12 | 1920x1080 -> 2560x1440 | FSR + adaptive, 30 | 30 + 208 | 721, 4.17 ms apart (3.0-5.2) | 841 |
| D3D12 | 5120x1440 -> 5120x1440 | 2x, 60 | 60 + 60 | 359, 8.33 ms apart (7.0-9.6) | 541 |

## Test tools

- `guest/nvk-rm/windows/vk_enhance_pattern.c` (built by
  `windows/build-tests.sh`): a native Vulkan test pattern (fine grid, a
  moving box, a static HUD box, a binary frame counter) with a frame-rate
  cap, in a borderless window over the primary monitor or a window of a
  given size; prints the frame rate and the time `vkQueuePresentKHR` took.
  `VK_DIRECT_DRIVER` points it at a test NVK.
- `guest/windows/tools/d3d11_enhance_pattern.cpp` and
  `d3d12_enhance_pattern.cpp`: D3D11 and D3D12 stand-ins for a game, a back
  buffer smaller than its borderless window, a moving object and a static
  HUD, a frame-rate cap, and optionally a `ResizeBuffers` half way. Run with
  `HELIOS_NVK_ICD=<test vulkan_nouveau.dll>` to test an NVK build in one
  process.

## Not done yet

- D3D10, swap chains for composition or a CoreWindow, HDR and 10-bit
  formats, and multisampled chains (they stay with DXGI).
- NIS, CAS, SGSR and pixel-art scalers, a learned upscaler tier, and a
  learned interpolator tier.
- Profiles from the tray for applications without an open window (the
  registry takes any executable name).
