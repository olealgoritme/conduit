// chromium_like: the frame path a Chromium/Edge GPU process takes for video,
// reduced to what the driver sees, with every wait timed.
//
//   chromium_like.exe [SECONDS=10 [VIDEO=nv12|yuy2|bgra|none [HANDOFF=keyed|fence|none [FPS=60]]]]
//
// The parent is the GPU process: a window, a DirectComposition tree with a
// BGRA "UI" swap chain (the page) and a "video" swap chain (VIDEO format; a
// YUV one carries DXGI_SWAP_CHAIN_FLAG_YUV_VIDEO, as Chromium's overlay
// swap chains do), both flip-sequential composition swap chains presented
// with Present(1, 0) every frame. Each frame the video swap chain's back
// buffer is filled by ID3D11VideoContext::VideoProcessorBlt from the current
// "decoded" frame.
//
// The decoded frame comes from a child process (the renderer / decoder) at
// FPS, through a shared texture:
//   keyed  NT handle + keyed mutex (child: AcquireSync(0), write, ReleaseSync(1);
//          parent: AcquireSync(1), use, ReleaseSync(0))
//   fence  NT handle without a mutex + a shared ID3D11Fence (child: write,
//          Signal(n); parent: ID3D11DeviceContext4::Wait(n), use)
//   none   the parent writes its own frame
//
// Per second and at the end it prints frames, presents, and the slowest
// acquire / fence wait / blit / present in milliseconds, and counts frames
// over 100 ms. A capped wait that times out every frame shows up as ~cap ms
// in exactly one column.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static chromium_like.cpp
//   -o chromium_like.exe -ld3d11 -ldxgi -ldcomp -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_4.h>
#include <dcomp.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>

static const wchar_t *kTexName = L"Local\\HeliosChromiumLikeFrame";
static const UINT kW = 1280, kH = 720;

// The value the child last signalled on the shared fence, as Chromium passes
// it over IPC with the frame (the parent waits for it on the GPU).
static volatile LONG64 *signalled_value() {
  static volatile LONG64 *p = [] {
    HANDLE m = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0, 64, L"Local\\HeliosChromiumLikeSeq");
    return m ? static_cast<volatile LONG64 *>(MapViewOfFile(m, FILE_MAP_ALL_ACCESS, 0, 0, 64)) : nullptr;
  }();
  return p;
}

// A file whose existence asks both processes to finish now, from outside the
// desktop session (a harness must not force-kill an NVK app that may own the
// screen through scanout): chromium_like.stop next to the executable.
static bool stop_requested() {
  static char path[MAX_PATH] = "";
  if (!path[0]) {
    GetModuleFileNameA(nullptr, path, sizeof(path));
    char *slash = std::strrchr(path, '\\');
    std::snprintf(slash ? slash + 1 : path, sizeof(path) - size_t((slash ? slash + 1 : path) - path),
                  "chromium_like.stop");
  }
  return GetFileAttributesA(path) != INVALID_FILE_ATTRIBUTES;
}

static double now_ms() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1000.0 * double(t.QuadPart) / double(f.QuadPart);
}

#define CHECK(what, expr)                                                                          \
  do {                                                                                             \
    HRESULT hr_ = (expr);                                                                          \
    if (FAILED(hr_)) {                                                                             \
      std::printf("[FAIL] %s: 0x%08lx\n", what, static_cast<unsigned long>(hr_));                 \
      return 1;                                                                                    \
    }                                                                                              \
  } while (0)

template <class T> static void release(T *&p) {
  if (p) p->Release();
  p = nullptr;
}

static bool make_device(ID3D11Device5 **dev, ID3D11DeviceContext4 **ctx) {
  IDXGIFactory1 *f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void **>(&f)))) return false;
  IDXGIAdapter1 *a = nullptr;
  f->EnumAdapters1(0, &a);
  f->Release();
  ID3D11Device *d = nullptr;
  ID3D11DeviceContext *c = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1;
  HRESULT hr = D3D11CreateDevice(a, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                 D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, &fl, 1,
                                 D3D11_SDK_VERSION, &d, nullptr, &c);
  if (a) a->Release();
  if (FAILED(hr)) {
    std::printf("[FAIL] D3D11CreateDevice: 0x%08lx\n", static_cast<unsigned long>(hr));
    return false;
  }
  bool ok = SUCCEEDED(d->QueryInterface(__uuidof(ID3D11Device5), reinterpret_cast<void **>(dev))) &&
            SUCCEEDED(c->QueryInterface(__uuidof(ID3D11DeviceContext4), reinterpret_cast<void **>(ctx)));
  d->Release();
  c->Release();
  return ok;
}

static void fill(ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, UINT frame) {
  static UINT32 row[kW];
  const UINT32 color = 0xff000000u | ((frame * 37u) & 0xff) << 16 | ((frame * 11u) & 0xff) << 8 | (frame & 0xff);
  for (UINT x = 0; x < kW; x++) row[x] = (x / 64 + frame) & 1 ? color : ~color | 0xff000000u;
  // A band of rows per frame keeps it cheap but changes the picture.
  D3D11_BOX box = {0, (frame * 8) % kH, 0, kW, (frame * 8) % kH + 8, 1};
  static UINT32 band[kW * 8];
  for (UINT y = 0; y < 8; y++) std::memcpy(band + y * kW, row, sizeof(row));
  ctx->UpdateSubresource(tex, 0, &box, band, kW * 4, 0);
}

// ---- child: the renderer / decoder ----------------------------------------

static int child(const char *handoff, UINT fps, double seconds, DWORD parent_pid, unsigned long long fence_handle) {
  ID3D11Device5 *dev = nullptr;
  ID3D11DeviceContext4 *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 1;
  ID3D11Texture2D *tex = nullptr;
  CHECK("child: open the shared frame",
        dev->OpenSharedResourceByName(kTexName, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                      __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&tex)));
  const bool keyed = !std::strcmp(handoff, "keyed");
  IDXGIKeyedMutex *km = nullptr;
  ID3D11Fence *fence = nullptr;
  if (keyed) {
    CHECK("child: keyed mutex", tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km)));
  } else {
    HANDLE parent = OpenProcess(PROCESS_DUP_HANDLE, FALSE, parent_pid);
    HANDLE mine = nullptr;
    if (!parent || !DuplicateHandle(parent, reinterpret_cast<HANDLE>(fence_handle), GetCurrentProcess(), &mine, 0,
                                    FALSE, DUPLICATE_SAME_ACCESS)) {
      std::printf("[FAIL] child: duplicate the fence handle: %lu\n", GetLastError());
      return 1;
    }
    CloseHandle(parent);
    CHECK("child: OpenSharedFence", dev->OpenSharedFence(mine, __uuidof(ID3D11Fence), reinterpret_cast<void **>(&fence)));
    CloseHandle(mine);
  }
  const double period = 1000.0 / fps, t0 = now_ms();
  double worst_acquire = 0;
  UINT frame = 0, acquire_timeouts = 0;
  while (now_ms() - t0 < seconds * 1000.0 && !stop_requested()) {
    frame++;
    if (keyed) {
      const double a = now_ms();
      const HRESULT hr = km->AcquireSync(0, 5000);
      const double w = now_ms() - a;
      if (w > worst_acquire) worst_acquire = w;
      if (hr != S_OK) {
        acquire_timeouts++;
        continue;
      }
    }
    fill(ctx, tex, frame);
    if (keyed) {
      ctx->Flush();
      km->ReleaseSync(1);
    } else {
      ctx->Signal(fence, frame);
      ctx->Flush();
      if (signalled_value()) InterlockedExchange64(signalled_value(), frame);
    }
    const double next = t0 + frame * period;
    const double left = next - now_ms();
    if (left > 1.0) Sleep(DWORD(left));
  }
  std::printf("       child: %u frames at %u Hz (%s), worst AcquireSync(0) %.1f ms, %u timeouts\n", frame, fps,
              handoff, worst_acquire, acquire_timeouts);
  release(km);
  release(fence);
  release(tex);
  release(ctx);
  release(dev);
  return 0;
}

// ---- parent: the GPU process ----------------------------------------------

static bool g_closed = false;
static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_CLOSE) {
    g_closed = true;
    return 0;
  }
  return DefWindowProcW(h, m, w, l);
}

struct Stats {
  UINT frames = 0, slow = 0, acquire_timeouts = 0;
  double acquire = 0, wait = 0, blit = 0, ui = 0, video = 0, frame = 0;
  void take(double a, double wt, double b, double u, double v, double f) {
    frames++;
    if (a > acquire) acquire = a;
    if (wt > wait) wait = wt;
    if (b > blit) blit = b;
    if (u > ui) ui = u;
    if (v > video) video = v;
    if (f > frame) frame = f;
    if (f > 100.0) slow++;
  }
  void print(const char *when) const {
    std::printf("       %s: %u frames, %u over 100 ms; worst ms: acquire %.1f, fence wait %.1f, blit %.1f, "
                "UI present %.1f, video present %.1f, frame %.1f; %u acquire timeouts\n",
                when, frames, slow, acquire, wait, blit, ui, video, frame, acquire_timeouts);
  }
};

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc >= 7 && !std::strcmp(argv[1], "child"))
    return child(argv[2], UINT(std::atoi(argv[3])), std::atof(argv[4]), DWORD(std::strtoul(argv[5], nullptr, 10)),
                 std::strtoull(argv[6], nullptr, 16));
  const double seconds = argc > 1 ? std::atof(argv[1]) : 10.0;
  const char *video = argc > 2 ? argv[2] : "nv12";
  const char *handoff = argc > 3 ? argv[3] : "keyed";
  const UINT fps = argc > 4 ? UINT(std::atoi(argv[4])) : 60u;
  std::printf("       chromium_like: %.0f s, video %s, hand-off %s, %u Hz, HELIOS_ICD=%s\n", seconds, video, handoff,
              fps, std::getenv("HELIOS_ICD") ? std::getenv("HELIOS_ICD") : "(unset)");

  WNDCLASSW wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"chromium_like";
  RegisterClassW(&wc);
  HWND hwnd = CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP, wc.lpszClassName, L"chromium_like", WS_OVERLAPPEDWINDOW,
                              100, 100, kW, kH, nullptr, nullptr, wc.hInstance, nullptr);
  ShowWindow(hwnd, SW_SHOW);

  ID3D11Device5 *dev = nullptr;
  ID3D11DeviceContext4 *ctx = nullptr;
  if (!make_device(&dev, &ctx)) return 1;
  IDXGIDevice *dxgi = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), reinterpret_cast<void **>(&dxgi));
  IDXGIAdapter *adapter = nullptr;
  dxgi->GetAdapter(&adapter);
  IDXGIFactory2 *factory = nullptr;
  adapter->GetParent(__uuidof(IDXGIFactory2), reinterpret_cast<void **>(&factory));

  IDCompositionDevice *dcomp = nullptr;
  CHECK("DCompositionCreateDevice", DCompositionCreateDevice(dxgi, __uuidof(IDCompositionDevice),
                                                             reinterpret_cast<void **>(&dcomp)));
  IDCompositionTarget *target = nullptr;
  CHECK("CreateTargetForHwnd", dcomp->CreateTargetForHwnd(hwnd, TRUE, &target));
  IDCompositionVisual *root = nullptr, *video_visual = nullptr;
  dcomp->CreateVisual(&root);
  dcomp->CreateVisual(&video_visual);

  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = kW;
  sd.Height = kH;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = 2;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
  sd.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
  IDXGISwapChain1 *ui = nullptr;
  CHECK("UI swap chain", factory->CreateSwapChainForComposition(dev, &sd, nullptr, &ui));
  root->SetContent(ui);

  IDXGISwapChain1 *vsc = nullptr;
  const bool yuv = !std::strcmp(video, "nv12") || !std::strcmp(video, "yuy2");
  if (std::strcmp(video, "none")) {
    DXGI_SWAP_CHAIN_DESC1 vd = sd;
    vd.Format = !std::strcmp(video, "nv12") ? DXGI_FORMAT_NV12
                : !std::strcmp(video, "yuy2") ? DXGI_FORMAT_YUY2 : DXGI_FORMAT_B8G8R8A8_UNORM;
    vd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
    vd.BufferCount = 3;
    vd.Flags = yuv ? DXGI_SWAP_CHAIN_FLAG_YUV_VIDEO : 0;
    vd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    HRESULT hr = factory->CreateSwapChainForComposition(dev, &vd, nullptr, &vsc);
    std::printf("       video swap chain %s %ux%u flags 0x%x: 0x%08lx\n", video, vd.Width, vd.Height, vd.Flags,
                static_cast<unsigned long>(hr));
    if (FAILED(hr)) return 1;
    video_visual->SetContent(vsc);
    video_visual->SetOffsetX(0.0f);
    video_visual->SetOffsetY(0.0f);
    root->AddVisual(video_visual, TRUE, nullptr);
  }
  target->SetRoot(root);
  CHECK("DComp Commit", dcomp->Commit());

  // The decoded frame (shared with the child unless HANDOFF=none)
  const bool keyed = !std::strcmp(handoff, "keyed"), fenced = !std::strcmp(handoff, "fence");
  D3D11_TEXTURE2D_DESC td = {};
  td.Width = kW;
  td.Height = kH;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
  td.MiscFlags = keyed ? D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX
                 : fenced ? D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED : 0;
  ID3D11Texture2D *frame_tex = nullptr;
  CHECK("decoded-frame texture", dev->CreateTexture2D(&td, nullptr, &frame_tex));
  IDXGIKeyedMutex *km = nullptr;
  ID3D11Fence *fence = nullptr;
  HANDLE nt = nullptr, fence_nt = nullptr;
  PROCESS_INFORMATION pi = {};
  if (fenced) signalled_value();
  if (keyed || fenced) {
    IDXGIResource1 *r = nullptr;
    frame_tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void **>(&r));
    CHECK("CreateSharedHandle(frame)", r->CreateSharedHandle(nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                                             kTexName, &nt));
    r->Release();
    if (keyed) {
      frame_tex->QueryInterface(__uuidof(IDXGIKeyedMutex), reinterpret_cast<void **>(&km));
    } else {
      CHECK("CreateFence(shared)", dev->CreateFence(0, D3D11_FENCE_FLAG_SHARED, __uuidof(ID3D11Fence),
                                                    reinterpret_cast<void **>(&fence)));
      CHECK("fence CreateSharedHandle", fence->CreateSharedHandle(nullptr, GENERIC_ALL, nullptr, &fence_nt));
    }
    char self[MAX_PATH], cmd[MAX_PATH + 128];
    GetModuleFileNameA(nullptr, self, sizeof(self));
    std::snprintf(cmd, sizeof(cmd), "\"%s\" child %s %u %.1f %lu %llx", self, handoff, fps, seconds + 1.0,
                  GetCurrentProcessId(), static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(fence_nt)));
    STARTUPINFOA si = {};
    si.cb = sizeof(si);
    if (!CreateProcessA(nullptr, cmd, nullptr, nullptr, FALSE, 0, nullptr, nullptr, &si, &pi)) {
      std::printf("[FAIL] start the child: %lu\n", GetLastError());
      return 1;
    }
  }

  // Video processor: decoded frame -> video swap chain back buffer
  ID3D11VideoDevice *vdev = nullptr;
  ID3D11VideoContext *vctx = nullptr;
  ID3D11VideoProcessorEnumerator *venum = nullptr;
  ID3D11VideoProcessor *vp = nullptr;
  ID3D11VideoProcessorInputView *vin = nullptr;
  if (vsc) {
    CHECK("ID3D11VideoDevice", dev->QueryInterface(__uuidof(ID3D11VideoDevice), reinterpret_cast<void **>(&vdev)));
    CHECK("ID3D11VideoContext", ctx->QueryInterface(__uuidof(ID3D11VideoContext), reinterpret_cast<void **>(&vctx)));
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd = {};
    cd.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    cd.InputWidth = kW;
    cd.InputHeight = kH;
    cd.OutputWidth = kW;
    cd.OutputHeight = kH;
    cd.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    CHECK("CreateVideoProcessorEnumerator", vdev->CreateVideoProcessorEnumerator(&cd, &venum));
    CHECK("CreateVideoProcessor", vdev->CreateVideoProcessor(venum, 0, &vp));
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd = {};
    ivd.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    CHECK("video input view", vdev->CreateVideoProcessorInputView(frame_tex, venum, &ivd, &vin));
  }

  Stats total, second;
  UINT startup_slow = 0;
  const double t0 = now_ms(), period = 1000.0 / fps;
  double last_report = t0;
  UINT frame = 0;
  MSG msg;
  while (now_ms() - t0 < seconds * 1000.0) {
    while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
    if (g_closed || ((frame & 15) == 0 && stop_requested())) {
      std::printf("       stopped early (stop file or WM_CLOSE)\n");
      break;
    }
    const double f0 = now_ms();
    frame++;
    double t_acq = 0, t_wait = 0;
    if (keyed) {
      const double a = now_ms();
      const HRESULT hr = km->AcquireSync(1, 100);
      t_acq = now_ms() - a;
      if (hr != S_OK) {
        total.acquire_timeouts++;
        second.acquire_timeouts++;
      }
    } else if (fenced) {
      const double a = now_ms();
      // A GPU wait for the child's last signal, then a CPU wait for the frame
      // that consumed it (event query), timed: a shared fence that never
      // reaches its value in this process shows here.
      const UINT64 v = signalled_value() ? UINT64(*signalled_value()) : 0;
      if (v) ctx->Wait(fence, v);
      D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
      ID3D11Query *q = nullptr;
      dev->CreateQuery(&qd, &q);
      ctx->End(q);
      BOOL done = FALSE;
      while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK && now_ms() - a < 3000.0) Sleep(0);
      release(q);
      t_wait = now_ms() - a;
      if (!done) {
        total.acquire_timeouts++;
        second.acquire_timeouts++;
      }
    } else {
      fill(ctx, frame_tex, frame);
    }
    // UI: clear the page
    ID3D11Texture2D *bb = nullptr;
    ui->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb));
    ID3D11RenderTargetView *rtv = nullptr;
    dev->CreateRenderTargetView(bb, nullptr, &rtv);
    const float c[4] = {0.1f, 0.1f, 0.3f + 0.2f * float(frame % 2), 1.0f};
    ctx->ClearRenderTargetView(rtv, c);
    release(rtv);
    release(bb);
    // Video: blit the frame into the video swap chain
    double t_blit = 0;
    if (vsc) {
      const double b = now_ms();
      ID3D11Texture2D *vb = nullptr;
      vsc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&vb));
      D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd = {};
      ovd.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
      ID3D11VideoProcessorOutputView *vout = nullptr;
      HRESULT hr = vdev->CreateVideoProcessorOutputView(vb, venum, &ovd, &vout);
      if (SUCCEEDED(hr)) {
        D3D11_VIDEO_PROCESSOR_STREAM stream = {};
        stream.Enable = TRUE;
        stream.pInputSurface = vin;
        hr = vctx->VideoProcessorBlt(vp, vout, 0, 1, &stream);
      }
      if (FAILED(hr) && frame == 1)
        std::printf("       video blit failed: 0x%08lx\n", static_cast<unsigned long>(hr));
      release(vout);
      release(vb);
      t_blit = now_ms() - b;
    }
    if (keyed) km->ReleaseSync(0);
    const double u = now_ms();
    ui->Present(1, 0);
    const double t_ui = now_ms() - u;
    double t_video = 0;
    if (vsc) {
      const double v = now_ms();
      vsc->Present(1, 0);
      t_video = now_ms() - v;
    }
    const double t_frame = now_ms() - f0;
    total.take(t_acq, t_wait, t_blit, t_ui, t_video, t_frame);
    // Start-up (first second: shader compiles, first imports) is not a stall.
    if (t_frame > 100.0 && now_ms() - t0 < 1000.0) startup_slow++;
    second.take(t_acq, t_wait, t_blit, t_ui, t_video, t_frame);
    if (now_ms() - last_report >= 1000.0) {
      char when[32];
      std::snprintf(when, sizeof(when), "t=%.0f s", (now_ms() - t0) / 1000.0);
      second.print(when);
      second = Stats();
      last_report = now_ms();
    }
    const double left = (f0 + period) - now_ms();
    if (left > 1.0) Sleep(DWORD(left));
  }
  const double elapsed = (now_ms() - t0) / 1000.0;
  total.print("total");
  std::printf("       %.1f frames/s over %.1f s\n", total.frames / elapsed, elapsed);
  if (vsc) {
    DXGI_FRAME_STATISTICS fs = {};
    const HRESULT hr = vsc->GetFrameStatistics(&fs);
    UINT last = 0;
    vsc->GetLastPresentCount(&last);
    std::printf("       video swap chain: GetFrameStatistics 0x%08lx PresentCount %u PresentRefreshCount %u "
                "SyncRefreshCount %u; last present count %u\n",
                static_cast<unsigned long>(hr), fs.PresentCount, fs.PresentRefreshCount, fs.SyncRefreshCount, last);
  }
  if (pi.hProcess) {
    WaitForSingleObject(pi.hProcess, 15000);
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    std::printf("       child exit %lu\n", code);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
  }
  const bool smooth = total.frames > UINT(0.8 * fps * seconds) && total.slow == startup_slow;
  std::printf("%s\n", smooth ? "CHROMIUM-LIKE SMOOTH" : "CHROMIUM-LIKE STALLED");
  release(vin);
  release(vp);
  release(venum);
  release(vctx);
  release(vdev);
  release(km);
  release(fence);
  if (nt) CloseHandle(nt);
  if (fence_nt) CloseHandle(fence_nt);
  release(frame_tex);
  release(vsc);
  release(ui);
  release(video_visual);
  release(root);
  release(target);
  release(dcomp);
  release(factory);
  release(adapter);
  release(dxgi);
  release(ctx);
  release(dev);
  DestroyWindow(hwnd);
  return smooth ? 0 : 1;
}
