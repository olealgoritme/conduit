// d3d11_iflip.cpp: a flip-model (DXGI_SWAP_EFFECT_FLIP_DISCARD) D3D11 swap chain in a window,
// reporting how Windows presents it. The vehicle for independent flip
// (guest/windows/docs/independent-flip.md section 11).
//
// What it reports:
//   * at start: the adapter, its output and current mode, and what dxgkrnl derived from the
//     KMD's caps (D3DKMTQueryAdapterInfo: DIRECTFLIP_SUPPORT 19, INDEPENDENTFLIP_SUPPORT 28,
//     MULTIPLANEOVERLAY_SUPPORT 20, INDEPENDENTFLIP_SECONDARY_SUPPORT 39);
//   * once a second: frames per second, and the presentation mode DXGI reports through
//     IDXGISwapChainMedia::GetFrameStatisticsMedia (COMPOSED, OVERLAY, NONE, COMPOSITION_FAILURE),
//     plus GetFrameStatistics' present / sync refresh counts;
//   * at the end: one "RESULT" line with the per-mode sample counts.
// PresentMon's PresentMode column is the authority for "Hardware: Independent Flip"; the DXGI
// mode is a second, in-process reading (NONE is the direct, not-composed case).
//
// Build on the Linux host (mingw-w64):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_iflip.exe d3d11_iflip.cpp -ld3d11 -ldxgi -lgdi32 -luser32
//
// Run in the interactive session (schtasks /IT; session 0 has no desktop):
//   d3d11_iflip.exe [seconds] [options...]
//     seconds     run time (default 20)
//     window      a decorated 1280x720 window (default: borderless, covering the output at its
//                 current mode, which is what independent flip needs without overlay planes)
//     interval0   Present(0, 0) (default Present(1, 0))
//     tearing     ALLOW_TEARING swap chain + DXGI_PRESENT_ALLOW_TEARING with interval0
//     cursor      keep the mouse cursor visible over the window (default: hidden, so a software
//                 cursor cannot force composition)
//     rgba        R8G8B8A8_UNORM back buffers (default B8G8R8A8_UNORM)
//     buffers=N   back buffer count (default 2)
//     adapter=N   DXGI adapter index (default: the first adapter whose name contains "Helios",
//                 else the default adapter)
// Logs to stdout and to d3d11_iflip.txt in the current directory.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_6.h>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

static FILE* g_log = nullptr;
static void L(const char* fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  char buf[1024];
  vsnprintf(buf, sizeof(buf), fmt, ap);
  va_end(ap);
  fputs(buf, stdout);
  fputc('\n', stdout);
  fflush(stdout);
  if (g_log) {
    fputs(buf, g_log);
    fputc('\n', g_log);
    fflush(g_log);
  }
}

static bool g_quit = false;
static bool g_hide_cursor = true;
static LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
  switch (m) {
    case WM_DESTROY: g_quit = true; PostQuitMessage(0); return 0;
    case WM_KEYDOWN: if (w == VK_ESCAPE) DestroyWindow(h); return 0;
    case WM_SETCURSOR:
      if (g_hide_cursor && LOWORD(l) == HTCLIENT) { SetCursor(nullptr); return TRUE; }
      break;
  }
  return DefWindowProc(h, m, w, l);
}

// ---- D3DKMT caps probe (gdi32, by LUID) -------------------------------------------------
typedef LONG(APIENTRY* PfnOpenFromLuid)(void*);
typedef LONG(APIENTRY* PfnQuery)(void*);
typedef LONG(APIENTRY* PfnClose)(void*);
struct KmtOpenFromLuid { LUID luid; UINT hAdapter; };
struct KmtQuery { UINT hAdapter; UINT type; void* data; UINT size; };
struct KmtClose { UINT hAdapter; };

static void ProbeKmt(LUID luid) {
  HMODULE gdi = LoadLibraryA("gdi32.dll");
  auto open = (PfnOpenFromLuid)GetProcAddress(gdi, "D3DKMTOpenAdapterFromLuid");
  auto query = (PfnQuery)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
  auto close = (PfnClose)GetProcAddress(gdi, "D3DKMTCloseAdapter");
  if (!open || !query || !close) { L("kmt: entry points missing"); return; }
  KmtOpenFromLuid o = {luid, 0};
  LONG st = open(&o);
  if (st < 0) { L("kmt: OpenAdapterFromLuid status=0x%08lx", (unsigned long)st); return; }
  struct { UINT type; const char* name; } kinds[] = {
      {19, "DIRECTFLIP_SUPPORT"}, {28, "INDEPENDENTFLIP_SUPPORT"},
      {20, "MULTIPLANEOVERLAY_SUPPORT"}, {39, "INDEPENDENTFLIP_SECONDARY_SUPPORT"}};
  for (auto& k : kinds) {
    // Each answer is a struct whose first member is a BOOL (Supported); 16 bytes of room.
    UINT out[4] = {};
    KmtQuery q = {o.hAdapter, k.type, out, sizeof(out)};
    LONG s = query(&q);
    // Retry with the exact 4-byte size if the kernel insists on it.
    if (s < 0) { q.size = 4; s = query(&q); }
    L("kmt: %-34s status=0x%08lx supported=%u", k.name, (unsigned long)s, s >= 0 ? out[0] : 0u);
  }
  KmtClose c = {o.hAdapter};
  close(&c);
}

static const char* ModeName(DXGI_FRAME_PRESENTATION_MODE m) {
  switch (m) {
    case DXGI_FRAME_PRESENTATION_MODE_COMPOSED: return "COMPOSED";
    case DXGI_FRAME_PRESENTATION_MODE_OVERLAY: return "OVERLAY";
    case DXGI_FRAME_PRESENTATION_MODE_NONE: return "NONE";
    case DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE: return "COMPOSITION_FAILURE";
  }
  return "?";
}

int main(int argc, char** argv) {
  g_log = fopen("d3d11_iflip.txt", "w");
  int seconds = 20;
  bool windowed = false, interval0 = false, tearing = false, rgba = false;
  int buffers = 2, adapterIndex = -1;
  for (int i = 1; i < argc; ++i) {
    const char* a = argv[i];
    if (a[0] >= '0' && a[0] <= '9') seconds = atoi(a);
    else if (!strcmp(a, "window")) windowed = true;
    else if (!strcmp(a, "interval0")) interval0 = true;
    else if (!strcmp(a, "tearing")) tearing = true;
    else if (!strcmp(a, "cursor")) g_hide_cursor = false;
    else if (!strcmp(a, "rgba")) rgba = true;
    else if (!strncmp(a, "buffers=", 8)) buffers = atoi(a + 8);
    else if (!strncmp(a, "adapter=", 8)) adapterIndex = atoi(a + 8);
    else L("unknown option %s", a);
  }
  if (buffers < 2) buffers = 2;
  if (buffers > 16) buffers = 16;
  L("d3d11_iflip pid=%lu seconds=%d %s interval=%d tearing=%d cursor=%s format=%s buffers=%d",
    GetCurrentProcessId(), seconds, windowed ? "window" : "borderless", interval0 ? 0 : 1, tearing,
    g_hide_cursor ? "hidden" : "visible", rgba ? "RGBA8" : "BGRA8", buffers);

  IDXGIFactory2* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory2), (void**)&factory);
  if (FAILED(hr)) { L("CreateDXGIFactory1 hr=0x%08x", (unsigned)hr); return 2; }

  // Adapter: by index, or the first Helios one.
  IDXGIAdapter1* adapter = nullptr;
  {
    IDXGIAdapter1* a = nullptr;
    for (UINT i = 0; factory->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
      DXGI_ADAPTER_DESC1 d{};
      a->GetDesc1(&d);
      L("adapter[%u] luid=%08lx:%08lx flags=0x%x %ls", i, (unsigned long)d.AdapterLuid.HighPart,
        (unsigned long)d.AdapterLuid.LowPart, (unsigned)d.Flags, d.Description);
      bool take = adapterIndex >= 0 ? (int)i == adapterIndex
                                    : (wcsstr(d.Description, L"Helios") != nullptr);
      if (take && !adapter) { adapter = a; a->AddRef(); L("  -> selected"); }
      a->Release();
    }
  }
  DXGI_ADAPTER_DESC1 ad{};
  if (adapter) {
    adapter->GetDesc1(&ad);
    ProbeKmt(ad.AdapterLuid);
  }

  // The output and its current mode: the rectangle a borderless window must cover.
  RECT target = {0, 0, 1280, 720};
  {
    IDXGIOutput* out = nullptr;
    if (adapter && adapter->EnumOutputs(0, &out) == S_OK) {
      DXGI_OUTPUT_DESC od{};
      out->GetDesc(&od);
      target = od.DesktopCoordinates;
      DEVMODEW dm{};
      dm.dmSize = sizeof(dm);
      EnumDisplaySettingsW(od.DeviceName, ENUM_CURRENT_SETTINGS, &dm);
      L("output %ls desktop=%ld,%ld-%ld,%ld mode=%lux%lu@%lu attached=%d", od.DeviceName,
        target.left, target.top, target.right, target.bottom, dm.dmPelsWidth, dm.dmPelsHeight,
        dm.dmDisplayFrequency, od.AttachedToDesktop);
      out->Release();
    } else {
      L("adapter has no output 0: borderless falls back to the primary monitor");
      target = {0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)};
    }
  }

  // Window.
  WNDCLASSA wc = {};
  wc.lpfnWndProc = WndProc;
  wc.hInstance = GetModuleHandle(nullptr);
  wc.lpszClassName = "iflip";
  wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassA(&wc);
  HWND hwnd;
  UINT width, height;
  if (windowed) {
    RECT rc = {target.left + 100, target.top + 100, target.left + 100 + 1280, target.top + 100 + 720};
    AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
    hwnd = CreateWindowA("iflip", "d3d11-iflip", WS_OVERLAPPEDWINDOW, rc.left, rc.top,
                         rc.right - rc.left, rc.bottom - rc.top, nullptr, nullptr, wc.hInstance, nullptr);
    width = 1280;
    height = 720;
  } else {
    width = target.right - target.left;
    height = target.bottom - target.top;
    hwnd = CreateWindowExA(0, "iflip", "d3d11-iflip", WS_POPUP, target.left, target.top, width,
                           height, nullptr, nullptr, wc.hInstance, nullptr);
  }
  ShowWindow(hwnd, SW_SHOW);
  SetForegroundWindow(hwnd);
  RECT cr{};
  GetClientRect(hwnd, &cr);
  L("hwnd=%p client=%ldx%ld", hwnd, cr.right - cr.left, cr.bottom - cr.top);

  // Device.
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL got{};
  hr = D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                         nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
                         &dev, &got, &ctx);
  L("D3D11CreateDevice hr=0x%08x level=0x%x", (unsigned)hr, (unsigned)got);
  if (FAILED(hr)) return 3;

  if (tearing) {
    IDXGIFactory5* f5 = nullptr;
    BOOL allow = FALSE;
    if (SUCCEEDED(factory->QueryInterface(__uuidof(IDXGIFactory5), (void**)&f5))) {
      f5->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &allow, sizeof(allow));
      f5->Release();
    }
    L("DXGI_FEATURE_PRESENT_ALLOW_TEARING=%d", allow);
    if (!allow) tearing = false;
  }

  // Flip-model swap chain.
  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = width;
  sd.Height = height;
  sd.Format = rgba ? DXGI_FORMAT_R8G8B8A8_UNORM : DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = buffers;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  sd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
  sd.Flags = tearing ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0;
  IDXGISwapChain1* sc = nullptr;
  hr = factory->CreateSwapChainForHwnd(dev, hwnd, &sd, nullptr, nullptr, &sc);
  L("CreateSwapChainForHwnd(FLIP_DISCARD %ux%u) hr=0x%08x", width, height, (unsigned)hr);
  if (FAILED(hr)) return 4;
  factory->MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
  IDXGISwapChainMedia* media = nullptr;
  sc->QueryInterface(__uuidof(IDXGISwapChainMedia), (void**)&media);
  L("IDXGISwapChainMedia %s", media ? "available" : "NOT available");

  ID3D11Texture2D* bb = nullptr;
  ID3D11RenderTargetView* rtv = nullptr;
  sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)&bb);
  dev->CreateRenderTargetView(bb, nullptr, &rtv);
  bb->Release();

  LARGE_INTEGER freq, t0, last;
  QueryPerformanceFrequency(&freq);
  QueryPerformanceCounter(&t0);
  last = t0;
  unsigned frames = 0, framesSec = 0;
  unsigned modeCount[5] = {};  // COMPOSED, OVERLAY, NONE, FAILURE, unavailable
  UINT interval = interval0 ? 0 : 1;
  UINT flags = (tearing && interval0) ? DXGI_PRESENT_ALLOW_TEARING : 0;
  MSG msg;
  while (!g_quit) {
    while (PeekMessage(&msg, nullptr, 0, 0, PM_REMOVE)) {
      TranslateMessage(&msg);
      DispatchMessage(&msg);
    }
    if (g_quit) break;
    LARGE_INTEGER now;
    QueryPerformanceCounter(&now);
    double t = double(now.QuadPart - t0.QuadPart) / freq.QuadPart;
    if (t >= seconds) break;
    // A moving color so a frozen (kept) picture is visible at a glance.
    float c[4] = {0.5f + 0.5f * (float)__builtin_sin(t * 3.0), 0.2f,
                  0.5f + 0.5f * (float)__builtin_cos(t * 2.0), 1.0f};
    // FLIP_DISCARD: the render target must be rebound to the current buffer every frame.
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->ClearRenderTargetView(rtv, c);
    hr = sc->Present(interval, flags);
    if (FAILED(hr)) { L("Present hr=0x%08x at frame %u", (unsigned)hr, frames); break; }
    frames++;
    framesSec++;
    double dt = double(now.QuadPart - last.QuadPart) / freq.QuadPart;
    if (dt >= 1.0) {
      const char* mode = "unavailable";
      unsigned mi = 4;
      if (media) {
        DXGI_FRAME_STATISTICS_MEDIA fsm{};
        HRESULT h = media->GetFrameStatisticsMedia(&fsm);
        if (SUCCEEDED(h)) { mode = ModeName(fsm.CompositionMode); mi = (unsigned)fsm.CompositionMode & 3; }
        else if (h == DXGI_ERROR_FRAME_STATISTICS_DISJOINT) mode = "disjoint";
      }
      modeCount[mi]++;
      DXGI_FRAME_STATISTICS fs{};
      HRESULT hs = sc->GetFrameStatistics(&fs);
      UINT lastCount = 0;
      sc->GetLastPresentCount(&lastCount);
      L("t=%5.1fs fps=%6.1f dxgi_mode=%s stats_hr=0x%08x present=%u refresh=%u sync_refresh=%u last=%u",
        t, framesSec / dt, mode, (unsigned)hs, fs.PresentCount, fs.PresentRefreshCount,
        fs.SyncRefreshCount, lastCount);
      framesSec = 0;
      last = now;
    }
  }
  LARGE_INTEGER end;
  QueryPerformanceCounter(&end);
  double total = double(end.QuadPart - t0.QuadPart) / freq.QuadPart;
  L("RESULT frames=%u seconds=%.1f fps=%.1f COMPOSED=%u OVERLAY=%u NONE=%u FAILURE=%u unavailable=%u",
    frames, total, frames / total, modeCount[0], modeCount[1], modeCount[2], modeCount[3], modeCount[4]);
  if (media) media->Release();
  rtv->Release();
  sc->Release();
  ctx->Release();
  dev->Release();
  if (adapter) adapter->Release();
  factory->Release();
  if (g_log) fclose(g_log);
  return 0;
}
