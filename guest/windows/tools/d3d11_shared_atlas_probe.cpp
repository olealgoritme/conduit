// d3d11_shared_atlas_probe.cpp: a glyph atlas shared between two D3D11
// devices, the way XAML and RichEdit split text rendering (one device
// rasterizes glyphs into a KMT-shared atlas, another device opens it with
// OpenSharedResource and samples it when it draws the text).
//
// Device A creates the atlas (A8_UNORM or B8G8R8A8_UNORM, RT|SRV, MISC_SHARED)
// and, per round, writes a new value into one cell (ClearView, then a draw
// over half of the cell), then Flushes. Device B samples the cell through its
// opened view into its own target (Load().a, the glyph-quad pattern) and also
// copies it to a staging texture, and checks both against what A wrote.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_shared_atlas_probe.exe d3d11_shared_atlas_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_shared_atlas_probe.exe [a8|bgra] [rounds] [size=256]   (HELIOS_ICD as usual)
// Exit code 0 when every round matches.
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

static UINT S = 256;
static const UINT CELL = 32;

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

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile %s: %s\n", target, e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

static const char* kVS =
  "float4 main(uint id : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((id << 1) & 2, id & 2);\n"
  "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
  "}\n";
static const char* kPSConst =
  "cbuffer C : register(b0) { float4 color; };\n"
  "float4 main() : SV_Target { return color; }\n";
static const char* kPSLoad =
  "Texture2D<float4> t : register(t0);\n"
  "float4 main(float4 pos : SV_Position) : SV_Target {\n"
  "  float a = t.Load(int3(pos.xy, 0)).a;\n"
  "  return float4(a, a, a, 1);\n"
  "}\n";

struct Dev {
  ID3D11Device* dev = nullptr;
  ID3D11DeviceContext1* ctx = nullptr;
  ID3D11VertexShader* vs = nullptr;
  ID3D11PixelShader* psConst = nullptr;
  ID3D11PixelShader* psLoad = nullptr;
  ID3D11Buffer* cb = nullptr;
};

static bool MakeDev(IDXGIAdapter1* adapter, Dev& d, ID3DBlob* vs, ID3DBlob* pc, ID3DBlob* pl) {
  ID3D11DeviceContext* c0 = nullptr;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_1;
  HRESULT hr = D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                                 nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &fl, 1, D3D11_SDK_VERSION,
                                 &d.dev, nullptr, &c0);
  if (FAILED(hr)) { printf("D3D11CreateDevice hr=0x%08lx\n", hr); return false; }
  c0->QueryInterface(__uuidof(ID3D11DeviceContext1), (void**)&d.ctx);
  d.dev->CreateVertexShader(vs->GetBufferPointer(), vs->GetBufferSize(), nullptr, &d.vs);
  d.dev->CreatePixelShader(pc->GetBufferPointer(), pc->GetBufferSize(), nullptr, &d.psConst);
  d.dev->CreatePixelShader(pl->GetBufferPointer(), pl->GetBufferSize(), nullptr, &d.psLoad);
  D3D11_BUFFER_DESC bd{16, D3D11_USAGE_DEFAULT, D3D11_BIND_CONSTANT_BUFFER, 0, 0, 0};
  d.dev->CreateBuffer(&bd, nullptr, &d.cb);
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE;
  rd.DepthClipEnable = TRUE; rd.ScissorEnable = TRUE;
  ID3D11RasterizerState* rs; d.dev->CreateRasterizerState(&rd, &rs);
  D3D11_VIEWPORT vp{0, 0, (float)S, (float)S, 0, 1};
  d.ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  d.ctx->VSSetShader(d.vs, nullptr, 0);
  d.ctx->RSSetState(rs);
  d.ctx->RSSetViewports(1, &vp);
  d.ctx->PSSetConstantBuffers(0, 1, &d.cb);
  return true;
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const bool a8 = !(argc > 1 && strcmp(argv[1], "bgra") == 0);
  const int rounds = argc > 2 ? atoi(argv[2]) : 32;
  if (argc > 3) S = (UINT)atoi(argv[3]);
  IDXGIAdapter1* adapter = HeliosAdapter();
  ID3DBlob* vsb = Compile(kVS, "vs_4_0");
  ID3DBlob* pcb = Compile(kPSConst, "ps_4_0");
  ID3DBlob* plb = Compile(kPSLoad, "ps_4_0");
  Dev A, B;
  if (!MakeDev(adapter, A, vsb, pcb, plb) || !MakeDev(adapter, B, vsb, pcb, plb)) return 2;

  const DXGI_FORMAT fmt = a8 ? DXGI_FORMAT_A8_UNORM : DXGI_FORMAT_B8G8R8A8_UNORM;
  D3D11_TEXTURE2D_DESC td{};
  td.Width = S; td.Height = S; td.MipLevels = 1; td.ArraySize = 1; td.SampleDesc.Count = 1;
  td.Format = fmt; td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
  ID3D11Texture2D* atlasA = nullptr;
  HRESULT hr = A.dev->CreateTexture2D(&td, nullptr, &atlasA);
  if (FAILED(hr)) { printf("shared CreateTexture2D hr=0x%08lx\n", hr); return 2; }
  ID3D11RenderTargetView* rtvA; A.dev->CreateRenderTargetView(atlasA, nullptr, &rtvA);
  float zero[4] = {0, 0, 0, 0};
  A.ctx->ClearRenderTargetView(rtvA, zero);
  A.ctx->Flush();

  IDXGIResource* dr = nullptr; atlasA->QueryInterface(__uuidof(IDXGIResource), (void**)&dr);
  HANDLE sh = nullptr; dr->GetSharedHandle(&sh);
  ID3D11Texture2D* atlasB = nullptr;
  hr = B.dev->OpenSharedResource(sh, __uuidof(ID3D11Texture2D), (void**)&atlasB);
  printf("%s atlas %ux%u shared handle %p, B open hr=0x%08lx\n", a8 ? "A8" : "BGRA", S, S, sh, hr);
  if (FAILED(hr)) return 2;
  ID3D11ShaderResourceView* srvB; B.dev->CreateShaderResourceView(atlasB, nullptr, &srvB);
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM; td.MiscFlags = 0;
  ID3D11Texture2D* targetB; B.dev->CreateTexture2D(&td, nullptr, &targetB);
  ID3D11RenderTargetView* rtvB; B.dev->CreateRenderTargetView(targetB, nullptr, &rtvB);
  td.Usage = D3D11_USAGE_STAGING; td.BindFlags = 0; td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* stagingTarget; B.dev->CreateTexture2D(&td, nullptr, &stagingTarget);
  td.Format = fmt;
  ID3D11Texture2D* stagingAtlas; B.dev->CreateTexture2D(&td, nullptr, &stagingAtlas);

  int bad = 0;
  for (int r = 0; r < rounds; ++r) {
    const UINT cells = S / CELL;
    const UINT cx = (r % cells) * CELL, cy = ((r / cells) % cells) * CELL;
    const uint8_t clearV = (uint8_t)(40 + (r * 37) % 200);
    const uint8_t drawV = (uint8_t)(255 - clearV / 2);
    // A: ClearView the cell, draw over its left half, flush.
    float cv[4] = {clearV / 255.f, clearV / 255.f, clearV / 255.f, clearV / 255.f};
    D3D11_RECT cell{(LONG)cx, (LONG)cy, (LONG)(cx + CELL), (LONG)(cy + CELL)};
    A.ctx->ClearView(rtvA, cv, &cell, 1);
    float col[4] = {drawV / 255.f, drawV / 255.f, drawV / 255.f, drawV / 255.f};
    A.ctx->UpdateSubresource(A.cb, 0, nullptr, col, 0, 0);
    A.ctx->OMSetRenderTargets(1, &rtvA, nullptr);
    A.ctx->PSSetShader(A.psConst, nullptr, 0);
    D3D11_RECT half{(LONG)cx, (LONG)cy, (LONG)(cx + CELL / 2), (LONG)(cy + CELL)};
    A.ctx->RSSetScissorRects(1, &half);
    A.ctx->Draw(3, 0);
    A.ctx->Flush();
    // B: sample the cell into its target and copy the atlas, read both back.
    B.ctx->OMSetRenderTargets(1, &rtvB, nullptr);
    B.ctx->PSSetShader(B.psLoad, nullptr, 0);
    B.ctx->PSSetShaderResources(0, 1, &srvB);
    B.ctx->RSSetScissorRects(1, &cell);
    B.ctx->Draw(3, 0);
    ID3D11ShaderResourceView* none = nullptr;
    B.ctx->PSSetShaderResources(0, 1, &none);
    B.ctx->CopyResource(stagingTarget, targetB);
    B.ctx->CopyResource(stagingAtlas, atlasB);
    D3D11_MAPPED_SUBRESOURCE m{};
    B.ctx->Map(stagingTarget, 0, D3D11_MAP_READ, 0, &m);
    auto px = [&](UINT x, UINT y) { return ((const uint8_t*)m.pData)[y * m.RowPitch + x * 4]; };
    int sDraw = px(cx + 4, cy + 4), sClear = px(cx + CELL - 4, cy + 4);
    B.ctx->Unmap(stagingTarget, 0);
    B.ctx->Map(stagingAtlas, 0, D3D11_MAP_READ, 0, &m);
    UINT bpp = a8 ? 1 : 4, ch = a8 ? 0 : 3;
    auto ax = [&](UINT x, UINT y) { return ((const uint8_t*)m.pData)[y * m.RowPitch + x * bpp + ch]; };
    int cDraw = ax(cx + 4, cy + 4), cClear = ax(cx + CELL - 4, cy + 4);
    B.ctx->Unmap(stagingAtlas, 0);
    bool ok = abs(sDraw - drawV) <= 2 && abs(sClear - clearV) <= 2 && abs(cDraw - drawV) <= 2 &&
              abs(cClear - clearV) <= 2;
    if (!ok) bad++;
    if (!ok || r < 2)
      printf("round %2d cell (%3u,%3u): A wrote draw %3d clear %3d; B sampled %3d %3d, copied %3d %3d  %s\n",
             r, cx, cy, drawV, clearV, sDraw, sClear, cDraw, cClear, ok ? "ok" : "MISMATCH");
  }
  printf("%s: %d of %d rounds wrong\n", a8 ? "A8" : "BGRA", bad, rounds);
  return bad ? 1 : 0;
}
