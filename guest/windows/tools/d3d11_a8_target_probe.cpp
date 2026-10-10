// d3d11_a8_target_probe.cpp: render into DXGI_FORMAT_A8_UNORM render
// targets the way Direct2D builds its glyph atlas (XAML and RichEdit text,
// Segoe Fluent Icons caption glyphs): clear, ClearView of a cell, a draw that
// writes alpha, blended draws, then sample the atlas into a BGRA target. Each
// step is read back and compared with the value D3D11 defines, so the same
// binary checks WARP (the reference) and the hardware driver.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_a8_target_probe.exe d3d11_a8_target_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest, any session):
//   d3d11_a8_target_probe.exe [helios|warp]
// Exit code 0 when every check passes, 1 otherwise.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstdlib>
#include <cwchar>

static const UINT S = 64;
static int g_fail = 0;

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

static const char* kVS =
  "float4 main(uint id : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((id << 1) & 2, id & 2);\n"
  "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
  "}\n";
static const char* kPSConst =
  "cbuffer C : register(b0) { float4 color; };\n"
  "float4 main() : SV_Target { return color; }\n";
// Samples the A8 atlas: what a glyph quad does (coverage from .a).
static const char* kPSSample =
  "Texture2D<float4> t : register(t0); SamplerState s : register(s0);\n"
  "float4 main(float4 pos : SV_Position) : SV_Target {\n"
  "  float a = t.Load(int3(pos.xy, 0)).a;\n"
  "  return float4(a, a, a, 1);\n"
  "}\n";

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  HRESULT hr = D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e);
  if (FAILED(hr)) { printf("compile %s: %s\n", target, e ? (const char*)e->GetBufferPointer() : "?"); exit(3); }
  return b;
}

struct Ctx {
  ID3D11Device* dev; ID3D11DeviceContext1* ctx;
};

static uint8_t ReadA8(Ctx& c, ID3D11Texture2D* tex, UINT x, UINT y) {
  D3D11_TEXTURE2D_DESC d; tex->GetDesc(&d);
  d.Usage = D3D11_USAGE_STAGING; d.BindFlags = 0; d.CPUAccessFlags = D3D11_CPU_ACCESS_READ; d.MiscFlags = 0;
  ID3D11Texture2D* st = nullptr; c.dev->CreateTexture2D(&d, nullptr, &st);
  c.ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{}; c.ctx->Map(st, 0, D3D11_MAP_READ, 0, &m);
  uint8_t v = ((const uint8_t*)m.pData)[y * m.RowPitch + x];
  c.ctx->Unmap(st, 0); st->Release();
  return v;
}

static uint32_t ReadBgra(Ctx& c, ID3D11Texture2D* tex, UINT x, UINT y) {
  D3D11_TEXTURE2D_DESC d; tex->GetDesc(&d);
  d.Usage = D3D11_USAGE_STAGING; d.BindFlags = 0; d.CPUAccessFlags = D3D11_CPU_ACCESS_READ; d.MiscFlags = 0;
  ID3D11Texture2D* st = nullptr; c.dev->CreateTexture2D(&d, nullptr, &st);
  c.ctx->CopyResource(st, tex);
  D3D11_MAPPED_SUBRESOURCE m{}; c.ctx->Map(st, 0, D3D11_MAP_READ, 0, &m);
  uint32_t v = *(const uint32_t*)((const uint8_t*)m.pData + y * m.RowPitch + x * 4);
  c.ctx->Unmap(st, 0); st->Release();
  return v;
}

static void Check(const char* what, int got, int want, int tol = 2) {
  bool ok = abs(got - want) <= tol;
  printf("%-58s got %3d want %3d  %s\n", what, got, want, ok ? "ok" : "FAIL");
  if (!ok) g_fail++;
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  IDXGIAdapter1* adapter = strcmp(sel, "warp") == 0 ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = strcmp(sel, "warp") == 0 ? D3D_DRIVER_TYPE_WARP
                       : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx0 = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1;
  HRESULT hr = D3D11CreateDevice(adapter, dt, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1,
                                 D3D11_SDK_VERSION, &dev, nullptr, &ctx0);
  printf("device (%s) hr=0x%08lx\n", sel, hr);
  if (FAILED(hr)) return 2;
  Ctx c{dev, nullptr};
  ctx0->QueryInterface(__uuidof(ID3D11DeviceContext1), (void**)&c.ctx);

  UINT support = 0;
  dev->CheckFormatSupport(DXGI_FORMAT_A8_UNORM, &support);
  printf("A8_UNORM support 0x%08x (render target %s, blendable %s)\n", support,
         support & D3D11_FORMAT_SUPPORT_RENDER_TARGET ? "yes" : "no",
         support & D3D11_FORMAT_SUPPORT_BLENDABLE ? "yes" : "no");

  ID3DBlob* vsb = Compile(kVS, "vs_4_0");
  ID3DBlob* psc = Compile(kPSConst, "ps_4_0");
  ID3DBlob* pss = Compile(kPSSample, "ps_4_0");
  ID3D11VertexShader* vs; ID3D11PixelShader* psConst; ID3D11PixelShader* psSample;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psc->GetBufferPointer(), psc->GetBufferSize(), nullptr, &psConst);
  dev->CreatePixelShader(pss->GetBufferPointer(), pss->GetBufferSize(), nullptr, &psSample);
  D3D11_BUFFER_DESC bd{16, D3D11_USAGE_DEFAULT, D3D11_BIND_CONSTANT_BUFFER, 0, 0, 0};
  ID3D11Buffer* cb; dev->CreateBuffer(&bd, nullptr, &cb);

  D3D11_TEXTURE2D_DESC td{};
  td.Width = S; td.Height = S; td.MipLevels = 1; td.ArraySize = 1; td.SampleDesc.Count = 1;
  td.Format = DXGI_FORMAT_A8_UNORM;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  ID3D11Texture2D* atlas; dev->CreateTexture2D(&td, nullptr, &atlas);
  ID3D11RenderTargetView* rtv; dev->CreateRenderTargetView(atlas, nullptr, &rtv);
  ID3D11ShaderResourceView* srv; dev->CreateShaderResourceView(atlas, nullptr, &srv);
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  ID3D11Texture2D* bgra; dev->CreateTexture2D(&td, nullptr, &bgra);
  ID3D11RenderTargetView* rtvB; dev->CreateRenderTargetView(bgra, nullptr, &rtvB);

  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE; rd.ScissorEnable = TRUE;
  ID3D11RasterizerState* rs; dev->CreateRasterizerState(&rd, &rs);
  D3D11_VIEWPORT vp{0, 0, (float)S, (float)S, 0, 1};
  c.ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  c.ctx->VSSetShader(vs, nullptr, 0);
  c.ctx->RSSetState(rs);
  c.ctx->RSSetViewports(1, &vp);
  c.ctx->PSSetConstantBuffers(0, 1, &cb);

  auto scissor = [&](LONG l, LONG t, LONG r, LONG b) { D3D11_RECT sr{l, t, r, b}; c.ctx->RSSetScissorRects(1, &sr); };
  auto color = [&](float r, float g, float b, float a) { float v[4] = {r, g, b, a}; c.ctx->UpdateSubresource(cb, 0, nullptr, v, 0, 0); };
  auto blend = [&](D3D11_BLEND src, D3D11_BLEND dst, D3D11_BLEND srcA, D3D11_BLEND dstA, D3D11_BLEND_OP opA) {
    D3D11_BLEND_DESC b{}; b.RenderTarget[0].BlendEnable = TRUE;
    b.RenderTarget[0].SrcBlend = src; b.RenderTarget[0].DestBlend = dst; b.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    b.RenderTarget[0].SrcBlendAlpha = srcA; b.RenderTarget[0].DestBlendAlpha = dstA; b.RenderTarget[0].BlendOpAlpha = opA;
    b.RenderTarget[0].RenderTargetWriteMask = 0xf;
    ID3D11BlendState* s; dev->CreateBlendState(&b, &s); c.ctx->OMSetBlendState(s, nullptr, 0xffffffff); s->Release();
  };

  // 1. Clear, then a plain draw writing alpha 0.8 into the left half.
  float zero[4] = {0, 0, 0, 0};
  c.ctx->ClearRenderTargetView(rtv, zero);
  c.ctx->OMSetRenderTargets(1, &rtv, nullptr);
  c.ctx->OMSetBlendState(nullptr, nullptr, 0xffffffff);
  c.ctx->PSSetShader(psConst, nullptr, 0);
  color(0.2f, 0.4f, 0.6f, 0.8f);
  scissor(0, 0, S / 2, S);
  c.ctx->Draw(3, 0);
  Check("draw, no blend: alpha 0.8 lands", ReadA8(c, atlas, 4, 4), 204);
  Check("draw, no blend: outside the scissor stays clear", ReadA8(c, atlas, S - 4, 4), 0);

  // 2. ClearView of a cell (D2D clears a glyph cell before drawing into it).
  float cv[4] = {0.9f, 0.9f, 0.9f, 0.25f};
  D3D11_RECT cell{40, 8, 56, 24};
  c.ctx->ClearView(rtv, cv, &cell, 1);
  Check("ClearView cell: alpha 0.25", ReadA8(c, atlas, 48, 16), 64);
  Check("ClearView cell: outside the cell untouched", ReadA8(c, atlas, 48, 40), 0);
  float cr[4] = {0.9f, 0.9f, 0.9f, 0.5f};
  c.ctx->ClearRenderTargetView(rtv, cr);
  Check("ClearRenderTargetView: alpha 0.5", ReadA8(c, atlas, 10, 10), 128);
  c.ctx->ClearRenderTargetView(rtv, zero);

  // 3. Additive coverage, the way a glyph rasterizer accumulates.
  blend(D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD);
  color(0, 0, 0, 0.25f);
  scissor(0, 0, S, S);
  c.ctx->Draw(3, 0);
  c.ctx->Draw(3, 0);
  Check("additive blend: 0.25 + 0.25", ReadA8(c, atlas, 20, 20), 128);

  // 4. Source-over with alpha in the source (premultiplied text over a cell).
  c.ctx->ClearRenderTargetView(rtv, cr);
  blend(D3D11_BLEND_ONE, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_OP_ADD);
  color(0, 0, 0, 0.5f);
  c.ctx->Draw(3, 0);
  Check("source-over: 0.5 over 0.5", ReadA8(c, atlas, 20, 20), 191);

  // 5. MAX blend on alpha.
  c.ctx->ClearRenderTargetView(rtv, cr);
  blend(D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_ONE, D3D11_BLEND_OP_MAX);
  color(0, 0, 0, 0.75f);
  c.ctx->Draw(3, 0);
  Check("max blend: max(0.5, 0.75)", ReadA8(c, atlas, 20, 20), 191);

  // 6. The atlas sampled into a BGRA target: coverage comes from .a.
  c.ctx->ClearRenderTargetView(rtv, zero);
  c.ctx->OMSetBlendState(nullptr, nullptr, 0xffffffff);
  color(0, 0, 0, 0.8f);
  scissor(0, 0, S / 2, S);
  c.ctx->PSSetShader(psConst, nullptr, 0);
  c.ctx->Draw(3, 0);
  c.ctx->OMSetRenderTargets(1, &rtvB, nullptr);
  scissor(0, 0, S, S);
  c.ctx->PSSetShader(psSample, nullptr, 0);
  c.ctx->PSSetShaderResources(0, 1, &srv);
  c.ctx->Draw(3, 0);
  uint32_t px = ReadBgra(c, bgra, 4, 4), px2 = ReadBgra(c, bgra, S - 4, 4);
  Check("sampled atlas .a inside the glyph (B channel)", px & 0xff, 204);
  Check("sampled atlas .a outside the glyph (B channel)", px2 & 0xff, 0);

  printf("%s: %d check(s) failed\n", sel, g_fail);
  return g_fail ? 1 : 0;
}
