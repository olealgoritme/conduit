// d3d11_depth_views_probe.cpp: depth buffers drawn through views of one array
// layer or one mip level, and through one layered view, the way shadow
// cascades, cube shadows and depth pyramids use them, with depth tests that a
// stale or shared hierarchical depth (ZCULL) state would get wrong.
//
// Every case uses reverse Z (clear 0.0, GREATER). Layer or level B is drawn
// near (0.9) after layer or level A was drawn far (0.1); A is then loaded
// again and drawn at 0.5, which passes on A and must show everywhere. A
// culling state taken from B would drop it. Each case runs for 1x and 4x
// MSAA (layers) and for D32 and D24S8.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_depth_views_probe.exe d3d11_depth_views_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_depth_views_probe.exe [helios|warp] [width height]
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
#include <initializer_list>

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

// A full-viewport quad at a depth, in a color, to a layer (GS variant).
static const char* kVS =
  "cbuffer C : register(b0) { float depth; uint layer; float2 pad; float4 color; };\n"
  "struct O { float4 pos : SV_Position; uint layer : LAYER; };\n"
  "O main(uint id : SV_VertexID) { O o; float2 p = float2((id << 1) & 2, id & 2);\n"
  "  o.pos = float4(p * float2(2, -2) + float2(-1, 1), depth, 1); o.layer = layer; return o; }\n";
static const char* kGS =
  "struct I { float4 pos : SV_Position; uint layer : LAYER; };\n"
  "struct O { float4 pos : SV_Position; uint rt : SV_RenderTargetArrayIndex; };\n"
  "[maxvertexcount(3)] void main(triangle I v[3], inout TriangleStream<O> s) {\n"
  "  for (int i = 0; i < 3; i++) { O o; o.pos = v[i].pos; o.rt = v[i].layer; s.Append(o); } }\n";
static const char* kPS =
  "cbuffer C : register(b0) { float depth; uint layer; float2 pad; float4 color; };\n"
  "float4 main(float4 pos : SV_Position) : SV_Target { return color; }\n";

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

struct CB { float depth; UINT layer; float pad[2]; float color[4]; };

static ID3D11Device* dev;
static ID3D11DeviceContext* ctx;
static ID3D11Buffer* cb;
static ID3D11VertexShader* vs;
static ID3D11GeometryShader* gs;
static ID3D11PixelShader* ps;
static ID3D11DepthStencilState* dss;
static ID3D11RasterizerState* rs;

static void Quad(float depth, UINT layer, const float* color, bool layered) {
  CB c{depth, layer, {0, 0}, {color[0], color[1], color[2], color[3]}};
  ctx->UpdateSubresource(cb, 0, nullptr, &c, 0, 0);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->IASetInputLayout(nullptr);
  ctx->VSSetShader(vs, nullptr, 0); ctx->VSSetConstantBuffers(0, 1, &cb);
  ctx->GSSetShader(layered ? gs : nullptr, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0); ctx->PSSetConstantBuffers(0, 1, &cb);
  ctx->OMSetDepthStencilState(dss, 0); ctx->RSSetState(rs);
  ctx->Draw(3, 0);
}

// Counts texels of (array slice / mip) sub of an RGBA8 texture that differ
// from want; an MSAA texture is resolved first.
static unsigned CountBad(ID3D11Texture2D* tex, UINT sub, UINT w, UINT h, const float* want) {
  D3D11_TEXTURE2D_DESC d; tex->GetDesc(&d);
  ID3D11Texture2D* src = tex; UINT srcSub = sub;
  ID3D11Texture2D* res = nullptr;
  if (d.SampleDesc.Count > 1) {
    D3D11_TEXTURE2D_DESC r = d; r.SampleDesc.Count = 1; r.ArraySize = 1; r.MipLevels = 1;
    r.BindFlags = D3D11_BIND_RENDER_TARGET;
    dev->CreateTexture2D(&r, nullptr, &res);
    ctx->ResolveSubresource(res, 0, tex, sub, d.Format);
    src = res; srcSub = 0;
  }
  D3D11_TEXTURE2D_DESC s{}; s.Width = w; s.Height = h; s.MipLevels = 1; s.ArraySize = 1; s.Format = d.Format;
  s.SampleDesc.Count = 1; s.Usage = D3D11_USAGE_STAGING; s.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* st; dev->CreateTexture2D(&s, nullptr, &st);
  ctx->CopySubresourceRegion(st, 0, 0, 0, 0, src, srcSub, nullptr);
  D3D11_MAPPED_SUBRESOURCE m{};
  unsigned bad = 0;
  if (SUCCEEDED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) {
    uint8_t wb[4]; for (int k = 0; k < 4; k++) wb[k] = (uint8_t)std::lround(want[k] * 255.0f);
    for (UINT y = 0; y < h; y++)
      for (UINT x = 0; x < w; x++)
        if (memcmp((const uint8_t*)m.pData + (size_t)y * m.RowPitch + x * 4, wb, 4)) bad++;
    ctx->Unmap(st, 0);
  } else {
    bad = w * h;
  }
  st->Release(); if (res) res->Release();
  return bad;
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  const UINT W = argc > 3 ? (UINT)atoi(argv[2]) : 1920, H = argc > 3 ? (UINT)atoi(argv[3]) : 1080;
  const bool warp = strcmp(sel, "warp") == 0;
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  printf("device %s %ux%u\n", warp ? "WARP" : (adapter ? "Helios" : "default"), W, H);
  ID3DBlob* b = Compile(kVS, "vs_5_0"); dev->CreateVertexShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &vs);
  b = Compile(kGS, "gs_5_0"); dev->CreateGeometryShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &gs);
  b = Compile(kPS, "ps_5_0"); dev->CreatePixelShader(b->GetBufferPointer(), b->GetBufferSize(), nullptr, &ps);
  D3D11_BUFFER_DESC bd{}; bd.ByteWidth = sizeof(CB); bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  dev->CreateBuffer(&bd, nullptr, &cb);
  D3D11_DEPTH_STENCIL_DESC dsd{}; dsd.DepthEnable = TRUE; dsd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL;
  dsd.DepthFunc = D3D11_COMPARISON_GREATER;
  dev->CreateDepthStencilState(&dsd, &dss);
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE;
  rd.MultisampleEnable = TRUE;
  dev->CreateRasterizerState(&rd, &rs);

  const float black[4] = {0, 0, 0, 1}, far_[4] = {1, 0, 0, 1}, near_[4] = {0, 0, 1, 1}, mid[4] = {0, 1, 0, 1};
  int failed = 0;
  struct Fmt { DXGI_FORMAT tex, dsv; const char* name; };
  const Fmt fmts[] = {{DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, "D32"},
                      {DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, "D24S8"}};
  for (const Fmt& f : fmts) {
    for (UINT samples : {1u, 4u}) {
      for (int kind = 0; kind < 4; kind++) {
        // kind 0: one layer per view (A, B, A again)    1: one mip per view
        //      2: one layered view, B then A in one pass
        //      3: as 0, with every view and the textures created anew between
        //         passes (an engine re-creating targets after a settings change)
        if (kind == 1 && samples > 1) continue;  // MSAA textures have one level
        const bool mips = kind == 1;
        D3D11_TEXTURE2D_DESC td{}; td.Width = W; td.Height = H; td.MipLevels = mips ? 2 : 1;
        td.ArraySize = mips ? 1 : 2; td.SampleDesc.Count = samples; td.Usage = D3D11_USAGE_DEFAULT;
        td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.BindFlags = D3D11_BIND_RENDER_TARGET;
        ID3D11Texture2D *color, *depth;
        dev->CreateTexture2D(&td, nullptr, &color);
        td.Format = f.tex; td.BindFlags = D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE;
        dev->CreateTexture2D(&td, nullptr, &depth);
        auto views = [&](UINT idx, ID3D11RenderTargetView** rtv, ID3D11DepthStencilView** dsv, bool all) {
          D3D11_RENDER_TARGET_VIEW_DESC rv{}; rv.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
          D3D11_DEPTH_STENCIL_VIEW_DESC dv{}; dv.Format = f.dsv;
          if (mips) {
            rv.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2D; rv.Texture2D.MipSlice = idx;
            dv.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2D; dv.Texture2D.MipSlice = idx;
          } else if (samples > 1) {
            rv.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY;
            rv.Texture2DMSArray.FirstArraySlice = all ? 0 : idx; rv.Texture2DMSArray.ArraySize = all ? 2 : 1;
            dv.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY;
            dv.Texture2DMSArray.FirstArraySlice = all ? 0 : idx; dv.Texture2DMSArray.ArraySize = all ? 2 : 1;
          } else {
            rv.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DARRAY;
            rv.Texture2DArray.FirstArraySlice = all ? 0 : idx; rv.Texture2DArray.ArraySize = all ? 2 : 1;
            dv.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DARRAY;
            dv.Texture2DArray.FirstArraySlice = all ? 0 : idx; dv.Texture2DArray.ArraySize = all ? 2 : 1;
          }
          dev->CreateRenderTargetView(color, &rv, rtv);
          dev->CreateDepthStencilView(depth, &dv, dsv);
        };
        auto pass = [&](UINT idx, bool clear, float d, const float* c) {
          ID3D11RenderTargetView* rtv; ID3D11DepthStencilView* dsv;
          views(idx, &rtv, &dsv, false);
          UINT w = mips ? (W >> idx) : W, h = mips ? (H >> idx) : H;
          D3D11_VIEWPORT vp{0, 0, (float)w, (float)h, 0, 1}; ctx->RSSetViewports(1, &vp);
          if (clear) { ctx->ClearRenderTargetView(rtv, black); ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH | D3D11_CLEAR_STENCIL, 0.0f, 0); }
          ctx->OMSetRenderTargets(1, &rtv, dsv);
          Quad(d, 0, c, false);
          ctx->OMSetRenderTargets(0, nullptr, nullptr);
          rtv->Release(); dsv->Release();
        };
        unsigned badA = 0, badB = 0;
        UINT wA = W, hA = H, wB = mips ? W / 2 : W, hB = mips ? H / 2 : H;
        UINT subA = 0, subB = mips ? 1 : D3D11CalcSubresource(0, 1, 1);
        if (kind == 2) {
          ID3D11RenderTargetView* rtv; ID3D11DepthStencilView* dsv;
          views(0, &rtv, &dsv, true);
          D3D11_VIEWPORT vp{0, 0, (float)W, (float)H, 0, 1}; ctx->RSSetViewports(1, &vp);
          ctx->ClearRenderTargetView(rtv, black); ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH, 0.0f, 0);
          ctx->OMSetRenderTargets(1, &rtv, dsv);
          Quad(0.1f, 0, far_, true);   // A far
          Quad(0.9f, 1, near_, true);  // B near
          Quad(0.5f, 0, mid, true);    // A mid: passes on A
          ctx->OMSetRenderTargets(0, nullptr, nullptr);
          rtv->Release(); dsv->Release();
        } else {
          pass(0, true, 0.1f, far_);   // A far
          pass(1, true, 0.9f, near_);  // B near
          if (kind == 3) {
            // A settings change: new targets, drawn the same way, in memory the
            // old ones just gave back.
            color->Release(); depth->Release();
            td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.BindFlags = D3D11_BIND_RENDER_TARGET;
            dev->CreateTexture2D(&td, nullptr, &color);
            td.Format = f.tex; td.BindFlags = D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE;
            dev->CreateTexture2D(&td, nullptr, &depth);
            pass(0, true, 0.1f, far_);
            pass(1, true, 0.9f, near_);
          }
          pass(0, false, 0.5f, mid);   // A mid, loaded: passes on A
        }
        badA = CountBad(color, subA, wA, hA, mid);
        badB = CountBad(color, subB, wB, hB, near_);
        static const char* kinds[] = {"layer views", "mip views", "layered view", "recreated"};
        bool ok = !badA && !badB;
        printf("%-5s %ux %-13s %s", f.name, samples, kinds[kind], ok ? "ok" : "FAIL");
        if (!ok) printf("  A wrong %u of %u, B wrong %u of %u", badA, wA * hA, badB, wB * hB);
        printf("\n");
        if (!ok) failed++;
        color->Release(); depth->Release();
      }
    }
  }
  printf("RESULT %s (%d failing cases)\n", failed ? "FAIL" : "PASS", failed);
  return failed ? 1 : 0;
}
