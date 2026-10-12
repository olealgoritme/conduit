// d3d11_mip_chain_probe.cpp: does a pass that reads one mip of a texture and
// writes another mip of the same texture see the right data?
//
// Games build mip chains, bloom pyramids and depth pyramids this way: an SRV
// limited to mip N (MostDetailedMip=N, MipLevels=1) is bound while mip N+1 is
// the render target or UAV. That is legal D3D11 (the views do not overlap);
// the runtime only nulls bindings whose subresources overlap, and tells the
// driver through pfnShaderResourceViewReadAfterWriteHazard that the SRV's
// resource was written before (a cache hint, nothing to unbind).
//
// The probe fills mip 0 of an RGBA16F texture with a known pattern, then
// builds the chain twice: with a pixel shader (SRV mip N -> RTV mip N+1) and
// with a compute shader (SRV mip N -> UAV mip N+1, plus an array-slice
// variant: SRV slice 0 -> RTV slice 1 of the same mip). Every texel of every
// mip is read back and compared with the CPU's box-filtered chain.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_mip_chain_probe.exe d3d11_mip_chain_probe.cpp
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_mip_chain_probe.exe [helios|warp] [size=256] [rounds=20]
// Exit code 0 when every texel matches.
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

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile %s: %s\n", target, e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

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

// Fullscreen triangle; the pixel shader averages the 2x2 texels of the source
// mip under each destination texel (Load, so no sampler state is involved).
static const char* kVS =
  "float4 main(uint v : SV_VertexID) : SV_Position {\n"
  "  float2 p = float2((v << 1) & 2, v & 2); return float4(p * float2(2, -2) + float2(-1, 1), 0, 1); }\n";
static const char* kPS =
  "Texture2DArray<float4> src : register(t0);\n"
  "cbuffer C : register(b0) { uint slice; uint3 pad; };\n"
  "float4 main(float4 pos : SV_Position) : SV_Target {\n"
  "  int2 d = int2(pos.xy) * 2;\n"
  "  return (src.Load(int4(d, slice, 0)) + src.Load(int4(d + int2(1,0), slice, 0)) +\n"
  "          src.Load(int4(d + int2(0,1), slice, 0)) + src.Load(int4(d + int2(1,1), slice, 0))) * 0.25; }\n";
static const char* kCS =
  "Texture2DArray<float4> src : register(t0); RWTexture2DArray<float4> dst : register(u0);\n"
  "[numthreads(8,8,1)] void main(uint3 i : SV_DispatchThreadID) {\n"
  "  uint w, h, n; dst.GetDimensions(w, h, n); if (i.x >= w || i.y >= h) return;\n"
  "  int2 d = int2(i.xy) * 2;\n"
  "  dst[uint3(i.xy, 0)] = (src.Load(int4(d, 0, 0)) + src.Load(int4(d + int2(1,0), 0, 0)) +\n"
  "                        src.Load(int4(d + int2(0,1), 0, 0)) + src.Load(int4(d + int2(1,1), 0, 0))) * 0.25; }\n";

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const bool warp = argc > 1 && !strcmp(argv[1], "warp");
  const int N = argc > 2 ? atoi(argv[2]) : 256;
  const int rounds = argc > 3 ? atoi(argv[3]) : 20;
  int mips = 0; for (int s = N; s >= 1; s >>= 1) mips++;
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  printf("device %s size %d mips %d rounds %d\n", warp ? "WARP" : adapter ? "Helios" : "default", N, mips, rounds);

  ID3DBlob* vsb = Compile(kVS, "vs_5_0"); ID3DBlob* psb = Compile(kPS, "ps_5_0"); ID3DBlob* csb = Compile(kCS, "cs_5_0");
  ID3D11VertexShader* vs; ID3D11PixelShader* ps; ID3D11ComputeShader* cs;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps);
  dev->CreateComputeShader(csb->GetBufferPointer(), csb->GetBufferSize(), nullptr, &cs);
  D3D11_BUFFER_DESC cbd{16, D3D11_USAGE_DEFAULT, D3D11_BIND_CONSTANT_BUFFER, 0, 0, 0};
  ID3D11Buffer* cb[2];
  for (uint32_t s = 0; s < 2; ++s) { uint32_t v[4] = {s, 0, 0, 0}; D3D11_SUBRESOURCE_DATA d{v, 0, 0}; dev->CreateBuffer(&cbd, &d, &cb[s]); }

  // CPU reference chain (slice 0); slice 1 starts as a different pattern.
  std::vector<std::vector<float>> ref(mips);
  ref[0].resize((size_t)N * N * 4);
  for (int y = 0; y < N; ++y) for (int x = 0; x < N; ++x) for (int c = 0; c < 4; ++c)
    ref[0][((size_t)y * N + x) * 4 + c] = (float)(((x * 7 + y * 13 + c * 29) % 64)) * 0.25f;
  for (int m = 1; m < mips; ++m) {
    int s = N >> m, ps2 = N >> (m - 1); ref[m].resize((size_t)s * s * 4);
    for (int y = 0; y < s; ++y) for (int x = 0; x < s; ++x) for (int c = 0; c < 4; ++c) {
      auto at = [&](int xx, int yy) { return FromHalf(Half(ref[m - 1][((size_t)yy * ps2 + xx) * 4 + c])); };
      ref[m][((size_t)y * s + x) * 4 + c] = (at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1)) * 0.25f;
    }
  }
  std::vector<uint16_t> init0((size_t)N * N * 4), init1((size_t)N * N * 4, Half(-1.0f));
  for (size_t i = 0; i < init0.size(); ++i) init0[i] = Half(ref[0][i]);

  int bad = 0;
  for (int r = 0; r < rounds; ++r) {
    const bool compute = r & 1;
    D3D11_TEXTURE2D_DESC td{(UINT)N, (UINT)N, (UINT)mips, 2, DXGI_FORMAT_R16G16B16A16_FLOAT, {1, 0}, D3D11_USAGE_DEFAULT,
                            D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET | D3D11_BIND_UNORDERED_ACCESS, 0, 0};
    std::vector<D3D11_SUBRESOURCE_DATA> sd((size_t)mips * 2);
    std::vector<std::vector<uint16_t>> junk(mips);
    for (int m = 0; m < mips; ++m) {
      int s = N >> m; junk[m].assign((size_t)s * s * 4, Half(-2.0f));
      sd[m] = {m == 0 ? init0.data() : junk[m].data(), (UINT)s * 8, 0};
      sd[mips + m] = {m == 0 ? init1.data() : junk[m].data(), (UINT)s * 8, 0};
    }
    ID3D11Texture2D* tex = nullptr;
    if (FAILED(dev->CreateTexture2D(&td, sd.data(), &tex))) { printf("texture create failed\n"); return 2; }
    for (int m = 0; m + 1 < mips; ++m) {
      D3D11_SHADER_RESOURCE_VIEW_DESC sv{}; sv.Format = td.Format; sv.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
      sv.Texture2DArray = {(UINT)m, 1, 0, 1};
      ID3D11ShaderResourceView* srv = nullptr; dev->CreateShaderResourceView(tex, &sv, &srv);
      if (compute) {
        D3D11_UNORDERED_ACCESS_VIEW_DESC uv{}; uv.Format = td.Format; uv.ViewDimension = D3D11_UAV_DIMENSION_TEXTURE2DARRAY;
        uv.Texture2DArray = {(UINT)m + 1, 0, 1};
        ID3D11UnorderedAccessView* uav = nullptr; dev->CreateUnorderedAccessView(tex, &uv, &uav);
        ctx->CSSetShader(cs, nullptr, 0);
        ctx->CSSetUnorderedAccessViews(0, 1, &uav, nullptr);
        ctx->CSSetShaderResources(0, 1, &srv);
        int s = N >> (m + 1); ctx->Dispatch((s + 7) / 8, (s + 7) / 8, 1);
        ID3D11ShaderResourceView* nsrv = nullptr; ID3D11UnorderedAccessView* nuav = nullptr;
        ctx->CSSetShaderResources(0, 1, &nsrv); ctx->CSSetUnorderedAccessViews(0, 1, &nuav, nullptr);
        uav->Release();
      } else {
        D3D11_RENDER_TARGET_VIEW_DESC rv{}; rv.Format = td.Format; rv.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DARRAY;
        rv.Texture2DArray = {(UINT)m + 1, 0, 1};
        ID3D11RenderTargetView* rtv = nullptr; dev->CreateRenderTargetView(tex, &rv, &rtv);
        // Bind the SRV first, then the RTV of the other mip: neither may be
        // unbound (no overlap), unlike a same-mip bind.
        ctx->PSSetShaderResources(0, 1, &srv);
        ctx->OMSetRenderTargets(1, &rtv, nullptr);
        int s = N >> (m + 1); D3D11_VIEWPORT vp{0, 0, (float)s, (float)s, 0, 1}; ctx->RSSetViewports(1, &vp);
        ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST); ctx->IASetInputLayout(nullptr);
        ctx->VSSetShader(vs, nullptr, 0); ctx->PSSetShader(ps, nullptr, 0); ctx->PSSetConstantBuffers(0, 1, &cb[0]);
        ctx->Draw(3, 0);
        ID3D11RenderTargetView* nrtv = nullptr; ctx->OMSetRenderTargets(1, &nrtv, nullptr);
        rtv->Release();
      }
      srv->Release();
    }
    // Array-slice variant on mip 0: read slice 0 (mip 0) while writing slice 1 (mip 1).
    if (!compute) {
      D3D11_SHADER_RESOURCE_VIEW_DESC sv{}; sv.Format = td.Format; sv.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
      sv.Texture2DArray = {0, 1, 0, 1};
      D3D11_RENDER_TARGET_VIEW_DESC rv{}; rv.Format = td.Format; rv.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DARRAY;
      rv.Texture2DArray = {1, 1, 1};
      ID3D11ShaderResourceView* srv = nullptr; ID3D11RenderTargetView* rtv = nullptr;
      dev->CreateShaderResourceView(tex, &sv, &srv); dev->CreateRenderTargetView(tex, &rv, &rtv);
      ctx->OMSetRenderTargets(1, &rtv, nullptr); ctx->PSSetShaderResources(0, 1, &srv);
      D3D11_VIEWPORT vp{0, 0, (float)(N / 2), (float)(N / 2), 0, 1}; ctx->RSSetViewports(1, &vp);
      ctx->Draw(3, 0);
      ID3D11RenderTargetView* nrtv = nullptr; ctx->OMSetRenderTargets(1, &nrtv, nullptr);
      srv->Release(); rtv->Release();
    }
    td.Usage = D3D11_USAGE_STAGING; td.BindFlags = 0; td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D* st = nullptr; dev->CreateTexture2D(&td, nullptr, &st); ctx->CopyResource(st, tex);
    int roundBad = 0;
    for (int slice = 0; slice < (compute ? 1 : 2); ++slice) {
      for (int m = 0; m < mips; ++m) {
        if (slice == 1 && m != 1) continue;
        D3D11_MAPPED_SUBRESOURCE map{};
        UINT sub = D3D11CalcSubresource(m, slice, mips);
        if (FAILED(ctx->Map(st, sub, D3D11_MAP_READ, 0, &map))) { roundBad++; continue; }
        int s = N >> m;
        for (int y = 0; y < s; ++y) for (int x = 0; x < s; ++x) for (int c = 0; c < 4; ++c) {
          float got = FromHalf(((uint16_t*)((uint8_t*)map.pData + y * map.RowPitch))[x * 4 + c]);
          float want = ref[m][((size_t)y * s + x) * 4 + c];
          if (fabsf(got - want) > 0.02f + 0.002f * fabsf(want)) {
            if (roundBad < 4) printf("  round %d %s slice %d mip %d texel %d,%d.%d: got %g want %g\n", r,
                                     compute ? "cs" : "ps", slice, m, x, y, c, got, want);
            roundBad++;
          }
        }
        ctx->Unmap(st, sub);
      }
    }
    if (roundBad) printf("round %d (%s): %d bad texels\n", r, compute ? "compute" : "pixel", roundBad);
    bad += roundBad;
    st->Release(); tex->Release();
  }
  printf("RESULT %s (%d bad texels over %d rounds)\n", bad ? "FAIL" : "PASS", bad, rounds);
  return bad ? 1 : 0;
}
