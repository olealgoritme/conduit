// d3d9_windowed.exe [window|argb|scaled|borderless] [LOG_PATH]
// Native D3D9 rendering and composed-screen verification. Keep the window visible.
// Build: i686-w64-mingw32-g++ -std=c++17 -O2 -static d3d9_windowed.cpp
//   -o d3d9_windowed.exe -ld3d9 -lgdi32 -luser32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d9.h>
#include <cstdio>
#include <cstring>

#define CHECK(expr) do { \
  const HRESULT hr = (expr); \
  if (FAILED(hr)) { \
    std::printf("FAIL %s: 0x%08lx\n", #expr, static_cast<unsigned long>(hr)); \
    return 1; \
  } \
} while (0)

int main(int argc, char **argv) {
  if (argc > 2 && !std::freopen(argv[2], "w", stdout)) return 1;
  std::setvbuf(stdout, nullptr, _IONBF, 0);
  const char *mode = argc > 1 ? argv[1] : "window";
  const bool borderless = std::strcmp(mode, "borderless") == 0;
  const bool scaled = std::strcmp(mode, "scaled") == 0;
  const D3DFORMAT format = std::strcmp(mode, "argb") == 0 ? D3DFMT_A8R8G8B8 : D3DFMT_X8R8G8B8;
  SetProcessDPIAware();
  // Direct scanout bypasses DWM, whose image GetPixel samples below.
  SetEnvironmentVariableA("HELIOS_NVK_PRESENT", "2");
  const int width = borderless ? GetSystemMetrics(SM_CXSCREEN) : 640;
  const int height = borderless ? GetSystemMetrics(SM_CYSCREEN) : 450;
  WNDCLASSW wc = {};
  wc.lpfnWndProc = DefWindowProcW;
  wc.hInstance = GetModuleHandleW(nullptr);
  wc.lpszClassName = L"NativeD3D9WindowedProbe";
  RegisterClassW(&wc);
  const DWORD style = borderless ? WS_POPUP : WS_OVERLAPPEDWINDOW;
  RECT rect = {0, 0, width, height};
  AdjustWindowRect(&rect, style, FALSE);
  HWND window = CreateWindowExW(WS_EX_TOPMOST, wc.lpszClassName, L"Native D3D9 windowed",
      style | WS_VISIBLE, borderless ? 0 : 100, borderless ? 0 : 100,
      rect.right - rect.left, rect.bottom - rect.top, nullptr, nullptr, wc.hInstance, nullptr);
  if (!window) return 1;
  SetForegroundWindow(window);

  IDirect3D9 *d3d = Direct3DCreate9(D3D_SDK_VERSION);
  if (!d3d) return 1;
  D3DADAPTER_IDENTIFIER9 adapter = {};
  CHECK(d3d->GetAdapterIdentifier(D3DADAPTER_DEFAULT, 0, &adapter));
  wchar_t module[MAX_PATH] = {};
  GetModuleFileNameW(GetModuleHandleW(L"d3d9.dll"), module, MAX_PATH);
  std::printf("mode=%s adapter=%s runtime=%ls\n", mode, adapter.Description, module);
  D3DPRESENT_PARAMETERS pp = {};
  pp.BackBufferWidth = scaled ? width / 2 : width;
  pp.BackBufferHeight = scaled ? height / 2 : height;
  pp.BackBufferFormat = format;
  pp.BackBufferCount = 1;
  pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
  pp.hDeviceWindow = window;
  pp.Windowed = TRUE;
  pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
  IDirect3DDevice9 *device = nullptr;
  CHECK(d3d->CreateDevice(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
      D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &device));
  struct Vertex { float x, y, z, rhw; DWORD color; };
  const DWORD green = D3DCOLOR_XRGB(32, 224, 64);
  const Vertex vertices[] = {
      {0.5f * pp.BackBufferWidth, 0.1f * pp.BackBufferHeight, 0, 1, green},
      {0.1f * pp.BackBufferWidth, 0.9f * pp.BackBufferHeight, 0, 1, green},
      {0.9f * pp.BackBufferWidth, 0.9f * pp.BackBufferHeight, 0, 1, green}};
  CHECK(device->SetFVF(D3DFVF_XYZRHW | D3DFVF_DIFFUSE));
  CHECK(device->SetRenderState(D3DRS_LIGHTING, FALSE));
  CHECK(device->SetRenderState(D3DRS_CULLMODE, D3DCULL_NONE));
  unsigned matches = 0;
  for (unsigned frame = 0; frame < 180; ++frame) {
    MSG msg;
    while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
      TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
    CHECK(device->Clear(0, nullptr, D3DCLEAR_TARGET, D3DCOLOR_XRGB(5, 23, 41), 1, 0));
    CHECK(device->BeginScene());
    CHECK(device->DrawPrimitiveUP(D3DPT_TRIANGLELIST, 1, vertices, sizeof(Vertex)));
    CHECK(device->EndScene());
    CHECK(device->Present(nullptr, nullptr, nullptr, nullptr));
    Sleep(30);
    if (frame >= 60 && frame % 30 == 0) {
      POINT center = {width / 2, height / 2};
      ClientToScreen(window, &center);
      HDC screen = GetDC(nullptr);
      const COLORREF pixel = GetPixel(screen, center.x, center.y);
      ReleaseDC(nullptr, screen);
      std::printf("frame=%u composed COLORREF=%06lx\n", frame, static_cast<unsigned long>(pixel));
      if (pixel == RGB(32, 224, 64)) ++matches;
    }
  }
  device->Release();
  d3d->Release();
  DestroyWindow(window);
  std::printf("%s: %u/4 composed samples match\n", matches == 4 ? "PASS" : "FAIL", matches);
  return matches == 4 ? 0 : 2;
}
