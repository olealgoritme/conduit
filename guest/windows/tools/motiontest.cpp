// motiontest.cpp: a white bar sliding at constant speed (position from QPC time) in a borderless
// topmost window covering the output: flip model, 3 buffers, ALLOW_TEARING, Present(0,
// ALLOW_TEARING), the presentation of an uncapped game (PresentMon: "Hardware: Independent Flip").
// The vehicle for frame delivery through the immediate-flip path (guest KMD to host viewer):
// compare what the application presented (the per-phase line below, or PresentMon) with what the
// viewer received (its per-second "stats: commit N fps" lines).
//
// Phases (10 s each), the phase number drawn as N small squares in the top-left corner:
//   1) even 300 fps;
//   2) CS2-like irregular pacing: a frame every 2.37 ms, and once per 15.625 ms one frame 4.33 ms
//      longer (about 300 fps, frame times 2.37 / 6.70 ms);
//   3) even 240 fps.
// Each phase's pacing clocks start at the phase's first frame. (An earlier copy kept phase 2's
// 15.625 ms clock at the program start, so phase 2 spent its first 7.5 s catching up with one
// long frame after every short one: 149 fps at an even 6.70 ms, which a viewer on a 240 Hz
// display shows at alternating 4.17 / 8.33 ms.)
//
// At each phase end one line: frames, rate, and the application's own present-to-present
// intervals (p10 / p50 / p90 / max, and how many were longer than 5 ms).
//
// Build on the Linux host (mingw-w64):
//   x86_64-w64-mingw32-g++ -O2 -static -o motiontest.exe motiontest.cpp -ld3d11 -ldxgi -lwinmm
// Run in the interactive session (schtasks /IT; session 0 has no desktop). Esc quits.
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_5.h>
#include <stdio.h>
#include <stdlib.h>
#include <math.h>
#include <vector>
#include <algorithm>
#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "winmm.lib")

static LARGE_INTEGER F;
static double now() {
  LARGE_INTEGER c;
  QueryPerformanceCounter(&c);
  return (double)c.QuadPart / F.QuadPart;
}
static void waitUntil(double t) {
  while (now() < t) {
    if (t - now() > 0.002) Sleep(1);
  }
}
static LRESULT CALLBACK wp(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_KEYDOWN && w == VK_ESCAPE) PostQuitMessage(0);
  return DefWindowProcW(h, m, w, l);
}

static void report(int phase, int frames, double secs, std::vector<double> &iv) {
  std::sort(iv.begin(), iv.end());
  size_t n = iv.size();
  size_t longer = 0;
  for (double d : iv)
    if (d > 5.0) longer++;
  if (n == 0) {
    printf("phase %d: %d frames in %.1f s\n", phase + 1, frames, secs);
  } else {
    printf("phase %d: %d frames in %.1f s (%.0f fps), present interval ms p10 %.2f p50 %.2f p90 %.2f max %.2f, >5 ms %zu\n",
           phase + 1, frames, secs, frames / secs, iv[n / 10], iv[n / 2], iv[n * 9 / 10], iv[n - 1], longer);
  }
  fflush(stdout);
  iv.clear();
}

int main() {
  timeBeginPeriod(1);
  QueryPerformanceFrequency(&F);
  int W = GetSystemMetrics(SM_CXSCREEN), H = GetSystemMetrics(SM_CYSCREEN);
  WNDCLASSW wc = {0};
  wc.lpfnWndProc = wp;
  wc.hInstance = GetModuleHandleW(NULL);
  wc.lpszClassName = L"motiontest";
  wc.hCursor = NULL;
  RegisterClassW(&wc);
  HWND hw = CreateWindowExW(WS_EX_TOPMOST, L"motiontest", L"motiontest", WS_POPUP | WS_VISIBLE, 0, 0, W, H, NULL, NULL,
                            wc.hInstance, NULL);
  ShowCursor(FALSE);
  SetForegroundWindow(hw);
  ID3D11Device *dev;
  ID3D11DeviceContext *ctx;
  D3D_FEATURE_LEVEL fl;
  if (FAILED(D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &dev, &fl, &ctx))) {
    printf("device failed\n");
    return 1;
  }
  IDXGIDevice *xd;
  dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&xd);
  IDXGIAdapter *ad;
  xd->GetAdapter(&ad);
  IDXGIFactory2 *fac;
  ad->GetParent(__uuidof(IDXGIFactory2), (void **)&fac);
  DXGI_SWAP_CHAIN_DESC1 sd = {0};
  sd.Width = W;
  sd.Height = H;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = 3;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  sd.Flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
  IDXGISwapChain1 *sc;
  if (FAILED(fac->CreateSwapChainForHwnd(dev, hw, &sd, NULL, NULL, &sc))) {
    printf("swapchain failed\n");
    return 1;
  }
  fac->MakeWindowAssociation(hw, DXGI_MWA_NO_ALT_ENTER);
  D3D11_TEXTURE2D_DESC td = {0};
  td.Width = 64;
  td.Height = H;
  td.MipLevels = 1;
  td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_IMMUTABLE;
  td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
  unsigned *px = (unsigned *)malloc(64 * H * 4);
  for (int i = 0; i < 64 * H; i++) px[i] = 0xffffffff;
  D3D11_SUBRESOURCE_DATA srd = {px, 64 * 4, 0};
  ID3D11Texture2D *bar;
  dev->CreateTexture2D(&td, &srd, &bar);

  const double speed = 2000.0;  // px per second
  const double tStart = now();
  double next = tStart, lastTick = tStart, phaseStart = tStart, lastPresent = 0;
  int frames = 0, phase = 0;
  std::vector<double> iv;
  iv.reserve(4096);
  MSG msg;
  bool quit = false;
  while (!quit) {
    while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) quit = true;
      DispatchMessageW(&msg);
    }
    double t = now();
    int ph = (int)((t - tStart) / 10.0);
    if (ph >= 3) break;
    if (ph != phase) {
      report(phase, frames, t - phaseStart, iv);
      phase = ph;
      frames = 0;
      phaseStart = t;
      // Every pacing clock starts at the phase's first frame.
      next = t;
      lastTick = t;
    }
    ID3D11Texture2D *bb;
    sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb);
    ID3D11RenderTargetView *rtv;
    dev->CreateRenderTargetView(bb, NULL, &rtv);
    float black[4] = {0, 0, 0, 1};
    ctx->ClearRenderTargetView(rtv, black);
    int x = (int)fmod(t * speed, (double)(W - 64));
    ctx->CopySubresourceRegion(bb, 0, x, 0, 0, bar, 0, NULL);
    for (int i = 0; i <= phase; i++) {
      D3D11_BOX b = {0, 0, 0, 24, 24, 1};
      ctx->CopySubresourceRegion(bb, 0, 20 + i * 40, 20, 0, bar, 0, &b);
    }
    rtv->Release();
    bb->Release();
    sc->Present(0, DXGI_PRESENT_ALLOW_TEARING);
    double tp = now();
    if (frames > 0) iv.push_back((tp - lastPresent) * 1000.0);
    lastPresent = tp;
    frames++;
    if (phase == 0) {
      next += 1.0 / 300;
    } else if (phase == 1) {
      next += 0.00237;
      // At most one long frame per 15.625 ms period; a period missed entirely is skipped, never
      // made up with a run of long frames.
      if (now() - lastTick >= 0.015625) {
        lastTick += 0.015625;
        if (now() - lastTick >= 0.015625) lastTick = now();
        next += 0.00433;
      }
    } else {
      next += 1.0 / 240;
    }
    waitUntil(next);
    if (now() - next > 0.05) next = now();
  }
  report(phase, frames, now() - phaseStart, iv);
  return 0;
}
