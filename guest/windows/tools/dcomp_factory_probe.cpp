// dcomp_factory_probe.cpp: a DirectComposition surface rendered by one D3D11
// device and presented by another, the split Notepad's RichEdit and XAML use:
// the DComp device is created on device P (it presents, Present1 with the
// surfaces), the surface comes from IDCompositionDevice2::CreateSurfaceFactory
// on device R's Direct2D device, so BeginDraw hands out R's D2D context and R
// renders the pixels. The probe draws a white page and a line of text into a
// fresh surface, commits, waits, then captures its window from the screen (what
// DWM composed) and counts the ink.
//
// Optional GPU delay on R before the text (N full 4096x4096 blended fills) so
// R's work is still running when P presents. If the compositor reads the
// surface before R's GPU work has finished, the capture misses the text (or
// the whole page shows black, the fresh surface's memory).
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o dcomp_factory_probe.exe dcomp_factory_probe.cpp \
//       -ld2d1 -ldwrite -ld3d11 -ldxgi -ldcomp -ldxguid -luuid -lole32 -lgdi32 -luser32
// Run (interactive session, schtasks /it):
//   dcomp_factory_probe.exe <out-prefix> [delay-fills=0] [same] [ct]
//     same: the surface factory uses P's own D2D device (one device: control)
//     ct:   ClearType text (an opaque surface allows it; RichEdit uses it)
//     warm: P draws the same text first, so R's glyphs come from the glyph
//           cache P filled (Direct2D's per-factory cache, a shared texture)
// Exit code 0 when the text is visible, 1 when it is missing.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d2d1_1.h>
#include <dwrite.h>
#include <dcomp.h>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstdlib>
#include <cwchar>

static const int W = 900, H = 120;
static const wchar_t kText[] = L"Typed text in a surface factory surface 0123";

static LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
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

struct Gpu {
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  ID2D1Device* d2 = nullptr;
};

static bool MakeGpu(IDXGIAdapter1* adapter, ID2D1Factory1* f, Gpu& g) {
  HRESULT hr = D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                                 nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                 D3D11_SDK_VERSION, &g.dev, nullptr, &g.ctx);
  if (FAILED(hr)) { printf("D3D11CreateDevice hr=0x%08lx\n", hr); return false; }
  IDXGIDevice* dx = nullptr;
  g.dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dx);
  hr = f->CreateDevice(dx, &g.d2);
  dx->Release();
  return SUCCEEDED(hr);
}

int main(int argc, char** argv) {
  const char* out = argc > 1 ? argv[1] : "dcomp_factory";
  int fills = argc > 2 ? atoi(argv[2]) : 0;
  bool same = false, cleartype = false, warm = false;
  for (int i = 3; i < argc; ++i) {
    if (strcmp(argv[i], "same") == 0) same = true;
    if (strcmp(argv[i], "ct") == 0) cleartype = true;
    if (strcmp(argv[i], "warm") == 0) warm = true;
  }
  setvbuf(stdout, nullptr, _IONBF, 0);
  CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  SetProcessDPIAware();

  WNDCLASSW wc{}; wc.lpfnWndProc = WndProc; wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"dcompfactory"; wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassW(&wc);
  RECT wr{0, 0, W, H};
  AdjustWindowRectEx(&wr, WS_OVERLAPPEDWINDOW, FALSE, WS_EX_NOREDIRECTIONBITMAP);
  HWND hwnd = CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST, wc.lpszClassName, L"dcomp-factory",
                              WS_OVERLAPPEDWINDOW | WS_VISIBLE, 200, 420, wr.right - wr.left,
                              wr.bottom - wr.top, nullptr, nullptr, wc.hInstance, nullptr);
  Pump();

  IDXGIAdapter1* adapter = HeliosAdapter();
  ID2D1Factory1* d2f = nullptr;
  D2D1_FACTORY_OPTIONS fo{};
  D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, __uuidof(ID2D1Factory1), &fo, (void**)&d2f);
  Gpu P, R;
  if (!MakeGpu(adapter, d2f, P)) return 3;
  if (!same && !MakeGpu(adapter, d2f, R)) return 3;
  Gpu& rend = same ? P : R;
  printf("presenting device %p, rendering device %p (%s)\n", (void*)P.dev, (void*)rend.dev,
         same ? "same device" : "two devices");

  IDXGIDevice* pdx = nullptr;
  P.dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&pdx);
  IDCompositionDesktopDevice* dc = nullptr;
  HRESULT hr = DCompositionCreateDevice3(pdx, __uuidof(IDCompositionDesktopDevice), (void**)&dc);
  printf("DCompositionCreateDevice3 hr=0x%08lx\n", hr);
  if (FAILED(hr)) return 4;
  IDCompositionTarget* target = nullptr;
  dc->CreateTargetForHwnd(hwnd, TRUE, &target);
  IDCompositionVisual2* visual = nullptr;
  dc->CreateVisual(&visual);
  IDCompositionSurfaceFactory* factory = nullptr;
  hr = dc->CreateSurfaceFactory(rend.d2, &factory);
  printf("CreateSurfaceFactory hr=0x%08lx\n", hr);
  if (FAILED(hr)) return 5;
  IDCompositionSurface* surf = nullptr;
  hr = factory->CreateSurface(W, H, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_IGNORE, &surf);
  if (FAILED(hr)) { printf("CreateSurface hr=0x%08lx\n", hr); return 5; }
  visual->SetContent(surf);
  target->SetRoot(visual);

  IDWriteFactory* dwf = nullptr;
  DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory), (IUnknown**)&dwf);
  IDWriteTextFormat* tf = nullptr;
  dwf->CreateTextFormat(L"Consolas", nullptr, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL, 15.f, L"en-us", &tf);

  if (warm) {
    // P draws the same text first (offscreen), so the glyphs land in the
    // factory's glyph cache from P before R draws them: Direct2D keeps one
    // glyph cache per factory, a shared texture that the other devices open.
    ID2D1DeviceContext* wd = nullptr;
    P.d2->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &wd);
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED);
    bp.dpiX = bp.dpiY = 96; bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET;
    ID2D1Bitmap1* wb = nullptr;
    wd->CreateBitmap(D2D1::SizeU(W, H), nullptr, 0, &bp, &wb);
    wd->SetTarget(wb);
    ID2D1SolidColorBrush* br = nullptr;
    wd->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &br);
    wd->BeginDraw();
    wd->Clear(D2D1::ColorF(1, 1, 1, 1));
    wd->SetTextAntialiasMode(cleartype ? D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE
                                       : D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    wd->DrawText(kText, (UINT32)wcslen(kText), tf, D2D1::RectF(10, 40, W - 10, 80), br);
    wd->EndDraw();
    P.ctx->Flush();
    br->Release(); wd->SetTarget(nullptr); wb->Release(); wd->Release();
  }
  if (fills > 0) {
    // Keep the rendering device's GPU busy so its text lands late.
    ID2D1DeviceContext* hd = nullptr;
    rend.d2->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &hd);
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED);
    bp.dpiX = bp.dpiY = 96; bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET;
    ID2D1Bitmap1* big = nullptr;
    hd->CreateBitmap(D2D1::SizeU(4096, 4096), nullptr, 0, &bp, &big);
    hd->SetTarget(big);
    ID2D1SolidColorBrush* tb = nullptr;
    hd->CreateSolidColorBrush(D2D1::ColorF(0.5f, 0.2f, 0.1f, 0.01f), &tb);
    hd->BeginDraw();
    for (int k = 0; k < fills; ++k) hd->FillRectangle(D2D1::RectF(0, 0, 4096, 4096), tb);
    hd->EndDraw();
    tb->Release(); hd->SetTarget(nullptr); big->Release(); hd->Release();
  }

  POINT off{};
  ID2D1DeviceContext* d2 = nullptr;
  hr = surf->BeginDraw(nullptr, __uuidof(ID2D1DeviceContext), (void**)&d2, &off);
  if (FAILED(hr)) { printf("BeginDraw hr=0x%08lx\n", hr); return 6; }
  d2->SetTransform(D2D1::Matrix3x2F::Translation((float)off.x, (float)off.y));
  d2->Clear(D2D1::ColorF(1, 1, 1, 1));
  ID2D1SolidColorBrush* black = nullptr;
  d2->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &black);
  d2->SetTextAntialiasMode(cleartype ? D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE
                                     : D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
  d2->DrawText(kText, (UINT32)wcslen(kText), tf, D2D1::RectF(10, 40, W - 10, 80), black);
  black->Release(); d2->Release();
  surf->EndDraw();
  dc->Commit();
  // Nothing changes from here on: whatever DWM read at this commit stays.
  for (int t = 0; t < 20; ++t) { Pump(); Sleep(50); }

  POINT org{0, 0}; ClientToScreen(hwnd, &org);
  HDC sdc = GetDC(nullptr); HDC mdc = CreateCompatibleDC(sdc);
  BITMAPINFO bi{}; bi.bmiHeader.biSize = sizeof(bi.bmiHeader); bi.bmiHeader.biWidth = W;
  bi.bmiHeader.biHeight = -H; bi.bmiHeader.biPlanes = 1; bi.bmiHeader.biBitCount = 32;
  void* bits = nullptr;
  HBITMAP hb = CreateDIBSection(sdc, &bi, DIB_RGB_COLORS, &bits, nullptr, 0);
  SelectObject(mdc, hb);
  BitBlt(mdc, 0, 0, W, H, sdc, org.x, org.y, SRCCOPY | CAPTUREBLT);
  GdiFlush();
  const uint8_t* p = (const uint8_t*)bits;
  int ink = 0, black_px = 0, white_px = 0;
  for (int y = 0; y < H; ++y)
    for (int x = 0; x < W; ++x) {
      const uint8_t* px = p + (y * W + x) * 4;
      int v = (px[0] + px[1] + px[2]) / 3;
      if (y >= 40 && y < 80 && v < 128) ink++;
      if (v < 16) black_px++;
      if (v > 240) white_px++;
    }
  char path[512]; snprintf(path, sizeof(path), "%s.bmp", out);
  if (FILE* fp = fopen(path, "wb")) {
    uint8_t hdr[54] = {'B', 'M'};
    *(uint32_t*)(hdr + 2) = 54 + W * H * 4; *(uint32_t*)(hdr + 10) = 54; *(uint32_t*)(hdr + 14) = 40;
    *(int32_t*)(hdr + 18) = W; *(int32_t*)(hdr + 22) = -H; *(uint16_t*)(hdr + 26) = 1;
    *(uint16_t*)(hdr + 28) = 32; *(uint32_t*)(hdr + 34) = W * H * 4;
    fwrite(hdr, 1, 54, fp); fwrite(p, 1, W * H * 4, fp); fclose(fp);
  }
  bool page = white_px > W * H / 2;
  bool text = ink > 500;
  printf("page white px=%d black px=%d text ink px=%d -> page %s, text %s\n", white_px, black_px, ink,
         page ? "VISIBLE" : "MISSING", text ? "VISIBLE" : "MISSING");
  DestroyWindow(hwnd);
  return page && text ? 0 : 1;
}
