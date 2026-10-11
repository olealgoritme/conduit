// d3d11_partial_present: the partial redraws a Chromium/CEF window makes on mouse hover.
//
//   d3d11_partial_present.exe [SECONDS=20 [BUFFERS=2 [HOST=hwnd|dcomp [DIRTY=1|0 [FRONTEND=keep|0|1]]]]]
//
// A FLIP_SEQUENTIAL swap chain (for the window, or for composition through a DirectComposition
// visual as Chromium makes it) of BUFFERS (2 to 4) buffers, 640x480. The first BUFFERS frames
// draw the whole picture (a static grid) into each buffer. Every later frame draws ONLY a 48x48
// square at a new position and the grid back where the square was in the last BUFFERS frames,
// into the back buffer it gets, and presents with Present1(0, 0, {those rects}) (DIRTY=1) or
// Present(0, 0) (DIRTY=0). Outside those rects the application never writes again: it relies on
// flip-sequential buffers keeping their content, as Chromium does. (Restoring the last BUFFERS
// positions keeps the picture right whether or not DXGI also carries the unchanged region of a
// partial present forward.)
//
// Two checks, each counted per second:
//   * back buffer: right after each present, the new back buffer is read back (staging) and every
//     16th pixel outside the last two frames' squares must be the grid. Black or anything else
//     there is content the driver lost or never carried forward ("bb-bad");
//   * screen: every 15th frame the window's client area is captured from the composed desktop
//     (BitBlt of the screen DC) and the grid checked the same way, away from the square's last
//     four positions ("screen-bad"). This is what the user sees: black boxes and flicker show up
//     here.
// Exit code 0: both counts 0. Keep the window unobscured while it runs (the screen check reads
// the desktop).
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d11_partial_present.cpp
//   -o d3d11_partial_present.exe -ld3d11 -ldxgi -ldcomp -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dcomp.h>
#include <dxgi1_2.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

static const UINT kW = 640, kH = 480, kSq = 48;

static double now_ms() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1000.0 * double(t.QuadPart) / double(f.QuadPart);
}

template <class T> static void release(T *&p) {
  if (p) p->Release();
  p = nullptr;
}

#define CHECK(what, expr)                                                                          \
  do {                                                                                             \
    HRESULT hr_ = (expr);                                                                          \
    if (FAILED(hr_)) {                                                                             \
      std::printf("[FAIL] %s: 0x%08lx\n", what, static_cast<unsigned long>(hr_));                 \
      return 1;                                                                                    \
    }                                                                                              \
  } while (0)

// The static picture: a grid of 32-pixel cells in two colours with a 1-pixel line.
static UINT32 grid(UINT x, UINT y) {
  if (x % 32 == 0 || y % 32 == 0) return 0xff202020u;
  return ((x / 32 + y / 32) & 1) ? 0xff3070c0u : 0xffd0d0d0u;
}
static const UINT32 kSquare = 0xffe04020u;

// The square's top-left corner at frame n (n >= 1): a Lissajous path over the client area.
static POINT square_at(UINT n) {
  const double t = n * 0.05;
  POINT p;
  p.x = LONG((kW - kSq) * (0.5 + 0.5 * __builtin_sin(t * 1.3)));
  p.y = LONG((kH - kSq) * (0.5 + 0.5 * __builtin_sin(t * 0.7 + 1.0)));
  return p;
}

static bool near_any(UINT x, UINT y, const POINT *ps, int n, LONG margin) {
  for (int i = 0; i < n; i++)
    if (LONG(x) >= ps[i].x - margin && LONG(x) < ps[i].x + LONG(kSq) + margin && LONG(y) >= ps[i].y - margin &&
        LONG(y) < ps[i].y + LONG(kSq) + margin)
      return true;
  return false;
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) PostQuitMessage(0);
  return DefWindowProcW(h, m, w, l);
}

int main(int argc, char **argv) {
  const double seconds = argc > 1 ? std::atof(argv[1]) : 20.0;
  UINT buffers = argc > 2 ? UINT(std::atoi(argv[2])) : 2u;
  buffers = buffers < 2 ? 2 : buffers > 4 ? 4 : buffers;
  const bool dcomp = argc > 3 && !std::strcmp(argv[3], "dcomp");
  const bool dirty = argc > 4 ? std::atoi(argv[4]) != 0 : true;
  const char *frontend = argc > 5 ? argv[5] : "keep";
  if (!std::strcmp(frontend, "0") || !std::strcmp(frontend, "1")) SetEnvironmentVariableA("HELIOS_D3D11_FRONTEND", frontend);
  std::printf("d3d11_partial_present: %.0f s, %u buffers, %s, dirty rects %s, front end %s\n", seconds, buffers,
              dcomp ? "DirectComposition" : "HWND", dirty ? "on" : "off", frontend);

  WNDCLASSW wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"HeliosPartialPresent";
  wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassW(&wc);
  RECT r = {0, 0, LONG(kW), LONG(kH)};
  AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowExW(dcomp ? WS_EX_NOREDIRECTIONBITMAP : 0, L"HeliosPartialPresent", L"partial-present",
                              WS_OVERLAPPEDWINDOW | WS_VISIBLE, 80, 80, r.right - r.left, r.bottom - r.top, nullptr,
                              nullptr, wc.hInstance, nullptr);
  if (!hwnd) return 1;
  SetForegroundWindow(hwnd);

  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  CHECK("D3D11CreateDevice", D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                               D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1, D3D11_SDK_VERSION, &dev,
                                               nullptr, &ctx));
  IDXGIDevice *dxgi_dev = nullptr;
  CHECK("IDXGIDevice", dev->QueryInterface(__uuidof(IDXGIDevice), reinterpret_cast<void **>(&dxgi_dev)));
  IDXGIAdapter *adapter = nullptr;
  dxgi_dev->GetAdapter(&adapter);
  IDXGIFactory2 *f = nullptr;
  CHECK("IDXGIFactory2", adapter->GetParent(__uuidof(IDXGIFactory2), reinterpret_cast<void **>(&f)));

  DXGI_SWAP_CHAIN_DESC1 d = {};
  d.Width = kW;
  d.Height = kH;
  d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  d.BufferCount = buffers;
  d.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
  IDXGISwapChain1 *sc = nullptr;
  IDCompositionDevice *dc = nullptr;
  IDCompositionTarget *target = nullptr;
  IDCompositionVisual *visual = nullptr;
  if (dcomp) {
    d.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
    CHECK("CreateSwapChainForComposition", f->CreateSwapChainForComposition(dev, &d, nullptr, &sc));
    CHECK("DCompositionCreateDevice",
          DCompositionCreateDevice(dxgi_dev, __uuidof(IDCompositionDevice), reinterpret_cast<void **>(&dc)));
    CHECK("CreateTargetForHwnd", dc->CreateTargetForHwnd(hwnd, TRUE, &target));
    CHECK("CreateVisual", dc->CreateVisual(&visual));
    CHECK("SetContent", visual->SetContent(sc));
    CHECK("SetRoot", target->SetRoot(visual));
    CHECK("Commit", dc->Commit());
  } else {
    CHECK("CreateSwapChainForHwnd", f->CreateSwapChainForHwnd(dev, hwnd, &d, nullptr, nullptr, &sc));
  }

  D3D11_TEXTURE2D_DESC sd = {};
  sd.Width = kW;
  sd.Height = kH;
  sd.MipLevels = 1;
  sd.ArraySize = 1;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.Usage = D3D11_USAGE_STAGING;
  sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D *staging = nullptr;
  CHECK("staging", dev->CreateTexture2D(&sd, nullptr, &staging));

  std::vector<UINT32> full(size_t(kW) * kH), sq(kSq * kSq, kSquare), patch(kSq * kSq);
  for (UINT y = 0; y < kH; y++)
    for (UINT x = 0; x < kW; x++) full[size_t(y) * kW + x] = grid(x, y);

  HDC screen = GetDC(nullptr);
  HDC mem = CreateCompatibleDC(screen);
  BITMAPINFO bi = {};
  bi.bmiHeader.biSize = sizeof(bi.bmiHeader);
  bi.bmiHeader.biWidth = LONG(kW);
  bi.bmiHeader.biHeight = -LONG(kH);
  bi.bmiHeader.biPlanes = 1;
  bi.bmiHeader.biBitCount = 32;
  void *bits = nullptr;
  HBITMAP dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &bits, nullptr, 0);
  SelectObject(mem, dib);

  unsigned long long frames = 0, bb_bad = 0, screen_bad = 0, bb_checks = 0, screen_checks = 0;
  // The square's positions, newest first (x < 0: none yet).
  POINT hist[8];
  for (POINT &p : hist) p = POINT{-1000, -1000};
  const double t0 = now_ms();
  double next_report = t0 + 1000.0;
  bool quit = false;
  while (!quit && now_ms() - t0 < seconds * 1000.0) {
    MSG msg;
    while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) quit = true;
      TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
    ID3D11Texture2D *bb = nullptr;
    CHECK("GetBuffer", sc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb)));
    HRESULT hr;
    if (frames < buffers) {
      ctx->UpdateSubresource(bb, 0, nullptr, full.data(), kW * 4, 0);
      hr = sc->Present(0, 0);
    } else {
      const POINT now = square_at(UINT(frames));
      RECT rects[5];
      UINT nr = 0;
      // The grid back where the square was (the application's own redraw of the old rects).
      for (UINT i = 0; i < buffers; i++) {
        const POINT old = hist[i];
        if (old.x < 0) continue;
        for (UINT y = 0; y < kSq; y++)
          for (UINT x = 0; x < kSq; x++) patch[y * kSq + x] = grid(UINT(old.x) + x, UINT(old.y) + y);
        D3D11_BOX box = {UINT(old.x), UINT(old.y), 0, UINT(old.x) + kSq, UINT(old.y) + kSq, 1};
        ctx->UpdateSubresource(bb, 0, &box, patch.data(), kSq * 4, 0);
        rects[nr++] = RECT{old.x, old.y, old.x + LONG(kSq), old.y + LONG(kSq)};
      }
      D3D11_BOX box = {UINT(now.x), UINT(now.y), 0, UINT(now.x) + kSq, UINT(now.y) + kSq, 1};
      ctx->UpdateSubresource(bb, 0, &box, sq.data(), kSq * 4, 0);
      rects[nr++] = RECT{now.x, now.y, now.x + LONG(kSq), now.y + LONG(kSq)};
      for (int i = 7; i > 0; i--) hist[i] = hist[i - 1];
      hist[0] = now;
      if (dirty) {
        DXGI_PRESENT_PARAMETERS pp = {};
        pp.DirtyRectsCount = nr;
        pp.pDirtyRects = rects;
        hr = sc->Present1(0, 0, &pp);
      } else {
        hr = sc->Present(0, 0);
      }
    }
    release(bb);
    if (FAILED(hr)) {
      std::printf("[FAIL] Present: 0x%08lx\n", static_cast<unsigned long>(hr));
      return 1;
    }
    frames++;
    // The back buffer the application gets next: everything outside the recent squares must be the
    // grid (the frame it last presented from this buffer, kept, or carried forward). Checked once
    // every buffer was drawn whole.
    CHECK("GetBuffer (check)", sc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb)));
    ctx->CopyResource(staging, bb);
    release(bb);
    D3D11_MAPPED_SUBRESOURCE m;
    if (frames >= buffers && SUCCEEDED(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) {
      bb_checks++;
      UINT bad = 0, bx = 0, by = 0;
      for (UINT y = 0; y < kH; y += 16)
        for (UINT x = 0; x < kW; x += 16) {
          if (near_any(x, y, hist, int(buffers) + 2, 1)) continue;
          if (static_cast<const UINT32 *>(m.pData)[size_t(y) * (m.RowPitch / 4) + x] != grid(x, y)) {
            if (!bad) bx = x, by = y;
            bad++;
          }
        }
      ctx->Unmap(staging, 0);
      if (bad) {
        if (bb_bad < 20)
          std::printf("[BB-BAD] frame %llu: %u sampled pixels outside the dirty rects differ, first at (%u,%u)\n",
                      frames, bad, bx, by);
        bb_bad++;
      }
    }
    // The composed desktop.
    if (frames % 15 == 0 && frames > buffers + 4) {
      Sleep(20);  // a composition or two
      POINT o = {0, 0};
      ClientToScreen(hwnd, &o);
      if (BitBlt(mem, 0, 0, int(kW), int(kH), screen, o.x, o.y, SRCCOPY)) {
        screen_checks++;
        POINT recent[6] = {hist[0], hist[1], hist[2], hist[3], square_at(UINT(frames)), square_at(UINT(frames + 1))};
        UINT bad = 0, bx = 0, by = 0, black = 0;
        for (UINT y = 0; y < kH; y += 16)
          for (UINT x = 0; x < kW; x += 16) {
            if (near_any(x, y, recent, 6, 2)) continue;
            const UINT32 got = static_cast<const UINT32 *>(bits)[size_t(y) * kW + x] | 0xff000000u;
            if (got != grid(x, y)) {
              if (!bad) bx = x, by = y;
              bad++;
              black += (got & 0xffffffu) == 0;
            }
          }
        if (bad) {
          if (screen_bad < 20)
            std::printf("[SCREEN-BAD] frame %llu: %u sampled pixels differ (%u black), first at (%u,%u)\n", frames,
                        bad, black, bx, by);
          screen_bad++;
        }
      }
    }
    if (now_ms() >= next_report) {
      next_report += 1000.0;
      std::printf("t=%3.0fs frames %llu bb-checks %llu bb-bad %llu screen-checks %llu screen-bad %llu\n",
                  (now_ms() - t0) / 1000.0, frames, bb_checks, bb_bad, screen_checks, screen_bad);
      std::fflush(stdout);
    }
  }
  std::printf("%s: frames %llu bb-checks %llu bb-bad %llu screen-checks %llu screen-bad %llu\n",
              bb_bad || screen_bad ? "[FAIL]" : "[PASS]", frames, bb_checks, bb_bad, screen_checks, screen_bad);
  DeleteObject(dib);
  DeleteDC(mem);
  ReleaseDC(nullptr, screen);
  release(visual);
  release(target);
  release(dc);
  release(staging);
  release(sc);
  release(f);
  release(adapter);
  release(dxgi_dev);
  release(ctx);
  release(dev);
  DestroyWindow(hwnd);
  return bb_bad || screen_bad ? 2 : 0;
}
