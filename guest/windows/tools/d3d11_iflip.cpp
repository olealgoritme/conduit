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
//     rgb10       R10G10B10A2_UNORM back buffers (the KMD cannot scan these out: promoted, the
//                 picture must not freeze; the safe outcome is that DWM keeps composing)
//     fp16        R16G16B16A16_FLOAT back buffers (as rgb10)
//     buffers=N   back buffer count (default 2)
//     adapter=N   DXGI adapter index (default: the first adapter whose name contains "Helios",
//                 else the default adapter)
//     topmost     WS_EX_TOPMOST on the borderless window (nothing, the taskbar included, can sit
//                 above it)
//     fg          take the foreground at start (a process started by the task scheduler is not
//                 allowed to: SetForegroundWindow fails and the console or the taskbar stays
//                 active, which keeps the window from being treated as full screen)
//     noconsole   detach from (and so hide) this program's own console window
//     exclusive   full-screen exclusive (SetFullscreenState TRUE) at size=WxH (default: the current
//                 mode): Windows switches the output to that mode. ALLOW_TEARING is not allowed in
//                 exclusive mode, so `tearing` is dropped there; interval0 is immediate anyway.
//     size=WxH    swap chain (and borderless window) size (default: the output's current mode)
//     setmode=WxH ChangeDisplaySettingsEx the output to WxH first (CDS_FULLSCREEN, as SDL's
//                 exclusive full screen does), restored at the end; the window then covers it
//     counter     clear every frame to a colour encoding its index (R bits 0..7, G 8..15, B 16..23,
//                 A 255) for the host's CONDUIT_FRAME_PROBE (B8G8R8A8 back buffers)
//     modes       log the output's DXGI mode list (B8G8R8A8) and the closest match to size=
//     fps=N       cap the frame rate at N (absolute QPC deadlines, yield then spin); default
//                 uncapped. With fps=200 / fps=300 on a 240 Hz mode it is the pacing test row.
// Timing (the RESULT_TIMING lines): the interval between Present calls (p50/p90/p99/max), how long
// each Present call blocked (a vblank wait shows as a ms-scale p99), and, from
// GetFrameStatistics, the present-to-screen latency (SyncQPCTime of a present minus the QPC at
// which it was issued) and the share of screen intervals that are two refreshes or more.
// Every second it also logs whether the window is the foreground window and which window is
// under the output's four corners and centre (anything but this window there is an overlap that
// forces composition), and once the output's hardware composition support
// (IDXGIOutput6::CheckHardwareCompositionSupport).
//     log=PATH    write the log there (default: d3d11_iflip.txt next to the executable; a task
//                 started by the scheduler runs in C:\Windows\System32, where the old
//                 current-directory log could not be created, and its stdout is not captured)
// Logs to stdout and to the log file.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_6.h>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <algorithm>
#include <vector>

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
  {
    const char* logPath = nullptr;
    for (int i = 1; i < argc; ++i)
      if (!strncmp(argv[i], "log=", 4)) logPath = argv[i] + 4;
    char def[MAX_PATH] = "";
    if (!logPath) {
      DWORD n = GetModuleFileNameA(nullptr, def, MAX_PATH);
      char* slash = n ? strrchr(def, '\\') : nullptr;
      if (slash && (size_t)(slash + 1 - def) + 16 < sizeof(def)) {
        strcpy(slash + 1, "d3d11_iflip.txt");
        logPath = def;
      } else {
        logPath = "d3d11_iflip.txt";
      }
    }
    g_log = fopen(logPath, "w");
  }
  int seconds = 20;
  double fpsCap = 0.0;
  bool exclusive = false, listModes = false, counter = false;
  UINT reqW = 0, reqH = 0, setW = 0, setH = 0;
  bool windowed = false, interval0 = false, tearing = false, rgba = false;
  int buffers = 2, adapterIndex = -1;
  bool topmost = false, takeFg = false, noConsole = false, rgb10 = false, fp16 = false;
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
    else if (!strcmp(a, "topmost")) topmost = true;
    else if (!strcmp(a, "rgb10")) rgb10 = true;
    else if (!strncmp(a, "log=", 4)) {}
    else if (!strcmp(a, "fp16")) fp16 = true;
    else if (!strcmp(a, "fg")) takeFg = true;
    else if (!strcmp(a, "noconsole")) noConsole = true;
    else if (!strncmp(a, "fps=", 4)) fpsCap = atof(a + 4);
    else if (!strcmp(a, "exclusive")) exclusive = true;
    else if (!strncmp(a, "size=", 5)) sscanf(a + 5, "%ux%u", &reqW, &reqH);
    else if (!strncmp(a, "setmode=", 8)) sscanf(a + 8, "%ux%u", &setW, &setH);
    else if (!strcmp(a, "modes")) listModes = true;
    else if (!strcmp(a, "counter")) counter = true;
    else L("unknown option %s", a);
  }
  if (buffers < 2) buffers = 2;
  if (buffers > 16) buffers = 16;
  L("d3d11_iflip pid=%lu seconds=%d %s interval=%d tearing=%d cursor=%s format=%s buffers=%d "
    "topmost=%d fg=%d noconsole=%d",
    GetCurrentProcessId(), seconds, windowed ? "window" : "borderless", interval0 ? 0 : 1, tearing,
    g_hide_cursor ? "hidden" : "visible",
    fp16 ? "RGBA16F" : rgb10 ? "RGB10A2" : rgba ? "RGBA8" : "BGRA8", buffers, topmost, takeFg,
    noConsole);
  if (noConsole) FreeConsole();

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
  WCHAR outName[32] = L"";
  {
    IDXGIOutput* out = nullptr;
    if (adapter && adapter->EnumOutputs(0, &out) == S_OK) {
      DXGI_OUTPUT_DESC od{};
      out->GetDesc(&od);
      wcsncpy(outName, od.DeviceName, 31);
      if (setW && setH) {
        DEVMODEW want{};
        want.dmSize = sizeof(want);
        want.dmPelsWidth = setW;
        want.dmPelsHeight = setH;
        want.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        LONG r = ChangeDisplaySettingsExW(od.DeviceName, &want, nullptr, CDS_FULLSCREEN, nullptr);
        Sleep(1500);
        out->GetDesc(&od);
        L("setmode %ux%u: ChangeDisplaySettingsEx=%ld (0 = DISP_CHANGE_SUCCESSFUL)", setW, setH, r);
      }
      if (listModes) {
        UINT n = 0;
        out->GetDisplayModeList(DXGI_FORMAT_B8G8R8A8_UNORM, 0, &n, nullptr);
        DXGI_MODE_DESC* ml = (DXGI_MODE_DESC*)calloc(n ? n : 1, sizeof(DXGI_MODE_DESC));
        out->GetDisplayModeList(DXGI_FORMAT_B8G8R8A8_UNORM, 0, &n, ml);
        char line[1024] = "";
        for (UINT i = 0; i < n && strlen(line) < 900; ++i) {
          char e[48];
          snprintf(e, sizeof(e), " %ux%u@%.2f", ml[i].Width, ml[i].Height,
                   ml[i].RefreshRate.Denominator ? (double)ml[i].RefreshRate.Numerator / ml[i].RefreshRate.Denominator : 0.0);
          strcat(line, e);
        }
        L("output mode list (%u):%s", n, line);
        free(ml);
        if (reqW && reqH) {
          DXGI_MODE_DESC wantm = {}, got = {};
          wantm.Width = reqW;
          wantm.Height = reqH;
          wantm.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
          HRESULT hc = out->FindClosestMatchingMode(&wantm, &got, nullptr);
          L("closest to %ux%u: hr=0x%08x %ux%u", reqW, reqH, (unsigned)hc, got.Width, got.Height);
        }
      }
      target = od.DesktopCoordinates;
      DEVMODEW dm{};
      dm.dmSize = sizeof(dm);
      EnumDisplaySettingsW(od.DeviceName, ENUM_CURRENT_SETTINGS, &dm);
      L("output %ls desktop=%ld,%ld-%ld,%ld mode=%lux%lu@%lu attached=%d", od.DeviceName,
        target.left, target.top, target.right, target.bottom, dm.dmPelsWidth, dm.dmPelsHeight,
        dm.dmDisplayFrequency, od.AttachedToDesktop);
      IDXGIOutput6* o6 = nullptr;
      if (SUCCEEDED(out->QueryInterface(__uuidof(IDXGIOutput6), (void**)&o6))) {
        UINT hcs = 0;
        HRESULT h = o6->CheckHardwareCompositionSupport(&hcs);
        L("output hardware composition support hr=0x%08x flags=0x%x (FULLSCREEN=1 WINDOWED=2 "
          "CURSOR_STRETCHED=4)", (unsigned)h, hcs);
        o6->Release();
      }
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
    width = reqW ? reqW : target.right - target.left;
    height = reqH ? reqH : target.bottom - target.top;
    hwnd = CreateWindowExA(topmost ? WS_EX_TOPMOST : 0, "iflip", "d3d11-iflip", WS_POPUP,
                           target.left, target.top, width, height, nullptr, nullptr, wc.hInstance,
                           nullptr);
  }
  ShowWindow(hwnd, SW_SHOW);
  if (takeFg) {
    // The documented ways a background process may take the foreground: attach to the input of
    // the thread that owns it, and a synthetic key event (the foreground lock yields to input).
    HWND fgw = GetForegroundWindow();
    DWORD fgThread = fgw ? GetWindowThreadProcessId(fgw, nullptr) : 0;
    DWORD me = GetCurrentThreadId();
    if (fgThread && fgThread != me) AttachThreadInput(me, fgThread, TRUE);
    keybd_event(VK_MENU, 0, 0, 0);
    keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
    SetWindowPos(hwnd, topmost ? HWND_TOPMOST : HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    SetForegroundWindow(hwnd);
    SetActiveWindow(hwnd);
    SetFocus(hwnd);
    if (fgThread && fgThread != me) AttachThreadInput(me, fgThread, FALSE);
  } else {
    SetForegroundWindow(hwnd);
  }
  L("foreground after start: %s (foreground hwnd=%p)",
    GetForegroundWindow() == hwnd ? "this window" : "ANOTHER window", GetForegroundWindow());
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

  if (exclusive && tearing) {
    L("exclusive: ALLOW_TEARING is not allowed in full-screen exclusive mode; dropped");
    tearing = false;
  }
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
  sd.Format = fp16    ? DXGI_FORMAT_R16G16B16A16_FLOAT
              : rgb10 ? DXGI_FORMAT_R10G10B10A2_UNORM
              : rgba  ? DXGI_FORMAT_R8G8B8A8_UNORM
                      : DXGI_FORMAT_B8G8R8A8_UNORM;
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

  if (exclusive) {
    HRESULT hf = sc->SetFullscreenState(TRUE, nullptr);
    DXGI_MODE_DESC md = {};
    md.Width = width;
    md.Height = height;
    md.Format = sd.Format;
    HRESULT ht = sc->ResizeTarget(&md);
    HRESULT hb = sc->ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, sd.Flags);
    BOOL fs = FALSE;
    sc->GetFullscreenState(&fs, nullptr);
    Sleep(1000);
    DEVMODEW dm{};
    dm.dmSize = sizeof(dm);
    EnumDisplaySettingsW(outName[0] ? outName : nullptr, ENUM_CURRENT_SETTINGS, &dm);
    L("exclusive: SetFullscreenState hr=0x%08x ResizeTarget hr=0x%08x ResizeBuffers hr=0x%08x "
      "fullscreen=%d mode now %lux%lu@%lu",
      (unsigned)hf, (unsigned)ht, (unsigned)hb, fs, dm.dmPelsWidth, dm.dmPelsHeight,
      dm.dmDisplayFrequency);
  }
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
  // Timing samples (microseconds).
  std::vector<double> callUs, gapUs, latUs, syncGapUs;
  callUs.reserve(1 << 20);
  gapUs.reserve(1 << 20);
  latUs.reserve(1 << 20);
  syncGapUs.reserve(1 << 20);
  // QPC at which present number n (GetLastPresentCount after it) was issued, by n % ring.
  const unsigned kRing = 4096;
  std::vector<long long> issuedAt(kRing, 0);
  std::vector<unsigned> issuedN(kRing, ~0u);
  unsigned lastStatCount = 0;
  long long lastSyncQpc = 0;
  UINT lastSyncRefresh = 0;
  long long prevPresentQpc = 0;
  const double us = 1e6 / double(freq.QuadPart);
  long long frameTicks = fpsCap > 0 ? (long long)(double(freq.QuadPart) / fpsCap) : 0;
  long long nextDue = t0.QuadPart;
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
    if (counter) {
      unsigned idx = frames & 0xFFFFFFu;
      c[0] = (float)(idx & 0xFF) / 255.0f;
      c[1] = (float)((idx >> 8) & 0xFF) / 255.0f;
      c[2] = (float)((idx >> 16) & 0xFF) / 255.0f;
      c[3] = 1.0f;
    }
    // FLIP_DISCARD: the render target must be rebound to the current buffer every frame.
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->ClearRenderTargetView(rtv, c);
    if (frameTicks) {
      // Absolute deadlines: yield while more than 2 ms remain, then spin (no Sleep: its granularity
      // is the system timer resolution, up to 15.6 ms).
      nextDue += frameTicks;
      LARGE_INTEGER q;
      for (;;) {
        QueryPerformanceCounter(&q);
        long long left = nextDue - q.QuadPart;
        if (left <= 0) break;
        if (left * us > 2000.0) SwitchToThread();
        else YieldProcessor();
      }
      if (q.QuadPart - nextDue > frameTicks) nextDue = q.QuadPart;  // fell behind: no burst
    }
    LARGE_INTEGER pa, pb;
    QueryPerformanceCounter(&pa);
    hr = sc->Present(interval, flags);
    QueryPerformanceCounter(&pb);
    if (FAILED(hr)) { L("Present hr=0x%08x at frame %u", (unsigned)hr, frames); break; }
    if (t > 1.0) {  // skip the first second (promotion, warm-up)
      callUs.push_back(double(pb.QuadPart - pa.QuadPart) * us);
      if (prevPresentQpc) gapUs.push_back(double(pa.QuadPart - prevPresentQpc) * us);
    }
    prevPresentQpc = pa.QuadPart;
    {
      UINT n = 0;
      if (SUCCEEDED(sc->GetLastPresentCount(&n))) {
        issuedAt[n % kRing] = pa.QuadPart;
        issuedN[n % kRing] = n;
      }
      DXGI_FRAME_STATISTICS fs{};
      if (SUCCEEDED(sc->GetFrameStatistics(&fs)) && fs.PresentCount != lastStatCount) {
        lastStatCount = fs.PresentCount;
        unsigned slot = fs.PresentCount % kRing;
        if (t > 1.0 && issuedN[slot] == fs.PresentCount && fs.SyncQPCTime.QuadPart > issuedAt[slot])
          latUs.push_back(double(fs.SyncQPCTime.QuadPart - issuedAt[slot]) * us);
        if (t > 1.0 && lastSyncQpc && fs.SyncQPCTime.QuadPart > lastSyncQpc && fs.SyncRefreshCount > lastSyncRefresh)
          syncGapUs.push_back(double(fs.SyncRefreshCount - lastSyncRefresh));
        lastSyncQpc = fs.SyncQPCTime.QuadPart;
        lastSyncRefresh = fs.SyncRefreshCount;
      }
    }
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
      // Overlap check: the window under the four corners and the centre of the output.
      POINT pts[5] = {{target.left, target.top}, {target.right - 1, target.top},
                      {target.left, target.bottom - 1}, {target.right - 1, target.bottom - 1},
                      {(target.left + target.right) / 2, (target.top + target.bottom) / 2}};
      int covered = 0;
      HWND other = nullptr;
      for (auto& p : pts) {
        HWND w = WindowFromPoint(p);
        if (w == hwnd) covered++;
        else if (!other) other = w;
      }
      char otherClass[64] = "";
      if (other) GetClassNameA(other, otherClass, sizeof(otherClass));
      L("t=%5.1fs fps=%6.1f dxgi_mode=%s stats_hr=0x%08x present=%u refresh=%u sync_refresh=%u last=%u "
        "fg=%d points_on_us=%d/5 other=%p(%s)",
        t, framesSec / dt, mode, (unsigned)hs, fs.PresentCount, fs.PresentRefreshCount,
        fs.SyncRefreshCount, lastCount, GetForegroundWindow() == hwnd, covered, other, otherClass);
      framesSec = 0;
      last = now;
    }
  }
  LARGE_INTEGER end;
  QueryPerformanceCounter(&end);
  double total = double(end.QuadPart - t0.QuadPart) / freq.QuadPart;
  L("RESULT frames=%u seconds=%.1f fps=%.1f COMPOSED=%u OVERLAY=%u NONE=%u FAILURE=%u unavailable=%u",
    frames, total, frames / total, modeCount[0], modeCount[1], modeCount[2], modeCount[3], modeCount[4]);
  auto pct = [](std::vector<double>& v, double p) -> double {
    if (v.empty()) return 0.0;
    size_t i = (size_t)(p * double(v.size() - 1) + 0.5);
    std::nth_element(v.begin(), v.begin() + i, v.end());
    return v[i];
  };
  auto line = [&](const char* name, std::vector<double>& v) {
    double mx = v.empty() ? 0.0 : *std::max_element(v.begin(), v.end());
    double sum = 0;
    for (double x : v) sum += x;
    L("RESULT_TIMING %s n=%zu mean=%.1f p50=%.1f p90=%.1f p99=%.1f max=%.1f", name, v.size(),
      v.empty() ? 0.0 : sum / v.size(), pct(v, 0.5), pct(v, 0.9), pct(v, 0.99), mx);
  };
  line("present_interval_us", gapUs);
  line("present_call_us", callUs);
  line("present_to_screen_us", latUs);
  {
    size_t multi = 0;
    for (double r : syncGapUs) if (r >= 2.0) multi++;
    L("RESULT_TIMING screen_refresh_steps n=%zu two_or_more=%.2f%%", syncGapUs.size(),
      syncGapUs.empty() ? 0.0 : 100.0 * multi / syncGapUs.size());
  }
  if (exclusive) sc->SetFullscreenState(FALSE, nullptr);
  if (setW && setH) ChangeDisplaySettingsExW(outName, nullptr, nullptr, 0, nullptr);
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
