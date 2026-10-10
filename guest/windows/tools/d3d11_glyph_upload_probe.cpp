// d3d11_glyph_upload_probe.cpp: partial UpdateSubresource uploads into
// small, short textures, the way Direct2D fills its ClearType glyph cache
// (Notepad's RichEdit: a 1024x20 B8G8R8A8 SHADER_RESOURCE texture, one box per
// new glyph), checked by sampling the texture in a shader (what the glyph
// quads do) and by a staging copy.
//
// For each texture size and format the probe uploads a pattern in boxes of
// glyph size at increasing x (and a few y offsets), then compares every texel
// against the pattern twice: through Load() in a pixel shader into a target
// that is read back, and through CopyResource into a staging texture.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_glyph_upload_probe.exe d3d11_glyph_upload_probe.cpp \
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_glyph_upload_probe.exe [helios|warp]   (HELIOS_ICD as usual)
// Exit code 0 when every case matches.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstdlib>
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

static const char* kVS =
  "float4 main(uint id : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((id << 1) & 2, id & 2);\n"
  "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
  "}\n";
// Copies the texture texel for texel into an RGBA8 target (bytes preserved).
static const char* kPS =
  "Texture2D<float4> t : register(t0);\n"
  "float4 main(float4 pos : SV_Position) : SV_Target { return t.Load(int3(pos.xy, 0)); }\n";

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

static uint8_t Pattern(UINT x, UINT y, UINT c, UINT gen) { return (uint8_t)(x * 7 + y * 31 + c * 101 + gen * 13 + 1); }

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  IDXGIAdapter1* adapter = strcmp(sel, "warp") == 0 ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = strcmp(sel, "warp") == 0 ? D3D_DRIVER_TYPE_WARP
                       : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                               D3D11_SDK_VERSION, &dev, nullptr, &ctx))) return 2;
  ID3DBlob* vsb = Compile(kVS, "vs_4_0"); ID3DBlob* psb = Compile(kPS, "ps_4_0");
  ID3D11VertexShader* vs; ID3D11PixelShader* ps;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps);
  ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ctx->VSSetShader(vs, nullptr, 0);
  ctx->PSSetShader(ps, nullptr, 0);

  struct Case { UINT w, h; DXGI_FORMAT fmt; UINT bw, bh; const char* name; };
  const Case cases[] = {
    {1024, 20, DXGI_FORMAT_B8G8R8A8_UNORM, 9, 14, "1024x20 BGRA8 (RichEdit ClearType cache)"},
    {1024, 20, DXGI_FORMAT_R8G8B8A8_UNORM, 9, 14, "1024x20 RGBA8"},
    {1024, 20, DXGI_FORMAT_A8_UNORM, 9, 14, "1024x20 A8"},
    {256, 80, DXGI_FORMAT_B8G8R8A8_UNORM, 9, 14, "256x80 BGRA8 (D2D default cache)"},
    {512, 5, DXGI_FORMAT_B8G8R8A8_UNORM, 7, 5, "512x5 BGRA8"},
    {300, 33, DXGI_FORMAT_B8G8R8A8_UNORM, 11, 17, "300x33 BGRA8"},
    {1024, 40, DXGI_FORMAT_B8G8R8A8_UNORM, 9, 14, "1024x40 BGRA8"},
    {64, 12, DXGI_FORMAT_B8G8R8A8_UNORM, 5, 12, "64x12 BGRA8"},
  };
  int failed = 0;
  for (const Case& c : cases) {
    const UINT bpp = c.fmt == DXGI_FORMAT_A8_UNORM ? 1 : 4;
    D3D11_TEXTURE2D_DESC td{};
    td.Width = c.w; td.Height = c.h; td.MipLevels = 1; td.ArraySize = 1; td.SampleDesc.Count = 1;
    td.Format = c.fmt; td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    ID3D11Texture2D* tex; dev->CreateTexture2D(&td, nullptr, &tex);
    ID3D11ShaderResourceView* srv; dev->CreateShaderResourceView(tex, nullptr, &srv);
    std::vector<uint8_t> expect((size_t)c.w * c.h * bpp, 0);
    // Zero first, as the cache starts cleared.
    ctx->UpdateSubresource(tex, 0, nullptr, expect.data(), c.w * bpp, 0);
    // Glyph boxes along the row, with a second band at y offset 3 where it fits.
    UINT gen = 0;
    for (UINT y0 = 0; y0 + c.bh <= c.h; y0 += (c.h - c.bh > 3 ? 3 : c.h)) {
      for (UINT x0 = 1; x0 + c.bw <= c.w; x0 += c.bw + 1, ++gen) {
        std::vector<uint8_t> box((size_t)c.bw * c.bh * bpp);
        for (UINT y = 0; y < c.bh; ++y)
          for (UINT x = 0; x < c.bw; ++x)
            for (UINT k = 0; k < bpp; ++k) {
              uint8_t v = Pattern(x0 + x, y0 + y, k, gen);
              box[((size_t)y * c.bw + x) * bpp + k] = v;
              expect[((size_t)(y0 + y) * c.w + (x0 + x)) * bpp + k] = v;
            }
        D3D11_BOX b{x0, y0, 0, x0 + c.bw, y0 + c.bh, 1};
        ctx->UpdateSubresource(tex, 0, &b, box.data(), c.bw * bpp, 0);
      }
      if (c.h - c.bh <= 3) break;
    }
    // Readback 1: staging copy.
    D3D11_TEXTURE2D_DESC sd = td; sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D* st; dev->CreateTexture2D(&sd, nullptr, &st);
    ctx->CopyResource(st, tex);
    D3D11_MAPPED_SUBRESOURCE m{};
    ctx->Map(st, 0, D3D11_MAP_READ, 0, &m);
    UINT badCopy = 0, firstX = 0, firstY = 0;
    for (UINT y = 0; y < c.h; ++y)
      for (UINT x = 0; x < c.w; ++x)
        for (UINT k = 0; k < bpp; ++k)
          if (((const uint8_t*)m.pData)[y * m.RowPitch + x * bpp + k] != expect[((size_t)y * c.w + x) * bpp + k]) {
            if (!badCopy) { firstX = x; firstY = y; }
            badCopy++;
          }
    ctx->Unmap(st, 0);
    // Readback 2: sampled by a shader into an RGBA8 target.
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.BindFlags = D3D11_BIND_RENDER_TARGET;
    ID3D11Texture2D* rt; dev->CreateTexture2D(&td, nullptr, &rt);
    ID3D11RenderTargetView* rtv; dev->CreateRenderTargetView(rt, nullptr, &rtv);
    D3D11_VIEWPORT vp{0, 0, (float)c.w, (float)c.h, 0, 1};
    ctx->RSSetViewports(1, &vp);
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->PSSetShaderResources(0, 1, &srv);
    ctx->Draw(3, 0);
    ID3D11ShaderResourceView* none = nullptr; ctx->PSSetShaderResources(0, 1, &none);
    sd = td; sd.Usage = D3D11_USAGE_STAGING; sd.BindFlags = 0; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D* st2; dev->CreateTexture2D(&sd, nullptr, &st2);
    ctx->CopyResource(st2, rt);
    ctx->Map(st2, 0, D3D11_MAP_READ, 0, &m);
    UINT badSample = 0, sX = 0, sY = 0;
    for (UINT y = 0; y < c.h; ++y)
      for (UINT x = 0; x < c.w; ++x) {
        const uint8_t* px = (const uint8_t*)m.pData + y * m.RowPitch + x * 4;
        const uint8_t* e = &expect[((size_t)y * c.w + x) * bpp];
        bool ok;
        if (bpp == 1) ok = px[3] == e[0];
        else if (c.fmt == DXGI_FORMAT_B8G8R8A8_UNORM) ok = px[0] == e[2] && px[1] == e[1] && px[2] == e[0] && px[3] == e[3];
        else ok = memcmp(px, e, 4) == 0;
        if (!ok) { if (!badSample) { sX = x; sY = y; } badSample++; }
      }
    ctx->Unmap(st2, 0);
    bool pass = !badCopy && !badSample;
    if (!pass) failed++;
    printf("%-42s copy: %6u bad (first %u,%u)  sampled: %6u bad (first %u,%u)  %s\n", c.name,
           badCopy, firstX, firstY, badSample, sX, sY, pass ? "ok" : "FAIL");
    st2->Release(); rtv->Release(); rt->Release(); st->Release(); srv->Release(); tex->Release();
  }
  printf("%s: %d case(s) failed\n", sel, failed);
  return failed ? 1 : 0;
}
