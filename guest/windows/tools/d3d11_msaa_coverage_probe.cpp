// d3d11_msaa_coverage_probe.cpp: per-sample coverage of multisampled draws,
// checked sample by sample through Texture2DMS loads: alpha-to-coverage
// (one target and with a second target whose alpha differs, with discard,
// with depth), the blend state's sample mask, SV_Coverage written by the
// pixel shader, and sample-frequency shading (SV_SampleIndex).
//
// Every case draws pixel-aligned regions whose expected samples are known:
// alpha 0 or 1 covers no or all samples, a sample mask or SV_Coverage names
// the samples, and a per-sample shader writes a value per sample index.
// Alpha 0.5 is only checked for samples being the clear color or the drawn
// color, and for about half of them being covered.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_msaa_coverage_probe.exe d3d11_msaa_coverage_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_msaa_coverage_probe.exe [helios|warp] [width height]
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

static const char* kFullVS =
  "cbuffer C : register(b0) { float depth; uint mode; uint sampleIdx; uint mask; };\n"
  "float4 main(uint id : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((id << 1) & 2, id & 2);\n"
  "  return float4(p * float2(2, -2) + float2(-1, 1), depth, 1);\n"
  "}\n";

// Region of a pixel: 32x32 blocks.
// mode 0: A2C, alpha 0/1 by region          mode 1: A2C, MRT, RT1 alpha inverted
// mode 2: A2C, alpha 0.5                    mode 3: plain draw (sample mask from blend state)
// mode 4: SV_Coverage = mask rotated by region
// mode 5: per-sample shading, value by SV_SampleIndex
// mode 6: A2C + discard on region % 3 == 0
static const char* kDrawPS =
  "cbuffer C : register(b0) { float depth; uint mode; uint sampleIdx; uint mask; };\n"
  "struct O { float4 c0 : SV_Target0; float4 c1 : SV_Target1;\n"
  "#if COV\n"
  "  uint cov : SV_Coverage;\n"
  "#endif\n"
  "};\n"
  "O main(float4 pos : SV_Position\n"
  "#if SS\n"
  "  , uint si : SV_SampleIndex\n"
  "#endif\n"
  "  ) {\n"
  "  uint2 b = uint2(pos.xy) / 32; uint r = b.x + b.y * 977;\n"
  "  O o; float a = (r & 1) ? 1.0 : 0.0;\n"
  "  if (mode == 2) a = 0.5;\n"
  "  if (mode == 6 && (r % 3) == 0) discard;\n"
  "  o.c0 = float4(1.0, 0.25, 0.5, a);\n"
  "  o.c1 = float4(0.5, 0.5, 0.5, 1.0 - a);\n"
  "#if COV\n"
  "  o.cov = ((mask << (r % 4)) | (mask >> (8 - (r % 4)))) & 0xff;\n"
  "#endif\n"
  "#if SS\n"
  "  o.c0 = float4((si + 1) / 16.0, 0.75, 0.125, 1.0);\n"
  "#endif\n"
  "  return o;\n"
  "}\n";

static const char* kLoadPS =
  "Texture2DMS<float4> t : register(t0);\n"
  "cbuffer C : register(b0) { float depth; uint mode; uint sampleIdx; uint mask; };\n"
  "float4 main(float4 pos : SV_Position) : SV_Target { return t.Load(int2(pos.xy), sampleIdx); }\n";

static ID3DBlob* Compile(const char* src, const char* target, const D3D_SHADER_MACRO* defs) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, defs, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

struct CB { float depth; UINT mode; UINT sampleIdx; UINT mask; };

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  const int W = argc > 3 ? atoi(argv[2]) : 1024, H = argc > 3 ? atoi(argv[3]) : 512;
  const bool warp = strcmp(sel, "warp") == 0;
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  printf("device %s %dx%d\n", warp ? "WARP" : (adapter ? "Helios" : "default"), W, H);

  ID3DBlob* b = Compile(kFullVS, "vs_5_0", nullptr);
  ID3D11VertexShader* vs; dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &vs);
  D3D_SHADER_MACRO plain[] = {{"COV", "0"}, {"SS", "0"}, {nullptr, nullptr}};
  D3D_SHADER_MACRO cov[] = {{"COV", "1"}, {"SS", "0"}, {nullptr, nullptr}};
  D3D_SHADER_MACRO ss[] = {{"COV", "0"}, {"SS", "1"}, {nullptr, nullptr}};
  ID3D11PixelShader *psPlain, *psCov, *psSS, *psLoad;
  b = Compile(kDrawPS, "ps_5_0", plain); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &psPlain);
  b = Compile(kDrawPS, "ps_5_0", cov); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &psCov);
  b = Compile(kDrawPS, "ps_5_0", ss); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &psSS);
  b = Compile(kLoadPS, "ps_5_0", nullptr); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &psLoad);

  D3D11_BUFFER_DESC bd{}; bd.ByteWidth = sizeof(CB); bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  ID3D11Buffer* cb; dev->CreateBuffer(&bd, nullptr, &cb);
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE;
  rd.MultisampleEnable = TRUE;
  ID3D11RasterizerState* rs; dev->CreateRasterizerState(&rd, &rs);
  D3D11_BLEND_DESC bld{}; bld.IndependentBlendEnable = FALSE;
  bld.RenderTarget[0].RenderTargetWriteMask = 0xf;
  ID3D11BlendState* bsPlain; dev->CreateBlendState(&bld, &bsPlain);
  bld.AlphaToCoverageEnable = TRUE;
  ID3D11BlendState* bsA2C; dev->CreateBlendState(&bld, &bsA2C);
  D3D11_DEPTH_STENCIL_DESC dsd{}; dsd.DepthEnable = TRUE; dsd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL;
  dsd.DepthFunc = D3D11_COMPARISON_GREATER;
  ID3D11DepthStencilState* dss; dev->CreateDepthStencilState(&dsd, &dss);
  dsd.DepthEnable = FALSE;
  ID3D11DepthStencilState* dssOff; dev->CreateDepthStencilState(&dsd, &dssOff);

  const float clear[4] = {0.0f, 0.0f, 0.0f, 0.0f};
  const float drawn[4] = {1.0f, 0.25f, 0.5f, 1.0f};
  const float behind[4] = {0.0f, 1.0f, 0.0f, 1.0f};
  int failed = 0;
  for (UINT n : {2u, 4u, 8u}) {
    D3D11_TEXTURE2D_DESC td{}; td.Width = W; td.Height = H; td.MipLevels = 1; td.ArraySize = 1;
    td.SampleDesc.Count = n; td.Usage = D3D11_USAGE_DEFAULT; td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    ID3D11Texture2D *rt0, *rt1, *ds, *out, *st;
    dev->CreateTexture2D(&td, nullptr, &rt0); dev->CreateTexture2D(&td, nullptr, &rt1);
    td.Format = DXGI_FORMAT_D32_FLOAT; td.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    dev->CreateTexture2D(&td, nullptr, &ds);
    td.SampleDesc.Count = 1; td.Format = DXGI_FORMAT_R32G32B32A32_FLOAT; td.BindFlags = D3D11_BIND_RENDER_TARGET;
    dev->CreateTexture2D(&td, nullptr, &out);
    td.Usage = D3D11_USAGE_STAGING; td.BindFlags = 0; td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    dev->CreateTexture2D(&td, nullptr, &st);
    ID3D11RenderTargetView *rtv0, *rtv1, *rtvOut; ID3D11DepthStencilView* dsv; ID3D11ShaderResourceView* srv;
    dev->CreateRenderTargetView(rt0, nullptr, &rtv0); dev->CreateRenderTargetView(rt1, nullptr, &rtv1);
    dev->CreateRenderTargetView(out, nullptr, &rtvOut); dev->CreateDepthStencilView(ds, nullptr, &dsv);
    dev->CreateShaderResourceView(rt0, nullptr, &srv);
    const UINT full = (1u << n) - 1;
    const UINT smask = n == 2 ? 0x1 : n == 4 ? 0x5 : 0x55;

    struct Case { const char* name; int mode; };
    const Case cases[] = {{"a2c", 0}, {"a2c-mrt", 1}, {"a2c-half", 2}, {"samplemask", 3},
                          {"sv_coverage", 4}, {"sample-shading", 5}, {"a2c-discard", 6}, {"a2c-depth", 7}};
    for (const Case& c : cases) {
      D3D11_VIEWPORT vp{0, 0, (float)W, (float)H, 0, 1};
      ctx->RSSetViewports(1, &vp); ctx->RSSetState(rs);
      ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
      ctx->IASetInputLayout(nullptr);
      ctx->VSSetShader(vs, nullptr, 0); ctx->VSSetConstantBuffers(0, 1, &cb); ctx->PSSetConstantBuffers(0, 1, &cb);
      ctx->ClearRenderTargetView(rtv0, clear); ctx->ClearRenderTargetView(rtv1, clear);
      ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH, 0.0f, 0);
      ID3D11RenderTargetView* rtvs[2] = {rtv0, rtv1};
      int mode = c.mode == 7 ? 0 : c.mode;
      if (c.mode == 7) {
        // Opaque green behind, at depth 0.25
        CB d{0.25f, 3, 0, 0}; ctx->UpdateSubresource(cb, 0, nullptr, &d, 0, 0);
        ctx->OMSetRenderTargets(1, rtvs, dsv); ctx->OMSetDepthStencilState(dss, 0);
        ctx->OMSetBlendState(bsPlain, nullptr, 0xffffffff);
        ctx->PSSetShader(psSS, nullptr, 0);  // any opaque color; overwritten below
        ctx->Draw(3, 0);
        ctx->ClearRenderTargetView(rtv0, behind);  // depth stays 0.25 where drawn (everywhere)
      }
      CB d{c.mode == 7 ? 0.75f : 0.5f, (UINT)mode, 0, full};
      if (c.mode == 4) d.mask = n == 2 ? 0x1 : n == 4 ? 0x3 : 0x0f;
      ctx->UpdateSubresource(cb, 0, nullptr, &d, 0, 0);
      ctx->OMSetRenderTargets(c.mode == 1 ? 2 : 1, rtvs, c.mode == 7 ? dsv : nullptr);
      ctx->OMSetDepthStencilState(c.mode == 7 ? dss : dssOff, 0);
      bool a2c = c.mode == 0 || c.mode == 1 || c.mode == 2 || c.mode == 6 || c.mode == 7;
      ctx->OMSetBlendState(a2c ? bsA2C : bsPlain, nullptr, c.mode == 3 ? smask : 0xffffffff);
      ctx->PSSetShader(c.mode == 4 ? psCov : c.mode == 5 ? psSS : psPlain, nullptr, 0);
      ctx->Draw(3, 0);
      ctx->OMSetRenderTargets(0, nullptr, nullptr);
      ctx->OMSetBlendState(bsPlain, nullptr, 0xffffffff);
      ctx->OMSetDepthStencilState(dssOff, 0);

      unsigned bad = 0, covered = 0, total = 0; int fx = 0, fy = 0, fs = 0; float fg[4] = {0}, fw[4] = {0};
      for (UINT s = 0; s < n; s++) {
        CB l{0.0f, 0, s, 0}; ctx->UpdateSubresource(cb, 0, nullptr, &l, 0, 0);
        ctx->OMSetRenderTargets(1, &rtvOut, nullptr);
        ctx->PSSetShader(psLoad, nullptr, 0); ctx->PSSetShaderResources(0, 1, &srv);
        ctx->Draw(3, 0);
        ID3D11ShaderResourceView* nul = nullptr; ctx->PSSetShaderResources(0, 1, &nul);
        ctx->OMSetRenderTargets(0, nullptr, nullptr);
        ctx->CopyResource(st, out);
        D3D11_MAPPED_SUBRESOURCE m{};
        if (FAILED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) { printf("map failed\n"); return 4; }
        for (int y = 0; y < H; y++) {
          const float* row = (const float*)((const uint8_t*)m.pData + (size_t)y * m.RowPitch);
          for (int x = 0; x < W; x++) {
            UINT r = (UINT)(x / 32) + (UINT)(y / 32) * 977;
            const float* got = &row[x * 4];
            float want[4];
            const float* base = c.mode == 7 ? behind : clear;
            bool cov = false, any = false;
            switch (c.mode) {
            case 0: case 1: case 7: cov = (r & 1) != 0; break;
            case 2: any = true; break;
            case 3: cov = (smask >> s) & 1; break;
            case 4: { UINT mk = d.mask; UINT rot = ((mk << (r % 4)) | (mk >> (8 - (r % 4)))) & 0xff; cov = (rot >> s) & 1; break; }
            case 5: cov = true; break;
            case 6: cov = (r & 1) != 0 && (r % 3) != 0; break;
            }
            for (int k = 0; k < 4; k++) want[k] = cov ? drawn[k] : base[k];
            if (cov && c.mode != 5) want[3] = (c.mode == 4 || c.mode == 3) ? ((r & 1) ? 1.0f : 0.0f) : 1.0f;
            if (c.mode == 5) { want[0] = (s + 1) / 16.0f; want[1] = 0.75f; want[2] = 0.125f; want[3] = 1.0f; }
            bool ok = true;
            if (any) {
              bool isClear = true, isDrawn = true;
              for (int k = 0; k < 3; k++) { isClear &= std::fabs(got[k] - clear[k]) < 0.003f; isDrawn &= std::fabs(got[k] - drawn[k]) < 0.003f; }
              ok = isClear || isDrawn; covered += isDrawn; total++;
              for (int k = 0; k < 4; k++) want[k] = drawn[k];
            } else {
              for (int k = 0; k < 4; k++) ok &= std::fabs(got[k] - want[k]) < 0.003f;
            }
            if (!ok) { if (!bad++) { fx = x; fy = y; fs = (int)s; for (int k = 0; k < 4; k++) { fg[k] = got[k]; fw[k] = want[k]; } } }
          }
        }
        ctx->Unmap(st, 0);
      }
      bool halfOk = true;
      if (c.mode == 2) { double frac = total ? (double)covered / total : 0; halfOk = frac > 0.3 && frac < 0.7;
        printf("%ux %-15s covered %.3f of samples\n", n, c.name, frac); }
      if (bad || !halfOk) {
        failed++;
        printf("%ux %-15s FAIL bad=%u first (%d,%d) sample %d got %.3f %.3f %.3f %.3f want %.3f %.3f %.3f %.3f\n", n, c.name, bad, fx, fy, fs,
               fg[0], fg[1], fg[2], fg[3], fw[0], fw[1], fw[2], fw[3]);
      } else {
        printf("%ux %-15s ok\n", n, c.name);
      }
    }
    srv->Release(); rtv0->Release(); rtv1->Release(); rtvOut->Release(); dsv->Release();
    rt0->Release(); rt1->Release(); ds->Release(); out->Release(); st->Release();
  }
  printf("RESULT %s (%d failing cases)\n", failed ? "FAIL" : "PASS", failed);
  return failed ? 1 : 0;
}
