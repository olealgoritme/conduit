# D3D11 hardware video decode on NVK on RM

Windows apps decode video through the D3D11 video API (D3D11VA / DXVA):
Chromium and Edge, Media Foundation (and through it most players), FFmpeg's
`d3d11va` hwaccel. Until now neither the Helios UMD nor DXVK implemented a
D3D11 video *decoder*, so every app in the guest decoded on the CPU
(`guest/nvk-rm/docs/video.md` on research/nvk-rm-video, option (b)).

This adds:

1. **A D3D11 video decoder in the Helios DXVK fork** on Vulkan Video
   (`third_party/patches/dxvk/0004-helios-d3d11-video-decoder-on-vulkan-video.patch`).
   H.264 (`D3D11_DECODER_PROFILE_H264_VLD_NOFGT`), NV12 output, decoded
   straight into the app's D3D11 texture, which the video processor, shaders
   and copies then use like any other texture.
2. **The D3D11.1 video DDI in the UMD** (`umd/src/forward/video.rs`): decoder,
   video processor and decoder/processor views, forwarded to DXVK the way all
   other DDIs are. Without it the D3D11 runtime offers apps no
   `ID3D11VideoDevice`, whatever DXVK can do.
3. **A test**: `tools/d3d11va_decode_test.c`, an H.264 D3D11VA decode
   checked frame by frame against libavcodec's software decode, plus a video
   processor (NV12 -> BGRA) check, and `tools/d3d11va-decode-test.ps1` to run
   it and FFmpeg's `d3d11va` hwaccel app-local.

It needs NVK with Vulkan Video decode on GB20x: patch `0035` (NVDEC channel
through RM, research/nvk-rm-video) and a build with `-Dvideo-codecs=h264dec`.

## Results (RTX 5090 GB202, RM 610.57.04, win11 guest, 2026-10-06)

App-local: DXVK's `d3d11.dll`/`dxgi.dll` next to the test, NVK through the
`vulkan-1.dll` shim (`guest/nvk-rm/windows/vulkan_shim.c`), no driver change.
Every decoded frame read back, converted to yuv420p and MD5'd like
`ffmpeg -f framemd5`, compared with libavcodec's software decode:

| clip (testsrc2 + libx264) | d3d11va_decode_test | FFmpeg `-hwaccel d3d11va` | video processor check |
|---|---|---|---|
| 640x480 constrained baseline, 60 frames | 60/60 bit-exact | 60/60 | 60/60 |
| 640x480 high, 2 B-frames, 60 frames | 60/60 | 60/60 | 60/60 |
| 1920x1080p60 high, 3 B-frames, 4 refs, 600 frames | 600/600 | 600/600 | 600/600 |
| 1280x720 high, 4 slices/frame, 3 B-frames, 120 frames | 120/120 | 120/120 | 120/120 |
| 1280x720 main CAVLC, 8 slices/frame, 90 frames | 90/90 | 90/90 | 90/90 |
| 1280x720 high, custom scaling matrices (`cqm=jvt`) | 0/90 | 0/90 | - |

The scaling-matrix clip fails in FFmpeg's own Vulkan hwaccel on NVK the same
way (0/90): NVK's H.264 decoder programs flat weight scales
(`rust/video/decode/h264.rs`, `WeightScale`) and ignores the SPS/PPS lists.
That is an NVK gap, not the DXVA mapping, which passes the lists through.
Most consumer H.264 (web video) uses flat matrices.

The video processor check compares a 7x7 grid of flat-area pixels of the
BGRA blit with a CPU BT.601 conversion of the same frame: worst channel
difference 1.

Decode speed, 1080p clip, 600 frames, frames left on the GPU:

| path | time | fps |
|---|---|---|
| FFmpeg `-hwaccel d3d11va`, DXVK -> NVK (default threads) | 0.63 s | ~950 |
| FFmpeg `-hwaccel d3d11va -threads 1` | 0.67 s | ~895 |
| d3d11va_decode_test `-bench` (1 thread) | 0.77 s | ~780 |
| FFmpeg `-hwaccel vulkan` on NVK, for reference (research/nvk-rm-video) | 0.65 s | ~925 |

So the D3D11 layer costs nothing measurable over the native Vulkan path.

## How the decoder works (DXVK)

- **Device**: `VK_KHR_video_queue`, `_decode_queue`, `_decode_h264` are enabled
  when present, with a queue from the video decode family
  (`DxvkDeviceQueueSet::videoDecode`). `DXVK_HELIOS_VIDEO_DECODE=0` keeps them
  off. Without them (Venus, NVK without `NVK_EXPERIMENTAL=video`) the decoder
  reports no profiles and nothing else changes.
- **Profiles and configs**: H264_VLD_NoFGT, NV12, `ConfigBitstreamRaw` 2
  (short slice control, preferred by FFmpeg and Chromium) and 1 (long).
  NV12 reports `D3D11_FORMAT_SUPPORT_DECODER_OUTPUT`.
- **Surfaces**: a `D3D11_BIND_DECODER` NV12 texture gets
  `VIDEO_DECODE_DST|DPB` usage with the H.264 profile list, `CONCURRENT`
  sharing between the graphics and decode queue families (no ownership
  transfers), and is never relocated (the decoder records its raw `VkImage`).
  A decoder output view is a per-layer `VkImageView`.
- **Parameters**: `DXVA_PicParams_H264` and `DXVA_Qmatrix_H264` carry every
  SPS/PPS field the hardware needs (it parses slice headers itself), so a
  StdVideo SPS and PPS with id 0 are rebuilt from them each frame, and a new
  `VkVideoSessionParametersKHR` is made only when they change. IDR is read
  from the first slice's NAL header (DXVA does not flag it).
- **Slices**: Vulkan needs only the slice offsets, which are the slices'
  `BSNALunitDataLocation` (start code included, as in Vulkan). Several
  `SubmitDecoderBuffers` per frame accumulate (Chromium submits when its
  buffer is full).
- **DPB**: DXVA names pictures by surface index, Vulkan by DPB slot. A surface
  holds a slot while `RefFrameList` references it and gives it up as soon as it
  does not; the current picture takes a free slot. Output and DPB coincide
  (`DPB_AND_OUTPUT_COINCIDE`, what NVK supports): the picture is decoded into
  the app's texture layer, which later serves as the reference.
- **Bitstream**: the app writes straight into a host-visible Vulkan buffer
  (8-slot ring, one slot per frame in flight); no copy.
- **Synchronization**: per frame the CS thread brings the touched layers into
  their default layout (`DxvkContext::prepareImageForExternalQueue`), signals
  a timeline fence and flushes the graphics list, waits until that submission
  reached `vkQueueSubmit` (no reliance on wait-before-signal), submits the
  decode on the decode queue (waiting for that fence, signalling the decode
  fence), and makes the next graphics submission wait for the decode fence.
  The decode's command buffer moves the layers GENERAL -> DPB -> GENERAL. The
  CS chunk is flushed right after each frame so decodes reach the GPU at once.
- **Video processor fix**: studio-range YCbCr input now also expands chroma
  (DXVK expanded only luma, so colors came out ~12% desaturated), and
  `Nominal_Range` 0-255 is no longer treated as studio range.

Limitations: H.264 only (NVK has no other decoder); progressive only (NVK's
caps); 8-bit 4:2:0; no content protection; scaling matrices ignored by NVK.
In the app-local test DXVK reports an AMD vendor id (its NVIDIA-hiding
default), which makes FFmpeg send the scaling lists in raster order (its ATI
workaround); with NVK ignoring them this changes nothing today.

## UMD: the D3D11.1 video DDI

The D3D11 runtime asks for the video function table through
`PFND3D10DDI_RETRIEVESUBOBJECT` (`D3D11_1DDI_VIDEO_FUNCTIONS`). `CreateDevice`
now hands out `retrieve_sub_object` for the 11.1 and WDDM 1.3 interfaces;
it fills `D3D11_1DDI_VIDEODEVICEFUNCS` for devices the `VideoDdi` knob admits
and refuses (E_NOTIMPL, as before) otherwise.

- Decoder, video processor, enumerator and the three view kinds are bare-COM
  handle slots holding the DXVK object, like the other views.
- Profiles, formats, configs, caps, rate conversion, filter ranges and all
  processor state setters forward to `ID3D11VideoDevice` /
  `ID3D11VideoContext` (the DDI structs are the API structs field for field;
  conversions are size-checked reinterpretations).
- Decoder buffers: the runtime creates one resource per buffer type
  (`pfnGetVideoDecoderBufferInfo`: picture parameters, IQ matrix, slice
  control, bitstream) with `DecoderBufferType` set, maps it for the app, and
  names it in `pfnVideoDecoderSubmitBuffers`. `create_resource` makes these
  CPU-readable staging buffers (and `resource_map` turns a DISCARD map of one
  into WRITE); `SubmitBuffers` copies each into DXVK's decoder buffer of the
  same type (`GetDecoderBuffer`, the bitstream one being DXVK's Vulkan
  buffer) and submits through the API: one memcpy per buffer per frame.
- Content protection (crypto sessions, authenticated channels) refuses.
- `CheckFormatSupport`: with the video DDI the video processor and decoder
  output bits pass through; without it decoder output is removed (DXVK
  reports it on NVK) as video bits already were for multisample formats.
  The encoder bit is always removed.

Knobs (`HKLM\SOFTWARE\Helios`):

| knob | default | |
|---|---|---|
| `VideoDdi` (DWORD) | 1 | 0 = no video DDI (the old behaviour), 1 = NVK devices, 2 = every device (Venus gets the DXVK video processor, no decoder). Unless 0, the bridge also puts `NVK_EXPERIMENTAL=video` in NVK processes' environment (appended to an existing value). |
| `NvkAllowList` (REG_SZ, S3) | - | takes executables off the NVK deny-list: the per-app opt-in for testing video apps on NVK, e.g. `msedge.exe;msedgewebview2.exe` |
| `HELIOS_ICD=nvk` (environment, S3) | - | one launch on NVK regardless of the lists |

**Deny-list decision: browsers and players stay on Venus by default.** The
UMD path is untested until the package is installed, NVK video is
experimental upstream, and those apps share surfaces with Venus processes,
which NVK cannot import yet (dxvk-on-nvk S3). Test them with `NvkAllowList`.

## Building

DXVK app-local (MinGW, on the host):

```sh
cd guest/windows/third_party/dxvk
git apply ../patches/dxvk/0001-*.patch ../patches/dxvk/0002-*.patch ../patches/dxvk/0003-*.patch ../patches/dxvk/0004-*.patch
meson setup build64 --cross-file build-win64.txt --buildtype release \
    -Denable_d3d8=false -Denable_d3d9=false -Denable_d3d10=false -Db_vscrt=none
ninja -C build64    # build64/src/d3d11/d3d11.dll, build64/src/dxgi/dxgi.dll
```

NVK: `MESON_ARGS=-Dvideo-codecs=h264dec guest/nvk-rm/build-windows.sh` with
`patches-windows/0035`. The test app, against a shared FFmpeg's `include/`
and `lib/` (BtbN win64-lgpl-shared):

```sh
x86_64-w64-mingw32-gcc -O2 -o d3d11va_decode_test.exe guest/windows/tools/d3d11va_decode_test.c \
    -I<ffmpeg>/include -L<ffmpeg>/lib -lavformat -lavcodec -lavutil -ld3d11 -ldxgi -luuid -lole32
```

Then put everything in one directory in the guest (see the header of
`tools/d3d11va-decode-test.ps1`) and run
`d3d11va-decode-test.ps1 -Clip 1080 -Tool app` / `-Tool ffmpeg` / `-Mode bench`.

The driver package (`ci/vm/win-build.sh`) picks the DXVK patch up through
`Build-Driver.ps1`.

## Install and test plan for the UMD path (Edge / Chromium)

Needs a driver install (main session), so it is a plan:

1. Build the package from this branch (`WIN_ROOT` of your choice;
   `ci/vm/README.md`), with `HELIOS_KMD_VERSION` bumped in
   `kmd_render/driver-version.env` so pnputil replaces the installed driver.
2. Stage an NVK built with 0035 and `-Dvideo-codecs=h264dec`
   (`tools/Set-HeliosNvk.ps1`: `NvkIcdPath` -> that `vulkan_nouveau.dll`).
3. Install (`ci/vm/README.md`, "Installing the package"), reboot.
4. Smoke test without a browser: `d3d11va_decode_test.exe test-1080.mp4
   sw-1080.md5` from a directory **without** DXVK DLLs and without the
   Vulkan shim, with `HELIOS_ICD=nvk` in the environment: the system
   `d3d11.dll` -> Helios UMD -> DXVK -> NVK. Expect the same `probe:` line
   (1 profile, NV12, configs raw=2 raw=1, DECODER_OUTPUT yes) and
   600/600 bit-exact. `HELIOS_ICD=venus` must report 0 profiles (or no video
   device with `VideoDdi`=1) and fail cleanly.
5. Same with FFmpeg's `-hwaccel d3d11va` (system DLLs).
6. Edge: `NvkAllowList` = `msedge.exe;msedgewebview2.exe`, restart Edge, open
   `edge://gpu`: "Video Decode: Hardware accelerated" and, under Video
   Acceleration Information, "Decode h264 baseline/main/high". Play an H.264
   test page (e.g. a local `<video>` with test-1080.mp4) and check
   `edge://media-internals` (decoder `D3D11VideoDecoder`, not `FFmpegVideoDecoder`/
   `VDAVideoDecoder` fallback) and that CPU use drops. Remove the
   `NvkAllowList` entry afterwards.
7. Movies & TV / MF: same with the player's executable in `NvkAllowList`.

What to look at when it fails: the UMD log (`DDI RetrieveSubObject: D3D11.1
video function table installed`, `DDI create_video_decoder`), DXVK's log
(`Helios: video decode queue family 1`, `D3D11VideoDecoder: H.264 session`),
and whether the process is on NVK at all (S3's backend line).
