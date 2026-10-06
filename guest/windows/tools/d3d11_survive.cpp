// d3d11_survive: a long-lived D3D11 app that must keep presenting across a
// driver update or device restart (pnputil /restart-device): it renders and
// presents a flip-model swap chain at 60 Hz, and when the device is removed it
// recreates device and swap chain and goes on, as a well-behaved app (or DWM)
// does.
//
//   d3d11_survive.exe [SECONDS=120]
//
// One line per second: frames presented, the worst frame time, removals so
// far. On a removal: the reason, and how long until the first frame on the
// new device. Stops early on C:\Users\Public\s6\fast\d3d11_survive.stop.
// Whether the new device is NVK or Venus is in the process's UMD log
// ("icd backend: ...", librmclient's "generation N -> N+1" in
// helios_icd_diag.log); the run script greps both.
//
// Exit code 0 if it presented in the last 5 s of the run.
//
// Build: x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d11_survive.cpp
//          -o d3d11_survive.exe -ld3d11 -ldxgi -luser32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstdlib>

static double now_ms() {
  static LARGE_INTEGER f = [] { LARGE_INTEGER x; QueryPerformanceFrequency(&x); return x; }();
  LARGE_INTEGER t;
  QueryPerformanceCounter(&t);
  return 1000.0 * double(t.QuadPart) / double(f.QuadPart);
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcW(h, m, w, l); }

struct Gfx {
  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  IDXGISwapChain1 *sc = nullptr;
  void reset() {
    if (ctx) ctx->ClearState();
    if (sc) sc->Release();
    if (ctx) ctx->Release();
    if (dev) dev->Release();
    sc = nullptr;
    ctx = nullptr;
    dev = nullptr;
  }
  bool make(HWND hwnd) {
    IDXGIFactory2 *f = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory2), (void **)&f))) return false;
    IDXGIAdapter1 *a = nullptr;
    f->EnumAdapters1(0, &a);
    D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
    HRESULT hr = D3D11CreateDevice(a, D3D_DRIVER_TYPE_UNKNOWN, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1,
                                   D3D11_SDK_VERSION, &dev, nullptr, &ctx);
    DXGI_ADAPTER_DESC1 ad = {};
    if (a) {
      a->GetDesc1(&ad);
      a->Release();
    }
    if (FAILED(hr)) {
      f->Release();
      std::printf("  D3D11CreateDevice: 0x%08lx\n", (unsigned long)hr);
      return false;
    }
    DXGI_SWAP_CHAIN_DESC1 d = {};
    d.Width = 960;
    d.Height = 540;
    d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    d.SampleDesc.Count = 1;
    d.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    d.BufferCount = 2;
    d.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    hr = f->CreateSwapChainForHwnd(dev, hwnd, &d, nullptr, nullptr, &sc);
    f->Release();
    std::printf("  device on \"%ls\": swap chain 0x%08lx\n", ad.Description, (unsigned long)hr);
    return SUCCEEDED(hr);
  }
};

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const double seconds = argc > 1 ? std::atof(argv[1]) : 120.0;
  std::printf("d3d11_survive: %.0f s, pid %lu, HELIOS_ICD=%s\n", seconds, GetCurrentProcessId(),
              std::getenv("HELIOS_ICD") ? std::getenv("HELIOS_ICD") : "(unset)");
  WNDCLASSW wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"d3d11_survive";
  RegisterClassW(&wc);
  HWND hwnd = CreateWindowExW(0, wc.lpszClassName, L"d3d11_survive", WS_OVERLAPPEDWINDOW, 200, 200, 976, 579, nullptr,
                              nullptr, wc.hInstance, nullptr);
  ShowWindow(hwnd, SW_SHOW);

  Gfx g;
  while (!g.make(hwnd)) {
    g.reset();
    Sleep(500);
  }
  const double t0 = now_ms();
  double last_report = t0, last_frame = t0, removed_at = 0, worst = 0, last_present_ok = t0;
  unsigned frames = 0, total = 0, removals = 0, frame_no = 0;
  MSG msg;
  while (now_ms() - t0 < seconds * 1000.0) {
    while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
    if (GetFileAttributesA("C:\\Users\\Public\\s6\\fast\\d3d11_survive.stop") != INVALID_FILE_ATTRIBUTES) break;
    HRESULT hr = S_OK;
    if (g.sc) {
      ID3D11Texture2D *bb = nullptr;
      ID3D11RenderTargetView *rtv = nullptr;
      hr = g.sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb);
      if (SUCCEEDED(hr)) hr = g.dev->CreateRenderTargetView(bb, nullptr, &rtv);
      if (SUCCEEDED(hr)) {
        frame_no++;
        const float c[4] = {float(frame_no % 120) / 120.f, 0.3f, 1.f - float(frame_no % 120) / 120.f, 1.f};
        g.ctx->ClearRenderTargetView(rtv, c);
        hr = g.sc->Present(1, 0);
      }
      if (rtv) rtv->Release();
      if (bb) bb->Release();
    }
    if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET || hr == DXGI_ERROR_DEVICE_HUNG ||
        (g.dev && g.dev->GetDeviceRemovedReason() != S_OK) || !g.sc) {
      const HRESULT why = g.dev ? g.dev->GetDeviceRemovedReason() : hr;
      if (!removed_at) {
        removals++;
        removed_at = now_ms();
        std::printf("t=%.1f s: device removed (present 0x%08lx, reason 0x%08lx); recreating\n", (removed_at - t0) / 1000,
                    (unsigned long)hr, (unsigned long)why);
      }
      g.reset();
      if (!g.make(hwnd)) {
        g.reset();
        Sleep(250);
      }
      continue;
    }
    const double t = now_ms();
    if (removed_at && SUCCEEDED(hr)) {
      std::printf("t=%.1f s: first frame on the new device %.0f ms after the removal\n", (t - t0) / 1000, t - removed_at);
      removed_at = 0;
    }
    if (SUCCEEDED(hr)) {
      frames++;
      total++;
      last_present_ok = t;
    }
    if (t - last_frame > worst) worst = t - last_frame;
    last_frame = t;
    if (t - last_report >= 1000.0) {
      std::printf("t=%.0f s: %u frames, worst %.1f ms, removals %u\n", (t - t0) / 1000, frames, worst, removals);
      frames = 0;
      worst = 0;
      last_report = t;
    }
  }
  const bool ok = now_ms() - last_present_ok < 5000.0;
  std::printf("total %u frames, %u removal(s): %s\n", total, removals,
              ok ? "SURVIVE-OK (presenting at the end)" : "SURVIVE-FAIL (not presenting at the end)");
  g.reset();
  DestroyWindow(hwnd);
  return ok ? 0 : 1;
}
