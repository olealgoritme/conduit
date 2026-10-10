// d3d11_msaa_probe.cpp: multisampled color and depth render targets checked
// texel for texel, the way a D3D11 engine with MSAA uses them: a scene drawn
// into MSAA color + depth in two parts with a ResolveSubresource between them
// (so the second part loads both attachments), a final resolve, the MSAA
// depth and color read in shaders through Texture2DMS loads, and CopyResource
// of both MSAA surfaces read the same way.
//
// The scene is pixel-aligned rectangles at distinct depths (reverse Z,
// GREATER), so every sample of a pixel is covered or not and each result is
// known exactly on the CPU, whatever the sample positions. A band of rotated
// triangles adds partially covered pixels; there the hardware resolve is
// compared with a box-filter resolve in a shader, and every frame with the
// first.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_msaa_probe.exe d3d11_msaa_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_msaa_probe.exe [helios|warp] [width height] [frames]
// Exit code 0 when every case matches.
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

// Scene: position in pixels, depth, color.
static const char* kSceneVS =
  "cbuffer C : register(b0) { float2 size; };\n"
  "struct V { float2 p : POS; float d : DEPTH; float4 c : COLOR; };\n"
  "struct O { float4 pos : SV_Position; float4 c : COLOR; };\n"
  "O main(V v) { O o; o.pos = float4(v.p / size * float2(2, -2) + float2(-1, 1), v.d, 1);\n"
  "  o.c = v.c; return o; }\n";
static const char* kScenePS =
  "float4 main(float4 pos : SV_Position, float4 c : COLOR) : SV_Target { return c; }\n";
static const char* kFullVS =
  "float4 main(uint id : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((id << 1) & 2, id & 2);\n"
  "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
  "}\n";
// Box-filter resolve of the MSAA color through Texture2DMS loads; alpha of
// the second target says whether all samples were equal.
static const char* kColorPS =
  "#if MS\n"
  "Texture2DMS<float4> t : register(t0);\n"
  "#else\n"
  "Texture2D<float4> t : register(t0);\n"
  "#endif\n"
  "cbuffer C : register(b0) { float2 size; uint samples; };\n"
  "float4 main(float4 pos : SV_Position) : SV_Target {\n"
  "  int2 p = int2(pos.xy); float4 s = 0;\n"
  "#if MS\n"
  "  for (uint i = 0; i < samples; i++) s += t.Load(p, i);\n"
  "  return s / samples;\n"
  "#else\n"
  "  return t.Load(int3(p, 0));\n"
  "#endif\n"
  "}\n";
// Min and max depth over the samples of a pixel.
static const char* kDepthPS =
  "#if MS\n"
  "Texture2DMS<float> t : register(t0);\n"
  "#else\n"
  "Texture2D<float> t : register(t0);\n"
  "#endif\n"
  "cbuffer C : register(b0) { float2 size; uint samples; };\n"
  "float2 main(float4 pos : SV_Position) : SV_Target {\n"
  "  int2 p = int2(pos.xy);\n"
  "#if MS\n"
  "  float lo = 2, hi = -1;\n"
  "  for (uint i = 0; i < samples; i++) { float d = t.Load(p, i); lo = min(lo, d); hi = max(hi, d); }\n"
  "  return float2(lo, hi);\n"
  "#else\n"
  "  float d = t.Load(int3(p, 0)); return float2(d, d);\n"
  "#endif\n"
  "}\n";

static ID3DBlob* Compile(const char* src, const char* target, bool ms) {
  D3D_SHADER_MACRO defs[] = {{"MS", ms ? "1" : "0"}, {nullptr, nullptr}};
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, defs, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

struct Vtx { float x, y, d; float c[4]; };
struct Rect { int x0, y0, x1, y1; float d; float c[4]; };

static uint32_t g_rng = 12345;
static uint32_t Rnd() { g_rng = g_rng * 1664525u + 1013904223u; return g_rng >> 8; }

static void PushQuad(std::vector<Vtx>& v, const Rect& r) {
  Vtx a{(float)r.x0, (float)r.y0, r.d, {r.c[0], r.c[1], r.c[2], r.c[3]}};
  Vtx b = a; b.x = (float)r.x1;
  Vtx c = a; c.y = (float)r.y1;
  Vtx d = a; d.x = (float)r.x1; d.y = (float)r.y1;
  v.push_back(a); v.push_back(b); v.push_back(c);
  v.push_back(c); v.push_back(b); v.push_back(d);
}

struct Readback {
  ID3D11Texture2D* st = nullptr;
  D3D11_MAPPED_SUBRESOURCE m{};
};

static bool Map(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* src, Readback& rb) {
  D3D11_TEXTURE2D_DESC d; src->GetDesc(&d);
  d.Usage = D3D11_USAGE_STAGING; d.BindFlags = 0; d.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  d.MiscFlags = 0;
  if (!rb.st && FAILED(dev->CreateTexture2D(&d, nullptr, &rb.st))) return false;
  ctx->CopyResource(rb.st, src);
  return SUCCEEDED(ctx->Map(rb.st, 0, D3D11_MAP_READ, 0, &rb.m));
}

static float Half(uint16_t h) {
  uint32_t s = (h >> 15) & 1, e = (h >> 10) & 31, m = h & 1023;
  float v = e == 0 ? std::ldexp((float)m, -24) : std::ldexp((float)(m | 1024), (int)e - 25);
  return s ? -v : v;
}
static float F11(uint32_t x) { uint32_t e = (x >> 6) & 31, m = x & 63;
  return e == 0 ? std::ldexp((float)m, -20) : std::ldexp((float)(m | 64), (int)e - 21); }
static float F10(uint32_t x) { uint32_t e = (x >> 5) & 31, m = x & 31;
  return e == 0 ? std::ldexp((float)m, -19) : std::ldexp((float)(m | 32), (int)e - 20); }

// Reads texel (x,y) of a resolved color target as float RGBA.
static void ColorAt(DXGI_FORMAT f, const D3D11_MAPPED_SUBRESOURCE& m, int x, int y, float out[4]) {
  const uint8_t* row = (const uint8_t*)m.pData + (size_t)y * m.RowPitch;
  switch (f) {
  case DXGI_FORMAT_R8G8B8A8_UNORM:
    for (int k = 0; k < 4; k++) out[k] = row[x * 4 + k] / 255.0f;
    break;
  case DXGI_FORMAT_R16G16B16A16_FLOAT:
    for (int k = 0; k < 4; k++) out[k] = Half(((const uint16_t*)row)[x * 4 + k]);
    break;
  case DXGI_FORMAT_R11G11B10_FLOAT: {
    uint32_t v = ((const uint32_t*)row)[x];
    out[0] = F11(v & 2047); out[1] = F11((v >> 11) & 2047); out[2] = F10(v >> 22); out[3] = 1;
    break;
  }
  default:
    for (int k = 0; k < 4; k++) out[k] = ((const float*)row)[x * 4 + k];
  }
}

struct Config { UINT samples; DXGI_FORMAT color; DXGI_FORMAT depthTex, depthDsv, depthSrv; const char* name; };

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  const int W = argc > 3 ? atoi(argv[2]) : 2560, H = argc > 3 ? atoi(argv[3]) : 1440;
  const int frames = argc > 4 ? atoi(argv[4]) : 8;
  const bool warp = strcmp(sel, "warp") == 0;
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP
                       : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  printf("device %s %dx%d frames=%d\n", warp ? "WARP" : (adapter ? "Helios" : "default"), W, H, frames);

  ID3DBlob* b = Compile(kSceneVS, "vs_5_0", false);
  ID3D11VertexShader* sceneVS; dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &sceneVS);
  D3D11_INPUT_ELEMENT_DESC ie[] = {
    {"POS", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
    {"DEPTH", 0, DXGI_FORMAT_R32_FLOAT, 0, 8, D3D11_INPUT_PER_VERTEX_DATA, 0},
    {"COLOR", 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 12, D3D11_INPUT_PER_VERTEX_DATA, 0},
  };
  ID3D11InputLayout* il; dev->CreateInputLayout(ie, 3, b->GetBufferPointer(), b->GetBufferSize(), &il);
  b = Compile(kScenePS, "ps_5_0", false);
  ID3D11PixelShader* scenePS; dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &scenePS);
  b = Compile(kFullVS, "vs_5_0", false);
  ID3D11VertexShader* fullVS; dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &fullVS);
  ID3D11PixelShader *colorPS[2], *depthPS[2];
  for (int ms = 0; ms < 2; ms++) {
    b = Compile(kColorPS, "ps_5_0", ms); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &colorPS[ms]);
    b = Compile(kDepthPS, "ps_5_0", ms); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &depthPS[ms]);
  }

  // Scene: rects over the top 7/8, rotated triangles in the bottom band.
  const int bandY = H * 7 / 8;
  std::vector<Rect> rects;
  const int nRects = 160;
  for (int i = 0; i < nRects; i++) {
    Rect r;
    int w = 16 + (int)(Rnd() % (W / 4)), h = 16 + (int)(Rnd() % (bandY / 3));
    r.x0 = (int)(Rnd() % (W - w)); r.y0 = (int)(Rnd() % (bandY - h));
    r.x1 = r.x0 + w; r.y1 = r.y0 + h;
    r.d = (float)(1 + i * 5) / 1024.0f;  // distinct, exact in D32 and close in D24
    for (int k = 0; k < 4; k++) r.c[k] = (float)(Rnd() % 9) / 8.0f;
    rects.push_back(r);
  }
  // Shuffle draw order so depth, not order, decides.
  for (int i = nRects - 1; i > 0; i--) { int j = (int)(Rnd() % (i + 1)); Rect t = rects[i]; rects[i] = rects[j]; rects[j] = t; }
  std::vector<Vtx> vtx;
  for (const Rect& r : rects) PushQuad(vtx, r);
  const UINT rectVerts = (UINT)vtx.size();
  for (int i = 0; i < 400; i++) {
    float cx = (float)(Rnd() % W), cy = (float)(bandY + Rnd() % (H - bandY));
    float a = (float)(Rnd() % 6283) / 1000.0f, s = 8.0f + (float)(Rnd() % 60);
    float d = (float)(Rnd() % 1000) / 1024.0f + 0.001f;
    float c[4]; for (int k = 0; k < 4; k++) c[k] = (float)(Rnd() % 9) / 8.0f;
    for (int k = 0; k < 3; k++) {
      float ang = a + k * 2.0944f;
      Vtx v{cx + s * std::cos(ang), cy + s * std::sin(ang), d, {c[0], c[1], c[2], c[3]}};
      if (v.y < bandY) v.y = (float)bandY;
      vtx.push_back(v);
    }
  }
  const UINT totalVerts = (UINT)vtx.size();
  D3D11_BUFFER_DESC bd{}; bd.ByteWidth = (UINT)(vtx.size() * sizeof(Vtx)); bd.Usage = D3D11_USAGE_IMMUTABLE;
  bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
  D3D11_SUBRESOURCE_DATA sd{vtx.data(), 0, 0};
  ID3D11Buffer* vb; dev->CreateBuffer(&bd, &sd, &vb);
  struct { float size[2]; UINT samples; UINT pad; } cbData{{(float)W, (float)H}, 1, 0};
  bd = {}; bd.ByteWidth = 16; bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  ID3D11Buffer* cb; dev->CreateBuffer(&bd, nullptr, &cb);

  // Expected top rect per pixel of the rect area.
  std::vector<int> top((size_t)W * bandY, -1);
  {
    std::vector<float> zb((size_t)W * bandY, 0.0f);
    for (int i = 0; i < nRects; i++) {
      const Rect& r = rects[i];
      for (int y = r.y0; y < r.y1; y++)
        for (int x = r.x0; x < r.x1; x++)
          if (r.d > zb[(size_t)y * W + x]) { zb[(size_t)y * W + x] = r.d; top[(size_t)y * W + x] = i; }
    }
  }
  const float clearColor[4] = {0.25f, 0.5f, 0.75f, 1.0f};

  D3D11_DEPTH_STENCIL_DESC dsd{}; dsd.DepthEnable = TRUE; dsd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL;
  dsd.DepthFunc = D3D11_COMPARISON_GREATER;
  ID3D11DepthStencilState* dss; dev->CreateDepthStencilState(&dsd, &dss);
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE; rd.MultisampleEnable = TRUE;
  ID3D11RasterizerState* rs; dev->CreateRasterizerState(&rd, &rs);

  const Config configs[] = {
    {1, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "1x RGBA8 D32"},
    {4, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "4x RGBA8 D32"},
    {4, DXGI_FORMAT_R11G11B10_FLOAT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, "4x R11G11B10 D24S8"},
    {4, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "4x RGBA16F D32"},
    {8, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, "8x RGBA8 D32"},
    {8, DXGI_FORMAT_R11G11B10_FLOAT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, "8x R11G11B10 D24S8"},
    {2, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, "2x RGBA16F D24S8"},
  };
  int failedCases = 0;
  for (const Config& cfg : configs) {
    UINT q = 0;
    if (FAILED(dev->CheckMultisampleQualityLevels(cfg.color, cfg.samples, &q)) || !q) {
      printf("%-22s unsupported\n", cfg.name); continue;
    }
    const bool ms = cfg.samples > 1;
    D3D11_TEXTURE2D_DESC td{}; td.Width = W; td.Height = H; td.MipLevels = 1; td.ArraySize = 1;
    td.SampleDesc.Count = cfg.samples; td.Usage = D3D11_USAGE_DEFAULT;
    td.Format = cfg.color; td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
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
    D3D11_DEPTH_STENCIL_VIEW_DESC dvd{}; dvd.Format = cfg.depthDsv;
    dvd.ViewDimension = ms ? D3D11_DSV_DIMENSION_TEXTURE2DMS : D3D11_DSV_DIMENSION_TEXTURE2D;
    ID3D11DepthStencilView* dsv; dev->CreateDepthStencilView(depthMS, &dvd, &dsv);
    D3D11_SHADER_RESOURCE_VIEW_DESC svd{}; svd.ViewDimension = ms ? D3D11_SRV_DIMENSION_TEXTURE2DMS : D3D11_SRV_DIMENSION_TEXTURE2D;
    svd.Texture2D.MipLevels = 1;
    ID3D11ShaderResourceView *srvColor, *srvColorCopy, *srvDepth, *srvDepthCopy;
    svd.Format = cfg.color;
    dev->CreateShaderResourceView(colorMS, &svd, &srvColor);
    dev->CreateShaderResourceView(colorCopy, &svd, &srvColorCopy);
    svd.Format = cfg.depthSrv;
    dev->CreateShaderResourceView(depthMS, &svd, &srvDepth);
    dev->CreateShaderResourceView(depthCopy, &svd, &srvDepthCopy);
    cbData.samples = cfg.samples;
    ctx->UpdateSubresource(cb, 0, nullptr, &cbData, 0, 0);

    const float dTol = cfg.depthDsv == DXGI_FORMAT_D32_FLOAT ? 0.0f : 1.5f / 16777215.0f;
    const float cTol = cfg.color == DXGI_FORMAT_R8G8B8A8_UNORM ? 0.5f / 255.0f : 1e-6f;
    // Counters: resolve, box resolve, depth, copy color, copy depth, band
    // (hardware vs box resolve), frame-to-frame.
    unsigned bad[7] = {0}; int firstX[7], firstY[7]; float firstGot[7][4], firstWant[7][4];
    std::vector<float> band0;
    Readback rbRes, rbCol, rbDep;
    D3D11_VIEWPORT vp{0, 0, (float)W, (float)H, 0, 1};
    for (int f = 0; f < frames; f++) {
      // Scene part 1
      ctx->ClearRenderTargetView(rtvMS, clearColor);
      ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 0.0f, 0);
      UINT stride = sizeof(Vtx), off = 0;
      ctx->IASetVertexBuffers(0, 1, &vb, &stride, &off);
      ctx->IASetInputLayout(il);
      ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
      ctx->VSSetShader(sceneVS, nullptr, 0); ctx->VSSetConstantBuffers(0, 1, &cb);
      ctx->PSSetShader(scenePS, nullptr, 0);
      ctx->RSSetViewports(1, &vp); ctx->RSSetState(rs);
      ctx->OMSetDepthStencilState(dss, 0);
      ctx->OMSetRenderTargets(1, &rtvMS, dsv);
      const UINT half = (rectVerts / 12) * 6;
      ctx->Draw(half, 0);
      // A resolve in the middle ends the render pass; part 2 loads.
      if (ms) ctx->ResolveSubresource(resolved, 0, colorMS, 0, cfg.color);
      else ctx->CopyResource(resolved, colorMS);
      ctx->OMSetRenderTargets(1, &rtvMS, dsv);
      ctx->Draw(rectVerts - half, half);
      ctx->Draw(totalVerts - rectVerts, rectVerts);
      ctx->OMSetRenderTargets(0, nullptr, nullptr);
      if (ms) ctx->ResolveSubresource(resolved, 0, colorMS, 0, cfg.color);
      else ctx->CopyResource(resolved, colorMS);
      ctx->CopyResource(colorCopy, colorMS);
      ctx->CopyResource(depthCopy, depthMS);

      for (int pass = 0; pass < 2; pass++) {
        // pass 0: the surfaces; pass 1: their copies
        ID3D11ShaderResourceView* cs = pass ? srvColorCopy : srvColor;
        ID3D11ShaderResourceView* ds = pass ? srvDepthCopy : srvDepth;
        ctx->IASetInputLayout(nullptr);
        ctx->VSSetShader(fullVS, nullptr, 0);
        ctx->PSSetConstantBuffers(0, 1, &cb);
        ctx->OMSetDepthStencilState(nullptr, 0);
        ctx->OMSetRenderTargets(1, &rtvOutColor, nullptr);
        ctx->PSSetShader(colorPS[ms], nullptr, 0); ctx->PSSetShaderResources(0, 1, &cs);
        ctx->Draw(3, 0);
        ctx->OMSetRenderTargets(1, &rtvOutDepth, nullptr);
        ctx->PSSetShader(depthPS[ms], nullptr, 0); ctx->PSSetShaderResources(0, 1, &ds);
        ctx->Draw(3, 0);
        ID3D11ShaderResourceView* nul = nullptr; ctx->PSSetShaderResources(0, 1, &nul);
        ctx->OMSetRenderTargets(0, nullptr, nullptr);

        if (!Map(dev, ctx, outColor, rbCol) || !Map(dev, ctx, outDepth, rbDep)) { printf("map failed\n"); return 4; }
        if (pass == 0 && !Map(dev, ctx, resolved, rbRes)) { printf("map failed\n"); return 4; }
        auto note = [&](int k, int x, int y, const float* got, const float* want) {
          if (!bad[k]++) { firstX[k] = x; firstY[k] = y;
            for (int c = 0; c < 4; c++) { firstGot[k][c] = got[c]; firstWant[k][c] = want[c]; } }
        };
        for (int y = 0; y < H; y++) {
          const float* oc = (const float*)((const uint8_t*)rbCol.m.pData + (size_t)y * rbCol.m.RowPitch);
          const float* od = (const float*)((const uint8_t*)rbDep.m.pData + (size_t)y * rbDep.m.RowPitch);
          for (int x = 0; x < W; x++) {
            if (y < bandY) {
              int t = top[(size_t)y * W + x];
              const float* want = t >= 0 ? rects[t].c : clearColor;
              float wd = t >= 0 ? rects[t].d : 0.0f;
              float got[4]; for (int c = 0; c < 4; c++) got[c] = oc[x * 4 + c];
              bool ok = true;
              for (int c = 0; c < (cfg.color == DXGI_FORMAT_R11G11B10_FLOAT ? 3 : 4); c++) ok &= std::fabs(got[c] - want[c]) <= cTol;
              if (!ok) note(pass ? 3 : 1, x, y, got, want);
              float gd[4] = {od[x * 2], od[x * 2 + 1], 0, 0}, wdv[4] = {wd, wd, 0, 0};
              if (std::fabs(gd[0] - wd) > dTol || std::fabs(gd[1] - wd) > dTol) note(pass ? 4 : 2, x, y, gd, wdv);
              if (pass == 0) {
                ColorAt(cfg.color, rbRes.m, x, y, got);
                ok = true;
                for (int c = 0; c < (cfg.color == DXGI_FORMAT_R11G11B10_FLOAT ? 3 : 4); c++) ok &= std::fabs(got[c] - want[c]) <= cTol;
                if (!ok) note(0, x, y, got, want);
              }
            } else if (pass == 0) {
              float hw[4]; ColorAt(cfg.color, rbRes.m, x, y, hw);
              const float* box = &oc[x * 4];
              float tol = cfg.color == DXGI_FORMAT_R8G8B8A8_UNORM ? 1.5f / 255.0f : 0.02f;
              bool ok = true;
              for (int c = 0; c < 3; c++) ok &= std::fabs(hw[c] - box[c]) <= tol;
              if (!ok) note(5, x, y, hw, box);
              size_t idx = ((size_t)(y - bandY) * W + x) * 3;
              if (f == 0) { if (band0.size() < (size_t)(H - bandY) * W * 3) band0.resize((size_t)(H - bandY) * W * 3);
                for (int c = 0; c < 3; c++) band0[idx + c] = hw[c]; }
              else { bool same = true; for (int c = 0; c < 3; c++) same &= band0[idx + c] == hw[c];
                if (!same) note(6, x, y, hw, &band0[idx]); }
            }
          }
        }
        ctx->Unmap(rbCol.st, 0); ctx->Unmap(rbDep.st, 0);
        if (pass == 0) ctx->Unmap(rbRes.st, 0);
      }
    }
    static const char* names[7] = {"resolve", "msload", "depth", "copy-msload", "copy-depth", "band", "frame"};
    unsigned total = 0; for (unsigned v : bad) total += v;
    printf("%-22s %s", cfg.name, total ? "FAIL" : "ok");
    for (int k = 0; k < 7; k++) if (bad[k]) printf(" %s=%u", names[k], bad[k]);
    printf("\n");
    for (int k = 0; k < 7; k++) if (bad[k])
      printf("    %s first (%d,%d) got %.4f %.4f %.4f %.4f want %.4f %.4f %.4f %.4f\n", names[k], firstX[k], firstY[k],
             firstGot[k][0], firstGot[k][1], firstGot[k][2], firstGot[k][3],
             firstWant[k][0], firstWant[k][1], firstWant[k][2], firstWant[k][3]);
    if (total) failedCases++;
    rbRes.st->Release(); rbCol.st->Release(); rbDep.st->Release();
    srvColor->Release(); srvColorCopy->Release(); srvDepth->Release(); srvDepthCopy->Release();
    dsv->Release(); rtvMS->Release(); rtvOutColor->Release(); rtvOutDepth->Release();
    colorMS->Release(); colorCopy->Release(); depthMS->Release(); depthCopy->Release();
    resolved->Release(); outColor->Release(); outDepth->Release();
  }
  printf("RESULT %s (%d failing configs)\n", failedCases ? "FAIL" : "PASS", failedCases);
  return failedCases ? 1 : 0;
}
