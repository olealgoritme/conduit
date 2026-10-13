// d3d11_blt_reuse: the windowed legacy-blt present of the 3DMark launcher, with an objective check
// for the black areas and flicker it showed.
//
//   d3d11_blt_reuse.exe [SECONDS=15 [EFFECT=discard|sequential|flip [BUFFERS=1 [SYNC=0 [FRONTEND=keep|0|1]]]]]
//
// The launcher's D3D11 device presents a 1384x981 BGRA swap chain in the blt model: one back buffer,
// Present1 into DWM's redirection surface (the KMD's Blt copies the back buffer there), and every
// frame starts with a ClearRenderTargetView of that same buffer to transparent black before the page
// is drawn again. Only the order of the GPU work keeps the Blt of frame N ahead of the clear of
// frame N+1; when the clear (or a half-drawn frame) wins, the window shows black tiles and flickers.
//
// Here, every frame on the GPU: ClearRenderTargetView(0,0,0,0), a CopySubresourceRegion of a static
// grid (no black pixel anywhere in it) from a texture over the whole back buffer, and a moving square,
// then Present(SYNC, 0). Nothing in the loop reads back or waits, so the next frame's clear is
// queued right behind the present, as in the launcher.
//
// A second thread captures the window's client area from the composed desktop (BitBlt of the screen
// DC, what the user sees) as fast as it can and samples every 8th pixel of every 8th row: a sample that is neither the grid nor the square (black: the clear) is a bad capture. EFFECT=flip uses
// a FLIP_SEQUENTIAL swap chain (BUFFERS >= 2) for comparison.
//
// Exit code 0: no bad capture; 2: at least one. Keep the window unobscured while it runs.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d11_blt_reuse.cpp
//   -o d3d11_blt_reuse.exe -ld3d11 -ldxgi -luser32 -lgdi32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

static const UINT kW = 1280, kH = 900, kSq = 64;

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

// The static picture: 32-pixel cells in two colours with a dark (not black) 1-pixel line.
static UINT32 grid(UINT x, UINT y) {
  if (x % 32 == 0 || y % 32 == 0) return 0xff404040u;
  return ((x / 32 + y / 32) & 1) ? 0xff3070c0u : 0xffd0d0d0u;
}

static const UINT32 kSquare = 0xffe04020u;

static POINT square_at(unsigned long long n) {
  const double t = double(n) * 0.02;
  POINT p;
  p.x = LONG((kW - kSq) * (0.5 + 0.5 * __builtin_sin(t * 1.3)));
  p.y = LONG((kH - kSq) * (0.5 + 0.5 * __builtin_sin(t * 0.7 + 1.0)));
  return p;
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) PostQuitMessage(0);
  return DefWindowProcW(h, m, w, l);
}

struct Capture {
  std::atomic<bool> stop{false};
  std::atomic<unsigned long long> frames{0};  // presents so far (written by the render thread)
  unsigned long long checks = 0, bad = 0, black = 0;
};

static void capture_loop(HWND hwnd, Capture *c) {
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
  // Let the first frames reach the screen before judging it.
  while (!c->stop.load() && c->frames.load() < 30) Sleep(5);
  Sleep(300);
  while (!c->stop.load()) {
    POINT o = {0, 0};
    ClientToScreen(hwnd, &o);
    const unsigned long long f0 = c->frames.load();
    if (!BitBlt(mem, 0, 0, int(kW), int(kH), screen, o.x, o.y, SRCCOPY)) {
      Sleep(2);
      continue;
    }
    const unsigned long long f1 = c->frames.load();
    c->checks++;
    UINT bad = 0, black = 0, bx = 0, by = 0;
    for (UINT y = 4; y < kH; y += 8)
      for (UINT x = 4; x < kW; x += 8) {
        const UINT32 got = static_cast<const UINT32 *>(bits)[size_t(y) * kW + x] | 0xff000000u;
        if (got != grid(x, y) && got != kSquare) {
          if (!bad) bx = x, by = y;
          bad++;
          black += (got & 0xffffffu) == 0;
        }
      }
    if (bad) {
      if (c->bad < 20)
        std::printf("[SCREEN-BAD] capture %llu (frames %llu..%llu): %u sampled pixels differ (%u black), first at "
                    "(%u,%u)\n",
                    c->checks, f0, f1, bad, black, bx, by);
      c->bad++;
      if (black) c->black++;
    }
  }
  DeleteObject(dib);
  DeleteDC(mem);
  ReleaseDC(nullptr, screen);
}

int main(int argc, char **argv) {
  const double seconds = argc > 1 ? std::atof(argv[1]) : 15.0;
  const char *effect = argc > 2 ? argv[2] : "discard";
  UINT buffers = argc > 3 ? UINT(std::atoi(argv[3])) : 1u;
  const UINT sync = argc > 4 ? UINT(std::atoi(argv[4])) : 0u;
  const char *frontend = argc > 5 ? argv[5] : "keep";
  if (!std::strcmp(frontend, "0") || !std::strcmp(frontend, "1")) SetEnvironmentVariableA("HELIOS_D3D11_FRONTEND", frontend);
  const bool flip = !std::strcmp(effect, "flip");
  const bool sequential = !std::strcmp(effect, "sequential");
  if (flip && buffers < 2) buffers = 2;
  if (buffers < 1) buffers = 1;
  if (buffers > 4) buffers = 4;
  std::printf("d3d11_blt_reuse: %.0f s, %ux%u, %s, %u buffer(s), sync %u, front end %s\n", seconds, kW, kH,
              flip ? "FLIP_SEQUENTIAL" : sequential ? "SEQUENTIAL (blt)" : "DISCARD (blt)", buffers, sync, frontend);

  WNDCLASSW wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"HeliosBltReuse";
  wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassW(&wc);
  RECT r = {0, 0, LONG(kW), LONG(kH)};
  AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowExW(0, L"HeliosBltReuse", L"blt-reuse", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 40, 40,
                              r.right - r.left, r.bottom - r.top, nullptr, nullptr, wc.hInstance, nullptr);
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
  d.SwapEffect = flip ? DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL
                      : sequential ? DXGI_SWAP_EFFECT_SEQUENTIAL : DXGI_SWAP_EFFECT_DISCARD;
  IDXGISwapChain1 *sc = nullptr;
  CHECK("CreateSwapChainForHwnd", f->CreateSwapChainForHwnd(dev, hwnd, &d, nullptr, nullptr, &sc));

  std::vector<UINT32> full(size_t(kW) * kH), sq(size_t(kSq) * kSq, kSquare);
  for (UINT y = 0; y < kH; y++)
    for (UINT x = 0; x < kW; x++) full[size_t(y) * kW + x] = grid(x, y);
  D3D11_TEXTURE2D_DESC td = {};
  td.Width = kW;
  td.Height = kH;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
  D3D11_SUBRESOURCE_DATA init = {full.data(), kW * 4, 0};
  ID3D11Texture2D *picture = nullptr;
  CHECK("picture", dev->CreateTexture2D(&td, &init, &picture));
  td.Width = td.Height = kSq;
  D3D11_SUBRESOURCE_DATA sinit = {sq.data(), kSq * 4, 0};
  ID3D11Texture2D *square = nullptr;
  CHECK("square", dev->CreateTexture2D(&td, &sinit, &square));

  Capture cap;
  std::thread capture(capture_loop, hwnd, &cap);
  const float clear[4] = {0, 0, 0, 0};
  const double t0 = now_ms();
  double next_report = t0 + 1000.0;
  unsigned long long frames = 0;
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
    ID3D11RenderTargetView *rtv = nullptr;
    CHECK("CreateRenderTargetView", dev->CreateRenderTargetView(bb, nullptr, &rtv));
    // What the launcher does first every frame: the whole back buffer to transparent black.
    ctx->ClearRenderTargetView(rtv, clear);
    ctx->CopySubresourceRegion(bb, 0, 0, 0, 0, picture, 0, nullptr);
    const POINT p = square_at(frames);
    ctx->CopySubresourceRegion(bb, 0, UINT(p.x), UINT(p.y), 0, square, 0, nullptr);
    release(rtv);
    release(bb);
    const HRESULT hr = sc->Present(sync, 0);
    if (FAILED(hr)) {
      std::printf("[FAIL] Present: 0x%08lx\n", static_cast<unsigned long>(hr));
      break;
    }
    frames++;
    cap.frames.store(frames);
    if (now_ms() >= next_report) {
      next_report += 1000.0;
      std::printf("t=%3.0fs frames %llu captures %llu bad %llu (black %llu)\n", (now_ms() - t0) / 1000.0, frames,
                  cap.checks, cap.bad, cap.black);
      std::fflush(stdout);
    }
  }
  cap.stop.store(true);
  capture.join();
  std::printf("%s: frames %llu captures %llu bad %llu (black %llu)\n", cap.bad ? "[FAIL]" : "[PASS]", frames,
              cap.checks, cap.bad, cap.black);
  release(square);
  release(picture);
  release(sc);
  release(f);
  release(adapter);
  release(dxgi_dev);
  release(ctx);
  release(dev);
  DestroyWindow(hwnd);
  return cap.bad ? 2 : 0;
}
