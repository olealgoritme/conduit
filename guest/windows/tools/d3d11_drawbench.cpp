// d3d11_drawbench: the app-thread cost of a draw-call-heavy D3D11 frame, the
// shape of a CPU-bound game's render thread (CS2: a dynamic constant buffer
// mapped with DISCARD, shader resources and constant buffers bound, a draw,
// a few thousand times a frame). Offscreen and tiny on the GPU, so what it
// measures is the CPU path: the D3D11 runtime, the UMD and DXVK on the
// calling thread, plus the frame-pacing wait.
//
//   d3d11_drawbench.exe [seconds=10] [draws=2000] [lag=2] [srvs=8] [size=256] [mode=hot|real] [present]
//
// `present` renders into a flip-model swap chain (a size x size window) and
// Presents every frame (sync interval 0) instead of Flush, so the driver's
// present path runs; it needs the interactive desktop (run-in-session.ps1).
// Every run ends with the frame-time distribution (end of one frame's Present
// or Flush to the next): p50/p90/p99/p99.9/max, standard deviation, and a
// histogram relative to the median -- pacing, not only the average.
//
// mode=hot (default) is the loop below with a handful of objects, so every
// driver structure stays in cache. mode=real resembles a game's front-end
// cost instead: thousands of distinct objects bound in varied order, so the
// per-call driver path takes the cache misses real games show:
//   4096 textures/SRVs, 512 sampler states, 1024 dynamic constant buffers
//   (256 B..4 KiB, two mapped DISCARD per draw), 512 static constant buffers,
//   32 vertex shaders x 64 pixel shaders (switched every 4 draws), an input
//   layout per vertex shader and 512 vertex buffers, 64 blend / 64 rasterizer
//   / 64 depth-stencil states (switched every 8 draws), 4 render targets with
//   depth (switched and cleared every draws/4). Per draw: 2 maps, VS+PS
//   constant buffers (4 slots), 4 SRVs, 2 samplers, a vertex buffer, Draw.
//   The binding sequence is precomputed (xorshift), `srvs` is ignored.
//
// Per frame: `draws` x { Map(WRITE_DISCARD) a 256-byte constant buffer, write
// it, Unmap, VSSetConstantBuffers + PSSetConstantBuffers, PSSetShaderResources
// (one of `srvs` textures, rotating), Draw(3) } into a `size` x `size` target,
// then End an event query and Flush. Before the next frame it waits (GetData
// spin with Sleep(0)) for the query of `lag` frames ago, as a game's frame
// limiter on outstanding frames does.
//
// Prints once a second and at the end: frames/s, draws/s, the app thread's
// microseconds per frame split into submit (the draw loop + Flush) and wait
// (the query), and the thread's CPU time per frame (GetThreadTimes). A
// CPU-bound run has submit ~= frame time and cpu ~= submit.
//
// HELIOS_ICD=nvk / venus selects the UMD backend for this process.
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d11_drawbench.cpp -o d3d11_drawbench.exe
//       -ld3d11 -ldxgi -ld3dcompiler
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>
#include <algorithm>
#include <cmath>

static const char kShader[] = R"(
cbuffer C : register(b0) { float4 xf; float4 tint; float4 pad[14]; };
Texture2D t : register(t0);
SamplerState s : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
  float2 p = float2(id == 1 ? 1.0 : 0.0, id == 2 ? 1.0 : 0.0);
  V o;
  o.pos = float4(xf.xy + p * xf.zw, 0.5, 1);
  o.uv = p;
  return o;
}
float4 ps(V i) : SV_Target { return t.SampleLevel(s, i.uv, 0) * tint; }
)";

// mode=real: K selects the variant (compiled 32 x VS, 64 x PS).
static const char kRealShader[] = R"(
cbuffer A : register(b0) { float4 xf; float4 tint; float4 pa[14]; };
cbuffer B : register(b1) { float4 pb[16]; };
cbuffer C : register(b2) { float4 pc[16]; };
cbuffer D : register(b3) { float4 pd[16]; };
Texture2D t0 : register(t0); Texture2D t1 : register(t1);
Texture2D t2 : register(t2); Texture2D t3 : register(t3);
SamplerState s0 : register(s0); SamplerState s1 : register(s1);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 c : COLOR0; };
V vs(uint id : SV_VertexID, float4 a : TEXCOORD0) {
  float2 p = float2(id == 1 ? 1.0 : 0.0, id == 2 ? 1.0 : 0.0);
  V o;
  o.pos = float4(xf.xy + p * xf.zw, 0.5, 1);
  o.uv = p;
  o.c = a * pb[K % 16] + pc[(K * 3) % 16];
  return o;
}
float4 ps(V i) : SV_Target {
  float4 c = t0.SampleLevel(s0, i.uv, 0) + t1.SampleLevel(s1, i.uv, 0) * pd[K % 16];
  c += t2.SampleLevel(s0, i.uv * K, 0) * t3.SampleLevel(s1, i.uv, 0);
  return c * tint * i.c + pa[K % 14];
}
)";


template <class T> static void release(T *&p) {
  if (p) {
    p->Release();
    p = nullptr;
  }
}

static double now_us(LARGE_INTEGER f) {
  LARGE_INTEGER c;
  QueryPerformanceCounter(&c);
  return double(c.QuadPart) * 1e6 / double(f.QuadPart);
}

static double thread_cpu_us() {
  FILETIME a, b, k, u;
  GetThreadTimes(GetCurrentThread(), &a, &b, &k, &u);
  auto t = [](FILETIME x) { return (double(x.dwHighDateTime) * 4294967296.0 + x.dwLowDateTime) / 10.0; };
  return t(k) + t(u);
}

// --- frame pacing and the optional swap chain ------------------------------

static IDXGISwapChain1 *g_swap = nullptr;
static ID3D11RenderTargetView *g_backbuffer = nullptr;
static HWND g_hwnd = nullptr;
static std::vector<float> g_frame_us;
static double g_last_frame_end = 0;

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcA(h, m, w, l); }

static bool create_swapchain(ID3D11Device *dev, int size) {
  WNDCLASSA wc = {};
  wc.lpfnWndProc = wndproc;
  wc.hInstance = GetModuleHandleA(nullptr);
  wc.lpszClassName = "d3d11_drawbench";
  RegisterClassA(&wc);
  RECT r = {0, 0, size, size};
  AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
  g_hwnd = CreateWindowA("d3d11_drawbench", "d3d11_drawbench", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 50, 50,
                         r.right - r.left, r.bottom - r.top, nullptr, nullptr, wc.hInstance, nullptr);
  if (!g_hwnd)
    return false;
  IDXGIDevice *xd = nullptr;
  IDXGIAdapter *ad = nullptr;
  IDXGIFactory2 *f = nullptr;
  dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&xd);
  if (xd)
    xd->GetAdapter(&ad);
  if (ad)
    ad->GetParent(__uuidof(IDXGIFactory2), (void **)&f);
  DXGI_SWAP_CHAIN_DESC1 d = {};
  d.Width = d.Height = UINT(size);
  d.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  d.SampleDesc.Count = 1;
  d.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  d.BufferCount = 2;
  d.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  if (f)
    f->CreateSwapChainForHwnd(dev, g_hwnd, &d, nullptr, nullptr, &g_swap);
  release(f);
  release(ad);
  release(xd);
  if (!g_swap)
    return false;
  ID3D11Texture2D *bb = nullptr;
  g_swap->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb);
  if (bb)
    dev->CreateRenderTargetView(bb, nullptr, &g_backbuffer);
  release(bb);
  return g_backbuffer != nullptr;
}

// End a frame: Present (or Flush) and record the frame time.
static void end_frame(ID3D11DeviceContext *ctx, LARGE_INTEGER freq) {
  if (g_swap) {
    MSG m;
    while (PeekMessageA(&m, nullptr, 0, 0, PM_REMOVE))
      DispatchMessageA(&m);
    g_swap->Present(0, 0);
  } else {
    ctx->Flush();
  }
  double t = now_us(freq);
  if (g_last_frame_end != 0)
    g_frame_us.push_back(float(t - g_last_frame_end));
  g_last_frame_end = t;
}

static void print_pacing() {
  if (g_frame_us.size() < 2)
    return;
  std::vector<float> v = g_frame_us;
  std::sort(v.begin(), v.end());
  auto pct = [&](double p) { return v[std::min(v.size() - 1, size_t(p * double(v.size() - 1) + 0.5))]; };
  double mean = 0, var = 0;
  for (float x : v)
    mean += x;
  mean /= double(v.size());
  for (float x : v)
    var += (x - mean) * (x - mean);
  double sd = std::sqrt(var / double(v.size()));
  double med = pct(0.5);
  printf("frametime us: p50 %.0f p90 %.0f p99 %.0f p99.9 %.0f max %.0f mean %.0f sd %.0f (%.1f%% of mean)\n", med,
         pct(0.9), pct(0.99), pct(0.999), v.back(), mean, sd, 100.0 * sd / mean);
  const double edges[] = {0.5, 0.9, 1.1, 1.5, 2.0, 4.0};
  size_t n[7] = {};
  for (float x : v) {
    int b = 0;
    while (b < 6 && x >= edges[b] * med)
      b++;
    n[b]++;
  }
  printf("frametime histogram (x median): <0.5 %zu  0.5-0.9 %zu  0.9-1.1 %zu  1.1-1.5 %zu  1.5-2 %zu  2-4 %zu  >4 %zu\n",
         n[0], n[1], n[2], n[3], n[4], n[5], n[6]);
}

static ID3DBlob *compile_variant(const char *entry, const char *target, int k) {
  char kdef[16];
  snprintf(kdef, sizeof kdef, "%d", k);
  D3D_SHADER_MACRO defs[] = {{"K", kdef}, {nullptr, nullptr}};
  ID3DBlob *code = nullptr, *err = nullptr;
  if (FAILED(D3DCompile(kRealShader, sizeof kRealShader - 1, "drawbench-real", defs, nullptr, entry, target, 0, 0,
                        &code, &err))) {
    fprintf(stderr, "compile %s K=%d: %s\n", entry, k, err ? (const char *)err->GetBufferPointer() : "?");
    return nullptr;
  }
  release(err);
  return code;
}

static unsigned xorshift(unsigned &s) {
  s ^= s << 13;
  s ^= s >> 17;
  s ^= s << 5;
  return s;
}

int run_real(ID3D11Device *dev, ID3D11DeviceContext *ctx, double seconds, int draws, int lag, int size);

static ID3DBlob *compile(const char *entry, const char *target) {
  ID3DBlob *code = nullptr, *err = nullptr;
  if (FAILED(D3DCompile(kShader, sizeof kShader - 1, "drawbench", nullptr, nullptr, entry, target, 0, 0,
                        &code, &err))) {
    fprintf(stderr, "compile %s: %s\n", entry, err ? (const char *)err->GetBufferPointer() : "?");
    return nullptr;
  }
  release(err);
  return code;
}

int main(int argc, char **argv) {
  const double seconds = argc > 1 ? atof(argv[1]) : 10.0;
  const int draws = argc > 2 ? atoi(argv[2]) : 2000;
  const int lag = argc > 3 ? atoi(argv[3]) : 2;
  const int nsrv = argc > 4 ? atoi(argv[4]) : 8;
  const int size = argc > 5 ? atoi(argv[5]) : 256;
  const bool real = argc > 6 && strcmp(argv[6], "real") == 0;
  const bool present = argc > 7 && strcmp(argv[7], "present") == 0;
  if (draws < 1 || lag < 1 || lag > 8 || nsrv < 1 || nsrv > 64 || size < 16) {
    fprintf(stderr, "usage: d3d11_drawbench [seconds] [draws>=1] [lag 1..8] [srvs 1..64] [size>=16]\n");
    return 2;
  }

  ID3D11Device *dev = nullptr;
  ID3D11DeviceContext *ctx = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  if (FAILED(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev,
                               nullptr, &ctx))) {
    fprintf(stderr, "D3D11CreateDevice failed\n");
    return 1;
  }

  if (present && !create_swapchain(dev, size)) {
    fprintf(stderr, "swap chain creation failed\n");
    return 1;
  }
  if (real) {
    int rc = run_real(dev, ctx, seconds, draws, lag, size);
    print_pacing();
    release(g_backbuffer);
    release(g_swap);
    ctx->ClearState();
    release(ctx);
    release(dev);
    return rc;
  }

  ID3DBlob *vsb = compile("vs", "vs_5_0"), *psb = compile("ps", "ps_5_0");
  if (!vsb || !psb)
    return 1;
  ID3D11VertexShader *vs = nullptr;
  ID3D11PixelShader *ps = nullptr;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps);

  D3D11_TEXTURE2D_DESC td = {};
  td.Width = td.Height = UINT(size);
  td.MipLevels = td.ArraySize = 1;
  td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET;
  ID3D11Texture2D *rt = nullptr;
  ID3D11RenderTargetView *rtv = nullptr;
  dev->CreateTexture2D(&td, nullptr, &rt);
  dev->CreateRenderTargetView(rt, nullptr, &rtv);

  ID3D11ShaderResourceView *srv[64] = {};
  {
    D3D11_TEXTURE2D_DESC sd = td;
    sd.Width = sd.Height = 16;
    sd.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    sd.Usage = D3D11_USAGE_IMMUTABLE;
    UINT texels[256];
    D3D11_SUBRESOURCE_DATA init = {texels, 16 * 4, 0};
    for (int i = 0; i < nsrv; i++) {
      for (UINT &t : texels)
        t = 0xff000000u | (UINT(i * 37) << 16) | (UINT(i * 91) << 8) | UINT(255 - i * 13);
      ID3D11Texture2D *tex = nullptr;
      dev->CreateTexture2D(&sd, &init, &tex);
      dev->CreateShaderResourceView(tex, nullptr, &srv[i]);
      release(tex);
    }
  }

  D3D11_BUFFER_DESC bd = {};
  bd.ByteWidth = 256;
  bd.Usage = D3D11_USAGE_DYNAMIC;
  bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  bd.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
  ID3D11Buffer *cb = nullptr;
  dev->CreateBuffer(&bd, nullptr, &cb);

  D3D11_SAMPLER_DESC smp = {};
  smp.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
  smp.AddressU = smp.AddressV = smp.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
  smp.MaxLOD = D3D11_FLOAT32_MAX;
  ID3D11SamplerState *sampler = nullptr;
  dev->CreateSamplerState(&smp, &sampler);

  ID3D11Query *q[8] = {};
  D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
  for (int i = 0; i < lag; i++)
    dev->CreateQuery(&qd, &q[i]);

  if (!vs || !ps || !rtv || !cb || !sampler || !srv[nsrv - 1] || !q[lag - 1]) {
    fprintf(stderr, "resource creation failed\n");
    return 1;
  }

  D3D11_VIEWPORT vp = {0, 0, float(size), float(size), 0, 1};
  ctx->RSSetViewports(1, &vp);
  ctx->OMSetRenderTargets(1, &rtv, nullptr);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);
  ctx->PSSetSamplers(0, 1, &sampler);

  LARGE_INTEGER freq;
  QueryPerformanceFrequency(&freq);
  printf("d3d11_drawbench: %d draws/frame, lag %d, %d SRVs, %dx%d target, %.0f s\n", draws, lag, nsrv, size,
         size, seconds);

  long long frames = 0, sec_frames = 0;
  double sum_submit = 0, sum_wait = 0, sec_submit = 0, sec_wait = 0;
  const double t0 = now_us(freq), cpu0 = thread_cpu_us();
  double sec_t = t0, sec_cpu = cpu0;
  for (;;) {
    ID3D11Query *slot = q[frames % lag];
    // The frame of `lag` ago used this query: wait for it.
    double w0 = now_us(freq);
    if (frames >= lag) {
      BOOL done = FALSE;
      while (ctx->GetData(slot, &done, sizeof done, 0) != S_OK || !done)
        Sleep(0);
    }
    double s0 = now_us(freq);
    if (g_backbuffer) // flip model unbinds the back buffer at Present
      ctx->OMSetRenderTargets(1, &g_backbuffer, nullptr);
    for (int i = 0; i < draws; i++) {
      D3D11_MAPPED_SUBRESOURCE m;
      if (SUCCEEDED(ctx->Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
        float *f = (float *)m.pData;
        f[0] = float(i % 64) / 32.0f - 1.0f;
        f[1] = float(i / 64 % 64) / 32.0f - 1.0f;
        f[2] = f[3] = 1.0f / 32.0f;
        f[4] = f[5] = f[6] = f[7] = 1.0f;
        ctx->Unmap(cb, 0);
      }
      ctx->VSSetConstantBuffers(0, 1, &cb);
      ctx->PSSetConstantBuffers(0, 1, &cb);
      ctx->PSSetShaderResources(0, 1, &srv[i % nsrv]);
      ctx->Draw(3, 0);
    }
    ctx->End(slot);
    end_frame(ctx, freq);
    double s1 = now_us(freq);
    sum_wait += s0 - w0;
    sec_wait += s0 - w0;
    sum_submit += s1 - s0;
    sec_submit += s1 - s0;
    frames++;
    sec_frames++;
    if (s1 - sec_t >= 1e6) {
      double cpu = thread_cpu_us();
      printf("  %6.0f fps  %8.0f draws/s  submit %7.1f us  wait %7.1f us  cpu %7.1f us /frame\n",
             sec_frames * 1e6 / (s1 - sec_t), sec_frames * double(draws) * 1e6 / (s1 - sec_t),
             sec_submit / sec_frames, sec_wait / sec_frames, (cpu - sec_cpu) / sec_frames);
      sec_t = s1;
      sec_cpu = cpu;
      sec_frames = 0;
      sec_submit = sec_wait = 0;
    }
    if (s1 - t0 >= seconds * 1e6)
      break;
  }
  const double t1 = now_us(freq), cpu1 = thread_cpu_us();
  printf("avg %.0f fps  %.0f draws/s  submit %.1f us  wait %.1f us  cpu %.1f us /frame  (%.2f us/draw)\n",
         frames * 1e6 / (t1 - t0), frames * double(draws) * 1e6 / (t1 - t0), sum_submit / frames,
         sum_wait / frames, (cpu1 - cpu0) / frames, sum_submit / frames / draws);

  print_pacing();
  release(g_backbuffer);
  release(g_swap);
  for (int i = 0; i < lag; i++)
    release(q[i]);
  for (int i = 0; i < nsrv; i++)
    release(srv[i]);
  release(sampler);
  release(cb);
  release(rtv);
  release(rt);
  release(ps);
  release(vs);
  release(psb);
  release(vsb);
  ctx->ClearState();
  release(ctx);
  release(dev);
  return 0;
}

// ---------------------------------------------------------------------------
// mode=real
// ---------------------------------------------------------------------------

int run_real(ID3D11Device *dev, ID3D11DeviceContext *ctx, double seconds, int draws, int lag, int size) {
  enum { NTEX = 4096, NSAMP = 512, NDYN = 1024, NSTAT = 512, NVS = 32, NPS = 64, NVB = 512, NSTATE = 64, NRT = 4 };
  std::vector<ID3D11ShaderResourceView *> srv(NTEX);
  std::vector<ID3D11SamplerState *> samp(NSAMP);
  std::vector<ID3D11Buffer *> dyn(NDYN), stat(NSTAT), vb(NVB);
  std::vector<UINT> dyn_size(NDYN);
  ID3D11VertexShader *vs[NVS] = {};
  ID3D11PixelShader *ps[NPS] = {};
  ID3D11InputLayout *il[NVS] = {};
  ID3D11BlendState *bs[NSTATE] = {};
  ID3D11RasterizerState *rs[NSTATE] = {};
  ID3D11DepthStencilState *ds[NSTATE] = {};
  ID3D11Texture2D *rt[NRT] = {}, *depth[NRT] = {};
  ID3D11RenderTargetView *rtv[NRT] = {};
  ID3D11DepthStencilView *dsv[NRT] = {};
  ID3D11Query *q[8] = {};
  unsigned seed = 0x2545f491u;

  // Textures: 16x16, distinct contents, created in shuffled order so the
  // driver objects do not sit contiguously.
  {
    D3D11_TEXTURE2D_DESC sd = {};
    sd.Width = sd.Height = 16;
    sd.MipLevels = sd.ArraySize = 1;
    sd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.Usage = D3D11_USAGE_IMMUTABLE;
    sd.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    UINT texels[256];
    D3D11_SUBRESOURCE_DATA init = {texels, 16 * 4, 0};
    for (int i = 0; i < NTEX; i++) {
      for (UINT &t : texels)
        t = 0xff000000u | (xorshift(seed) & 0xffffffu);
      ID3D11Texture2D *tex = nullptr;
      dev->CreateTexture2D(&sd, &init, &tex);
      if (tex)
        dev->CreateShaderResourceView(tex, nullptr, &srv[i]);
      release(tex);
    }
  }
  for (int i = 0; i < NSAMP; i++) {
    D3D11_SAMPLER_DESC d = {};
    d.Filter = (i & 1) ? D3D11_FILTER_MIN_MAG_MIP_LINEAR : D3D11_FILTER_MIN_MAG_MIP_POINT;
    d.AddressU = D3D11_TEXTURE_ADDRESS_MODE(1 + i % 4);
    d.AddressV = D3D11_TEXTURE_ADDRESS_MODE(1 + (i / 4) % 4);
    d.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    d.MipLODBias = float(i) / 64.0f - 4.0f;
    d.MaxLOD = D3D11_FLOAT32_MAX;
    d.ComparisonFunc = D3D11_COMPARISON_NEVER;
    dev->CreateSamplerState(&d, &samp[i]);
  }
  for (int i = 0; i < NDYN; i++) {
    D3D11_BUFFER_DESC d = {};
    dyn_size[i] = 256u << (i % 5); // 256 B .. 4 KiB
    d.ByteWidth = dyn_size[i];
    d.Usage = D3D11_USAGE_DYNAMIC;
    d.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    d.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
    dev->CreateBuffer(&d, nullptr, &dyn[i]);
  }
  {
    float data[64] = {};
    for (int i = 0; i < NSTAT; i++) {
      for (float &f : data)
        f = float(xorshift(seed) & 1023) / 1024.0f;
      D3D11_BUFFER_DESC d = {};
      d.ByteWidth = sizeof data;
      d.Usage = D3D11_USAGE_IMMUTABLE;
      d.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
      D3D11_SUBRESOURCE_DATA init = {data, 0, 0};
      dev->CreateBuffer(&d, &init, &stat[i]);
    }
  }
  {
    float data[12];
    for (int i = 0; i < NVB; i++) {
      for (float &f : data)
        f = float(xorshift(seed) & 255) / 255.0f;
      D3D11_BUFFER_DESC d = {};
      d.ByteWidth = sizeof data;
      d.Usage = D3D11_USAGE_IMMUTABLE;
      d.BindFlags = D3D11_BIND_VERTEX_BUFFER;
      D3D11_SUBRESOURCE_DATA init = {data, 0, 0};
      dev->CreateBuffer(&d, &init, &vb[i]);
    }
  }
  D3D11_INPUT_ELEMENT_DESC ied = {"TEXCOORD", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0};
  for (int k = 0; k < NVS; k++) {
    ID3DBlob *b = compile_variant("vs", "vs_5_0", k);
    if (!b)
      return 1;
    dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &vs[k]);
    dev->CreateInputLayout(&ied, 1, b->GetBufferPointer(), b->GetBufferSize(), &il[k]);
    release(b);
  }
  for (int k = 0; k < NPS; k++) {
    ID3DBlob *b = compile_variant("ps", "ps_5_0", k);
    if (!b)
      return 1;
    dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &ps[k]);
    release(b);
  }
  for (int i = 0; i < NSTATE; i++) {
    D3D11_BLEND_DESC b = {};
    b.RenderTarget[0].BlendEnable = (i & 1) != 0;
    b.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
    b.RenderTarget[0].DestBlend = (i & 2) ? D3D11_BLEND_INV_SRC_ALPHA : D3D11_BLEND_ONE;
    b.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    b.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    b.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ZERO;
    b.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    b.RenderTarget[0].RenderTargetWriteMask = UINT8(1 + i % 15);
    dev->CreateBlendState(&b, &bs[i]);
    D3D11_RASTERIZER_DESC r = {};
    r.FillMode = D3D11_FILL_SOLID;
    r.CullMode = D3D11_CULL_MODE(1 + i % 3);
    r.DepthBias = i;
    r.SlopeScaledDepthBias = float(i) / 16.0f;
    r.DepthClipEnable = TRUE;
    r.ScissorEnable = (i & 4) != 0;
    dev->CreateRasterizerState(&r, &rs[i]);
    D3D11_DEPTH_STENCIL_DESC z = {};
    z.DepthEnable = TRUE;
    z.DepthWriteMask = (i & 1) ? D3D11_DEPTH_WRITE_MASK_ALL : D3D11_DEPTH_WRITE_MASK_ZERO;
    z.DepthFunc = D3D11_COMPARISON_FUNC(1 + i % 8);
    z.StencilEnable = (i & 2) != 0;
    z.StencilReadMask = z.StencilWriteMask = UINT8(0xff - i);
    z.FrontFace = {D3D11_STENCIL_OP_KEEP, D3D11_STENCIL_OP_KEEP, D3D11_STENCIL_OP_INCR, D3D11_COMPARISON_ALWAYS};
    z.BackFace = z.FrontFace;
    dev->CreateDepthStencilState(&z, &ds[i]);
  }
  for (int i = 0; i < NRT; i++) {
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = td.Height = UINT(size);
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    dev->CreateTexture2D(&td, nullptr, &rt[i]);
    if (rt[i])
      dev->CreateRenderTargetView(rt[i], nullptr, &rtv[i]);
    td.Format = DXGI_FORMAT_D24_UNORM_S8_UINT;
    td.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    dev->CreateTexture2D(&td, nullptr, &depth[i]);
    if (depth[i])
      dev->CreateDepthStencilView(depth[i], nullptr, &dsv[i]);
  }
  D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
  for (int i = 0; i < lag; i++)
    dev->CreateQuery(&qd, &q[i]);
  for (int i = 0; i < NTEX; i++)
    if (!srv[i]) { fprintf(stderr, "texture %d creation failed\n", i); return 1; }
  for (int i = 0; i < NRT; i++)
    if (!rtv[i] || !dsv[i]) { fprintf(stderr, "render target creation failed\n"); return 1; }
  for (int i = 0; i < NSTATE; i++)
    if (!bs[i] || !rs[i] || !ds[i]) { fprintf(stderr, "state creation failed\n"); return 1; }

  // The binding sequence of one frame, precomputed.
  struct Draw {
    unsigned short tex[4], samp[2], dyn[2], stat[2], vb, vs, ps, bs, rs, ds;
  };
  std::vector<Draw> seq(static_cast<size_t>(draws));
  for (Draw &d : seq) {
    for (auto &t : d.tex) t = (unsigned short)(xorshift(seed) % NTEX);
    for (auto &t : d.samp) t = (unsigned short)(xorshift(seed) % NSAMP);
    for (auto &t : d.dyn) t = (unsigned short)(xorshift(seed) % NDYN);
    for (auto &t : d.stat) t = (unsigned short)(xorshift(seed) % NSTAT);
    d.vb = (unsigned short)(xorshift(seed) % NVB);
    d.vs = (unsigned short)(xorshift(seed) % NVS);
    d.ps = (unsigned short)(xorshift(seed) % NPS);
    d.bs = (unsigned short)(xorshift(seed) % NSTATE);
    d.rs = (unsigned short)(xorshift(seed) % NSTATE);
    d.ds = (unsigned short)(xorshift(seed) % NSTATE);
  }

  D3D11_VIEWPORT vp = {0, 0, float(size), float(size), 0, 1};
  D3D11_RECT sc = {0, 0, size, size};
  ctx->RSSetViewports(1, &vp);
  ctx->RSSetScissorRects(1, &sc);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

  LARGE_INTEGER freq;
  QueryPerformanceFrequency(&freq);
  printf("d3d11_drawbench real: %d draws/frame, lag %d, %dx%d targets x%d, %d textures, %d samplers, "
         "%d dynamic CBs, %d VS x %d PS, %.0f s\n",
         draws, lag, size, size, int(NRT), int(NTEX), int(NSAMP), int(NDYN), int(NVS), int(NPS), seconds);

  const float clear[4] = {0.1f, 0.2f, 0.3f, 1.0f};
  const int per_rt = draws / NRT > 0 ? draws / NRT : 1;
  long long frames = 0, sec_frames = 0;
  double sum_submit = 0, sum_wait = 0, sec_submit = 0, sec_wait = 0;
  const double t0 = now_us(freq), cpu0 = thread_cpu_us();
  double sec_t = t0, sec_cpu = cpu0;
  for (;;) {
    ID3D11Query *slot = q[frames % lag];
    double w0 = now_us(freq);
    if (frames >= lag) {
      BOOL done = FALSE;
      while (ctx->GetData(slot, &done, sizeof done, 0) != S_OK || !done)
        Sleep(0);
    }
    double s0 = now_us(freq);
    for (int i = 0; i < draws; i++) {
      const Draw &d = seq[size_t(i)];
      if (i % per_rt == 0) {
        int r = (i / per_rt) % NRT;
        ID3D11RenderTargetView *target = (r == 0 && g_backbuffer) ? g_backbuffer : rtv[r];
        ctx->OMSetRenderTargets(1, &target, dsv[r]);
        ctx->ClearRenderTargetView(target, clear);
        ctx->ClearDepthStencilView(dsv[r], D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 1.0f, 0);
      }
      if (i % 4 == 0) {
        ctx->VSSetShader(vs[d.vs], nullptr, 0);
        ctx->PSSetShader(ps[d.ps], nullptr, 0);
        ctx->IASetInputLayout(il[d.vs]);
      }
      if (i % 8 == 0) {
        const float bf[4] = {1, 1, 1, 1};
        ctx->OMSetBlendState(bs[d.bs], bf, 0xffffffffu);
        ctx->RSSetState(rs[d.rs]);
        ctx->OMSetDepthStencilState(ds[d.ds], i & 0xff);
      }
      for (int c = 0; c < 2; c++) {
        D3D11_MAPPED_SUBRESOURCE m;
        if (SUCCEEDED(ctx->Map(dyn[d.dyn[c]], 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
          float *f = (float *)m.pData;
          f[0] = float(i % 64) / 32.0f - 1.0f;
          f[1] = float(i / 64 % 64) / 32.0f - 1.0f;
          f[2] = f[3] = 1.0f / 32.0f;
          for (int k = 4; k < 32; k++)
            f[k] = float(k + c);
          ctx->Unmap(dyn[d.dyn[c]], 0);
        }
      }
      ID3D11Buffer *cbs[4] = {dyn[d.dyn[0]], dyn[d.dyn[1]], stat[d.stat[0]], stat[d.stat[1]]};
      ctx->VSSetConstantBuffers(0, 4, cbs);
      ctx->PSSetConstantBuffers(0, 4, cbs);
      ID3D11ShaderResourceView *views[4] = {srv[d.tex[0]], srv[d.tex[1]], srv[d.tex[2]], srv[d.tex[3]]};
      ctx->PSSetShaderResources(0, 4, views);
      ID3D11SamplerState *ss[2] = {samp[d.samp[0]], samp[d.samp[1]]};
      ctx->PSSetSamplers(0, 2, ss);
      UINT stride = 16, offset = 0;
      ctx->IASetVertexBuffers(0, 1, &vb[d.vb], &stride, &offset);
      ctx->Draw(3, 0);
    }
    ctx->End(slot);
    end_frame(ctx, freq);
    double s1 = now_us(freq);
    sum_wait += s0 - w0;
    sec_wait += s0 - w0;
    sum_submit += s1 - s0;
    sec_submit += s1 - s0;
    frames++;
    sec_frames++;
    if (s1 - sec_t >= 1e6) {
      double cpu = thread_cpu_us();
      printf("  %6.0f fps  %8.0f draws/s  submit %7.1f us  wait %7.1f us  cpu %7.1f us /frame\n",
             sec_frames * 1e6 / (s1 - sec_t), sec_frames * double(draws) * 1e6 / (s1 - sec_t),
             sec_submit / sec_frames, sec_wait / sec_frames, (cpu - sec_cpu) / sec_frames);
      sec_t = s1;
      sec_cpu = cpu;
      sec_frames = 0;
      sec_submit = sec_wait = 0;
    }
    if (s1 - t0 >= seconds * 1e6)
      break;
  }
  const double t1 = now_us(freq), cpu1 = thread_cpu_us();
  printf("avg %.0f fps  %.0f draws/s  submit %.1f us  wait %.1f us  cpu %.1f us /frame  (%.2f us/draw)\n",
         frames * 1e6 / (t1 - t0), frames * double(draws) * 1e6 / (t1 - t0), sum_submit / frames,
         sum_wait / frames, (cpu1 - cpu0) / frames, sum_submit / frames / draws);

  ctx->ClearState();
  for (auto *p : srv) release(p);
  for (auto *p : samp) release(p);
  for (auto *p : dyn) release(p);
  for (auto *p : stat) release(p);
  for (auto *p : vb) release(p);
  for (auto *&p : vs) release(p);
  for (auto *&p : ps) release(p);
  for (auto *&p : il) release(p);
  for (auto *&p : bs) release(p);
  for (auto *&p : rs) release(p);
  for (auto *&p : ds) release(p);
  for (int i = 0; i < NRT; i++) {
    release(rtv[i]);
    release(dsv[i]);
    release(rt[i]);
    release(depth[i]);
  }
  for (int i = 0; i < lag; i++)
    release(q[i]);
  return 0;
}
