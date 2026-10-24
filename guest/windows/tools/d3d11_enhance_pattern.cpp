// d3d11_enhance_pattern: a D3D11 game stand-in for the composed present's
// upscaling and frame generation (guest/nvk-rm/docs/enhance.md): a borderless
// window covering the primary monitor whose flip-model swap chain is smaller
// (the game's render resolution), a spinning fan and a bar moving at a fixed
// speed, at a capped frame rate like an fps-locked game.
//
//   d3d11_enhance_pattern.exe [seconds=10] [width=1280 height=720] [fps cap=60]
//                             [resize WxH]
//
// With a resize size, the swap chain is resized to it half way through
// (ResizeBuffers), as a game does when its resolution changes.
//
// On DXVK on NVK (HELIOS_ICD=nvk, HELIOS_NVK_ICD=<test vulkan_nouveau.dll>)
// with MESA_WSI_ENHANCE_SCALER / MESA_WSI_ENHANCE_FG set, DXVK's Vulkan
// swapchain takes the back buffer's size and NVK's present scales it to the
// window and adds the generated pictures.  Prints the frame rate and the time
// Present took (average and worst).
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d11_enhance_pattern.cpp -o d3d11_enhance_pattern.exe \
//       -ld3d11 -ldxgi -ld3dcompiler -lgdi32 -luser32 -lwinmm
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <mmsystem.h>
#include <d3d11_1.h>
#include <d3dcompiler.h>
#include <dxgi1_2.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>

static const char kShader[] = R"(
cbuffer C : register(b0) { float4 rot; };
struct V { float4 pos : SV_Position; float4 col : COLOR; };
V vs(uint id : SV_VertexID) {
  // A 12-segment triangle fan as a list: 36 vertices, no vertex buffer.
  uint tri = id / 3, k = id % 3;
  float a0 = tri * 6.2831853 / 12.0, a1 = (tri + 1) * 6.2831853 / 12.0;
  float2 p = k == 0 ? float2(0, 0) : (k == 1 ? float2(cos(a0), sin(a0)) : float2(cos(a1), sin(a1)));
  float2 r = float2(p.x * rot.x - p.y * rot.y, p.x * rot.y + p.y * rot.x) * 0.5;
  V o;
  o.pos = float4(r.x * rot.z + rot.w, r.y, 0.5, 1);
  o.col = k == 0 ? float4(1, 1, 1, 1) : float4(0.5 + 0.5 * cos(a0), 0.5 + 0.5 * sin(a0 * 2), 0.5 + 0.5 * sin(a0), 1);
  return o;
}
float4 ps(V i) : SV_Target { return i.col; }
)";

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
  if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
  return DefWindowProcA(h, m, w, l);
}

static ID3DBlob* compile(const char* entry, const char* target) {
  ID3DBlob* code = nullptr;
  ID3DBlob* err = nullptr;
  if (FAILED(D3DCompile(kShader, sizeof(kShader) - 1, "pattern", nullptr, nullptr, entry, target,
                        0, 0, &code, &err))) {
    std::printf("D3DCompile %s: %s\n", entry, err ? (const char*)err->GetBufferPointer() : "?");
    std::exit(1);
  }
  return code;
}

int main(int argc, char** argv) {
  const int seconds = argc > 1 ? std::atoi(argv[1]) : 10;
  UINT width = argc > 3 ? std::atoi(argv[2]) : 1280;
  UINT height = argc > 3 ? std::atoi(argv[3]) : 720;
  const double cap = argc > 4 ? std::atof(argv[4]) : 60.0;
  unsigned rw = 0, rh = 0;
  if (argc > 5) std::sscanf(argv[5], "%ux%u", &rw, &rh);

  timeBeginPeriod(1);
  SetProcessDPIAware();
  WNDCLASSA wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "d3d11_enhance_pattern";
  RegisterClassA(&wc);
  const int sw = GetSystemMetrics(SM_CXSCREEN), sh = GetSystemMetrics(SM_CYSCREEN);
  HWND hwnd = CreateWindowA("d3d11_enhance_pattern", "d3d11_enhance_pattern",
                            WS_POPUP | WS_VISIBLE, 0, 0, sw, sh, nullptr, nullptr,
                            wc.hInstance, nullptr);
  SetForegroundWindow(hwnd);

  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext* ctx = nullptr;
  D3D_FEATURE_LEVEL fl = {};
  HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
                                 D3D11_SDK_VERSION, &dev, &fl, &ctx);
  if (FAILED(hr)) { std::printf("D3D11CreateDevice: 0x%08lx\n", hr); return 1; }

  IDXGIDevice* dxgiDev = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), (void**)&dxgiDev);
  IDXGIAdapter* adapter = nullptr;
  dxgiDev->GetAdapter(&adapter);
  DXGI_ADAPTER_DESC ad = {};
  adapter->GetDesc(&ad);
  std::printf("adapter: %ls, feature level 0x%x; window %dx%d, swap chain %ux%u\n",
              ad.Description, fl, sw, sh, width, height);
  IDXGIFactory2* factory = nullptr;
  adapter->GetParent(__uuidof(IDXGIFactory2), (void**)&factory);

  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = width;
  sd.Height = height;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = 3;
  sd.Scaling = DXGI_SCALING_STRETCH;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  IDXGISwapChain1* sc = nullptr;
  hr = factory->CreateSwapChainForHwnd(dev, hwnd, &sd, nullptr, nullptr, &sc);
  if (FAILED(hr)) { std::printf("CreateSwapChainForHwnd: 0x%08lx\n", hr); return 1; }

  ID3DBlob* vsb = compile("vs", "vs_5_0");
  ID3DBlob* psb = compile("ps", "ps_5_0");
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* ps = nullptr;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps);
  D3D11_BUFFER_DESC cbd = {};
  cbd.ByteWidth = 16;
  cbd.Usage = D3D11_USAGE_DYNAMIC;
  cbd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  cbd.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
  ID3D11Buffer* cb = nullptr;
  dev->CreateBuffer(&cbd, nullptr, &cb);
  D3D11_RASTERIZER_DESC rsd = {};
  rsd.FillMode = D3D11_FILL_SOLID;
  rsd.CullMode = D3D11_CULL_NONE;
  rsd.DepthClipEnable = TRUE;
  ID3D11RasterizerState* rs = nullptr;
  dev->CreateRasterizerState(&rsd, &rs);
  ID3D11DeviceContext1* ctx1 = nullptr;
  ctx->QueryInterface(__uuidof(ID3D11DeviceContext1), (void**)&ctx1);

  LARGE_INTEGER freq, start, now;
  QueryPerformanceFrequency(&freq);
  QueryPerformanceCounter(&start);
  auto secs = [&]() { LARGE_INTEGER t; QueryPerformanceCounter(&t);
                      return double(t.QuadPart - start.QuadPart) / freq.QuadPart; };
  unsigned frames = 0, failed = 0;
  double next = 0, psum = 0, pmax = 0;
  MSG msg;
  bool quit = false;
  while (!quit) {
    while (PeekMessageA(&msg, nullptr, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) quit = true;
      TranslateMessage(&msg);
      DispatchMessageA(&msg);
    }
    double t = secs();
    if (t >= seconds) break;
    if (cap > 0) {
      if (t < next) { Sleep(DWORD((next - t) * 1000.0)); while (secs() < next) {} }
      next += 1.0 / cap;
      if (secs() > next + 0.1) next = secs();
      t = secs();
    }

    if (rw && t >= seconds / 2.0) {
      ctx->OMSetRenderTargets(0, nullptr, nullptr);
      hr = sc->ResizeBuffers(0, rw, rh, DXGI_FORMAT_UNKNOWN, 0);
      std::printf("ResizeBuffers(%ux%u): 0x%08lx\n", rw, rh, (unsigned long)hr);
      width = rw;
      height = rh;
      rw = 0;
    }
    ID3D11Texture2D* bb = nullptr;
    sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)&bb);
    ID3D11RenderTargetView* rtv = nullptr;
    dev->CreateRenderTargetView(bb, nullptr, &rtv);
    const float clear[4] = {0.05f, 0.05f, 0.15f, 1.0f};
    ctx->ClearRenderTargetView(rtv, clear);
    D3D11_MAPPED_SUBRESOURCE m;
    if (SUCCEEDED(ctx->Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
      float* f = (float*)m.pData;
      f[0] = float(std::cos(t * 2.0));
      f[1] = float(std::sin(t * 2.0));
      f[2] = float(height) / float(width);
      /* across the screen and back every 4 s */
      const double ph = std::fmod(t / 2.0, 2.0);
      f[3] = float((ph < 1.0 ? ph : 2.0 - ph) * 1.2 - 0.6);
      ctx->Unmap(cb, 0);
    }
    D3D11_VIEWPORT vp = {0, 0, float(width), float(height), 0, 1};
    ctx->RSSetViewports(1, &vp);
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx->VSSetShader(vs, nullptr, 0);
    ctx->VSSetConstantBuffers(0, 1, &cb);
    ctx->PSSetShader(ps, nullptr, 0);
    ctx->RSSetState(rs);
    ctx->Draw(36, 0);
    if (ctx1) {
      /* a static "HUD" bar, and a white bar moving at 0.5 widths per second */
      const float red[4] = {0.9f, 0.1f, 0.1f, 1.0f};
      const D3D11_RECT hud = {16, 16, LONG(16 + width / 6), LONG(16 + height / 16)};
      ctx1->ClearView(rtv, red, &hud, 1);
      const double ph = std::fmod(t * 0.5, 2.0);
      const LONG bw = LONG(width / 16);
      const LONG x = LONG((ph < 1.0 ? ph : 2.0 - ph) * double(width - bw));
      const float white[4] = {1, 1, 1, 1};
      const D3D11_RECT bar = {x, LONG(height * 3 / 4), x + bw, LONG(height * 3 / 4 + height / 8)};
      ctx1->ClearView(rtv, white, &bar, 1);
    }
    rtv->Release();
    bb->Release();
    const double p0 = secs();
    hr = sc->Present(0, 0);
    const double pd = secs() - p0;
    psum += pd;
    if (pd > pmax) pmax = pd;
    if (FAILED(hr)) {
      if (++failed <= 5) std::printf("Present: 0x%08lx\n", hr);
      if (failed > 100) break;
    }
    frames++;
  }
  QueryPerformanceCounter(&now);
  const double total = double(now.QuadPart - start.QuadPart) / freq.QuadPart;
  std::printf("%u frames in %.2f s: %.1f fps; Present avg %.3f ms, max %.3f ms; %u failed\n",
              frames, total, frames / total, frames ? psum / frames * 1000.0 : 0.0,
              pmax * 1000.0, failed);
  return 0;
}
