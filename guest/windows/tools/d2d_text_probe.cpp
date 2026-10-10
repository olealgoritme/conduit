// d2d_text_probe.cpp: draw DirectWrite text with Direct2D into an offscreen
// D3D11 texture and read it back, to compare a driver against WARP.
//
// Each row is one (font, size, antialias mode) case. Small sizes go through
// D2D's glyph atlas (DirectWrite rasterises glyph bitmaps on the CPU, D2D
// uploads them into a texture and draws one quad per glyph); large sizes are
// drawn as outlines. The probe writes a BMP of the target and, per row, the
// ink bounding box and the number of inked columns, which separates "glyphs
// collapsed to bars" from "glyphs right".
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d2d_text_probe.exe d2d_text_probe.cpp \
//       -ld2d1 -ldwrite -ld3d11 -ldxgi -ldxguid -luuid -lole32
// Run (guest, any session; no window):
//   d2d_text_probe.exe <helios|warp|default> <out-prefix> [dc|rt]
//     dc = ID2D1DeviceContext on a DXGI-surface bitmap (what XAML/RichEdit use)
//     rt = ID2D1Factory::CreateDxgiSurfaceRenderTarget (D2D 1.0 path)
//     a8 = as dc, on an A8_UNORM target (a glyph-atlas page)
//   HELIOS_ICD=nvk / HELIOS_ICD=venus choose the driver for the helios adapter.
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
#include <vector>

static const UINT W = 1024, H = 900;

struct Case {
  const wchar_t* font;
  float size;
  D2D1_TEXT_ANTIALIAS_MODE aa;
  const wchar_t* text;
  const char* name;
};

static const Case kCases[] = {
  {L"Segoe UI", 15.f, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, L"Hello typed text 0123", "segoe15-cleartype"},
  {L"Segoe UI", 15.f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"Hello typed text 0123", "segoe15-gray"},
  {L"Segoe UI", 15.f, D2D1_TEXT_ANTIALIAS_MODE_ALIASED, L"Hello typed text 0123", "segoe15-aliased"},
  {L"Consolas", 15.f, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, L"Hello typed text 0123", "consolas15-cleartype"},
  {L"Consolas", 15.f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"Hello typed text 0123", "consolas15-gray"},
  {L"Segoe UI Variable Text", 14.7f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"Hello typed text 0123", "segoevar-gray"},
  {L"Segoe Fluent Icons", 10.f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"\xE921 \xE922 \xE8BB", "fluent10-gray"},
  {L"Segoe Fluent Icons", 10.f, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, L"\xE921 \xE922 \xE8BB", "fluent10-cleartype"},
  {L"Segoe UI", 30.f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"Hello typed text 0123", "segoe30-gray"},
  {L"Segoe UI", 80.f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, L"Hello big", "segoe80-gray"},
};

static IDXGIAdapter1* PickAdapter(const char* sel) {
  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return nullptr;
  IDXGIAdapter1* a = nullptr; IDXGIAdapter1* chosen = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    printf("adapter %u: %ls\n", i, d.Description);
    if (!chosen && wcsstr(d.Description, L"Helios")) { chosen = a; continue; }
    a->Release();
  }
  f->Release();
  return chosen;
}

static void WriteBmp(const char* path, const uint8_t* bgra, UINT pitch) {
  FILE* fp = fopen(path, "wb");
  if (!fp) return;
  uint32_t imgsz = W * H * 4;
  uint8_t hdr[54] = {'B', 'M'};
  *(uint32_t*)(hdr + 2) = 54 + imgsz;
  *(uint32_t*)(hdr + 10) = 54;
  *(uint32_t*)(hdr + 14) = 40;
  *(int32_t*)(hdr + 18) = W;
  *(int32_t*)(hdr + 22) = -(int32_t)H;
  *(uint16_t*)(hdr + 26) = 1;
  *(uint16_t*)(hdr + 28) = 32;
  *(uint32_t*)(hdr + 34) = imgsz;
  fwrite(hdr, 1, 54, fp);
  for (UINT y = 0; y < H; ++y) fwrite(bgra + y * pitch, 1, W * 4, fp);
  fclose(fp);
}

int main(int argc, char** argv) {
  const char* sel = argc > 1 ? argv[1] : "helios";
  const char* out = argc > 2 ? argv[2] : "d2d_text";
  bool useRt = argc > 3 && strcmp(argv[3], "rt") == 0;
  // a8: the target is DXGI_FORMAT_A8_UNORM, as a glyph-atlas page that XAML
  // draws glyph runs into (D2D rasterizes the outlines on the GPU then).
  bool a8 = argc > 3 && strcmp(argv[3], "a8") == 0;
  const DXGI_FORMAT tfmt = a8 ? DXGI_FORMAT_A8_UNORM : DXGI_FORMAT_B8G8R8A8_UNORM;
  setvbuf(stdout, nullptr, _IONBF, 0);
  CoInitializeEx(nullptr, COINIT_MULTITHREADED);

  IDXGIAdapter1* adapter = nullptr;
  D3D_DRIVER_TYPE dt = D3D_DRIVER_TYPE_HARDWARE;
  if (strcmp(sel, "warp") == 0) dt = D3D_DRIVER_TYPE_WARP;
  else if (strcmp(sel, "helios") == 0) {
    adapter = PickAdapter(sel);
    if (!adapter) { printf("no Helios adapter\n"); return 2; }
    dt = D3D_DRIVER_TYPE_UNKNOWN;
  }
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL fl;
  HRESULT hr = D3D11CreateDevice(adapter, dt, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                 nullptr, 0, D3D11_SDK_VERSION, &dev, &fl, &ctx);
  printf("D3D11CreateDevice hr=0x%08lx fl=0x%x\n", hr, fl);
  if (FAILED(hr)) return 3;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = W; td.Height = H; td.MipLevels = 1; td.ArraySize = 1;
  td.Format = tfmt; td.SampleDesc.Count = 1;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  // PROBE_SHARED=1: a MISC_SHARED target, like a DirectComposition surface
  // (shared surfaces stay uncompressed on NVK).
  if (getenv("PROBE_SHARED")) td.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
  ID3D11Texture2D* tex = nullptr;
  if (FAILED(dev->CreateTexture2D(&td, nullptr, &tex))) return 4;
  IDXGISurface* surf = nullptr;
  tex->QueryInterface(__uuidof(IDXGISurface), (void**)&surf);

  ID2D1Factory1* d2f = nullptr;
  D2D1_FACTORY_OPTIONS fo{}; fo.debugLevel = D2D1_DEBUG_LEVEL_NONE;
  hr = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory1), &fo, (void**)&d2f);
  if (FAILED(hr)) { printf("D2D1CreateFactory hr=0x%08lx\n", hr); return 5; }
  IDWriteFactory* dwf = nullptr;
  DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory), (IUnknown**)&dwf);

  ID2D1RenderTarget* rt = nullptr;
  ID2D1DeviceContext* dc = nullptr;
  if (useRt) {
    D2D1_RENDER_TARGET_PROPERTIES rp = D2D1::RenderTargetProperties(
        D2D1_RENDER_TARGET_TYPE_HARDWARE,
        D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED), 96, 96);
    hr = d2f->CreateDxgiSurfaceRenderTarget(surf, &rp, &rt);
    printf("CreateDxgiSurfaceRenderTarget hr=0x%08lx\n", hr);
    if (FAILED(hr)) return 6;
  } else {
    IDXGIDevice* dxgiDev = nullptr;
    dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgiDev);
    ID2D1Device* d2dev = nullptr;
    hr = d2f->CreateDevice(dxgiDev, &d2dev);
    printf("ID2D1Factory1::CreateDevice hr=0x%08lx\n", hr);
    if (FAILED(hr)) return 6;
    d2dev->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &dc);
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = D2D1::PixelFormat(tfmt, D2D1_ALPHA_MODE_PREMULTIPLIED);
    bp.dpiX = bp.dpiY = 96;
    bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW;
    ID2D1Bitmap1* bmp = nullptr;
    hr = dc->CreateBitmapFromDxgiSurface(surf, &bp, &bmp);
    printf("CreateBitmapFromDxgiSurface hr=0x%08lx\n", hr);
    if (FAILED(hr)) return 7;
    dc->SetTarget(bmp);
    rt = dc;
  }

  ID2D1SolidColorBrush* black = nullptr;
  rt->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &black);
  const UINT ncases = sizeof(kCases) / sizeof(kCases[0]);
  float rowY[32]; float rowH[32];
  // PROBE_REPEAT_MS: keep redrawing for that long (per-pass driver dumps).
  int reps = 1;
  if (const char* r = getenv("PROBE_REPEAT_MS")) reps = 1 + atoi(r) / 100;
  for (int rep = 0; rep < reps; ++rep) {
  if (rep) Sleep(100);
  rt->BeginDraw();
  rt->Clear(a8 ? D2D1::ColorF(0, 0, 0, 0) : D2D1::ColorF(1, 1, 1, 1));
  float y = 4;
  for (UINT i = 0; i < ncases; ++i) {
    const Case& c = kCases[i];
    IDWriteTextFormat* tf = nullptr;
    dwf->CreateTextFormat(c.font, nullptr, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                          DWRITE_FONT_STRETCH_NORMAL, c.size, L"en-us", &tf);
    rt->SetTextAntialiasMode(c.aa);
    float h = c.size * 1.6f + 6;
    rowY[i] = y; rowH[i] = h;
    rt->DrawText(c.text, (UINT32)wcslen(c.text), tf, D2D1::RectF(8, y, W - 8, y + h), black);
    tf->Release();
    y += h;
  }
  hr = rt->EndDraw();
  }
  printf("EndDraw hr=0x%08lx\n", hr);

  td.Usage = D3D11_USAGE_STAGING; td.BindFlags = 0; td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  td.MiscFlags = 0;
  ID3D11Texture2D* st = nullptr;
  dev->CreateTexture2D(&td, nullptr, &st);
  ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{};
  hr = ctx->Map(st, 0, D3D11_MAP_READ, 0, &m);
  if (FAILED(hr)) { printf("Map hr=0x%08lx\n", hr); return 8; }
  const uint8_t* src = (const uint8_t*)m.pData;
  std::vector<uint8_t> expanded;
  const uint8_t* p = src;
  if (a8) {
    // White page, ink = coverage, so the stats and the BMP read the same way.
    expanded.resize((size_t)W * H * 4);
    for (UINT yy = 0; yy < H; ++yy)
      for (UINT x = 0; x < W; ++x) {
        uint8_t v = 255 - src[yy * m.RowPitch + x];
        uint8_t* d = &expanded[((size_t)yy * W + x) * 4];
        d[0] = d[1] = d[2] = v; d[3] = 255;
      }
    p = expanded.data();
    m.RowPitch = W * 4;
  }
  char path[512];
  snprintf(path, sizeof(path), "%s.bmp", out);
  WriteBmp(path, p, m.RowPitch);
  for (UINT i = 0; i < ncases; ++i) {
    UINT y0 = (UINT)rowY[i], y1 = (UINT)(rowY[i] + rowH[i]);
    if (y1 > H) y1 = H;
    int minx = W, maxx = -1, cols = 0; uint64_t ink = 0; int colorful = 0;
    for (UINT x = 0; x < W; ++x) {
      bool any = false;
      for (UINT yy = y0; yy < y1; ++yy) {
        const uint8_t* px = p + yy * m.RowPitch + x * 4;
        int d = 255 - (px[0] + px[1] + px[2]) / 3;
        if (d > 32) { any = true; ink += d; }
        int mx = px[0] > px[1] ? (px[0] > px[2] ? px[0] : px[2]) : (px[1] > px[2] ? px[1] : px[2]);
        int mn = px[0] < px[1] ? (px[0] < px[2] ? px[0] : px[2]) : (px[1] < px[2] ? px[1] : px[2]);
        if (mx - mn > 24) colorful++;
      }
      if (any) { cols++; if ((int)x < minx) minx = x; if ((int)x > maxx) maxx = x; }
    }
    printf("row %-22s inkcols=%4d x=[%d,%d] ink=%llu colorpx=%d\n", kCases[i].name, cols, minx,
           maxx, (unsigned long long)ink, colorful);
  }
  ctx->Unmap(st, 0);
  printf("done\n");
  return 0;
}
