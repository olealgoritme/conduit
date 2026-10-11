// d3d11_drawbench: the app-thread cost of a draw-call-heavy D3D11 frame, the
// shape of a CPU-bound game's render thread (CS2: a dynamic constant buffer
// mapped with DISCARD, shader resources and constant buffers bound, a draw,
// a few thousand times a frame). Offscreen and tiny on the GPU, so what it
// measures is the CPU path: the D3D11 runtime, the UMD and DXVK on the
// calling thread, plus the frame-pacing wait.
//
//   d3d11_drawbench.exe [seconds=10] [draws=2000] [lag=2] [srvs=8] [size=256]
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

#include <cstdio>
#include <cstdlib>
#include <cstring>

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
    ctx->Flush();
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
