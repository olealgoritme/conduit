// d3d11_gpupass: GPU time per render pass of a game-like D3D11 frame, from
// D3D11 timestamp queries. Offscreen (no swap chain, no present), so only the
// GPU side is measured: run it on NVK (HELIOS_ICD=nvk) and on the host's
// NVIDIA driver through Venus (HELIOS_ICD=venus) and compare pass by pass.
//
//   d3d11_gpupass.exe [key=value ...]
//
//   w=5120 h=1440      target size
//   msaa=8             samples of the scene color and depth targets (1,2,4,8)
//   color=rgba16f      scene color format: rgba16f, r11g11b10, rgba8
//   depth=d24s8        scene depth format: d24s8, d32, d32s8
//   rz=1               reverse Z (clear 0, GREATER), as DXVK games use
//   draws=2000         objects per frame (DrawIndexed each, own dynamic cbuffer)
//   grid=24            mesh tessellation: 2*grid*grid triangles per object
//   scale=0.05         object size relative to its distance (overdraw)
//   tex=256            distinct textures (512x512, full mips); 4 bound per draw
//   texfmt=rgba8       rgba8 or bc1
//   lights=16          lights looped over per pixel in the opaque pass
//   shadow=4096        shadow map size (D32, depth only, all objects); 0 = off
//   particles=200      alpha-blended screen-space quads in the blend pass
//   sampleshade=0      1: the opaque pixel shader reads SV_SampleIndex
//   passes=all         comma list of clear,zpre,shadow,opaque,blend,resolve,post,cs
//   seconds=8          measured time (after 2 s warm-up)
//
// Per frame: clear, depth prepass, shadow map, opaque (depth EQUAL, textures,
// lights, shadow PCF), blend (particles), ResolveSubresource, post (full-screen
// pass into RGBA8), compute (2x2 downsample to a UAV). A timestamp query
// brackets every pass. Prints, per pass, the mean / median / minimum GPU time
// in microseconds and the share of the frame.
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -O2 -static -std=c++17 d3d11_gpupass.cpp -o d3d11_gpupass.exe
//       -ld3d11 -ldxgi -ld3dcompiler
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#define CHECK(x)                                                              \
   do {                                                                       \
      HRESULT hr_ = (x);                                                      \
      if (FAILED(hr_)) {                                                      \
         fprintf(stderr, "%s:%d %s failed 0x%08lx\n", __FILE__, __LINE__, #x, \
                 (unsigned long)hr_);                                         \
         exit(1);                                                             \
      }                                                                       \
   } while (0)

static const char kShaders[] = R"(
cbuffer Frame : register(b0) {
  row_major float4x4 vp;
  row_major float4x4 lvp;
  float4 lightPos[32];
  float4 lightCol[32];
  float4 camPos;
  uint nLights; uint3 fpad;
};
cbuffer Obj : register(b1) {
  row_major float4x4 world;
  float4 tint;
  float4 uvScale;
  float4 opad[10];
};
Texture2D t0 : register(t0);
Texture2D t1 : register(t1);
Texture2D t2 : register(t2);
Texture2D t3 : register(t3);
Texture2D<float> shadowMap : register(t4);
SamplerState s : register(s0);
SamplerComparisonState sc : register(s1);

struct VIn { float3 pos : POSITION; float3 n : NORMAL; float2 uv : TEXCOORD0; };
struct V { float4 pos : SV_Position; float3 n : NORMAL; float2 uv : TEXCOORD0;
           float3 wp : TEXCOORD1; float4 lp : TEXCOORD2; };

float4 vs_depth(VIn i) : SV_Position {
  float4 w = mul(float4(i.pos, 1), world);
  return mul(w, vp);
}
float4 vs_shadow(VIn i) : SV_Position {
  float4 w = mul(float4(i.pos, 1), world);
  return mul(w, lvp);
}
V vs_main(VIn i) {
  V o;
  float4 w = mul(float4(i.pos, 1), world);
  o.pos = mul(w, vp);
  o.n = normalize(mul(float4(i.n, 0), world).xyz);
  o.uv = i.uv * uvScale.xy;
  o.wp = w.xyz;
  o.lp = mul(w, lvp);
  return o;
}
float4 shade(V i) {
  float4 a = t0.Sample(s, i.uv);
  float4 b = t1.Sample(s, i.uv * 2.3);
  float3 n = normalize(i.n + (t2.Sample(s, i.uv * 1.7).xyz - 0.5));
  float4 r = t3.Sample(s, i.uv * 0.7);
  float3 vdir = normalize(camPos.xyz - i.wp);
  float3 acc = 0;
  for (uint k = 0; k < nLights; k++) {
    float3 L = lightPos[k].xyz - i.wp;
    float d2 = dot(L, L);
    L *= rsqrt(d2);
    float nl = saturate(dot(n, L));
    float3 H = normalize(L + vdir);
    float sp = pow(saturate(dot(n, H)), 32 * r.w + 1);
    acc += lightCol[k].rgb * (nl * a.rgb + sp * r.rgb) / (1 + d2 * lightPos[k].w);
  }
  float3 lp = i.lp.xyz / i.lp.w;
  float2 suv = lp.xy * float2(0.5, -0.5) + 0.5;
  float sh = 0;
  [unroll] for (int k2 = 0; k2 < 4; k2++) {
    float2 o = float2(k2 & 1, k2 >> 1) * (1.0 / 4096) - 0.5 / 4096;
    sh += shadowMap.SampleCmpLevelZero(sc, suv + o, lp.z - 0.002);
  }
  sh = 0.25 + 0.75 * sh * 0.25;
  return float4(acc * sh * b.rgb + tint.rgb * 0.05, a.a);
}
float4 ps_main(V i) : SV_Target { return shade(i); }
float4 ps_main_ss(V i, uint si : SV_SampleIndex) : SV_Target {
  return shade(i) + si * 1e-7;
}

struct PV { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 c : COLOR0; };
cbuffer Part : register(b2) { float4 quads[256]; float4 pcol[256]; };
PV vs_part(uint vid : SV_VertexID) {
  uint q = vid / 6, c = vid % 6;
  float2 corner = float2((c == 1 || c == 2 || c == 4) ? 1 : 0,
                         (c == 2 || c == 4 || c == 5) ? 1 : 0);
  float4 qd = quads[q];
  PV o;
  o.pos = float4(qd.xy + corner * qd.zw, 0.5, 1);
  o.uv = corner;
  o.c = pcol[q];
  return o;
}
float4 ps_part(PV i) : SV_Target {
  float4 t = t0.Sample(s, i.uv);
  return float4(t.rgb * i.c.rgb, t.a * i.c.a);
}

struct FV { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
FV vs_full(uint vid : SV_VertexID) {
  FV o;
  float2 p = float2((vid << 1) & 2, vid & 2);
  o.uv = p;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
  return o;
}
Texture2D scene : register(t0);
float4 ps_post(FV i) : SV_Target {
  float2 d = float2(1.0 / 5120, 1.0 / 1440);
  float3 c = scene.Sample(s, i.uv).rgb * 0.4;
  c += scene.Sample(s, i.uv + float2(d.x, 0)).rgb * 0.15;
  c += scene.Sample(s, i.uv - float2(d.x, 0)).rgb * 0.15;
  c += scene.Sample(s, i.uv + float2(0, d.y)).rgb * 0.15;
  c += scene.Sample(s, i.uv - float2(0, d.y)).rgb * 0.15;
  c = c / (1 + c);
  return float4(pow(c, 1 / 2.2), 1);
}

Texture2D<float4> csIn : register(t0);
RWTexture2D<float4> csOut : register(u0);
[numthreads(8, 8, 1)]
void cs_down(uint3 id : SV_DispatchThreadID) {
  uint2 p = id.xy * 2;
  float4 c = csIn[p] + csIn[p + uint2(1, 0)] + csIn[p + uint2(0, 1)] + csIn[p + uint2(1, 1)];
  csOut[id.xy] = c * 0.25;
}
)";

struct Opts {
   int w = 5120, h = 1440, msaa = 8, rz = 1, draws = 2000, grid = 24, tex = 256;
   int lights = 16, shadow = 4096, particles = 200, sampleshade = 0;
   double scale = 0.05, seconds = 8;
   std::string color = "rgba16f", depth = "d24s8", texfmt = "rgba8", passes = "all";
};

enum Pass { P_CLEAR, P_ZPRE, P_SHADOW, P_OPAQUE, P_BLEND, P_RESOLVE, P_POST, P_CS, P_COUNT };
static const char *kPassNames[P_COUNT] = {"clear",  "zpre",    "shadow", "opaque",
                                          "blend",  "resolve", "post",   "cs"};

static uint32_t rng_state = 0x12345678;
static uint32_t xr() {
   rng_state ^= rng_state << 13;
   rng_state ^= rng_state >> 17;
   rng_state ^= rng_state << 5;
   return rng_state;
}
static float frand() { return (xr() & 0xffffff) / float(0x1000000); }

static ID3DBlob *compile(const char *entry, const char *target) {
   ID3DBlob *code = nullptr, *err = nullptr;
   HRESULT hr = D3DCompile(kShaders, sizeof(kShaders) - 1, "gpupass", nullptr, nullptr, entry,
                           target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &err);
   if (FAILED(hr)) {
      fprintf(stderr, "compile %s: %s\n", entry, err ? (const char *)err->GetBufferPointer() : "?");
      exit(1);
   }
   return code;
}

struct Mat {
   float m[16];
};
static Mat mul(const Mat &a, const Mat &b) {
   Mat r;
   for (int i = 0; i < 4; i++)
      for (int j = 0; j < 4; j++) {
         float s = 0;
         for (int k = 0; k < 4; k++) s += a.m[i * 4 + k] * b.m[k * 4 + j];
         r.m[i * 4 + j] = s;
      }
   return r;
}

int main(int argc, char **argv) {
   Opts o;
   for (int i = 1; i < argc; i++) {
      const char *a = argv[i];
      const char *eq = strchr(a, '=');
      if (!eq) continue;
      std::string k(a, eq - a), v(eq + 1);
      if (k == "w") o.w = atoi(v.c_str());
      else if (k == "h") o.h = atoi(v.c_str());
      else if (k == "msaa") o.msaa = atoi(v.c_str());
      else if (k == "rz") o.rz = atoi(v.c_str());
      else if (k == "draws") o.draws = atoi(v.c_str());
      else if (k == "grid") o.grid = atoi(v.c_str());
      else if (k == "tex") o.tex = std::max(4, atoi(v.c_str()));
      else if (k == "lights") o.lights = std::min(32, atoi(v.c_str()));
      else if (k == "shadow") o.shadow = atoi(v.c_str());
      else if (k == "particles") o.particles = std::min(256, atoi(v.c_str()));
      else if (k == "sampleshade") o.sampleshade = atoi(v.c_str());
      else if (k == "scale") o.scale = atof(v.c_str());
      else if (k == "seconds") o.seconds = atof(v.c_str());
      else if (k == "color") o.color = v;
      else if (k == "depth") o.depth = v;
      else if (k == "texfmt") o.texfmt = v;
      else if (k == "passes") o.passes = v;
   }
   bool on[P_COUNT];
   for (int p = 0; p < P_COUNT; p++)
      on[p] = o.passes == "all" || ("," + o.passes + ",").find(std::string(",") + kPassNames[p] + ",") != std::string::npos;
   if (!o.shadow) on[P_SHADOW] = false;

   ID3D11Device *dev = nullptr;
   ID3D11DeviceContext *ctx = nullptr;
   D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1, got;
   CHECK(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, &fl, 1,
                           D3D11_SDK_VERSION, &dev, &got, &ctx));
   {
      IDXGIDevice *xd = nullptr;
      IDXGIAdapter *ad = nullptr;
      DXGI_ADAPTER_DESC desc = {};
      if (SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&xd)) &&
          SUCCEEDED(xd->GetAdapter(&ad)) && SUCCEEDED(ad->GetDesc(&desc)))
         printf("adapter: %ls\n", desc.Description);
      const char *icd = getenv("HELIOS_ICD");
      printf("HELIOS_ICD=%s\n", icd ? icd : "(unset)");
   }

   DXGI_FORMAT cfmt = o.color == "rgba8"       ? DXGI_FORMAT_R8G8B8A8_UNORM
                      : o.color == "r11g11b10" ? DXGI_FORMAT_R11G11B10_FLOAT
                                               : DXGI_FORMAT_R16G16B16A16_FLOAT;
   DXGI_FORMAT dtyp = DXGI_FORMAT_R24G8_TYPELESS, ddsv = DXGI_FORMAT_D24_UNORM_S8_UINT;
   if (o.depth == "d32") dtyp = DXGI_FORMAT_R32_TYPELESS, ddsv = DXGI_FORMAT_D32_FLOAT;
   if (o.depth == "d32s8") dtyp = DXGI_FORMAT_R32G8X24_TYPELESS, ddsv = DXGI_FORMAT_D32_FLOAT_S8X24_UINT;

   UINT q = 0;
   CHECK(dev->CheckMultisampleQualityLevels(cfmt, o.msaa, &q));
   if (!q) { fprintf(stderr, "msaa %d not supported for color\n", o.msaa); return 1; }

   // Scene targets
   ID3D11Texture2D *colorMS, *depthMS, *resolved, *postOut, *csTex, *shadowTex = nullptr;
   ID3D11RenderTargetView *colorRTV, *postRTV;
   ID3D11DepthStencilView *depthDSV, *shadowDSV = nullptr;
   ID3D11ShaderResourceView *resolvedSRV, *shadowSRV = nullptr;
   ID3D11UnorderedAccessView *csUAV;
   {
      D3D11_TEXTURE2D_DESC td = {};
      td.Width = o.w; td.Height = o.h; td.MipLevels = 1; td.ArraySize = 1;
      td.Format = cfmt; td.SampleDesc.Count = o.msaa; td.Usage = D3D11_USAGE_DEFAULT;
      td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
      CHECK(dev->CreateTexture2D(&td, nullptr, &colorMS));
      CHECK(dev->CreateRenderTargetView(colorMS, nullptr, &colorRTV));
      td.Format = dtyp; td.BindFlags = D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE;
      CHECK(dev->CreateTexture2D(&td, nullptr, &depthMS));
      D3D11_DEPTH_STENCIL_VIEW_DESC dd = {};
      dd.Format = ddsv;
      dd.ViewDimension = o.msaa > 1 ? D3D11_DSV_DIMENSION_TEXTURE2DMS : D3D11_DSV_DIMENSION_TEXTURE2D;
      CHECK(dev->CreateDepthStencilView(depthMS, &dd, &depthDSV));
      td.SampleDesc.Count = 1; td.Format = cfmt;
      td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
      CHECK(dev->CreateTexture2D(&td, nullptr, &resolved));
      CHECK(dev->CreateShaderResourceView(resolved, nullptr, &resolvedSRV));
      td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.BindFlags = D3D11_BIND_RENDER_TARGET;
      CHECK(dev->CreateTexture2D(&td, nullptr, &postOut));
      CHECK(dev->CreateRenderTargetView(postOut, nullptr, &postRTV));
      td.Width = o.w / 2; td.Height = o.h / 2; td.Format = DXGI_FORMAT_R16G16B16A16_FLOAT;
      td.BindFlags = D3D11_BIND_UNORDERED_ACCESS;
      CHECK(dev->CreateTexture2D(&td, nullptr, &csTex));
      CHECK(dev->CreateUnorderedAccessView(csTex, nullptr, &csUAV));
      if (o.shadow) {
         td.Width = td.Height = o.shadow; td.Format = DXGI_FORMAT_R32_TYPELESS;
         td.BindFlags = D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE;
         CHECK(dev->CreateTexture2D(&td, nullptr, &shadowTex));
         dd.Format = DXGI_FORMAT_D32_FLOAT; dd.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2D;
         CHECK(dev->CreateDepthStencilView(shadowTex, &dd, &shadowDSV));
         D3D11_SHADER_RESOURCE_VIEW_DESC sd = {};
         sd.Format = DXGI_FORMAT_R32_FLOAT; sd.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
         sd.Texture2D.MipLevels = 1;
         CHECK(dev->CreateShaderResourceView(shadowTex, &sd, &shadowSRV));
      }
   }

   // Textures
   std::vector<ID3D11ShaderResourceView *> srvs(o.tex);
   {
      const int ts = 512, levels = 10;
      bool bc1 = o.texfmt == "bc1";
      std::vector<std::vector<uint32_t>> data(levels);
      for (int t = 0; t < o.tex; t++) {
         D3D11_SUBRESOURCE_DATA init[levels];
         for (int l = 0; l < levels; l++) {
            int s = std::max(1, ts >> l);
            if (bc1) {
               int bw = std::max(1, (s + 3) / 4);
               data[l].resize(bw * bw * 2);
               for (auto &x : data[l]) x = xr();
               init[l] = {data[l].data(), UINT(bw * 8), 0};
            } else {
               data[l].resize(s * s);
               uint32_t base = xr();
               for (int y = 0; y < s; y++)
                  for (int x = 0; x < s; x++)
                     data[l][y * s + x] = base ^ ((x * 7 + y * 13 + t) * 0x010101u) ^ (((x ^ y) & 8) ? 0x00ff00u : 0);
               init[l] = {data[l].data(), UINT(s * 4), 0};
            }
         }
         D3D11_TEXTURE2D_DESC td = {};
         td.Width = td.Height = ts; td.MipLevels = levels; td.ArraySize = 1;
         td.Format = bc1 ? DXGI_FORMAT_BC1_UNORM : DXGI_FORMAT_R8G8B8A8_UNORM;
         td.SampleDesc.Count = 1; td.Usage = D3D11_USAGE_IMMUTABLE;
         td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
         ID3D11Texture2D *tx;
         CHECK(dev->CreateTexture2D(&td, init, &tx));
         CHECK(dev->CreateShaderResourceView(tx, nullptr, &srvs[t]));
      }
   }

   // Mesh: a lumpy sphere
   struct Vtx { float p[3], n[3], uv[2]; };
   std::vector<Vtx> verts;
   std::vector<uint16_t> idx;
   {
      int g = o.grid;
      for (int j = 0; j <= g; j++)
         for (int i = 0; i <= g; i++) {
            float u = i / float(g) * 6.2831853f, v = j / float(g) * 3.1415926f;
            float r = 1 + 0.15f * sinf(5 * u) * sinf(4 * v);
            float nx = sinf(v) * cosf(u), ny = cosf(v), nz = sinf(v) * sinf(u);
            verts.push_back({{r * nx, r * ny, r * nz}, {nx, ny, nz}, {i / float(g), j / float(g)}});
         }
      for (int j = 0; j < g; j++)
         for (int i = 0; i < g; i++) {
            uint16_t a = j * (g + 1) + i, b = a + 1, c = a + g + 1, d = c + 1;
            idx.insert(idx.end(), {a, c, b, b, c, d});
         }
   }
   ID3D11Buffer *vb, *ib;
   {
      D3D11_BUFFER_DESC bd = {UINT(verts.size() * sizeof(Vtx)), D3D11_USAGE_IMMUTABLE, D3D11_BIND_VERTEX_BUFFER};
      D3D11_SUBRESOURCE_DATA sd = {verts.data()};
      CHECK(dev->CreateBuffer(&bd, &sd, &vb));
      bd = {UINT(idx.size() * 2), D3D11_USAGE_IMMUTABLE, D3D11_BIND_INDEX_BUFFER};
      sd = {idx.data()};
      CHECK(dev->CreateBuffer(&bd, &sd, &ib));
   }

   // Shaders and states
   ID3DBlob *bvsd = compile("vs_depth", "vs_5_0"), *bvss = compile("vs_shadow", "vs_5_0"),
            *bvsm = compile("vs_main", "vs_5_0"),
            *bpsm = compile(o.sampleshade ? "ps_main_ss" : "ps_main", "ps_5_0"),
            *bvsp = compile("vs_part", "vs_5_0"), *bpsp = compile("ps_part", "ps_5_0"),
            *bvsf = compile("vs_full", "vs_5_0"), *bpsf = compile("ps_post", "ps_5_0"),
            *bcs = compile("cs_down", "cs_5_0");
   ID3D11VertexShader *vsDepth, *vsShadow, *vsMain, *vsPart, *vsFull;
   ID3D11PixelShader *psMain, *psPart, *psPost;
   ID3D11ComputeShader *csDown;
   CHECK(dev->CreateVertexShader(bvsd->GetBufferPointer(), bvsd->GetBufferSize(), nullptr, &vsDepth));
   CHECK(dev->CreateVertexShader(bvss->GetBufferPointer(), bvss->GetBufferSize(), nullptr, &vsShadow));
   CHECK(dev->CreateVertexShader(bvsm->GetBufferPointer(), bvsm->GetBufferSize(), nullptr, &vsMain));
   CHECK(dev->CreateVertexShader(bvsp->GetBufferPointer(), bvsp->GetBufferSize(), nullptr, &vsPart));
   CHECK(dev->CreateVertexShader(bvsf->GetBufferPointer(), bvsf->GetBufferSize(), nullptr, &vsFull));
   CHECK(dev->CreatePixelShader(bpsm->GetBufferPointer(), bpsm->GetBufferSize(), nullptr, &psMain));
   CHECK(dev->CreatePixelShader(bpsp->GetBufferPointer(), bpsp->GetBufferSize(), nullptr, &psPart));
   CHECK(dev->CreatePixelShader(bpsf->GetBufferPointer(), bpsf->GetBufferSize(), nullptr, &psPost));
   CHECK(dev->CreateComputeShader(bcs->GetBufferPointer(), bcs->GetBufferSize(), nullptr, &csDown));
   ID3D11InputLayout *il;
   {
      D3D11_INPUT_ELEMENT_DESC ie[] = {
         {"POSITION", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
         {"NORMAL", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 12, D3D11_INPUT_PER_VERTEX_DATA, 0},
         {"TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 24, D3D11_INPUT_PER_VERTEX_DATA, 0},
      };
      CHECK(dev->CreateInputLayout(ie, 3, bvsm->GetBufferPointer(), bvsm->GetBufferSize(), &il));
   }
   ID3D11DepthStencilState *dsPre, *dsEq, *dsTest, *dsShadow;
   {
      D3D11_DEPTH_STENCIL_DESC d = {};
      d.DepthEnable = TRUE; d.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL;
      d.DepthFunc = o.rz ? D3D11_COMPARISON_GREATER : D3D11_COMPARISON_LESS;
      CHECK(dev->CreateDepthStencilState(&d, &dsPre));
      d.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ZERO; d.DepthFunc = D3D11_COMPARISON_EQUAL;
      CHECK(dev->CreateDepthStencilState(&d, &dsEq));
      d.DepthFunc = o.rz ? D3D11_COMPARISON_GREATER_EQUAL : D3D11_COMPARISON_LESS_EQUAL;
      CHECK(dev->CreateDepthStencilState(&d, &dsTest));
      d.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL; d.DepthFunc = D3D11_COMPARISON_LESS;
      CHECK(dev->CreateDepthStencilState(&d, &dsShadow));
   }
   ID3D11RasterizerState *rsBack, *rsNone;
   {
      D3D11_RASTERIZER_DESC r = {};
      r.FillMode = D3D11_FILL_SOLID; r.CullMode = D3D11_CULL_BACK; r.DepthClipEnable = TRUE;
      r.MultisampleEnable = o.msaa > 1;
      CHECK(dev->CreateRasterizerState(&r, &rsBack));
      r.CullMode = D3D11_CULL_NONE;
      CHECK(dev->CreateRasterizerState(&r, &rsNone));
   }
   ID3D11BlendState *bsAlpha;
   {
      D3D11_BLEND_DESC b = {};
      b.RenderTarget[0].BlendEnable = TRUE;
      b.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
      b.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
      b.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
      b.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
      b.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
      b.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
      b.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
      CHECK(dev->CreateBlendState(&b, &bsAlpha));
   }
   ID3D11SamplerState *samp, *sampCmp;
   {
      D3D11_SAMPLER_DESC s = {};
      s.Filter = D3D11_FILTER_ANISOTROPIC; s.MaxAnisotropy = 8;
      s.AddressU = s.AddressV = s.AddressW = D3D11_TEXTURE_ADDRESS_WRAP;
      s.MaxLOD = D3D11_FLOAT32_MAX;
      CHECK(dev->CreateSamplerState(&s, &samp));
      s.Filter = D3D11_FILTER_COMPARISON_MIN_MAG_LINEAR_MIP_POINT; s.MaxAnisotropy = 1;
      s.AddressU = s.AddressV = s.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
      s.ComparisonFunc = D3D11_COMPARISON_LESS_EQUAL;
      CHECK(dev->CreateSamplerState(&s, &sampCmp));
   }

   // Constant buffers
   struct FrameCB {
      Mat vp, lvp;
      float lightPos[32][4], lightCol[32][4], camPos[4];
      uint32_t nLights, pad[3];
   };
   struct ObjCB {
      Mat world;
      float tint[4], uvScale[4], pad[40];
   };
   struct PartCB { float quads[256][4], col[256][4]; };
   ID3D11Buffer *frameCB, *objCB, *partCB;
   {
      D3D11_BUFFER_DESC bd = {sizeof(FrameCB), D3D11_USAGE_DYNAMIC, D3D11_BIND_CONSTANT_BUFFER, D3D11_CPU_ACCESS_WRITE};
      CHECK(dev->CreateBuffer(&bd, nullptr, &frameCB));
      bd.ByteWidth = sizeof(ObjCB);
      CHECK(dev->CreateBuffer(&bd, nullptr, &objCB));
      bd.ByteWidth = sizeof(PartCB);
      CHECK(dev->CreateBuffer(&bd, nullptr, &partCB));
   }

   // Scene: objects in the view frustum, camera at the origin looking down +z
   const float aspect = float(o.w) / o.h, f = 1.7320508f, zn = 0.5f, zf = 200.f;
   Mat proj = {};
   proj.m[0] = f / aspect; proj.m[5] = f; proj.m[11] = 1;
   if (o.rz) { proj.m[10] = zn / (zn - zf); proj.m[14] = -zf * zn / (zn - zf); }
   else { proj.m[10] = zf / (zf - zn); proj.m[14] = -zn * zf / (zf - zn); }
   Mat lvp = {};  // orthographic from above: x -> x, z -> y, height -> depth
   lvp.m[0] = 1 / 40.f; lvp.m[9] = 1 / 40.f; lvp.m[6] = 1 / 80.f;
   lvp.m[13] = -0.8f; lvp.m[14] = 0.5f; lvp.m[15] = 1;
   std::vector<Mat> worlds(o.draws);
   std::vector<int> texSel(o.draws);
   for (int i = 0; i < o.draws; i++) {
      float z = 5 + 60 * frand();
      float x = (frand() * 2 - 1) * z / f * aspect, y = (frand() * 2 - 1) * z / f;
      float s = float(o.scale) * z * (0.5f + frand());
      Mat w = {};
      w.m[0] = w.m[5] = w.m[10] = s; w.m[15] = 1;
      w.m[12] = x; w.m[13] = y; w.m[14] = z;
      worlds[i] = w;
      texSel[i] = xr() % (o.tex - 3);
   }
   // Draw order front to back-ish shuffled, as engines sort by material
   FrameCB fcb = {};
   fcb.vp = proj; fcb.lvp = lvp; fcb.nLights = o.lights;
   for (int k = 0; k < 32; k++) {
      fcb.lightPos[k][0] = (frand() * 2 - 1) * 40; fcb.lightPos[k][1] = (frand() * 2 - 1) * 15;
      fcb.lightPos[k][2] = 5 + 60 * frand(); fcb.lightPos[k][3] = 0.01f;
      fcb.lightCol[k][0] = frand(); fcb.lightCol[k][1] = frand(); fcb.lightCol[k][2] = frand();
   }
   PartCB pcb;
   for (int k = 0; k < 256; k++) {
      float sz = 0.2f + 0.4f * frand();
      pcb.quads[k][0] = frand() * 2 - 1 - sz / 2; pcb.quads[k][1] = frand() * 2 - 1 - sz / 2;
      pcb.quads[k][2] = sz / aspect * 2; pcb.quads[k][3] = sz;
      pcb.col[k][0] = frand(); pcb.col[k][1] = frand(); pcb.col[k][2] = frand(); pcb.col[k][3] = 0.3f;
   }

   // Queries
   const int LAG = 4;
   ID3D11Query *qdis[LAG], *qts[LAG][P_COUNT + 1];
   for (int l = 0; l < LAG; l++) {
      D3D11_QUERY_DESC qd = {D3D11_QUERY_TIMESTAMP_DISJOINT, 0};
      CHECK(dev->CreateQuery(&qd, &qdis[l]));
      qd.Query = D3D11_QUERY_TIMESTAMP;
      for (int p = 0; p <= P_COUNT; p++) CHECK(dev->CreateQuery(&qd, &qts[l][p]));
   }

   auto draw_objects = [&](bool textured) {
      UINT stride = sizeof(Vtx), off = 0;
      ctx->IASetVertexBuffers(0, 1, &vb, &stride, &off);
      ctx->IASetIndexBuffer(ib, DXGI_FORMAT_R16_UINT, 0);
      ctx->IASetInputLayout(il);
      ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
      for (int i = 0; i < o.draws; i++) {
         D3D11_MAPPED_SUBRESOURCE m;
         CHECK(ctx->Map(objCB, 0, D3D11_MAP_WRITE_DISCARD, 0, &m));
         ObjCB *c = (ObjCB *)m.pData;
         c->world = worlds[i];
         c->tint[0] = (i & 7) / 7.f; c->tint[1] = 0.5f; c->tint[2] = 0.25f; c->tint[3] = 1;
         c->uvScale[0] = c->uvScale[1] = 1 + (i & 3);
         ctx->Unmap(objCB, 0);
         ctx->VSSetConstantBuffers(1, 1, &objCB);
         if (textured) {
            ctx->PSSetConstantBuffers(1, 1, &objCB);
            ctx->PSSetShaderResources(0, 4, &srvs[texSel[i]]);
         }
         ctx->DrawIndexed(UINT(idx.size()), 0, 0);
      }
   };

   const float clr0[4] = {0.1f, 0.2f, 0.3f, 1};
   ID3D11ShaderResourceView *nullSRV[5] = {};
   ID3D11UnorderedAccessView *nullUAV = nullptr;
   std::vector<double> samples[P_COUNT + 1];
   LARGE_INTEGER qpf, t0, now;
   QueryPerformanceFrequency(&qpf);
   QueryPerformanceCounter(&t0);
   double warm = 2.0, total = warm + o.seconds;
   long frame = 0, measured = 0, disjoint = 0;
   for (;; frame++) {
      QueryPerformanceCounter(&now);
      double el = double(now.QuadPart - t0.QuadPart) / qpf.QuadPart;
      int slot = frame % LAG;
      if (frame >= LAG) {
         D3D11_QUERY_DATA_TIMESTAMP_DISJOINT dj;
         while (ctx->GetData(qdis[slot], &dj, sizeof(dj), 0) != S_OK) Sleep(0);
         uint64_t ts[P_COUNT + 1];
         for (int p = 0; p <= P_COUNT; p++)
            while (ctx->GetData(qts[slot][p], &ts[p], 8, 0) != S_OK) Sleep(0);
         if (dj.Disjoint) disjoint++;
         else if (el > warm) {
            for (int p = 0; p < P_COUNT; p++)
               samples[p].push_back(double(ts[p + 1] - ts[p]) * 1e6 / dj.Frequency);
            samples[P_COUNT].push_back(double(ts[P_COUNT] - ts[0]) * 1e6 / dj.Frequency);
            measured++;
         }
      }
      if (el > total) break;

      ctx->Begin(qdis[slot]);
      ctx->End(qts[slot][0]);
      D3D11_MAPPED_SUBRESOURCE m;
      CHECK(ctx->Map(frameCB, 0, D3D11_MAP_WRITE_DISCARD, 0, &m));
      memcpy(m.pData, &fcb, sizeof(fcb));
      ctx->Unmap(frameCB, 0);
      CHECK(ctx->Map(partCB, 0, D3D11_MAP_WRITE_DISCARD, 0, &m));
      memcpy(m.pData, &pcb, sizeof(pcb));
      ctx->Unmap(partCB, 0);
      ctx->VSSetConstantBuffers(0, 1, &frameCB);
      ctx->PSSetConstantBuffers(0, 1, &frameCB);
      D3D11_VIEWPORT vp = {0, 0, float(o.w), float(o.h), 0, 1};

      // clear
      if (on[P_CLEAR]) {
         ctx->ClearRenderTargetView(colorRTV, clr0);
         ctx->ClearDepthStencilView(depthDSV, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, o.rz ? 0.f : 1.f, 0);
         if (shadowDSV) ctx->ClearDepthStencilView(shadowDSV, D3D11_CLEAR_DEPTH, 1.f, 0);
      }
      ctx->End(qts[slot][1]);

      // depth prepass
      if (on[P_ZPRE]) {
         ctx->OMSetRenderTargets(0, nullptr, depthDSV);
         ctx->RSSetViewports(1, &vp);
         ctx->RSSetState(rsBack);
         ctx->OMSetDepthStencilState(dsPre, 0);
         ctx->OMSetBlendState(nullptr, nullptr, 0xffffffff);
         ctx->VSSetShader(vsDepth, nullptr, 0);
         ctx->PSSetShader(nullptr, nullptr, 0);
         draw_objects(false);
      }
      ctx->End(qts[slot][2]);

      // shadow map
      if (on[P_SHADOW]) {
         D3D11_VIEWPORT svp = {0, 0, float(o.shadow), float(o.shadow), 0, 1};
         ctx->OMSetRenderTargets(0, nullptr, shadowDSV);
         ctx->RSSetViewports(1, &svp);
         ctx->RSSetState(rsNone);
         ctx->OMSetDepthStencilState(dsShadow, 0);
         ctx->VSSetShader(vsShadow, nullptr, 0);
         ctx->PSSetShader(nullptr, nullptr, 0);
         draw_objects(false);
      }
      ctx->End(qts[slot][3]);

      // opaque
      if (on[P_OPAQUE]) {
         ctx->OMSetRenderTargets(1, &colorRTV, depthDSV);
         ctx->RSSetViewports(1, &vp);
         ctx->RSSetState(rsBack);
         ctx->OMSetDepthStencilState(on[P_ZPRE] ? dsEq : dsPre, 0);
         ctx->VSSetShader(vsMain, nullptr, 0);
         ctx->PSSetShader(psMain, nullptr, 0);
         ID3D11SamplerState *ss[2] = {samp, sampCmp};
         ctx->PSSetSamplers(0, 2, ss);
         ctx->PSSetShaderResources(4, 1, &shadowSRV);
         draw_objects(true);
         ctx->PSSetShaderResources(0, 5, nullSRV);
      }
      ctx->End(qts[slot][4]);

      // blend
      if (on[P_BLEND] && o.particles) {
         ctx->OMSetRenderTargets(1, &colorRTV, depthDSV);
         ctx->RSSetViewports(1, &vp);
         ctx->RSSetState(rsNone);
         ctx->OMSetDepthStencilState(dsTest, 0);
         ctx->OMSetBlendState(bsAlpha, nullptr, 0xffffffff);
         ctx->IASetInputLayout(nullptr);
         ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
         ctx->VSSetShader(vsPart, nullptr, 0);
         ctx->VSSetConstantBuffers(2, 1, &partCB);
         ctx->PSSetShader(psPart, nullptr, 0);
         ctx->PSSetSamplers(0, 1, &samp);
         ctx->PSSetShaderResources(0, 1, &srvs[0]);
         for (int k = 0; k < o.particles; k += 16)
            ctx->Draw(UINT(std::min(16, o.particles - k) * 6), UINT(k * 6));
         ctx->OMSetBlendState(nullptr, nullptr, 0xffffffff);
         ctx->PSSetShaderResources(0, 1, nullSRV);
      }
      ctx->End(qts[slot][5]);

      // resolve
      ctx->OMSetRenderTargets(0, nullptr, nullptr);
      if (on[P_RESOLVE]) {
         if (o.msaa > 1) ctx->ResolveSubresource(resolved, 0, colorMS, 0, cfmt);
         else ctx->CopyResource(resolved, colorMS);
      }
      ctx->End(qts[slot][6]);

      // post
      if (on[P_POST]) {
         ctx->OMSetRenderTargets(1, &postRTV, nullptr);
         ctx->RSSetViewports(1, &vp);
         ctx->RSSetState(rsNone);
         ctx->OMSetDepthStencilState(nullptr, 0);
         ctx->IASetInputLayout(nullptr);
         ctx->VSSetShader(vsFull, nullptr, 0);
         ctx->PSSetShader(psPost, nullptr, 0);
         ctx->PSSetSamplers(0, 1, &samp);
         ctx->PSSetShaderResources(0, 1, &resolvedSRV);
         ctx->Draw(3, 0);
         ctx->PSSetShaderResources(0, 1, nullSRV);
         ctx->OMSetRenderTargets(0, nullptr, nullptr);
      }
      ctx->End(qts[slot][7]);

      // compute
      if (on[P_CS]) {
         ctx->CSSetShader(csDown, nullptr, 0);
         ctx->CSSetShaderResources(0, 1, &resolvedSRV);
         ctx->CSSetUnorderedAccessViews(0, 1, &csUAV, nullptr);
         ctx->Dispatch((o.w / 2 + 7) / 8, (o.h / 2 + 7) / 8, 1);
         ctx->CSSetShaderResources(0, 1, nullSRV);
         ctx->CSSetUnorderedAccessViews(0, 1, &nullUAV, nullptr);
      }
      ctx->End(qts[slot][8]);
      ctx->End(qdis[slot]);
      ctx->Flush();
   }

   printf("%dx%d msaa=%d color=%s depth=%s rz=%d draws=%d tris/draw=%d tex=%d(%s) lights=%d "
          "shadow=%d particles=%d sampleshade=%d\n",
          o.w, o.h, o.msaa, o.color.c_str(), o.depth.c_str(), o.rz, o.draws, o.grid * o.grid * 2,
          o.tex, o.texfmt.c_str(), o.lights, o.shadow, o.particles, o.sampleshade);
   printf("frames measured %ld, disjoint %ld\n", measured, disjoint);
   if (!measured) return 1;
   double frameMean = 0;
   for (double v : samples[P_COUNT]) frameMean += v;
   frameMean /= samples[P_COUNT].size();
   printf("%-8s %9s %9s %9s %6s\n", "pass", "mean_us", "p50_us", "min_us", "share");
   for (int p = 0; p <= P_COUNT; p++) {
      auto &s = samples[p];
      double mean = 0;
      for (double v : s) mean += v;
      mean /= s.size();
      std::sort(s.begin(), s.end());
      printf("%-8s %9.1f %9.1f %9.1f %5.1f%%\n", p < P_COUNT ? kPassNames[p] : "frame", mean,
             s[s.size() / 2], s[0], 100 * mean / frameMean);
   }
   return 0;
}
