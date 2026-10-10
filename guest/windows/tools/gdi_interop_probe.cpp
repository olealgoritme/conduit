// gdi_interop_probe.cpp: GPU rendering into a GDI-compatible D3D11 texture,
// then GDI reading it through IDXGISurface1::GetDC, the way RichEdit
// (Notepad's text), MMC (Device Manager) and other GDI/D3D hybrids put their
// D3D-rendered pixels on screen: render with Direct2D, GetDC, BitBlt from the
// texture's DC, ReleaseDC.
//
// DXGI calls the driver's ResolveSharedResource on the texture before GDI
// touches it. GDI then reads the allocation (on this stack: the KMD copies the
// NVK image with the copy engine), so whatever GPU work the device still has
// in flight for the texture must be complete by the time GetDC returns.
//
// Each round: a GPU delay (N blended full-texture fills, so the work is still
// running when GetDC is called), white page, a line of text, EndDraw, GetDC,
// BitBlt into a DIB section, ReleaseDC, then count text pixels in the DIB.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o gdi_interop_probe.exe gdi_interop_probe.cpp \
//       -ld2d1 -ldwrite -ld3d11 -ldxgi -ldxguid -luuid -lole32 -lgdi32 -luser32 -ldwmapi
// Run (guest; `window` needs the interactive session, schtasks /it):
//   gdi_interop_probe.exe [rounds=8] [delay-fills=200] [warp] [appwait] [window]
//     appwait: wait for the GPU in the application before GetDC (control)
//     window:  BitBlt from the texture's DC into a redirected top-level window
//              (what RichEdit does with Notepad's text) and read the window
//              back from the screen after DWM composed it, instead of BitBlt
//              into a DIB section
//     gdidraw: GDI draws INTO the texture through its DC (a FillRect and a
//              PatBlt DSTINVERT, the read-modify-write kind: carets, selection,
//              GDI text on a D3D surface), then D3D reads the texture back
//              through a staging copy and checks both boxes went black
// Exit code 0 when GDI saw the text in every round.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d2d1_1.h>
#include <dwrite.h>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstdlib>
#include <cwchar>
#include <dwmapi.h>

static const UINT W = 1024, H = 128;
static const wchar_t kText[] = L"GDI reads what the GPU drew 0123";

static LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_ERASEBKGND) return 1;
  if (m == WM_PAINT) { ValidateRect(h, nullptr); return 0; }
  return DefWindowProcW(h, m, w, l);
}

static void Pump() {
  MSG msg;
  while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
}

static IDXGIAdapter1* HeliosAdapter() {
  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return nullptr;
  IDXGIAdapter1* a = nullptr; IDXGIAdapter1* chosen = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    if (!chosen && wcsstr(d.Description, L"Helios")) { chosen = a; continue; }
    a->Release();
  }
  f->Release();
  return chosen;
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const int rounds = argc > 1 ? atoi(argv[1]) : 8;
  const int fills = argc > 2 ? atoi(argv[2]) : 200;
  bool warp = false, appWait = false, window = false, gdiDraw = false;
  for (int i = 3; i < argc; ++i) {
    if (strcmp(argv[i], "warp") == 0) warp = true;
    // appwait: the application itself waits for the GPU before GetDC (what
    // the driver must do in ResolveSharedResource; shows the order fixes it).
    if (strcmp(argv[i], "appwait") == 0) appWait = true;
    if (strcmp(argv[i], "window") == 0) window = true;
    if (strcmp(argv[i], "gdidraw") == 0) gdiDraw = true;
  }
  CoInitializeEx(nullptr, COINIT_MULTITHREADED);

  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  HRESULT hr = D3D11CreateDevice(adapter, warp ? D3D_DRIVER_TYPE_WARP
                                 : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                                 nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                 D3D11_SDK_VERSION, &dev, nullptr, &ctx);
  printf("D3D11CreateDevice (%s) hr=0x%08lx\n", warp ? "warp" : "helios", hr);
  if (FAILED(hr)) return 2;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = W; td.Height = H; td.MipLevels = 1; td.ArraySize = 1; td.SampleDesc.Count = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_GDI_COMPATIBLE;
  ID3D11Texture2D* tex = nullptr;
  hr = dev->CreateTexture2D(&td, nullptr, &tex);
  if (FAILED(hr)) { printf("GDI-compatible CreateTexture2D hr=0x%08lx\n", hr); return 2; }
  IDXGISurface1* surf = nullptr;
  tex->QueryInterface(__uuidof(IDXGISurface1), (void**)&surf);

  ID2D1Factory1* d2f = nullptr;
  D2D1_FACTORY_OPTIONS fo{};
  D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory1), &fo, (void**)&d2f);
  IDXGIDevice* dx = nullptr; dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dx);
  ID2D1Device* d2dev = nullptr; d2f->CreateDevice(dx, &d2dev);
  ID2D1DeviceContext* dc = nullptr;
  d2dev->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &dc);
  D2D1_BITMAP_PROPERTIES1 bp{};
  bp.pixelFormat = D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_IGNORE);
  bp.dpiX = bp.dpiY = 96;
  bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_GDI_COMPATIBLE;
  ID2D1Bitmap1* target = nullptr;
  hr = dc->CreateBitmapFromDxgiSurface(surf, &bp, &target);
  if (FAILED(hr)) { printf("CreateBitmapFromDxgiSurface hr=0x%08lx\n", hr); return 2; }

  // A separate offscreen target for the GPU delay, on the same device.
  bp.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
  bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET;
  ID2D1Bitmap1* big = nullptr;
  dc->CreateBitmap(D2D1::SizeU(4096, 4096), nullptr, 0, &bp, &big);

  IDWriteFactory* dwf = nullptr;
  DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory), (IUnknown**)&dwf);
  IDWriteTextFormat* tf = nullptr;
  dwf->CreateTextFormat(L"Consolas", nullptr, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL, 20.f, L"en-us", &tf);
  ID2D1SolidColorBrush* black = nullptr; dc->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &black);
  ID2D1SolidColorBrush* tint = nullptr; dc->CreateSolidColorBrush(D2D1::ColorF(0.5f, 0.2f, 0.1f, 0.01f), &tint);

  HDC screen = GetDC(nullptr);
  HDC mem = CreateCompatibleDC(screen);
  BITMAPINFO bi{}; bi.bmiHeader.biSize = sizeof(bi.bmiHeader); bi.bmiHeader.biWidth = W;
  bi.bmiHeader.biHeight = -(LONG)H; bi.bmiHeader.biPlanes = 1; bi.bmiHeader.biBitCount = 32;
  void* bits = nullptr;
  HBITMAP dib = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &bits, nullptr, 0);
  SelectObject(mem, dib);

  // window: a plain redirected popup (GDI redirection surface, no DComp), topmost.
  HWND hwnd = nullptr;
  const int wx = 3800, wy = 300;
  if (window) {
    SetProcessDPIAware();
    WNDCLASSW wc{}; wc.lpfnWndProc = WndProc; wc.hInstance = GetModuleHandleW(nullptr);
    wc.lpszClassName = L"gdi_interop_probe"; wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
    RegisterClassW(&wc);
    hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_NOACTIVATE, wc.lpszClassName, L"gdi-interop",
                           WS_POPUP | WS_VISIBLE, wx, wy, W, H, nullptr, nullptr, wc.hInstance, nullptr);
    if (!hwnd) { printf("CreateWindowEx failed %lu\n", GetLastError()); return 2; }
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    for (int i = 0; i < 20; ++i) { Pump(); Sleep(25); }
  }

  ID3D11Texture2D* staging = nullptr;
  if (gdiDraw) {
    D3D11_TEXTURE2D_DESC sd = td;
    sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0; sd.MiscFlags = 0;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(dev->CreateTexture2D(&sd, nullptr, &staging))) { printf("staging failed\n"); return 2; }
  }

  int bad = 0;
  for (int r = 0; r < rounds; ++r) {
    // Clear the DIB to mid grey so "GDI copied nothing" is visible as such.
    memset(bits, 0x80, (size_t)W * H * 4);
    if (fills > 0) {
      dc->SetTarget(big);
      dc->BeginDraw();
      for (int k = 0; k < fills; ++k) dc->FillRectangle(D2D1::RectF(0, 0, 4096, 4096), tint);
      dc->EndDraw();
    }
    dc->SetTarget(target);
    dc->BeginDraw();
    dc->Clear(D2D1::ColorF(1, 1, 1, 1));
    dc->SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    float x = 10.f + (float)(r % 4) * 40.f;
    dc->DrawText(kText, (UINT32)wcslen(kText), tf, D2D1::RectF(x, 40, (float)W - 10, 90), black);
    hr = dc->EndDraw();
    if (FAILED(hr)) { printf("EndDraw hr=0x%08lx\n", hr); return 3; }

    if (appWait) {
      D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
      ID3D11Query* q = nullptr; dev->CreateQuery(&qd, &q);
      ctx->End(q);
      BOOL done = FALSE;
      while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK || !done) Sleep(0);
      q->Release();
    }
    if (gdiDraw) {
      HDC gdc = nullptr;
      hr = surf->GetDC(FALSE, &gdc);
      if (FAILED(hr)) { printf("GetDC hr=0x%08lx\n", hr); return 3; }
      RECT fill{600, 20, 700, 108};
      FillRect(gdc, &fill, (HBRUSH)GetStockObject(BLACK_BRUSH));
      PatBlt(gdc, 720, 20, 100, 88, DSTINVERT);
      surf->ReleaseDC(nullptr);
      GdiFlush();
      // The KMD runs GDI's commands on its own engine; give them time before the NVK read (this
      // checks what GDI wrote, not the ordering).
      Sleep(200);
      ctx->CopyResource(staging, tex);
      D3D11_MAPPED_SUBRESOURCE m{};
      if (FAILED(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &m))) { printf("Map failed\n"); return 3; }
      auto black_in = [&](int x0, int y0, int x1, int y1) {
        int n = 0;
        for (int y = y0; y < y1; ++y)
          for (int x = x0; x < x1; ++x) {
            const uint8_t* px = (const uint8_t*)m.pData + (size_t)y * m.RowPitch + (size_t)x * 4;
            if (px[0] < 40 && px[1] < 40 && px[2] < 40) n++;
          }
        return n;
      };
      int nf = black_in(600, 20, 700, 108), ni = black_in(720, 20, 820, 108), all = 100 * 88;
      int page = black_in(900, 100, 1000, 128);
      ctx->Unmap(staging, 0);
      bool ok = nf > all * 9 / 10 && ni > all * 9 / 10 && page == 0;
      if (!ok) bad++;
      printf("round %d: GDI FillRect black px=%d/%d, PatBlt DSTINVERT black px=%d/%d, page black px=%d -> %s\n",
             r, nf, all, ni, all, page, ok ? "GDI writes landed" : "GDI WRITES MISSING");
      continue;
    }
    HDC tdc = nullptr;
    hr = surf->GetDC(FALSE, &tdc);
    if (FAILED(hr)) { printf("GetDC hr=0x%08lx\n", hr); return 3; }
    if (window) {
      // Mid grey first, so "GDI copied nothing" shows as grey on the screen too.
      HDC wdc = GetDC(hwnd);
      HBRUSH grey = CreateSolidBrush(RGB(0x80, 0x80, 0x80));
      RECT all{0, 0, (LONG)W, (LONG)H};
      FillRect(wdc, &all, grey);
      DeleteObject(grey);
      BitBlt(wdc, 0, 0, W, H, tdc, 0, 0, SRCCOPY);
      ReleaseDC(hwnd, wdc);
      surf->ReleaseDC(nullptr);
      GdiFlush();
      for (int i = 0; i < 6; ++i) { Pump(); DwmFlush(); Sleep(25); }
      BitBlt(mem, 0, 0, W, H, screen, wx, wy, SRCCOPY);
      GdiFlush();
    } else {
      BitBlt(mem, 0, 0, W, H, tdc, 0, 0, SRCCOPY);
      surf->ReleaseDC(nullptr);
      GdiFlush();
    }

    int white = 0, ink = 0, grey = 0;
    const uint8_t* p = (const uint8_t*)bits;
    for (UINT y = 0; y < H; ++y)
      for (UINT xx = 0; xx < W; ++xx) {
        const uint8_t* px = p + ((size_t)y * W + xx) * 4;
        int v = (px[0] + px[1] + px[2]) / 3;
        if (v > 240) white++;
        else if (v < 100) ink++;
        else if (px[0] >= 0x7e && px[0] <= 0x82 && px[1] >= 0x7e && px[1] <= 0x82 && px[2] >= 0x7e && px[2] <= 0x82) grey++;
      }
    bool ok = white > (int)(W * H / 2) && ink > 300;
    {
      // What GDI read, per round: gdi_interop_round<N>.bmp.
      char path[64]; snprintf(path, sizeof(path), "gdi_interop_round%d.bmp", r);
      if (FILE* fp = fopen(path, "wb")) {
        uint32_t sz = W * H * 4;
        uint8_t hdr[54] = {'B', 'M'};
        *(uint32_t*)(hdr + 2) = 54 + sz; *(uint32_t*)(hdr + 10) = 54; *(uint32_t*)(hdr + 14) = 40;
        *(int32_t*)(hdr + 18) = W; *(int32_t*)(hdr + 22) = -(int32_t)H;
        *(uint16_t*)(hdr + 26) = 1; *(uint16_t*)(hdr + 28) = 32; *(uint32_t*)(hdr + 34) = sz;
        fwrite(hdr, 1, 54, fp); fwrite(bits, 1, sz, fp); fclose(fp);
      }
    }
    if (!ok) bad++;
    printf("round %d: GDI read white=%d text=%d untouched=%d -> %s\n", r, white, ink, grey,
           ok ? "page and text" : (white > (int)(W * H / 2) ? "TEXT MISSING" : "PAGE MISSING"));
  }
  printf("%d of %d rounds wrong\n", bad, rounds);
  return bad ? 1 : 0;
}
