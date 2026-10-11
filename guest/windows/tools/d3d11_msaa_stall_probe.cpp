// d3d11_msaa_stall_probe: finds which step of d3d11_msaa_probe's frame keeps
// the GPU busy for seconds (seen with 4x RGBA16F + D32 and 2x RGBA16F + D24S8
// on NVK: GPU at 100 %, the probe never finishing the case).
//
// Runs the probe's frame for the given MSAA configurations, one step at a time:
// after every step it flushes and waits for an event query, at most `limit_ms`.
// A step that takes longer is reported and the program exits at once, so the
// GPU is never left saturated for long (process exit tears the context down).
//
//   d3d11_msaa_stall_probe.exe [frames=2] [w=1920] [h=1080] [limit_ms=2000]
//       [fixed=0] [cases=all|<index,...>]
//
// fixed=1 compiles the Texture2DMS reading shaders with the sample count as a
// literal instead of reading it from the constant buffer: if the stall goes
// away, the shader read a wrong loop bound from its constant buffer.
//
// Exit code: 0 no step over the limit, 5 a step over the limit, 2/3 setup errors.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_msaa_stall_probe.exe d3d11_msaa_stall_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
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

static IDXGIAdapter1 *HeliosAdapter() {
   IDXGIFactory1 *f = nullptr;
   if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void **)&f))) return nullptr;
   IDXGIAdapter1 *a = nullptr, *chosen = nullptr;
   for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
      DXGI_ADAPTER_DESC1 d{};
      a->GetDesc1(&d);
      if (!chosen && wcsstr(d.Description, L"Helios")) { chosen = a; continue; }
      a->Release();
   }
   f->Release();
   return chosen;
}

static const char *kSceneVS =
   "cbuffer C : register(b0) { float2 size; };\n"
   "struct V { float2 p : POS; float d : DEPTH; float4 c : COLOR; };\n"
   "struct O { float4 pos : SV_Position; float4 c : COLOR; };\n"
   "O main(V v) { O o; o.pos = float4(v.p / size * float2(2, -2) + float2(-1, 1), v.d, 1);\n"
   "  o.c = v.c; return o; }\n";
static const char *kScenePS =
   "float4 main(float4 pos : SV_Position, float4 c : COLOR) : SV_Target { return c; }\n";
static const char *kFullVS =
   "float4 main(uint id : SV_VertexID) : SV_Position {\n"
   "  float2 p = float2((id << 1) & 2, id & 2);\n"
   "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
   "}\n";
static const char *kColorPS =
   "Texture2DMS<float4> t : register(t0);\n"
   "cbuffer C : register(b0) { float2 size; uint samples; };\n"
   "#if FIXED\n#define NS FIXED\n#else\n#define NS samples\n#endif\n"
   "float4 main(float4 pos : SV_Position) : SV_Target {\n"
   "  int2 p = int2(pos.xy); float4 s = 0;\n"
   "  for (uint i = 0; i < NS; i++) s += t.Load(p, i);\n"
   "  return s / NS;\n"
   "}\n";
static const char *kDepthPS =
   "Texture2DMS<float> t : register(t0);\n"
   "cbuffer C : register(b0) { float2 size; uint samples; };\n"
   "#if FIXED\n#define NS FIXED\n#else\n#define NS samples\n#endif\n"
   "float2 main(float4 pos : SV_Position) : SV_Target {\n"
   "  int2 p = int2(pos.xy); float lo = 2, hi = -1;\n"
   "  for (uint i = 0; i < NS; i++) { float d = t.Load(p, i); lo = min(lo, d); hi = max(hi, d); }\n"
   "  return float2(lo, hi);\n"
   "}\n";

static ID3DBlob *Compile(const char *src, const char *target, int fixed) {
   char fx[16];
   snprintf(fx, sizeof(fx), "%d", fixed);
   D3D_SHADER_MACRO defs[] = {{"FIXED", fx}, {nullptr, nullptr}};
   ID3DBlob *b = nullptr, *e = nullptr;
   if (FAILED(D3DCompile(src, strlen(src), nullptr, defs, nullptr, "main", target, 0, 0, &b, &e))) {
      printf("compile: %s\n", e ? (const char *)e->GetBufferPointer() : "?");
      exit(3);
   }
   return b;
}

struct Vtx { float x, y, d; float c[4]; };
static uint32_t g_rng = 12345;
static uint32_t Rnd() { g_rng = g_rng * 1664525u + 1013904223u; return g_rng >> 8; }

struct Config { UINT samples; DXGI_FORMAT color, depthTex, depthDsv, depthSrv; const char *name; };

int main(int argc, char **argv) {
   setvbuf(stdout, nullptr, _IONBF, 0);
   int frames = 2, W = 1920, H = 1080, limit = 2000, fixedLoop = 0;
   std::string cases = "all";
   for (int i = 1; i < argc; i++) {
      const char *eq = strchr(argv[i], '=');
      if (!eq) continue;
      std::string k(argv[i], eq - argv[i]);
      if (k == "frames") frames = atoi(eq + 1);
      else if (k == "w") W = atoi(eq + 1);
      else if (k == "h") H = atoi(eq + 1);
      else if (k == "limit_ms") limit = atoi(eq + 1);
      else if (k == "fixed") fixedLoop = atoi(eq + 1);
      else if (k == "cases") cases = eq + 1;
   }
   IDXGIAdapter1 *adapter = HeliosAdapter();
   D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
   ID3D11Device *dev = nullptr;
   ID3D11DeviceContext *ctx = nullptr;
   if (FAILED(D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE, nullptr, 0,
                                &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
      printf("no device\n");
      return 2;
   }
   printf("device %s %dx%d frames=%d limit=%d ms fixed=%d\n", adapter ? "Helios" : "default", W, H, frames, limit,
          fixedLoop);

   ID3DBlob *b = Compile(kSceneVS, "vs_5_0", 0);
   ID3D11VertexShader *sceneVS, *fullVS;
   dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &sceneVS);
   D3D11_INPUT_ELEMENT_DESC ie[] = {
      {"POS", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
      {"DEPTH", 0, DXGI_FORMAT_R32_FLOAT, 0, 8, D3D11_INPUT_PER_VERTEX_DATA, 0},
      {"COLOR", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 12, D3D11_INPUT_PER_VERTEX_DATA, 0},
   };
   ID3D11InputLayout *il;
   dev->CreateInputLayout(ie, 3, b->GetBufferPointer(), b->GetBufferSize(), &il);
   b = Compile(kScenePS, "ps_5_0", 0);
   ID3D11PixelShader *scenePS;
   dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &scenePS);
   b = Compile(kFullVS, "vs_5_0", 0);
   dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &fullVS);

   // Scene: 160 rects at distinct depths, 400 triangles in the bottom band
   const int bandY = H * 7 / 8;
   std::vector<Vtx> vtx;
   for (int i = 0; i < 160; i++) {
      int w = 16 + (int)(Rnd() % (W / 4)), h = 16 + (int)(Rnd() % (bandY / 3));
      float x0 = (float)(Rnd() % (W - w)), y0 = (float)(Rnd() % (bandY - h)), x1 = x0 + w, y1 = y0 + h;
      float d = (float)(1 + i * 5) / 1024.0f, c[4];
      for (int k = 0; k < 4; k++) c[k] = (float)(Rnd() % 9) / 8.0f;
      Vtx a{x0, y0, d, {c[0], c[1], c[2], c[3]}}, bb = a, cc = a, dd = a;
      bb.x = x1; cc.y = y1; dd.x = x1; dd.y = y1;
      vtx.insert(vtx.end(), {a, bb, cc, cc, bb, dd});
   }
   const UINT rectVerts = (UINT)vtx.size();
   for (int i = 0; i < 400; i++) {
      float cx = (float)(Rnd() % W), cy = (float)(bandY + Rnd() % (H - bandY));
      float a = (float)(Rnd() % 6283) / 1000.0f, s = 8.0f + (float)(Rnd() % 60);
      float d = (float)(Rnd() % 1000) / 1024.0f + 0.001f, c[4];
      for (int k = 0; k < 4; k++) c[k] = (float)(Rnd() % 9) / 8.0f;
      for (int k = 0; k < 3; k++) {
         float ang = a + k * 2.0944f;
         Vtx v{cx + s * cosf(ang), cy + s * sinf(ang), d, {c[0], c[1], c[2], c[3]}};
         if (v.y < bandY) v.y = (float)bandY;
         vtx.push_back(v);
      }
   }
   const UINT totalVerts = (UINT)vtx.size();
   D3D11_BUFFER_DESC bd{};
   bd.ByteWidth = (UINT)(vtx.size() * sizeof(Vtx)); bd.Usage = D3D11_USAGE_IMMUTABLE;
   bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
   D3D11_SUBRESOURCE_DATA sd{vtx.data(), 0, 0};
   ID3D11Buffer *vb, *cb;
   dev->CreateBuffer(&bd, &sd, &vb);
   struct { float size[2]; UINT samples; UINT pad; } cbData{{(float)W, (float)H}, 1, 0};
   bd = {}; bd.ByteWidth = 16; bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
   dev->CreateBuffer(&bd, nullptr, &cb);

   const float clearColor[4] = {0.25f, 0.5f, 0.75f, 1.0f};
   D3D11_DEPTH_STENCIL_DESC dsd{};
   dsd.DepthEnable = TRUE; dsd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL; dsd.DepthFunc = D3D11_COMPARISON_GREATER;
   ID3D11DepthStencilState *dss;
   dev->CreateDepthStencilState(&dsd, &dss);
   D3D11_RASTERIZER_DESC rd{};
   rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE; rd.MultisampleEnable = TRUE;
   ID3D11RasterizerState *rs;
   dev->CreateRasterizerState(&rd, &rs);
   D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
   ID3D11Query *q;
   dev->CreateQuery(&qd, &q);
   LARGE_INTEGER qpf;
   QueryPerformanceFrequency(&qpf);

   const Config configs[] = {
      {4, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "4x RGBA8 D32"},
      {4, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "4x RGBA16F D32"},
      {2, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, "2x RGBA16F D24S8"},
      {8, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS, "8x RGBA16F D32S8"},
      {8, DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "8x RGBA32F D32"},
   };
   int worst_ms = 0;
   for (int ci = 0; ci < (int)(sizeof(configs) / sizeof(configs[0])); ci++) {
      if (cases != "all" && ("," + cases + ",").find("," + std::to_string(ci) + ",") == std::string::npos) continue;
      const Config &cfg = configs[ci];
      ID3D11PixelShader *colorPS, *depthPS;
      b = Compile(kColorPS, "ps_5_0", fixedLoop ? (int)cfg.samples : 0);
      dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &colorPS);
      b = Compile(kDepthPS, "ps_5_0", fixedLoop ? (int)cfg.samples : 0);
      dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &depthPS);

      D3D11_TEXTURE2D_DESC td{};
      td.Width = W; td.Height = H; td.MipLevels = 1; td.ArraySize = 1; td.SampleDesc.Count = cfg.samples;
      td.Usage = D3D11_USAGE_DEFAULT; td.Format = cfg.color;
      td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
      ID3D11Texture2D *colorMS, *colorCopy, *depthMS, *depthCopy, *resolved, *outColor, *outDepth;
      dev->CreateTexture2D(&td, nullptr, &colorMS);
      dev->CreateTexture2D(&td, nullptr, &colorCopy);
      td.Format = cfg.depthTex; td.BindFlags = D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE;
      dev->CreateTexture2D(&td, nullptr, &depthMS);
      dev->CreateTexture2D(&td, nullptr, &depthCopy);
      td.SampleDesc.Count = 1; td.Format = cfg.color; td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
      dev->CreateTexture2D(&td, nullptr, &resolved);
      td.Format = DXGI_FORMAT_R32G32B32A32_FLOAT; td.BindFlags = D3D11_BIND_RENDER_TARGET;
      dev->CreateTexture2D(&td, nullptr, &outColor);
      td.Format = DXGI_FORMAT_R32G32_FLOAT;
      dev->CreateTexture2D(&td, nullptr, &outDepth);
      ID3D11RenderTargetView *rtvMS, *rtvOutColor, *rtvOutDepth;
      dev->CreateRenderTargetView(colorMS, nullptr, &rtvMS);
      dev->CreateRenderTargetView(outColor, nullptr, &rtvOutColor);
      dev->CreateRenderTargetView(outDepth, nullptr, &rtvOutDepth);
      D3D11_DEPTH_STENCIL_VIEW_DESC dvd{};
      dvd.Format = cfg.depthDsv; dvd.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DMS;
      ID3D11DepthStencilView *dsv;
      dev->CreateDepthStencilView(depthMS, &dvd, &dsv);
      D3D11_SHADER_RESOURCE_VIEW_DESC svd{};
      svd.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DMS;
      ID3D11ShaderResourceView *srvColor, *srvColorCopy, *srvDepth, *srvDepthCopy;
      svd.Format = cfg.color;
      dev->CreateShaderResourceView(colorMS, &svd, &srvColor);
      dev->CreateShaderResourceView(colorCopy, &svd, &srvColorCopy);
      svd.Format = cfg.depthSrv;
      dev->CreateShaderResourceView(depthMS, &svd, &srvDepth);
      dev->CreateShaderResourceView(depthCopy, &svd, &srvDepthCopy);
      cbData.samples = cfg.samples;
      ctx->UpdateSubresource(cb, 0, nullptr, &cbData, 0, 0);

      auto step = [&](int f, const char *what) {
         ctx->End(q);
         ctx->Flush();
         LARGE_INTEGER t0, t1;
         QueryPerformanceCounter(&t0);
         BOOL done = FALSE;
         int ms = 0;
         for (;;) {
            if (ctx->GetData(q, &done, sizeof(done), 0) == S_OK && done) break;
            QueryPerformanceCounter(&t1);
            ms = (int)((t1.QuadPart - t0.QuadPart) * 1000 / qpf.QuadPart);
            if (ms > limit) {
               printf("%-20s frame %d step %-26s STALL > %d ms\n", cfg.name, f, what, limit);
               ExitProcess(5);
            }
            Sleep(0);
         }
         QueryPerformanceCounter(&t1);
         ms = (int)((t1.QuadPart - t0.QuadPart) * 1000 / qpf.QuadPart);
         if (ms > worst_ms) worst_ms = ms;
         if (ms >= 50) printf("%-20s frame %d step %-26s %d ms\n", cfg.name, f, what, ms);
      };

      D3D11_VIEWPORT vp{0, 0, (float)W, (float)H, 0, 1};
      for (int f = 0; f < frames; f++) {
         ctx->ClearRenderTargetView(rtvMS, clearColor);
         ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 0.0f, 0);
         step(f, "clear");
         UINT stride = sizeof(Vtx), off = 0;
         ctx->IASetVertexBuffers(0, 1, &vb, &stride, &off);
         ctx->IASetInputLayout(il);
         ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
         ctx->VSSetShader(sceneVS, nullptr, 0);
         ctx->VSSetConstantBuffers(0, 1, &cb);
         ctx->PSSetShader(scenePS, nullptr, 0);
         ctx->RSSetViewports(1, &vp);
         ctx->RSSetState(rs);
         ctx->OMSetDepthStencilState(dss, 0);
         ctx->OMSetRenderTargets(1, &rtvMS, dsv);
         const UINT half = (rectVerts / 12) * 6;
         ctx->Draw(half, 0);
         step(f, "scene part 1");
         ctx->ResolveSubresource(resolved, 0, colorMS, 0, cfg.color);
         step(f, "resolve 1");
         ctx->OMSetRenderTargets(1, &rtvMS, dsv);
         ctx->Draw(rectVerts - half, half);
         ctx->Draw(totalVerts - rectVerts, rectVerts);
         step(f, "scene part 2 (load)");
         ctx->OMSetRenderTargets(0, nullptr, nullptr);
         ctx->ResolveSubresource(resolved, 0, colorMS, 0, cfg.color);
         step(f, "resolve 2");
         ctx->CopyResource(colorCopy, colorMS);
         step(f, "copy color MSAA");
         ctx->CopyResource(depthCopy, depthMS);
         step(f, "copy depth MSAA");
         for (int pass = 0; pass < 2; pass++) {
            ID3D11ShaderResourceView *cs = pass ? srvColorCopy : srvColor, *ds = pass ? srvDepthCopy : srvDepth;
            ctx->IASetInputLayout(nullptr);
            ctx->VSSetShader(fullVS, nullptr, 0);
            ctx->PSSetConstantBuffers(0, 1, &cb);
            ctx->OMSetDepthStencilState(nullptr, 0);
            ctx->OMSetRenderTargets(1, &rtvOutColor, nullptr);
            ctx->PSSetShader(colorPS, nullptr, 0);
            ctx->PSSetShaderResources(0, 1, &cs);
            ctx->Draw(3, 0);
            step(f, pass ? "msload color (copy)" : "msload color");
            ctx->OMSetRenderTargets(1, &rtvOutDepth, nullptr);
            ctx->PSSetShader(depthPS, nullptr, 0);
            ctx->PSSetShaderResources(0, 1, &ds);
            ctx->Draw(3, 0);
            step(f, pass ? "msload depth (copy)" : "msload depth");
            ID3D11ShaderResourceView *nul = nullptr;
            ctx->PSSetShaderResources(0, 1, &nul);
            ctx->OMSetRenderTargets(0, nullptr, nullptr);
         }
      }
      printf("%-20s done\n", cfg.name);
      srvColor->Release(); srvColorCopy->Release(); srvDepth->Release(); srvDepthCopy->Release();
      dsv->Release(); rtvMS->Release(); rtvOutColor->Release(); rtvOutDepth->Release();
      colorMS->Release(); colorCopy->Release(); depthMS->Release(); depthCopy->Release();
      resolved->Release(); outColor->Release(); outDepth->Release();
      colorPS->Release(); depthPS->Release();
   }
   printf("RESULT no step over %d ms (slowest %d ms)\n", limit, worst_ms);
   return 0;
}
