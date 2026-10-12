// d3d11_shader_corpus.cpp: does every shader the D3D11 runtime is handed get
// created on the Helios adapter, for every stage, and do the ones that need
// optional features run correctly?
//
// Two layers can refuse a shader. d3d11.dll compares the container's SFI0
// flags (D3D_SHADER_REQUIRES_*) with the device caps and fails Create*Shader
// with E_INVALIDARG before the driver sees it; the UMD's engine (DXVK) can
// fail the compile. The first leaves no trace in any driver log, the second
// is a line in C:\ProgramData\Helios\shader-failures.log. This tool drives
// both through the real API and names the cause of each failure.
//
// Shaders:
//   --builtin      HLSL compiled here (d3dcompiler_47): a plain shader for each
//                  of VS/HS/DS/GS/PS/CS plus one per SFI0 requirement class
//                  (typed UAV loads of additional formats, doubles, extended
//                  doubles, SAD4, UAVs at every stage, 64 UAVs, min precision,
//                  stencil ref, ROVs, VP/RT index from VS, tiled resources,
//                  inner coverage, early depth-stencil).
//   --dir PATH     every *.dxbc / *.cso below PATH (recursive). The stage comes
//                  from the SHDR/SHEX program type. A corpus dumped from a game
//                  stays on the test machine; nothing from one is committed.
//   --every K      only every K-th corpus file; --limit N stops after N.
//   --dispatch     run the builtin compute cases that have a checkable result
//                  (typed UAV load of R16G16B16A16_FLOAT, doubles) and compare
//                  the read-back with the expected values.
//
// A failure is UNEXPECTED when the device reports every feature the shader's
// SFI0 flags require (or when the driver ledger gained a line): that is a bug
// in the driver stack. It is UNSUPPORTED when a required feature is reported
// absent: the shader cannot exist on this device, and the cap list printed at
// the start says which one. Exit code: UNEXPECTED count (plus UNSUPPORTED with
// --strict, plus dispatch mismatches), 0 when clean.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -municode -static -o d3d11_shader_corpus.exe d3d11_shader_corpus.cpp
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_shader_corpus.exe [--warp|--default] [--builtin] [--dispatch]
//              [--dir PATH [--every K] [--limit N]] [--strict] [--quiet]
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_3.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <string>
#include <vector>

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

// ---- DXBC facts -------------------------------------------------------------

struct Info { bool ok = false; uint64_t req = 0; int type = -1; int major = 0, minor = 0; };

static Info Parse(const uint8_t* b, size_t n) {
  Info info;
  auto u32 = [&](size_t off) -> uint32_t { uint32_t v; memcpy(&v, b + off, 4); return v; };
  if (n < 32 || memcmp(b, "DXBC", 4) != 0) return info;
  uint32_t total = u32(24), count = u32(28);
  if (total > n || total < 32 || count > 64 || 32 + 4ull * count > total) return info;
  for (uint32_t i = 0; i < count; ++i) {
    uint32_t off = u32(32 + 4 * i);
    if (off + 8ull > total) return info;
    uint32_t size = u32(off + 4);
    if (off + 8ull + size > total) return info;
    if (!memcmp(b + off, "SFI0", 4) && size >= 8) memcpy(&info.req, b + off + 8, 8);
    if ((!memcmp(b + off, "SHDR", 4) || !memcmp(b + off, "SHEX", 4)) && size >= 4) {
      uint32_t v = u32(off + 8);
      info.type = (int)(v >> 16); info.major = (v >> 4) & 0xf; info.minor = v & 0xf;
    }
  }
  info.ok = true;
  return info;
}

static const char* kStage[] = {"ps", "vs", "gs", "hs", "ds", "cs"};

struct Req { uint64_t bit; const char* name; };
static const Req kReq[] = {
  {0x1, "DOUBLES"}, {0x2, "EARLY_DEPTH_STENCIL"}, {0x4, "UAVS_AT_EVERY_STAGE"}, {0x8, "64_UAVS"},
  {0x10, "MINIMUM_PRECISION"}, {0x20, "11_1_DOUBLE_EXTENSIONS"}, {0x40, "11_1_SHADER_EXTENSIONS"},
  {0x80, "LEVEL_9_COMPARISON_FILTERING"}, {0x100, "TILED_RESOURCES"}, {0x200, "STENCIL_REF"},
  {0x400, "INNER_COVERAGE"}, {0x800, "TYPED_UAV_LOAD_ADDITIONAL_FORMATS"}, {0x1000, "ROVS"},
  {0x2000, "VP_AND_RT_ARRAY_INDEX_FROM_ANY_SHADER_FEEDING_RASTERIZER"},
};

static std::string Names(uint64_t f) {
  if (!f) return "-";
  std::string s; uint64_t rest = f;
  for (auto& r : kReq) if (f & r.bit) { if (!s.empty()) s += "|"; s += r.name; rest &= ~r.bit; }
  if (rest) { char t[32]; snprintf(t, sizeof t, "%s0x%llx", s.empty() ? "" : "|", (unsigned long long)rest); s += t; }
  return s;
}

// ---- what the runtime says the device supports ------------------------------

struct Caps {
  D3D_FEATURE_LEVEL fl{};
  D3D11_FEATURE_DATA_DOUBLES doubles{};
  D3D11_FEATURE_DATA_D3D11_OPTIONS o{};
  D3D11_FEATURE_DATA_D3D11_OPTIONS1 o1{};
  D3D11_FEATURE_DATA_D3D11_OPTIONS2 o2{};
  D3D11_FEATURE_DATA_D3D11_OPTIONS3 o3{};
  D3D11_FEATURE_DATA_SHADER_MIN_PRECISION_SUPPORT mp{};

  void Query(ID3D11Device* d) {
    fl = d->GetFeatureLevel();
    d->CheckFeatureSupport(D3D11_FEATURE_DOUBLES, &doubles, sizeof doubles);
    d->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS, &o, sizeof o);
    d->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS1, &o1, sizeof o1);
    d->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS2, &o2, sizeof o2);
    d->CheckFeatureSupport(D3D11_FEATURE_D3D11_OPTIONS3, &o3, sizeof o3);
    d->CheckFeatureSupport(D3D11_FEATURE_SHADER_MIN_PRECISION_SUPPORT, &mp, sizeof mp);
  }
  // The SFI0 bits whose feature the device reports absent.
  uint64_t Missing(uint64_t req) const {
    uint64_t m = 0;
    if ((req & 0x1) && !doubles.DoublePrecisionFloatShaderOps) m |= 0x1;
    if ((req & 0x4) && fl < D3D_FEATURE_LEVEL_11_1) m |= 0x4;
    if ((req & 0x8) && fl < D3D_FEATURE_LEVEL_11_1) m |= 0x8;
    if ((req & 0x20) && !o.ExtendedDoublesShaderInstructions) m |= 0x20;
    if ((req & 0x40) && !o.SAD4ShaderInstructions) m |= 0x40;
    if ((req & 0x100) && o1.TiledResourcesTier == D3D11_TILED_RESOURCES_NOT_SUPPORTED) m |= 0x100;
    if ((req & 0x200) && !o2.PSSpecifiedStencilRefSupported) m |= 0x200;
    if ((req & 0x400) && o2.ConservativeRasterizationTier < D3D11_CONSERVATIVE_RASTERIZATION_TIER_3) m |= 0x400;
    if ((req & 0x800) && !o2.TypedUAVLoadAdditionalFormats) m |= 0x800;
    if ((req & 0x1000) && !o2.ROVsSupported) m |= 0x1000;
    if ((req & 0x2000) && !o3.VPAndRTArrayIndexFromAnyShaderFeedingRasterizer) m |= 0x2000;
    return m;
  }
  void Print() const {
    printf("caps: FL=%x doubles=%d extDoubles=%d SAD4=%d tiledTier=%d typedUAVLoadAdditional=%d ROVs=%d "
           "stencilRef=%d conservativeTier=%d vpRtAnyStage=%d minPrecision ps=%u other=%u\n",
           fl, doubles.DoublePrecisionFloatShaderOps, o.ExtendedDoublesShaderInstructions, o.SAD4ShaderInstructions,
           o1.TiledResourcesTier, o2.TypedUAVLoadAdditionalFormats, o2.ROVsSupported, o2.PSSpecifiedStencilRefSupported,
           o2.ConservativeRasterizationTier, o3.VPAndRTArrayIndexFromAnyShaderFeedingRasterizer,
           mp.PixelShaderMinPrecision, mp.AllOtherShaderStagesMinPrecision);
  }
};

// ---- create -----------------------------------------------------------------

static HRESULT Create(ID3D11Device* d, int type, const void* p, size_t n) {
  HRESULT hr = E_INVALIDARG;
  switch (type) {
    case 0: { ID3D11PixelShader* s = nullptr; hr = d->CreatePixelShader(p, n, nullptr, &s); if (s) s->Release(); break; }
    case 1: { ID3D11VertexShader* s = nullptr; hr = d->CreateVertexShader(p, n, nullptr, &s); if (s) s->Release(); break; }
    case 2: { ID3D11GeometryShader* s = nullptr; hr = d->CreateGeometryShader(p, n, nullptr, &s); if (s) s->Release(); break; }
    case 3: { ID3D11HullShader* s = nullptr; hr = d->CreateHullShader(p, n, nullptr, &s); if (s) s->Release(); break; }
    case 4: { ID3D11DomainShader* s = nullptr; hr = d->CreateDomainShader(p, n, nullptr, &s); if (s) s->Release(); break; }
    case 5: { ID3D11ComputeShader* s = nullptr; hr = d->CreateComputeShader(p, n, nullptr, &s); if (s) s->Release(); break; }
  }
  return hr;
}

static const wchar_t kLedger[] = L"C:\\ProgramData\\Helios\\shader-failures.log";

static long long LedgerSize() {
  WIN32_FILE_ATTRIBUTE_DATA a{};
  if (!GetFileAttributesExW(kLedger, GetFileExInfoStandard, &a)) return 0;
  return ((long long)a.nFileSizeHigh << 32) | a.nFileSizeLow;
}

static std::string LedgerSince(long long off) {
  HANDLE h = CreateFileW(kLedger, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING, 0, nullptr);
  if (h == INVALID_HANDLE_VALUE) return "";
  LARGE_INTEGER li; li.QuadPart = off; SetFilePointerEx(h, li, nullptr, FILE_BEGIN);
  std::string s; char buf[4096]; DWORD got = 0;
  while (ReadFile(h, buf, sizeof buf, &got, nullptr) && got) s.append(buf, got);
  CloseHandle(h);
  return s;
}

struct Tally {
  int created = 0, unexpected = 0, unsupported = 0, skipped = 0, mismatches = 0;
  int perStage[6][3] = {};  // created, unexpected, unsupported
};

static bool g_quiet = false;

static void Try(ID3D11Device* d, const Caps& caps, Tally& t, const char* name, const void* p, size_t n, int forceType = -1) {
  Info info = Parse((const uint8_t*)p, n);
  int type = forceType >= 0 ? forceType : info.type;
  if (!info.ok || type < 0 || type > 5) { t.skipped++; if (!g_quiet) printf("SKIP %s: not a DXBC shader\n", name); return; }
  long long before = LedgerSize();
  HRESULT hr = Create(d, type, p, n);
  long long after = LedgerSize();
  bool ledger = after > before;
  if (SUCCEEDED(hr) && !ledger) { t.created++; t.perStage[type][0]++; return; }
  uint64_t missing = caps.Missing(info.req);
  bool unsupported = FAILED(hr) && missing && !ledger;
  if (unsupported) { t.unsupported++; t.perStage[type][2]++; }
  else { t.unexpected++; t.perStage[type][1]++; }
  printf("%s %s %s sm%d.%d hr=0x%08lx requires=%s%s%s\n", unsupported ? "UNSUPPORTED" : "UNEXPECTED", kStage[type], name,
         info.major, info.minor, (unsigned long)hr, Names(info.req).c_str(),
         missing ? (" missing=" + Names(missing)).c_str() : "", ledger ? " (driver ledger line)" : "");
  if (ledger) printf("  ledger: %s", LedgerSince(before).c_str());
}

// ---- builtin cases ----------------------------------------------------------

struct Case { const char* name; const char* target; const char* src; uint64_t expect; };

static const Case kCases[] = {
  {"plain_vs", "vs_5_0", "float4 main(float4 p : POSITION) : SV_Position { return p; }", 0},
  {"plain_ps", "ps_5_0", "Texture2D t; SamplerState s; float4 main(float4 p : SV_Position, float2 uv : TEXCOORD) : SV_Target { return t.Sample(s, uv); }", 0},
  {"plain_gs", "gs_5_0",
   "struct V { float4 p : SV_Position; };\n"
   "[maxvertexcount(3)] void main(triangle V i[3], inout TriangleStream<V> o) { for (int k = 0; k < 3; ++k) o.Append(i[k]); }", 0},
  {"plain_hs", "hs_5_0",
   "struct V { float4 p : POS; }; struct C { float e[3] : SV_TessFactor; float i : SV_InsideTessFactor; };\n"
   "C pc(InputPatch<V,3> ip) { C c; c.e[0] = c.e[1] = c.e[2] = 2; c.i = 2; return c; }\n"
   "[domain(\"tri\")] [partitioning(\"integer\")] [outputtopology(\"triangle_cw\")] [outputcontrolpoints(3)] [patchconstantfunc(\"pc\")]\n"
   "V main(InputPatch<V,3> ip, uint i : SV_OutputControlPointID) { return ip[i]; }", 0},
  {"plain_ds", "ds_5_0",
   "struct V { float4 p : POS; }; struct C { float e[3] : SV_TessFactor; float i : SV_InsideTessFactor; };\n"
   "[domain(\"tri\")] float4 main(C c, float3 b : SV_DomainLocation, const OutputPatch<V,3> p) : SV_Position {\n"
   "  return p[0].p * b.x + p[1].p * b.y + p[2].p * b.z; }", 0},
  {"plain_cs", "cs_5_0", "RWStructuredBuffer<uint> u; [numthreads(64,1,1)] void main(uint i : SV_DispatchThreadID) { u[i] = i; }", 0},
  {"typed_uav_load_rgba16f", "cs_5_0",
   "RWTexture2D<float4> u : register(u0); [numthreads(8,8,1)] void main(uint2 i : SV_DispatchThreadID) { u[i] = u[i] * 2 + 1; }", 0x800},
  {"typed_uav_load_ps", "ps_5_0",
   "RWTexture2D<float4> u : register(u1); float4 main(float4 p : SV_Position) : SV_Target { return u[uint2(p.xy)]; }", 0x800},
  {"doubles", "cs_5_0",
   "RWStructuredBuffer<uint2> u; [numthreads(1,1,1)] void main(uint i : SV_DispatchThreadID) {\n"
   "  double a = asdouble(u[0].x, u[0].y); double b = a * 3.0 + 0.25; uint lo, hi; asuint(b, lo, hi); u[1] = uint2(lo, hi); }", 0x1},
  {"extended_doubles", "cs_5_0",
   "RWStructuredBuffer<uint2> u; [numthreads(1,1,1)] void main() {\n"
   "  double a = asdouble(u[0].x, u[0].y); double b = a / 3.0; uint lo, hi; asuint(b, lo, hi); u[2] = uint2(lo, hi); }", 0x21},
  {"sad4", "cs_5_0", "RWStructuredBuffer<uint4> u; [numthreads(1,1,1)] void main() { u[1] = msad4(u[0].x, u[0].yz, u[0]); }", 0x40},
  {"uav_in_vs", "vs_5_0",
   "RWStructuredBuffer<uint> u : register(u1); float4 main(float4 p : POSITION, uint v : SV_VertexID) : SV_Position { u[v] = v; return p; }", 0x4},
  {"64_uavs", "cs_5_0", "RWStructuredBuffer<uint> u : register(u63); [numthreads(1,1,1)] void main() { u[0] = 1; }", 0x8},
  {"min_precision", "ps_5_0", "min16float4 main(min16float4 c : COLOR) : SV_Target { return c * (min16float)0.5; }", 0x10},
  {"stencil_ref", "ps_5_0", "uint main(float4 p : SV_Position) : SV_StencilRef { return 3; }", 0x200},
  {"rov", "ps_5_0",
   "RasterizerOrderedTexture2D<float4> r : register(u1); float4 main(float4 p : SV_Position) : SV_Target {\n"
   "  r[uint2(p.xy)] = r[uint2(p.xy)] + 1; return 0; }", 0x1000},
  {"vp_rt_index_from_vs", "vs_5_0",
   "struct O { float4 p : SV_Position; uint rt : SV_RenderTargetArrayIndex; uint vp : SV_ViewportArrayIndex; };\n"
   "O main(float4 p : POSITION, uint i : SV_InstanceID) { O o; o.p = p; o.rt = i; o.vp = i; return o; }", 0x2000},
  {"tiled_resources", "ps_5_0",
   "Texture2D t; SamplerState s; float4 main(float4 p : SV_Position, float2 uv : TEXCOORD) : SV_Target {\n"
   "  uint st; float4 c = t.Sample(s, uv, int2(0,0), 0.0, st); return CheckAccessFullyMapped(st) ? c : 0; }", 0x100},
  {"inner_coverage", "ps_5_0", "float4 main(float4 p : SV_Position, uint c : SV_InnerCoverage) : SV_Target { return c; }", 0x400},
  {"early_depth_stencil", "ps_5_0", "[earlydepthstencil] float4 main(float4 p : SV_Position) : SV_Target { return 1; }", 0},
};

static ID3DBlob* Compile(const Case& c) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(c.src, strlen(c.src), c.name, nullptr, nullptr, "main", c.target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &b, &e))) {
    printf("COMPILE-SKIP %s: %s\n", c.name, e ? (const char*)e->GetBufferPointer() : "?");
    if (e) e->Release();
    return nullptr;
  }
  if (e) e->Release();
  return b;
}

// ---- dispatch checks ----------------------------------------------------------

static uint16_t Half(float f) {
  uint32_t x; memcpy(&x, &f, 4);
  uint32_t s = (x >> 16) & 0x8000, e = (x >> 23) & 0xff, m = x & 0x7fffff;
  int he = (int)e - 127 + 15;
  if (e == 0 || he <= 0) return (uint16_t)s;
  if (he >= 31) return (uint16_t)(s | 0x7c00);
  return (uint16_t)(s | (he << 10) | (m >> 13));
}
static float FromHalf(uint16_t h) {
  uint32_t s = (h & 0x8000u) << 16, e = (h >> 10) & 0x1f, m = h & 0x3ff, x;
  if (e == 0) x = s; else if (e == 31) x = s | 0x7f800000 | (m << 13); else x = s | ((e - 15 + 127) << 23) | (m << 13);
  float f; memcpy(&f, &x, 4); return f;
}

// u[i] = u[i] * 2 + 1 on an R16G16B16A16_FLOAT UAV: every texel must come back right.
static int DispatchTypedLoad(ID3D11Device* d, ID3D11DeviceContext* ctx, ID3DBlob* cs) {
  const int W = 16, H = 16;
  std::vector<uint16_t> init(W * H * 4);
  for (int i = 0; i < W * H * 4; ++i) init[i] = Half((float)(i % 61) * 0.25f);
  D3D11_TEXTURE2D_DESC td{W, H, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, {1, 0}, D3D11_USAGE_DEFAULT, D3D11_BIND_UNORDERED_ACCESS, 0, 0};
  D3D11_SUBRESOURCE_DATA sd{init.data(), W * 8, 0};
  ID3D11Texture2D* tex = nullptr; ID3D11UnorderedAccessView* uav = nullptr; ID3D11ComputeShader* s = nullptr;
  if (FAILED(d->CreateTexture2D(&td, &sd, &tex)) || FAILED(d->CreateUnorderedAccessView(tex, nullptr, &uav)) ||
      FAILED(d->CreateComputeShader(cs->GetBufferPointer(), cs->GetBufferSize(), nullptr, &s))) {
    printf("DISPATCH typed_uav_load_rgba16f: setup failed\n"); return 1;
  }
  ctx->CSSetShader(s, nullptr, 0); ctx->CSSetUnorderedAccessViews(0, 1, &uav, nullptr); ctx->Dispatch(W / 8, H / 8, 1);
  ID3D11UnorderedAccessView* none = nullptr; ctx->CSSetUnorderedAccessViews(0, 1, &none, nullptr);
  td.Usage = D3D11_USAGE_STAGING; td.BindFlags = 0; td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* st = nullptr; d->CreateTexture2D(&td, nullptr, &st); ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{}; int bad = 0;
  if (SUCCEEDED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) {
    for (int y = 0; y < H; ++y) for (int x = 0; x < W * 4; ++x) {
      float want = FromHalf(init[y * W * 4 + x]) * 2 + 1;
      float got = FromHalf(((uint16_t*)((uint8_t*)m.pData + y * m.RowPitch))[x]);
      if (fabsf(got - want) > 0.01f) { if (bad < 4) printf("  texel %d,%d.%d: got %g want %g\n", x / 4, y, x % 4, got, want); bad++; }
    }
    ctx->Unmap(st, 0);
  } else bad = W * H * 4;
  printf("DISPATCH typed_uav_load_rgba16f: %s (%d bad of %d)\n", bad ? "MISMATCH" : "ok", bad, W * H * 4);
  st->Release(); s->Release(); uav->Release(); tex->Release();
  return bad ? 1 : 0;
}

// u[1] = u[0] * 3 + 0.25 and u[2] = u[0] / 3 in double precision.
static int DispatchDoubles(ID3D11Device* d, ID3D11DeviceContext* ctx, ID3DBlob* mul, ID3DBlob* div) {
  double a = 1234.5678901234567;
  uint32_t init[6] = {}; memcpy(init, &a, 8);
  D3D11_BUFFER_DESC bd{sizeof init, D3D11_USAGE_DEFAULT, D3D11_BIND_UNORDERED_ACCESS, 0, D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, 8};
  D3D11_SUBRESOURCE_DATA sd{init, 0, 0};
  ID3D11Buffer* buf = nullptr; ID3D11UnorderedAccessView* uav = nullptr;
  if (FAILED(d->CreateBuffer(&bd, &sd, &buf)) || FAILED(d->CreateUnorderedAccessView(buf, nullptr, &uav))) {
    printf("DISPATCH doubles: setup failed\n"); return 1;
  }
  int run = 0;
  for (ID3DBlob* b : {mul, div}) {
    ID3D11ComputeShader* s = nullptr;
    if (!b || FAILED(d->CreateComputeShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &s))) continue;
    ctx->CSSetShader(s, nullptr, 0); ctx->CSSetUnorderedAccessViews(0, 1, &uav, nullptr); ctx->Dispatch(1, 1, 1);
    s->Release(); run++;
  }
  ID3D11UnorderedAccessView* none = nullptr; ctx->CSSetUnorderedAccessViews(0, 1, &none, nullptr);
  bd.Usage = D3D11_USAGE_STAGING; bd.BindFlags = 0; bd.CPUAccessFlags = D3D11_CPU_ACCESS_READ; bd.MiscFlags = 0;
  ID3D11Buffer* st = nullptr; d->CreateBuffer(&bd, nullptr, &st); ctx->CopyResource(st, buf);
  D3D11_MAPPED_SUBRESOURCE m{}; int bad = 0;
  if (run && SUCCEEDED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) {
    double r[3]; memcpy(r, m.pData, sizeof r);
    if (mul && r[1] != a * 3.0 + 0.25) { printf("  mul: got %.17g want %.17g\n", r[1], a * 3.0 + 0.25); bad++; }
    if (div && fabs(r[2] - a / 3.0) > 1e-12 * fabs(a)) { printf("  div: got %.17g want %.17g\n", r[2], a / 3.0); bad++; }
    ctx->Unmap(st, 0);
  } else if (run) bad = 1;
  printf("DISPATCH doubles: %s (%d shaders run)\n", run == 0 ? "not run" : bad ? "MISMATCH" : "ok", run);
  st->Release(); uav->Release(); buf->Release();
  return bad;
}

// ---- corpus walk --------------------------------------------------------------

static void Walk(const std::wstring& dir, std::vector<std::wstring>& out) {
  WIN32_FIND_DATAW fd; HANDLE h = FindFirstFileW((dir + L"\\*").c_str(), &fd);
  if (h == INVALID_HANDLE_VALUE) return;
  do {
    std::wstring n = fd.cFileName;
    if (n == L"." || n == L"..") continue;
    std::wstring p = dir + L"\\" + n;
    if (fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) Walk(p, out);
    else {
      size_t dot = n.rfind(L'.');
      std::wstring ext = dot == std::wstring::npos ? L"" : n.substr(dot);
      if (_wcsicmp(ext.c_str(), L".dxbc") == 0 || _wcsicmp(ext.c_str(), L".cso") == 0) out.push_back(p);
    }
  } while (FindNextFileW(h, &fd));
  FindClose(h);
}

static bool ReadAll(const std::wstring& p, std::vector<uint8_t>& out) {
  HANDLE h = CreateFileW(p.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, 0, nullptr);
  if (h == INVALID_HANDLE_VALUE) return false;
  LARGE_INTEGER sz; GetFileSizeEx(h, &sz);
  out.resize((size_t)sz.QuadPart); DWORD got = 0;
  bool ok = sz.QuadPart < (1 << 26) && ReadFile(h, out.data(), (DWORD)out.size(), &got, nullptr) && got == out.size();
  CloseHandle(h);
  return ok;
}

int wmain(int argc, wchar_t** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  bool warp = false, anyhw = false, builtin = false, dispatch = false, strict = false;
  const wchar_t* dir = nullptr; int every = 1; long limit = -1;
  for (int i = 1; i < argc; ++i) {
    std::wstring a = argv[i];
    if (a == L"--warp") warp = true;
    else if (a == L"--default") anyhw = true;
    else if (a == L"--builtin") builtin = true;
    else if (a == L"--dispatch") dispatch = builtin = true;
    else if (a == L"--strict") strict = true;
    else if (a == L"--quiet") g_quiet = true;
    else if (a == L"--dir" && i + 1 < argc) dir = argv[++i];
    else if (a == L"--every" && i + 1 < argc) every = _wtoi(argv[++i]);
    else if (a == L"--limit" && i + 1 < argc) limit = _wtol(argv[++i]);
    else { printf("unknown argument %ls\n", argv[i]); return 64; }
  }
  if (!builtin && !dir) builtin = true;
  if (every < 1) every = 1;

  IDXGIAdapter1* adapter = warp || anyhw ? nullptr : HeliosAdapter();
  if (!warp && !anyhw && !adapter) { printf("no Helios adapter\n"); return 2; }
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : anyhw ? D3D_DRIVER_TYPE_HARDWARE : D3D_DRIVER_TYPE_UNKNOWN;
  const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr; D3D_FEATURE_LEVEL fl{};
  HRESULT hr = D3D11CreateDevice(adapter, dt, nullptr, 0, levels, 4, D3D11_SDK_VERSION, &dev, &fl, &ctx);
  if (FAILED(hr)) { printf("D3D11CreateDevice failed 0x%08lx\n", (unsigned long)hr); return 2; }
  Caps caps; caps.Query(dev);
  printf("device %s\n", warp ? "WARP" : anyhw ? "default hardware" : "Helios");
  caps.Print();

  Tally t;
  if (builtin) {
    ID3DBlob* typed = nullptr; ID3DBlob* mul = nullptr; ID3DBlob* div = nullptr;
    for (const Case& c : kCases) {
      ID3DBlob* b = Compile(c);
      if (!b) { t.skipped++; continue; }
      Info info = Parse((const uint8_t*)b->GetBufferPointer(), b->GetBufferSize());
      if ((info.req & c.expect) != c.expect)
        printf("NOTE %s: compiler wrote requires=%s, expected %s\n", c.name, Names(info.req).c_str(), Names(c.expect).c_str());
      int before = t.unexpected + t.unsupported;
      Try(dev, caps, t, c.name, b->GetBufferPointer(), b->GetBufferSize());
      if (t.unexpected + t.unsupported == before) printf("ok %s %s requires=%s\n", kStage[info.type < 0 ? 0 : info.type], c.name, Names(info.req).c_str());
      if (!strcmp(c.name, "typed_uav_load_rgba16f")) { typed = b; continue; }
      if (!strcmp(c.name, "doubles")) { mul = b; continue; }
      if (!strcmp(c.name, "extended_doubles")) { div = b; continue; }
      b->Release();
    }
    if (dispatch) {
      if (typed && !caps.Missing(0x800)) t.mismatches += DispatchTypedLoad(dev, ctx, typed);
      if ((mul || div) && !caps.Missing(0x1)) t.mismatches += DispatchDoubles(dev, ctx, mul, caps.Missing(0x20) ? nullptr : div);
    }
    for (ID3DBlob* b : {typed, mul, div}) if (b) b->Release();
  }
  if (dir) {
    std::vector<std::wstring> files; Walk(dir, files);
    printf("corpus %ls: %zu files, every %d%s\n", dir, files.size(), every, limit >= 0 ? " (limited)" : "");
    long done = 0; std::vector<uint8_t> buf;
    for (size_t i = 0; i < files.size(); i += every) {
      if (limit >= 0 && done >= limit) break;
      if (!ReadAll(files[i], buf)) { t.skipped++; continue; }
      char name[MAX_PATH * 3]; WideCharToMultiByte(CP_UTF8, 0, files[i].c_str(), -1, name, sizeof name, nullptr, nullptr);
      Try(dev, caps, t, name, buf.data(), buf.size());
      if (++done % 2000 == 0) printf("... %ld done: created %d unexpected %d unsupported %d\n", done, t.created, t.unexpected, t.unsupported);
    }
  }
  printf("per stage (created/unexpected/unsupported):");
  for (int s = 0; s < 6; ++s) printf(" %s %d/%d/%d", kStage[s], t.perStage[s][0], t.perStage[s][1], t.perStage[s][2]);
  printf("\nresult: created %d unexpected %d unsupported %d skipped %d dispatch-mismatches %d\n",
         t.created, t.unexpected, t.unsupported, t.skipped, t.mismatches);
  ctx->Release(); dev->Release(); if (adapter) adapter->Release();
  return t.unexpected + t.mismatches + (strict ? t.unsupported : 0);
}
