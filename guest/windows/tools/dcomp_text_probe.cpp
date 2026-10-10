// dcomp_text_probe.cpp: "typing" into a DirectComposition surface, the way a
// windowless edit control (RichEdit in Notepad, XAML text) updates its
// content: one BeginDraw per character with an update rectangle around the
// new glyph only, Direct2D text inside, EndDraw, Commit. Row 1 is typed that
// way, row 2 is drawn in one BeginDraw over the whole surface afterwards
// (the reference). The probe then captures its own window from the screen
// (what DWM composed) and compares the two rows column by column.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o dcomp_text_probe.exe dcomp_text_probe.cpp \
//       -ld2d1 -ldwrite -ld3d11 -ldxgi -ldcomp -ldxguid -luuid -lole32 -lgdi32 -luser32
// Run in the interactive session (schtasks /it), with the driver chosen as
// usual (HELIOS_ICD=nvk|venus):
//   dcomp_text_probe.exe <out-prefix> [hold-seconds] [update=char|line|full] [mode=d2d|dxgi] [stress]
//     update: the update rectangle of each typed character: the glyph cell
//             (char), the whole row (line) or the whole surface (full).
//     mode:   BeginDraw returns an ID2D1DeviceContext (d2d) or a DXGI surface
//             that the probe wraps in a D2D bitmap itself (dxgi).
//     stress: N full 4096x4096 blended fills queued on the GPU just before the
//             caption surfaces are drawn (0 = none; 300 is about 25 ms).
// Output: <out-prefix>.bmp (the captured window), per-row ink columns, and
// "MATCH" or "MISMATCH" for typed vs reference row.
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
#include <vector>

static const int W = 900, H = 160;
static const float kSize = 15.f;
static const wchar_t kText[] = L"Hello typed text 0123 abcxyz";

static LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProcW(h, m, w, l);
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

static void Pump() {
  MSG msg;
  while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
}

struct Ctx {
  IDCompositionDevice* dc = nullptr;
  IDCompositionSurface* surf = nullptr;
  ID2D1Device* d2dev = nullptr;
  ID2D1Factory1* d2f = nullptr;
  IDWriteTextFormat* tf = nullptr;
  bool dxgiMode = false;
};

// One BeginDraw/EndDraw on `rect` (surface coordinates); draws the first `n`
// characters of kText on row `row` clipped to the update rectangle.
static bool Draw(Ctx& c, const RECT* rect, int row, int n, bool clearAll) {
  POINT off{};
  ID2D1DeviceContext* d2 = nullptr;
  ID2D1Bitmap1* bmp = nullptr;
  HRESULT hr;
  if (!c.dxgiMode) {
    hr = c.surf->BeginDraw(rect, __uuidof(ID2D1DeviceContext), (void**)&d2, &off);
    if (FAILED(hr)) { printf("BeginDraw(d2d) hr=0x%08lx\n", hr); return false; }
  } else {
    IDXGISurface* ds = nullptr;
    hr = c.surf->BeginDraw(rect, __uuidof(IDXGISurface), (void**)&ds, &off);
    if (FAILED(hr)) { printf("BeginDraw(dxgi) hr=0x%08lx\n", hr); return false; }
    c.d2dev->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &d2);
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED);
    bp.dpiX = bp.dpiY = 96;
    bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW;
    hr = d2->CreateBitmapFromDxgiSurface(ds, &bp, &bmp);
    ds->Release();
    if (FAILED(hr)) { printf("CreateBitmapFromDxgiSurface hr=0x%08lx\n", hr); return false; }
    d2->SetTarget(bmp);
    d2->BeginDraw();
  }
  // D2D coordinates: surface coordinates of the update rect map to `off`.
  float ox = (float)off.x - (rect ? rect->left : 0);
  float oy = (float)off.y - (rect ? rect->top : 0);
  d2->SetTransform(D2D1::Matrix3x2F::Translation(ox, oy));
  RECT r = rect ? *rect : RECT{0, 0, W, H};
  d2->PushAxisAlignedClip(D2D1::RectF((float)r.left, (float)r.top, (float)r.right, (float)r.bottom),
                          D2D1_ANTIALIAS_MODE_ALIASED);
  ID2D1SolidColorBrush* white = nullptr; ID2D1SolidColorBrush* black = nullptr;
  d2->CreateSolidColorBrush(D2D1::ColorF(1, 1, 1, 1), &white);
  d2->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &black);
  if (clearAll) d2->Clear(D2D1::ColorF(1, 1, 1, 1));
  else d2->FillRectangle(D2D1::RectF((float)r.left, (float)r.top, (float)r.right, (float)r.bottom), white);
  d2->SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
  float y = 20.f + row * 50.f;
  if (n > 0) d2->DrawText(kText, n, c.tf, D2D1::RectF(10, y, W - 10, y + 40), black);
  d2->PopAxisAlignedClip();
  white->Release(); black->Release();
  if (c.dxgiMode) { d2->EndDraw(); d2->SetTarget(nullptr); bmp->Release(); }
  d2->Release();
  hr = c.surf->EndDraw();
  if (FAILED(hr)) { printf("EndDraw hr=0x%08lx\n", hr); return false; }
  c.dc->Commit();
  c.dc->WaitForCommitCompletion();
  return true;
}

int main(int argc, char** argv) {
  const char* out = argc > 1 ? argv[1] : "dcomp_text";
  int hold = argc > 2 ? atoi(argv[2]) : 2;
  const char* update = argc > 3 ? argv[3] : "char";
  Ctx c;
  c.dxgiMode = argc > 4 && strcmp(argv[4], "dxgi") == 0;
  int stress = argc > 5 ? atoi(argv[5]) : 0;
  setvbuf(stdout, nullptr, _IONBF, 0);
  CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  SetProcessDPIAware();

  WNDCLASSW wc{}; wc.lpfnWndProc = WndProc; wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"dcomptext"; wc.hCursor = LoadCursor(nullptr, IDC_ARROW);
  RegisterClassW(&wc);
  RECT wr{0, 0, W, H};
  AdjustWindowRectEx(&wr, WS_OVERLAPPEDWINDOW, FALSE, WS_EX_NOREDIRECTIONBITMAP);
  HWND hwnd = CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST, wc.lpszClassName, L"dcomp-text",
                              WS_OVERLAPPEDWINDOW | WS_VISIBLE, 200, 200, wr.right - wr.left,
                              wr.bottom - wr.top, nullptr, nullptr, wc.hInstance, nullptr);
  SetForegroundWindow(hwnd);
  Pump();

  IDXGIAdapter1* adapter = HeliosAdapter();
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  HRESULT hr = D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                                 nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                 D3D11_SDK_VERSION, &dev, nullptr, &ctx);
  printf("D3D11CreateDevice hr=0x%08lx adapter=%s\n", hr, adapter ? "helios" : "default");
  if (FAILED(hr)) return 3;
  IDXGIDevice* dxgiDev = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgiDev);
  D2D1_FACTORY_OPTIONS fo{};
  D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory1), &fo, (void**)&c.d2f);
  c.d2f->CreateDevice(dxgiDev, &c.d2dev);
  hr = DCompositionCreateDevice2(c.d2dev, __uuidof(IDCompositionDevice), (void**)&c.dc);
  printf("DCompositionCreateDevice2 hr=0x%08lx\n", hr);
  if (FAILED(hr)) return 4;
  IDCompositionTarget* target = nullptr;
  c.dc->CreateTargetForHwnd(hwnd, TRUE, &target);
  IDCompositionVisual* visual = nullptr;
  c.dc->CreateVisual(&visual);
  hr = c.dc->CreateSurface(W, H, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED, &c.surf);
  printf("CreateSurface hr=0x%08lx\n", hr);
  if (FAILED(hr)) return 5;
  visual->SetContent(c.surf);
  target->SetRoot(visual);

  IDWriteFactory* dwf = nullptr;
  DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory), (IUnknown**)&dwf);
  dwf->CreateTextFormat(L"Consolas", nullptr, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL, kSize, L"en-us", &c.tf);
  // Consolas is monospaced: one advance per character.
  IDWriteTextLayout* tl = nullptr;
  dwf->CreateTextLayout(L"M", 1, c.tf, 100, 100, &tl);
  DWRITE_TEXT_METRICS tm{}; tl->GetMetrics(&tm); tl->Release();
  float adv = tm.widthIncludingTrailingWhitespace;
  printf("advance=%.3f\n", adv);

  if (!Draw(c, nullptr, 99, 0, true)) return 6;
  Pump();
  const int n = (int)wcslen(kText);
  for (int i = 1; i <= n; ++i) {
    RECT r;
    if (strcmp(update, "full") == 0) r = RECT{0, 0, W, H};
    else if (strcmp(update, "line") == 0) r = RECT{0, 10, W, 60};
    else {
      int x0 = (int)(10 + (i - 1) * adv) - 1;
      r = RECT{x0, 14, (int)(x0 + adv + 3), 44};
    }
    if (!Draw(c, &r, 0, i, false)) return 7;
    Pump();
    Sleep(40);
  }
  // Reference row: one draw of row 1, update rect = that row only.
  RECT r2{0, 60, W, 110};
  if (!Draw(c, &r2, 1, n, false)) return 8;
  // Caption buttons: three 64x32 surfaces, cleared transparent, one Segoe
  // Fluent Icons glyph each (minimize, maximize, close), as child visuals
  // at the top right, the way a caption bar draws them.
  const wchar_t kIcons[3] = {0xE921, 0xE922, 0xE8BB};
  IDWriteTextFormat* itf = nullptr;
  dwf->CreateTextFormat(L"Segoe Fluent Icons", nullptr, DWRITE_FONT_WEIGHT_NORMAL,
                        DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 10.f, L"en-us", &itf);
  itf->SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
  itf->SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
  // stress: about 20-30 ms of GPU work queued right before the caption
  // surfaces are drawn, so their pixels land well after the Commit returns.
  // If the compositor reads a surface before the producer's GPU work for it
  // has finished, and nothing else changes, the stale pixels stay on screen.
  if (stress) {
    ID2D1DeviceContext* hd = nullptr;
    c.d2dev->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &hd);
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = D2D1::PixelFormat(DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_PREMULTIPLIED);
    bp.dpiX = bp.dpiY = 96;
    bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET;
    ID2D1Bitmap1* big = nullptr;
    hd->CreateBitmap(D2D1::SizeU(4096, 4096), nullptr, 0, &bp, &big);
    hd->SetTarget(big);
    ID2D1SolidColorBrush* tb = nullptr;
    hd->CreateSolidColorBrush(D2D1::ColorF(0.5f, 0.2f, 0.1f, 0.01f), &tb);
    hd->BeginDraw();
    for (int k = 0; k < stress; ++k) hd->FillRectangle(D2D1::RectF(0, 0, 4096, 4096), tb);
    hd->EndDraw();
    tb->Release(); hd->SetTarget(nullptr); big->Release(); hd->Release();
  }
  for (int i = 0; i < 3; ++i) {
    IDCompositionSurface* cs = nullptr;
    hr = c.dc->CreateSurface(64, 32, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED, &cs);
    if (FAILED(hr)) { printf("caption CreateSurface hr=0x%08lx\n", hr); return 9; }
    POINT off{}; ID2D1DeviceContext* d2 = nullptr;
    hr = cs->BeginDraw(nullptr, __uuidof(ID2D1DeviceContext), (void**)&d2, &off);
    if (FAILED(hr)) { printf("caption BeginDraw hr=0x%08lx\n", hr); return 9; }
    d2->SetTransform(D2D1::Matrix3x2F::Translation((float)off.x, (float)off.y));
    d2->Clear(D2D1::ColorF(0, 0, 0, 0));
    ID2D1SolidColorBrush* br = nullptr;
    d2->CreateSolidColorBrush(D2D1::ColorF(0, 0, 0, 1), &br);
    d2->SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    d2->DrawText(&kIcons[i], 1, itf, D2D1::RectF(0, 0, 64, 32), br);
    br->Release(); d2->Release();
    cs->EndDraw();
    IDCompositionVisual* v = nullptr;
    c.dc->CreateVisual(&v);
    v->SetContent(cs);
    v->SetOffsetX((float)(W - 3 * 64 + i * 64));
    v->SetOffsetY(110.f);
    visual->AddVisual(v, TRUE, nullptr);
  }
  c.dc->Commit();
  {
    // How long the GPU still needs after the Commit (the stress window).
    D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
    ID3D11Query* q = nullptr;
    dev->CreateQuery(&qd, &q);
    ctx->End(q);
    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f); QueryPerformanceCounter(&t0);
    BOOL done = FALSE;
    while (ctx->GetData(q, &done, sizeof(done), 0) != S_OK || !done) Sleep(0);
    QueryPerformanceCounter(&t1);
    printf("GPU idle %.2f ms after the caption Commit\n", (t1.QuadPart - t0.QuadPart) * 1000.0 / f.QuadPart);
    q->Release();
  }
  c.dc->WaitForCommitCompletion();
  for (int t = 0; t < 10; ++t) { Pump(); Sleep(50); }

  // Capture what DWM shows of the client area.
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
  char path[512]; snprintf(path, sizeof(path), "%s.bmp", out);
  FILE* fp = fopen(path, "wb");
  if (fp) {
    uint8_t hdr[54] = {'B', 'M'};
    *(uint32_t*)(hdr + 2) = 54 + W * H * 4; *(uint32_t*)(hdr + 10) = 54; *(uint32_t*)(hdr + 14) = 40;
    *(int32_t*)(hdr + 18) = W; *(int32_t*)(hdr + 22) = -H; *(uint16_t*)(hdr + 26) = 1;
    *(uint16_t*)(hdr + 28) = 32; *(uint32_t*)(hdr + 34) = W * H * 4;
    fwrite(hdr, 1, 54, fp); fwrite(p, 1, W * H * 4, fp); fclose(fp);
  }
  int diff = 0, cols[2] = {0, 0}; uint64_t ink[2] = {0, 0};
  for (int x = 0; x < W; ++x) {
    for (int row = 0; row < 2; ++row) {
      bool any = false;
      for (int y = 0; y < 40; ++y) {
        const uint8_t* px = p + ((14 + row * 50 + y) * W + x) * 4;
        int d = 255 - (px[0] + px[1] + px[2]) / 3;
        if (d > 32) { any = true; ink[row] += d; }
        if (row == 0) {
          const uint8_t* q = p + ((14 + 50 + y) * W + x) * 4;
          int e = 255 - (q[0] + q[1] + q[2]) / 3;
          if (abs(d - e) > 48) diff++;
        }
      }
      if (any) cols[row]++;
    }
  }
  printf("typed: inkcols=%d ink=%llu  reference: inkcols=%d ink=%llu  differing px=%d -> %s\n",
         cols[0], (unsigned long long)ink[0], cols[1], (unsigned long long)ink[1], diff,
         diff < 20 ? "MATCH" : "MISMATCH");
  for (int i = 0; i < 3; ++i) {
    int inked = 0;
    for (int y = 110; y < 142; ++y)
      for (int x = W - 3 * 64 + i * 64; x < W - 3 * 64 + (i + 1) * 64; ++x) {
        const uint8_t* px = p + (y * W + x) * 4;
        if (255 - (px[0] + px[1] + px[2]) / 3 > 64) inked++;
      }
    printf("caption glyph %d: inked px=%d -> %s\n", i, inked, inked > 4 ? "VISIBLE" : "MISSING");
  }
  for (int t = 0; t < hold * 20; ++t) { Pump(); Sleep(50); }
  DestroyWindow(hwnd);
  return 0;
}
