/* SPDX-License-Identifier: MIT */
/*
 * d3d11va_decode_test: H.264 decode through the D3D11 video API (D3D11VA),
 * checked frame by frame against libavcodec's software decode.
 *
 * libavcodec parses the bitstream and drives the D3D11 video decoder
 * (ID3D11VideoDecoder, DXVA picture parameters / slice control / bitstream
 * buffers), the same way Chromium's and Media Foundation's decoders do. The
 * test owns the D3D11 device, so whichever d3d11.dll sits next to the exe
 * (DXVK, for the Helios decoder on Vulkan Video) is the one under test.
 *
 * Per frame it reads the decoded NV12 surface back, converts it to planar
 * yuv420p and hashes it like `ffmpeg -f framemd5` does, and runs the frame
 * through the D3D11 video processor (NV12 -> BGRA, what a browser does to
 * show it) and checks the result against a CPU BT.601 conversion of the
 * same frame.
 *
 *   d3d11va_decode_test.exe <clip.mp4> [sw.md5] [-bench] [-noblt] [-sw]
 *
 *   sw.md5   framemd5 output of a software decode (ffmpeg -i clip -f framemd5)
 *   -bench   decode only, frames stay on the GPU; prints fps
 *   -noblt   skip the video processor check
 *   -sw      decode in software instead (sanity check of the harness)
 *
 * Output: "frames N/M, bit-exact K, blt ok B" and exit code 0 if every frame
 * matched.
 *
 * Build (MinGW, against a shared FFmpeg build with include/ and lib/):
 *   x86_64-w64-mingw32-gcc -O2 -o d3d11va_decode_test.exe d3d11va_decode_test.c \
 *       -I<ffmpeg>/include -L<ffmpeg>/lib -lavformat -lavcodec -lavutil \
 *       -ld3d11 -ldxgi -luuid -lole32
 */
#define COBJMACROS
#define INITGUID
#include <windows.h>
#include <d3d11.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/md5.h>
#include <libavutil/imgutils.h>

static const GUID H264_VLD_NOFGT =
   { 0x1b81be68, 0xa0c7, 0x11d3, { 0xb9, 0x84, 0x00, 0xc0, 0x4f, 0x2e, 0x73, 0xc5 } };

static double now_s(void)
{
   LARGE_INTEGER f, c;
   QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&c);
   return (double)c.QuadPart / (double)f.QuadPart;
}

static enum AVPixelFormat get_hw_format(AVCodecContext *ctx, const enum AVPixelFormat *fmts)
{
   for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
      if (*p == AV_PIX_FMT_D3D11)
         return *p;
   }
   fprintf(stderr, "d3d11va not offered by libavcodec\n");
   return AV_PIX_FMT_NONE;
}

/* What the video processor check needs, created on the first frame */
struct blt {
   ID3D11VideoDevice *vdev;
   ID3D11VideoContext *vctx;
   ID3D11VideoProcessorEnumerator *venum;
   ID3D11VideoProcessor *vproc;
   ID3D11Texture2D *rgb, *rgb_staging;
   ID3D11VideoProcessorOutputView *ov;
   int w, h;
   int verbose, reports;
};

static int blt_init(struct blt *b, ID3D11Device *dev, ID3D11DeviceContext *ctx, int w, int h)
{
   HRESULT hr;
   b->w = w;
   b->h = h;
   if (FAILED(ID3D11Device_QueryInterface(dev, &IID_ID3D11VideoDevice, (void **)&b->vdev)) ||
       FAILED(ID3D11DeviceContext_QueryInterface(ctx, &IID_ID3D11VideoContext, (void **)&b->vctx)))
      return -1;

   D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd = { 0 };
   cd.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
   cd.InputWidth = w;
   cd.InputHeight = h;
   cd.OutputWidth = w;
   cd.OutputHeight = h;
   cd.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
   if (FAILED(hr = ID3D11VideoDevice_CreateVideoProcessorEnumerator(b->vdev, &cd, &b->venum)) ||
       FAILED(hr = ID3D11VideoDevice_CreateVideoProcessor(b->vdev, b->venum, 0, &b->vproc))) {
      fprintf(stderr, "video processor: %08lx\n", hr);
      return -1;
   }

   D3D11_TEXTURE2D_DESC td = { 0 };
   td.Width = w;
   td.Height = h;
   td.MipLevels = 1;
   td.ArraySize = 1;
   td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
   td.SampleDesc.Count = 1;
   td.Usage = D3D11_USAGE_DEFAULT;
   td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
   if (FAILED(ID3D11Device_CreateTexture2D(dev, &td, NULL, &b->rgb)))
      return -1;
   td.Usage = D3D11_USAGE_STAGING;
   td.BindFlags = 0;
   td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
   if (FAILED(ID3D11Device_CreateTexture2D(dev, &td, NULL, &b->rgb_staging)))
      return -1;

   D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd = { 0 };
   ovd.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
   if (FAILED(hr = ID3D11VideoDevice_CreateVideoProcessorOutputView(b->vdev, (ID3D11Resource *)b->rgb,
                                                                    b->venum, &ovd, &b->ov))) {
      fprintf(stderr, "output view: %08lx\n", hr);
      return -1;
   }

   /* Studio-range BT.601 in, full-range RGB out */
   D3D11_VIDEO_PROCESSOR_COLOR_SPACE in_cs = { 0 }, out_cs = { 0 };
   in_cs.YCbCr_Matrix = 0;
   in_cs.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235;
   out_cs.RGB_Range = 0;
   ID3D11VideoContext_VideoProcessorSetStreamColorSpace(b->vctx, b->vproc, 0, &in_cs);
   ID3D11VideoContext_VideoProcessorSetOutputColorSpace(b->vctx, b->vproc, &out_cs);

   /* Decoder surfaces are macroblock aligned (1920x1088 for 1080p): take only
    * the visible picture, as players do, so nothing gets scaled. */
   RECT r = { 0, 0, w, h };
   ID3D11VideoContext_VideoProcessorSetStreamSourceRect(b->vctx, b->vproc, 0, TRUE, &r);
   ID3D11VideoContext_VideoProcessorSetStreamDestRect(b->vctx, b->vproc, 0, TRUE, &r);
   return 0;
}

static int clamp255(double v)
{
   return v < 0 ? 0 : v > 255 ? 255 : (int)(v + 0.5);
}

/* Blit one decoded surface to BGRA and compare a grid of pixels with a CPU
 * BT.601 conversion of the downloaded frame. Returns 1 if within tolerance. */
static int blt_check(struct blt *b, ID3D11Device *dev, ID3D11DeviceContext *ctx,
                     ID3D11Texture2D *tex, UINT slice, const AVFrame *yuv)
{
   HRESULT hr;
   ID3D11VideoProcessorInputView *iv = NULL;
   D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd = { 0 };
   ivd.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
   ivd.Texture2D.ArraySlice = slice;
   if (FAILED(hr = ID3D11VideoDevice_CreateVideoProcessorInputView(b->vdev, (ID3D11Resource *)tex,
                                                                   b->venum, &ivd, &iv))) {
      fprintf(stderr, "input view: %08lx\n", hr);
      return 0;
   }

   D3D11_VIDEO_PROCESSOR_STREAM st = { 0 };
   st.Enable = TRUE;
   st.pInputSurface = iv;
   hr = ID3D11VideoContext_VideoProcessorBlt(b->vctx, b->vproc, b->ov, 0, 1, &st);
   ID3D11VideoProcessorInputView_Release(iv);
   if (FAILED(hr)) {
      fprintf(stderr, "VideoProcessorBlt: %08lx\n", hr);
      return 0;
   }

   ID3D11DeviceContext_CopyResource(ctx, (ID3D11Resource *)b->rgb_staging, (ID3D11Resource *)b->rgb);
   D3D11_MAPPED_SUBRESOURCE map;
   if (FAILED(ID3D11DeviceContext_Map(ctx, (ID3D11Resource *)b->rgb_staging, 0, D3D11_MAP_READ, 0, &map)))
      return 0;

   /* Compare with CPU conversions for both BT.601 and BT.709 studio-range
    * matrices: which one the processor applies for the default color space
    * is the processor's business, a broken input surface matches neither. */
   static const double mat[2][4] = {
      { 1.596, 0.392, 0.813, 2.017 },   /* BT.601 */
      { 1.793, 0.213, 0.533, 2.112 },   /* BT.709 */
   };
   int ok = 1, worst = 0, worst_m[2] = { 0, 0 }, points = 0;
   for (int gy = 1; gy < 8; gy++) {
      for (int gx = 1; gx < 8; gx++) {
         int x = b->w * gx / 8, y = b->h * gy / 8;
         /* Average a 2x2 block so chroma siting does not matter */
         x &= ~1;
         y &= ~1;
         /* Only flat areas: edges depend on the processor's chroma filter */
         int flat = 1;
         for (int j = -2; j < 4 && flat; j += 2) {
            for (int i = -2; i < 4 && flat; i += 2) {
               const uint8_t *py = yuv->data[0] + (y + j) * yuv->linesize[0] + x + i;
               const uint8_t *pu = yuv->data[1] + ((y + j) / 2) * yuv->linesize[1] + (x + i) / 2;
               const uint8_t *pv = yuv->data[2] + ((y + j) / 2) * yuv->linesize[2] + (x + i) / 2;
               const uint8_t *cy = yuv->data[0] + y * yuv->linesize[0] + x;
               const uint8_t *cu = yuv->data[1] + (y / 2) * yuv->linesize[1] + x / 2;
               const uint8_t *cv = yuv->data[2] + (y / 2) * yuv->linesize[2] + x / 2;
               if (abs(*py - *cy) > 3 || abs(*pu - *cu) > 3 || abs(*pv - *cv) > 3)
                  flat = 0;
            }
         }
         if (!flat)
            continue;
         points++;
         double Y = 0;
         for (int j = 0; j < 2; j++)
            for (int i = 0; i < 2; i++)
               Y += yuv->data[0][(y + j) * yuv->linesize[0] + x + i];
         Y /= 4.0;
         double U = yuv->data[1][(y / 2) * yuv->linesize[1] + x / 2] - 128.0;
         double V = yuv->data[2][(y / 2) * yuv->linesize[2] + x / 2] - 128.0;
         double C = (Y - 16.0) * 1.164;
         int R = 0, G = 0, B = 0;
         for (int j = 0; j < 2; j++) {
            for (int i = 0; i < 2; i++) {
               const uint8_t *p = (const uint8_t *)map.pData + (y + j) * map.RowPitch + (x + i) * 4;
               B += p[0];
               G += p[1];
               R += p[2];
            }
         }
         R /= 4; G /= 4; B /= 4;
         if (getenv("VDEC_DEBUG") && !b->reports && gy == 4)
            printf("  px %d,%d: YUV %.0f %.0f %.0f -> RGB %d %d %d\n", x, y, Y, U + 128, V + 128, R, G, B);
         for (int m = 0; m < 2; m++) {
            int r = clamp255(C + mat[m][0] * V);
            int g = clamp255(C - mat[m][1] * U - mat[m][2] * V);
            int bl = clamp255(C + mat[m][3] * U);
            int d = abs(R - r);
            if (abs(G - g) > d) d = abs(G - g);
            if (abs(B - bl) > d) d = abs(B - bl);
            if (d > worst_m[m]) worst_m[m] = d;
         }
      }
   }
   int m = worst_m[1] < worst_m[0];
   worst = worst_m[m];
   ID3D11DeviceContext_Unmap(ctx, (ID3D11Resource *)b->rgb_staging, 0);
   /* Smooth testsrc2 content: a correct conversion lands within a few levels */
   if (worst > 12 || points < 8)
      ok = 0;
   if (!ok || !b->reports) {
      if (b->reports++ < 3)
         printf("blt: worst channel difference %d against CPU %s over %d flat points (%s)\n", worst,
                m ? "BT.709" : "BT.601", points, ok ? "ok" : "FAIL");
   }
   return ok;
}

static void probe_video_device(ID3D11Device *dev)
{
   ID3D11VideoDevice *vdev = NULL;
   if (FAILED(ID3D11Device_QueryInterface(dev, &IID_ID3D11VideoDevice, (void **)&vdev))) {
      printf("probe: no ID3D11VideoDevice\n");
      return;
   }
   UINT n = ID3D11VideoDevice_GetVideoDecoderProfileCount(vdev);
   int h264 = 0;
   for (UINT i = 0; i < n; i++) {
      GUID g;
      if (SUCCEEDED(ID3D11VideoDevice_GetVideoDecoderProfile(vdev, i, &g)) && IsEqualGUID(&g, &H264_VLD_NOFGT))
         h264 = 1;
   }
   BOOL nv12 = FALSE;
   ID3D11VideoDevice_CheckVideoDecoderFormat(vdev, &H264_VLD_NOFGT, DXGI_FORMAT_NV12, &nv12);
   D3D11_VIDEO_DECODER_DESC dd = { H264_VLD_NOFGT, 1920, 1080, DXGI_FORMAT_NV12 };
   UINT cfgs = 0;
   ID3D11VideoDevice_GetVideoDecoderConfigCount(vdev, &dd, &cfgs);
   printf("probe: %u decoder profiles, H264_VLD_NoFGT %s, NV12 %s, %u configs (1080p):", n,
          h264 ? "yes" : "no", nv12 ? "yes" : "no", cfgs);
   for (UINT i = 0; i < cfgs; i++) {
      D3D11_VIDEO_DECODER_CONFIG c;
      if (SUCCEEDED(ID3D11VideoDevice_GetVideoDecoderConfig(vdev, &dd, i, &c)))
         printf(" raw=%u", c.ConfigBitstreamRaw);
   }
   UINT fs = 0;
   ID3D11Device_CheckFormatSupport(dev, DXGI_FORMAT_NV12, &fs);
   printf(", NV12 DECODER_OUTPUT %s\n", (fs & D3D11_FORMAT_SUPPORT_DECODER_OUTPUT) ? "yes" : "no");
   ID3D11VideoDevice_Release(vdev);
}

int main(int argc, char **argv)
{
   const char *clip = NULL, *ref = NULL;
   int bench = 0, noblt = 0, sw = 0;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "-bench")) bench = 1;
      else if (!strcmp(argv[i], "-noblt")) noblt = 1;
      else if (!strcmp(argv[i], "-sw")) sw = 1;
      else if (!clip) clip = argv[i];
      else ref = argv[i];
   }
   if (!clip) {
      fprintf(stderr, "usage: %s <clip.mp4> [sw.md5] [-bench] [-noblt] [-sw]\n", argv[0]);
      return 2;
   }

   /* Reference hashes, one per frame, last column of framemd5 lines */
   char (*refs)[33] = calloc(100000, 33);
   int nrefs = 0;
   if (ref) {
      FILE *f = fopen(ref, "r");
      char line[512];
      while (f && fgets(line, sizeof(line), f) && nrefs < 100000) {
         if (line[0] == '#')
            continue;
         char *h = strrchr(line, ',');
         if (!h)
            continue;
         h++;
         while (*h == ' ') h++;
         memcpy(refs[nrefs], h, 32);
         refs[nrefs][32] = 0;
         nrefs++;
      }
      if (f) fclose(f);
   }

   ID3D11Device *dev = NULL;
   ID3D11DeviceContext *ctx = NULL;
   D3D_FEATURE_LEVEL fl;
   HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL,
                                  D3D11_CREATE_DEVICE_VIDEO_SUPPORT, NULL, 0, D3D11_SDK_VERSION,
                                  &dev, &fl, &ctx);
   if (FAILED(hr)) {
      fprintf(stderr, "D3D11CreateDevice: %08lx\n", hr);
      return 1;
   }
   {
      IDXGIDevice *dxgi = NULL;
      IDXGIAdapter *ad = NULL;
      DXGI_ADAPTER_DESC desc;
      if (SUCCEEDED(ID3D11Device_QueryInterface(dev, &IID_IDXGIDevice, (void **)&dxgi)) &&
          SUCCEEDED(IDXGIDevice_GetAdapter(dxgi, &ad)) && SUCCEEDED(IDXGIAdapter_GetDesc(ad, &desc)))
         printf("adapter: %ls (vendor %04x)\n", desc.Description, desc.VendorId);
      if (ad) IDXGIAdapter_Release(ad);
      if (dxgi) IDXGIDevice_Release(dxgi);
   }
   probe_video_device(dev);

   AVFormatContext *fmt = NULL;
   if (avformat_open_input(&fmt, clip, NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) {
      fprintf(stderr, "cannot open %s\n", clip);
      return 1;
   }
   const AVCodec *codec = NULL;
   int vs = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, &codec, 0);
   if (vs < 0) {
      fprintf(stderr, "no video stream\n");
      return 1;
   }
   AVCodecContext *cc = avcodec_alloc_context3(codec);
   avcodec_parameters_to_context(cc, fmt->streams[vs]->codecpar);
   cc->thread_count = 1;

   if (!sw) {
      AVBufferRef *hwdev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
      AVHWDeviceContext *hd = (AVHWDeviceContext *)hwdev->data;
      AVD3D11VADeviceContext *d3d = hd->hwctx;
      d3d->device = dev;
      ID3D11Device_AddRef(dev);
      if (av_hwdevice_ctx_init(hwdev) < 0) {
         fprintf(stderr, "av_hwdevice_ctx_init failed\n");
         return 1;
      }
      cc->hw_device_ctx = hwdev;
      cc->get_format = get_hw_format;
   }
   if (avcodec_open2(cc, codec, NULL) < 0) {
      fprintf(stderr, "avcodec_open2 failed\n");
      return 1;
   }

   AVPacket *pkt = av_packet_alloc();
   AVFrame *frame = av_frame_alloc(), *nv12 = av_frame_alloc(), *yuv = av_frame_alloc();
   struct AVMD5 *md5 = av_md5_alloc();
   struct blt b = { 0 };
   int nframes = 0, exact = 0, bltok = 0, bltrun = 0, mismatch_print = 0, blt_failed = 0;
   uint8_t *buf = NULL;
   double t0 = now_s();
   int eof = 0;

   for (;;) {
      int r = eof ? 0 : av_read_frame(fmt, pkt);
      if (r < 0) {
         eof = 1;
         avcodec_send_packet(cc, NULL);
      } else if (!eof) {
         if (pkt->stream_index != vs) {
            av_packet_unref(pkt);
            continue;
         }
         r = avcodec_send_packet(cc, pkt);
         av_packet_unref(pkt);
         if (r < 0 && r != AVERROR(EAGAIN)) {
            fprintf(stderr, "send_packet: %d\n", r);
            break;
         }
      }

      int got_eof = 0;
      while (1) {
         r = avcodec_receive_frame(cc, frame);
         if (r == AVERROR(EAGAIN))
            break;
         if (r == AVERROR_EOF) {
            got_eof = 1;
            break;
         }
         if (r < 0) {
            fprintf(stderr, "receive_frame: %d\n", r);
            got_eof = 1;
            break;
         }

         if (bench) {
            nframes++;
            av_frame_unref(frame);
            continue;
         }

         const AVFrame *cpu = frame;
         if (frame->format == AV_PIX_FMT_D3D11) {
            av_frame_unref(nv12);
            if (av_hwframe_transfer_data(nv12, frame, 0) < 0) {
               fprintf(stderr, "transfer failed\n");
               got_eof = 1;
               break;
            }
            cpu = nv12;
         }

         /* to planar yuv420p, tightly packed, like rawvideo in framemd5 */
         int w = frame->width, h = frame->height;
         if (!buf) {
            buf = av_malloc(av_image_get_buffer_size(AV_PIX_FMT_YUV420P, w, h, 1));
            yuv->format = AV_PIX_FMT_YUV420P;
            yuv->width = w;
            yuv->height = h;
            av_frame_get_buffer(yuv, 32);
         }
         for (int y = 0; y < h; y++)
            memcpy(yuv->data[0] + y * yuv->linesize[0], cpu->data[0] + y * cpu->linesize[0], w);
         if (cpu->format == AV_PIX_FMT_NV12) {
            for (int y = 0; y < h / 2; y++) {
               const uint8_t *s = cpu->data[1] + y * cpu->linesize[1];
               for (int x = 0; x < w / 2; x++) {
                  yuv->data[1][y * yuv->linesize[1] + x] = s[2 * x];
                  yuv->data[2][y * yuv->linesize[2] + x] = s[2 * x + 1];
               }
            }
         } else {
            for (int y = 0; y < h / 2; y++) {
               memcpy(yuv->data[1] + y * yuv->linesize[1], cpu->data[1] + y * cpu->linesize[1], w / 2);
               memcpy(yuv->data[2] + y * yuv->linesize[2], cpu->data[2] + y * cpu->linesize[2], w / 2);
            }
         }
         int size = av_image_copy_to_buffer(buf, av_image_get_buffer_size(AV_PIX_FMT_YUV420P, w, h, 1),
                                            (const uint8_t *const *)yuv->data, yuv->linesize,
                                            AV_PIX_FMT_YUV420P, w, h, 1);
         uint8_t digest[16];
         char hex[33];
         av_md5_init(md5);
         av_md5_update(md5, buf, size);
         av_md5_final(md5, digest);
         for (int i = 0; i < 16; i++)
            sprintf(hex + 2 * i, "%02x", digest[i]);
         if (nframes < nrefs && !strcmp(hex, refs[nframes]))
            exact++;
         else if (nrefs && mismatch_print++ < 5)
            printf("frame %d: %s, expected %s\n", nframes, hex, nframes < nrefs ? refs[nframes] : "-");

         if (!noblt && frame->format == AV_PIX_FMT_D3D11 && !blt_failed) {
            if (!b.vproc && blt_init(&b, dev, ctx, w, h) < 0) {
               printf("video processor setup failed\n");
               blt_failed = 1;
            } else {
               bltrun++;
               bltok += blt_check(&b, dev, ctx, (ID3D11Texture2D *)frame->data[0],
                                  (UINT)(intptr_t)frame->data[1], yuv);
            }
         }
         nframes++;
         av_frame_unref(frame);
      }
      if (got_eof)
         break;
   }
   double dt = now_s() - t0;

   if (bench)
      printf("bench: %d frames in %.3f s, %.1f fps\n", nframes, dt, nframes / dt);
   printf("frames %d/%d, bit-exact %d, blt ok %d/%d, %.1f fps\n", nframes, nrefs, exact, bltok, bltrun,
          nframes / dt);

   int pass = bench ? nframes > 0 : (nrefs > 0 && nframes == nrefs && exact == nrefs && bltok == bltrun);
   return pass ? 0 : 1;
}
