// d3d11_kmd_restart_survivor: a long-lived D3D11 process across a KMD restart, as explorer.exe
// and the shell hosts are across a live driver update or a display-device restart.
//
//   d3d11_kmd_restart_survivor.exe [SECONDS=120 [PERIOD_MS=3000]]
//
// Every PERIOD_MS it makes a fresh device (the old one is released; after a KMD restart the old
// one is removed anyway), a small window with a FLIP_SEQUENTIAL swap chain, and a shared
// render-target texture (MISC_SHARED: an allocation DWM or another process can open), presents
// three frames and reports what the UMD made of the surfaces. Restart the display device while it
// runs (pnputil /restart-device on the Helios adapter, or a live driver install) and compare the
// iterations before and after: on NVK every surface should get a KMD resource id again after the
// restart. The UMD log (C:\ProgramData\Helios\umd-<pid>.log) has the "nvk resource id: vr=..."
// line of every surface; NVK's own warnings go to stderr.
//
// Per iteration it prints the device creation result, the removed reason of the previous device
// and whether presenting worked. Exit code 0 when every iteration presented.
//
// Build (one line): x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d11_kmd_restart_survivor.cpp
//   -o d3d11_kmd_restart_survivor.exe -ld3d11 -ldxgi -luser32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <cstdio>
#include <cstdlib>

template <class T> static void release(T *&p) {
  if (p) p->Release();
  p = nullptr;
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcW(h, m, w, l); }

int main(int argc, char **argv) {
  const double seconds = argc > 1 ? std::atof(argv[1]) : 120.0;
  const DWORD period = argc > 2 ? DWORD(std::atoi(argv[2])) : 3000;
  WNDCLASSW wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"HeliosKmdRestartSurvivor";
  RegisterClassW(&wc);
  const ULONGLONG t0 = GetTickCount64();
  unsigned iter = 0, failed = 0;
  ID3D11Device *prev = nullptr;
  while (GetTickCount64() - t0 < ULONGLONG(seconds * 1000.0)) {
    iter++;
    if (prev) {
      const HRESULT why = prev->GetDeviceRemovedReason();
      if (why != S_OK) std::printf("#%u previous device removed: 0x%08lx\n", iter, static_cast<unsigned long>(why));
      release(prev);
    }
    ID3D11Device *dev = nullptr;
    ID3D11DeviceContext *ctx = nullptr;
    D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1,
                                   D3D11_SDK_VERSION, &dev, nullptr, &ctx);
    if (FAILED(hr)) {
      std::printf("#%u D3D11CreateDevice: 0x%08lx\n", iter, static_cast<unsigned long>(hr));
      failed++;
      Sleep(period);
      continue;
    }
    D3D11_TEXTURE2D_DESC d = {};
    d.Width = 256;
    d.Height = 256;
    d.MipLevels = 1;
    d.ArraySize = 1;
    d.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    d.SampleDesc.Count = 1;
    d.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    d.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
    ID3D11Texture2D *shared = nullptr;
    const HRESULT hs = dev->CreateTexture2D(&d, nullptr, &shared);
    HWND hwnd = CreateWindowExW(0, wc.lpszClassName, L"kmd-restart-survivor", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 40, 40,
                                320, 240, nullptr, nullptr, wc.hInstance, nullptr);
    IDXGIDevice *dd = nullptr;
    IDXGIAdapter *ad = nullptr;
    IDXGIFactory2 *f = nullptr;
    dev->QueryInterface(__uuidof(IDXGIDevice), reinterpret_cast<void **>(&dd));
    if (dd) dd->GetAdapter(&ad);
    if (ad) ad->GetParent(__uuidof(IDXGIFactory2), reinterpret_cast<void **>(&f));
    DXGI_SWAP_CHAIN_DESC1 sd = {};
    sd.Width = 300;
    sd.Height = 200;
    sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.BufferCount = 2;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
    IDXGISwapChain1 *sc = nullptr;
    HRESULT hc = f ? f->CreateSwapChainForHwnd(dev, hwnd, &sd, nullptr, nullptr, &sc) : E_FAIL;
    HRESULT hp = E_FAIL;
    if (sc) {
      for (int i = 0; i < 3; i++) {
        ID3D11Texture2D *bb = nullptr;
        ID3D11RenderTargetView *rtv = nullptr;
        sc->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void **>(&bb));
        if (bb) dev->CreateRenderTargetView(bb, nullptr, &rtv);
        const float c[4] = {0.1f * float(iter % 10), 0.5f, 0.2f, 1.0f};
        if (rtv) ctx->ClearRenderTargetView(rtv, c);
        release(rtv);
        release(bb);
        hp = sc->Present(0, 0);
        MSG msg;
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
      }
    }
    std::printf("#%u t=%llus device ok, shared texture 0x%08lx, swap chain 0x%08lx, present 0x%08lx\n", iter,
                (GetTickCount64() - t0) / 1000, static_cast<unsigned long>(hs), static_cast<unsigned long>(hc),
                static_cast<unsigned long>(hp));
    std::fflush(stdout);
    if (FAILED(hp)) failed++;
    release(sc);
    release(f);
    release(ad);
    release(dd);
    release(shared);
    ctx->ClearState();
    ctx->Flush();
    release(ctx);
    DestroyWindow(hwnd);
    prev = dev;
    Sleep(period);
  }
  release(prev);
  std::printf("%s: %u iterations, %u failed\n", failed ? "[FAIL]" : "[PASS]", iter, failed);
  return failed ? 2 : 0;
}
