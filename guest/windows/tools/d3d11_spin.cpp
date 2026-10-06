// d3d11_spin: a minimal D3D11 frame-rate test for the Helios UMD (64-bit or
// 32-bit), used for dxvk-on-nvk S3: a spinning, shaded triangle fan presented
// through a flip-model DXGI swap chain, frames counted per second.
//
//   d3d11_spin.exe [seconds=10] [width=1920 height=1080] [sync=0] [fullscreen=0]
//
// Prints the adapter, the swap-chain format, fps per second and the average.
// HELIOS_ICD=nvk / venus in the environment selects the UMD backend for this
// process (umd_common/bridge/bridge_icd_backend.h); helios_umd's log says which
// one it took.
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d11_spin.cpp -o d3d11_spin.exe \
//       -ld3d11 -ldxgi -ld3dcompiler -lgdi32 -luser32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
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
  float2 r = float2(p.x * rot.x - p.y * rot.y, p.x * rot.y + p.y * rot.x) * 0.8;
  V o;
  o.pos = float4(r.x * rot.z, r.y, 0.5, 1);
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
  if (FAILED(D3DCompile(kShader, sizeof(kShader) - 1, "spin", nullptr, nullptr, entry, target,
                        0, 0, &code, &err))) {
    std::printf("D3DCompile %s: %s\n", entry, err ? (const char*)err->GetBufferPointer() : "?");
    std::exit(1);
  }
  return code;
}

int main(int argc, char** argv) {
  const int seconds = argc > 1 ? std::atoi(argv[1]) : 10;
  const UINT width = argc > 3 ? std::atoi(argv[2]) : 1920;
  const UINT height = argc > 3 ? std::atoi(argv[3]) : 1080;
  const UINT sync = argc > 4 ? std::atoi(argv[4]) : 0;
  const BOOL fullscreen = argc > 5 ? std::atoi(argv[5]) : 0;

  WNDCLASSA wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "d3d11_spin";
  RegisterClassA(&wc);
  RECT rc = {0, 0, LONG(width), LONG(height)};
  AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);
  HWND hwnd = CreateWindowA("d3d11_spin", "d3d11_spin", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0,
                            rc.right - rc.left, rc.bottom - rc.top, nullptr, nullptr,
                            wc.hInstance, nullptr);

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
  std::printf("adapter: %ls (vendor 0x%04x device 0x%04x), feature level 0x%x\n",
              ad.Description, ad.VendorId, ad.DeviceId, fl);
  IDXGIFactory2* factory = nullptr;
  adapter->GetParent(__uuidof(IDXGIFactory2), (void**)&factory);

  DXGI_SWAP_CHAIN_DESC1 sd = {};
  sd.Width = width;
  sd.Height = height;
  sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  sd.SampleDesc.Count = 1;
  sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  sd.BufferCount = 3;
  sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  IDXGISwapChain1* sc = nullptr;
  hr = factory->CreateSwapChainForHwnd(dev, hwnd, &sd, nullptr, nullptr, &sc);
  if (FAILED(hr)) { std::printf("CreateSwapChainForHwnd: 0x%08lx\n", hr); return 1; }
  if (fullscreen) {
    hr = sc->SetFullscreenState(TRUE, nullptr);
    std::printf("SetFullscreenState: 0x%08lx\n", hr);
    sc->ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, 0);
  }
  std::printf("swap chain %ux%u B8G8R8A8 flip-discard x3, sync %u, fullscreen %d\n", width,
              height, sync, fullscreen);

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

  LARGE_INTEGER freq, start, last, now;
  QueryPerformanceFrequency(&freq);
  QueryPerformanceCounter(&start);
  last = start;
  unsigned frames = 0, window_frames = 0, failed = 0;
  MSG msg;
  bool quit = false;
  while (!quit) {
    while (PeekMessageA(&msg, nullptr, 0, 0, PM_REMOVE)) {
      if (msg.message == WM_QUIT) quit = true;
      TranslateMessage(&msg);
      DispatchMessageA(&msg);
    }
    QueryPerformanceCounter(&now);
    const double t = double(now.QuadPart - start.QuadPart) / freq.QuadPart;
    if (t >= seconds) break;

    ID3D11Texture2D* bb = nullptr;
    sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)&bb);
    ID3D11RenderTargetView* rtv = nullptr;
    dev->CreateRenderTargetView(bb, nullptr, &rtv);
    const float clear[4] = {0.05f, 0.05f, 0.1f + 0.1f * float(std::sin(t)), 1.0f};
    ctx->ClearRenderTargetView(rtv, clear);
    D3D11_MAPPED_SUBRESOURCE m;
    if (SUCCEEDED(ctx->Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
      float* f = (float*)m.pData;
      f[0] = float(std::cos(t * 2.0));
      f[1] = float(std::sin(t * 2.0));
      f[2] = float(height) / float(width);
      f[3] = 0.0f;
      ctx->Unmap(cb, 0);
    }
    D3D11_VIEWPORT vp = {0, 0, float(width), float(height), 0, 1};
    ctx->RSSetViewports(1, &vp);
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx->VSSetShader(vs, nullptr, 0);
    ctx->VSSetConstantBuffers(0, 1, &cb);
    ctx->PSSetShader(ps, nullptr, 0);
    ctx->Draw(36, 0);
    rtv->Release();
    bb->Release();
    hr = sc->Present(sync, 0);
    if (FAILED(hr)) {
      if (++failed <= 5) std::printf("Present: 0x%08lx\n", hr);
      if (failed > 100) break;
    }
    frames++;
    window_frames++;
    const double dt = double(now.QuadPart - last.QuadPart) / freq.QuadPart;
    if (dt >= 1.0) {
      std::printf("t=%5.1fs %7.1f fps\n", t, window_frames / dt);
      std::fflush(stdout);
      window_frames = 0;
      last = now;
    }
  }
  QueryPerformanceCounter(&now);
  const double total = double(now.QuadPart - start.QuadPart) / freq.QuadPart;
  std::printf("%u frames in %.2f s: %.1f fps average, %u failed presents\n", frames, total,
              frames / total, failed);
  if (fullscreen) sc->SetFullscreenState(FALSE, nullptr);
  return 0;
}
