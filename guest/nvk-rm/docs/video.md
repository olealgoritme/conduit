# Video decode and encode with NVK on RM

Can Windows apps in the `win11` guest decode (and encode) video in hardware
when they run on Mesa NVK-on-RM? This is the roadmap item "Video
decode/encode" in `docs/NVK-ROADMAP.md`. Short answer:

- **Vulkan Video decode works now.** NVK's own H.264 decoder runs on GB202's
  NVDEC through RM with one small backend patch (**0035**). It is bit-exact,
  in the `win11` guest and natively on the host.
- **Windows apps do not use Vulkan Video.** Browsers, Media Foundation and
  players decode through the D3D11 video API (D3D11VA/DXVA). Neither
  Helios's UMD nor DXVK implements a D3D11 video *decoder*, so nothing on
  Windows can reach the NVDEC path yet. That holds on Venus as well:
  today **no** Windows app in the guest gets hardware decode, on either ICD.
- **Encode:** NVK has no Vulkan Video encode. The RM side would be the same
  small step as for decode, but there is no encoder in NVK to drive it.

## What exists (Mesa main 70c4c018, the base of the series)

`src/nouveau/vulkan/nvk_video_session.c` and `nvk_cmd_video.c`, plus Rust in
`src/nouveau/vulkan/rust/video/decode/{mod,h264}.rs` (Collabora/Red Hat,
2023-24):

| | |
|---|---|
| codecs | H.264 decode only: baseline/main/high, 8-bit 4:2:0, progressive, up to 4096x4096, level 5.2, 17 DPB slots. No HEVC, AV1, VP9 or encode. `headers/nvidia/video/` has `nvdec_drv.h` and `nvenc_drv.h`, but only H.264 is written against them. |
| gating | `NVK_EXPERIMENTAL=video` at run time; `VIDEO_CODEC_H264DEC` at build time, so `-Dvideo-codecs=h264dec`. The default `all_free` leaves out the patent-encumbered codecs, and `build-windows.sh` passes `-Dvideo-codecs=` (none). |
| queue | One extra queue family (`VIDEO_DECODE \| SPARSE_BINDING`) on an `NVKMD_ENGINE_VDEC` context, i.e. a channel of its own on the NVDEC engine |
| hardware | Programs NVC5B0 (Turing NVDEC) methods. SET_OBJECT was hard-coded to 0xC5B0. |
| nouveau | `has_video` needs nouveau 1.4.3 ("VDEC contexts can be created") |
| RM backend before 0035 | `has_video = false`, and `nvkmd_rm_create_ctx` refused `NVKMD_ENGINE_VDEC` |

Blackwell compatibility, checked against NVIDIA's headers: GB202's NVDEC
class is **NVCFB0**. Every method NVK uses has the same offset in NVC5B0 and
NVCFB0: `SET_APPLICATION_ID` 0x200, `EXECUTE` 0x300, `SET_CONTROL_PARAMS`
0x400 through `SET_NVDEC_STATUS_OFFSET` 0x424, picture luma/chroma offsets
0x430/0x474, `H264_SET_MBHIST_BUF_OFFSET` 0x500. `nvdec_h264_pic_s` is
identical, field for field, in Mesa's `nvdec_drv.h` and NVIDIA's
`cfb0_drv.h`. So the only GB202-specific part is the channel and the class.

## Patch 0035: `nvk/rm: video decode on an NVDEC channel`

`patches-windows/0035-nvk-rm-video-decode-on-an-NVDEC-channel.patch`, on top
of the integration stack (applies after 0029 and before
`patches-windows-dxvk/`; checked with `git am` in `build-windows.sh` order).
5 files, +64/-8:

- `nvkmd_rm_pdev.c`: `cls_vdec` = the newest NVDEC class in the class list,
  from 0xc4b0 to 0xcfb0. `has_video = cls_vdec != 0`, with
  `NVK_RM_VIDEO=0` to turn it off. Without `NVK_EXPERIMENTAL=video` NVK still
  shows no video queue, so default behaviour does not change.
- `nvkmd_rm_ctx.c`: an `NVKMD_ENGINE_VDEC` context is the existing TSG +
  SYNC subcontext + GPFIFO channel, with `engineType = NV2080_ENGINE_TYPE_NVDEC0`
  (0x13) on the TSG, the channel and `NVA06F_CTRL_CMD_BIND`. The decoder
  object goes alone on the channel, allocated with
  `NV_NVDEC_ALLOCATION_PARAMETERS { size, 0, engineInstance 0 }`. RM refuses
  the allocation without these parameters (`kernel_nvdec_engdesc.c`).
  Ring, USERD, doorbell, semaphore waits/signals and the tracking release all
  stay the same: they are host (PBDMA) methods, so every runlist executes them.
- `nvrm_api.h`: `NV2080_ENGINE_TYPE_NVDEC0` and `NV_NVDEC_ALLOCATION_PARAMETERS`
  (size-asserted at 12 bytes, the size the host allowlist expects)
- `nvk_queue.c` and `rust/video/decode/mod.rs`: SET_OBJECT on the video
  queue names `pdev->info.cls_vdec`. If that is 0 it falls back to 0xC5B0.

Nothing changed in the host backend or the KMD. The backend serves the NVDEC
classes under its `video` capability (`host/backend/device/src/caps.rs`,
default on). Linux guests already use it through NVIDIA's own userspace.

## Results (RTX 5090 GB202, RM 610.57.04, 2026-10-06)

The test is FFmpeg's Vulkan hwaccel (`-init_hw_device vulkan -hwaccel vulkan`).
Each decoded frame is downloaded and its MD5 compared with libavcodec's
software decode. Clips are from `testsrc2` + libx264:

| clip | `win11` guest (KMD transport) | host, native NVK-on-RM |
|---|---|---|
| 640x480, constrained baseline, 60 frames | 60/60 bit-exact | 60/60 bit-exact |
| 640x480, high, 2 B-frames, 60 frames | 60/60 bit-exact | 60/60 bit-exact |
| 1920x1080p60, high, 3 B-frames, 4 refs, 12 Mbit/s, 600 frames | 600/600 bit-exact | 600/600 bit-exact |

Decode speed: the 1080p clip, frames left in VRAM (`-hwaccel_output_format
vulkan -f null`):

| | 600 frames | fps |
|---|---|---|
| NVK on RM, `win11` guest | 0.65 s | ~925 |
| NVK on RM, host native | 0.60 s | ~1000 |
| NVIDIA's Vulkan driver, host | 0.38 s | ~1590 |
| libavcodec, 1 CPU thread, host | 1.51 s | ~400 |

`testsrc2` is easy content. These numbers say that the engine runs at full
rate and that the guest adds little. They are not a codec benchmark. Image
compression (patch 0028) was on and did not disturb the decode.

Probe (`tests/vk_video_probe.c`: queue families with their codec operations,
video extensions, H.264 capabilities, decode formats):

| | queue families | video extensions |
|---|---|---|
| before 0035 (host and `win11`, with or without `NVK_EXPERIMENTAL=video`) | 0: 0xf | none |
| 0035, `NVK_EXPERIMENTAL=video` | 0: 0xf, 1: 0x28 (DECODE, H.264) | `KHR_video_queue`, `_decode_queue`, `_decode_h264`, `_maintenance1/2` |
| NVIDIA's driver on the host, for reference | 6, decode family 0xf (H.264/H.265/AV1/VP9) and an encode family | 14, decode + encode |

### Running it

Host (Linux):

```sh
meson configure build-rm -Dvideo-codecs=h264dec   # then guest/nvk-rm/build.sh as usual
NVK_RM=1 NVK_EXPERIMENTAL=video NVK_RMCLIENT_LIB=.../librmclient.so \
VK_DRIVER_FILES=.../nouveau_devenv_icd.x86_64.json \
  ffmpeg -init_hw_device vulkan=vk:0 -hwaccel vulkan -hwaccel_device vk \
         -i clip.mp4 -pix_fmt yuv420p -f framemd5 nvk.md5
```

On Linux, FFmpeg fails first with `Failed to create semaphore:
VK_ERROR_INVALID_EXTERNAL_HANDLE`. The RM backend advertises
`VK_KHR_external_semaphore_fd` whenever it has dma-bufs, but its syncs cannot
be exported. That is a separate gap in the Linux RM backend, and it hits any
app that makes exportable semaphores (mpv, browsers). For these runs the
extension was hidden in a local build. Windows does not advertise it.

`win11` (`windows/vk-video-decode.ps1`): build with
`MESON_ARGS=-Dvideo-codecs=h264dec guest/nvk-rm/build-windows.sh`. Put these
in one directory: `vulkan_nouveau.dll`, `librmclient.dll`, a `vulkan-1.dll`
built from `windows/vulkan_shim.c` (FFmpeg loads `vulkan-1.dll` from its own
directory, so it sees only NVK even in an elevated session), and a shared
Windows FFmpeg with `--enable-vulkan` (BtbN's win64-lgpl-shared was used).
Then run `vk-video-decode.ps1 -Clip high`. `vk_video_probe.exe` imports the
whole loader API, so it lives in a directory without the shim and uses
`VK_DIRECT_DRIVER`.

## Options for Windows apps

What a Windows app reaches for decode: Chromium/Edge use D3D11VA
(`ID3D11VideoDecoder`, DXVA GUIDs such as `DXVA2_ModeH264_VLD_NoFGT`). Media
Foundation (Movies & TV, Edge's MF path, most players) uses DXVA through a
D3D11 device manager. Old players use DXVA2 (D3D9). None of them uses Vulkan
Video on Windows. Today:

- `guest/windows/umd/src/forward/format_caps.rs`: "Helios does not implement
  the D3D11 video DDI". The UMD clears the `VIDEO_*` format support bits.
- DXVK (the Helios fork, `src/d3d11/d3d11_video.cpp`) implements the video
  *processor* only. `GetVideoDecoderProfileCount` returns 0, and
  `CreateVideoDecoder`, `DecoderBeginFrame` and `SubmitDecoderBuffers` are
  stubs. Encode is not in the D3D11 video API at all (that is MF/NVENC SDK).
- So apps fall back to CPU decode (Chromium: FFmpeg/libvpx/dav1d), on Venus and
  on NVK alike.

| option | what it takes | size | verdict |
|---|---|---|---|
| **(a) Vulkan Video in NVK on RM** | done for H.264 decode (0035). More codecs and encode are NVK work, not RM work: an NVK HEVC/AV1/VP9 decoder against `nvdec_drv.h` (H.264 took ~800 lines of Rust plus session code). Encode needs a whole NVK encoder (NVENC class NVCFB7, `nvenc_drv.h`, rate control, Vulkan encode API). On the RM side encode is the same ~50 lines as 0035: `NV2080_ENGINE_TYPE_NVENC0` (0x1b), `NV_MSENC_ALLOCATION_PARAMETERS`. | done; per codec ~1-2 weeks upstream-quality; encode ~1-2 months | only useful once something on Windows calls Vulkan Video |
| **(b) DXVK `ID3D11VideoDecoder` on Vulkan Video** | in DXVK: decoder profiles/configs (H.264 VLD, `ConfigBitstreamRaw` 1/2), DXVA `DXVA_PicParams_H264` / `DXVA_Qmatrix_H264` / slice control to Vulkan's StdVideo H.264 SPS/PPS/picture info, DPB slot bookkeeping (DXVA's `RefFrameList` indices to Vulkan reference slots), the bitstream buffer, decoder output views onto NV12 texture arrays (image usage DECODE_DST\|DPB plus SAMPLED for the video processor and the app), a video queue and its syncs in DXVK's submission thread. In Helios's UMD: forward the D3D11 video DDI (CreateDecodeDevice, DecoderBeginFrame/SubmitBuffers/EndFrame, CheckVideoDecoderFormat...) to DXVK, and stop masking the video format bits on NVK. Validate with Chromium/Edge (`chrome://gpu` "Video Decode: Hardware accelerated") and MF. | H.264 only: ~3-5 weeks for DXVK + ~1 week for the UMD DDI. HEVC/AV1 then need (a) per codec first. | **the only route that gives browsers/MF hardware decode**. Large, and NVK video is still experimental upstream |
| **(c) status quo: video apps on Venus (S3 deny-list)** | nothing | 0 | correct for now: no hardware decode either way, and Venus is the proven path for D3D11 video *processing* and browser compositing. It does not cost hardware decode, because there is none to lose. |
| (d) a D3D11VA path straight to NVDEC through RM (own DXVA implementation in the UMD driving NVDEC channels and `nvdec_drv.h` without Vulkan) | everything NVK's decoder does (picture setup, DPB, buffers, per codec), plus sync and memory management outside Vulkan, plus the D3D11 DDI | months | no: duplicates NVK and (b) |
| (e) a Media Foundation decoder MFT on Vulkan Video | an MFT registered as a hardware decoder, D3D11 interop for its output samples | ~3-4 weeks | covers MF apps, not Chromium. Its D3D11 interop needs (b)'s plumbing anyway |
| (f) NVIDIA's NVDEC userspace (nvcuvid/NVENC SDK) in Windows | NVIDIA's Windows UMD stack, which needs NVIDIA's KMD | n/a | not possible with the Helios KMD (Linux guests do it because NVIDIA's Linux userspace talks RM directly) |

## Recommendation

1. Merge 0035 into the integration stack. It is small, off unless
   `NVK_EXPERIMENTAL=video` is set, and it is the foundation for anything
   later. Leave `-Dvideo-codecs` empty in default builds until someone decides
   on H.264 patents. Turn it on for experiments.
2. Keep browsers and video apps on the Venus deny-list (S3). They lose
   nothing that NVK could give them today.
3. If hardware decode in browsers matters, (b) is the path. Start with
   H.264, a Helios-DXVK decoder on Vulkan Video, measured with Chromium on
   NVK. It needs the browser itself to run well on NVK first (it is on the
   deny-list for other reasons too). Before then, CPU decode on the 7950X
   handles 1080p/4K H.264 and AV1 (dav1d) comfortably.
4. Separately, for Linux guests on NVK-on-RM: fix the
   `VK_KHR_external_semaphore_fd` advertisement (only advertise it when the
   sync type can export), or FFmpeg/mpv/browsers cannot use the device at all.
