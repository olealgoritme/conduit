// d3d11_mapbench: the per-stage CPU cost of a dynamic-buffer-heavy D3D11 frame
// on the calling thread, call class by call class. It reproduces the render
// thread pattern of a CPU-bound D3D11 game (CS2): per draw a few small dynamic
// constant buffers mapped WRITE_DISCARD, constants appended to a large dynamic
// constant buffer with WRITE_NO_OVERWRITE and bound by offset
// (*SetConstantBuffers1), vertices and indices appended to dynamic ring
// buffers with WRITE_NO_OVERWRITE (WRITE_DISCARD when a ring wraps), texture
// and sampler binds, a shader switch every few draws, DrawIndexed; per frame
// a few large (> 128 KiB) dynamic buffers mapped WRITE_DISCARD.
//
// Every D3D11 call is timed on its own (rdtsc before and after, calibrated
// against QPC), so the report is a time budget per call class instead of one
// frame rate:
//
//   stage  calls/frame  ns/call  us/frame  %frame  p50 ns  p99 ns  max us
//
// plus the process's page faults per frame and the CPU time per frame of
// every thread of the process (DXVK's dxvk-cs, dxvk-submit, dxvk-queue, the
// UMD front-end worker, ...) by thread name.
// The GPU work is tiny (one-pixel triangles into a small target), so a run is
// CPU-bound by construction.
//
//   d3d11_mapbench.exe [key=value ...]
//     seconds=10      measured time (after warmup)
//     warmup=2        seconds before measuring (caches, pools and chunks settle)
//     draws=2000      draws per frame
//     disc=2          small dynamic constant buffers mapped DISCARD per draw
//     cbs=1024        distinct small dynamic constant buffers (sizes below)
//     sizes=mix       mix = 64 B .. 16 KiB, weighted like a game's per-draw
//                     constants; or one size in bytes for all
//     ring=1          NO_OVERWRITE appends to the 1 MiB constant ring per draw
//                     (0 = no ring, no *SetConstantBuffers1)
//     vring=1         vertex+index ring appends per draw (NO_OVERWRITE, DISCARD
//                     on wrap; 4 MiB vertex ring, 1 MiB index ring)
//     big=4           large dynamic buffers (bigkb each) mapped DISCARD per frame
//     bigkb=512       size of each large dynamic buffer
//     srvs=4          shader resources bound per draw (of 1024 textures)
//     switch=4        a VS/PS/input-layout switch every N draws
//     lag=2           frames in flight (event query of `lag` frames ago waited on)
//     size=256        render target size
//     label=TEXT      printed in the header (for logs)
//
// Environment that changes what is measured (printed in the header):
//   HELIOS_D3D11_FRONTEND=0/1               the UMD's threaded front end
//   HELIOS_DXVK_LOCAL_ALLOC_CACHE_FALLBACK=0/1  DXVK's discard-slice cache in
//                                           host-visible system memory when there
//                                           is no host-visible VRAM (DXVK 0021)
//   HELIOS_ICD                              UMD backend
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static d3d11_mapbench.cpp -o d3d11_mapbench.exe
//       -ld3d11 -ldxgi -ld3dcompiler -lpsapi
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <d3dcompiler.h>
#include <tlhelp32.h>
#include <psapi.h>
#include <x86intrin.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

static const char kShader[] = R"(
cbuffer A : register(b0) { float4 xf; float4 tint; float4 pa[2]; };
cbuffer B : register(b1) { float4 pb[4]; };
cbuffer R : register(b2) { float4 pr[16]; };
Texture2D t0 : register(t0);
Texture2D t1 : register(t1);
Texture2D t2 : register(t2);
Texture2D t3 : register(t3);
SamplerState s0 : register(s0);
SamplerState s1 : register(s1);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 c : COLOR0; };
V vs(float4 p : POSITION) {
  V o;
  o.pos = float4(xf.xy + p.xy * xf.zw, 0.5, 1);
  o.uv = p.zw + pb[K % 4].xy;
  o.c = pr[K % 16] + pa[0];
  return o;
}
float4 ps(V i) : SV_Target {
  float4 c = t0.SampleLevel(s0, i.uv, 0) + t1.SampleLevel(s1, i.uv, 0);
  c += t2.SampleLevel(s0, i.uv * K, 0) * t3.SampleLevel(s1, i.uv, 0);
  return c * tint * i.c + pb[K % 4];
}
)";

template <class T> static void release(T *&p) {
  if (p) {
    p->Release();
    p = nullptr;
  }
}

static unsigned xorshift(unsigned &s) {
  s ^= s << 13;
  s ^= s >> 17;
  s ^= s << 5;
  return s;
}

static unsigned long long page_faults() {
  PROCESS_MEMORY_COUNTERS pmc = {};
  pmc.cb = sizeof pmc;
  return GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof pmc) ? pmc.PageFaultCount : 0;
}

static double qpc_us() {
  static LARGE_INTEGER f = [] {
    LARGE_INTEGER x;
    QueryPerformanceFrequency(&x);
    return x;
  }();
  LARGE_INTEGER c;
  QueryPerformanceCounter(&c);
  return double(c.QuadPart) * 1e6 / double(f.QuadPart);
}

// --- per-stage accounting ----------------------------------------------------

enum Stage {
  S_MAP_DISCARD,     // small dynamic CB, WRITE_DISCARD
  S_MAP_NOOVERWRITE, // ring append, WRITE_NO_OVERWRITE
  S_MAP_WRAP,        // ring wrap, WRITE_DISCARD
  S_MAP_BIG,         // large dynamic buffer, WRITE_DISCARD
  S_WRITE,           // the app's own writes into the mapped memory
  S_UNMAP,
  S_SET_CB,          // *SetConstantBuffers (whole buffers)
  S_SET_CB1,         // *SetConstantBuffers1 (ring offsets)
  S_SET_SRV_SAMP,    // PSSetShaderResources + PSSetSamplers
  S_SET_VB_IB,       // IASetVertexBuffers + IASetIndexBuffer
  S_SET_SHADER,      // VS/PS/input layout switch
  S_DRAW,            // DrawIndexed
  S_END_FRAME,       // End(query) + Flush
  S_WAIT,            // GetData spin on the query of `lag` frames ago
  S_COUNT
};

static const char *kStageName[S_COUNT] = {
    "map.discard",  "map.no_overwrite", "map.wrap_discard", "map.big_discard", "app.write",
    "unmap",        "set.cb",           "set.cb1",          "set.srv+sampler", "set.vb+ib",
    "set.shader",   "draw",             "end+flush",        "wait.query",
};

struct Acc {
  unsigned long long ticks = 0, calls = 0, max = 0;
  unsigned long long hist[48] = {}; // log2(ticks) buckets
  void add(unsigned long long t) {
    ticks += t;
    calls++;
    if (t > max)
      max = t;
    hist[t ? std::min(47, 63 - __builtin_clzll(t)) : 0]++;
  }
  // Upper edge of the bucket holding the p-th quantile, in ticks.
  double quantile(double p) const {
    unsigned long long want = (unsigned long long)(p * double(calls)), n = 0;
    for (int b = 0; b < 48; b++) {
      n += hist[b];
      if (n > want)
        return double(2ull << b);
    }
    return double(max);
  }
};

static Acc g_acc[S_COUNT];
static bool g_measuring = false;

struct Timed {
  Stage s;
  unsigned long long t0;
  explicit Timed(Stage stage) : s(stage), t0(__rdtsc()) {}
  ~Timed() {
    unsigned long long t = __rdtsc() - t0;
    if (g_measuring)
      g_acc[s].add(t);
  }
};
#define TIMED_CAT2(a, b) a##b
#define TIMED_CAT(a, b) TIMED_CAT2(a, b)
#define TIMED(stage) Timed TIMED_CAT(timed_, __LINE__)(stage)

// --- per-thread CPU ------------------------------------------------------------

struct ThreadCpu {
  DWORD tid;
  std::wstring name;
  double cpu_us;
};

typedef HRESULT(WINAPI *GetThreadDescriptionFn)(HANDLE, PWSTR *);

static std::vector<ThreadCpu> sample_threads() {
  static GetThreadDescriptionFn get_desc =
      (GetThreadDescriptionFn)(void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "GetThreadDescription");
  std::vector<ThreadCpu> out;
  HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
  if (snap == INVALID_HANDLE_VALUE)
    return out;
  THREADENTRY32 te = {};
  te.dwSize = sizeof te;
  const DWORD pid = GetCurrentProcessId();
  for (BOOL ok = Thread32First(snap, &te); ok; ok = Thread32Next(snap, &te)) {
    if (te.th32OwnerProcessID != pid)
      continue;
    HANDLE th = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, FALSE, te.th32ThreadID);
    if (!th)
      continue;
    FILETIME a, b, k, u;
    double cpu = 0;
    if (GetThreadTimes(th, &a, &b, &k, &u)) {
      auto t = [](FILETIME x) { return (double(x.dwHighDateTime) * 4294967296.0 + x.dwLowDateTime) / 10.0; };
      cpu = t(k) + t(u);
    }
    std::wstring name;
    PWSTR desc = nullptr;
    if (get_desc && SUCCEEDED(get_desc(th, &desc)) && desc) {
      name = desc;
      LocalFree(desc);
    }
    CloseHandle(th);
    out.push_back({te.th32ThreadID, name, cpu});
  }
  CloseHandle(snap);
  return out;
}

// --- arguments -------------------------------------------------------------------

struct Args {
  double seconds = 10, warmup = 2;
  int draws = 2000, disc = 2, cbs = 1024, ring = 1, vring = 1, big = 4, bigkb = 512, srvs = 4, sw = 4, lag = 2,
      size = 256, fixed_size = 0;
  std::string label;
};

static bool parse(int argc, char **argv, Args &a) {
  for (int i = 1; i < argc; i++) {
    const char *eq = strchr(argv[i], '=');
    if (!eq)
      return false;
    std::string k(argv[i], eq - argv[i]);
    const char *v = eq + 1;
    if (k == "seconds") a.seconds = atof(v);
    else if (k == "warmup") a.warmup = atof(v);
    else if (k == "draws") a.draws = atoi(v);
    else if (k == "disc") a.disc = atoi(v);
    else if (k == "cbs") a.cbs = atoi(v);
    else if (k == "sizes") a.fixed_size = strcmp(v, "mix") == 0 ? 0 : atoi(v);
    else if (k == "ring") a.ring = atoi(v);
    else if (k == "vring") a.vring = atoi(v);
    else if (k == "big") a.big = atoi(v);
    else if (k == "bigkb") a.bigkb = atoi(v);
    else if (k == "srvs") a.srvs = atoi(v);
    else if (k == "switch") a.sw = atoi(v);
    else if (k == "lag") a.lag = atoi(v);
    else if (k == "size") a.size = atoi(v);
    else if (k == "label") a.label = v;
    else return false;
  }
  return a.draws >= 1 && a.disc >= 0 && a.disc <= 4 && a.cbs >= 1 && a.ring >= 0 && a.ring <= 4 && a.vring >= 0 &&
         a.vring <= 4 && a.big >= 0 && a.bigkb >= 1 && a.srvs >= 0 && a.srvs <= 4 && a.sw >= 1 && a.lag >= 1 &&
         a.lag <= 8 && a.size >= 16 && (a.fixed_size == 0 || (a.fixed_size >= 16 && a.fixed_size % 16 == 0));
}

// Small dynamic constant buffer sizes, weighted towards the 64 B - 1 KiB a
// game's per-draw constants take, with a tail up to 16 KiB (bone palettes,
// light lists).
static UINT mix_size(unsigned &seed) {
  static const UINT sizes[] = {64, 128, 256, 256, 512, 512, 1024, 1024, 2048, 4096, 16384};
  return sizes[xorshift(seed) % (sizeof sizes / sizeof sizes[0])];
}

// A dynamic ring buffer: NO_OVERWRITE appends, DISCARD when the next append
// does not fit.
struct Ring {
  ID3D11Buffer *buf = nullptr;
  UINT size = 0, pos = 0;
  // Maps `bytes` (aligned to `align`) and returns the offset, or -1.
  long long append(ID3D11DeviceContext *ctx, UINT bytes, UINT align, void **ptr) {
    UINT at = (pos + align - 1) / align * align;
    D3D11_MAPPED_SUBRESOURCE m = {};
    HRESULT hr;
    if (at + bytes > size) {
      TIMED(S_MAP_WRAP);
      hr = ctx->Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, &m);
      at = 0;
    } else {
      TIMED(S_MAP_NOOVERWRITE);
      hr = ctx->Map(buf, 0, D3D11_MAP_WRITE_NO_OVERWRITE, 0, &m);
    }
    if (FAILED(hr) || !m.pData)
      return -1;
    pos = at + bytes;
    *ptr = (char *)m.pData + at;
    return at;
  }
};

int main(int argc, char **argv) {
  // A name for thread-sample.ps1 -Thread mapbench-app.
  typedef HRESULT(WINAPI * SetThreadDescriptionFn)(HANDLE, PCWSTR);
  if (auto set_desc = (SetThreadDescriptionFn)(void *)GetProcAddress(GetModuleHandleA("kernel32.dll"),
                                                                      "SetThreadDescription"))
    set_desc(GetCurrentThread(), L"mapbench-app");

  Args a;
  if (!parse(argc, argv, a)) {
    fprintf(stderr,
            "usage: d3d11_mapbench [seconds=10] [warmup=2] [draws=2000] [disc=0..4] [cbs=1024] [sizes=mix|N]\n"
            "                      [ring=0..4] [vring=0..4] [big=4] [bigkb=512] [srvs=0..4] [switch=4]\n"
            "                      [lag=1..8] [size=256] [label=TEXT]\n");
    return 2;
  }

  ID3D11Device *dev0 = nullptr;
  ID3D11DeviceContext *ctx0 = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1;
  if (FAILED(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev0,
                               nullptr, &ctx0))) {
    fprintf(stderr, "D3D11CreateDevice (11_1) failed\n");
    return 1;
  }
  ID3D11Device1 *dev = nullptr;
  ID3D11DeviceContext1 *ctx = nullptr;
  dev0->QueryInterface(__uuidof(ID3D11Device1), (void **)&dev);
  ctx0->QueryInterface(__uuidof(ID3D11DeviceContext1), (void **)&ctx);
  release(ctx0);
  release(dev0);
  if (!dev || !ctx) {
    fprintf(stderr, "no ID3D11Device1/ID3D11DeviceContext1\n");
    return 1;
  }

  // Shaders: NV variants, switched every `switch` draws.
  enum { NV = 16, NTEX = 1024, NSAMP = 64 };
  ID3D11VertexShader *vs[NV] = {};
  ID3D11PixelShader *ps[NV] = {};
  ID3D11InputLayout *il[NV] = {};
  D3D11_INPUT_ELEMENT_DESC ied = {"POSITION", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0};
  for (int k = 0; k < NV; k++) {
    char kdef[16];
    snprintf(kdef, sizeof kdef, "%d", k + 1);
    D3D_SHADER_MACRO defs[] = {{"K", kdef}, {nullptr, nullptr}};
    for (int stage = 0; stage < 2; stage++) {
      ID3DBlob *code = nullptr, *err = nullptr;
      if (FAILED(D3DCompile(kShader, sizeof kShader - 1, "mapbench", defs, nullptr, stage ? "ps" : "vs",
                            stage ? "ps_5_0" : "vs_5_0", 0, 0, &code, &err))) {
        fprintf(stderr, "compile: %s\n", err ? (const char *)err->GetBufferPointer() : "?");
        return 1;
      }
      if (stage)
        dev->CreatePixelShader(code->GetBufferPointer(), code->GetBufferSize(), nullptr, &ps[k]);
      else {
        dev->CreateVertexShader(code->GetBufferPointer(), code->GetBufferSize(), nullptr, &vs[k]);
        dev->CreateInputLayout(&ied, 1, code->GetBufferPointer(), code->GetBufferSize(), &il[k]);
      }
      release(code);
      release(err);
    }
    if (!vs[k] || !ps[k] || !il[k]) {
      fprintf(stderr, "shader creation failed\n");
      return 1;
    }
  }

  unsigned seed = 0x2545f491u;
  std::vector<ID3D11ShaderResourceView *> srv(NTEX);
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
      if (!srv[i]) {
        fprintf(stderr, "texture creation failed\n");
        return 1;
      }
    }
  }
  ID3D11SamplerState *samp[NSAMP] = {};
  for (int i = 0; i < NSAMP; i++) {
    D3D11_SAMPLER_DESC d = {};
    d.Filter = (i & 1) ? D3D11_FILTER_MIN_MAG_MIP_LINEAR : D3D11_FILTER_MIN_MAG_MIP_POINT;
    d.AddressU = D3D11_TEXTURE_ADDRESS_MODE(1 + i % 4);
    d.AddressV = D3D11_TEXTURE_ADDRESS_MODE(1 + (i / 4) % 4);
    d.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    d.MipLODBias = float(i) / 16.0f - 2.0f;
    d.MaxLOD = D3D11_FLOAT32_MAX;
    dev->CreateSamplerState(&d, &samp[i]);
    if (!samp[i]) {
      fprintf(stderr, "sampler creation failed\n");
      return 1;
    }
  }

  auto dynamic_buffer = [&](UINT bytes, UINT bind) {
    D3D11_BUFFER_DESC d = {};
    d.ByteWidth = bytes;
    d.Usage = D3D11_USAGE_DYNAMIC;
    d.BindFlags = bind;
    d.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
    ID3D11Buffer *b = nullptr;
    dev->CreateBuffer(&d, nullptr, &b);
    return b;
  };

  std::vector<ID3D11Buffer *> cb(size_t(a.cbs));
  std::vector<UINT> cb_size(size_t(a.cbs));
  for (int i = 0; i < a.cbs; i++) {
    cb_size[size_t(i)] = a.fixed_size ? UINT(a.fixed_size) : mix_size(seed);
    cb[size_t(i)] = dynamic_buffer(cb_size[size_t(i)], D3D11_BIND_CONSTANT_BUFFER);
    if (!cb[size_t(i)]) {
      fprintf(stderr, "constant buffer creation failed\n");
      return 1;
    }
  }
  std::vector<ID3D11Buffer *> bigb(size_t(a.big));
  for (auto &b : bigb) {
    b = dynamic_buffer(UINT(a.bigkb) * 1024u, D3D11_BIND_VERTEX_BUFFER | D3D11_BIND_SHADER_RESOURCE);
    if (!b) {
      fprintf(stderr, "large dynamic buffer creation failed\n");
      return 1;
    }
  }
  Ring cring, vring, iring;
  cring.size = 1u << 20;
  vring.size = 4u << 20;
  iring.size = 1u << 20;
  cring.buf = dynamic_buffer(cring.size, D3D11_BIND_CONSTANT_BUFFER);
  vring.buf = dynamic_buffer(vring.size, D3D11_BIND_VERTEX_BUFFER);
  iring.buf = dynamic_buffer(iring.size, D3D11_BIND_INDEX_BUFFER);
  // A static vertex/index buffer for vring=0.
  ID3D11Buffer *svb = nullptr, *sib = nullptr;
  {
    float v[12] = {0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 0, 1};
    unsigned short idx[3] = {0, 1, 2};
    D3D11_BUFFER_DESC d = {};
    d.Usage = D3D11_USAGE_IMMUTABLE;
    d.ByteWidth = sizeof v;
    d.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    D3D11_SUBRESOURCE_DATA init = {v, 0, 0};
    dev->CreateBuffer(&d, &init, &svb);
    d.ByteWidth = 8;
    d.BindFlags = D3D11_BIND_INDEX_BUFFER;
    unsigned short idx4[4] = {idx[0], idx[1], idx[2], 0};
    init.pSysMem = idx4;
    dev->CreateBuffer(&d, &init, &sib);
  }
  if (!cring.buf || !vring.buf || !iring.buf || !svb || !sib) {
    fprintf(stderr, "ring buffer creation failed\n");
    return 1;
  }

  ID3D11Texture2D *rt = nullptr;
  ID3D11RenderTargetView *rtv = nullptr;
  {
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = td.Height = UINT(a.size);
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    dev->CreateTexture2D(&td, nullptr, &rt);
    if (rt)
      dev->CreateRenderTargetView(rt, nullptr, &rtv);
  }
  ID3D11Query *q[8] = {};
  D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
  for (int i = 0; i < a.lag; i++)
    dev->CreateQuery(&qd, &q[i]);
  if (!rtv || !q[a.lag - 1]) {
    fprintf(stderr, "render target / query creation failed\n");
    return 1;
  }

  // The per-draw choices of one frame, precomputed.
  struct Draw {
    unsigned short cb[4], tex[4], samp[2], var;
  };
  std::vector<Draw> seq(size_t(a.draws));
  for (Draw &d : seq) {
    for (auto &c : d.cb) c = (unsigned short)(xorshift(seed) % unsigned(a.cbs));
    for (auto &t : d.tex) t = (unsigned short)(xorshift(seed) % NTEX);
    for (auto &s : d.samp) s = (unsigned short)(xorshift(seed) % NSAMP);
    d.var = (unsigned short)(xorshift(seed) % NV);
  }

  D3D11_VIEWPORT vp = {0, 0, float(a.size), float(a.size), 0, 1};
  ctx->RSSetViewports(1, &vp);
  ctx->OMSetRenderTargets(1, &rtv, nullptr);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

  auto env = [](const char *n) {
    const char *v = getenv(n);
    return v ? v : "-";
  };
  printf("d3d11_mapbench%s%s: draws=%d disc=%d cbs=%d sizes=%s ring=%d vring=%d big=%d bigkb=%d srvs=%d switch=%d "
         "lag=%d size=%d seconds=%.0f warmup=%.0f\n",
         a.label.empty() ? "" : " ", a.label.c_str(), a.draws, a.disc, a.cbs,
         a.fixed_size ? std::to_string(a.fixed_size).c_str() : "mix", a.ring, a.vring, a.big, a.bigkb, a.srvs, a.sw,
         a.lag, a.size, a.seconds, a.warmup);
  printf("env: HELIOS_D3D11_FRONTEND=%s HELIOS_DXVK_LOCAL_ALLOC_CACHE_FALLBACK=%s HELIOS_ICD=%s\n",
         env("HELIOS_D3D11_FRONTEND"), env("HELIOS_DXVK_LOCAL_ALLOC_CACHE_FALLBACK"), env("HELIOS_ICD"));

  static unsigned char payload[64 * 1024];
  for (size_t i = 0; i < sizeof payload; i++)
    payload[i] = (unsigned char)(i * 7);
  const UINT vstride = 16;

  long long frames = 0, measured_frames = 0, sec_frames = 0;
  const double start = qpc_us();
  double measure_t0 = 0, sec_t = start;
  unsigned long long measure_tsc0 = 0, frame_ticks = 0, sec_ticks = 0;
  std::vector<ThreadCpu> th0;
  unsigned long long faults0 = 0;
  std::vector<double> frame_us;

  for (;;) {
    const unsigned long long f0 = __rdtsc();
    ID3D11Query *slot = q[frames % a.lag];
    if (frames >= a.lag) {
      TIMED(S_WAIT);
      BOOL done = FALSE;
      while (ctx->GetData(slot, &done, sizeof done, 0) != S_OK || !done)
        Sleep(0);
    }
    // Large dynamic buffers: a skinning / particle upload per frame each.
    for (auto *b : bigb) {
      D3D11_MAPPED_SUBRESOURCE m = {};
      HRESULT hr;
      {
        TIMED(S_MAP_BIG);
        hr = ctx->Map(b, 0, D3D11_MAP_WRITE_DISCARD, 0, &m);
      }
      if (SUCCEEDED(hr) && m.pData) {
        {
          TIMED(S_WRITE);
          for (UINT off = 0; off < UINT(a.bigkb) * 1024u; off += sizeof payload)
            memcpy((char *)m.pData + off, payload, std::min<UINT>(sizeof payload, UINT(a.bigkb) * 1024u - off));
        }
        TIMED(S_UNMAP);
        ctx->Unmap(b, 0);
      }
    }
    for (int i = 0; i < a.draws; i++) {
      const Draw &d = seq[size_t(i)];
      if (i % a.sw == 0) {
        TIMED(S_SET_SHADER);
        ctx->VSSetShader(vs[d.var], nullptr, 0);
        ctx->PSSetShader(ps[d.var], nullptr, 0);
        ctx->IASetInputLayout(il[d.var]);
      }
      // Small per-draw constant buffers, DISCARD.
      ID3D11Buffer *bound[4] = {};
      for (int c = 0; c < a.disc; c++) {
        ID3D11Buffer *b = cb[d.cb[c]];
        const UINT bytes = cb_size[d.cb[c]];
        D3D11_MAPPED_SUBRESOURCE m = {};
        HRESULT hr;
        {
          TIMED(S_MAP_DISCARD);
          hr = ctx->Map(b, 0, D3D11_MAP_WRITE_DISCARD, 0, &m);
        }
        if (SUCCEEDED(hr) && m.pData) {
          {
            TIMED(S_WRITE);
            memcpy(m.pData, payload, bytes);
            float *f = (float *)m.pData;
            f[0] = float(i % 64) / 32.0f - 1.0f;
            f[1] = float(i / 64 % 64) / 32.0f - 1.0f;
            f[2] = f[3] = 1.0f / 256.0f;
          }
          TIMED(S_UNMAP);
          ctx->Unmap(b, 0);
        }
        bound[c] = b;
      }
      if (a.disc > 0) {
        TIMED(S_SET_CB);
        ctx->VSSetConstantBuffers(0, UINT(std::min(a.disc, 2)), bound);
        ctx->PSSetConstantBuffers(0, UINT(std::min(a.disc, 2)), bound);
      }
      // Constant ring: 256 B per append, bound by offset (16-constant units).
      for (int r = 0; r < a.ring; r++) {
        void *p = nullptr;
        long long at = cring.append(ctx, 256, 256, &p);
        if (at < 0)
          continue;
        {
          TIMED(S_WRITE);
          memcpy(p, payload, 256);
        }
        {
          TIMED(S_UNMAP);
          ctx->Unmap(cring.buf, 0);
        }
        if (r == 0) {
          UINT first = UINT(at / 16), num = 16;
          TIMED(S_SET_CB1);
          ctx->VSSetConstantBuffers1(2, 1, &cring.buf, &first, &num);
          ctx->PSSetConstantBuffers1(2, 1, &cring.buf, &first, &num);
        }
      }
      // Geometry: 64 vertices + 96 indices appended per ring step; the draw
      // uses the first triangle.
      UINT vfirst = 0, ifirst = 0;
      for (int r = 0; r < a.vring; r++) {
        void *p = nullptr;
        long long vat = vring.append(ctx, 64 * vstride, vstride, &p);
        if (vat >= 0) {
          {
            TIMED(S_WRITE);
            memcpy(p, payload, 64 * vstride);
          }
          TIMED(S_UNMAP);
          ctx->Unmap(vring.buf, 0);
        }
        long long iat = iring.append(ctx, 96 * 2, 4, &p);
        if (iat >= 0) {
          {
            TIMED(S_WRITE);
            unsigned short *ix = (unsigned short *)p;
            for (int k = 0; k < 96; k++)
              ix[k] = (unsigned short)(k % 3);
          }
          TIMED(S_UNMAP);
          ctx->Unmap(iring.buf, 0);
        }
        if (r == 0 && vat >= 0 && iat >= 0) {
          vfirst = UINT(vat) / vstride;
          ifirst = UINT(iat) / 2;
        }
      }
      {
        TIMED(S_SET_VB_IB);
        UINT offset = 0;
        ID3D11Buffer *vb = a.vring ? vring.buf : svb;
        ctx->IASetVertexBuffers(0, 1, &vb, &vstride, &offset);
        ctx->IASetIndexBuffer(a.vring ? iring.buf : sib, DXGI_FORMAT_R16_UINT, 0);
      }
      if (a.srvs > 0) {
        TIMED(S_SET_SRV_SAMP);
        ID3D11ShaderResourceView *views[4] = {srv[d.tex[0]], srv[d.tex[1]], srv[d.tex[2]], srv[d.tex[3]]};
        ctx->PSSetShaderResources(0, UINT(a.srvs), views);
        ID3D11SamplerState *ss[2] = {samp[d.samp[0]], samp[d.samp[1]]};
        ctx->PSSetSamplers(0, 2, ss);
      }
      {
        TIMED(S_DRAW);
        ctx->DrawIndexed(3, ifirst, INT(vfirst));
      }
    }
    {
      TIMED(S_END_FRAME);
      ctx->End(slot);
      ctx->Flush();
    }
    frames++;
    const unsigned long long f1 = __rdtsc();
    const double now = qpc_us();
    if (!g_measuring && now - start >= a.warmup * 1e6) {
      g_measuring = true;
      measure_t0 = now;
      measure_tsc0 = f1;
      sec_t = now;
      th0 = sample_threads();
      faults0 = page_faults();
      continue;
    }
    if (!g_measuring)
      continue;
    measured_frames++;
    sec_frames++;
    frame_ticks += f1 - f0;
    sec_ticks += f1 - f0;
    frame_us.push_back(double(f1 - f0));
    if (now - sec_t >= 1e6) {
      const double tick_us = (now - measure_t0) / double(f1 - measure_tsc0);
      printf("  %6.0f fps  frame %7.1f us\n", double(sec_frames) * 1e6 / (now - sec_t),
             double(sec_ticks) * tick_us / double(sec_frames));
      sec_t = now;
      sec_frames = 0;
      sec_ticks = 0;
    }
    if (now - measure_t0 >= a.seconds * 1e6)
      break;
  }

  const double measure_t1 = qpc_us();
  const unsigned long long measure_tsc1 = __rdtsc();
  std::vector<ThreadCpu> th1 = sample_threads();
  const unsigned long long faults1 = page_faults();
  const double tick_us = (measure_t1 - measure_t0) / double(measure_tsc1 - measure_tsc0);
  const double nf = double(measured_frames);
  const double frame_mean_us = double(frame_ticks) * tick_us / nf;

  printf("\n%lld frames in %.1f s: %.0f fps, frame %.1f us (TSC %.3f GHz)\n", measured_frames,
         (measure_t1 - measure_t0) / 1e6, nf * 1e6 / (measure_t1 - measure_t0), frame_mean_us, 1e-3 / tick_us);
  printf("page faults: %.1f per frame (process)\n", double(faults1 - faults0) / nf);
  printf("%-18s %11s %9s %10s %7s %9s %9s %9s\n", "stage", "calls/frame", "ns/call", "us/frame", "%frame", "p50 ns",
         "p99 ns", "max us");
  double timed_us = 0;
  for (int s = 0; s < S_COUNT; s++) {
    const Acc &x = g_acc[s];
    if (!x.calls)
      continue;
    const double us = double(x.ticks) * tick_us / nf;
    timed_us += us;
    printf("%-18s %11.1f %9.0f %10.1f %6.1f%% %9.0f %9.0f %9.1f\n", kStageName[s], double(x.calls) / nf,
           double(x.ticks) * tick_us * 1e3 / double(x.calls), us, 100.0 * us / frame_mean_us,
           x.quantile(0.5) * tick_us * 1e3, x.quantile(0.99) * tick_us * 1e3, double(x.max) * tick_us);
  }
  printf("%-18s %11s %9s %10.1f %6.1f%%\n", "untimed", "", "", frame_mean_us - timed_us,
         100.0 * (frame_mean_us - timed_us) / frame_mean_us);
  printf("%-18s %11s %9s %10.1f %6.1f%%\n", "frame", "", "", frame_mean_us, 100.0);

  // Frame-time spread.
  if (frame_us.size() > 2) {
    std::sort(frame_us.begin(), frame_us.end());
    auto pct = [&](double p) { return frame_us[std::min(frame_us.size() - 1, size_t(p * double(frame_us.size() - 1)))] * tick_us; };
    printf("frame us: p50 %.0f p90 %.0f p99 %.0f max %.0f\n", pct(0.5), pct(0.9), pct(0.99),
           frame_us.back() * tick_us);
  }

  // Threads: CPU per frame over the measured interval, busiest first.
  struct Row {
    DWORD tid;
    std::wstring name;
    double us;
  };
  std::vector<Row> rows;
  for (const ThreadCpu &t1 : th1) {
    double before = 0;
    for (const ThreadCpu &t0 : th0)
      if (t0.tid == t1.tid)
        before = t0.cpu_us;
    rows.push_back({t1.tid, t1.name, (t1.cpu_us - before) / nf});
  }
  std::sort(rows.begin(), rows.end(), [](const Row &x, const Row &y) { return x.us > y.us; });
  printf("threads (cpu us/frame, %% of frame):\n");
  int shown = 0;
  for (const Row &r : rows) {
    if (shown++ >= 10 || r.us < 1.0)
      break;
    printf("  %-24ls %6lu %9.1f %6.1f%%\n", r.name.empty() ? L"?" : r.name.c_str(), (unsigned long)r.tid, r.us,
           100.0 * r.us / frame_mean_us);
  }

  ctx->ClearState();
  ctx->Flush();
  for (auto *p : srv) release(p);
  for (auto *&p : samp) release(p);
  for (auto *p : cb) release(p);
  for (auto *p : bigb) release(p);
  release(cring.buf);
  release(vring.buf);
  release(iring.buf);
  release(svb);
  release(sib);
  for (int k = 0; k < NV; k++) {
    release(vs[k]);
    release(ps[k]);
    release(il[k]);
  }
  for (int i = 0; i < a.lag; i++)
    release(q[i]);
  release(rtv);
  release(rt);
  release(ctx);
  release(dev);
  return 0;
}
